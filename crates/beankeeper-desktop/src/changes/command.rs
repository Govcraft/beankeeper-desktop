use std::{fmt, net::SocketAddr, process::ExitCode};

use beankeeper_cli::{
    capability::Capability,
    cli::Cli,
    db::{Db, audit::receipts::AuditCursor},
    error::CliError,
    identity,
    output::json::EnvelopeError,
    passphrase::resolve_passphrase,
};
use clap::Args;
use secrecy::ExposeSecret;
use serde::Serialize;

use crate::{
    instance::BooksPath,
    output::{Envelope, Metadata},
    service::{self, LinkBooks},
    setup::{self, SetupError},
};

use super::{Acknowledged, Report, acknowledge, book};

/// Options for acknowledging a delivered page of changes.
#[derive(Args, Debug)]
pub struct MarkSeen {
    /// The through cursor returned by the report that was delivered.
    #[arg(long)]
    through: AuditCursor,
}

#[derive(Clone, Copy, Serialize)]
enum Operation {
    #[serde(rename = "audit.since")]
    Since,
    #[serde(rename = "audit.mark_seen")]
    MarkSeen,
}

/// Print one page of changes for the selected book.
pub fn run_since(cli: Cli) -> ExitCode {
    run(cli, Operation::Since, |db, books, cli| {
        let company = book(db, cli.company.clone(), None)?;
        let report = Report::read(db, books, company, None)?;
        let bound = service::link_address(LinkBooks::Explicit(&cli.db), None)
            .ok()
            .map(Into::into);
        Ok(Output::Since(report, bound))
    })
}

/// Advance the selected book's local last-seen marker.
pub fn run_mark_seen(cli: Cli, input: &MarkSeen) -> ExitCode {
    run(cli, Operation::MarkSeen, |db, books, cli| {
        let company = book(db, cli.company.clone(), None)?;
        acknowledge(db, books, company, input.through).map(Output::Marked)
    })
}

enum Output {
    Since(Report, Option<SocketAddr>),
    Marked(Acknowledged),
}

fn run(
    mut cli: Cli,
    operation: Operation,
    execute: impl FnOnce(&Db, &BooksPath, &Cli) -> Result<Output, CliError>,
) -> ExitCode {
    let mut meta = Metadata::new(operation);
    let result = (|| {
        let _guard = setup::stored_passphrase(&mut cli).map_err(|error| match error {
            SetupError::Io(error) => CliError::Io(error),
            SetupError::Ledger(error) => error,
            error => CliError::General(error.to_string()),
        })?;
        let secret = resolve_passphrase(
            cli.passphrase.passphrase_fd,
            cli.passphrase.passphrase_file.as_deref(),
            false,
        )?;
        let books = BooksPath::try_from(cli.db.as_path())?;
        let db = Db::open_snapshot(books.as_ref(), secret.as_ref())?;
        let agent = identity::authorize(
            db.read(),
            cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
            Capability::Read,
        )?;
        meta.agent = agent.map(|agent| agent.uri().to_owned());
        let data = execute(&db, &books, &cli)?;
        match data {
            Output::Since(report, bound) => emit(&cli, &meta, report.data(bound))?,
            Output::Marked(acknowledged) => emit(&cli, &meta, acknowledged)?,
        }
        Ok::<_, CliError>(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.is_json() {
                let envelope = Envelope {
                    ok: false,
                    meta,
                    data: None::<()>,
                    error: Some(EnvelopeError {
                        code: error.code(),
                        message: error.to_string(),
                    }),
                };
                match serde_json::to_string_pretty(&envelope) {
                    Ok(result) => eprintln!("{result}"),
                    Err(_) => error.report(true, None),
                }
            } else {
                error.report(false, None);
            }
            ExitCode::from(error.exit_code())
        }
    }
}

fn emit(
    cli: &Cli,
    meta: &Metadata<Operation>,
    data: impl Serialize + fmt::Display,
) -> Result<(), CliError> {
    if cli.is_json() {
        let envelope = Envelope {
            ok: true,
            meta: meta.clone(),
            data: Some(data),
            error: None::<EnvelopeError>,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope)
                .map_err(|error| CliError::General(format!("cannot encode changes: {error}")))?
        );
    } else {
        println!("{data}");
    }
    Ok(())
}
