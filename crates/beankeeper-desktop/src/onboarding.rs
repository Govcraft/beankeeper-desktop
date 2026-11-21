//! Setup readiness shared by local CLI and authenticated MCP.

use std::{
    process::ExitCode,
    sync::{Arc, Mutex},
};

use beankeeper::types::Currency;
use beankeeper_cli::{
    capability::Capability,
    cli::Cli,
    db::{self, Db, accounts::ListAccountParams},
    error::CliError,
    identity::{self, VerifiedAgent},
    mcp::{HostTool, HostToolEffect, HostToolHandler},
    output::json::{self, Envelope},
    passphrase,
};
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool},
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize, Serializer};

use crate::{navigation::Navigation, setup};

const STATUS_TOOL: &str = "setup_status";

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LedgerSnapshot {
    MissingLedger,
    Installed { books: Vec<Book> },
}

#[derive(Serialize)]
struct Book {
    id: String,
    name: String,
    accounts: Vec<Account>,
    observed_currencies: Vec<CurrencyCode>,
    base_currency: SettingState,
    fiscal_year_start: SettingState,
    imported_history: ImportHistory,
}

#[derive(Serialize)]
struct Account {
    code: String,
    name: String,
}

struct CurrencyCode(Currency);

impl Serialize for CurrencyCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.code())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum SettingState {
    Unavailable,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ImportHistory {
    NoTransactions,
    SourceUnknown,
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
            Self::ListAccounts => {
                "Confirm which books to keep, then ask for the names of the accounts the owner uses."
            }
            Self::ReviewOpeningBalances => {
                "Confirm that the listed accounts match the owner's books. Ask about starting balances and their dates."
            }
            Self::ReviewExistingActivity => {
                "These books already contain transactions. Confirm which statements and opening balances have already been entered before adding anything."
            }
        }
    }
}

#[derive(Serialize)]
struct SetupReport {
    #[serde(flatten)]
    ledger: LedgerSnapshot,
    next_step: NextStep,
    summary: String,
}

impl SetupReport {
    fn from_snapshot(ledger: LedgerSnapshot) -> Self {
        let next_step = match &ledger {
            LedgerSnapshot::MissingLedger => NextStep::InstallLedger,
            LedgerSnapshot::Installed { books } if books.is_empty() => NextStep::ChooseBooks,
            LedgerSnapshot::Installed { books }
                if books.iter().any(|book| book.accounts.is_empty()) =>
            {
                NextStep::ListAccounts
            }
            LedgerSnapshot::Installed { books }
                if books.iter().all(|book| book.observed_currencies.is_empty()) =>
            {
                NextStep::ReviewOpeningBalances
            }
            LedgerSnapshot::Installed { .. } => NextStep::ReviewExistingActivity,
        };
        let summary = match &ledger {
            LedgerSnapshot::MissingLedger => {
                format!("No local ledger exists yet. {}", next_step.summary())
            }
            LedgerSnapshot::Installed { books } => {
                let book_count = books.len();
                let account_count: usize = books.iter().map(|book| book.accounts.len()).sum();
                format!(
                    "The local ledger has {book_count} {} and {account_count} {}. {}",
                    if book_count == 1 {
                        "set of books"
                    } else {
                        "sets of books"
                    },
                    if account_count == 1 {
                        "account"
                    } else {
                        "accounts"
                    },
                    next_step.summary()
                )
            }
        };
        Self {
            ledger,
            summary,
            next_step,
        }
    }
}

impl LedgerSnapshot {
    fn read(db: &Db) -> Result<Self, CliError> {
        let conn = db.conn();
        let mut currencies = conn.prepare(
            "SELECT DISTINCT currency FROM transactions WHERE company_slug = ?1 ORDER BY currency",
        )?;
        let books = db::list_companies(conn)?
            .into_iter()
            .map(|company| {
                let accounts = db::list_accounts(
                    conn,
                    &ListAccountParams {
                        company_slug: &company.slug,
                        type_filter: None,
                        name_filter: None,
                    },
                )?
                .into_iter()
                .map(|row| Account {
                    code: row.code,
                    name: row.name,
                })
                .collect();
                let observed_currencies = currencies
                    .query_map([&company.slug], |row| row.get::<_, String>(0))?
                    .map(|code| {
                        Currency::from_code(&code?).map(CurrencyCode).map_err(|_| {
                            CliError::Database("a posted currency code is invalid".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let imported_history = if observed_currencies.is_empty() {
                    ImportHistory::NoTransactions
                } else {
                    ImportHistory::SourceUnknown
                };
                Ok(Book {
                    id: company.slug,
                    name: company.name,
                    accounts,
                    observed_currencies,
                    // The current ledger schema does not persist these settings.
                    base_currency: SettingState::Unavailable,
                    fiscal_year_start: SettingState::Unavailable,
                    imported_history,
                })
            })
            .collect::<Result<_, CliError>>()?;
        Ok(Self::Installed { books })
    }
}

fn inspect_cli(cli: &mut Cli) -> Result<(SetupReport, Option<VerifiedAgent>), CliError> {
    if !cli.db.try_exists()? {
        return Ok((
            SetupReport::from_snapshot(LedgerSnapshot::MissingLedger),
            None,
        ));
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
    let _snapshot = db.conn().unchecked_transaction()?;
    let agent = identity::authorize(
        db.conn(),
        cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
        Capability::Read,
    )?;
    let report = SetupReport::from_snapshot(LedgerSnapshot::read(&db)?);
    Ok((report, agent))
}

/// Print read-only setup status using the normal CLI output envelope and exit codes.
pub fn run_status(mut cli: Cli) -> ExitCode {
    let use_json = cli.is_json();
    let mut meta = json::meta("setup.status", None);
    let result = inspect_cli(&mut cli).and_then(|(report, agent)| {
        meta.agent = agent.map(|agent| agent.uri().to_owned());
        if use_json {
            let envelope = Envelope {
                ok: true,
                meta: meta.clone(),
                data: Some(report),
                error: None,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&envelope).map_err(|error| CliError::General(
                    format!("cannot encode setup status: {error}")
                ))?
            );
        } else {
            println!("{}", report.summary);
        }
        Ok(())
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error.report(use_json, Some(meta));
            ExitCode::from(error.exit_code())
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusInput {}

pub(crate) struct DesktopTools {
    pub(crate) navigation: Navigation,
    pub(crate) status_db: Arc<Mutex<Db>>,
}

impl HostToolHandler for DesktopTools {
    fn tools(&self) -> Vec<HostTool> {
        let mut tools = self.navigation.tools();
        tools.push(HostTool {
            tool: Tool::new(
                STATUS_TOOL,
                "Read the current books, account inventory, and recommended next setup step. Never creates or changes the books.",
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
        if request.name != STATUS_TOOL {
            return self.navigation.call_tool(request, identity).await;
        }
        if request
            .arguments
            .is_some_and(|arguments| !arguments.is_empty())
        {
            return Ok(tool_error(&CliError::Usage(
                "setup_status takes no arguments".into(),
            )));
        }
        let status_db = Arc::clone(&self.status_db);
        let result = tokio::task::spawn_blocking(move || {
            let db = status_db
                .lock()
                .map_err(|_| CliError::Database("setup status connection is unavailable".into()))?;
            let _snapshot = db.conn().unchecked_transaction()?;
            let report = SetupReport::from_snapshot(LedgerSnapshot::read(&db)?);
            let value = serde_json::to_value(&report).map_err(|error| {
                CliError::General(format!("cannot encode setup status: {error}"))
            })?;
            let mut result = CallToolResult::structured(value);
            result.content = vec![ContentBlock::text(report.summary)];
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
