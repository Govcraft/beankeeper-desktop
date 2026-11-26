use std::{io::IsTerminal, path::PathBuf, process::ExitCode};

use beankeeper_cli::{
    cli::{Cli, Command, McpCommand, PassphraseOptions},
    commands::{self, setup::SetupCommand},
    db::companies::CompanySlug,
    error::CliError,
    ledger_target::LedgerPath,
    output::json::Meta,
    passphrase,
};
use clap::{ArgMatches, FromArgMatches};
use secrecy::SecretString;
use tempfile::NamedTempFile;

use crate::setup;

use super::LocalBackups;

/// Dispatch a core CLI command after preparing any books its writer can open.
pub fn run_upstream(matches: &ArgMatches) -> ExitCode {
    let mut cli = Cli::from_arg_matches(matches).unwrap_or_else(|error| error.exit());
    let json_mode = cli.is_json();
    let meta = json_mode.then(|| {
        cli.meta(
            commands::operation(&cli.command),
            cli.company.as_ref().map(CompanySlug::as_str),
        )
    });
    match dispatch(&mut cli, meta.clone()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let meta = meta.map(|meta| cli.meta(meta.command, meta.company.as_deref()));
            error.report(json_mode, meta);
            ExitCode::from(error.exit_code())
        }
    }
}

fn dispatch(cli: &mut Cli, meta: Option<Meta>) -> Result<(), CliError> {
    let ledgers = DispatchBooks::try_from(&*cli)?;
    let _stored = setup::stored_passphrase_for(&mut cli.passphrase, &ledgers.primary)
        .map_err(|error| CliError::General(error.to_string()))?;
    let _replay = ledgers.prepare(cli)?;
    commands::dispatch(cli, meta)
}

struct DispatchBooks {
    primary: PathBuf,
    open: DispatchOpen,
}

enum DispatchOpen {
    CoreOnly,
    Existing,
    Http(Vec<LedgerPath>),
    Replace { encrypt: bool },
}

impl TryFrom<&Cli> for DispatchBooks {
    type Error = CliError;

    fn try_from(cli: &Cli) -> Result<Self, CliError> {
        let primary = match &cli.command {
            Command::Init {
                path: Some(path), ..
            } => path.clone(),
            _ => cli.db_path()?.to_owned(),
        };
        let open = match &cli.command {
            Command::Init {
                encrypt,
                force: true,
                ..
            } if cli.agent_token.is_none() => DispatchOpen::Replace { encrypt: *encrypt },
            Command::Init { .. }
            | Command::Backup { .. }
            | Command::Recover { .. }
            | Command::Restore { .. }
            | Command::RestoreUndo { .. }
            | Command::Setup(
                SetupCommand::Status
                | SetupCommand::BookShow { .. }
                | SetupCommand::OpeningPreview(_),
            ) => DispatchOpen::CoreOnly,
            Command::Mcp(McpCommand::Serve { http: false, .. })
                if cli.passphrase.passphrase_fd == Some(0) =>
            {
                // Let core reject a passphrase on MCP's transport descriptor.
                DispatchOpen::CoreOnly
            }
            Command::Mcp(McpCommand::Serve {
                http: true,
                bind,
                allow_remote,
                ..
            }) => {
                if !allow_remote && bind.is_some_and(|address| !address.ip().is_loopback()) {
                    DispatchOpen::CoreOnly
                } else {
                    let named = cli
                        .ledger_bindings()?
                        .map(|(_, path)| path.clone())
                        .collect();
                    DispatchOpen::Http(named)
                }
            }
            _ => DispatchOpen::Existing,
        };
        // HTTP opens the startup books as well as every named binding.
        let primary = if matches!(open, DispatchOpen::Http(_)) {
            cli.db.clone()
        } else {
            primary
        };
        Ok(Self { primary, open })
    }
}

impl DispatchBooks {
    fn prepare(self, cli: &mut Cli) -> Result<Option<NamedTempFile>, CliError> {
        // Core owns missing-ledger behavior, including creating or recovering
        // new books and standalone loan calculations.
        if matches!(self.open, DispatchOpen::CoreOnly) || !self.primary.try_exists()? {
            return Ok(None);
        }
        let key = DispatchKey::try_from((
            &cli.passphrase,
            matches!(self.open, DispatchOpen::Replace { encrypt: true }),
        ))?;
        // Core resolves its own credential. Replay the same resolved secret
        // through its file interface, including one-shot FDs and commands.
        let replay = key
            .0
            .as_ref()
            .map(setup::temporary_passphrase)
            .transpose()?;
        let books = LocalBackups::new(self.primary.as_path().try_into()?, key.0.clone());
        books.prepare(matches!(self.open, DispatchOpen::Replace { .. }))?;
        if let DispatchOpen::Http(named) = self.open {
            for path in named {
                let path = path.as_ref();
                if path.try_exists()? {
                    LocalBackups::new(path.try_into()?, key.0.clone()).prepare(false)?;
                }
            }
        }
        if let Some(file) = &replay {
            cli.passphrase.passphrase_fd = None;
            cli.passphrase.passphrase_file = Some(file.path().into());
        }
        Ok(replay)
    }
}

struct DispatchKey(Option<SecretString>);

impl TryFrom<(&PassphraseOptions, bool)> for DispatchKey {
    type Error = CliError;

    fn try_from((options, prompt): (&PassphraseOptions, bool)) -> Result<Self, CliError> {
        let resolved = passphrase::resolve_passphrase(
            options.passphrase_fd,
            options.passphrase_file.as_deref(),
            false,
        )?;
        if resolved.is_some() || !prompt {
            return Ok(Self(resolved));
        }
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Usage(
                "cannot prompt for passphrase: stdin is not a terminal; \
                 use --passphrase-file or --passphrase-fd instead"
                    .into(),
            ));
        }
        passphrase::prompt_new_passphrase().map(|secret| Self(Some(secret)))
    }
}
