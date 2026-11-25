use std::process::ExitCode;

use beankeeper_cli::{cli::Cli, commands, db::companies::CompanySlug};
use beankeeper_desktop::{
    DesktopError, agents, backups, bridge, browser, changes,
    cli::{self, OpenOptions, ServeOptions},
    demo, imports, links, onboarding, product, server, service, setup, updates,
};
use clap::{FromArgMatches, parser::ValueSource};

enum DesktopCommand {
    Link(Box<links::LinkOptions>),
    Agents(agents::AgentOptions),
    Service(service::ServiceOptions),
    License(product::LicenseOptions),
    CheckForUpdate,
    InstallUpdate,
    Serve(ServeOptions),
    Open(OpenOptions),
    SetupStatus,
    ChangesSince,
    ChangesMarkSeen(changes::MarkSeen),
    Setup(setup::SetupOptions),
    Bridge(bridge::BridgeOptions),
    Extension,
    Demo(demo::DemoOptions),
    Backups(backups::BackupOptions),
    Import(Box<imports::command::ImportOptions>),
    Ledger,
}

impl DesktopCommand {
    fn from_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        match matches.subcommand() {
            Some(("ui", ui)) => match ui.subcommand() {
                Some(("link", options)) => links::LinkOptions::from_arg_matches(options)
                    .map(Box::new)
                    .map(Self::Link),
                _ => Ok(Self::Ledger),
            },
            Some(("audit", audit)) => match audit.subcommand() {
                Some(("since", _)) => Ok(Self::ChangesSince),
                Some(("mark-seen", options)) => {
                    changes::MarkSeen::from_arg_matches(options).map(Self::ChangesMarkSeen)
                }
                _ => Ok(Self::Ledger),
            },
            Some(("agents", options)) => {
                agents::AgentOptions::from_arg_matches(options).map(Self::Agents)
            }
            Some(("service", options)) => {
                service::ServiceOptions::from_arg_matches(options).map(Self::Service)
            }
            Some(("backups", options)) => {
                backups::BackupOptions::from_arg_matches(options).map(Self::Backups)
            }
            Some(("demo", options)) => demo::DemoOptions::from_arg_matches(options).map(Self::Demo),
            Some(("import", options)) => imports::command::ImportOptions::from_arg_matches(options)
                .map(Box::new)
                .map(Self::Import),
            Some(("license", options)) => {
                product::LicenseOptions::from_arg_matches(options).map(Self::License)
            }
            Some(("update-check", _)) => Ok(Self::CheckForUpdate),
            Some(("self-update", _)) => Ok(Self::InstallUpdate),
            Some(("serve", options)) => ServeOptions::from_arg_matches(options).map(Self::Serve),
            Some(("open", options)) => OpenOptions::from_arg_matches(options).map(Self::Open),
            Some(("setup", options)) if matches!(options.subcommand(), Some(("status", _))) => {
                Ok(Self::SetupStatus)
            }
            Some(("setup", options)) if options.subcommand_name().is_none() => {
                setup::SetupOptions::from_arg_matches(options).map(Self::Setup)
            }
            Some(("mcp", mcp)) => match mcp.subcommand() {
                Some(("extension", _)) => Ok(Self::Extension),
                Some(("bridge", options)) => {
                    bridge::BridgeOptions::from_arg_matches(options).map(Self::Bridge)
                }
                _ => Ok(Self::Ledger),
            },
            _ => Ok(Self::Ledger),
        }
    }
}

fn main() -> ExitCode {
    let matches = cli::command().get_matches();
    let command = DesktopCommand::from_matches(&matches).unwrap_or_else(|error| error.exit());
    let result = match command {
        DesktopCommand::Backups(options) => return run_backups(options, &matches),
        DesktopCommand::Import(options) => {
            let ledger = desktop_ledger_options(&matches);
            return imports::command::run(&options, &ledger, selected_books(&matches, &ledger));
        }
        DesktopCommand::Link(options) => {
            let ledger = desktop_ledger_options(&matches);
            links::run(*options, &ledger, selected_books(&matches, &ledger))
        }
        DesktopCommand::Agents(options) => {
            let json = matches.get_flag("json")
                || matches!(
                    matches.get_one::<beankeeper_cli::cli::OutputFormat>("format"),
                    Some(beankeeper_cli::cli::OutputFormat::Json)
                );
            return agents::run(options, json, matches.get_flag("quiet"));
        }
        DesktopCommand::Service(options) => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            service::run(
                &options,
                ledger,
                matches.value_source("db") == Some(ValueSource::CommandLine),
            )
            .map_err(|error| DesktopError::Service(Box::new(error)))
        }
        DesktopCommand::License(options) => {
            product::run_license(options).map_err(DesktopError::Product)
        }
        DesktopCommand::CheckForUpdate => check_updates(),
        DesktopCommand::InstallUpdate => install_update(),
        DesktopCommand::Serve(options) => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            server::validate_bind(options.bind, options.allow_remote).and_then(|bind| {
                let _guard = setup::stored_passphrase(&mut ledger)
                    .map_err(|error| DesktopError::Setup(Box::new(error)))?;
                server::serve(&ledger, bind, options.read_only)
            })
        }
        DesktopCommand::Open(options) => browser::open(options),
        DesktopCommand::ChangesSince | DesktopCommand::ChangesMarkSeen(_) => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            return match command {
                DesktopCommand::ChangesMarkSeen(options) => {
                    changes::run_mark_seen(ledger, &options)
                }
                _ => changes::run_since(ledger),
            };
        }
        DesktopCommand::SetupStatus => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            return onboarding::run_status(ledger);
        }
        DesktopCommand::Setup(options) => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            setup::SetupBooks::try_from((ledger.db.as_path(), matches.value_source("db"), &options))
                .map_err(|error| DesktopError::Service(Box::new(error)))
                .and_then(|books| {
                    ledger.db = books.into();
                    setup::run(&mut ledger, &options)
                        .map_err(|error| DesktopError::Setup(Box::new(error)))
                })
        }
        DesktopCommand::Bridge(options) => bridge::run(&options).map_err(DesktopError::Bridge),
        DesktopCommand::Demo(options) => {
            let json = matches.get_flag("json")
                || matches!(
                    matches.get_one::<beankeeper_cli::cli::OutputFormat>("format"),
                    Some(beankeeper_cli::cli::OutputFormat::Json)
                );
            return demo::run(&options, json);
        }
        DesktopCommand::Extension => bridge::run_extension().map_err(DesktopError::Bridge),
        DesktopCommand::Ledger => return upstream(&matches),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(DesktopError::Ledger(error)) => {
            error.report(false, None);
            ExitCode::from(error.exit_code())
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run_backups(options: backups::BackupOptions, matches: &clap::ArgMatches) -> ExitCode {
    let mut ledger = desktop_ledger_options(matches);
    // Preserve upstream's typed ledger bindings when dispatching a Desktop subcommand.
    let mut globals = matches.clone();
    globals.remove_subcommand();
    ledger
        .update_from_arg_matches(&globals)
        .unwrap_or_else(|error| error.exit());
    backups::run(
        options,
        ledger,
        matches.value_source("db") == Some(ValueSource::DefaultValue)
            && !matches.contains_id("ledger_target"),
    )
}

fn desktop_ledger_options(matches: &clap::ArgMatches) -> Cli {
    let mut ledger = cli::ledger_options(matches).unwrap_or_else(|error| error.exit());
    ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(matches)
        .unwrap_or_else(|error| error.exit());
    ledger
}

fn selected_books<'a>(matches: &clap::ArgMatches, ledger: &'a Cli) -> service::LinkBooks<'a> {
    if matches.value_source("db") == Some(ValueSource::DefaultValue) {
        service::LinkBooks::ManagedOrDefault(&ledger.db)
    } else {
        service::LinkBooks::Explicit(&ledger.db)
    }
}

fn check_updates() -> Result<(), DesktopError> {
    product::state_dir()
        .and_then(|dir| {
            let status = updates::check(
                &dir.join("updates.json"),
                product::update_checks(),
                updates::now(),
            )?;
            println!("{status}");
            Ok(())
        })
        .map_err(DesktopError::Product)
}

fn install_update() -> Result<(), DesktopError> {
    let executable = std::env::current_exe()?;
    product::state_dir()
        .and_then(|dir| {
            if product::update_checks() == updates::UpdateChecks::Disabled {
                return Err(product::ProductError::Configuration(
                    "update checks are disabled; unset BEANKEEPER_NO_UPDATE_CHECK first",
                ));
            }
            updates::check(
                &dir.join("updates.json"),
                updates::UpdateChecks::Enabled,
                updates::now(),
            )?;
            updates::self_update(&dir.join("updates.json"), &executable)?;
            Ok(())
        })
        .map_err(DesktopError::Product)?;
    if service::restart_updated(&executable)
        .map_err(|error| DesktopError::Service(Box::new(error)))?
    {
        println!("Desktop updated and the local service restarted.");
    } else {
        println!(
            "Desktop updated. Restart any running bk serve command to use the installed version."
        );
    }
    Ok(())
}

fn upstream(matches: &clap::ArgMatches) -> ExitCode {
    let mut cli = Cli::from_arg_matches(matches).unwrap_or_else(|error| error.exit());
    let _passphrase = match setup::stored_passphrase(&mut cli) {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let json_mode = cli.is_json();
    let meta = json_mode.then(|| {
        cli.meta(
            commands::operation(&cli.command),
            cli.company.as_ref().map(CompanySlug::as_str),
        )
    });
    let result = commands::dispatch(&cli, meta.clone());
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let meta = meta.map(|meta| cli.meta(meta.command, meta.company.as_deref()));
            error.report(json_mode, meta);
            ExitCode::from(error.exit_code())
        }
    }
}
