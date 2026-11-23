//! Desktop's local acknowledgement and presentation of upstream audit receipts.

mod browser;
mod command;
mod marker;
mod mcp;

use std::{fmt, net::SocketAddr};

use beankeeper_cli::{
    db::{
        Db,
        audit::receipts::{
            AuditCursor, AuditReceipt, ReceiptChange, ReceiptId, ReceiptPage, ReceiptPageLimit,
            list_receipts,
        },
        companies::{CompanySlug, list_companies},
    },
    error::CliError,
};
use serde::Serialize;

use crate::{instance::BooksPath, server, views::encode};

pub(crate) use browser::router;
pub use command::{MarkSeen, run_mark_seen, run_since};
pub(crate) use mcp::{MARK_TOOL, SINCE_TOOL, call, definitions, failure};

pub(crate) const PATH: &str = "/changes";

fn book(
    db: &Db,
    requested: Option<CompanySlug>,
    configured: Option<CompanySlug>,
) -> Result<CompanySlug, CliError> {
    if let Some(company) = requested.or(configured) {
        return Ok(company);
    }
    match list_companies(db.read())?.as_slice() {
        [company] => Ok(company.slug.clone()),
        [] => Err(CliError::NotFound("there are no books yet".into())),
        _ => Err(CliError::Validation(
            "choose the book to check for changes".into(),
        )),
    }
}

#[derive(Clone, Copy, Serialize)]
struct ReportRange {
    after: AuditCursor,
    through: AuditCursor,
}

impl TryFrom<(AuditCursor, AuditCursor)> for ReportRange {
    type Error = CliError;

    fn try_from((after, through): (AuditCursor, AuditCursor)) -> Result<Self, Self::Error> {
        if after > through {
            return Err(CliError::Validation(
                "through must be at or after the starting cursor".into(),
            ));
        }
        Ok(Self { after, through })
    }
}

impl ReportRange {
    fn path(self, company: &CompanySlug) -> String {
        format!(
            "{PATH}?company={}&after={}&through={}",
            encode(company.as_str()),
            self.after,
            self.through
        )
    }
}

struct Report {
    company: CompanySlug,
    range: ReportRange,
    page: ReceiptPage,
    has_more: bool,
}

impl Report {
    fn read(
        db: &Db,
        books: &BooksPath,
        company: CompanySlug,
        range: Option<ReportRange>,
    ) -> Result<Self, CliError> {
        let after = match range {
            Some(range) => range.after,
            None => marker::Marker::new(books, &company)?.read()?,
        };
        let page = list_receipts(db, &company, after, ReceiptPageLimit::default())?;
        let through = page_through(&page)?;
        let through = range.map_or(through, |range| through.min(range.through));
        let has_more = page.continuation().is_some() && range.is_none();
        Ok(Self {
            company,
            range: ReportRange { after, through },
            page,
            has_more,
        })
    }

    fn receipts(&self) -> impl Iterator<Item = &AuditReceipt> {
        self.page
            .receipts()
            .iter()
            .take_while(|receipt| receipt.id().get() <= i64::from(self.range.through))
    }

    fn path(&self) -> String {
        self.range.path(&self.company)
    }

    fn summary(&self) -> String {
        match self.receipts().count() {
            0 => format!("No changes to report for {}.", self.company),
            1 => format!("There is one change to report for {}.", self.company),
            count => format!("There are {count} changes to report for {}.", self.company),
        }
    }

    fn data(&self, bound: Option<SocketAddr>) -> ReportData<'_> {
        let desktop_url = bound.map(|bound| {
            format!(
                "{}{}",
                server::local_url(bound).trim_end_matches('/'),
                self.path()
            )
        });
        let receipts = self
            .receipts()
            .map(|receipt| ReceiptData {
                receipt,
                source_reference: receipt.source_reference(),
                description: description(receipt),
                undo_url: desktop_url.as_ref().and_then(|url| {
                    UndoLink::try_from(receipt)
                        .ok()
                        .map(|target| target.link(url))
                }),
            })
            .collect();
        ReportData {
            company: &self.company,
            range: self.range,
            summary: self.summary(),
            has_more: self.has_more,
            receipts,
            desktop_url,
        }
    }
}

fn page_through(page: &ReceiptPage) -> Result<AuditCursor, CliError> {
    if page.continuation().is_some()
        && let Some(receipt) = page.receipts().last()
    {
        return receipt.id().get().try_into();
    }
    Ok(page.through())
}

// Carries the receipt whose shape supports an undo link. Actual eligibility
// depends on the live ledger and is checked by the upstream owner grant.
#[derive(Clone, Copy)]
struct UndoLink(ReceiptId);

struct NoUndoLink;

impl TryFrom<&AuditReceipt> for UndoLink {
    type Error = NoUndoLink;

    fn try_from(receipt: &AuditReceipt) -> Result<Self, Self::Error> {
        match receipt.change().summary() {
            ReceiptChange::AccountCreated { .. } | ReceiptChange::PostingCreated { .. } => {}
            ReceiptChange::ProposalResolved { .. } if receipt.posting().is_some() => {}
            _ => return Err(NoUndoLink),
        }
        Ok(Self(receipt.id()))
    }
}

impl UndoLink {
    fn link(self, page: &str) -> String {
        format!("{page}#receipt-{}", self.0)
    }
}

fn description(receipt: &AuditReceipt) -> String {
    let change = receipt.change();
    match change.summary() {
        ReceiptChange::AccountCreated { after } => format!("Opened the {} account", after.name),
        ReceiptChange::AccountDeleted { before } => format!("Removed the {} account", before.name),
        ReceiptChange::AccountArchived => "Archived an account".into(),
        ReceiptChange::PostingCreated { after } => format!("Recorded {}", after.description),
        ReceiptChange::PostingReversed => "Reversed a transaction".into(),
        ReceiptChange::PostingCorrelated => "Linked two transactions".into(),
        ReceiptChange::ProposalResolved { .. } if receipt.posting().is_some() => {
            "Posted an approved transaction".into()
        }
        _ => format!("Recorded a change ({})", change.action()),
    }
}

#[derive(Serialize)]
struct ReportData<'a> {
    company: &'a CompanySlug,
    #[serde(flatten)]
    range: ReportRange,
    summary: String,
    has_more: bool,
    receipts: Vec<ReceiptData<'a>>,
    desktop_url: Option<String>,
}

impl fmt::Display for ReportData<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.summary)?;
        for row in &self.receipts {
            let change = row.receipt.change();
            writeln!(
                f,
                "- {} ({} by {})",
                row.description,
                change.changed_at(),
                change.actor().as_str()
            )?;
            if let Some(url) = &row.undo_url {
                writeln!(f, "  Undo: {url}")?;
            }
        }
        if let Some(url) = &self.desktop_url {
            writeln!(f, "Desktop details: {url}")?;
        }
        if self.has_more {
            writeln!(f, "More changes are waiting.")?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ReceiptData<'a> {
    #[serde(flatten)]
    receipt: &'a AuditReceipt,
    source_reference: Option<&'a str>,
    description: String,
    undo_url: Option<String>,
}

#[derive(Serialize)]
struct Acknowledged {
    company: CompanySlug,
    last_seen: AuditCursor,
}

impl fmt::Display for Acknowledged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Marked the changes for {} seen through audit position {}.",
            self.company, self.last_seen
        )
    }
}

fn acknowledge(
    db: &Db,
    books: &BooksPath,
    company: CompanySlug,
    through: AuditCursor,
) -> Result<Acknowledged, CliError> {
    let last_seen = marker::Marker::new(books, &company)?.advance(db, &company, through)?;
    Ok(Acknowledged { company, last_seen })
}
