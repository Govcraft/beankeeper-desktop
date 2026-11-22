use std::process::ExitCode;

use beankeeper_cli::{
    cli::{Cli, Command},
    commands,
    db::companies::CompanySlug,
    error::CliError,
};
use beankeeper_desktop::{
    DesktopError, agents, bridge, browser,
    cli::{self, OpenOptions, ServeOptions},
    links, onboarding, product, server, service, setup, updates,
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
    Setup(setup::SetupOptions),
    Bridge(bridge::BridgeOptions),
    Extension,
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
            Some(("agents", options)) => {
                agents::AgentOptions::from_arg_matches(options).map(Self::Agents)
            }
            Some(("service", options)) => {
                service::ServiceOptions::from_arg_matches(options).map(Self::Service)
            }
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
        DesktopCommand::Link(options) => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            let json = ledger.is_json();
            links::run(*options, ledger.company.as_ref(), json).map_err(DesktopError::Link)
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
            let ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
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
        DesktopCommand::Open(options) => browser::open(options.bind),
        DesktopCommand::SetupStatus => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            return onboarding::run_status(ledger);
        }
        DesktopCommand::Setup(options) => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            setup::run(&mut ledger, &options).map_err(|error| DesktopError::Setup(Box::new(error)))
        }
        DesktopCommand::Bridge(options) => bridge::run(&options).map_err(DesktopError::Bridge),
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
            updates::self_update(&dir.join("updates.json"), &std::env::current_exe()?)?;
            println!("Desktop updated. Restart bk serve to use the installed version.");
            Ok(())
        })
        .map_err(DesktopError::Product)
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
    // Preserve upstream's explicit restore-destination safety check.
    let result = if matches!(cli.command, Command::Restore { .. })
        && matches.value_source("db") != Some(ValueSource::CommandLine)
    {
        Err(CliError::Usage(
            "restore requires an explicit --db destination".into(),
        ))
    } else {
        commands::dispatch(&cli, meta.clone())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let meta = meta.map(|meta| cli.meta(meta.command, meta.company.as_deref()));
            error.report(json_mode, meta);
            ExitCode::from(error.exit_code())
        }
    }
}
