//! Ephemeral, encrypted sample books hosted by the managed MCP endpoint.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::ExitCode,
    str::FromStr,
    sync::Arc,
};

use beankeeper_cli::{
    capability::Capability,
    commands::init::{create_demo_ledger, create_demo_ledger_from_profile},
    db::{Actor, Db, audit},
    demo::DemoProfile,
    identity::{Admin, Granted},
    mcp::{
        DetachedLedger, HostTool, HostToolEffect, LEDGER_TARGET_META_KEY, LedgerTargets,
        NamedLedgerTarget,
    },
};
use clap::{Args, Subcommand};
use mti::prelude::*;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, JsonObject, MetaObject, Tool,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::Builder;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Manage a disposable sample ledger on the running service.
#[derive(Args, Debug)]
pub struct DemoOptions {
    #[command(subcommand)]
    command: DemoCommand,
}

#[derive(Debug, Subcommand)]
enum DemoCommand {
    /// Prepare sample books using the running managed service.
    Start {
        /// Synthetic profile as a JSON object, or '-' to read JSON from stdin.
        #[arg(long, value_name = "JSON|-", verbatim_doc_comment)]
        profile: Option<String>,
    },
    /// Discard the sample books and their practice entries.
    Remove,
}

/// CLI transport input. The managed service validates the profile on receipt.
struct ProfileArguments(JsonObject);

impl TryFrom<&str> for ProfileArguments {
    type Error = DemoError;

    fn try_from(input: &str) -> Result<Self, Self::Error> {
        if input == "-" {
            serde_json::from_reader(io::stdin().lock())
        } else {
            serde_json::from_str(input)
        }
        .map(Self)
        .map_err(|_| DemoError::Arguments)
    }
}

impl From<ProfileArguments> for JsonObject {
    fn from(ProfileArguments(profile): ProfileArguments) -> Self {
        JsonObject::from_iter([("profile".into(), Value::Object(profile))])
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DemoOperation {
    Start,
    Remove,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartArguments {
    /// Optional interview choices for generating fictional books. Omit for the quick demo.
    profile: Option<DemoProfile>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

enum DemoRequest {
    Start(StartArguments),
    Remove,
}

impl TryFrom<(DemoOperation, Option<JsonObject>)> for DemoRequest {
    type Error = DemoError;

    fn try_from(
        (operation, arguments): (DemoOperation, Option<JsonObject>),
    ) -> Result<Self, Self::Error> {
        let value = Value::Object(arguments.unwrap_or_default());
        match operation {
            DemoOperation::Start => StartArguments::deserialize(value)
                .map(Self::Start)
                .map_err(|_| DemoError::Arguments),
            DemoOperation::Remove => NoArguments::deserialize(value)
                .map(|NoArguments {}| Self::Remove)
                .map_err(|_| DemoError::RemoveArguments),
        }
    }
}

impl FromStr for DemoOperation {
    type Err = ();

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.name() == name)
            .ok_or(())
    }
}

impl DemoOperation {
    pub(crate) const ALL: [Self; 2] = [Self::Start, Self::Remove];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Start => "demo_start",
            Self::Remove => "demo_remove",
        }
    }

    pub(crate) fn definition(self) -> HostTool {
        let (description, schema) = match self {
            Self::Start => (
                "Try Beankeeper with encrypted fictional books. Omit profile for a quick demo or to resume an active demo. To tailor a new demo, ask about work, book and account types, investments, country and currencies in chat, and pass only those categories. Tell the owner every figure is fictional. HTTP clients select the returned target on each ledger request.",
                schemars::schema_for!(StartArguments),
            ),
            Self::Remove => (
                "Discard the fictional books, including any practice entries. A managed connection returns to the owner's books after this succeeds.",
                schemars::schema_for!(NoArguments),
            ),
        };
        HostTool {
            tool: Tool::new(
                self.name(),
                description,
                schema.to_value().as_object().cloned().unwrap_or_default(),
            ),
            capability: Capability::Read,
            effect: HostToolEffect::Mutation,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum DemoReport {
    Active { target: NamedLedgerTarget },
    Removed { target: NamedLedgerTarget },
    Absent,
}

impl DemoReport {
    fn message(&self) -> &'static str {
        match self {
            Self::Active { .. } => {
                "The fictional books are ready. You can try reports and practice recording or reversing a transaction. Tell the owner that the books and figures are fictional."
            }
            Self::Removed { .. } => {
                "The fictional books and their practice entries were discarded. You can return to your own books."
            }
            Self::Absent => {
                "There are no fictional books to discard. You can return to your own books."
            }
        }
    }

    fn result(&self) -> CallToolResult {
        let mut result = CallToolResult::structured(json!(self));
        result.content = vec![ContentBlock::text(self.message())];
        if let Self::Active { target } | Self::Removed { target } = self {
            result.meta = Some(target_meta(target));
        }
        result
    }
}

pub(crate) fn target_meta(target: &NamedLedgerTarget) -> MetaObject {
    MetaObject(JsonObject::from_iter([(
        LEDGER_TARGET_META_KEY.into(),
        json!(target),
    )]))
}

#[derive(Debug)]
pub(crate) enum DemoError {
    Arguments,
    RemoveArguments,
    AlreadyActive,
    Create,
    Detach,
    Cleanup,
    Unavailable,
}

impl DemoError {
    fn code(&self) -> &'static str {
        match self {
            Self::Arguments | Self::RemoveArguments => "DEMO_ARGUMENTS",
            Self::AlreadyActive => "DEMO_ALREADY_ACTIVE",
            Self::Create => "DEMO_CREATE_FAILED",
            Self::Detach => "DEMO_DETACH_FAILED",
            Self::Cleanup => "DEMO_CLEANUP_FAILED",
            Self::Unavailable => "DEMO_UNAVAILABLE",
        }
    }

    pub(crate) fn result(&self) -> CallToolResult {
        let message = self.to_string();
        let mut result =
            CallToolResult::structured_error(json!({"code":self.code(), "message":message}));
        result.content = vec![ContentBlock::text(message)];
        result
    }
}

impl fmt::Display for DemoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Arguments => "Supply a JSON demo profile with supported interview choices, or omit it for the quick demo.",
            Self::RemoveArguments => "Demo removal takes no arguments.",
            Self::AlreadyActive => "Fictional books are already active. Resume them by starting without a profile, or discard them before starting a tailored demo.",
            Self::Create => "Beankeeper could not prepare the fictional books. Try starting the demo again.",
            Self::Detach => "Beankeeper could not safely detach the fictional books. The demo file was kept.",
            Self::Cleanup => "Beankeeper detached the fictional books but could not finish removing their files. Try removing the demo again.",
            Self::Unavailable => "The selected fictional books are unavailable. Start the demo again or remove it to return to your own books.",
        })
    }
}

#[derive(Clone)]
pub(crate) struct DemoBooks(Arc<Host>);

struct Host {
    targets: LedgerTargets,
    parent: PathBuf,
    state: Mutex<DemoState>,
    shutdown: CancellationToken,
}

#[derive(Default)]
enum DemoState {
    #[default]
    Absent,
    Hosted(HostedDemo),
    Detached(DetachedDemo),
    DetachFailed,
}

struct HostedDemo {
    target: NamedLedgerTarget,
    directory: PathBuf,
}

struct DetachedDemo {
    directory: PathBuf,
    proof: DetachedLedger,
}

impl DemoBooks {
    pub(crate) fn new(targets: LedgerTargets, books: &Path, shutdown: CancellationToken) -> Self {
        Self(Arc::new(Host {
            targets,
            parent: books.parent().unwrap_or_else(|| Path::new(".")).to_owned(),
            state: Mutex::default(),
            shutdown,
        }))
    }

    pub(crate) async fn call(
        &self,
        operation: DemoOperation,
        request: CallToolRequestParams,
    ) -> CallToolResult {
        let request = match DemoRequest::try_from((operation, request.arguments)) {
            Ok(request) => request,
            Err(error) => return error.result(),
        };
        let host = self.clone();
        // Finish lifecycle work even if the client cancels or drops its request.
        match tokio::spawn(async move {
            match request {
                DemoRequest::Start(arguments) => host.start(arguments).await,
                DemoRequest::Remove => host.remove().await,
            }
        })
        .await
        {
            Ok(Ok(report)) => report.result(),
            Ok(Err(error)) => error.result(),
            Err(_) => DemoError::Unavailable.result(),
        }
    }

    async fn start(
        &self,
        StartArguments { profile }: StartArguments,
    ) -> Result<DemoReport, DemoError> {
        let mut state = self.0.state.lock().await;
        if self.0.shutdown.is_cancelled() {
            return Err(DemoError::Unavailable);
        }
        match &*state {
            DemoState::Hosted(_) if profile.is_some() => return Err(DemoError::AlreadyActive),
            DemoState::Hosted(demo) => {
                return Ok(DemoReport::Active {
                    target: demo.target.clone(),
                });
            }
            DemoState::Detached(_) => return Err(DemoError::Cleanup),
            DemoState::DetachFailed => return Err(DemoError::Detach),
            DemoState::Absent => {}
        }
        let host = Arc::clone(&self.0);
        let demo = tokio::task::spawn_blocking(move || HostedDemo::create(&host, profile.as_ref()))
            .await
            .map_err(|_| DemoError::Create)??;
        let report = DemoReport::Active {
            target: demo.target.clone(),
        };
        *state = DemoState::Hosted(demo);
        Ok(report)
    }

    pub(crate) async fn remove(&self) -> Result<DemoReport, DemoError> {
        let mut state = self.0.state.lock().await;
        if let DemoState::Hosted(demo) = &*state {
            let detached = self.0.targets.detach(&demo.target).await;
            *state = match detached {
                Ok(proof) => DemoState::Detached(DetachedDemo {
                    directory: demo.directory.clone(),
                    proof,
                }),
                Err(_) => DemoState::DetachFailed,
            };
        }
        let demo = match &*state {
            DemoState::Detached(demo) => demo,
            DemoState::Absent => return Ok(DemoReport::Absent),
            DemoState::DetachFailed | DemoState::Hosted(_) => return Err(DemoError::Detach),
        };
        let report = demo.remove_files()?;
        *state = DemoState::Absent;
        Ok(report)
    }
}

impl HostedDemo {
    const FILE_NAME: &str = "demo.db";

    fn create(host: &Host, profile: Option<&DemoProfile>) -> Result<Self, DemoError> {
        const PREFIX: &str = "demo";
        let id = PREFIX.create_type_id::<V7>();
        let target: NamedLedgerTarget = format!("{PREFIX}-{}", id.suffix())
            .parse()
            .map_err(|_| DemoError::Create)?;
        let parent = host.parent.canonicalize().map_err(|_| DemoError::Create)?;
        let mut builder = Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let staging = builder
            .prefix(".beankeeper-demo-")
            .tempdir_in(parent)
            .map_err(|_| DemoError::Create)?;
        crate::setup::keychain::restrict_directory(staging.path())
            .map_err(|_| DemoError::Create)?;

        let actor = Actor::new("desktop-demo".parse().map_err(|_| DemoError::Create)?);
        let passphrase = crate::setup::generate_passphrase().map_err(|_| DemoError::Create)?;
        let path = staging.path().join(Self::FILE_NAME);
        let seeded = match profile {
            Some(profile) => {
                create_demo_ledger_from_profile(&path, Some(&passphrase), &actor, profile)
            }
            None => create_demo_ledger(&path, Some(&passphrase), &actor),
        }
        .map_err(|_| DemoError::Create)?;
        let db =
            Db::open_existing(seeded.path(), Some(&passphrase)).map_err(|_| DemoError::Create)?;
        Granted::<Admin>::with_db(&db, &actor, |grant| audit::set_policy(grant, true))
            .map_err(|_| DemoError::Create)?;

        host.targets
            .register_host_owned(target.clone().into(), db)
            .map_err(|_| DemoError::Create)?;
        // Once registered, ordinary scope exit must never delete the file.
        Ok(Self {
            target,
            directory: staging.keep(),
        })
    }
}

impl DetachedDemo {
    fn remove_files(&self) -> Result<DemoReport, DemoError> {
        let expected = self.directory.join(HostedDemo::FILE_NAME);
        let path = self.proof.path().ok_or(DemoError::Detach)?;
        if path == expected
            || path.canonicalize().is_ok_and(|path| {
                expected
                    .canonicalize()
                    .is_ok_and(|expected| path == expected)
            })
        {
            remove_if_present(fs::remove_file(path))?;
        } else if !expected
            .symlink_metadata()
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
        {
            return Err(DemoError::Detach);
        }
        remove_if_present(fs::remove_dir(&self.directory))?;
        Ok(DemoReport::Removed {
            target: self.proof.target().clone(),
        })
    }
}

fn remove_if_present(result: io::Result<()>) -> Result<(), DemoError> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(DemoError::Cleanup),
    }
}

/// Run a demo tool over the managed endpoint with its existing credential.
#[must_use = "return this status to the CLI caller"]
pub fn run(options: &DemoOptions, json_output: bool) -> ExitCode {
    let (operation, arguments) = match &options.command {
        DemoCommand::Start { profile } => (
            DemoOperation::Start,
            profile
                .as_deref()
                .map(ProfileArguments::try_from)
                .transpose(),
        ),
        DemoCommand::Remove => (DemoOperation::Remove, Ok(None)),
    };
    let arguments = match arguments {
        Ok(arguments) => arguments.map(JsonObject::from).unwrap_or_default(),
        Err(error) => return print_result(error.result(), json_output),
    };
    let result = match crate::bridge::call_demo(operation, &arguments) {
        Ok(result) => result,
        Err(error) => {
            let message = error.to_string();
            if json_output {
                eprintln!(
                    "{}",
                    json!({"code":error.diagnostic_code(), "message":message})
                );
            } else {
                eprintln!("{message}");
            }
            return ExitCode::FAILURE;
        }
    };
    print_result(result, json_output)
}

fn print_result(result: CallToolResult, json_output: bool) -> ExitCode {
    let output = if json_output {
        result.structured_content.unwrap_or(Value::Null).to_string()
    } else {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if result.is_error == Some(true) {
        eprintln!("{output}");
        ExitCode::FAILURE
    } else {
        println!("{output}");
        ExitCode::SUCCESS
    }
}
