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
    commands::init::create_demo_ledger,
    db::{Actor, Db, audit},
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
    operation: DemoOperation,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub(crate) enum DemoOperation {
    /// Prepare sample books using the running managed service.
    Start,
    /// Discard the sample books and their practice entries.
    Remove,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

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
        let description = match self {
            Self::Start => {
                "Try Beankeeper with fictional books. Creates or reuses encrypted sample books and selects them in a managed connection. Tell the owner that every figure is fictional. HTTP clients select the returned target on each ledger request."
            }
            Self::Remove => {
                "Discard the fictional books, including any practice entries. A managed connection returns to the owner's books after this succeeds."
            }
        };
        HostTool {
            tool: Tool::new(
                self.name(),
                description,
                schemars::schema_for!(NoArguments)
                    .to_value()
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ),
            capability: Capability::Read,
            effect: HostToolEffect::Mutation,
        }
    }
}

impl TryFrom<Option<JsonObject>> for NoArguments {
    type Error = DemoError;

    fn try_from(arguments: Option<JsonObject>) -> Result<Self, Self::Error> {
        serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
            .map_err(|_| DemoError::Arguments)
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
    Create,
    Detach,
    Cleanup,
    Unavailable,
}

impl DemoError {
    fn code(&self) -> &'static str {
        match self {
            Self::Arguments => "DEMO_ARGUMENTS",
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
            Self::Arguments => "Demo tools take no arguments.",
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
        if let Err(error) = NoArguments::try_from(request.arguments) {
            return error.result();
        }
        let host = self.clone();
        // Finish lifecycle work even if the client cancels or drops its request.
        match tokio::spawn(async move {
            match operation {
                DemoOperation::Start => host.start().await,
                DemoOperation::Remove => host.remove().await,
            }
        })
        .await
        {
            Ok(Ok(report)) => report.result(),
            Ok(Err(error)) => error.result(),
            Err(_) => DemoError::Unavailable.result(),
        }
    }

    async fn start(&self) -> Result<DemoReport, DemoError> {
        let mut state = self.0.state.lock().await;
        if self.0.shutdown.is_cancelled() {
            return Err(DemoError::Unavailable);
        }
        match &*state {
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
        let demo = tokio::task::spawn_blocking(move || HostedDemo::create(&host))
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

    fn create(host: &Host) -> Result<Self, DemoError> {
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
        let seeded = create_demo_ledger(
            &staging.path().join(Self::FILE_NAME),
            Some(&passphrase),
            &actor,
        )
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
#[must_use]
pub fn run(options: &DemoOptions, json_output: bool) -> ExitCode {
    let result = match crate::bridge::call_demo(options.operation) {
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
