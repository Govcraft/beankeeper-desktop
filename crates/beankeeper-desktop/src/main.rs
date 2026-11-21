use std::process::ExitCode;

use beankeeper_cli::{
    cli::{Cli, Command},
    commands,
    error::CliError,
};
use beankeeper_desktop::{
    DesktopError, agents, bridge, browser,
    cli::{self, OpenOptions, ServeOptions},
    links, onboarding, product, server, service, setup, updates,
};
use clap::{FromArgMatches, parser::ValueSource};

fn main() -> ExitCode {
    let matches = cli::command().get_matches();
    let result = match matches.subcommand() {
        Some(("ui", ui)) => {
            let options = links::LinkOptions::from_arg_matches(
                ui.subcommand_matches("link")
                    .unwrap_or_else(|| unreachable!()),
            )
            .unwrap_or_else(|error| error.exit());
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            let json = ledger.is_json();
            links::run(options, ledger.company, json).map_err(DesktopError::Link)
        }
        Some(("agents", options)) => {
            let options = agents::AgentOptions::from_arg_matches(options)
                .unwrap_or_else(|error| error.exit());
            let json = matches.get_flag("json")
                || matches!(
                    matches.get_one::<beankeeper_cli::cli::OutputFormat>("format"),
                    Some(beankeeper_cli::cli::OutputFormat::Json)
                );
            return agents::run(options, json, matches.get_flag("quiet"));
        }
        Some(("service", options)) => {
            let options = service::ServiceOptions::from_arg_matches(options)
                .unwrap_or_else(|error| error.exit());
            let ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            service::run(
                &options,
                ledger,
                matches.value_source("db") == Some(ValueSource::CommandLine),
            )
            .map_err(|error| DesktopError::Service(Box::new(error)))
        }
        Some(("license", options)) => {
            let options =
                product::LicenseOptions::from_arg_matches(options).unwrap_or_else(|e| e.exit());
            product::run_license(options).map_err(DesktopError::Product)
        }
        Some(("update-check", _)) => check_updates(),
        Some(("self-update", _)) => install_update(),
        Some(("serve", options)) => {
            let options =
                ServeOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            server::validate_bind(options.bind, options.allow_remote).and_then(|bind| {
                let _guard = setup::stored_passphrase(&mut ledger)
                    .map_err(|error| DesktopError::Setup(Box::new(error)))?;
                server::serve(&ledger, bind, options.read_only)
            })
        }
        Some(("open", options)) => {
            let options =
                OpenOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            browser::open(options.bind)
        }
        Some(("setup", options)) if options.subcommand_name() == Some("status") => {
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            ledger.output = beankeeper_cli::cli::FormatOptions::from_arg_matches(&matches)
                .unwrap_or_else(|error| error.exit());
            return onboarding::run_status(ledger);
        }
        Some(("setup", options)) => {
            let options =
                setup::SetupOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            setup::run(&mut ledger, &options).map_err(|error| DesktopError::Setup(Box::new(error)))
        }
        Some(("mcp", mcp)) if mcp.subcommand_name() == Some("extension") => {
            bridge::run_extension().map_err(DesktopError::Bridge)
        }
        Some(("mcp", mcp)) if mcp.subcommand_name() == Some("bridge") => {
            let options = bridge::BridgeOptions::from_arg_matches(
                mcp.subcommand_matches("bridge")
                    .unwrap_or_else(|| unreachable!()),
            )
            .unwrap_or_else(|error| error.exit());
            bridge::run(&options).map_err(DesktopError::Bridge)
        }
        _ => return upstream(&matches),
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
    let meta =
        json_mode.then(|| cli.meta(commands::command_name(&cli.command), cli.company.as_deref()));
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
            let meta = meta.map(|meta| cli.meta(&meta.command, meta.company.as_deref()));
            error.report(json_mode, meta);
            ExitCode::from(error.exit_code())
        }
    }
}
