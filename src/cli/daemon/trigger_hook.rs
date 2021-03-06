use clap::{App, Arg, ArgMatches};

use std::collections::HashMap;

use crate::cli::daemon::{get_daemon_client, get_daemon_connection_args, get_daemon_socket_path};
use crate::cli::*;
use crate::rpc::VarlinkClientInterface;

pub(super) struct TriggerHookSubcommand;

impl<'a> CliCommand<'a> for TriggerHookSubcommand {
    fn get_name(&self) -> &'static str {
        "trigger-hook"
    }

    fn get_app(&self) -> App<'a> {
        self.get_base_app()
            .about("Run a hook through the Lucky daemon")
            .arg(Arg::with_name("hook_name").help("The name of the hook to trigger"))
            .args(&get_daemon_connection_args())
    }

    fn get_subcommands(&self) -> Vec<Box<dyn CliCommand<'a>>> {
        vec![]
    }

    fn get_doc(&self) -> Option<CliDoc> {
        None
    }

    fn execute_command(&self, args: &ArgMatches, data: CliData) -> anyhow::Result<CliData> {
        let socket_path = get_daemon_socket_path(args);

        let hook_name = args
            .value_of("hook_name")
            .expect("Missing required argument: hook_name")
            .to_string();

        // Populate environment variables the Lucky daemon may need for executing the hook
        let mut environment: HashMap<String, String> = HashMap::new();
        for &var in &[
            "JUJU_RELATION",
            "JUJU_RELATION_ID",
            "JUJU_REMOTE_UNIT",
            "JUJU_CONTEXT_ID",
            "JUJU_REMOTE_APP",
        ] {
            if let Ok(value) = std::env::var(var) {
                environment.insert(var.into(), value);
            }
        }

        // Connect to lucky daemon
        let mut client = get_daemon_client(&socket_path)?;

        log::info!(r#"Triggering hook "{}""#, &hook_name);

        // Just trigger the hook and exit
        client.trigger_hook(hook_name.clone(), environment).call()?;

        log::info!(r#"Done running hook "{}""#, &hook_name);

        Ok(data)
    }
}
