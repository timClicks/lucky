use clap::{App, Arg, ArgMatches};

use crate::cli::*;

mod apply_updates;
mod delete;
mod env;
mod image;
mod port;
mod set_command;
mod set_entrypoint;
mod set_network;
mod volume;

pub(super) struct ContainerSubcommand;

impl<'a> CliCommand<'a> for ContainerSubcommand {
    fn get_name(&self) -> &'static str {
        "container"
    }

    #[rustfmt::skip]
    fn get_app(&self) -> App<'a> {
        self.get_base_app()
            .about("Manipulate the charm's container(s)")
            .setting(AppSettings::SubcommandRequiredElseHelp)
    }

    fn get_subcommands(&self) -> Vec<Box<dyn CliCommand<'a>>> {
        vec![
            Box::new(image::ImageSubcommand),
            Box::new(apply_updates::ApplyUpdatesSubcommand),
            Box::new(env::EnvSubcommand),
            Box::new(set_entrypoint::SetEntrypointSubcommand),
            Box::new(set_command::SetCommandSubcommand),
            Box::new(volume::VolumeSubcommand),
            Box::new(delete::DeleteSubcommand),
            Box::new(port::PortSubcommand),
            Box::new(set_network::SetNetworkSubcommand),
        ]
    }

    fn get_doc(&self) -> Option<CliDoc> {
        Some(CliDoc {
            name: "lucky_client_container",
            content: include_str!("cli_help/container.md"),
        })
    }

    fn execute_command(&self, _args: &ArgMatches, data: CliData) -> anyhow::Result<CliData> {
        Ok(data)
    }
}

/// Return the "--container" argument for use in subcommands
fn container_arg<'a>() -> Arg<'a> {
    Arg::with_name("container")
        .help(concat!(
            "The name of the container to update. If not specified the default container will be ",
            "used"
        ))
        .short('c')
        .long("container")
        .value_name("name")
        .takes_value(true)
}
