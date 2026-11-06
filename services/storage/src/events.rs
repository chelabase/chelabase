//! NATS cache invalidation (m9): storage drops what it cached the moment the control plane
//! says it changed, instead of waiting out its cache lifetimes (which stay as the fallback, so
//! with NATS down storage keeps serving on them).
//!
//! It listens as its own NATS user (`storage`, subscribe-only) to:
//! - `chelabase.rt.project.>`: a branch's status changed (a reset or restore ends with one), it
//!   was deleted, or one of its Postgres databases was created or dropped: the branch's
//!   connection info, pools and schema checks go;
//! - `chelabase.storage.events` ([`chelabase_nats::StorageEvent`]): a data key revoked, rotated
//!   or its database list changed (its signed URLs' liveness goes), or a branch's connection
//!   budget changed (as a branch event above);
//! - `chelabase.rt.access.>`: a member's access changed (removed, suspended, a grant): every
//!   cached key liveness goes, since a console URL lives on its member's access.

use std::sync::Arc;
use std::time::Duration;

use chelabase_nats::rt::{ACCESS, PROJECT, ProjectEvent};
use chelabase_nats::{NatsConfig, SUBJECT_STORAGE_EVENTS, StorageEvent};
use chelabase_pgsession::ConnSource;
use futures_util::StreamExt;
use uuid::Uuid;

use crate::app::AppState;

/// What an event makes storage drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drop {
    /// The branch's connection info, pools and schema checks.
    Branch(Uuid),
    /// One key's cached liveness.
    Key(Uuid),
    /// Every key's cached liveness.
    AllKeys,
}

/// What a message on `subject` makes storage drop (`None`: nothing, or unreadable).
pub fn classify(subject: &str, payload: &[u8]) -> Option<Drop> {
    if subject == SUBJECT_STORAGE_EVENTS {
        return match serde_json::from_slice(payload).ok()? {
            StorageEvent::Key { key_id } => Some(Drop::Key(key_id)),
            StorageEvent::Branch { branch_id } => Some(Drop::Branch(branch_id)),
        };
    }
    if subject.starts_with(&format!("{ACCESS}.")) {
        return Some(Drop::AllKeys);
    }
    if subject.starts_with(&format!("{PROJECT}.")) {
        return match serde_json::from_slice(payload).ok()? {
            ProjectEvent::BranchStatus { branch_id, .. }
            | ProjectEvent::BranchDeleted { branch_id, .. }
            | ProjectEvent::PgDatabaseCreated { branch_id, .. }
            | ProjectEvent::PgDatabaseDropped { branch_id, .. } => Some(Drop::Branch(branch_id)),
            ProjectEvent::BranchCreated { .. }
            | ProjectEvent::DatabaseCreated { .. }
            | ProjectEvent::DatabaseDeleted { .. } => None,
        };
    }
    None
}

/// Drops what `drop` names.
pub async fn apply<S: ConnSource, M: ConnSource>(state: &AppState<S, M>, drop: Drop) {
    match drop {
        Drop::Branch(branch) => state.engine.invalidate(branch).await,
        Drop::Key(key_id) => state.urls.forget_key(key_id),
        Drop::AllKeys => state.urls.forget_all(),
    }
}

/// How long to wait before connecting again after NATS refused or couldn't be reached.
const RETRY: Duration = Duration::from_secs(5);

/// Listens in the background for as long as the process runs: connects (again every
/// [`RETRY`] until NATS answers; meanwhile the cache lifetimes apply), subscribes and applies
/// each event. The client reconnects on its own once connected.
pub fn spawn<S: ConnSource, M: ConnSource>(state: Arc<AppState<S, M>>, config: NatsConfig) {
    tokio::spawn(async move {
        loop {
            match listen(&state, &config).await {
                Ok(()) => tracing::warn!("the NATS subscriptions ended; subscribing again"),
                Err(e) => {
                    tracing::warn!("NATS cache events unavailable (the cache lifetimes apply): {e}")
                }
            }
            tokio::time::sleep(RETRY).await;
        }
    });
}

async fn listen<S: ConnSource, M: ConnSource>(
    state: &AppState<S, M>,
    config: &NatsConfig,
) -> anyhow::Result<()> {
    let client = chelabase_nats::connect(config).await?;
    let subjects = [
        format!("{PROJECT}.>"),
        format!("{ACCESS}.>"),
        SUBJECT_STORAGE_EVENTS.to_string(),
    ];
    let mut subscriptions = Vec::new();
    for subject in subjects {
        subscriptions.push(client.subscribe(subject).await?);
    }
    tracing::info!("listening for NATS cache events");
    let mut messages = futures_util::stream::select_all(subscriptions);
    while let Some(message) = messages.next().await {
        if let Some(drop) = classify(message.subject.as_str(), &message.payload) {
            tracing::debug!(?drop, "NATS cache event");
            apply(state, drop).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Each subject and event maps to what it drops; anything else drops nothing.
    #[test]
    fn events_map_to_what_they_drop() {
        let (b, d, k) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let project = format!("{PROJECT}.{}", Uuid::new_v4());
        let ev = |v: serde_json::Value| serde_json::to_vec(&v).unwrap();
        for (subject, payload, want) in [
            (
                project.clone(),
                ev(
                    json!({ "event": "branch.status", "database_id": d, "branch_id": b, "status": "ready" }),
                ),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(json!({ "event": "branch.deleted", "database_id": d, "branch_id": b })),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(
                    json!({ "event": "pg_database.dropped", "database_id": d, "branch_id": b, "name": "app" }),
                ),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(
                    json!({ "event": "pg_database.created", "database_id": d, "branch_id": b, "name": "app" }),
                ),
                Some(Drop::Branch(b)),
            ),
            (
                project.clone(),
                ev(json!({ "event": "branch.created", "database_id": d, "branch_id": b })),
                None,
            ),
            (
                project.clone(),
                ev(json!({ "event": "database.deleted", "database_id": d })),
                None,
            ),
            (project.clone(), b"not json".to_vec(), None),
            (
                SUBJECT_STORAGE_EVENTS.to_string(),
                ev(json!({ "event": "key", "key_id": k })),
                Some(Drop::Key(k)),
            ),
            (
                SUBJECT_STORAGE_EVENTS.to_string(),
                ev(json!({ "event": "branch", "branch_id": b })),
                Some(Drop::Branch(b)),
            ),
            (
                SUBJECT_STORAGE_EVENTS.to_string(),
                ev(json!({ "event": "other" })),
                None,
            ),
            (
                format!("{ACCESS}.{}", Uuid::new_v4()),
                ev(json!({ "tenant_id": d, "member_sub": null })),
                Some(Drop::AllKeys),
            ),
            ("chelabase.rt.rows.x".to_string(), ev(json!({})), None),
        ] {
            assert_eq!(
                classify(&subject, &payload),
                want,
                "{subject} {}",
                String::from_utf8_lossy(&payload)
            );
        }
    }
}
