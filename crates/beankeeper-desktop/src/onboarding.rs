//! Setup readiness shared by local CLI and authenticated MCP.

use std::{
    net::SocketAddr,
    process::ExitCode,
    sync::{Arc, Mutex},
};

use beankeeper::types::Currency;
use beankeeper_cli::{
    capability::Capability,
    cli::Cli,
    commands::setup::{
        BookAccountCode, BookFacts, BookProfile, BookSettings, ProfileRevision,
        StatementImportSummary, book_facts, statement_import_summary,
    },
    db::{self, Db, companies::CompanySlug},
    error::CliError,
    identity::{self, VerifiedAgent},
    mcp::{HostTool, HostToolEffect, HostToolHandler},
    output::json::EnvelopeError,
    passphrase,
};
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool},
};
use secrecy::ExposeSecret;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::{
    links::mcp as link_tool,
    navigation::Navigation,
    output::{Envelope, Metadata},
    setup,
};

const STATUS_TOOL: &str = "setup_status";

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum LedgerSnapshot {
    MissingLedger,
    Installed { books: Vec<Book> },
}

#[derive(Serialize)]
struct Book {
    id: CompanySlug,
    name: String,
    accounts: Vec<Account>,
    observed_currencies: Vec<CurrencyCode>,
    #[serde(flatten)]
    profile: ProfileStatus,
    #[serde(flatten)]
    imported_history: ImportHistory,
}

#[derive(Serialize)]
struct Account {
    #[serde(serialize_with = "serialize_account_code")]
    code: BookAccountCode,
    name: String,
}

fn serialize_account_code<S: Serializer>(
    code: &BookAccountCode,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(code.as_str())
}

#[derive(Serialize)]
#[serde(untagged)]
enum ProfileStatus {
    Unavailable {
        base_currency: SettingState,
        fiscal_year_start: SettingState,
    },
    Recorded {
        profile_revision: ProfileRevision,
        #[serde(flatten)]
        settings: BookSettings,
    },
}

impl From<Option<BookProfile<'_>>> for ProfileStatus {
    fn from(profile: Option<BookProfile<'_>>) -> Self {
        match profile {
            Some(profile) => Self::Recorded {
                profile_revision: profile.revision,
                settings: profile.settings,
            },
            None => Self::Unavailable {
                base_currency: SettingState::Unavailable,
                fiscal_year_start: SettingState::Unavailable,
            },
        }
    }
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
#[serde(tag = "imported_history", rename_all = "snake_case")]
enum ImportHistory {
    NoTransactions,
    SourceUnknown,
    StatementsRecorded {
        #[serde(serialize_with = "serialize_statement_imports")]
        statement_imports: StatementImportSummary,
    },
}

fn serialize_statement_imports<S: Serializer>(
    summary: &StatementImportSummary,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut counts = serializer.serialize_struct("StatementImportSummary", 3)?;
    counts.serialize_field("committed_batches", &summary.committed_batches)?;
    counts.serialize_field("created_transactions", &summary.created_transactions)?;
    counts.serialize_field("preexisting_matches", &summary.preexisting_matches)?;
    counts.end()
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
            next_step,
            summary,
        }
    }
}

impl LedgerSnapshot {
    fn read(db: &Db) -> Result<Self, CliError> {
        let conn = db.read();
        let mut currencies = conn.prepare(
            "SELECT DISTINCT currency FROM transactions WHERE company_slug = ?1 ORDER BY currency",
        )?;
        let books = db::list_companies(conn)?
            .into_iter()
            .map(|company| {
                let company_slug = company.slug;
                let BookFacts {
                    name,
                    accounts,
                    profile,
                    ..
                } = book_facts(db, &company_slug)?;
                let accounts = accounts
                    .into_iter()
                    .map(|account| Account {
                        code: account.code,
                        name: account.name,
                    })
                    .collect();
                let observed_currencies = currencies
                    .query_map([&company_slug], |row| row.get::<_, String>(0))?
                    .map(|code| {
                        Currency::from_code(&code?).map(CurrencyCode).map_err(|_| {
                            CliError::Database("a posted currency code is invalid".into())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let statement_imports = statement_import_summary(db, &company_slug)?;
                let imported_history = if statement_imports.committed_batches != 0 {
                    ImportHistory::StatementsRecorded { statement_imports }
                } else if observed_currencies.is_empty() {
                    ImportHistory::NoTransactions
                } else {
                    ImportHistory::SourceUnknown
                };
                let profile = profile.into();
                Ok(Book {
                    id: company_slug,
                    name,
                    accounts,
                    observed_currencies,
                    profile,
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
    db.read_snapshot(|reader| {
        let agent = identity::authorize(
            reader,
            cli.agent_token.as_ref().map(ExposeSecret::expose_secret),
            Capability::Read,
        )?;
        let report = SetupReport::from_snapshot(LedgerSnapshot::read(&db)?);
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
            println!("{}", report.summary);
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

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusInput {}

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
                "Call this first when the owner asks to set up their books. Read the current books, account inventory, and recommended next setup step. Never creates or changes the books.",
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
            let report =
                db.read_snapshot(|_| LedgerSnapshot::read(&db).map(SetupReport::from_snapshot))?;
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
