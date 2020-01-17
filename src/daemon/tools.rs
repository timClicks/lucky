use anyhow::format_err;
use futures::prelude::*;
use shiplift::PullOptions;
use subprocess::{Exec, ExitStatus, Redirection};

use std::env;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use crate::docker::ContainerInfo;
use crate::rt::block_on;
use crate::types::{ScriptState, ScriptStatus};

use super::*;

/// Load the daemon state from the filesystem
pub(super) fn load_state(daemon: &LuckyDaemon) -> anyhow::Result<()> {
    let state_file_path = daemon.state_dir.join("state.yaml");
    if !state_file_path.exists() {
        return Ok(());
    }

    let state_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&state_file_path)
        .context(format!("Could not open state file: {:?}", state_file_path))?;

    *daemon.state.write().unwrap() = serde_yaml::from_reader(state_file).context(format!(
        "Could not parse state file as yaml: {:?}",
        state_file_path
    ))?;

    Ok(())
}

/// Write out the daemon state to fileystem
pub(super) fn flush_state(daemon: &LuckyDaemon) -> anyhow::Result<()> {
    log::debug!("Flushing daemon state to disk");
    let state_file_path = daemon.state_dir.join("state.yaml");
    let mut state_file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .create(true)
        .open(&state_file_path)?;

    // Write out comment to file
    state_file
        .write_all(b"# The daemon state will be written to this file when the daemon is shutdown\n")
        .context(format!(
            "Failed writing to state file: {:?}",
            state_file_path
        ))?;

    // Serialize state to file
    let state = &*daemon.state.read().unwrap();
    log::trace!("{:#?}", state);
    serde_yaml::to_writer(state_file, state).context(format!(
        "Failed to serialize daemon state to file: {:?}",
        state_file_path
    ))?;

    Ok(())
}

/// Set the status of a script
pub(super) fn set_script_status(
    state: &mut DaemonState,
    script_id: &str,
    status: ScriptStatus,
) -> anyhow::Result<()> {
    // Log the status hiding internal statuses unless trace logging is enabled
    log::info!(
        "Set status[{}]: {}",
        {
            if script_id.starts_with("__lucky::") && log::log_enabled!(log::Level::Trace) {
                script_id
            } else if script_id.starts_with("__lucky::") {
                "internal"
            } else {
                script_id
            }
        },
        status
    );

    // Insert script status
    state.script_statuses.insert(script_id.into(), status);

    // Set the Juju status to the consolidated script statuses
    crate::juju::set_status(tools::get_juju_status(state))?;

    Ok(())
}

/// Consolidate script statuses into one status that can be used as the global Juju Status
pub(super) fn get_juju_status(state: &DaemonState) -> ScriptStatus {
    // The resulting Juju state
    let mut juju_state = ScriptState::default();
    // The resulting Juju status message
    let mut juju_message = None;

    for status in state.script_statuses.values() {
        // If this script state has a higher precedence
        if status.state > juju_state {
            // Set the Juju state to the more precedent state
            juju_state = status.state;
        }

        // If there is a message with the status
        if let Some(message) = &status.message {
            if let Some(current) = juju_message {
                // Add this message to Juju message
                juju_message = Some([current, message.clone()].join(", "));
            } else {
                // Set Juju message to this message
                juju_message = Some(message.clone());
            }
        }
    }

    // Return Juju status
    ScriptStatus {
        state: juju_state,
        message: juju_message,
    }
}

// Run one of the charm's host scripts
pub(super) fn run_host_script(
    daemon: &LuckyDaemon,
    call: &mut dyn rpc::Call_TriggerHook,
    script_name: &str,
    environment: &HashMap<String, String>,
) -> anyhow::Result<()> {
    log::info!("Running host script: {}", script_name);

    // Add bin dirs to the PATH
    let path_env = if let Some(path) = std::env::var_os("PATH") {
        let mut paths = env::split_paths(&path).collect::<Vec<_>>();
        // Add the charm's bin dir
        paths.push(daemon.charm_dir.join("bin"));

        // Add the directory containing the Lucky executable
        if let Some(path) = std::env::current_exe()?.parent() {
            paths.push(path.to_owned());
        };

        env::join_paths(paths).context("Path contains invalid character")?
    } else {
        daemon.charm_dir.join("bin").as_os_str().to_owned()
    };

    // Build command
    let command_path = daemon.charm_dir.join("host_scripts").join(script_name);
    let mut command = Exec::cmd(&command_path)
        .stdout(Redirection::Pipe)
        .stderr(Redirection::Merge)
        .env("PATH", path_env)
        .env("LUCKY_CONTEXT", "client")
        .env("LUCKY_SCRIPT_ID", script_name);

    // Set environment for hook exececution
    for (k, v) in environment.iter() {
        command = command.env(k, v);
    }

    // Run script process
    let mut process = command
        .popen()
        .context(format!("Error executing script: {:?}", command_path))?;

    // Get script output buffer
    let output_buffer = BufReader::new(process.stdout.as_ref().expect("Stdout not opened"));

    // If the caller wants to get the streamed output
    if call.wants_more() {
        // Set the continues flag on the call to true
        call.set_continues(true);
    }

    // Loop through lines of output
    for line in output_buffer.lines() {
        let line = line?;
        log::debug!("output: {}", line);

        // Send caller output if they asked for it
        if call.wants_more() {
            call.reply(Some(line))?;
        }
    }

    // Wait for script to exit
    let exit_status = process.wait()?;

    match exit_status {
        // If the command exited with a code, return the code
        ExitStatus::Exited(0) => Ok(()),
        // If process had an abnormal exit code just exit 1
        ExitStatus::Exited(n) => Err(format_err!(
            r#"Host script "{}" exited non-zero ({})"#,
            script_name,
            n
        )),
        ExitStatus::Signaled(signum) => Err(format_err!(
            r#"Host script "{}" terminated by signal ({})"#,
            script_name,
            signum
        )),
        status => Err(format_err!(
            r#"Host script "{}" failed: {:?}"#,
            script_name,
            status
        )),
    }
}

#[function_name::named]
/// Apply any updates to container configuration for the charm by running
pub(super) fn apply_container_updates(daemon: &LuckyDaemon) -> anyhow::Result<()> {
    log::debug!("Applying container configuration");
    let mut state = daemon.state.write().unwrap();
    daemon_set_status!(
        &mut state,
        ScriptState::Maintenance,
        "Applying Docker configuration updates"
    );

    // Apply changes for any updated named containers
    for mut container in state.named_containers.values_mut() {
        apply_updates(daemon, &mut container)?;
    }

    // Remove named containers that are pending removal
    state
        .named_containers
        .retain(|_name, container| container.pending_removal == false);

    // Apply changes for the default container
    if let Some(container) = &mut state.default_container {
        apply_updates(daemon, container)?;

        // Remove container if pending removal
        if container.pending_removal == true {
            state.default_container = None;
        }
    }

    daemon_set_status!(&mut state, ScriptState::Active);
    Ok(())
}

fn apply_updates(
    daemon: &LuckyDaemon,
    container_info: &mut Cd<ContainerInfo>,
) -> anyhow::Result<()> {
    // Skip apply if container config is unchanged since last apply
    if container_info.is_clean() {
        return Ok(());
    }

    // Get the docker connection
    let docker_conn = daemon.get_docker_conn()?;
    let docker_conn = docker_conn.lock().unwrap();
    let containers = docker_conn.containers();
    let images = docker_conn.images();

    // If the container has already been deployed
    if let Some(id) = &container_info.id {
        // Remove the container
        let container = containers.get(&id);

        log::debug!("Stopping container: {}", id);
        block_on(container.stop(Some(Duration::from_secs(10))))?;
        log::debug!("Removing container: {}", id);
        block_on(container.delete())?;

        // Clear the containers ID
        container_info.id = None;
    }

    // If this contianer was not meant to be removed
    if !container_info.pending_removal {
        // Pull the image
        // TODO: Add `latest` tag if tag is missing
        let image_name = &container_info.config.image;
        log::debug!("Pulling container image: {}", image_name);
        block_on(
            images
                .pull(&PullOptions::builder().image(image_name).build())
                .collect(),
        )?;

        // Create the container
        let docker_options = container_info
            .config
            .to_container_options(&daemon.charm_dir, &daemon.socket_path)?;
        log::trace!("Creating container: docker {:#?}", docker_options);
        let create_info = block_on(containers.create(&docker_options))?;

        // Start the container
        log::debug!("Starting container: {}", create_info.id);
        let container = containers.get(&create_info.id);
        block_on(container.start())?;

        // Mark container_info as "clean" and up-to-date with the system config
        container_info.id = Some(create_info.id);
        container_info.clean();
    }

    Ok(())
}
