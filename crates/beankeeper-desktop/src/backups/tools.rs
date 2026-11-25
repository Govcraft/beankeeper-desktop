use std::{process::ExitCode, str::FromStr};

use beankeeper_cli::{
    capability::Capability,
    cli::Cli,
    db::{
        Actor, Db,
        audit::restore::{BackupPath, RestoreReceipt, RestoreRequest},
        ids::ReceiptId,
    },
    error::CliError,
    identity::{self, VerifiedAgent},
    mcp::{HostTool, HostToolEffect},
    output::json::EnvelopeError,
    passphrase,
    request::{LedgerRequest, LedgerResponse},
};
use clap::{Args, Subcommand};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool};
use schemars::JsonSchema;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::{
    onboarding::tool_error,
    output::{Envelope, Metadata},
    service, setup,
};

use super::{
    LocalBackups,
    books::{BooksState, CurrentBooks},
    catalog::{Backup, BackupId, Reason, Retention},
};

/// Local backup commands shared by the CLI and Desktop MCP.
#[derive(Args, Debug)]
pub struct BackupOptions {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(Subcommand, Debug)]
enum Operation {
    /// List automatic and on-demand local backups for these books.
    List,
    /// Take a verified local backup now.
    Create,
    /// Restore the books and their attachments from a local backup.
    Restore(RestoreOptions),
    /// Put back the books from before a restore, using its undo receipt.
    Undo(UndoOptions),
}

#[derive(Args, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RestoreOptions {
    /// ID returned by backup list.
    #[schemars(with = "String")]
    backup_id: BackupId,
}

#[derive(Args, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UndoOptions {
    /// Undo ID returned by restore.
    #[schemars(with = "i64")]
    undo_id: ReceiptId,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Clone, Copy)]
pub(crate) enum BackupTool {
    List,
    Create,
    Restore,
    Undo,
}

impl BackupTool {
    pub(crate) const ALL: [Self; 4] = [Self::List, Self::Create, Self::Restore, Self::Undo];

    fn name(self) -> &'static str {
        match self {
            Self::List => "backup_list",
            Self::Create => "backup_create",
            Self::Restore => "backup_restore",
            Self::Undo => "backup_undo",
        }
    }

    fn capability(self) -> Capability {
        match self {
            Self::List => Capability::Read,
            _ => Capability::Admin,
        }
    }

    pub(crate) fn definition(self) -> HostTool {
        let (description, schema, effect) = match self {
            Self::List => (
                "List the local backups for these books, newest first, with dates, sizes and retention.",
                schemars::schema_for!(NoArguments),
                HostToolEffect::ReadOnly,
            ),
            Self::Create => (
                "Take a verified local backup of the books and attachments now.",
                schemars::schema_for!(NoArguments),
                HostToolEffect::Mutation,
            ),
            Self::Restore => (
                "Restore the books and attachments from a backup_id returned by backup_list. Tell the owner what changed and offer to undo it with the returned undo_id.",
                schemars::schema_for!(RestoreOptions),
                HostToolEffect::Mutation,
            ),
            Self::Undo => (
                "Put the books back as they were before the restore, using its undo_id. Returns a new audited receipt.",
                schemars::schema_for!(UndoOptions),
                HostToolEffect::Mutation,
            ),
        };
        HostTool {
            tool: Tool::new(
                self.name(),
                description,
                schema.to_value().as_object().cloned().unwrap_or_default(),
            ),
            capability: self.capability(),
            effect,
        }
    }

    pub(crate) async fn call(
        self,
        request: CallToolRequestParams,
        books: LocalBackups,
        identity: VerifiedAgent,
    ) -> CallToolResult {
        match tokio::task::spawn_blocking(move || {
            let operation = self.decode(request)?;
            let actor = Actor::resolve(Some(identity.uri())).with_identity(identity);
            operation.execute(&books, |_| Ok(actor))?.into_tool_result()
        })
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => tool_error(&error),
            Err(_) => tool_error(&CliError::General("local backup worker failed".into())),
        }
    }

    fn decode(self, request: CallToolRequestParams) -> Result<Operation, CliError> {
        let value = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let operation = match self {
            Self::List => serde_json::from_value::<NoArguments>(value).map(|_| Operation::List),
            Self::Create => serde_json::from_value::<NoArguments>(value).map(|_| Operation::Create),
            Self::Restore => serde_json::from_value(value).map(Operation::Restore),
            Self::Undo => serde_json::from_value(value).map(Operation::Undo),
        };
        operation
            .map_err(|error| CliError::Validation(format!("invalid backup arguments: {error}")))
    }
}

impl Serialize for BackupTool {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.name())
    }
}

impl FromStr for BackupTool {
    type Err = CliError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| CliError::NotFound("unknown local backup tool".into()))
    }
}

impl From<&Operation> for BackupTool {
    fn from(operation: &Operation) -> Self {
        match operation {
            Operation::List => Self::List,
            Operation::Create => Self::Create,
            Operation::Restore(_) => Self::Restore,
            Operation::Undo(_) => Self::Undo,
        }
    }
}

impl Operation {
    fn execute(
        self,
        books: &LocalBackups,
        resolve_actor: impl FnOnce(&Db) -> Result<Actor, CliError>,
    ) -> Result<Reply, CliError> {
        let location = books.location()?;
        let catalog = location.lock()?;
        let current = BooksState::try_from((books, &catalog))?.current()?;
        let actor = resolve_actor(current.snapshot())?;
        match self {
            Self::List => Ok(Reply::Listed {
                backups: catalog.list()?,
                retention: Retention::default(),
            }),
            Self::Create => {
                let (backup, _) = catalog.capture(books, Reason::Requested)?;
                Ok(Reply::Created { backup })
            }
            Self::Restore(options) => {
                Restore::Backup(options.backup_id).execute(current, RestoreAgent::try_from(&actor)?)
            }
            Self::Undo(options) => {
                Restore::Undo(options.undo_id).execute(current, RestoreAgent::try_from(&actor)?)
            }
        }
    }
}

// Only a signed administrator may publish attachment files for a restore.
// Core checks the same grant against the live ledger inside its transaction.
#[derive(Clone, Copy)]
struct RestoreAgent<'a>(&'a Actor);

impl<'a> TryFrom<&'a Actor> for RestoreAgent<'a> {
    type Error = CliError;

    fn try_from(actor: &'a Actor) -> Result<Self, Self::Error> {
        let identity = actor.identity().ok_or_else(|| {
            CliError::Unauthorized("restore requires a signed ledger/admin credential".into())
        })?;
        if !identity.allows(Capability::Admin)? {
            return Err(CliError::Forbidden(
                "restore requires capability ledger/admin".into(),
            ));
        }
        Ok(Self(actor))
    }
}

enum Restore {
    Backup(BackupId),
    Undo(ReceiptId),
}

impl Restore {
    fn execute(
        self,
        current: CurrentBooks<'_>,
        agent: RestoreAgent<'_>,
    ) -> Result<Reply, CliError> {
        let request = match self {
            Self::Backup(id) => {
                RestoreRequest::Backup(BackupPath::try_from(current.restore_source(&id)?)?)
            }
            Self::Undo(id) => RestoreRequest::Undo(id),
        };
        let db = current.open_existing()?;
        match LedgerRequest::Restore(request).execute(&db, agent.0)? {
            LedgerResponse::Restore(_, receipt) => Ok(Reply::Restored { receipt }),
            _ => Err(CliError::General(
                "core returned an unexpected restore result".into(),
            )),
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum Reply {
    Listed {
        backups: Vec<Backup>,
        retention: Retention,
    },
    Created {
        backup: Backup,
    },
    Restored {
        #[serde(flatten)]
        receipt: RestoreReceipt,
    },
}

impl Reply {
    fn message(&self) -> String {
        match self {
            Self::Listed { backups, .. } if backups.is_empty() => {
                "There are no local backups for these books yet.".into()
            }
            Self::Listed { backups, .. } => backups
                .iter()
                .map(Backup::plain_text)
                .collect::<Vec<_>>()
                .join("\n"),
            Self::Created { backup } => format!("Saved a local backup. {}", backup.plain_text()),
            Self::Restored { receipt } => format!(
                "{} To put the books back, use undo ID {}.",
                receipt.summary(),
                receipt.restore_id()
            ),
        }
    }

    fn into_tool_result(self) -> Result<CallToolResult, CliError> {
        let message = self.message();
        let value = serde_json::to_value(self).map_err(|error| encoding(&error))?;
        let mut result = CallToolResult::structured(value);
        result.content = vec![ContentBlock::text(message)];
        Ok(result)
    }
}

fn encoding(error: &serde_json::Error) -> CliError {
    CliError::General(format!("cannot encode local backup result: {error}"))
}

/// Execute one local backup command and render the same result as Desktop MCP.
pub fn run(options: BackupOptions, mut cli: Cli, managed_or_default: bool) -> ExitCode {
    let mut meta = Metadata::new(BackupTool::from(&options.operation));
    let result = local_books(&mut cli, managed_or_default).and_then(|books| {
        let capability = BackupTool::from(&options.operation).capability();
        let reply = options.operation.execute(&books, |db| {
            let identity = db.read_snapshot(|reader| {
                identity::authorize(
                    reader,
                    cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
                    capability,
                )
            })?;
            let mut actor = Actor::resolve(cli.actor.as_deref());
            if let Some(identity) = identity {
                meta.agent = Some(identity.uri().to_owned());
                actor = actor.with_identity(identity);
            }
            Ok(actor)
        })?;
        if cli.is_json() {
            let envelope = Envelope {
                ok: true,
                meta: meta.clone(),
                data: Some(reply),
                error: None::<EnvelopeError>,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&envelope).map_err(|error| encoding(&error))?
            );
        } else {
            println!("{}", reply.message());
        }
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if cli.is_json()
                && let Ok(json) = serde_json::to_string_pretty(&Envelope::failure(meta, &error))
            {
                eprintln!("{json}");
            } else {
                error.report(false, None);
            }
            ExitCode::from(error.exit_code())
        }
    }
}

fn local_books(cli: &mut Cli, managed_or_default: bool) -> Result<LocalBackups, CliError> {
    if managed_or_default
        && let Some(books) =
            service::managed_books().map_err(|error| CliError::General(error.to_string()))?
    {
        cli.db = books;
    }
    cli.db = cli.db_path()?.to_owned();
    let _guard =
        setup::stored_passphrase(cli).map_err(|error| CliError::General(error.to_string()))?;
    let key = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    Ok(LocalBackups::new(cli.db.as_path().try_into()?, key))
}
