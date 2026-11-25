use std::sync::{Arc, Mutex};

use beankeeper::types::AccountCode;
use beankeeper_cli::db::{
    Db,
    companies::{Company, CompanySlug},
    ids::TransactionId,
    proposals::{self, ProposalId},
    transactions::get_entries_for_transaction,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) struct Scope {
    company: CompanySlug,
    suspense: AccountCode,
}

impl From<(&CompanySlug, &AccountCode)> for Scope {
    fn from((company, suspense): (&CompanySlug, &AccountCode)) -> Self {
        Self {
            company: company.clone(),
            suspense: suspense.clone(),
        }
    }
}

#[derive(Deserialize)]
struct Recorded {
    transaction_id: TransactionId,
}

#[derive(Deserialize)]
struct Proposed {
    #[serde(deserialize_with = "proposal_id")]
    proposal_id: ProposalId,
}

fn proposal_id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<ProposalId, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

#[derive(Deserialize)]
struct Outcomes {
    #[serde(default)]
    created: Vec<Recorded>,
    #[serde(default)]
    proposals: Vec<Proposed>,
    preexisting: Vec<serde::de::IgnoredAny>,
}

#[derive(Serialize)]
struct Counts {
    recorded: usize,
    skipped_duplicates: usize,
    pending: usize,
    uncertain: Option<usize>,
}

impl Scope {
    pub(super) async fn summarize(self, db: Arc<Mutex<Db>>, data: &mut Value) -> Option<String> {
        let outcomes = Outcomes::deserialize(&*data).ok()?;
        let mut counts = Counts {
            recorded: outcomes.created.len(),
            pending: outcomes.proposals.len(),
            skipped_duplicates: outcomes.preexisting.len(),
            uncertain: None,
        };
        counts.uncertain = tokio::task::spawn_blocking(move || {
            let db = db.lock().ok()?;
            db.read_snapshot(|reader| {
                let company = Company::bind(reader, self.company.clone())?;
                let mut uncertain = 0;
                for row in outcomes.created {
                    let entries = get_entries_for_transaction(&company, row.transaction_id)?;
                    uncertain += usize::from(
                        entries
                            .iter()
                            .any(|entry| entry.account_code == self.suspense.as_str()),
                    );
                }
                for row in outcomes.proposals {
                    let proposal = proposals::get(reader, &self.company, &row.proposal_id)?;
                    uncertain += usize::from(
                        proposal
                            .payload
                            .entries
                            .iter()
                            .any(|entry| entry.account_code == self.suspense.as_str()),
                    );
                }
                Ok(uncertain)
            })
            .ok()
        })
        .await
        .ok()
        .flatten();
        let message = counts.message();
        data["counts"] = json!(counts);
        Some(message)
    }
}

impl Counts {
    fn message(&self) -> String {
        let Self {
            recorded,
            skipped_duplicates,
            pending,
            uncertain,
        } = self;
        let mut message =
            format!("Recorded {recorded} and skipped {skipped_duplicates} duplicates.");
        if *pending > 0 {
            use std::fmt::Write as _;
            let _ = write!(message, " {pending} are pending.");
        }
        if let Some(uncertain) = uncertain {
            use std::fmt::Write as _;
            let _ = write!(message, " {uncertain} need categorization.");
        } else {
            message.push_str(" The count needing categorization is unavailable.");
        }
        message
    }
}
