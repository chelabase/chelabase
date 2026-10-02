//! `PATCH /compute/api/v2/computes/{id}/ddl`: the DDL receiver (spec 2026-10-01-login-roles,
//! Part 2). Each compute forwards every transaction that touches roles or databases here at
//! pre-commit (Neon's `neon_ddl_handler.c`), with its own token. A 200 lets the transaction
//! commit; a 4xx with a one-line `text/plain` body fails it with that text. The compute's curl
//! gives up after 3 s and retries, so the receiver is idempotent and answers "try again"
//! (503) when it can't take the endpoint's lock quickly.
//!
//! A refusal never echoes a password: the texts name roles, databases, keys and settings only.

use super::spec::authorize_compute;
use super::{InternalState, NeonMetadataPort};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cheladb_metadata::{MetadataError, RoleCopy, RoleOrigin};
use cheladb_neon::ComputeLocator;
use cheladb_neon::proxy_cache::ProxyCache;
use cheladb_secrets::{SecretKey, open_role_password, seal_role_password};
use cheladb_types::BranchId;
use cheladb_types::role_name::is_reserved_for;
use cheladb_types::role_password::check_forwarded_password;
use cheladb_types::role_settings::RoleSettings;
use serde::Deserialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

/// A boxed, sendable future (for [`CachePort`], which is used as a trait object).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The proxies' role-secret cache. [`ProxyCache`] in production, a fake in tests.
pub trait CachePort: Send + Sync + 'static {
    /// Tells every proxy to drop its cached secret for `role` on `endpoint_id`.
    fn password_updated<'a>(
        &'a self,
        endpoint_id: &'a str,
        role: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<u64>>;
}

impl CachePort for ProxyCache {
    fn password_updated<'a>(
        &'a self,
        endpoint_id: &'a str,
        role: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<u64>> {
        Box::pin(ProxyCache::password_updated(self, endpoint_id, role))
    }
}

/// The receiver's transaction: [`cheladb_metadata::DdlTx`]'s methods, so tests can use a
/// fake. Dropping it without [`DdlPort::commit`] rolls everything back.
pub trait DdlPort: Send + Sized + 'static {
    fn branch_id(&self) -> Option<BranchId>;
    fn owner(&self) -> &str;
    fn main_database(&self) -> Option<&str>;
    fn role(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<Option<RoleCopy>, MetadataError>> + Send;
    fn copy_count(&mut self) -> impl Future<Output = Result<u32, MetadataError>> + Send;
    fn live_secret_keys(
        &mut self,
        role: &str,
    ) -> impl Future<Output = Result<Vec<(Uuid, String)>, MetadataError>> + Send;
    fn upsert_sql_copy(
        &mut self,
        name: &str,
        verifier: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn set_owner_verifier(
        &mut self,
        verifier: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn remove_copy(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn rename_copy(
        &mut self,
        old: &str,
        new: &str,
        resealed: Option<&str>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn has_database(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn database_count(&mut self) -> impl Future<Output = Result<u32, MetadataError>> + Send;
    fn upsert_database(
        &mut self,
        name: &str,
        owner: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn rename_database(
        &mut self,
        old: &str,
        new: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn remove_database(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn commit(self) -> impl Future<Output = Result<(), MetadataError>> + Send;
}

// Each method calls `DdlTx`'s inherent method by its full path, so a rename there can't make
// it call itself.
impl DdlPort for cheladb_metadata::DdlTx {
    fn branch_id(&self) -> Option<BranchId> {
        cheladb_metadata::DdlTx::branch_id(self)
    }
    fn owner(&self) -> &str {
        cheladb_metadata::DdlTx::owner(self)
    }
    fn main_database(&self) -> Option<&str> {
        cheladb_metadata::DdlTx::main_database(self)
    }
    async fn role(&mut self, name: &str) -> Result<Option<RoleCopy>, MetadataError> {
        cheladb_metadata::DdlTx::role(self, name).await
    }
    async fn copy_count(&mut self) -> Result<u32, MetadataError> {
        cheladb_metadata::DdlTx::copy_count(self).await
    }
    async fn live_secret_keys(&mut self, role: &str) -> Result<Vec<(Uuid, String)>, MetadataError> {
        cheladb_metadata::DdlTx::live_secret_keys(self, role).await
    }
    async fn upsert_sql_copy(&mut self, name: &str, verifier: &str) -> Result<(), MetadataError> {
        cheladb_metadata::DdlTx::upsert_sql_copy(self, name, verifier).await
    }
    async fn set_owner_verifier(&mut self, verifier: &str) -> Result<(), MetadataError> {
        cheladb_metadata::DdlTx::set_owner_verifier(self, verifier).await
    }
    async fn remove_copy(&mut self, name: &str) -> Result<bool, MetadataError> {
        cheladb_metadata::DdlTx::remove_copy(self, name).await
    }
    async fn rename_copy(
        &mut self,
        old: &str,
        new: &str,
        resealed: Option<&str>,
    ) -> Result<bool, MetadataError> {
        cheladb_metadata::DdlTx::rename_copy(self, old, new, resealed).await
    }
    async fn has_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        cheladb_metadata::DdlTx::has_database(self, name).await
    }
    async fn database_count(&mut self) -> Result<u32, MetadataError> {
        cheladb_metadata::DdlTx::database_count(self).await
    }
    async fn upsert_database(&mut self, name: &str, owner: &str) -> Result<(), MetadataError> {
        cheladb_metadata::DdlTx::upsert_database(self, name, owner).await
    }
    async fn rename_database(&mut self, old: &str, new: &str) -> Result<bool, MetadataError> {
        cheladb_metadata::DdlTx::rename_database(self, old, new).await
    }
    async fn remove_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        cheladb_metadata::DdlTx::remove_database(self, name).await
    }
    async fn commit(self) -> Result<(), MetadataError> {
        cheladb_metadata::DdlTx::commit(self).await
    }
}

// ---- The payload (live-observed shapes: plan Status, "Pre-flight scan → Live") ----

/// One transaction's changes. `{"roles":[]}` and `{}` are empty deltas.
#[derive(Deserialize)]
struct Delta {
    #[serde(default)]
    roles: Vec<RoleOp>,
    #[serde(default)]
    dbs: Vec<DbOp>,
}

/// No `Debug`: it carries the password.
#[derive(Deserialize)]
struct RoleOp {
    op: Op,
    name: String,
    old_name: Option<String>,
    password: Option<String>,
    encrypted_password: Option<String>,
}

#[derive(Deserialize)]
struct DbOp {
    op: Op,
    name: String,
    owner: Option<String>,
    old_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Op {
    Set,
    Del,
}

/// A forwarded password: what was typed and what Postgres stored.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Password<'a> {
    plain: &'a str,
    encrypted: &'a str,
}

impl std::fmt::Debug for Password<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// One change, in the order it's applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step<'a> {
    RenameRole {
        old: &'a str,
        new: &'a str,
    },
    /// `password: None` is a group role or `PASSWORD NULL`.
    SetRole {
        name: &'a str,
        password: Option<Password<'a>>,
    },
    DelRole {
        name: &'a str,
    },
    RenameDb {
        old: &'a str,
        new: &'a str,
    },
    SetDb {
        name: &'a str,
        owner: &'a str,
    },
    DelDb {
        name: &'a str,
    },
}

/// Neon's order: role renames, role sets, role deletes, then databases the same way.
///
/// An entry with `old_name` is a rename; it is also a set only when it carries a password
/// (a plain rename has none and must not remove the copy), and a delete when its op is `del`
/// (renamed, then dropped, in one transaction). A database `set` without an owner (a rename
/// of a database created elsewhere) has nothing more to record.
fn steps(delta: &Delta) -> Vec<Step<'_>> {
    let mut out = Vec::new();
    for r in &delta.roles {
        if let Some(old) = r.old_name.as_deref() {
            out.push(Step::RenameRole { old, new: &r.name });
        }
    }
    for r in &delta.roles {
        if r.op != Op::Set {
            continue;
        }
        let password = r.password.as_deref().map(|plain| Password {
            plain,
            // Neon sends both or neither; a missing hash fails the SCRAM check.
            encrypted: r.encrypted_password.as_deref().unwrap_or(""),
        });
        if r.old_name.is_none() || password.is_some() {
            out.push(Step::SetRole {
                name: &r.name,
                password,
            });
        }
    }
    for r in &delta.roles {
        if r.op == Op::Del {
            out.push(Step::DelRole { name: &r.name });
        }
    }
    for d in &delta.dbs {
        if let Some(old) = d.old_name.as_deref() {
            out.push(Step::RenameDb { old, new: &d.name });
        }
    }
    for d in &delta.dbs {
        if let (Op::Set, Some(owner)) = (d.op, d.owner.as_deref()) {
            out.push(Step::SetDb {
                name: &d.name,
                owner,
            });
        }
    }
    for d in &delta.dbs {
        if d.op == Op::Del {
            out.push(Step::DelDb { name: &d.name });
        }
    }
    out
}

/// What the transaction knows that a step's decision needs.
#[derive(Debug, Default)]
struct Facts<'a> {
    owner: &'a str,
    main_database: Option<&'a str>,
    /// The row the step names (the old name for a rename).
    row: Option<RowFacts>,
    /// Copies the role cap counts (R5).
    copy_count: u32,
    /// Names of the live secret data keys using the role.
    live_keys: Vec<String>,
    /// The database the step names has a row.
    db_exists: bool,
    /// Databases the cap counts (R23).
    database_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowFacts {
    origin: RoleOrigin,
    has_verifier: bool,
    sealed: bool,
}

impl RowFacts {
    fn of(row: &RoleCopy) -> Self {
        Self {
            origin: row.origin,
            has_verifier: row.scram_verifier.is_some(),
            sealed: row.password_enc.is_some(),
        }
    }
}

/// What to write for one step.
#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    Nothing,
    SetOwnerVerifier(&'a str),
    UpsertCopy(&'a str),
    RemoveCopy(&'a str),
    /// `reseal`: the row's sealed password (an `api` row's, or an `sql` row's made revealable
    /// by an API reset) is opened under the old name and sealed under the new one.
    RenameCopy {
        old: &'a str,
        new: &'a str,
        reseal: bool,
    },
    UpsertDatabase {
        name: &'a str,
        owner: &'a str,
    },
    RenameDatabase {
        old: &'a str,
        new: &'a str,
    },
    /// `main`: the branch's main database (Q1: accepted, logged).
    RemoveDatabase {
        name: &'a str,
        main: bool,
    },
}

/// The decision for one step against the rows read so far. `Err` is the refusal's text.
fn decide<'a>(
    step: &Step<'a>,
    facts: &Facts<'_>,
    rules: &RoleSettings,
) -> Result<Action<'a>, String> {
    let owner = facts.owner;
    let reserved = |name: &str| {
        if is_reserved_for(name, owner) {
            Err(text::reserved(name))
        } else {
            Ok(())
        }
    };
    match *step {
        Step::RenameRole { old, new } => {
            if old == owner || new == owner {
                return Err(text::owner(owner));
            }
            reserved(old)?;
            reserved(new)?;
            Ok(match facts.row {
                None => Action::Nothing,
                Some(row) => Action::RenameCopy {
                    old,
                    new,
                    reseal: row.sealed,
                },
            })
        }
        Step::SetRole {
            name,
            password: Some(pw),
        } => {
            if name == owner {
                check_forwarded_password(pw.plain, pw.encrypted, rules)
                    .map_err(|r| r.message().to_string())?;
                return Ok(Action::SetOwnerVerifier(pw.encrypted));
            }
            reserved(name)?;
            check_forwarded_password(pw.plain, pw.encrypted, rules)
                .map_err(|r| r.message().to_string())?;
            // An MD5 row (no verifier) isn't counted yet; giving it one adds a copy.
            let new_copy = !facts.row.is_some_and(|r| r.has_verifier);
            if new_copy && facts.copy_count >= rules.role_cap {
                return Err(text::role_cap(rules.role_cap));
            }
            Ok(Action::UpsertCopy(pw.encrypted))
        }
        Step::SetRole {
            name,
            password: None,
        } => {
            if name == owner {
                return Err(text::OWNER_PASSWORD.to_string());
            }
            reserved(name)?;
            Ok(match facts.row {
                Some(_) => Action::RemoveCopy(name),
                None => Action::Nothing,
            })
        }
        Step::DelRole { name } => {
            if name == owner {
                return Err(text::owner(owner));
            }
            reserved(name)?;
            if !facts.live_keys.is_empty() {
                return Err(text::in_use(name, &facts.live_keys));
            }
            Ok(Action::RemoveCopy(name))
        }
        Step::RenameDb { old, new } => {
            if facts.main_database == Some(old) {
                return Err(text::main_database(old));
            }
            Ok(Action::RenameDatabase { old, new })
        }
        Step::SetDb { name, owner } => {
            if !facts.db_exists && facts.database_count >= rules.database_cap {
                return Err(text::database_cap(rules.database_cap));
            }
            Ok(Action::UpsertDatabase { name, owner })
        }
        Step::DelDb { name } => Ok(Action::RemoveDatabase {
            name,
            main: facts.main_database == Some(name),
        }),
    }
}

/// The answers' texts: one line of `text/plain`, at most [`text::MAX`] bytes.
mod text {
    pub const MAX: usize = 200;
    pub const TOKEN_REFUSED: &str = "compute token refused";
    pub const MALFORMED: &str = "malformed request from the compute";
    pub const UNKNOWN_ENDPOINT: &str = "unknown endpoint";
    pub const BUSY: &str = "role changes are busy; try again";
    pub const UNAVAILABLE: &str = "role changes are unavailable right now; try again";
    pub const OWNER_PASSWORD: &str =
        "the branch owner's password can't be removed; reset it instead";

    /// Control characters (a role can be named `"a\nb"`) become `?`, so the body stays one
    /// line.
    pub fn clean(name: &str) -> String {
        name.chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect()
    }

    /// Cut to `MAX` bytes on a character boundary.
    pub fn fit(mut s: String) -> String {
        if s.len() > MAX {
            let mut end = MAX;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
        }
        s
    }

    pub fn reserved(name: &str) -> String {
        fit(format!(
            "the role name \"{}\" is reserved by Cheladb",
            clean(name)
        ))
    }

    pub fn owner(name: &str) -> String {
        fit(format!(
            "the branch owner \"{}\" can't be dropped or renamed",
            clean(name)
        ))
    }

    pub fn role_cap(cap: u32) -> String {
        format!("this branch has reached its limit of {cap} login roles (CHELADB_BRANCH_ROLE_CAP)")
    }

    pub fn database_cap(cap: u32) -> String {
        format!(
            "this branch has reached its limit of {cap} databases (CHELADB_BRANCH_DATABASE_CAP)"
        )
    }

    pub fn main_database(name: &str) -> String {
        fit(format!(
            "the branch's main database \"{}\" can't be renamed",
            clean(name)
        ))
    }

    /// Names as many keys as fit in `MAX` bytes; the rest become `…`.
    pub fn in_use(role: &str, keys: &[String]) -> String {
        let head = format!("role \"{}\" backs data API key(s) ", clean(role));
        let tail = "; revoke them first";
        let more = ", …";
        let mut list = String::new();
        for (i, key) in keys.iter().enumerate() {
            let piece = if i == 0 {
                clean(key)
            } else {
                format!(", {}", clean(key))
            };
            let rest = if i + 1 < keys.len() { more.len() } else { 0 };
            if head.len() + list.len() + piece.len() + rest + tail.len() > MAX {
                if list.is_empty() {
                    list.push('…');
                } else {
                    list.push_str(more);
                }
                break;
            }
            list.push_str(&piece);
        }
        fit(format!("{head}{list}{tail}"))
    }
}

fn answer(status: StatusCode, body: impl Into<String>) -> Response {
    (status, body.into()).into_response()
}

/// A metadata error's answer: a lock timeout is "busy", anything else "unavailable".
fn metadata_failure(endpoint: &str, e: &MetadataError) -> Response {
    if let MetadataError::EndpointLockTimeout(_) = e {
        warn!(endpoint, "ddl: the endpoint's lock is busy");
        return answer(StatusCode::SERVICE_UNAVAILABLE, text::BUSY);
    }
    warn!(endpoint, "ddl: {e:#}");
    answer(StatusCode::SERVICE_UNAVAILABLE, text::UNAVAILABLE)
}

pub async fn receive<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // The token first (the spec route's path and timing), so a caller without it learns
    // nothing about the body format.
    if let Err(refused) = authorize_compute(&state, &id, &headers).await {
        return match refused.status() {
            StatusCode::UNAUTHORIZED => answer(StatusCode::UNAUTHORIZED, text::TOKEN_REFUSED),
            _ => answer(StatusCode::SERVICE_UNAVAILABLE, text::UNAVAILABLE),
        };
    }
    // The parse error isn't logged or returned: it can quote a value from the body.
    let Ok(delta) = serde_json::from_slice::<Delta>(&body) else {
        warn!(endpoint = %id, "ddl: malformed request body");
        return answer(StatusCode::BAD_REQUEST, text::MALFORMED);
    };
    // `ALTER ROLE … LOGIN`, `GRANT` and the like send `{"roles":[]}` (R4).
    if delta.roles.is_empty() && delta.dbs.is_empty() {
        return StatusCode::OK.into_response();
    }
    let tx = match state.metadata.begin_ddl(&id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return answer(StatusCode::NOT_FOUND, text::UNKNOWN_ENDPOINT),
        Err(e) => return metadata_failure(&id, &e),
    };
    let changed = match apply(tx, &steps(&delta), &state.roles, &state.secrets, &id).await {
        Ok(changed) => changed,
        Err(Failure::Refused(message)) => return answer(StatusCode::CONFLICT, message),
        Err(Failure::Metadata(e)) => return metadata_failure(&id, &e),
    };
    publish(state.cache.clone(), id, changed);
    StatusCode::OK.into_response()
}

enum Failure {
    Refused(String),
    Metadata(MetadataError),
}

impl From<MetadataError> for Failure {
    fn from(e: MetadataError) -> Self {
        Failure::Metadata(e)
    }
}

/// Applies every step in one transaction and commits; the first refusal drops the
/// transaction (rolling back what came before). Returns the role names whose secrets changed.
async fn apply<T: DdlPort>(
    mut tx: T,
    steps: &[Step<'_>],
    rules: &RoleSettings,
    key: &SecretKey,
    endpoint: &str,
) -> Result<Vec<String>, Failure> {
    let owner = tx.owner().to_string();
    let main_database = tx.main_database().map(str::to_string);
    let branch = tx.branch_id();
    let mut changed: Vec<String> = Vec::new();
    let mut touch = |name: &str| {
        if !changed.iter().any(|c| c == name) {
            changed.push(name.to_string());
        }
    };
    let tag = Uuid::new_v4().simple().to_string();
    let mut role_moves: Vec<Move> = Vec::new();
    let mut db_moves: Vec<Move> = Vec::new();
    for step in steps {
        if !matches!(step, Step::RenameRole { .. }) {
            finish_role_moves(&mut tx, &mut role_moves).await?;
        }
        if !matches!(step, Step::RenameDb { .. }) {
            finish_db_moves(&mut tx, &mut db_moves).await?;
        }
        let is_db = matches!(
            step,
            Step::RenameDb { .. } | Step::SetDb { .. } | Step::DelDb { .. }
        );
        if is_db && branch.is_none() {
            // A branch-less endpoint has no `branch_databases` to mirror into.
            continue;
        }
        let mut facts = Facts {
            owner: &owner,
            main_database: main_database.as_deref(),
            ..Facts::default()
        };
        match *step {
            Step::RenameRole { old: name, .. } | Step::SetRole { name, .. } => {
                facts.row = tx.role(name).await?.as_ref().map(RowFacts::of);
                if matches!(
                    step,
                    Step::SetRole {
                        password: Some(_),
                        ..
                    }
                ) {
                    facts.copy_count = tx.copy_count().await?;
                }
            }
            Step::DelRole { name } => {
                facts.live_keys = tx
                    .live_secret_keys(name)
                    .await?
                    .into_iter()
                    .map(|(_, key_name)| key_name)
                    .collect();
            }
            Step::SetDb { name, .. } => {
                facts.db_exists = tx.has_database(name).await?;
                facts.database_count = tx.database_count().await?;
            }
            Step::RenameDb { .. } | Step::DelDb { .. } => {}
        }
        let action = decide(step, &facts, rules).map_err(Failure::Refused)?;
        // An accepted rename frees its new name of any stale row, even when its old name
        // has none.
        match (step, &action) {
            (Step::RenameRole { new, .. }, Action::Nothing) => {
                role_moves.push(Move {
                    aside: None,
                    new: new.to_string(),
                    resealed: None,
                });
                touch(new);
            }
            (Step::RenameDb { new, .. }, _) => {
                let aside = aside_name(&tag, db_moves.len());
                let moved = tx.rename_database(step_old(step), &aside).await?;
                db_moves.push(Move {
                    aside: moved.then_some(aside),
                    new: new.to_string(),
                    resealed: None,
                });
                continue;
            }
            _ => {}
        }
        match action {
            Action::Nothing => {}
            Action::SetOwnerVerifier(verifier) => {
                tx.set_owner_verifier(verifier).await?;
                touch(&owner);
            }
            Action::UpsertCopy(verifier) => {
                let Step::SetRole { name, .. } = *step else {
                    unreachable!("an upsert comes from a set")
                };
                tx.upsert_sql_copy(name, verifier).await?;
                touch(name);
            }
            Action::RemoveCopy(name) => {
                tx.remove_copy(name).await?;
                touch(name);
            }
            Action::RenameCopy { old, new, reseal } => {
                let resealed = if reseal {
                    reseal_password(&mut tx, key, branch, old, new, endpoint).await?
                } else {
                    None
                };
                let aside = aside_name(&tag, role_moves.len());
                tx.rename_copy(old, &aside, resealed.as_deref()).await?;
                role_moves.push(Move {
                    aside: Some(aside),
                    new: new.to_string(),
                    resealed,
                });
                touch(old);
                touch(new);
            }
            Action::UpsertDatabase { name, owner } => tx.upsert_database(name, owner).await?,
            Action::RenameDatabase { .. } => unreachable!("moved aside above"),
            Action::RemoveDatabase { name, main } => {
                if main {
                    warn!(
                        endpoint,
                        database = %text::clean(name),
                        "ddl: the branch's main database was dropped"
                    );
                }
                tx.remove_database(name).await?;
            }
        }
    }
    finish_role_moves(&mut tx, &mut role_moves).await?;
    finish_db_moves(&mut tx, &mut db_moves).await?;
    tx.commit().await?;
    Ok(changed)
}

/// A rename's second hop (M2). Renames move in two hops: each row first goes aside under a
/// temporary name, and only once every rename of the transaction has left its old name does
/// it take its new one. A row still under a new name by then is stale (its role was dropped
/// in the same transaction, which Neon sends as the rename alone, or a leftover) and is
/// removed. So a swap of two names, which Neon sends as two renames onto each other's names,
/// moves both rows instead of hitting the unique key.
struct Move {
    /// Where the row waits; `None` when the old name had no row.
    aside: Option<String>,
    new: String,
    /// The sealed password for the new name (`rename_copy` sets it on each hop).
    resealed: Option<String>,
}

/// The `i`-th rename's temporary name in one request: a control character, a per-request
/// random tag (so no existing or forwarded name can be one) and the index; 52 bytes at most,
/// within the 63-byte limit.
fn aside_name(tag: &str, i: usize) -> String {
    format!("\u{1}cheladb rename {tag} {i}")
}

/// The old name of a rename step.
fn step_old<'a>(step: &Step<'a>) -> &'a str {
    match *step {
        Step::RenameRole { old, .. } | Step::RenameDb { old, .. } => old,
        _ => unreachable!("only renames move"),
    }
}

/// A new name that still backs live secret data keys once every renamed row (and its keys)
/// is aside means a role of that name was dropped in this transaction: Neon sends
/// `DROP ROLE a; ALTER ROLE b RENAME TO a` as the rename alone. It's refused as that
/// `DROP ROLE` would be, so the keys never pass to the renamed role.
async fn finish_role_moves<T: DdlPort>(tx: &mut T, moves: &mut Vec<Move>) -> Result<(), Failure> {
    for m in moves.iter() {
        let keys: Vec<String> = tx
            .live_secret_keys(&m.new)
            .await?
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        if !keys.is_empty() {
            return Err(Failure::Refused(text::in_use(&m.new, &keys)));
        }
    }
    for m in moves.drain(..) {
        tx.remove_copy(&m.new).await?;
        if let Some(aside) = m.aside {
            tx.rename_copy(&aside, &m.new, m.resealed.as_deref())
                .await?;
        }
    }
    Ok(())
}

async fn finish_db_moves<T: DdlPort>(
    tx: &mut T,
    moves: &mut Vec<Move>,
) -> Result<(), MetadataError> {
    for m in moves.drain(..) {
        tx.remove_database(&m.new).await?;
        if let Some(aside) = m.aside {
            tx.rename_database(&aside, &m.new).await?;
        }
    }
    Ok(())
}

/// The `api` row's sealed password, opened under `old`'s AAD and sealed under `new`'s. A
/// value that won't open (or no branch to bind it to) is dropped with a warning: the copy
/// still works, and the reveal then reports the password as set in SQL.
async fn reseal_password<T: DdlPort>(
    tx: &mut T,
    key: &SecretKey,
    branch: Option<BranchId>,
    old: &str,
    new: &str,
    endpoint: &str,
) -> Result<Option<String>, MetadataError> {
    let sealed = tx.role(old).await?.and_then(|r| r.password_enc);
    let (Some(sealed), Some(branch)) = (sealed, branch) else {
        return Ok(None);
    };
    let branch = branch.to_string();
    match open_role_password(key, &branch, old, &sealed) {
        Ok(password) => Ok(Some(seal_role_password(key, &branch, new, &password))),
        Err(e) => {
            warn!(
                endpoint,
                "ddl: a renamed role's sealed password won't open ({e}); dropped"
            );
            Ok(None)
        }
    }
}

/// `password_updated` for each changed role, at once and again about 1 s later (R22): the
/// receiver answers before the user's commit is visible. Failures are logged (R12); the
/// answer doesn't wait (a slow Valkey must not outlast the compute's 3 s curl timeout).
fn publish(cache: Arc<dyn CachePort>, endpoint: String, roles: Vec<String>) {
    if roles.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for round in 0..2 {
            if round == 1 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            for role in &roles {
                if let Err(e) = cache.password_updated(&endpoint, role).await {
                    warn!(
                        endpoint = %endpoint,
                        role = %text::clean(role),
                        "ddl: proxy cache not told: {e:#}"
                    );
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_types::mode::Mode;

    const SCRAM: &str = "SCRAM-SHA-256$4096:c2FsdA==$a2V5:c2VydmVy";

    fn rules(mode: Mode, strength: bool) -> RoleSettings {
        RoleSettings::from_lookup(mode, |k| {
            (k == "CHELADB_PASSWORD_STRENGTH").then(|| if strength { "on" } else { "off" }.into())
        })
        .unwrap()
    }

    fn delta(json: &str) -> Delta {
        serde_json::from_str(json).unwrap()
    }

    fn facts() -> Facts<'static> {
        Facts {
            owner: "postgres",
            main_database: Some("neondb"),
            ..Facts::default()
        }
    }

    fn row(origin: RoleOrigin, has_verifier: bool, sealed: bool) -> Option<RowFacts> {
        Some(RowFacts {
            origin,
            has_verifier,
            sealed,
        })
    }

    const PW: Password<'static> = Password {
        plain: "Tr0ub4dor&3x-horse",
        encrypted: SCRAM,
    };

    #[test]
    fn steps_follow_neons_order() {
        let d = delta(
            r#"{
            "dbs": [
                {"op":"del","name":"gone"},
                {"op":"set","name":"made","owner":"postgres"},
                {"op":"set","name":"new_db","old_name":"old_db"}
            ],
            "roles": [
                {"op":"del","name":"ghost"},
                {"op":"set","name":"app","password":"p","encrypted_password":"SCRAM-SHA-256$x"},
                {"op":"set","name":"b","old_name":"a"},
                {"op":"set","name":"d","old_name":"c","password":"q","encrypted_password":"SCRAM-SHA-256$y"},
                {"op":"set","name":"grp"},
                {"op":"del","name":"f","old_name":"e"}
            ]
        }"#,
        );
        let p = Password {
            plain: "p",
            encrypted: "SCRAM-SHA-256$x",
        };
        let q = Password {
            plain: "q",
            encrypted: "SCRAM-SHA-256$y",
        };
        assert_eq!(
            steps(&d),
            vec![
                Step::RenameRole { old: "a", new: "b" },
                Step::RenameRole { old: "c", new: "d" },
                Step::RenameRole { old: "e", new: "f" },
                Step::SetRole {
                    name: "app",
                    password: Some(p)
                },
                // A plain rename (`b`) is not also a password removal.
                Step::SetRole {
                    name: "d",
                    password: Some(q)
                },
                Step::SetRole {
                    name: "grp",
                    password: None
                },
                Step::DelRole { name: "ghost" },
                Step::DelRole { name: "f" },
                Step::RenameDb {
                    old: "old_db",
                    new: "new_db"
                },
                Step::SetDb {
                    name: "made",
                    owner: "postgres"
                },
                Step::DelDb { name: "gone" },
            ]
        );
    }

    #[test]
    fn a_password_without_its_hash_fails_the_scram_check() {
        let d = delta(r#"{"roles":[{"op":"set","name":"x","password":"Tr0ub4dor&3x-horse"}]}"#);
        let s = steps(&d);
        let got = decide(&s[0], &facts(), &rules(Mode::SelfHosted, false));
        assert_eq!(
            got,
            Err("this password format can't be used through Cheladb; set a plain password".into())
        );
    }

    #[test]
    fn decide_table() {
        let sh = rules(Mode::SelfHosted, false);
        let mut capped = sh;
        capped.role_cap = 1;
        capped.database_cap = 1;
        let owner_text = "the branch owner \"postgres\" can't be dropped or renamed";
        type Case = (
            &'static str,
            Step<'static>,
            Facts<'static>,
            RoleSettings,
            Result<Action<'static>, String>,
        );
        let cases: Vec<Case> = vec![
            (
                "new copy",
                Step::SetRole { name: "app", password: Some(PW) },
                facts(),
                sh,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "owner re-password",
                Step::SetRole { name: "postgres", password: Some(PW) },
                facts(),
                sh,
                Ok(Action::SetOwnerVerifier(SCRAM)),
            ),
            (
                "reserved",
                Step::SetRole { name: "neon_superuser", password: Some(PW) },
                facts(),
                sh,
                Err("the role name \"neon_superuser\" is reserved by Cheladb".into()),
            ),
            (
                "reserved group role",
                Step::SetRole { name: "cloud_admin", password: None },
                facts(),
                sh,
                Err("the role name \"cloud_admin\" is reserved by Cheladb".into()),
            ),
            (
                "cap at",
                Step::SetRole { name: "app", password: Some(PW) },
                Facts { copy_count: 1, ..facts() },
                capped,
                Err("this branch has reached its limit of 1 login roles (CHELADB_BRANCH_ROLE_CAP)".into()),
            ),
            (
                "cap below",
                Step::SetRole { name: "app", password: Some(PW) },
                Facts { copy_count: 0, ..facts() },
                capped,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "cap at, existing copy",
                Step::SetRole { name: "app", password: Some(PW) },
                Facts { copy_count: 1, row: row(RoleOrigin::Sql, true, false), ..facts() },
                capped,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "cap at, MD5 row gains a verifier",
                Step::SetRole { name: "app", password: Some(PW) },
                Facts { copy_count: 1, row: row(RoleOrigin::Sql, false, false), ..facts() },
                capped,
                Err("this branch has reached its limit of 1 login roles (CHELADB_BRANCH_ROLE_CAP)".into()),
            ),
            (
                "password null with a row",
                Step::SetRole { name: "app", password: None },
                Facts { row: row(RoleOrigin::Api, true, true), ..facts() },
                sh,
                Ok(Action::RemoveCopy("app")),
            ),
            (
                "group role",
                Step::SetRole { name: "grp", password: None },
                facts(),
                sh,
                Ok(Action::Nothing),
            ),
            (
                "owner password null",
                Step::SetRole { name: "postgres", password: None },
                facts(),
                sh,
                Err("the branch owner's password can't be removed; reset it instead".into()),
            ),
            (
                "rename api row",
                Step::RenameRole { old: "a", new: "b" },
                Facts { row: row(RoleOrigin::Api, true, true), ..facts() },
                sh,
                Ok(Action::RenameCopy { old: "a", new: "b", reseal: true }),
            ),
            (
                "rename sql row",
                Step::RenameRole { old: "a", new: "b" },
                Facts { row: row(RoleOrigin::Sql, true, false), ..facts() },
                sh,
                Ok(Action::RenameCopy { old: "a", new: "b", reseal: false }),
            ),
            (
                "rename revealable sql row (M4)",
                Step::RenameRole { old: "a", new: "b" },
                Facts { row: row(RoleOrigin::Sql, true, true), ..facts() },
                sh,
                Ok(Action::RenameCopy { old: "a", new: "b", reseal: true }),
            ),
            (
                "rename unknown",
                Step::RenameRole { old: "a", new: "b" },
                facts(),
                sh,
                Ok(Action::Nothing),
            ),
            (
                "rename owner",
                Step::RenameRole { old: "postgres", new: "b" },
                facts(),
                sh,
                Err(owner_text.into()),
            ),
            (
                "rename to reserved",
                Step::RenameRole { old: "a", new: "pg_x" },
                facts(),
                sh,
                Err("the role name \"pg_x\" is reserved by Cheladb".into()),
            ),
            (
                "drop owner",
                Step::DelRole { name: "postgres" },
                facts(),
                sh,
                Err(owner_text.into()),
            ),
            (
                "drop key-backed",
                Step::DelRole { name: "app" },
                Facts { live_keys: vec!["k1".into(), "k2".into()], ..facts() },
                sh,
                Err("role \"app\" backs data API key(s) k1, k2; revoke them first".into()),
            ),
            (
                "drop missing",
                Step::DelRole { name: "ghost" },
                facts(),
                sh,
                Ok(Action::RemoveCopy("ghost")),
            ),
            (
                "rename main db",
                Step::RenameDb { old: "neondb", new: "x" },
                facts(),
                sh,
                Err("the branch's main database \"neondb\" can't be renamed".into()),
            ),
            (
                "rename other db",
                Step::RenameDb { old: "a", new: "b" },
                facts(),
                sh,
                Ok(Action::RenameDatabase { old: "a", new: "b" }),
            ),
            (
                "db cap at",
                Step::SetDb { name: "x", owner: "postgres" },
                Facts { database_count: 1, ..facts() },
                capped,
                Err("this branch has reached its limit of 1 databases (CHELADB_BRANCH_DATABASE_CAP)".into()),
            ),
            (
                "db owner change at cap",
                Step::SetDb { name: "x", owner: "app" },
                Facts { database_count: 1, db_exists: true, ..facts() },
                capped,
                Ok(Action::UpsertDatabase { name: "x", owner: "app" }),
            ),
            (
                "drop main db",
                Step::DelDb { name: "neondb" },
                facts(),
                sh,
                Ok(Action::RemoveDatabase { name: "neondb", main: true }),
            ),
            (
                "drop other db",
                Step::DelDb { name: "x" },
                facts(),
                sh,
                Ok(Action::RemoveDatabase { name: "x", main: false }),
            ),
        ];
        for (what, step, facts, rules, want) in cases {
            assert_eq!(decide(&step, &facts, &rules), want, "{what}");
        }
    }

    #[test]
    fn texts_are_one_line_and_short() {
        assert_eq!(
            text::reserved("a\nb"),
            "the role name \"a?b\" is reserved by Cheladb"
        );
        let long = "x".repeat(300);
        assert!(text::reserved(&long).len() <= text::MAX);
        let keys: Vec<String> = (0..40).map(|i| format!("key-{i:02}")).collect();
        let t = text::in_use("app", &keys);
        assert!(t.len() <= text::MAX, "{t}");
        assert!(
            t.starts_with("role \"app\" backs data API key(s) key-00, key-01"),
            "{t}"
        );
        assert!(t.ends_with(", …; revoke them first"), "{t}");
        let one_huge = vec!["k".repeat(250)];
        let t = text::in_use("app", &one_huge);
        assert!(
            t.len() <= text::MAX && t.ends_with("…; revoke them first"),
            "{t}"
        );
        assert_eq!(
            text::in_use("app", &["a".into(), "b".into()]),
            "role \"app\" backs data API key(s) a, b; revoke them first"
        );
        let mb = "é".repeat(150);
        let t = text::fit(mb);
        assert!(t.len() <= text::MAX);
    }
}
