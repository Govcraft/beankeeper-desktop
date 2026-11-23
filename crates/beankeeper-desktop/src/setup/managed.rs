//! Managed access provenance and recoverable, owner-audited rotation.

use std::{io, path::Path};

use beankeeper_cli::{
    db::{
        Actor, Db,
        audit::{self, AuditAction, AuditEntity, AuditPageLimit, AuditPageOffset, ListAuditParams},
    },
    identity::{self, Admin, Granted, VerifiedAgent},
};
use mti::prelude::MagicTypeId;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::{
    private_file::{BEARER_FILE_LIMIT, PrivateFile},
    service::{lock, private_json, private_write, read_private},
};

use super::{AGENT_NAME, ManagedAccess, OwnerCredential, SetupError, issue};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Origin {
    Default,
    LegacyDefault,
    Preserved,
    Migrated,
    OwnerChoice,
}

#[derive(Deserialize, Serialize)]
struct Recorded {
    token: MagicTypeId,
    origin: Origin,
}

#[derive(Deserialize, Serialize)]
enum State {
    Current(Recorded),
    Pending {
        before: Option<Recorded>,
        old: MagicTypeId,
        after: Recorded,
    },
}

fn marker(books: &Path) -> std::path::PathBuf {
    books.with_extension("agent-access.json")
}

fn load(books: &Path) -> Result<Option<State>, SetupError> {
    match read_private(&marker(books)) {
        Ok(state) => Ok(Some(state)),
        Err(crate::service::ServiceError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(_) => Err(SetupError::Invalid(
            "cannot read private agent access state",
        )),
    }
}

fn save(books: &Path, state: &State) -> Result<(), SetupError> {
    private_json(&marker(books), state)
        .map_err(|_| SetupError::Invalid("cannot save private agent access state"))
}

pub(super) fn record(books: &Path, token: MagicTypeId, origin: Origin) -> Result<(), SetupError> {
    save(books, &State::Current(Recorded { token, origin }))
}

pub(crate) fn migrated(books: &Path, agent: &VerifiedAgent) -> bool {
    matches!(
        load(books),
        Ok(Some(State::Current(Recorded { token, origin: Origin::Migrated })))
            if token.as_str() == agent.jti()
    )
}

fn verify_file(db: &Db, path: &Path) -> Result<VerifiedAgent, SetupError> {
    let secret = PrivateFile::open(path)?.read_secret(BEARER_FILE_LIMIT)?;
    Ok(identity::verify(db.read(), secret.expose_secret().trim())?)
}

fn owner(db: &Db, books: &Path) -> Result<Actor, SetupError> {
    let owner = verify_file(db, &books.with_extension("owner-token"))
        .and_then(|agent| OwnerCredential::try_from(agent).map_err(Into::into))
        .map_err(|_| {
            SetupError::Invalid(
                "the private owner credential is unavailable; restore it to change managed access",
            )
        })?;
    Actor::try_from(owner)
        .map_err(beankeeper_cli::error::CliError::from)
        .map_err(Into::into)
}

fn revoke(db: &Db, actor: &Actor, token: &MagicTypeId) -> Result<(), SetupError> {
    Granted::<Admin>::with_db(db, actor, |grant| identity::revoke(grant, token.as_str()))?;
    Ok(())
}

fn recover(
    db: &Db,
    books: &Path,
    active: &VerifiedAgent,
    state: Option<State>,
) -> Result<Option<Recorded>, SetupError> {
    let (before, old, after) = match state {
        None => return Ok(None),
        Some(State::Current(record)) => return Ok(Some(record)),
        Some(State::Pending { before, old, after }) => (before, old, after),
    };
    let actor = owner(db, books)?;
    if active.jti() == after.token.as_str() {
        revoke(db, &actor, &old)?;
        record(books, after.token.clone(), after.origin)?;
        return Ok(Some(after));
    }
    if active.jti() != old.as_str() {
        return Err(SetupError::Invalid(
            "managed access changed during an unfinished rotation",
        ));
    }
    revoke(db, &actor, &after.token)?;
    match &before {
        Some(previous) => record(books, previous.token.clone(), previous.origin)?,
        None => std::fs::remove_file(marker(books))?,
    }
    Ok(before)
}

#[derive(Deserialize)]
struct RegistryEntry {
    agent_uri: String,
}

fn agent_subject(uri: &str) -> Option<MagicTypeId> {
    uri.rsplit_once('/')?.1.parse().ok()
}

fn has_desktop_agent_prefix(uri: &str) -> bool {
    agent_subject(uri).is_some_and(|subject| subject.prefix().as_str() == AGENT_NAME)
}

/// Select only the original Desktop bootstrap grant. Reissuances and grants
/// copied in from another agent are treated as owner choices.
fn legacy_default(db: &Db, active: &VerifiedAgent) -> Result<bool, SetupError> {
    if !ManagedAccess::ReadOnly.matches(active) || !has_desktop_agent_prefix(active.uri()) {
        return Ok(false);
    }
    let registry: Vec<RegistryEntry> = serde_json::from_value(identity::list(db.read())?)
        .map_err(|_| SetupError::Invalid("upstream returned an invalid agent registry"))?;
    if registry
        .iter()
        .filter(|entry| has_desktop_agent_prefix(&entry.agent_uri))
        .count()
        != 1
    {
        return Ok(false);
    }
    let bootstrap = "desktop-setup"
        .parse()
        .map_err(beankeeper_cli::error::CliError::from)?;
    let issue = ListAuditParams {
        actor: Some(&bootstrap),
        entity: Some(AuditEntity::Agent),
        action: Some(AuditAction::TokenIssue),
        agent_uri: None,
        company_slug: None,
        limit: AuditPageLimit::default(),
        offset: AuditPageOffset::default(),
    };
    Ok(audit::list_audit(db.read(), &issue)?
        .iter()
        .any(|row| row.entity_id == active.jti() && row.token_jti.is_some()))
}

#[derive(Clone, Copy)]
pub(crate) enum AccessChange {
    Migrate,
    Preserve,
    Set(ManagedAccess),
}

pub(crate) fn reconcile(db: &Db, books: &Path, change: AccessChange) -> Result<(), SetupError> {
    let _lock = lock(&books.with_extension("agent-access.lock"))
        .map_err(|_| SetupError::Invalid("another managed access operation is running"))?;
    let token_file = books.with_extension("agent-token");
    let active = verify_file(db, &token_file)?;
    let mut before = recover(db, books, &active, load(books)?)?;
    let old = token_id(&active)?;
    if before.is_none() && !matches!(change, AccessChange::Set(_)) {
        let origin = if matches!(change, AccessChange::Migrate) && legacy_default(db, &active)? {
            Origin::LegacyDefault
        } else {
            Origin::Preserved
        };
        record(books, old.clone(), origin)?;
        before = Some(Recorded {
            token: old.clone(),
            origin,
        });
    }
    let (next, origin) = match change {
        AccessChange::Migrate if matches!(&before, Some(Recorded { token, origin: Origin::LegacyDefault }) if token == &old) =>
        {
            if !books.with_extension("owner-token").try_exists()? {
                // Owners may keep this credential offline. Reads still work.
                return Ok(());
            }
            (ManagedAccess::default(), Origin::Migrated)
        }
        AccessChange::Migrate | AccessChange::Preserve => return Ok(()),
        AccessChange::Set(next) => (next, Origin::OwnerChoice),
    };
    let actor = owner(db, books)?;
    if next.matches(&active) {
        return record(books, old, origin);
    }
    let (capabilities, mode) = next.grants();
    let replacement = issue(db, &actor, AGENT_NAME, capabilities, mode)?;
    let after = Recorded {
        token: replacement.token_jti.clone(),
        origin,
    };
    let pending = State::Pending {
        before,
        old: old.clone(),
        after,
    };
    if let Err(error) = save(books, &pending) {
        revoke(db, &actor, &replacement.token_jti)?;
        return Err(error);
    }
    private_write(&token_file, replacement.token().expose_secret().as_bytes())
        .map_err(|_| SetupError::Invalid("cannot replace the private managed agent credential"))?;
    revoke(db, &actor, &old)?;
    record(books, replacement.token_jti, origin)
}

fn token_id(agent: &VerifiedAgent) -> Result<MagicTypeId, SetupError> {
    agent
        .jti()
        .parse()
        .map_err(|_| SetupError::Invalid("upstream returned an invalid credential ID"))
}
