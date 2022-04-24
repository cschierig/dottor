#![feature(let_chains)]

use std::env;

use clap::arg;
use clap::ArgMatches;
use clap::{command, Command};
use config::Configuration;
use config::RootConfiguration;
use err::Error;
use git2::Repository;
use globset::Glob;
use globset::GlobMatcher;
use globset::GlobSetBuilder;
use io_util::check_dir_null_or_empty;
use io_util::check_not_empty;
use io_util::check_root_present;
use io_util::prompt_bool;
use io_util::write;
use path_abs::FileRead;
use path_abs::PathAbs;
use path_abs::PathDir;
use path_abs::PathFile;
use path_abs::PathInfo;
use path_abs::PathOps;
use path_abs::PathType;
use similar::ChangeTag;
use similar::TextDiff;
use structure::Structure;

mod config;
mod err;
mod io_util;
mod structure;

mod subcommands {
    pub const CONFIG: &str = "config";
    pub const DEPLOY: &str = "deploy";
    pub const INIT: &str = "init";
    pub mod config {
        pub const CREATE: &str = "create";
        pub const DELETE: &str = "delete";
        pub const PULL: &str = "pull";
    }
}

fn main() {
    let structure = structure::Structure::resolve().unwrap();

    let matches = command!()
        .propagate_version(true)
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new(subcommands::CONFIG)
                .arg_required_else_help(true)
                .about("Manage your individual dotfile configurations")
                .subcommand(
                    Command::new(subcommands::config::CREATE)
                        .about("Create a new configuration")
                        .arg_required_else_help(true)
                        .arg(arg!(<NAME> "The name of the configuration")),
                )
                .subcommand(
                    Command::new(subcommands::config::DELETE)
                        .about("Delete a configuration")
                        .arg_required_else_help(true)
                        .arg(arg!(<NAME> "The name of the configuration")),
                )
                .subcommand(
                    Command::new(subcommands::config::PULL)
                        .about(
                            "Pull changes from the deployed configuration into the dotfiles repo",
                        )
                        .arg_required_else_help(true)
                        .arg(arg!(<NAME> "The name of the configuration")),
                )
                .arg(arg!([NAME] "The name of the configuration")),
        )
        .subcommand(
            Command::new(subcommands::DEPLOY)
                .about("Deploy your configurations to the system")
                .arg(arg!([name] "The name of the configuration")),
        )
        .subcommand(
            Command::new(subcommands::INIT)
                .about("Initialize a new dotfiles repo in the current directory"),
        )
        .get_matches();

    if let Err(error) = match matches.subcommand() {
        Some((subcommands::INIT, _)) => init(),
        Some((subcommands::CONFIG, sub_matches)) => config(sub_matches, structure),
        Some((subcommands::DEPLOY, sub_matches)) => deploy(sub_matches, structure),
        _ => Ok(()),
    } {
        eprintln!("{} Aborting!", error);
    }
}

fn init() -> err::Result<()> {
    // check that we don't accidentally populate an existing directory
    check_not_empty(&PathDir::current_dir()?)?;
    // create the default root configuration
    write(
        &PathAbs::new(config::ROOT_PATH)?,
        toml::to_string_pretty(&RootConfiguration::default())
            .map_err(|_| Error::new("could not create root configuration."))?
            .as_bytes(),
    )?;

    // initialize a new git repository
    match Repository::init("./") {
        Ok(_) => Ok(()),
        Err(_) => Err(Error::new("Could not initialize git repository.")),
    }
}

/// verifies that the structure of the dotfiles folder is correct
/// It does not however verify the configs inside of the folder
fn verify_structure(structure: Option<Structure>) -> err::Result<Structure> {
    match structure {
        Some(value) => Ok(value),
        None => Err(Error::new(
            "Structure of the dotfiles repository is invalid.",
        )),
    }
}

/// runs the config command
fn config(matches: &ArgMatches, structure: Option<Structure>) -> err::Result<()> {
    check_root_present()?;
    let structure = verify_structure(structure)?;

    match matches.subcommand() {
        Some((subcommands::config::CREATE, sub_matches)) => config_create(sub_matches, structure),
        Some((subcommands::config::DELETE, sub_matches)) => config_delete(sub_matches, structure),
        Some((subcommands::config::PULL, sub_matches)) => config_pull(sub_matches, structure),
        None => config_create(matches, structure), // if no subcommand was provided, create is implied (like e.g. git branch)
        _ => Err(err::Error::new("Invalid subcommand")),
    }
}

/// creates a new config
fn config_create(matches: &ArgMatches, structure: Structure) -> err::Result<()> {
    let name = matches.value_of("NAME").expect("name not provided");
    if structure.configs.contains_key(name) {
        return Err(Error::from_string(format!(
            "There already exists a config with the name '{}'",
            name
        )));
    }
    config::create_config(name)
}

/// deletes a config
fn config_delete(matches: &ArgMatches, structure: Structure) -> err::Result<()> {
    let name = matches.value_of("NAME").expect("name not provided");
    if structure.configs.contains_key(name) {
        return Err(Error::from_string(format!(
            "There is no config with the name '{}'",
            name
        )));
    }
    config::delete_config(name)
}

/// pull local changes from a config into the repository
fn config_pull(matches: &ArgMatches, mut structure: Structure) -> err::Result<()> {
    let name = matches.value_of("NAME").expect("name not provided");

    let config = structure.configs.remove(&String::from(name));
    match config {
        Some(config) => {
            let (deploy_target, pull_from) = match env::consts::OS {
                "windows" => (config.deploy.windows, config.pull.windows),
                "linux" => (config.deploy.linux, config.pull.windows),
                value => {
                    return Err(Error::from_string(format!(
                        "Operating system '{value}' is not supported."
                    )))
                }
            };
            let from = PathDir::new(
                shellexpand::tilde(&pull_from.from.unwrap_or(deploy_target.target)).into_owned(),
            )?;
            let config_dir = PathDir::new(name)?;
            let dotconfig = config_dir.concat(config::CONFIG_PATH)?;

            // resolve exclude glob patterns
            let mut exclude_patterns = GlobSetBuilder::new();
            config.pull.exclude.iter().for_each(|pattern| {
                exclude_patterns.add(Glob::new(pattern.as_str()).unwrap());
            });
            pull_from.exclude.iter().for_each(|pattern| {
                exclude_patterns.add(Glob::new(pattern.as_str()).unwrap());
            });
            let exclude_patterns = exclude_patterns.build().unwrap();

            for path in get_paths_in(&from, "**/*")? {
                let relative = path
                    .strip_prefix(&from)
                    .map_err(|_| Error::new("could not resolve relative path"))?;
                let to = config_dir.concat(&relative)?;

                if !exclude_patterns.is_match(&relative) {
                    // ensure that we aren't accidentally overwriting the dotconfig
                    if to == dotconfig {
                        return Err(Error::new("Trying to overwrite dotconfig.toml configuration file. Please add 'dotconfig.toml' to your excludes in the pull configuration."));
                    }

                    if to.exists() {
                        let mut from = FileRead::open(&path)?;
                        let mut to = FileRead::open(&to)?;

                        let from_contents = from.read_string();
                        let to_contents = to.read_string();

                        if let (Ok(from_contents), Ok(to_contents)) = (from_contents, to_contents) {
                            let diff = TextDiff::from_lines(&to_contents, &from_contents);

                            // compute the width of the line numbers
                            let ln_width = f32::ceil(f32::log10(usize::max(
                                from_contents.lines().count(),
                                to_contents.lines().count(),
                            )
                                as f32)) as usize;
                            let width_left = ln_width * 2 + 3;
                            let total_width = 80 - width_left - 1;

                            // print the file name
                            println!(
                                "{char:\u{2550}^ln_width$}\u{2564}{char:\u{2550}^total_width$}",
                                char = "\u{2550}",
                                ln_width = width_left,
                                total_width = total_width
                            );
                            println!(
                                "{: ^width_left$}\u{2502} {}",
                                " ",
                                relative.display(),
                                width_left = width_left
                            );
                            println!(
                                "{char:\u{2500}^ln_width$}\u{253C}{char:\u{2500}^total_width$}",
                                char = "\u{2500}",
                                ln_width = width_left,
                                total_width = total_width
                            );

                            // adapted from https://github.com/mitsuhiko/similar/blob/main/examples/terminal-inline.rs
                            for (idx, group) in diff.grouped_ops(2).iter().enumerate() {
                                // print separating line between changes
                                if idx > 0 {
                                    println!(
                                        "{char:\u{2500}^ln_width$}\u{253C}{char:\u{2500}^total_width$}",
                                        char="\u{2500}",
                                        ln_width = width_left,
                                        total_width = total_width
                                    );
                                }

                                // iterate over changes
                                for op in group {
                                    for change in diff.iter_inline_changes(&op) {
                                        let (bright_style, style, sign) = match change.tag() {
                                            ChangeTag::Delete => ("\x1b[91m", "\x1b[31m", '-'),
                                            ChangeTag::Insert => ("\x1b[92m", "\x1b[32m", '+'),
                                            ChangeTag::Equal => ("\x1b[2m", "\x1b[2m", ' '),
                                        };

                                        // print line numbers
                                        print!(
                                            "\x1b[2m{:ln_width$} {:ln_width$} \x1b[0m{style}{}\x1b[0m\u{2502}{style} ",
                                            change
                                                .old_index()
                                                .map_or(String::new(), |idx| idx.to_string()),
                                            change
                                                .new_index()
                                                .map_or(String::new(), |idx| idx.to_string()),
                                                sign,
                                            style=style,
                                            ln_width = ln_width
                                        );

                                        // print actual changes
                                        for (emphasized, value) in change.iter_strings_lossy() {
                                            if emphasized {
                                                print!("\x1b[0;3m{}{}", bright_style, &value);
                                            } else {
                                                print!("\x1b[0m{}{}", style, &value);
                                            }
                                        }

                                        // reset the style
                                        print!("\x1b[0m");

                                        // print a final newline if missing
                                        if change.missing_newline() {
                                            println!();
                                        }
                                    }
                                }
                            }
                        } else {
                            // print modification if file could not be read
                            let width_left = 4;
                            let total_width = 80 - width_left - 1;
                            println!(
                                "{char:\u{2550}^ln_width$}\u{2564}{char:\u{2550}^total_width$}",
                                char = "\u{2550}",
                                ln_width = width_left,
                                total_width = total_width
                            );
                            println!(
                                "{: ^width_left$}\x1b[36m~\x1b[0m \u{2502} {}",
                                " ",
                                relative.display(),
                                width_left = 2
                            );
                        }
                    } else if from.exists() && !to.exists() {
                        // print addition
                        let width_left = 4;
                        let total_width = 80 - width_left - 1;
                        println!(
                            "{char:\u{2550}^ln_width$}\u{2564}{char:\u{2550}^total_width$}",
                            char = "\u{2550}",
                            ln_width = width_left,
                            total_width = total_width
                        );
                        println!(
                            "{: ^width_left$}\x1b[32m+\x1b[0m \u{2502} {}",
                            " ",
                            relative.display(),
                            width_left = 2
                        );
                    }

                    // copy the file
                    if prompt_bool("Do you want to continue? ", true) {
                        PathDir::create_all(&to.parent()?)?;
                        path.copy(to)?;
                    }
                }
            }

            Ok(())
        }
        None => Err(Error::from_string(format!(
            "Config '{name}' does not exist."
        ))),
    }
}

/// deploy one or all configs to the local system
fn deploy(matches: &ArgMatches, structure: Option<Structure>) -> err::Result<()> {
    check_root_present()?;
    let mut structure = verify_structure(structure)?;

    let name = matches.value_of("name");

    if let Some(name) = name {
        let config = structure.configs.remove(name);
        match config {
            Some(config) => deploy_to(&String::from(name), config),
            None => Err(Error::from_string(format!(
                "Config '{name}' does not exist."
            ))),
        }
    } else {
        for (name, config) in structure.configs {
            match deploy_to(&name, config) {
                Ok(_) => {}
                Err(error) => println!("Could not deploy config '{}': {}", name, error),
            }
        }
        Ok(())
    }
}

fn deploy_to(name: &String, config: Configuration) -> err::Result<()> {
    let target = match env::consts::OS {
        "windows" => config.deploy.windows,
        "linux" => config.deploy.linux,
        value => {
            return Err(Error::from_string(format!(
                "Operating system '{value}' is not supported."
            )))
        }
    };

    let target_path = PathAbs::new(&shellexpand::tilde(&target.target).into_owned())?;

    // checks if the target directory already has files in it
    match &target.target_require_empty {
        Some(value) => {
            if *value {
                check_dir_null_or_empty(&target_path)?;
            }
        }
        None => {
            if config.deploy.target_require_empty {
                check_dir_null_or_empty(&target_path)?;
            }
        }
    }
    // create target
    PathDir::create_all(&target_path)?;

    // the source directoy
    let config_dir = PathDir::new(name)?;
    let dotconfig = PathFile::new(config_dir.concat(config::CONFIG_PATH)?)?;

    let mut exclude_patterns = GlobSetBuilder::new();
    config.deploy.exclude.iter().for_each(|pattern| {
        exclude_patterns.add(Glob::new(pattern.as_str()).unwrap());
    });
    target.exclude.iter().for_each(|pattern| {
        exclude_patterns.add(Glob::new(pattern.as_str()).unwrap());
    });
    let exclude_patterns = exclude_patterns.build().unwrap();

    // copy files to target
    for from in get_paths_in(&config_dir, "**/*")? {
        let to = target_path.concat(
            from.strip_prefix(&config_dir)
                .map_err(|_| Error::new("could not resolve relative path"))?,
        )?;

        if !(exclude_patterns.is_match(&from) || dotconfig == from) {
            PathDir::create_all(&to.parent()?)?;
            from.copy(to)?;
        }
    }

    Ok(())
}

fn get_paths_in(dir: &PathDir, pattern: &str) -> err::Result<Vec<PathFile>> {
    let glob = Glob::new(dir.concat(pattern)?.to_str().unwrap())
        .unwrap()
        .compile_matcher();

    return list_dir(&glob, dir);

    fn list_dir(glob: &GlobMatcher, dir: &PathDir) -> err::Result<Vec<PathFile>> {
        let mut paths = Vec::new();

        for value in dir.list()? {
            match value {
                Ok(value) => match value {
                    PathType::File(file) if glob.is_match(&file) => paths.push(file),
                    PathType::Dir(dir) => paths.append(&mut list_dir(glob, &dir)?),
                    _ => {}
                },
                Err(error) => return Err(error.into()),
            }
        }

        Ok(paths)
    }
}
