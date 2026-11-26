mod http;

#[cfg(test)]
pub(crate) use http::NoHostTools;

use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::ExitCode,
};

use beankeeper_cli::{
    cli::{AuditCommand, Cli, Command, McpCommand, PassphraseOptions},
    commands::{self, setup::SetupCommand},
    db::companies::CompanySlug,
    error::CliError,
    mcp::{self, HttpLedgerTargets, LedgerTarget, LedgerTargets},
    output::json::Meta,
    passphrase,
};
use clap::{ArgMatches, FromArgMatches};
use secrecy::SecretString;
use tempfile::NamedTempFile;

use crate::{entitlement::LedgerAccess, instance::BooksPath, setup};

use super::LocalBackups;
use http::HttpBind;

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
    let target = matches
        .get_one::<LedgerTarget>("ledger_target")
        .cloned()
        .unwrap_or_default();
    match dispatch(&mut cli, meta.clone(), target) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let meta = meta.map(|meta| cli.meta(meta.command, meta.company.as_deref()));
            error.report(json_mode, meta);
            ExitCode::from(error.exit_code())
        }
    }
}

fn dispatch(cli: &mut Cli, meta: Option<Meta>, target: LedgerTarget) -> Result<(), CliError> {
    let ledgers = DispatchBooks::try_from(&*cli)?;
    let _stored = setup::stored_passphrase_for(&mut cli.passphrase, &ledgers.primary)
        .map_err(|error| CliError::General(error.to_string()))?;
    ledgers.run(cli, meta, target)
}

struct DispatchBooks {
    primary: PathBuf,
    open: DispatchOpen,
}

enum DispatchOpen {
    CoreOnly,
    Existing,
    Stdio { read_only: bool },
    Http { bind: HttpBind, read_only: bool },
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
                read_only,
            }) => DispatchOpen::Http {
                bind: (*bind, *allow_remote).try_into()?,
                read_only: *read_only,
            },
            Command::Mcp(McpCommand::Serve { read_only, .. }) => DispatchOpen::Stdio {
                read_only: *read_only,
            },
            _ => DispatchOpen::Existing,
        };
        // HTTP opens the startup books as well as every named binding.
        let primary = if matches!(open, DispatchOpen::Http { .. }) {
            cli.db.clone()
        } else {
            primary
        };
        Ok(Self { primary, open })
    }
}

impl DispatchBooks {
    fn run(self, cli: &mut Cli, meta: Option<Meta>, target: LedgerTarget) -> Result<(), CliError> {
        match self.open {
            DispatchOpen::CoreOnly => commands::dispatch(cli, meta),
            // Core handles commands without books, such as loan schedules.
            DispatchOpen::Existing | DispatchOpen::Replace { .. }
                if !self.primary.try_exists()? =>
            {
                commands::dispatch(cli, meta)
            }
            DispatchOpen::Existing => {
                let books =
                    DispatchKey::try_from((&cli.passphrase, false))?.books(&self.primary)?;
                let db = if matches!(&cli.command, Command::Audit(args) if matches!(args.command, AuditCommand::Undo(_)))
                {
                    // Upstream exposes receipt undo as an ordinary write. This already
                    // parsed command can use the audited undo entry point exclusively.
                    books.open()?
                } else {
                    books.open_protected(target)?
                };
                commands::dispatch_with_db(cli, db, meta)
            }
            DispatchOpen::Stdio { read_only } => {
                let db = DispatchKey::try_from((&cli.passphrase, false))?
                    .books(&self.primary)?
                    .open_protected(target)?;
                mcp::serve_with_db(cli, db, read_only)
            }
            DispatchOpen::Http { bind, read_only } => {
                let targets = self.http_targets(cli)?;
                bind.serve(cli, targets, read_only)
            }
            DispatchOpen::Replace { encrypt } => {
                LedgerAccess::from(BooksPath::try_from(self.primary.as_path())?)
                    .ordinary_write(target)?;
                let key = DispatchKey::try_from((&cli.passphrase, encrypt))?;
                key.books(&self.primary)?.before_replacement()?;
                // Core init replaces the file and resolves the destination key itself.
                let _replay = key.replay(&mut cli.passphrase)?;
                commands::dispatch(cli, meta)
            }
        }
    }

    fn http_targets(&self, cli: &Cli) -> Result<HttpLedgerTargets, CliError> {
        let named = cli.ledger_bindings()?;
        let key = DispatchKey::try_from((&cli.passphrase, false))?;
        let startup = key
            .books(&self.primary)?
            .open_protected(LedgerTarget::default())?;
        if named.len() == 0 {
            return Ok(startup.into());
        }
        let targets = LedgerTargets::new(startup);
        for (name, path) in named {
            let target: LedgerTarget = name.clone().into();
            let db = key.books(path.as_ref())?.open_protected(target.clone())?;
            targets.register(target, db)?;
        }
        Ok(targets.into())
    }
}

struct DispatchKey(Option<SecretString>);

impl DispatchKey {
    fn books(&self, path: &Path) -> Result<LocalBackups, CliError> {
        let path = path.try_into().map_err(|error: io::Error| {
            if error.kind() == io::ErrorKind::NotFound {
                CliError::NotFound(format!(
                    "database '{}' not found; run `bk init --path '{}'` to create it",
                    path.display(),
                    path.display()
                ))
            } else {
                error.into()
            }
        })?;
        Ok(LocalBackups::new(path, self.0.clone()))
    }

    fn replay(&self, options: &mut PassphraseOptions) -> Result<Option<NamedTempFile>, CliError> {
        let replay = self
            .0
            .as_ref()
            .map(setup::temporary_passphrase)
            .transpose()?;
        if let Some(file) = &replay {
            options.passphrase_fd = None;
            options.passphrase_file = Some(file.path().into());
        }
        Ok(replay)
    }
}

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
