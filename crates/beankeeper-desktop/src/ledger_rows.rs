//! Typed reads from the currently pinned Beankeeper row API.

use std::{collections::HashSet, error::Error, fmt, num::ParseIntError};

use beankeeper::types::{
    AccountType, AccountTypeError, Amount, ClearanceStatus, Currency, CurrencyError, DebitOrCredit,
    Money,
};
use beankeeper_cli::db::{
    AccountRow, AuditLogRow, EntryRow,
    audit::{AuditEntity, EntryStatus},
};

#[derive(Debug)]
pub(crate) enum RowError {
    AccountType(AccountTypeError),
    Currency(CurrencyError),
    AuditId(ParseIntError),
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AccountType(error) => write!(f, "invalid ledger account type: {error}"),
            Self::Currency(error) => write!(f, "invalid transaction currency: {error}"),
            Self::AuditId(error) => write!(f, "invalid ledger audit record ID: {error}"),
        }
    }
}

impl Error for RowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(match self {
            Self::AccountType(error) => error,
            Self::Currency(error) => error,
            Self::AuditId(error) => error,
        })
    }
}

fn account_kind_order(kind: AccountType) -> u8 {
    match kind {
        AccountType::Asset => 0,
        AccountType::Liability => 1,
        AccountType::Equity => 2,
        AccountType::Revenue => 3,
        AccountType::Expense => 4,
        _ => 5,
    }
}

pub(crate) struct AccountGroup<'a> {
    pub(crate) kind: AccountType,
    pub(crate) rows: Vec<&'a AccountRow>,
}

pub(crate) fn accounts_by_kind(rows: &[AccountRow]) -> Result<Vec<AccountGroup<'_>>, RowError> {
    let mut groups = Vec::<AccountGroup<'_>>::new();
    for row in rows {
        let kind = row.account_type.parse().map_err(RowError::AccountType)?;
        if let Some(group) = groups.iter_mut().find(|group| group.kind == kind) {
            group.rows.push(row);
        } else {
            groups.push(AccountGroup {
                kind,
                rows: vec![row],
            });
        }
    }
    groups.sort_by_key(|group| account_kind_order(group.kind));
    Ok(groups)
}

pub(crate) fn currency(code: &str) -> Result<Currency, RowError> {
    code.parse().map_err(RowError::Currency)
}

pub(crate) const fn clearance(status: EntryStatus) -> ClearanceStatus {
    match status {
        EntryStatus::Uncleared => ClearanceStatus::Uncleared,
        EntryStatus::Cleared => ClearanceStatus::Cleared,
        EntryStatus::Reconciled => ClearanceStatus::Reconciled,
    }
}

pub(crate) struct EntryValues {
    direction: DebitOrCredit,
    pub(crate) clearance: ClearanceStatus,
    pub(crate) amount: Money,
}

impl EntryValues {
    pub(crate) fn read(row: &EntryRow, currency: Currency) -> Self {
        Self {
            direction: row.direction,
            clearance: clearance(row.status),
            amount: Money::new(Amount::from(row.amount), currency),
        }
    }

    pub(crate) const fn direction_label(&self) -> &'static str {
        self.direction.code()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuditSubject {
    Transaction(i64),
    Entry(i64),
}

impl AuditSubject {
    pub(crate) fn read(row: &AuditLogRow) -> Result<Option<Self>, RowError> {
        let parse_id = || row.entity_id.parse().map_err(RowError::AuditId);
        match row.entity {
            AuditEntity::Transaction => parse_id().map(Self::Transaction).map(Some),
            AuditEntity::Entry => parse_id().map(Self::Entry).map(Some),
            _ => Ok(None),
        }
    }

    pub(crate) fn matches(&self, transaction: i64, entries: &HashSet<i64>) -> bool {
        match self {
            Self::Transaction(id) => *id == transaction,
            Self::Entry(id) => entries.contains(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_subjects_compare_the_id_in_the_correct_entity_namespace() {
        let entries = HashSet::from([12, 13]);
        assert!(AuditSubject::Transaction(10).matches(10, &entries));
        assert!(!AuditSubject::Entry(10).matches(10, &entries));
        assert!(AuditSubject::Entry(12).matches(10, &entries));
        assert!(!AuditSubject::Transaction(12).matches(10, &entries));
    }
}
