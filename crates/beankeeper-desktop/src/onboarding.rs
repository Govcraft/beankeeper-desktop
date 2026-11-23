//! Setup readiness shared by local CLI and authenticated MCP.

use std::{
    net::SocketAddr,
    process::ExitCode,
    sync::{Arc, Mutex},
};

use beankeeper_cli::{
    capability::Capability,
    cli::Cli,
    commands::setup::{SetupStatus, status},
    db::Db,
    error::CliError,
    identity::{self, VerifiedAgent},
    mcp::{HostTool, HostToolEffect, HostToolHandler},
    output::json::EnvelopeError,
    passphrase,
};
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, JsonObject, Tool},
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::{
    links::mcp as link_tool,
    navigation::Navigation,
    output::{Envelope, Metadata},
    setup,
};

pub(crate) const STATUS_TOOL: &str = "setup_status";

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LedgerState {
    MissingLedger,
    Installed {
        #[serde(flatten)]
        status: SetupStatus,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum NextStep {
    InstallLedger,
    ChooseBooks,
    ListAccounts,
    ReviewOpeningBalances,
    ReviewExistingActivity,
}

impl NextStep {
    fn summary(&self) -> &'static str {
        match self {
            Self::InstallLedger => "Install Beankeeper and save the owner's recovery key.",
            Self::ChooseBooks => {
                "Ask whether these books are for personal finances, a business, or both."
            }
            Self::ListAccounts => "Ask for the names of the accounts the owner uses.",
            Self::ReviewOpeningBalances => {
                "Check each book’s accounts and ask about starting balances and their dates."
            }
            Self::ReviewExistingActivity => {
                "These books already contain transactions. Check each book’s accounts and ask which statements and opening balances are already recorded."
            }
        }
    }
}

#[derive(Serialize)]
struct SetupReport {
    #[serde(flatten)]
    ledger: LedgerState,
    next_step: NextStep,
    summary: String,
    agent_access: Option<setup::AgentAccess>,
}

impl SetupReport {
    fn new(ledger: LedgerState, agent: Option<&VerifiedAgent>) -> Self {
        let (next_step, summary) = match &ledger {
            LedgerState::MissingLedger => (
                NextStep::InstallLedger,
                "No local ledger exists yet.".into(),
            ),
            LedgerState::Installed { status } => {
                let next_step = match status {
                    SetupStatus { companies: 0, .. } => NextStep::ChooseBooks,
                    SetupStatus { accounts: 0, .. } => NextStep::ListAccounts,
                    SetupStatus {
                        transactions: 0, ..
                    } => NextStep::ReviewOpeningBalances,
                    _ => NextStep::ReviewExistingActivity,
                };
                let SetupStatus {
                    companies,
                    accounts,
                    ..
                } = status;
                let books_label = if *companies == 1 {
                    "set of books"
                } else {
                    "sets of books"
                };
                let accounts_label = if *accounts == 1 {
                    "account"
                } else {
                    "accounts"
                };
                (
                    next_step,
                    format!(
                        "The local ledger has {companies} {books_label} and {accounts} {accounts_label}."
                    ),
                )
            }
        };
        let summary = format!("{summary} {}", next_step.summary());
        Self {
            ledger,
            next_step,
            summary,
            agent_access: agent.map(setup::AgentAccess::from),
        }
    }

    fn plain_text(&self) -> String {
        match &self.agent_access {
            Some(access) => format!("{}\n{}", self.summary, access.note()),
            None => self.summary.clone(),
        }
    }
}

fn inspect_cli(cli: &mut Cli) -> Result<(SetupReport, Option<VerifiedAgent>), CliError> {
    if !cli.db.try_exists()? {
        return Ok((SetupReport::new(LedgerState::MissingLedger, None), None));
    }
    let _guard = setup::stored_passphrase(cli).map_err(|error| match error {
        setup::SetupError::Io(error) => CliError::Io(error),
        setup::SetupError::Ledger(error) => error,
        error => CliError::General(error.to_string()),
    })?;
    let secret = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let db = Db::open_snapshot(&cli.db, secret.as_ref())?;
    db.read_snapshot(|reader| {
        let agent = identity::authorize(
            reader,
            cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
            Capability::Read,
        )?;
        let report = StatusInput::default().read(&db, agent.as_ref())?;
        Ok((report, agent))
    })
}

#[derive(Clone, Copy, Serialize)]
enum SetupOperation {
    #[serde(rename = "setup.status")]
    Status,
}

/// Print read-only setup status using the Desktop CLI response envelope and standard exit codes.
pub fn run_status(mut cli: Cli) -> ExitCode {
    let use_json = cli.is_json();
    let mut meta = Metadata::new(SetupOperation::Status);
    let result = inspect_cli(&mut cli).and_then(|(report, agent)| {
        meta.agent = agent.map(|agent| agent.uri().to_owned());
        if use_json {
            let envelope = Envelope {
                ok: true,
                meta: meta.clone(),
                data: Some(report),
                error: None::<EnvelopeError>,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&envelope).map_err(|error| {
                    CliError::General(format!("cannot encode setup status: {error}"))
                })?
            );
        } else {
            println!("{}", report.plain_text());
        }
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if use_json {
                let envelope = Envelope {
                    ok: false,
                    meta,
                    data: None::<SetupReport>,
                    error: Some(EnvelopeError {
                        code: error.code(),
                        message: error.to_string(),
                    }),
                };
                match serde_json::to_string_pretty(&envelope) {
                    Ok(response) => eprintln!("{response}"),
                    Err(_) => error.report(true, None),
                }
            } else {
                error.report(false, None);
            }
            ExitCode::from(error.exit_code())
        }
    }
}

#[derive(Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusInput {}

impl TryFrom<Option<JsonObject>> for StatusInput {
    type Error = CliError;

    fn try_from(arguments: Option<JsonObject>) -> Result<Self, Self::Error> {
        serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default()))
            .map_err(|_| CliError::Usage("setup_status takes no arguments".into()))
    }
}

impl StatusInput {
    fn read(self, db: &Db, agent: Option<&VerifiedAgent>) -> Result<SetupReport, CliError> {
        let Self {} = self;
        let ledger = LedgerState::Installed {
            status: status(db)?,
        };
        Ok(SetupReport::new(ledger, agent))
    }
}

pub(crate) struct DesktopTools {
    pub(crate) bound: SocketAddr,
    pub(crate) navigation: Navigation,
    pub(crate) status_db: Arc<Mutex<Db>>,
}

impl HostToolHandler for DesktopTools {
    fn tools(&self) -> Vec<HostTool> {
        let mut tools = self.navigation.tools();
        tools.push(link_tool::definition());
        tools.push(HostTool {
            tool: Tool::new(
                STATUS_TOOL,
                "Call this first when the owner asks to set up their books. Read the ledger counts, recommended next step, and your verified grant and write mode.",
                schemars::schema_for!(StatusInput).to_value().as_object().cloned().unwrap_or_default(),
            ),
            capability: Capability::Read,
            effect: HostToolEffect::ReadOnly,
        });
        tools
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        identity: VerifiedAgent,
    ) -> Result<CallToolResult, ErrorData> {
        if request.name == link_tool::NAME {
            return Ok(link_tool::call(
                serde_json::Value::Object(request.arguments.unwrap_or_default()),
                self.bound,
            ));
        }
        if request.name != STATUS_TOOL {
            return self.navigation.call_tool(request, identity).await;
        }
        let input = match StatusInput::try_from(request.arguments) {
            Ok(input) => input,
            Err(error) => return Ok(tool_error(&error)),
        };
        let status_db = Arc::clone(&self.status_db);
        let result = tokio::task::spawn_blocking(move || {
            let db = status_db
                .lock()
                .map_err(|_| CliError::Database("setup status connection is unavailable".into()))?;
            let report = db.read_snapshot(|_| input.read(&db, Some(&identity)))?;
            let value = serde_json::to_value(&report).map_err(|error| {
                CliError::General(format!("cannot encode setup status: {error}"))
            })?;
            let mut result = CallToolResult::structured(value);
            result.content = vec![ContentBlock::text(report.plain_text())];
            Ok::<_, CliError>(result)
        })
        .await;
        Ok(match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => tool_error(&error),
            Err(_) => tool_error(&CliError::General("setup status worker failed".into())),
        })
    }
}

fn tool_error(error: &CliError) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": error.error_code(),
        "message": error.to_string(),
    }))
}
