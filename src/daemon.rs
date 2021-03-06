//! Contains the Lucky Daemon and RPC implementaiton used for client->daemon communication.
use anyhow::Context;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use shiplift::Docker;

use crossbeam::{channel::unbounded as unbounded_channel, scope as thread_scope};

use std::collections::HashMap;
use std::convert::TryInto;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};

use crate::docker::{ContainerInfo, PortBinding, VolumeSource, VolumeTarget};
use crate::juju;
use crate::rpc;
use crate::types::{LuckyMetadata, ScriptStatus};

use crate::VOLUME_DIR;

/// Void type
enum Void {}

/// Daemon tools
mod tools;
// Built-in daemon hook handlers
mod hook_handlers;
// Daemon helper types
mod types;
use types::*;

#[derive(Debug, Default, Serialize, Deserialize)]
/// Contains the daemon state, which can be serialize and deserialized for persistance across
/// daemon crashes, upgrades, etc.
struct DaemonState {
    #[serde(rename = "script-statuses")]
    /// The statuses of all of the scripts
    script_statuses: HashMap<String, ScriptStatus>,
    // TODO: Key-value store implementation is not currently sufficient for detecting changes for
    // reactive.
    /// The unit-local key-value store
    kv: HashMap<String, Cd<String>>,
    default_container: Option<Cd<ContainerInfo>>,
    /// Other containers that the daemon is supervising
    named_containers: HashMap<String, Cd<ContainerInfo>>,
    /// The cached charm config obtained from Juju's `config-get` hook tool
    charm_config: HashMap<String, Cd<JsonValue>>,
}

/// The Lucky Daemon RPC service
struct LuckyDaemon {
    /// The charm directory
    charm_dir: PathBuf,
    /// The directory in which to store the daemon state
    lucky_data_dir: PathBuf,
    /// The path to the socket that the daemon is listening on
    socket_path: PathBuf,
    /// The contents of the charm's lucky.yaml config
    lucky_metadata: LuckyMetadata,
    /// Used to indicate that the server should stop listening.
    /// This will be set to true to indicate that the server should stop.
    stop_listening: Arc<AtomicBool>,
    /// The daemon state. This will be serialized and written to disc for persistance when the
    /// daemon crashes or is shutdown.  
    state: Arc<RwLock<DaemonState>>,
    /// The last time that the cron tick was run
    last_cron_tick: Arc<Mutex<DateTime<Local>>>,
    /// The docker daemon connection if it has been loaded
    docker_conn: Arc<Mutex<Option<Arc<Mutex<Docker>>>>>,
}

pub(crate) struct LuckyDaemonOptions {
    pub lucky_metadata: LuckyMetadata,
    pub charm_dir: PathBuf,
    pub data_dir: PathBuf,
    pub socket_path: PathBuf,
    pub stop_listening: Arc<AtomicBool>,
}

// TODO: set juju status upon errors
// Utility macro for handling anyhow results with `handle_err!(function_that_returns_anyhow_err())`
macro_rules! handle_err {
    ($expr:expr, $call:ident) => {
        match $expr {
            Ok(v) => v,
            Err(e) => {
                let e = format!("{:?}", e);
                log::error!("{}", e);
                return $call.reply_error(e);
            }
        }
    };
}

impl LuckyDaemon {
    /// Create a new daemon instance
    ///
    /// `stop_listening` will be set to `true` by the daemon if it recieves a `StopDaemon` RPC. The
    /// actual stopping of the server itself is not handled by the daemon.
    fn new(options: LuckyDaemonOptions) -> Self {
        let daemon = LuckyDaemon {
            lucky_metadata: options.lucky_metadata,
            charm_dir: options.charm_dir,
            lucky_data_dir: options.data_dir,
            socket_path: options.socket_path,
            stop_listening: options.stop_listening,
            state: Default::default(),
            last_cron_tick: Arc::new(Mutex::new(Local::now())),
            docker_conn: Arc::new(Mutex::new(None)),
        };

        // Load daemon state
        tools::load_state(&daemon)
            .context("Could not load daemon state from filesystem")
            .unwrap_or_else(|e| log::error!("{:?}", e));

        // Update the Juju status
        crate::juju::set_status(tools::get_juju_status(&daemon.state.read().unwrap()))
            .unwrap_or_else(|e| {
                log::warn!("{:?}", e.context("Could not set juju status"));
            });

        log::trace!("Loaded daemon state: {:#?}", daemon.state.read().unwrap());

        daemon
    }

    /// Gets a handle to the daemon's Docker connection, creating a new one if one doesn't already
    /// exist.
    fn get_docker_conn(&self) -> anyhow::Result<Arc<Mutex<Docker>>> {
        let mut docker_conn = self.docker_conn.lock().unwrap();

        // If we have a connection already, return it
        if let Some(docker_conn) = &*docker_conn {
            Ok(docker_conn.clone())
        // If there is no connection
        } else {
            // Connect to docker
            log::debug!("Connecting to Docker");
            let conn = Docker::new();

            // Test getting Docker info
            log::trace!("Docker info: {:?}", crate::rt::block_on(conn.info())?);

            // Return connection
            let conn = Arc::new(Mutex::new(conn));
            *docker_conn = Some(conn.clone());
            Ok(conn)
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn _trigger_hook(
        &self,
        call: &mut dyn rpc::Call_TriggerHook,
        hook_name: &str,
        environment: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        // Run any built-in hook handler
        hook_handlers::handle_pre_hook(&self, &hook_name).context(format!(
            r#"Error running internal hook handler for hook "{}""#,
            hook_name
        ))?;

        // Create a mutable clone of the recieved environment
        let mut environment = environment.clone();

        // Add LUCKY_HOOK environment variable
        environment.insert("LUCKY_HOOK".into(), hook_name.into());
        // Make environment a reference so it can be used in threads
        let environment = &environment;

        // Create a thread scope so script threads will be able to use references
        thread_scope(|s| -> anyhow::Result<()> {
            // Run hook scripts
            if let Some(hook_scripts) = self.lucky_metadata.hooks.get(hook_name) {
                let mut async_handles = Vec::new();

                // Execute all scripts registered for this hook
                for (i, hook_script) in hook_scripts.iter().enumerate() {
                    // Helper to run script
                    macro_rules! run_script {
                        () => {
                            tools::run_charm_script(
                                &self,
                                hook_name,
                                hook_script,
                                environment,
                                // Add hook index as script_id override to make it unique
                                // TODO: Fix clugy script id override
                                Some(&format!("{}_{}", hook_name, i)),
                            )?;

                            // If docker is enabled, update container configuration
                            if self.lucky_metadata.use_docker {
                                tools::apply_container_updates(self)?;
                            }
                        };
                    }

                    // If the script is asynchronous
                    if hook_script.is_async {
                        log::trace!("Running async hook script: {:#?}", hook_script);
                        // Spawn it in another thread
                        async_handles.push(s.spawn(move |_| -> anyhow::Result<()> {
                            run_script!();
                            Ok(())
                        }));

                    // If the script is synchronous
                    } else {
                        log::trace!("Running hook script: {:#?}", hook_script);
                        // Run it in place
                        run_script!();
                    }
                }

                // Join and handle any errors from async scripts
                for async_handle in async_handles {
                    async_handle.join().expect("Scoped thread paniced")?;
                }
            }

            Ok(())
        })
        .expect("Scoped thread paniced")?;

        // Run post-script hook handlers
        hook_handlers::handle_post_hook(&self, &hook_name).context(format!(
            r#"Error running internal hook handler for hook "{}""#,
            hook_name
        ))?;

        // Reply empty
        call.reply()?;

        Ok(())
    }
}

impl rpc::VarlinkInterface for LuckyDaemon {
    /// Stop the Lucky daemon
    fn stop_daemon(&self, call: &mut dyn rpc::Call_StopDaemon) -> varlink::Result<()> {
        log::info!("Shutting down server");
        // Set the stop_listening=true.
        self.stop_listening.store(true, Ordering::SeqCst);

        // Reply and exit
        call.reply()?;
        Ok(())
    }

    /// Handle the cron tick and run scheduled cron jobs
    fn cron_tick(
        &self,
        call: &mut dyn rpc::Call_CronTick,
        juju_context_id: String,
    ) -> varlink::Result<()> {
        // Set the Juju context
        std::env::set_var("JUJU_CONTEXT_ID", &juju_context_id);

        log::trace!("Cron tick");

        // Create environment map
        let mut environment: HashMap<String, String> = HashMap::new();
        environment.insert("JUJU_CONTEXT_ID".into(), juju_context_id);
        // Make environment a reference ( so it can be used in threads )
        let environment = &environment;

        // Get the last cron tick time and the current time
        let mut last_cron_tick = self.last_cron_tick.lock().unwrap();
        let now = Local::now();

        // Create a channel used to transefer our job results from their threads
        let (job_sender, job_receiver) = unbounded_channel();
        let job_sender_ref = &job_sender;

        // Create a thread scope allowing us to use references inside of the job threads
        thread_scope(|s| {
            // Loop through cron jobs and run them if necessary
            for (job_index, (schedule_str, scripts)) in
                self.lucky_metadata.cron_jobs.iter().enumerate()
            {
                let schedule: cron::Schedule = handle_err!(schedule_str.parse(), call);

                // If this job should be run
                if let Some(date) = schedule.after(&last_cron_tick).next() {
                    if date < now {
                        log::info!("Triggering cron job with schedule: {}", schedule_str);
                        // Spawn thread to run the job
                        s.spawn(move |ss| {
                            // For every script in the job
                            for (script_index, script) in scripts.iter().enumerate() {
                                let hook_name = "cron";

                                // helper to send error results over channel
                                macro_rules! send_if_error {
                                    ($result:expr) => {
                                        if let Err(e) = $result {
                                            job_sender_ref
                                                .send(Err(e))
                                                .expect("Channel dropped prematuresly");
                                            return Ok(());
                                        }
                                    };
                                }

                                // helper to run the script
                                macro_rules! run_script {
                                    () => {
                                        let run_result = tools::run_charm_script(
                                            &self,
                                            hook_name,
                                            &script,
                                            environment,
                                            // Add job and script index to script id override to
                                            // make sure script id is unique
                                            Some(&format!(
                                                "{}_{}_{}",
                                                hook_name, job_index, script_index
                                            )),
                                        );

                                        send_if_error!(run_result);

                                        // If docker is enabled, update container configuration
                                        if self.lucky_metadata.use_docker {
                                            send_if_error!(tools::apply_container_updates(self));
                                        }
                                    };
                                }

                                // If the script is asynchronous
                                if script.is_async {
                                    // Spawn it in another thread
                                    ss.spawn(move |_| {
                                        log::trace!(
                                            "Running async cron job script for schedule[{}]: {:#?}",
                                            schedule_str,
                                            script
                                        );
                                        run_script!();
                                        Ok::<(), Void>(())
                                    });

                                // If the script is synchronous
                                } else {
                                    log::trace!(
                                        "Running cron job script for schedule[{}]: {:#?}",
                                        schedule_str,
                                        script
                                    );
                                    // Run it in place
                                    run_script!();
                                }
                            }

                            Ok::<(), Void>(())
                        });
                    }
                }
            }

            Ok(())
        })
        .expect("Panic in scoped thread")?;

        // Close the channel
        drop(job_sender);

        // Loop through job results
        for job_result in job_receiver.iter() {
            // Handle any errors
            handle_err!(job_result, call);
        }

        // Update the last cron tick
        *last_cron_tick = Local::now();

        // Unset the Juju context as it will be invalid when the cron tick command exits
        std::env::remove_var("JUJU_CONTEXT_ID");

        // Reply empty
        call.reply()
    }

    /// Trigger a Juju hook
    fn trigger_hook(
        &self,
        call: &mut dyn rpc::Call_TriggerHook,
        hook_name: String,
        environment: HashMap<String, String>,
    ) -> varlink::Result<()> {
        // Set the hook environment variables
        for (var, value) in &environment {
            std::env::set_var(var, value);
        }

        log::info!("Triggering hook: {}", hook_name);

        // Trigger hook
        handle_err!(self._trigger_hook(call, &hook_name, &environment), call);

        // Unset the hook environment variables as they will be invalid when the hook exits
        for var in environment.keys() {
            std::env::remove_var(var);
        }

        log::info!("Done triggering hook: {}", hook_name);

        Ok(())
    }

    /// Set a script's status
    fn set_status(
        &self,
        call: &mut dyn rpc::Call_SetStatus,
        script_id: String,
        status: rpc::ScriptStatus,
    ) -> varlink::Result<()> {
        // Add status to script statuses
        let status: ScriptStatus = status.into();

        handle_err!(
            tools::set_script_status(&mut self.state.write().unwrap(), &script_id, status),
            call
        );

        // Reply
        call.reply()
    }

    /// Get a value in the unit local key-value store
    fn unit_kv_get(&self, call: &mut dyn rpc::Call_UnitKvGet, key: String) -> varlink::Result<()> {
        // Get with key
        let state = self.state.read().unwrap();
        let value = state.kv.get(&key);

        // Reply with value
        call.reply(value.map(|x| x.clone().into_inner()))
    }

    /// Set a value in the unit local key-value store
    fn unit_kv_set(
        &self,
        call: &mut dyn rpc::Call_UnitKvSet,
        data: HashMap<String, Option<String>>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        for (key, value) in data {
            // If a value has been provided
            if let Some(value) = value {
                log::debug!("Key-Value set: {} = {}", key, value);
                // Set key to value
                state.kv.insert(key, value.into());
            } else {
                log::debug!("Key-Value delete: {}", key);
                // Erase key
                state.kv.remove(&key);
            }
        }

        // Reply empty
        call.reply()
    }

    fn unit_kv_get_all(&self, call: &mut dyn rpc::Call_UnitKvGetAll) -> varlink::Result<()> {
        let state = self.state.read().unwrap();

        // Reply with pairs
        call.reply(
            state
                .kv
                .iter()
                .map(|(k, v)| rpc::UnitKvGetAll_Reply_pairs {
                    key: k.clone(),
                    value: v.clone().into_inner(),
                })
                .collect(),
        )
    }

    fn relation_set(
        &self,
        call: &mut dyn rpc::Call_RelationSet,
        data: HashMap<String, String>,
        relation_id: Option<String>,
        app: bool,
    ) -> varlink::Result<()> {
        handle_err!(juju::relation_set(data, relation_id, app), call);

        // Reply empty
        call.reply()
    }

    fn relation_get(
        &self,
        call: &mut dyn rpc::Call_RelationGet,
        relation: Option<rpc::RelationGet_Args_relation>,
        app: bool,
    ) -> varlink::Result<()> {
        call.reply(handle_err!(
            juju::relation_get(
                relation.map(|r| {
                    juju::SpecificRelation {
                        relation_id: r.relation_id,
                        remote_unit: r.remote_unit,
                    }
                }),
                app
            ),
            call
        ))
    }

    fn relation_list(
        &self,
        call: &mut dyn rpc::Call_RelationList,
        relation_id: Option<String>,
    ) -> varlink::Result<()> {
        call.reply(handle_err!(juju::relation_list(relation_id), call))
    }

    fn relation_ids(
        &self,
        call: &mut dyn rpc::Call_RelationIds,
        relation_name: String,
    ) -> varlink::Result<()> {
        call.reply(handle_err!(juju::relation_ids(&relation_name), call))
    }

    fn leader_is_leader(&self, call: &mut dyn rpc::Call_LeaderIsLeader) -> varlink::Result<()> {
        call.reply(handle_err!(juju::is_leader(), call))
    }

    fn leader_set(
        &self,
        call: &mut dyn rpc::Call_LeaderSet,
        data: HashMap<String, String>,
    ) -> varlink::Result<()> {
        handle_err!(juju::leader_set(data), call);

        // Reply empty
        call.reply()
    }

    fn leader_get(&self, call: &mut dyn rpc::Call_LeaderGet) -> varlink::Result<()> {
        call.reply(handle_err!(juju::leader_get(), call))
    }

    fn get_config(&self, call: &mut dyn rpc::Call_GetConfig) -> varlink::Result<()> {
        let state = self.state.read().unwrap();

        // Return all of the key-value config pairs
        call.reply(
            state
                .charm_config
                .iter()
                .map(|(k, v)| rpc::GetConfig_Reply_config {
                    key: k.clone(),
                    // Value is the string representation of the JSON value
                    value: v.clone().into_inner().to_string(),
                })
                .collect(),
        )
    }

    fn get_resource(
        &self,
        call: &mut dyn rpc::Call_GetResource,
        resource_name: String,
    ) -> varlink::Result<()> {
        // Reply with path to resource
        call.reply(handle_err!(juju::resource_get(&resource_name), call))
    }

    fn port_open(&self, call: &mut dyn rpc::Call_PortOpen, port: String) -> varlink::Result<()> {
        log::debug!("Opening port: {}", port);

        // Open the port
        handle_err!(juju::open_port(&port), call);

        // Reply empty
        call.reply()
    }

    fn port_close(&self, call: &mut dyn rpc::Call_PortClose, port: String) -> varlink::Result<()> {
        log::debug!("Closing port: {}", port);

        // Close the port
        handle_err!(juju::close_port(&port), call);

        // Reply empty
        call.reply()
    }

    fn port_close_all(&self, call: &mut dyn rpc::Call_PortCloseAll) -> varlink::Result<()> {
        // For each opened port
        for port in handle_err!(juju::opened_ports(), call) {
            log::debug!("Closing port: {}", port);

            // Close the port
            handle_err!(juju::close_port(&port), call);
        }

        // Reply empty
        call.reply()
    }

    fn port_get_opened(&self, call: &mut dyn rpc::Call_PortGetOpened) -> varlink::Result<()> {
        // Reply with port list
        call.reply(handle_err!(juju::opened_ports(), call))
    }

    fn get_private_address(
        &self,
        call: &mut dyn rpc::Call_GetPrivateAddress,
    ) -> varlink::Result<()> {
        call.reply(handle_err!(juju::unit_get_private_address(), call))
    }

    fn get_public_address(&self, call: &mut dyn rpc::Call_GetPublicAddress) -> varlink::Result<()> {
        call.reply(handle_err!(juju::unit_get_public_address(), call))
    }

    fn container_apply(&self, call: &mut dyn rpc::Call_ContainerApply) -> varlink::Result<()> {
        if self.lucky_metadata.use_docker {
            handle_err!(tools::apply_container_updates(self), call);
        }

        call.reply()
    }

    fn container_delete(
        &self,
        call: &mut dyn rpc::Call_ContainerDelete,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let container = match &container_name {
            Some(container_name) => state.named_containers.get_mut(container_name),
            None => state.default_container.as_mut(),
        };

        if let Some(container) = container {
            // Mark container as needing removal
            container.update(|c| c.pending_removal = true);
        }

        // Reply empty
        call.reply()
    }

    fn container_set_entrypoint(
        &self,
        call: &mut dyn rpc::Call_ContainerSetEntrypoint,
        entrypoint: Option<String>,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // If a container was specified
        if let Some(name) = container_name {
            // If specified container exists
            if let Some(container) = state.named_containers.get_mut(&name) {
                log::debug!(
                    "Set Docker entrypoint [{}]: {}",
                    name,
                    entrypoint.as_ref().unwrap_or(&"unset".to_string())
                );
                // Set entrypoint
                container.update(|c| c.config.entrypoint = entrypoint);
            }

        // If no container was specified
        } else {
            // If default container exists
            if let Some(container) = &mut state.default_container {
                log::debug!(
                    "Set Docker entrypoint: {}",
                    entrypoint.as_ref().unwrap_or(&"unset".to_string())
                );
                // Set entrypoint
                container.update(|c| c.config.entrypoint = entrypoint);
            }
        }

        call.reply()
    }

    fn container_set_command(
        &self,
        call: &mut dyn rpc::Call_ContainerSetCommand,
        command: Option<Vec<String>>,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // If a container was specified
        if let Some(name) = container_name {
            // If specified container exists
            if let Some(container) = state.named_containers.get_mut(&name) {
                log::debug!(
                    "Set Docker command [{}]: {}",
                    name,
                    command.as_ref().map_or("unset".into(), |x| x.join(" "))
                );
                // Set command
                container.update(|c| c.config.command = command);
            }

        // If no container was specified
        } else {
            // If default container exists
            if let Some(container) = &mut state.default_container {
                log::debug!(
                    "Set Docker command: {}",
                    command.as_ref().map_or("unset".into(), |x| x.join(" "))
                );
                // Set command
                container.update(|c| c.config.command = command);
            }
        }

        // Reply empty
        call.reply()
    }

    // The uncollapsed if is easier to understand in this case
    #[allow(clippy::collapsible_if)]
    fn container_image_set(
        &self,
        call: &mut dyn rpc::Call_ContainerImageSet,
        image: String,
        container_name: Option<String>,
        no_pull: bool,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // If this is for a named container
        if let Some(name) = container_name {
            if let Some(container) = state.named_containers.get_mut(&name) {
                log::debug!("Set Docker image [{}]: {}", name, image);
                // Set the image on existing container
                container.update(|c| {
                    c.config.image = image;
                    c.pull_image = !no_pull;
                });
            } else {
                log::debug!("Adding new docker container: {}", name);
                log::debug!("Set Docker image [{}]: {}", name, image);
                // Create a new container with the given image
                let mut new_container = ContainerInfo::new(&image);
                new_container.pull_image = !no_pull;
                state.named_containers.insert(name, new_container.into());
            }
        // If this is for the default container
        } else {
            if let Some(container) = &mut state.default_container {
                log::debug!("Set container image: {}", image);
                // Set the image on existing container
                container.update(|c| {
                    c.config.image = image;
                    c.pull_image = !no_pull;
                });
            } else {
                log::debug!("Adding container");
                log::debug!("Set container image: {}", image);
                // Create a new container with the given image
                let mut new_container = ContainerInfo::new(&image);
                new_container.pull_image = !no_pull;
                state.default_container = Some(new_container.into());
            }
        }

        // Reply empty
        call.reply()
    }

    // The uncollapsed if is easier to understand in this case
    #[allow(clippy::collapsible_if)]
    fn container_image_get(
        &self,
        call: &mut dyn rpc::Call_ContainerImageGet,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let state = self.state.read().unwrap();

        // If this is for a named container
        if let Some(name) = container_name {
            if let Some(container) = state.named_containers.get(&name) {
                call.reply(Some(container.config.image.clone()))
            } else {
                call.reply(None)
            }
        // If this is for the default container
        } else {
            if let Some(container) = &state.default_container {
                call.reply(Some(container.config.image.clone()))
            } else {
                call.reply(None)
            }
        }
    }

    fn container_env_get(
        &self,
        call: &mut dyn rpc::Call_ContainerEnvGet,
        key: String,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let state = self.state.read().unwrap();

        // Get the config for the requested container
        let container = match &container_name {
            Some(container_name) => state.named_containers.get(container_name),
            None => state.default_container.as_ref(),
        };

        // If the specified container exists
        if let Some(container) = container {
            // Reply with the environment variable's value
            call.reply(container.config.env_vars.get(&key).map(ToOwned::to_owned))
        } else {
            // Reply with None
            call.reply(None)
        }
    }

    fn container_env_get_all(
        &self,
        call: &mut dyn rpc::Call_ContainerEnvGetAll,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let state = self.state.read().unwrap();

        // This call must be called with more
        if !call.wants_more() {
            call.reply_requires_more()?;
            return Ok(());
        }

        // Get the config for the requested container
        let container = match &container_name {
            Some(container_name) => state.named_containers.get(container_name),
            None => state.default_container.as_ref(),
        };

        // If the container exists
        if let Some(container) = container {
            // Loop through key-value pairs and return result to client
            let pairs: Vec<rpc::ContainerEnvGetAll_Reply_pairs> = container
                .config
                .env_vars
                .iter()
                .map(|(k, v)| rpc::ContainerEnvGetAll_Reply_pairs {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect();

            // Reply with pairs
            call.reply(pairs)

        // If the container doesn't exist
        } else {
            // Reply with empty array
            call.reply(vec![])
        }
    }

    /// Set a value in the unit local key-value store
    fn container_env_set(
        &self,
        call: &mut dyn rpc::Call_ContainerEnvSet,
        vars: HashMap<String, Option<String>>,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container = match &container_name {
            Some(container_name) => state.named_containers.get_mut(container_name),
            None => state.default_container.as_mut(),
        };

        if let Some(container) = &mut container {
            for (key, value) in vars {
                // If a value has been provided
                if let Some(value) = value {
                    log::debug!("Container env set: {} = {}", key, value);
                    // Set key to value
                    container.update(|c| {
                        c.config.env_vars.insert(key, value);
                    });
                } else {
                    log::debug!("Container env deleted: {}", key);
                    // Erase key
                    container.update(|c| {
                        c.config.env_vars.remove(&key);
                    });
                }
            }
        }

        // Reply empty
        call.reply()
    }

    fn container_volume_add(
        &self,
        call: &mut dyn rpc::Call_ContainerVolumeAdd,
        source: String,
        target: String,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container_log_name = None;
        let mut container = match &container_name {
            Some(container_name) => {
                container_log_name = Some(container_name.clone());
                state.named_containers.get_mut(container_name)
            }
            None => state.default_container.as_mut(),
        };

        if let Some(container) = &mut container {
            log::debug!(
                "Creating container volume{}: {}:{}",
                container_log_name.map_or("".into(), |x| format!("[{}]", x)),
                source,
                target
            );
            // Add volume to container config
            container.update(|c| {
                c.config
                    .volumes
                    .insert(VolumeTarget(target), VolumeSource(source));
            });
        }

        // Reply empty
        call.reply()
    }

    fn container_volume_remove(
        &self,
        call: &mut dyn rpc::Call_ContainerVolumeRemove,
        target: String,
        delete_data: bool,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container_log_name = None;
        let mut container = match &container_name {
            Some(container_name) => {
                container_log_name = Some(container_name.clone());
                state.named_containers.get_mut(container_name)
            }
            None => state.default_container.as_mut(),
        };

        // If the specified container exists
        if let Some(container) = &mut container {
            log::debug!(
                "Deleting container volume{}: {}",
                container_log_name.map_or("".into(), |x| format!("[{}]", x)),
                target
            );

            // Remove the container volume
            container.update(|container| {
                let volumes = &mut container.config.volumes;

                // Get source and remove from volume list
                let source = volumes.remove(&VolumeTarget(target));

                // If there is a volume for the given target path
                if let Some(source) = source {
                    // If we should delete the source data
                    if delete_data {
                        // If there are no other volumes with the same source
                        if volumes.values().find(|&x| *x == source).is_none() {
                            log::debug!("Deleting volume data source: {}", &*source);

                            // Delete data
                            if source.starts_with('/') {
                                handle_err!(std::fs::remove_dir_all(&*source), call);
                            } else {
                                handle_err!(
                                    std::fs::remove_dir_all(
                                        self.lucky_data_dir.join(VOLUME_DIR).join(&*source)
                                    ),
                                    call
                                );
                            }

                            call.reply(true /* data deleted */)?;
                            return Ok(());
                        }
                    }
                }

                call.reply(false /* no data deleted */)
            })

        // If the specified container didn't exist
        } else {
            call.reply(false /* no data deleted */)
        }
    }

    fn container_volume_get_all(
        &self,
        call: &mut dyn rpc::Call_ContainerVolumeGetAll,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container = match &container_name {
            Some(container_name) => state.named_containers.get_mut(container_name),
            None => state.default_container.as_mut(),
        };

        // If the container exists
        if let Some(container) = &mut container {
            // Reply wth volumes
            call.reply(
                container
                    .config
                    .volumes
                    .iter()
                    .map(
                        |(target, source)| rpc::ContainerVolumeGetAll_Reply_volumes {
                            source: (**source).clone(),
                            target: (**target).clone(),
                        },
                    )
                    .collect(),
            )
        } else {
            // Reply empty
            call.reply(vec![])
        }
    }

    fn container_port_add(
        &self,
        call: &mut dyn rpc::Call_ContainerPortAdd,
        host_port: i64,
        container_port: i64,
        protocol: String,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container_log_name = None;
        let mut container = match &container_name {
            Some(container_name) => {
                container_log_name = Some(container_name.clone());
                state.named_containers.get_mut(container_name)
            }
            None => state.default_container.as_mut(),
        };

        if let Some(container) = &mut container {
            log::debug!(
                "Adding port to container{}: {}:{}/{}",
                container_log_name.map_or("".into(), |x| format!("[{}]", x)),
                host_port,
                container_port,
                protocol
            );

            let host_port = handle_err!(host_port.try_into().context("Invalid port number"), call);
            let container_port = handle_err!(
                container_port.try_into().context("Invalid port number"),
                call
            );

            let port_binding = PortBinding {
                host_port,
                container_port,
                protocol,
            };

            // If there are other port bindings with the same protocol and host or container port
            // but isn't the exact same port binding
            if let Some(offending_binding) = container.config.ports.iter().find(|&b| {
                // With the same host port
                (b.host_port == port_binding.host_port
                        // or with the same container port
                        || b.container_port == port_binding.container_port)
                        // and with the same protocol
                        && b.protocol == port_binding.protocol
                        // and not the same exact port binding
                        && b != &port_binding
            }) {
                // Throw an error because we can't add port binding that has the same port as
                // another.
                call.reply_error(format!(
                    concat!(
                        "Not adding port binding `{}` because it conflicts with a port binding ",
                        "already added to the container: {}"
                    ),
                    port_binding, offending_binding
                ))?;
                return Ok(());
            }

            container.update(|c| {
                c.config.ports.insert(port_binding);
            });
        }

        // Reply empty
        call.reply()
    }

    fn container_port_remove(
        &self,
        call: &mut dyn rpc::Call_ContainerPortRemove,
        host_port: i64,
        container_port: i64,
        protocol: String,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container_log_name = None;
        let mut container = match &container_name {
            Some(container_name) => {
                container_log_name = Some(container_name.clone());
                state.named_containers.get_mut(container_name)
            }
            None => state.default_container.as_mut(),
        };

        if let Some(container) = &mut container {
            log::debug!(
                "Removing port from container{}: {}:{}/{}",
                container_log_name.map_or("".into(), |x| format!("[{}]", x)),
                host_port,
                container_port,
                protocol
            );

            container.update(|c| {
                c.config.ports.remove(&PortBinding {
                    host_port: handle_err!(
                        host_port.try_into().context("Invalid port number"),
                        call
                    ),
                    container_port: handle_err!(
                        container_port.try_into().context("Invalid port number"),
                        call
                    ),
                    protocol,
                });

                Ok(())
            })?;
        }

        // Reply empty
        call.reply()
    }

    fn container_port_remove_all(
        &self,
        call: &mut dyn rpc::Call_ContainerPortRemoveAll,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container_log_name = None;
        let container = match &container_name {
            Some(container_name) => {
                container_log_name = Some(container_name.clone());
                state.named_containers.get_mut(container_name)
            }
            None => state.default_container.as_mut(),
        };

        if let Some(container) = container {
            // For each port
            for port_binding in &container.config.ports.clone() {
                log::debug!(
                    "Removing port from container{}: {}:{}/{}",
                    container_log_name
                        .as_ref()
                        .map_or("".into(), |x| format!("[{}]", x)),
                    port_binding.host_port,
                    port_binding.container_port,
                    port_binding.protocol
                );

                // Remove the port
                container.update(|c| {
                    c.config.ports.remove(&port_binding);
                });
            }
        }

        // Reply empty
        call.reply()
    }

    fn container_port_get_all(
        &self,
        call: &mut dyn rpc::Call_ContainerPortGetAll,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let state = self.state.read().unwrap();

        // Get the config for the requested container
        let mut container = match &container_name {
            Some(container_name) => state.named_containers.get(container_name),
            None => state.default_container.as_ref(),
        };

        if let Some(container) = &mut container {
            call.reply(
                container
                    .config
                    .ports
                    .iter()
                    .map(|port| rpc::ContainerPortGetAll_Reply_ports {
                        container_port: port.container_port.into(),
                        host_port: port.host_port.into(),
                        protocol: port.protocol.clone(),
                    })
                    .collect(),
            )
        } else {
            // Reply empty
            call.reply(vec![])
        }
    }

    fn container_network_set(
        &self,
        call: &mut dyn rpc::Call_ContainerNetworkSet,
        network_name: Option<String>,
        container_name: Option<String>,
    ) -> varlink::Result<()> {
        let mut state = self.state.write().unwrap();

        // Get the config for the requested container
        let mut container_log_name = None;
        let mut container = match &container_name {
            Some(container_name) => {
                container_log_name = Some(container_name.clone());
                state.named_containers.get_mut(container_name)
            }
            None => state.default_container.as_mut(),
        };

        if let Some(container) = &mut container {
            log::debug!(
                "Setting container network{}: {}",
                container_log_name.map_or("".into(), |x| format!("[{}]", x)),
                network_name.as_ref().unwrap_or(&"unset".to_string()),
            );

            container.update(|c| c.config.network = network_name);
        }

        // Reply empty
        call.reply()
    }
}

impl Drop for LuckyDaemon {
    /// Persist the daeomon state before it is dropped
    fn drop(&mut self) {
        tools::flush_state(&self).unwrap_or_else(|e| log::error!("{:?}", e));
    }
}

//
// Client Helpers
//

/// Get the server service
pub(crate) fn get_service(options: LuckyDaemonOptions) -> varlink::VarlinkService {
    // Create a new daemon instance
    let daemon_instance = LuckyDaemon::new(options);

    // Return the varlink service
    varlink::VarlinkService::new(
        "lucky.rpc",
        "lucky daemon",
        clap::crate_version!(),
        "https://github.com/katharostech/lucky",
        vec![Box::new(rpc::new(Box::new(daemon_instance)))],
    )
}

/// Get the client given a connection
pub(crate) fn get_client(connection: Arc<RwLock<varlink::Connection>>) -> rpc::VarlinkClient {
    // Return the varlink client
    rpc::VarlinkClient::new(connection)
}
