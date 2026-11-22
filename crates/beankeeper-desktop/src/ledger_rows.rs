//! Typed reads from the currently pinned Beankeeper row API.

use std::{collections::HashSet, error::Error, fmt};

use beankeeper::types::{AccountType, Amount, ClearanceStatus, Currency, DebitOrCredit, Money};
use beankeeper_cli::db::{
    AccountRow, AuditLogRow, EntryRow,
    audit::{AuditEntity, EntryStatus},
    ids::{EntryId, TransactionId},
};

#[derive(Debug)]
pub(crate) struct RowError(beankeeper_cli::error::CliError);

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid ledger audit record ID: {}", self.0)
    }
}

impl Error for RowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

fn account_kind_order(kind: AccountType) -> u8 {
    match kind {
        AccountType::Asset => 0,
        AccountType::Liability => 1,
        AccountType::Equity => 2,
        AccountType::Revenue => 3,
        AccountType::Expense => 4,
    }
}

pub(crate) struct AccountGroup<'a> {
    pub(crate) kind: AccountType,
    pub(crate) rows: Vec<&'a AccountRow>,
}

pub(crate) fn accounts_by_kind(rows: &[AccountRow]) -> Vec<AccountGroup<'_>> {
    let mut groups = Vec::<AccountGroup<'_>>::new();
    for row in rows {
        let kind = row.account_type;
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
    groups
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
    Transaction(TransactionId),
    Entry(EntryId),
}

impl AuditSubject {
    pub(crate) fn read(row: &AuditLogRow) -> Result<Option<Self>, RowError> {
        match row.entity {
            AuditEntity::Transaction => row
                .entity_id
                .parse()
                .map(Self::Transaction)
                .map(Some)
                .map_err(RowError),
            AuditEntity::Entry => row
                .entity_id
                .parse()
                .map(Self::Entry)
                .map(Some)
                .map_err(RowError),
            _ => Ok(None),
        }
    }

    pub(crate) fn matches(&self, transaction: TransactionId, entries: &HashSet<EntryId>) -> bool {
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
    fn audit_subjects_compare_the_id_in_the_correct_entity_namespace() -> Result<(), Box<dyn Error>>
    {
        let transaction = TransactionId::try_from(10)?;
        let entries = HashSet::from([EntryId::try_from(12)?, EntryId::try_from(13)?]);
        assert!(AuditSubject::Transaction(transaction).matches(transaction, &entries));
        assert!(!AuditSubject::Entry(10.try_into()?).matches(transaction, &entries));
        assert!(AuditSubject::Entry(12.try_into()?).matches(transaction, &entries));
        assert!(!AuditSubject::Transaction(12.try_into()?).matches(transaction, &entries));
        Ok(())
    }
}
