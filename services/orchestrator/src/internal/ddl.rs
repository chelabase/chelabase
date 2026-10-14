//! `PATCH /compute/api/v2/computes/{id}/ddl`: the DDL receiver (spec 2026-10-01-login-roles,
//! Part 2). Each compute forwards every transaction that touches roles or databases here at
//! pre-commit (Neon's `neon_ddl_handler.c`), with its own token. A 200 lets the transaction
//! commit; a 4xx with a one-line `text/plain` body fails it with that text. The compute's curl
//! gives up after 3 s and retries, so the receiver is idempotent and answers "try again"
//! (503) when it can't take the endpoint's lock quickly.
//!
//! A refusal never echoes a password: the texts name roles, databases, keys and settings only.

use super::spec::authorize_compute;
use super::{DDL_BODY_LIMIT, InternalState, NeonMetadataPort};
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chelabase_metadata::{MetadataError, RoleCopy, RoleOrigin};
use chelabase_nats::{
    MemberRolesReconcileRequest, ResyncBranchRequest, SUBJECT_MEMBER_ROLES_ENQUEUE,
    SUBJECT_RESYNC_BRANCH,
};
use chelabase_neon::ComputeLocator;
use chelabase_neon::proxy_cache::ProxyCache;
use chelabase_secrets::{SecretKey, open_role_password, seal_role_password};
use chelabase_types::BranchId;
use chelabase_types::role_name::is_reserved_for;
use chelabase_types::role_password::check_forwarded_password;
use chelabase_types::role_settings::RoleSettings;
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

/// Asks the worker to resync a branch's roles soon (`chelabase.resync.branch`). NATS in
/// production, a fake in tests.
pub trait ResyncPort: Send + Sync + 'static {
    fn resync_branch(&self, branch: BranchId) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Asks the worker to reconcile the branch's member roles soon
    /// (`chelabase.member_roles.reconcile`).
    fn reconcile_member_roles(&self, branch: BranchId) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl ResyncPort for async_nats::Client {
    fn resync_branch(&self, branch: BranchId) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&ResyncBranchRequest { branch_id: branch })?;
            self.publish(SUBJECT_RESYNC_BRANCH, body.into()).await?;
            Ok(())
        })
    }

    fn reconcile_member_roles(&self, branch: BranchId) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(&MemberRolesReconcileRequest {
                branch_id: branch.as_uuid(),
                member_id: None,
            })?;
            self.publish(SUBJECT_MEMBER_ROLES_ENQUEUE, body.into()).await?;
            Ok(())
        })
    }
}

/// The receiver's transaction: [`chelabase_metadata::DdlTx`]'s methods, so tests can use a
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
    fn set_login(
        &mut self,
        name: &str,
        login: bool,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_owner_verifier(
        &mut self,
        verifier: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn mark_password_unreadable(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn remove_copy(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn upsert_no_password(
        &mut self,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
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
impl DdlPort for chelabase_metadata::DdlTx {
    fn branch_id(&self) -> Option<BranchId> {
        chelabase_metadata::DdlTx::branch_id(self)
    }
    fn owner(&self) -> &str {
        chelabase_metadata::DdlTx::owner(self)
    }
    fn main_database(&self) -> Option<&str> {
        chelabase_metadata::DdlTx::main_database(self)
    }
    async fn role(&mut self, name: &str) -> Result<Option<RoleCopy>, MetadataError> {
        chelabase_metadata::DdlTx::role(self, name).await
    }
    async fn copy_count(&mut self) -> Result<u32, MetadataError> {
        chelabase_metadata::DdlTx::copy_count(self).await
    }
    async fn live_secret_keys(&mut self, role: &str) -> Result<Vec<(Uuid, String)>, MetadataError> {
        chelabase_metadata::DdlTx::live_secret_keys(self, role).await
    }
    async fn upsert_sql_copy(&mut self, name: &str, verifier: &str) -> Result<(), MetadataError> {
        chelabase_metadata::DdlTx::upsert_sql_copy(self, name, verifier).await
    }
    async fn set_login(&mut self, name: &str, login: bool) -> Result<bool, MetadataError> {
        chelabase_metadata::DdlTx::set_login(self, name, login).await
    }
    async fn set_owner_verifier(&mut self, verifier: &str) -> Result<(), MetadataError> {
        chelabase_metadata::DdlTx::set_owner_verifier(self, verifier).await
    }
    async fn mark_password_unreadable(&mut self, name: &str) -> Result<(), MetadataError> {
        chelabase_metadata::DdlTx::mark_password_unreadable(self, name).await
    }
    async fn remove_copy(&mut self, name: &str) -> Result<bool, MetadataError> {
        chelabase_metadata::DdlTx::remove_copy(self, name).await
    }
    async fn upsert_no_password(&mut self, name: &str) -> Result<(), MetadataError> {
        chelabase_metadata::DdlTx::upsert_no_password(self, name).await
    }
    async fn rename_copy(
        &mut self,
        old: &str,
        new: &str,
        resealed: Option<&str>,
    ) -> Result<bool, MetadataError> {
        chelabase_metadata::DdlTx::rename_copy(self, old, new, resealed).await
    }
    async fn has_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        chelabase_metadata::DdlTx::has_database(self, name).await
    }
    async fn database_count(&mut self) -> Result<u32, MetadataError> {
        chelabase_metadata::DdlTx::database_count(self).await
    }
    async fn upsert_database(&mut self, name: &str, owner: &str) -> Result<(), MetadataError> {
        chelabase_metadata::DdlTx::upsert_database(self, name, owner).await
    }
    async fn rename_database(&mut self, old: &str, new: &str) -> Result<bool, MetadataError> {
        chelabase_metadata::DdlTx::rename_database(self, old, new).await
    }
    async fn remove_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        chelabase_metadata::DdlTx::remove_database(self, name).await
    }
    async fn commit(self) -> Result<(), MetadataError> {
        chelabase_metadata::DdlTx::commit(self).await
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
    /// `None`: no key; `Some(None)`: an explicit JSON null (a rename that also sets
    /// `PASSWORD NULL`).
    #[serde(default, deserialize_with = "nullable")]
    password: Option<Option<String>>,
    encrypted_password: Option<String>,
    /// `LOGIN`/`NOLOGIN` as written in the statement; absent when it didn't say.
    #[serde(default)]
    login: Option<bool>,
    /// `VALID UNTIL` as written. Accepted and not mirrored (Chelabase keeps no expiry, S1),
    /// but it makes the entry an attribute change.
    #[serde(default)]
    valid_until: Option<String>,
    /// The role was dropped and created again in the same transaction (the fork sends the key
    /// only when true, with `touched`). Old payloads lack it.
    #[serde(default)]
    recreated: bool,
    /// The statement changed something the receiver can't mirror (an attribute, a grant): the
    /// branch gets a resync shortly.
    #[serde(default)]
    touched: bool,
}

/// Tells an absent key (`None`, through `default`) from an explicit null (`Some(None)`).
fn nullable<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
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
    /// The role was dropped and created again in one transaction (`recreated`): the drop's
    /// refusals apply before the set that follows, and the dropped role's row goes, so the new
    /// role starts from a fresh one (origin `sql`, plain, no API fields).
    CheckRecreated {
        name: &'a str,
    },
    /// A forwarded `LOGIN`/`NOLOGIN` for a role with a copy.
    SetLogin {
        name: &'a str,
        login: bool,
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
/// (a plain rename has none and must not remove the copy). Since round 2's OID tracking the
/// fork sends a role renamed, then dropped, in one transaction as a `del` under its name from
/// before the transaction (no `old_name`); a `del` with `old_name` from older computes is
/// still accepted. The fork relies on this order (renames, then sets with `CheckRecreated`
/// before `SetRole`, then deletes) and on keys moving with every rename. A database `set`
/// without an owner (a rename of a database created elsewhere) has nothing more to record.
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
        let password = r.password.as_ref().and_then(|p| p.as_deref()).map(|plain| Password {
            plain,
            // Neon sends both or neither; a missing hash fails the SCRAM check.
            encrypted: r.encrypted_password.as_deref().unwrap_or(""),
        });
        // An explicit null next to `old_name` removes the copy (or is refused for the owner);
        // without `old_name` it is the same as a missing password.
        let explicit_null = matches!(r.password, Some(None));
        // An entry with no password key that carries a login or a touch is an attribute
        // change (`ALTER ROLE … NOLOGIN`, `GRANT`), not a group role or a password removal.
        let attribute_only = r.password.is_none()
            && r.encrypted_password.is_none()
            && (r.login.is_some() || r.valid_until.is_some() || r.touched);
        // A `recreated` entry that also carries `old_name` checks the new name's keys. The
        // rename step before it has moved the dropped role's keys (live and revoked, with or
        // without a `neon_roles` row) to that name, so they are found there.
        if r.recreated {
            out.push(Step::CheckRecreated { name: &r.name });
        }
        if !attribute_only && (r.old_name.is_none() || password.is_some() || explicit_null) {
            out.push(Step::SetRole {
                name: &r.name,
                password,
            });
        }
        if let Some(login) = r.login {
            out.push(Step::SetLogin {
                name: &r.name,
                login,
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

/// Some role of the delta was touched, or dropped (a dropped group changes its members'
/// derived levels): the branch is resynced soon.
fn touched(delta: &Delta) -> bool {
    delta.roles.iter().any(|r| r.touched || r.op == Op::Del)
}

/// What the transaction knows that a step's decision needs.
#[derive(Debug, Default)]
struct Facts<'a> {
    owner: &'a str,
    main_database: Option<&'a str>,
    /// The row the step names (the old name for a rename).
    row: Option<RowFacts>,
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
    /// The copy's `login` flag (and `forwarded_at`).
    SetLogin {
        name: &'a str,
        login: bool,
    },
    RemoveCopy(&'a str),
    /// A role with no password (a group role, or `PASSWORD NULL`): an `sql` row without a
    /// verifier, inserted or updated (`forwarded_at` set).
    UpsertNoPassword(&'a str),
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
            // The role cap isn't checked per step: `apply` checks it once on the final count.
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
                // An `api` row's password is the control plane's: the resync sets it back.
                Some(row) if row.origin == RoleOrigin::Api => Action::RemoveCopy(name),
                // A group role, or a password removed in SQL: the role exists, so its row is
                // kept (or made) without a verifier, and members can be granted it.
                _ => Action::UpsertNoPassword(name),
            })
        }
        // The owner always logs in, a reserved name has no copy, and a role with no copy
        // (no password, so no verifier) has nothing to flag: none of these is a refusal.
        Step::SetLogin { name, login } => Ok(
            if name == owner || is_reserved_for(name, owner) || facts.row.is_none() {
                Action::Nothing
            } else {
                Action::SetLogin { name, login }
            },
        ),
        Step::CheckRecreated { name } => {
            if name == owner {
                return Err(text::owner(owner));
            }
            reserved(name)?;
            if !facts.live_keys.is_empty() {
                return Err(text::in_use(name, &facts.live_keys));
            }
            Ok(Action::RemoveCopy(name))
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
    pub const TOO_LARGE: &str = "request body too large";
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
            "the role name \"{}\" is reserved by Chelabase",
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
        format!("this branch has reached its limit of {cap} login roles (CHELABASE_BRANCH_ROLE_CAP)")
    }

    pub fn database_cap(cap: u32) -> String {
        format!(
            "this branch has reached its limit of {cap} databases (CHELABASE_BRANCH_DATABASE_CAP)"
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
    body: Body,
) -> Response {
    // The token first (the spec route's path and timing), so a caller without it learns
    // nothing about the body format.
    if let Err(refused) = authorize_compute(&state, &id, &headers).await {
        return match refused.status() {
            StatusCode::UNAUTHORIZED => answer(StatusCode::UNAUTHORIZED, text::TOKEN_REFUSED),
            _ => answer(StatusCode::SERVICE_UNAVAILABLE, text::UNAVAILABLE),
        };
    }
    // Buffered only now, after the token (C6); an oversized body is still 413.
    let body = match axum::body::to_bytes(body, DDL_BODY_LIMIT).await {
        Ok(body) => body,
        Err(_) => return answer(StatusCode::PAYLOAD_TOO_LARGE, text::TOO_LARGE),
    };
    // The parse error isn't logged or returned: it can quote a value from the body.
    let Ok(delta) = serde_json::from_slice::<Delta>(&body) else {
        warn!(endpoint = %id, "ddl: malformed request body");
        return answer(StatusCode::BAD_REQUEST, text::MALFORMED);
    };
    // `ALTER ROLE … LOGIN`, `GRANT` and the like send `{"roles":[]}` (R4).
    if delta.roles.is_empty() && delta.dbs.is_empty() {
        return StatusCode::OK.into_response();
    }
    let plan = steps(&delta);
    // A delta that only touches roles (`ALTER ROLE … CREATEDB`, `GRANT`) changes no row here:
    // no transaction and no endpoint lock, just the resync request.
    if plan.is_empty() && delta.dbs.is_empty() {
        if touched(&delta) {
            match state.metadata.endpoint_branch(&id).await {
                Ok(Some(branch)) => {
                    request_resync(state.resync.clone(), branch);
                    request_member_reconcile(state.resync.clone(), branch);
                }
                // An unknown or deleted endpoint with a step-less delta answers 200 on purpose:
                // there is nothing to apply and no branch to resync.
                Ok(None) => {}
                Err(e) => warn!(endpoint = %id, "ddl: resync not requested: {e:#}"),
            }
        }
        return StatusCode::OK.into_response();
    }
    let tx = match state.metadata.begin_ddl(&id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return answer(StatusCode::NOT_FOUND, text::UNKNOWN_ENDPOINT),
        Err(e) => return metadata_failure(&id, &e),
    };
    let Applied {
        roles: changed,
        branch,
        served_databases_changed,
    } = match apply(tx, &plan, &state.roles, &state.secrets, &id).await {
        Ok(applied) => applied,
        Err(Failure::Refused(message)) => return answer(StatusCode::CONFLICT, message),
        Err(Failure::Metadata(e)) => return metadata_failure(&id, &e),
    };
    // A dropped or renamed database may be one rest serves: it must forget it. Off the response
    // path (compute_ctl waits for us), and its failure is only logged.
    if let Some(branch) = served_databases_changed {
        let state = state.clone();
        tokio::spawn(async move { state.rest.invalidate(branch).await });
    }
    // Attributes and grants aren't mirrored here: the worker reads them from the catalog
    // soon. After the commit, once per request; its failure only delays that to the periodic
    // resync.
    if let (true, Some(branch)) = (touched(&delta), branch) {
        request_resync(state.resync.clone(), branch);
    }
    // T8 I3: a role made, dropped, renamed or changed in SQL can change what member roles may
    // hold (R16) or which names grants find. PB R12: a database made (or renamed, or given a
    // new owner: any `set`) needs the protected-branch guard, which the same reconcile
    // installs; a database only dropped needs nothing (member roles are cluster-wide, R17).
    let makes_database = delta.dbs.iter().any(|d| d.op == Op::Set);
    if let (true, Some(branch)) = (!delta.roles.is_empty() || makes_database, branch) {
        request_member_reconcile(state.resync.clone(), branch);
    }
    publish(state.cache.clone(), id, changed);
    StatusCode::OK.into_response()
}

/// Asks the worker for a resync of `branch`, off the response path.
fn request_resync(resync: Arc<dyn ResyncPort>, branch: BranchId) {
    tokio::spawn(async move {
        if let Err(e) = resync.resync_branch(branch).await {
            warn!(%branch, "ddl: resync not requested: {e:#}");
        }
    });
}

/// Asks the worker for a member-role reconcile of `branch`, off the response path. A lost
/// request is caught by the worker's periodic sweep.
fn request_member_reconcile(resync: Arc<dyn ResyncPort>, branch: BranchId) {
    tokio::spawn(async move {
        if let Err(e) = resync.reconcile_member_roles(branch).await {
            warn!(%branch, "ddl: member-role reconcile not requested: {e:#}");
        }
    });
}

/// What a committed delta changed.
struct Applied {
    /// The role names whose secrets changed.
    roles: Vec<String>,
    /// The endpoint's branch, if any.
    branch: Option<BranchId>,
    /// The branch whose databases lost or renamed one, if any.
    served_databases_changed: Option<BranchId>,
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
/// transaction (rolling back what came before).
async fn apply<T: DdlPort>(
    mut tx: T,
    steps: &[Step<'_>],
    rules: &RoleSettings,
    key: &SecretKey,
    endpoint: &str,
) -> Result<Applied, Failure> {
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
    let mut databases_changed = false;
    // The role cap is checked once, on the final count, so the order of a delta's steps
    // doesn't matter: only a delta that ends over the cap and above where it began is refused.
    let copies_before = if steps.iter().any(|s| {
        matches!(
            s,
            Step::SetRole {
                password: Some(_),
                ..
            }
        )
    }) {
        Some(tx.copy_count().await?)
    } else {
        None
    };
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
        databases_changed |= matches!(step, Step::RenameDb { .. } | Step::DelDb { .. });
        let mut facts = Facts {
            owner: &owner,
            main_database: main_database.as_deref(),
            ..Facts::default()
        };
        match *step {
            Step::RenameRole { old: name, .. }
            | Step::SetRole { name, .. }
            | Step::SetLogin { name, .. } => {
                facts.row = tx.role(name).await?.as_ref().map(RowFacts::of);
            }
            Step::DelRole { name } | Step::CheckRecreated { name } => {
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
            // A role with no copy (no password) can still back keys (key creation checks the
            // role on the compute, not its row): they go aside with the name, so they keep
            // their role's current name (their OID binds them to it either way).
            (Step::RenameRole { old, new }, Action::Nothing) => {
                let aside = aside_name(&tag, role_moves.len());
                tx.rename_copy(old, &aside, None).await?;
                role_moves.push(Move {
                    aside: Some(aside),
                    new: new.to_string(),
                    resealed: None,
                    unreadable: false,
                });
                touch(old);
                touch(new);
            }
            (Step::RenameDb { new, .. }, _) => {
                let aside = aside_name(&tag, db_moves.len());
                let moved = tx.rename_database(step_old(step), &aside).await?;
                db_moves.push(Move {
                    aside: moved.then_some(aside),
                    new: new.to_string(),
                    resealed: None,
                    unreadable: false,
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
            Action::SetLogin { name, login } => {
                tx.set_login(name, login).await?;
                touch(name);
            }
            Action::RemoveCopy(name) => {
                tx.remove_copy(name).await?;
                touch(name);
            }
            Action::UpsertNoPassword(name) => {
                tx.upsert_no_password(name).await?;
                touch(name);
            }
            Action::RenameCopy { old, new, reseal } => {
                let (resealed, unreadable) = if reseal {
                    reseal_password(&mut tx, key, branch, old, new, endpoint).await?
                } else {
                    (None, false)
                };
                let aside = aside_name(&tag, role_moves.len());
                tx.rename_copy(old, &aside, resealed.as_deref()).await?;
                role_moves.push(Move {
                    aside: Some(aside),
                    new: new.to_string(),
                    resealed,
                    unreadable,
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
    if let Some(before) = copies_before {
        let after = tx.copy_count().await?;
        if after > rules.role_cap && after > before {
            return Err(Failure::Refused(text::role_cap(rules.role_cap)));
        }
    }
    tx.commit().await?;
    Ok(Applied {
        roles: changed,
        branch,
        served_databases_changed: branch.filter(|_| databases_changed),
    })
}

/// A rename's second hop (M2). Renames move in two hops: each row first goes aside under a
/// temporary name, and only once every rename of the transaction has left its old name does
/// it take its new one. A row still under a new name by then is stale (its role was dropped
/// in the same transaction, which Neon sends as the rename alone, or a leftover) and is
/// removed. So a swap of two names, which Neon sends as two renames onto each other's names,
/// moves both rows instead of hitting the unique key.
struct Move {
    /// Where the row and the keys of the old name wait (a name without a row still has keys
    /// to move); `None` for a database with no row.
    aside: Option<String>,
    new: String,
    /// The sealed password for the new name (`rename_copy` sets it on each hop).
    resealed: Option<String>,
    /// The old sealed password won't open: mark the row unreadable once it has its new name.
    unreadable: bool,
}

/// The `i`-th rename's temporary name in one request: a control character, a per-request
/// random tag (so no existing or forwarded name can be one) and the index; 52 bytes at most,
/// within the 63-byte limit.
fn aside_name(tag: &str, i: usize) -> String {
    format!("\u{1}chelabase rename {tag} {i}")
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
            if m.unreadable {
                tx.mark_password_unreadable(&m.new).await?;
            }
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
/// value that won't open is dropped with a warning (the second field is then true, so the
/// row gets marked `unreadable`); the copy still works. No branch to bind it to: dropped
/// quietly.
async fn reseal_password<T: DdlPort>(
    tx: &mut T,
    key: &SecretKey,
    branch: Option<BranchId>,
    old: &str,
    new: &str,
    endpoint: &str,
) -> Result<(Option<String>, bool), MetadataError> {
    let sealed = tx.role(old).await?.and_then(|r| r.password_enc);
    let (Some(sealed), Some(branch)) = (sealed, branch) else {
        return Ok((None, false));
    };
    let branch = branch.to_string();
    match open_role_password(key, &branch, old, &sealed) {
        Ok(password) => Ok((Some(seal_role_password(key, &branch, new, &password)), false)),
        Err(e) => {
            warn!(
                endpoint,
                "ddl: a renamed role's sealed password won't open ({e}); dropped"
            );
            Ok((None, true))
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
    use chelabase_types::mode::Mode;

    const SCRAM: &str = "SCRAM-SHA-256$4096:c2FsdA==$a2V5:c2VydmVy";

    fn rules(mode: Mode, strength: bool) -> RoleSettings {
        RoleSettings::from_lookup(mode, |k| {
            (k == "CHELABASE_PASSWORD_STRENGTH").then(|| if strength { "on" } else { "off" }.into())
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
    fn a_forwarded_login_becomes_a_step_after_the_password() {
        let d = delta(
            r#"{"roles":[
                {"op":"set","name":"new_copy","password":"p","encrypted_password":"SCRAM-SHA-256$x","login":false},
                {"op":"set","name":"app","login":false,"valid_until":"2030-01-01","touched":true},
                {"op":"set","name":"grp","touched":true},
                {"op":"set","name":"b","old_name":"a","login":true}
            ]}"#,
        );
        let p = Password {
            plain: "p",
            encrypted: "SCRAM-SHA-256$x",
        };
        assert_eq!(
            steps(&d),
            vec![
                Step::RenameRole { old: "a", new: "b" },
                Step::SetRole {
                    name: "new_copy",
                    password: Some(p)
                },
                Step::SetLogin {
                    name: "new_copy",
                    login: false
                },
                Step::SetLogin {
                    name: "app",
                    login: false
                },
                Step::SetLogin {
                    name: "b",
                    login: true
                },
            ],
            "a touched-only entry is not a password removal"
        );
        assert!(touched(&d));
    }

    // The fork sends an explicit `"password": null` beside `login` for a CREATE ROLE without
    // PASSWORD, so the refusals and RemoveCopy apply (characterization pins of the contract).
    #[test]
    fn create_role_without_password_is_a_set_then_a_login() {
        let d = delta(r#"{"roles":[{"op":"set","name":"r1","password":null,"login":false}]}"#);
        assert_eq!(
            steps(&d),
            vec![
                Step::SetRole {
                    name: "r1",
                    password: None
                },
                Step::SetLogin {
                    name: "r1",
                    login: false
                },
            ]
        );
        let sh = rules(Mode::SelfHosted, false);
        let refused = |name: &str, with_row: bool| {
            let body = format!(
                r#"{{"roles":[{{"op":"set","name":"{name}","password":null,"login":false}}]}}"#
            );
            let d = delta(&body);
            let s = steps(&d);
            let f = Facts {
                row: if with_row {
                    row(RoleOrigin::Sql, true, false)
                } else {
                    None
                },
                ..facts()
            };
            // Debug text: the action borrows from the local delta.
            decide(&s[0], &f, &sh).map(|a| format!("{a:?}"))
        };
        assert_eq!(
            refused("chelabase_admin", false),
            Err("the role name \"chelabase_admin\" is reserved by Chelabase".into()),
            "reserved"
        );
        assert_eq!(
            refused("postgres", false),
            Err("the branch owner's password can't be removed; reset it instead".into()),
            "owner"
        );
        // The role exists without a password: its row is kept (or made), with no verifier, so
        // the role can still be granted to members.
        assert_eq!(
            refused("r1", true),
            Ok("UpsertNoPassword(\"r1\")".to_string()),
            "existing copy"
        );
        assert_eq!(refused("r1", false), Ok("UpsertNoPassword(\"r1\")".to_string()), "no copy");
    }

    #[test]
    fn a_valid_until_only_entry_is_no_set_role() {
        let d = delta(
            r#"{"roles":[{"op":"set","name":"app","valid_until":"infinity"},
                {"op":"set","name":"postgres","valid_until":"2030-01-01","touched":true}]}"#,
        );
        assert_eq!(steps(&d), vec![], "an attribute change, not a password removal");
    }

    // D20: the fork marks a drop-and-create in one transaction; the drop's key check runs
    // first, with a password and without one.
    #[test]
    fn a_recreated_entry_checks_keys_before_its_set() {
        let d = delta(
            r#"{"roles":[
                {"op":"set","name":"a","password":"p","encrypted_password":"SCRAM-SHA-256$x","recreated":true,"touched":true},
                {"op":"set","name":"b","password":null,"recreated":true,"touched":true},
                {"op":"set","name":"c","password":"p","encrypted_password":"SCRAM-SHA-256$x","recreated":false}
            ]}"#,
        );
        let p = Password {
            plain: "p",
            encrypted: "SCRAM-SHA-256$x",
        };
        assert_eq!(
            steps(&d),
            vec![
                Step::CheckRecreated { name: "a" },
                Step::SetRole { name: "a", password: Some(p) },
                Step::CheckRecreated { name: "b" },
                Step::SetRole { name: "b", password: None },
                Step::SetRole { name: "c", password: Some(p) },
            ]
        );
    }

    /// Create, rename (either side) and drop of `chelabase_…` / `cb_m_…` (any case) are refused.
    #[test]
    fn hidden_prefixes_at_the_receiver() {
        let sh = rules(Mode::SelfHosted, false);
        let reserved = |n: &str| Err(format!("the role name \"{n}\" is reserved by Chelabase"));
        let pw = Some(PW);
        for name in ["chelabase_x", "cb_m_x", "CB_M_X"] {
            assert_eq!(
                decide(&Step::SetRole { name, password: pw }, &facts(), &sh),
                reserved(name),
                "create {name}"
            );
            assert_eq!(
                decide(&Step::SetRole { name, password: None }, &facts(), &sh),
                reserved(name),
                "create group {name}"
            );
            assert_eq!(
                decide(&Step::RenameRole { old: "a", new: name }, &facts(), &sh),
                reserved(name),
                "rename to {name}"
            );
            assert_eq!(
                decide(&Step::CheckRecreated { name }, &facts(), &sh),
                reserved(name),
                "recreate {name}"
            );
            assert_eq!(
                decide(&Step::DelRole { name }, &facts(), &sh),
                reserved(name),
                "drop {name}"
            );
            assert_eq!(
                decide(&Step::RenameRole { old: name, new: "a" }, &facts(), &sh),
                reserved(name),
                "rename away {name}"
            );
        }
    }

    #[test]
    fn a_recreated_check_refuses_the_owner() {
        let sh = rules(Mode::SelfHosted, false);
        assert_eq!(
            decide(&Step::CheckRecreated { name: "postgres" }, &facts(), &sh),
            Err("the branch owner \"postgres\" can't be dropped or renamed".into())
        );
    }

    #[test]
    fn a_recreated_check_refuses_like_a_drop_while_keys_live() {
        let sh = rules(Mode::SelfHosted, false);
        let step = Step::CheckRecreated { name: "app" };
        assert_eq!(
            decide(&step, &facts(), &sh),
            Ok(Action::RemoveCopy("app")),
            "no live keys: the dropped role's row goes, the new role starts fresh"
        );
        assert_eq!(
            decide(&Step::CheckRecreated { name: "anon" }, &facts(), &sh),
            Err("the role name \"anon\" is reserved by Chelabase".into()),
            "a reserved name can't be dropped, so not recreated either"
        );
        assert_eq!(
            decide(
                &step,
                &Facts { live_keys: vec!["k1".into(), "k2".into()], ..facts() },
                &sh
            ),
            Err("role \"app\" backs data API key(s) k1, k2; revoke them first".into())
        );
    }

    #[test]
    fn a_role_drop_is_touched() {
        let d = delta(r#"{"roles":[{"op":"del","name":"grp"}]}"#);
        assert!(touched(&d), "a dropped group changes its members' derived levels");
        let d = delta(r#"{"roles":[{"op":"set","name":"app","password":"p","encrypted_password":"SCRAM-SHA-256$x"}]}"#);
        assert!(!touched(&d));
    }

    #[test]
    fn an_old_payload_parses_and_touches_nothing() {
        let d = delta(
            r#"{"roles":[{"op":"set","name":"app","password":"p","encrypted_password":"SCRAM-SHA-256$x"},{"op":"set","name":"grp"}]}"#,
        );
        assert!(!touched(&d));
        assert!(
            d.roles
                .iter()
                .all(|r| r.login.is_none() && r.valid_until.is_none() && !r.recreated)
        );
        let p = Password {
            plain: "p",
            encrypted: "SCRAM-SHA-256$x",
        };
        assert_eq!(
            steps(&d),
            vec![
                Step::SetRole {
                    name: "app",
                    password: Some(p)
                },
                Step::SetRole {
                    name: "grp",
                    password: None
                },
            ]
        );
    }

    #[test]
    fn set_login_decisions() {
        let sh = rules(Mode::SelfHosted, false);
        let step = |name| Step::SetLogin { name, login: false };
        let with_row = Facts {
            row: row(RoleOrigin::Sql, true, false),
            ..facts()
        };
        assert_eq!(
            decide(&step("app"), &with_row, &sh),
            Ok(Action::SetLogin {
                name: "app",
                login: false
            })
        );
        assert_eq!(
            decide(&step("app"), &facts(), &sh),
            Ok(Action::Nothing),
            "no copy"
        );
        assert_eq!(
            decide(&step("postgres"), &with_row, &sh),
            Ok(Action::Nothing),
            "owner"
        );
        assert_eq!(
            decide(&step("anon"), &with_row, &sh),
            Ok(Action::Nothing),
            "reserved"
        );
    }

    #[test]
    fn a_password_without_its_hash_fails_the_scram_check() {
        let d = delta(r#"{"roles":[{"op":"set","name":"x","password":"Tr0ub4dor&3x-horse"}]}"#);
        let s = steps(&d);
        let got = decide(&s[0], &facts(), &rules(Mode::SelfHosted, false));
        assert_eq!(
            got,
            Err("this password format can't be used through Chelabase; set a plain password".into())
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
                Err("the role name \"neon_superuser\" is reserved by Chelabase".into()),
            ),
            (
                "reserved group role",
                Step::SetRole { name: "cloud_admin", password: None },
                facts(),
                sh,
                Err("the role name \"cloud_admin\" is reserved by Chelabase".into()),
            ),
            (
                "cap at (checked once by apply, not per step)",
                Step::SetRole { name: "app", password: Some(PW) },
                facts(),
                capped,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "cap below",
                Step::SetRole { name: "app", password: Some(PW) },
                facts(),
                capped,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "cap at, existing copy",
                Step::SetRole { name: "app", password: Some(PW) },
                Facts { row: row(RoleOrigin::Sql, true, false), ..facts() },
                capped,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "cap at, MD5 row gains a verifier (checked once by apply)",
                Step::SetRole { name: "app", password: Some(PW) },
                Facts { row: row(RoleOrigin::Sql, false, false), ..facts() },
                capped,
                Ok(Action::UpsertCopy(SCRAM)),
            ),
            (
                "password null with a row",
                Step::SetRole { name: "app", password: None },
                Facts { row: row(RoleOrigin::Api, true, true), ..facts() },
                sh,
                Ok(Action::RemoveCopy("app")),
            ),
            (
                "password null on an sql row: kept, without a password",
                Step::SetRole { name: "app", password: None },
                Facts { row: row(RoleOrigin::Sql, true, false), ..facts() },
                sh,
                Ok(Action::UpsertNoPassword("app")),
            ),
            (
                "group role: a row without a password",
                Step::SetRole { name: "grp", password: None },
                facts(),
                sh,
                Ok(Action::UpsertNoPassword("grp")),
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
                Err("the role name \"pg_x\" is reserved by Chelabase".into()),
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
                Err("this branch has reached its limit of 1 databases (CHELABASE_BRANCH_DATABASE_CAP)".into()),
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
            "the role name \"a?b\" is reserved by Chelabase"
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
