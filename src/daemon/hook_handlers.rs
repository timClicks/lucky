//! Built-in handlers for Juju hooks that are executed by the daemon

use std::time::Duration;

use super::*;
use crate::docker::ContainerInfo;
use crate::rt::block_on;
use crate::types::{ScriptState, ScriptStatus};

pub(super) fn handle_hook(daemon: &LuckyDaemon, hook_name: &str) -> anyhow::Result<()> {
    match hook_name {
        "install" => handle_install(daemon),
        "stop" => handle_stop(daemon),
        _ => Ok(()),
    }
}

#[function_name::named]
fn handle_install(daemon: &LuckyDaemon) -> anyhow::Result<()> {
    let mut state = daemon.state.write().unwrap();

    // If Docker support is enabled
    if daemon.lucky_metadata.use_docker {
        daemon_set_status!(&mut state, ScriptState::Maintenance, "Installing docker");

        // Make sure Docker is installed
        crate::docker::ensure_docker()?;

        daemon_set_status!(&mut state, ScriptState::Active);
    }

    Ok(())
}

#[function_name::named]
fn handle_stop(daemon: &LuckyDaemon) -> anyhow::Result<()> {
    let mut state = daemon.state.write().unwrap();
    let docker_conn = daemon.get_docker_conn()?;
    let docker_conn = docker_conn.lock().unwrap();

    daemon_set_status!(&mut state, ScriptState::Maintenance, "Removing containers");

    for mut container_info in state.named_containers.values_mut() {
        remove_container(&docker_conn, &mut container_info)?;
    }

    // Erase container config
    state.named_containers.clear();

    if let Some(container_info) = &mut state.default_container {
        remove_container(&docker_conn, container_info)?;
    }

    // Erase container config
    state.default_container = None;

    daemon_set_status!(&mut state, ScriptState::Active);
    Ok(())
}

//
// Helpers
//

/// Helper to remove a given container
fn remove_container(
    docker_conn: &shiplift::Docker,
    container_info: &mut Cd<ContainerInfo>,
) -> anyhow::Result<()> {
    // If container has an ID
    if let Some(id) = &container_info.id {
        let container = docker_conn.containers().get(id);

        // Stop the container
        log::debug!("Stopping container: {}", id);
        block_on(container.stop(Some(Duration::from_secs(10))))?;

        // Remove the container
        log::debug!("Removing container: {}", id);
        block_on(container.delete())?;

        // Unset the container id
        container_info.id = None;
    }

    Ok(())
}
