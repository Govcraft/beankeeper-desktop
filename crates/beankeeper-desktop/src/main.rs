use std::process::ExitCode;

use beankeeper_cli::{
    cli::{Cli, Command},
    commands,
    error::CliError,
};
use beankeeper_desktop::{
    DesktopError, bridge, browser,
    cli::{self, OpenOptions, ServeOptions},
    links, product, server, setup, updates,
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
        Some(("license", options)) => {
            let options =
                product::LicenseOptions::from_arg_matches(options).unwrap_or_else(|e| e.exit());
            product::run_license(options).map_err(DesktopError::Product)
        }
        Some(("update-check", _)) => product::state_dir()
            .and_then(|dir| {
                let status = updates::check(
                    &dir.join("updates.json"),
                    product::updates_disabled(),
                    updates::now(),
                )?;
                println!("{status}");
                Ok(())
            })
            .map_err(DesktopError::Product),
        Some(("self-update", _)) => product::state_dir()
            .and_then(|dir| {
                if product::updates_disabled() {
                    return Err(product::ProductError::Configuration(
                        "update checks are disabled; unset BEANKEEPER_NO_UPDATE_CHECK first",
                    ));
                }
                updates::check(&dir.join("updates.json"), false, updates::now())?;
                updates::self_update(&dir.join("updates.json"), &std::env::current_exe()?)?;
                println!("Desktop updated. Restart bk serve to use the installed version.");
                Ok(())
            })
            .map_err(DesktopError::Product),
        Some(("serve", options)) => {
            let options =
                ServeOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            setup::stored_passphrase(&mut ledger)
                .map_err(|error| DesktopError::Setup(Box::new(error)))
                .and_then(|_guard| server::serve(&ledger, options))
        }
        Some(("open", options)) => {
            let options =
                OpenOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            browser::open(options.bind)
        }
        Some(("setup", options)) => {
            let options =
                setup::SetupOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            let mut ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            setup::run(&mut ledger, &options).map_err(|error| DesktopError::Setup(Box::new(error)))
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
