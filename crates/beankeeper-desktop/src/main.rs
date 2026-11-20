use std::process::ExitCode;

use beankeeper_cli::{
    cli::{Cli, Command},
    commands,
    error::CliError,
};
use beankeeper_desktop::{
    DesktopError, browser,
    cli::{self, OpenOptions, ServeOptions},
    server,
};
use clap::{FromArgMatches, parser::ValueSource};

fn main() -> ExitCode {
    let matches = cli::command().get_matches();
    let result = match matches.subcommand() {
        Some(("serve", options)) => {
            let options =
                ServeOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            let ledger = cli::ledger_options(&matches).unwrap_or_else(|error| error.exit());
            server::serve(&ledger, options)
        }
        Some(("open", options)) => {
            let options =
                OpenOptions::from_arg_matches(options).unwrap_or_else(|error| error.exit());
            browser::open(options.bind)
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
    let cli = Cli::from_arg_matches(matches).unwrap_or_else(|error| error.exit());
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
