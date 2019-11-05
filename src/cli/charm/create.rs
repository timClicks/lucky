use std::fs;
use std::io;
use std::path::PathBuf;

use clap::{App, Arg, ArgMatches, SubCommand};
use handlebars::Handlebars;
use rprompt::prompt_reply_stdout;
use serde::Serialize;

#[derive(Serialize)]
struct TemplateData {
    pub charm_display_name: String,
    pub charm_name: String,
    pub charm_summary: String,
    pub charm_maintainer: String,
}

impl Default for TemplateData {
    fn default() -> Self {
        TemplateData {
            charm_display_name: String::from("My App"),
            charm_name: String::from("my_app"),
            charm_summary: String::from("A short summary of my app."),
            charm_maintainer: String::from("John Doe <johndoe@emailprovider.com>"),
        }
    }
}

#[rustfmt::skip]
pub(crate) fn get_subcommand<'a, 'b>() -> App<'a, 'b> {
    SubCommand::with_name("create")
        .about("Create a new lucky charm.")
        .arg(Arg::with_name("target_dir")
            .help("The directory to create the charm in")
            .required(true))
        .arg(Arg::with_name("use_defaults")
            .long("use-defaults")
            .short("D")
            .help("Do not prompt and use default values for unprovided fields"))
        .arg(Arg::with_name("charm_name")
            .long("name")
            .short("n")
            .help("The name of the charm. Defaults to the target_dir")
            .takes_value(true))
        .arg(Arg::with_name("display_name")
            .long("display-name")
            .short("d")
            .help("The display name of the charm ( may contain spaces )")
            .takes_value(true))
        .arg(Arg::with_name("charm_summary")
            .long("summary")
            .short("s")
            .help("Short description of the charm")
            .takes_value(true))
        .arg(Arg::with_name("charm_maintainer")
            .long("maintainer")
            .short("m")
            .help("The charm maintainer")
            .takes_value(true))       
}

pub(crate) fn run(args: &ArgMatches) {
    // Create handlebars tempate engine
    let mut handlebars = Handlebars::new();
    // Clear the escape handler
    handlebars.register_escape_fn(handlebars::no_escape);

    // Initialize template
    let mut template_settings = TemplateData::default();

    // Set charm name
    if let Some(value) = args.value_of("charm_name") {
        template_settings.charm_name = String::from(value);
    }

    // Set display name
    if let Some(value) = args.value_of("display_name") {
        template_settings.charm_display_name = String::from(value);
    }

    // Set charm summary
    if let Some(value) = args.value_of("charm_summary") {
        template_settings.charm_summary = String::from(value);
    }

    // Set charm name
    if let Some(value) = args.value_of("charm_maintainer") {
        template_settings.charm_maintainer = String::from(value);
    }

    // If the defaults flag is not provided
    if !args.is_present("use_defaults") {
        // Prompt for missing display name
        if !args.is_present("display_name") {
            let default = args.value_of("target_dir").expect("Missing target dir");
            let response = prompt_reply_stdout(&format!("Display name [{}]: ", default)).unwrap();
            let value: String;
            if response.trim() == "" {
                value = String::from(default);
            } else {
                value = response;
            }
            template_settings.charm_display_name = value;
        }

        // Prompt for missing name
        if !args.is_present("charm_name") {
            let default = &template_settings
                .charm_display_name
                .replace(" ", "_")
                .to_lowercase();
            let response = prompt_reply_stdout(&format!("Charm name [{}]: ", default)).unwrap();
            let value: String;
            if response.trim() == "" {
                value = String::from(default);
            } else {
                value = response;
            }
            template_settings.charm_name = value;
        }

        // Prompt for missing summary
        if !args.is_present("charm_summary") {
            let default = &template_settings.charm_summary;
            let response = prompt_reply_stdout(&format!("Charm summary [{}]: ", default)).unwrap();
            let value: String;
            if response.trim() == "" {
                value = String::from(default);
            } else {
                value = response;
            }
            template_settings.charm_summary = value;
        }

        // Prompt for missing maintainer
        if !args.is_present("charm_maintainer") {
            let default = &template_settings.charm_maintainer;
            let response =
                prompt_reply_stdout(&format!("Charm maintainer [{}]: ", default)).unwrap();
            let value: String;
            if response.trim() == "" {
                value = String::from(default);
            } else {
                value = response;
            }
            template_settings.charm_maintainer = value;
        }

    // User skipped prompts and opt-ed for default values
    } else {
        if !args.is_present("display_name") {
            template_settings.charm_display_name =
                String::from(args.value_of("target_dir").expect("Missing target dir"));
        }
        if !args.is_present("charm_name") {
            template_settings.charm_name = template_settings
                .charm_display_name
                .replace(" ", "_")
                .to_lowercase();
        }
    }

    // Create the zip reader from the embeded charm template archive
    let zip_reader = std::io::Cursor::new(crate::CHARM_TEMPLATE_ARCHIVE);
    let mut zip = zip::ZipArchive::new(zip_reader).unwrap();

    // Iterate through the items in the zip
    for i in 0..zip.len() {
        let mut file = zip.by_index(i).unwrap();
        let mut outpath = PathBuf::from(args.value_of("target_dir").unwrap());
        outpath.push(file.sanitized_name());

        // If file entry is a directory
        if file.name().ends_with('/') {
            // Create a directory
            fs::create_dir_all(&outpath).unwrap();

        // If it is a file
        } else {
            // If the file has a parent
            if let Some(p) = outpath.parent() {
                // If the parent doesn't exist yet
                if !p.exists() {
                    // Create the parent directories
                    fs::create_dir_all(&p).unwrap();
                }
            }

            // If the file is a handlebars template
            if file.name().ends_with(".hbs") {
                // Strip the `.hbs` extension from the output file path
                outpath =
                    PathBuf::from(&outpath.to_str().unwrap().rsplitn(2, ".hbs").nth(1).unwrap());

                // Render the template to the output file
                let mut outfile = fs::File::create(&outpath).unwrap();
                handlebars
                    .render_template_source_to_write(&mut file, &template_settings, &mut outfile)
                    .unwrap();

            // If it is a normal file
            } else {
                // Create file and write contents
                let mut outfile = fs::File::create(&outpath).unwrap();
                io::copy(&mut file, &mut outfile).unwrap();
            }
        }

        // If we are on a unix system
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // If there is a mode set for the file in the zip
            if let Some(mode) = file.unix_mode() {
                // Set ther permissions on the created file
                fs::set_permissions(&outpath, fs::Permissions::from_mode(mode)).unwrap();
            }
        }
    }
}