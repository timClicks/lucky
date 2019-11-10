use std::fs;
use std::path::Path;

use anyhow::Context;
use clap::{App, Arg, ArgMatches};
use walkdir::WalkDir;

use crate::cli::doc;
use crate::types::CharmMetadata;

#[rustfmt::skip]
/// Return the `build` subcommand
pub(crate) fn get_subcommand<'a>() -> App<'a> {
    crate::cli::new_app("build")
        .about("Build a Lucky charm and make it ready for deployment")
        .long_about(concat!(
            "Build a Lucky charm and make it ready for deployment to the Juju ",
            "server or charm store"))
        .arg(doc::get_arg())
        .help_heading("LUCKY_INSTALL_SOURCE")
        .arg(Arg::with_name("use_local_lucky")
            .help("Build the charm with the local copy of lucky included")
            .long_help(include_str!("build/arg_use-local-lucky.txt"))
            .long("use-local-lucky")
            .short('l'))
        .stop_custom_headings()
        .arg(Arg::with_name("build_dir")
            .help("The directory to put the built charm in")
            .long_help(concat!(
                "The directory to put the built charm in. The built charm will be in ",
                "`build_dir/charm_name`."))
            .long("build-dir")
            .short('b')
            .default_value("build"))
        .arg(Arg::with_name("charm_dir")
            .help("The path to the charm you want to build")
            .required(false)
            .default_value("."))
}

/// Run the `build` subcommand
pub(crate) fn run(args: &ArgMatches) -> anyhow::Result<()> {
    // Show the docs if necessary
    doc::show_doc(
        &args,
        get_subcommand(),
        "lucky_charm_build",
        include_str!("build/build.md"),
    )?;

    // Get charm dir
    let charm_path = Path::new(
        args.value_of("charm_dir")
            .expect("Missing required argument: charm_dir"),
    );

    // Create build dir
    let build_dir = Path::new(
        args.value_of("build_dir")
            .expect("Missing required argument: build_dir"),
    );
    fs::create_dir_all(&build_dir).context("Could not create build directory")?;

    // Load charm metadata
    let metadata_path = if charm_path.join("metadata.yaml").exists() {
        charm_path.join("metadata.yaml")
    } else {
        charm_path.join("metadata.yml")
    };
    if !metadata_path.exists() {
        anyhow::bail!(
            "Could not locate a metadata.yaml file in the given charm directory: {:?}",
            &charm_path.canonicalize()?
        );
    }
    let metadata_content = fs::read_to_string(&metadata_path)
        .context(format!("Couldn't read file: {:?}", metadata_path))?;
    let metadata: CharmMetadata =
        serde_yaml::from_str(&metadata_content).context("Couldn't parse charm metadata YAML")?;
    let charm_name = &metadata.name;
    let target_dir = build_dir.join(charm_name);

    // Clear the target directory
    if target_dir.exists() {
        fs::remove_dir_all(&target_dir).context(format!(
            "Could not remove build target directory: {:?}",
            target_dir
        ))?;
    }

    // Copy charm contents to build directory
    let build_dir_canonical = build_dir.canonicalize()?;
    for entry in WalkDir::new(charm_path).into_iter().filter_entry(|e| {
        // Don't include any files in the build dir
        let entry_path = if let Ok(path) = e.path().canonicalize() {
            path
        } else {
            return false;
        };
        !entry_path.strip_prefix(&build_dir_canonical).is_ok()
    }) {
        let entry = entry?;
        let relative_path = entry
            .path()
            .strip_prefix(charm_path)
            .expect("Internal error parsing build paths");
        let source_path = entry.path();
        let target_path = target_dir.join(relative_path);

        // Create parent dir
        if let Some(parent) = &target_path.parent() {
            if !parent.exists() {
                fs::create_dir_all(&parent)
                    .context(format!("Could not create directory: {:?}", parent))?;
            }
        }

        // Copy file
        if source_path.is_file() {
            fs::copy(source_path, &target_path).context(format!(
                "Could not copy file {:?} to {:?}",
                source_path, &target_path
            ))?;
        }
    }

    // Create bin dir
    let bin_dir = target_dir.join("bin");
    if !bin_dir.exists() {
        fs::create_dir_all(&bin_dir)
            .context(format!("Could not create dir: {:?}", bin_dir))?;
    }

    // Create hook dir
    let hook_dir = target_dir.join("hooks");
    if !hook_dir.exists() {
        fs::create_dir_all(&hook_dir)
            .context(format!("Could not create dir: {:?}", hook_dir))?;
    }

    // Copy in Lucky binary
    if !args.is_present("use_local_lucky") {
        // We will require the -l flag until our first release
        anyhow::bail!(concat!(
            "Currently the --use-local-lucky or -l flag is required to build a charm. Once we ",
            "have made our first release, lucky will be able to automatically download the ",
            "required version from GitHub so that it can run on whatever architecture the charm ",
            "is deployed to"
        ));
    } else {
        // Copy in the Lucky executable
        let lucky_path = bin_dir.join("lucky");
        let executable_path = std::env::current_exe()?;
        fs::copy(&executable_path, &lucky_path)?;

        // Create install hook
        let install_hook_path = hook_dir.join("install");
        fs::write(&install_hook_path, include_str!("build/install-hook.sh"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
             fs::set_permissions(&install_hook_path, fs::Permissions::from_mode(0o755)).context(
                format!("Could not set permissions on created file: {:?}", &install_hook_path),
            )?;
        }
    }

    // Create Juju hooks


    Ok(())
}
