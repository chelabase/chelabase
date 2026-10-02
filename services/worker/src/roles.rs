//! Login roles made through the API (spec 2026-10-01-login-roles, revision 2, Part 3):
//! create, reset and delete, each under the endpoint's lock and straight in SQL (forwarding is
//! off for `admin_sql`), with the metadata copy written alongside.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use cheladb_metadata::{BranchKind, BranchStatus, InsertRoleOutcome, RoleOrigin};
use cheladb_nats::{
    KeyRef, RESET_NOT_APPLIED, ROLE_CREATE_NOT_APPLIED, ROLE_DELETE_NOT_APPLIED, RoleCreateRequest,
    RoleReply, RoleRequest, SUBJECT_ROLE_CREATE, SUBJECT_ROLE_DELETE, SUBJECT_ROLE_RESET,
};
use cheladb_neon::ComputeBackend;
use cheladb_neon::backend::admin_psql_command;
use cheladb_secrets::seal_role_password;
use cheladb_types::BranchId;
use cheladb_types::data_key::DataKeyKind;
use cheladb_types::role_name::{
    RoleNameError, check_any_role_name, check_login_role_name, is_reserved_for, quote_ident,
    quote_literal,
};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::{create_role_sql, set_password_sql};
use crate::{OWNER_ROLE, Worker, credentials};

/// How a role operation ended; [`role_reply`] turns it into the RPC's reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleOutcome {
    Ok,
    /// The caller's fault: `(code, message)`.
    Refused(&'static str, String),
    /// Data API keys still use the role.
    InUse(Vec<KeyRef>),
    /// The branch isn't ready, or its compute can't be reached.
    NotReady,
    /// No such login role.
    NotFound,
    /// Stored, but not yet applied to the compute; the resync retries.
    NotApplied,
    /// Something broke on our side; the message is for logs.
    Failed(String),
}

/// `outcome` as the RPC's reply.
pub fn role_reply(outcome: RoleOutcome) -> RoleReply {
    let refused = |code: &str, error: String, keys: Vec<KeyRef>| RoleReply {
        ok: false,
        code: Some(code.to_string()),
        error: Some(error),
        keys,
    };
    match outcome {
        RoleOutcome::Ok => RoleReply {
            ok: true,
            code: None,
            error: None,
            keys: Vec::new(),
        },
        RoleOutcome::Refused(code, message) => refused(code, message, Vec::new()),
        RoleOutcome::InUse(keys) => refused(
            "role_in_use",
            "data API keys still use the role; revoke them first".into(),
            keys,
        ),
        RoleOutcome::NotReady => refused(
            "branch_not_ready",
            "the branch isn't ready, or its compute can't be reached".into(),
            Vec::new(),
        ),
        RoleOutcome::NotFound => refused(
            "not_found",
            "the branch has no login role of that name".into(),
            Vec::new(),
        ),
        RoleOutcome::NotApplied => refused("not_applied", RESET_NOT_APPLIED.into(), Vec::new()),
        RoleOutcome::Failed(message) => refused("failed", message, Vec::new()),
    }
}

/// The create rules (lowercase, not reserved), as a refusal.
fn check_name(name: &str) -> Result<(), RoleOutcome> {
    check_login_role_name(name).map_err(|e| match e {
        RoleNameError::Invalid => RoleOutcome::Refused(
            "invalid_role_name",
            format!(
                "{name:?} is not a valid role name: lowercase letters, digits and underscores, \
                 starting with a letter or an underscore, at most 63 characters"
            ),
        ),
        RoleNameError::Reserved => reserved(name),
    })
}

/// Any name Postgres can hold (R14: reset and delete reach roles made in SQL), as a refusal.
fn check_any_name(name: &str) -> Result<(), RoleOutcome> {
    check_any_role_name(name).map_err(|_| {
        RoleOutcome::Refused(
            "invalid_role_name",
            format!("{name:?} is not a valid role name: 1 to 63 bytes, without NUL"),
        )
    })
}

fn reserved(name: &str) -> RoleOutcome {
    RoleOutcome::Refused(
        "reserved_role_name",
        format!("the role name {name:?} is reserved"),
    )
}

/// Every database a role's objects can live in, one hex-encoded name per line (a name may
/// hold any character, a newline included).
const DATABASES_SQL: &str = "SET search_path = pg_catalog, pg_temp; SELECT \
    encode(convert_to(datname, 'UTF8'), 'hex') FROM pg_database WHERE datallowconn AND NOT \
    datistemplate ORDER BY datname;";

/// [`DATABASES_SQL`]'s answer as names, each one psql can be given.
fn database_names(out: &str) -> anyhow::Result<Vec<String>> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|line| {
            let name = hex::decode(line)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .with_context(|| format!("unexpected database name {line:?} in the listing"))?;
            admin_psql_command(&name)?;
            Ok(name)
        })
        .collect()
}

/// Whether `name` exists in SQL (`admin_sql` prints `1` for a hit).
fn exists_sql(name: &str) -> String {
    format!(
        "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {}",
        quote_literal(name)
    )
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Creates a login role (spec Part 3): the name rules, a ready `normal` branch, then under
    /// the endpoint's lock the SQL existence check, the `pending` row, the role made in SQL as
    /// the owner, the row confirmed and the proxies told. The password is never returned or
    /// logged.
    pub async fn create_role(
        &self,
        branch: BranchId,
        name: &str,
        owner_level: bool,
    ) -> RoleOutcome {
        self.create_role_tracked(branch, name, owner_level, &AtomicBool::new(false))
            .await
    }

    /// [`Self::create_role`], setting `stored` once the role's row is stored (a create cut
    /// short after that is `not_applied`; before it, nothing happened).
    async fn create_role_tracked(
        &self,
        branch: BranchId,
        name: &str,
        owner_level: bool,
        stored: &AtomicBool,
    ) -> RoleOutcome {
        if let Err(refusal) = check_name(name) {
            return refusal;
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };

        // A role made in SQL is never adopted: its password isn't ours to reveal.
        match self
            .computes
            .admin_sql(&ep, &database, &exists_sql(name))
            .await
        {
            Ok(out) if !out.trim().is_empty() => return exists(name),
            Ok(_) => {}
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "checking whether role {name} exists: {e:#}");
                return RoleOutcome::NotReady;
            }
        }

        let password = credentials::generate_password();
        let verifier = cheladb_neon::scram::verifier(&password);
        let sealed = seal_role_password(&self.key, &branch.to_string(), name, &password);
        let cap = self.roles.role_cap;
        match self
            .metadata
            .insert_api_role(&ep, name, owner_level, &verifier, &sealed, cap)
            .await
        {
            Ok(InsertRoleOutcome::Inserted) => stored.store(true, Ordering::SeqCst),
            Ok(InsertRoleOutcome::Exists) => return exists(name),
            Ok(InsertRoleOutcome::CapReached) => {
                return RoleOutcome::Refused(
                    "role_cap_reached",
                    format!("the branch already has {cap} login roles, the most it may have"),
                );
            }
            Err(e) => return failed(branch, "storing the role", &e),
        }

        let script = format!(
            "SET search_path = pg_catalog, pg_temp;\n{}",
            create_role_sql(OWNER_ROLE, name, &verifier, owner_level)
        );
        if let Err(outcome) = self
            .apply_and_confirm(branch, &ep, &database, name, &script)
            .await
        {
            return outcome;
        }
        info!(branch_id = %branch, endpoint_id = %ep, role = name, owner_level, "login role created");
        RoleOutcome::Ok
    }

    /// A new password for a login role; the owner's name takes the owner's reset. Any
    /// non-owner row works, one made in SQL too (it becomes revealable).
    pub async fn reset_role(&self, branch: BranchId, name: &str) -> RoleOutcome {
        if name == OWNER_ROLE {
            return match self.reset_password(branch).await {
                Ok(()) => RoleOutcome::Ok,
                Err(e) if e == RESET_NOT_APPLIED => RoleOutcome::NotApplied,
                Err(e)
                    if matches!(
                        e.as_str(),
                        "not ready" | "not found" | "credentials unavailable"
                    ) =>
                {
                    RoleOutcome::NotReady
                }
                Err(e) => RoleOutcome::Failed(e),
            };
        }
        if let Err(refusal) = check_any_name(name) {
            return refusal;
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };
        let password = credentials::generate_password();
        let verifier = cheladb_neon::scram::verifier(&password);
        let sealed = seal_role_password(&self.key, &branch.to_string(), name, &password);
        match self
            .metadata
            .replace_role_secret(&ep, name, &verifier, &sealed)
            .await
        {
            Ok(true) => {}
            Ok(false) => return RoleOutcome::NotFound,
            Err(e) => return failed(branch, "storing the new password", &e),
        }
        let sql = set_password_sql(name, &verifier);
        match self
            .apply_and_confirm(branch, &ep, &database, name, &sql)
            .await
        {
            Ok(()) => {}
            Err(RoleOutcome::NotApplied) => {
                // Gone from SQL meanwhile: the resync removes (or re-creates) its row.
                return match self
                    .computes
                    .admin_sql(&ep, &database, &exists_sql(name))
                    .await
                {
                    Ok(out) if out.trim().is_empty() => RoleOutcome::NotFound,
                    _ => RoleOutcome::NotApplied,
                };
            }
            Err(outcome) => return outcome,
        }
        info!(branch_id = %branch, endpoint_id = %ep, role = name, "login role password reset");
        RoleOutcome::Ok
    }

    /// Deletes a role (spec Part 3): refused for the owner, a reserved name, or while a live
    /// secret data key uses it; then under the endpoint's lock the role is dropped in SQL (its
    /// objects reassigned to the owner in every database) and its row deleted.
    pub async fn delete_role(&self, branch: BranchId, name: &str) -> RoleOutcome {
        if name == OWNER_ROLE {
            return reserved(name);
        }
        if let Err(refusal) = check_any_name(name) {
            return refusal;
        }
        if is_reserved_for(name, OWNER_ROLE) {
            return reserved(name);
        }
        let (ep, database) = match self.role_target(branch).await {
            Ok(target) => target,
            Err(outcome) => return outcome,
        };
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(branch, "locking the endpoint", &e),
        };
        match self.metadata.role_copies(&ep).await {
            Ok((rows, _))
                if rows
                    .iter()
                    .any(|r| r.origin != RoleOrigin::Owner && r.name == name) => {}
            Ok(_) => return RoleOutcome::NotFound,
            Err(e) => return failed(branch, "reading the roles", &e),
        }
        let keys: Vec<KeyRef> = match self.metadata.list_data_keys(branch).await {
            Ok(keys) => keys
                .into_iter()
                .filter(|k| k.kind == DataKeyKind::Secret && k.role == name)
                .map(|k| KeyRef {
                    id: k.id,
                    name: k.name,
                })
                .collect(),
            Err(e) => return failed(branch, "reading the data API keys", &e),
        };
        if !keys.is_empty() {
            return RoleOutcome::InUse(keys);
        }

        if let Err(e) = self.drop_role_sql(&ep, &database, name).await {
            warn!(branch_id = %branch, endpoint_id = %ep, role = name, "dropping the role; the row stays: {e:#}");
            return RoleOutcome::Failed(format!("dropping the role {name}: {e:#}"));
        }
        match self.metadata.delete_role_copy(&ep, name).await {
            // `false`: the row went meanwhile (a forwarded DROP ROLE); the role is gone either way.
            Ok(_) => {}
            Err(e) => return failed(branch, "deleting the role's row", &e),
        }
        self.publish_twice(&ep, &[name.to_string()]).await;
        info!(branch_id = %branch, endpoint_id = %ep, role = name, "login role deleted");
        RoleOutcome::Ok
    }

    /// After a row's write: reads the version, runs `sql` (forwarding is off), confirms the
    /// row and tells the proxies. A failed `sql` is `NotApplied`: the row stays `pending` and
    /// the next resync applies it. A confirm that finds the version moved leaves `pending` to
    /// the resync too; the role itself is done.
    async fn apply_and_confirm(
        &self,
        branch: BranchId,
        ep: &str,
        database: &str,
        name: &str,
        sql: &str,
    ) -> Result<(), RoleOutcome> {
        let version = match self.metadata.role_copies(ep).await {
            Ok((_, version)) => version,
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, role = name, "stored but not applied; the resync will: reading the version: {e}");
                return Err(RoleOutcome::NotApplied);
            }
        };
        if let Err(e) = self.computes.admin_sql(ep, database, sql).await {
            warn!(branch_id = %branch, endpoint_id = %ep, role = name, "stored but not applied; the resync will: {e:#}");
            return Err(RoleOutcome::NotApplied);
        }
        match self.metadata.confirm_role(ep, name, version).await {
            Ok(true) => {}
            Ok(false) => {
                info!(branch_id = %branch, endpoint_id = %ep, role = name, "roles changed meanwhile; the resync clears pending");
            }
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, role = name, "confirming the role; the resync clears pending: {e}");
            }
        }
        self.publish_twice(ep, &[name.to_string()]).await;
        Ok(())
    }

    /// The role gone from SQL: no new logins, its sessions ended, what it owns moved to the
    /// owner (and its grants dropped) in every connectable database, then the role itself.
    /// A role already missing is done. Every step is safe to run again.
    async fn drop_role_sql(&self, ep: &str, database: &str, name: &str) -> anyhow::Result<()> {
        let exists = self
            .computes
            .admin_sql(ep, database, &exists_sql(name))
            .await
            .context("checking whether the role exists")?;
        if exists.trim().is_empty() {
            return Ok(());
        }
        // Listed and checked first: a database psql can't be given must not leave the role
        // locked out below.
        let databases = self
            .computes
            .admin_sql(ep, database, DATABASES_SQL)
            .await
            .context("listing the databases")?;
        let databases = database_names(&databases)?;
        let (ident, literal) = (quote_ident(name), quote_literal(name));
        self.computes
            .admin_sql(
                ep,
                database,
                &format!(
                    "SET search_path = pg_catalog, pg_temp; ALTER ROLE {ident} NOLOGIN; \
                     SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = {literal};"
                ),
            )
            .await
            .context("ending the role's sessions")?;
        for db in &databases {
            self.computes
                .admin_sql(
                    ep,
                    db,
                    &format!("REASSIGN OWNED BY {ident} TO {OWNER_ROLE}; DROP OWNED BY {ident};"),
                )
                .await
                .with_context(|| format!("moving the role's objects in {db}"))?;
        }
        self.computes
            .admin_sql(ep, database, &format!("DROP ROLE IF EXISTS {ident};"))
            .await
            .context("dropping the role")?;
        Ok(())
    }

    /// One role RPC: `payload` decoded for `subject`, handled within `deadline`, as the reply.
    /// A reset past its deadline may have stored its row (the resync applies it), so it answers
    /// `not_applied`, as does a create that stored its row; a create cut short before that,
    /// and a delete, answer `failed` (a retry is safe). `not_applied` says what was stored.
    pub async fn role_rpc_reply(
        &self,
        subject: &str,
        payload: &[u8],
        deadline: Duration,
    ) -> RoleReply {
        let bad = |e: &dyn std::fmt::Display| {
            role_reply(RoleOutcome::Failed(format!("bad request: {e}")))
        };
        let (branch, outcome) = match subject {
            SUBJECT_ROLE_CREATE => match serde_json::from_slice::<RoleCreateRequest>(payload) {
                Ok(r) => {
                    let stored = AtomicBool::new(false);
                    let outcome = timeout(
                        deadline,
                        self.create_role_tracked(r.branch_id, &r.name, r.owner_level, &stored),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        if stored.load(Ordering::SeqCst) {
                            RoleOutcome::NotApplied
                        } else {
                            RoleOutcome::Failed(
                                "timed out before the role was stored; a retry is safe".into(),
                            )
                        }
                    });
                    (r.branch_id, outcome)
                }
                Err(e) => return bad(&e),
            },
            SUBJECT_ROLE_RESET | SUBJECT_ROLE_DELETE => {
                let r = match serde_json::from_slice::<RoleRequest>(payload) {
                    Ok(r) => r,
                    Err(e) => return bad(&e),
                };
                let outcome = if subject == SUBJECT_ROLE_RESET {
                    timeout(deadline, self.reset_role(r.branch_id, &r.name))
                        .await
                        .unwrap_or(RoleOutcome::NotApplied)
                } else {
                    timeout(deadline, self.delete_role(r.branch_id, &r.name))
                        .await
                        .unwrap_or_else(|_| {
                            RoleOutcome::Failed("timed out; a retry is safe".into())
                        })
                };
                (r.branch_id, outcome)
            }
            other => return bad(&format!("unknown subject {other}")),
        };
        if !matches!(outcome, RoleOutcome::Ok) {
            info!(branch_id = %branch, subject, ?outcome, "role request not done");
        }
        let not_applied = matches!(outcome, RoleOutcome::NotApplied);
        let mut reply = role_reply(outcome);
        if not_applied {
            let message = match subject {
                SUBJECT_ROLE_CREATE => ROLE_CREATE_NOT_APPLIED,
                SUBJECT_ROLE_DELETE => ROLE_DELETE_NOT_APPLIED,
                _ => RESET_NOT_APPLIED,
            };
            reply.error = Some(message.into());
        }
        reply
    }

    /// The endpoint of a ready `normal` branch and its recorded main database; otherwise the
    /// outcome to answer.
    async fn role_target(&self, branch: BranchId) -> Result<(String, String), RoleOutcome> {
        let ep = self.role_endpoint(branch).await?;
        match self.metadata.neon_databases(&ep).await {
            Ok(dbs) => match dbs.into_iter().next() {
                Some(db) => Ok((ep, db.name)),
                None => Err(RoleOutcome::NotReady),
            },
            Err(e) => Err(failed(branch, "reading the endpoint's database", &e)),
        }
    }

    /// The endpoint of a ready `normal` branch; otherwise the outcome to answer.
    async fn role_endpoint(&self, branch: BranchId) -> Result<String, RoleOutcome> {
        let record = self
            .metadata
            .get_branch(branch)
            .await
            .map_err(|e| failed(branch, "reading the branch", &e))?;
        record
            .filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
            .and_then(|b| b.endpoint_id)
            .ok_or(RoleOutcome::NotReady)
    }
}

fn exists(name: &str) -> RoleOutcome {
    RoleOutcome::Refused("role_exists", format!("a role named {name} already exists"))
}

/// Logs `err` and answers `Failed`.
fn failed(branch: BranchId, what: &str, err: &dyn std::fmt::Display) -> RoleOutcome {
    warn!(branch_id = %branch, "{what}: {err}");
    RoleOutcome::Failed(format!("{what}: {err}"))
}

#[cfg(test)]
mod tests {
    use cheladb_metadata::RoleLevel;
    use cheladb_nats::RESET_NOT_APPLIED;
    use cheladb_secrets::open_role_password;
    use uuid::Uuid;

    use super::*;
    use crate::OWNER_ROLE;
    use crate::fakes::{FakeLogin, FakeWorker, Journal, entries, position, ready_database, worker};

    /// The existence check, as the fake backend journals it.
    const EXISTS_SQL: &str = "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname";
    /// The first line of the create script.
    const SCRIPT: &str = "SET search_path = pg_catalog, pg_temp;\nSET ROLE \"postgres\";\n";

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn count(journal: &Journal, entry: &str) -> usize {
        entries(journal).iter().filter(|e| *e == entry).count()
    }

    /// Main of a ready database: `(branch, endpoint, database name)`; the journal starts empty.
    async fn ready(w: &FakeWorker, journal: &Journal) -> (BranchId, String, String) {
        let id = ready_database(w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.unwrap();
        let database = w.metadata.state().endpoints[&ep].databases[0].name.clone();
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        (main.id, ep, database)
    }

    /// Every SQL text that reached the compute.
    fn sql(w: &FakeWorker) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    /// `(verifier, sealed password, pending)` of a non-owner row.
    fn login(
        w: &FakeWorker,
        ep: &str,
        name: &str,
    ) -> Option<(Option<String>, Option<String>, bool)> {
        let state = w.metadata.state();
        let endpoint = &state.endpoints[ep];
        let row = endpoint.logins.get(name)?;
        Some((
            endpoint.verifier(name),
            row.password_enc.clone(),
            row.pending,
        ))
    }

    /// A row made in SQL, as the receiver or the resync records it.
    fn sql_row(w: &FakeWorker, ep: &str, name: &str, verifier: &str) {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(ep).unwrap();
        endpoint.set_verifier(name, Some(verifier));
        endpoint.logins.insert(
            name.into(),
            FakeLogin {
                origin: RoleOrigin::Sql,
                password_enc: None,
                pending: false,
                login: true,
                level: RoleLevel::Plain,
                api_owner_level: false,
            },
        );
    }

    fn owner_secret(w: &FakeWorker, branch: BranchId, ep: &str) -> (String, String) {
        let sealed = w.metadata.branch(branch).owner_password_enc.unwrap();
        let state = w.metadata.state();
        (state.endpoints[ep].verifier(OWNER_ROLE).unwrap(), sealed)
    }

    fn refused(code: &'static str) -> impl Fn(&RoleOutcome) -> bool {
        move |o| matches!(o, RoleOutcome::Refused(c, _) if *c == code)
    }

    // --- Create --------------------------------------------------------------------------

    #[tokio::test]
    async fn create_runs_as_the_owner_in_one_script() {
        let (w, journal) = worker();
        let (branch, ep, db) = ready(&w, &journal).await;

        assert_eq!(w.create_role(branch, "app", false).await, RoleOutcome::Ok);

        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        let verifier = verifier.unwrap();
        assert!(!pending, "confirmed");
        let password =
            open_role_password(&w.key, &branch.to_string(), "app", &sealed.unwrap()).unwrap();
        assert!(cheladb_neon::scram::verifier(&password).starts_with("SCRAM-SHA-256$"));
        let sent = sql(&w);
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(
            sent[0],
            "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app'"
        );
        assert_eq!(
            sent[1],
            format!("{SCRIPT}CREATE ROLE \"app\" LOGIN PASSWORD '{verifier}';\nRESET ROLE;")
        );
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(
                &journal,
                &format!("compute.sql {ep} {db} {EXISTS_SQL} = 'app'"),
            ),
            pos(&journal, "metadata.insert_api_role"),
            pos(&journal, &format!("compute.sql {ep} {db} SET search_path")),
            pos(&journal, "metadata.confirm_role"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // No spec push: the compute has the role already.
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
        let state = w.metadata.state();
        let row = &state.endpoints[&ep].logins["app"];
        assert_eq!(
            (row.origin, row.level, row.api_owner_level),
            (RoleOrigin::Api, RoleLevel::Plain, false)
        );
    }

    #[tokio::test]
    async fn create_owner_level_grants_neon_superuser() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(w.create_role(branch, "admin", true).await, RoleOutcome::Ok);

        let script = sql(&w).pop().unwrap();
        let verifier = login(&w, &ep, "admin").unwrap().0.unwrap();
        // The owner's attributes (N1), and the membership as cloud_admin.
        assert_eq!(
            script,
            format!(
                "{SCRIPT}CREATE ROLE \"admin\" LOGIN CREATEROLE CREATEDB BYPASSRLS REPLICATION \
                 PASSWORD '{verifier}';\nRESET ROLE;\nGRANT neon_superuser TO \"admin\";"
            )
        );
        let state = w.metadata.state();
        let row = &state.endpoints[&ep].logins["admin"];
        assert_eq!(
            (row.level, row.api_owner_level),
            (RoleLevel::OwnerLevel, true)
        );
    }

    #[tokio::test]
    async fn create_refuses_bad_and_reserved_names_without_sql() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = ready(&w, &journal).await;

        for (name, code) in [
            ("Bad-Name", "invalid_role_name"),
            ("My Role", "invalid_role_name"),
            ("", "invalid_role_name"),
            ("postgres", "reserved_role_name"),
            ("authenticator", "reserved_role_name"),
            ("pg_x", "reserved_role_name"),
        ] {
            let outcome = w.create_role(branch, name, false).await;
            assert!(refused(code)(&outcome), "{name:?}: {outcome:?}");
        }
        assert!(position(&journal, "compute.sql").is_none());
        assert!(position(&journal, "metadata.lock_endpoint").is_none());
    }

    #[tokio::test]
    async fn create_conflicts_on_an_existing_sql_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        w.computes.answer(EXISTS_SQL, "1\n");

        let outcome = w.create_role(branch, "app", false).await;

        assert!(refused("role_exists")(&outcome), "{outcome:?}");
        assert!(position(&journal, "metadata.insert_api_role").is_none());
        assert!(login(&w, &ep, "app").is_none(), "nothing adopted");
        assert_eq!(sql(&w).len(), 1);

        // A row already there (made in SQL, recorded by the receiver) is a conflict too.
        w.computes.answers.lock().unwrap().clear();
        sql_row(&w, &ep, "reporter", "SCRAM-SHA-256$x");
        let outcome = w.create_role(branch, "reporter", false).await;
        assert!(refused("role_exists")(&outcome), "{outcome:?}");
        assert_eq!(login(&w, &ep, "reporter").unwrap().1, None);
    }

    #[tokio::test]
    async fn create_at_the_cap_is_refused() {
        let (mut w, journal) = worker();
        w.roles.role_cap = 2;
        let (branch, ep, _db) = ready(&w, &journal).await;
        // A role made in SQL counts.
        sql_row(&w, &ep, "reporter", "SCRAM-SHA-256$x");
        assert_eq!(w.create_role(branch, "a", true).await, RoleOutcome::Ok);

        let outcome = w.create_role(branch, "b", true).await;

        assert!(
            matches!(&outcome, RoleOutcome::Refused("role_cap_reached", m) if m.contains('2')),
            "{outcome:?}"
        );
        assert!(login(&w, &ep, "b").is_none());
        assert!(!sql(&w).iter().any(|s| s.contains("CREATE ROLE \"b\"")));
    }

    #[tokio::test]
    async fn create_on_a_not_ready_branch_is_not_ready() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(
            w.create_role(BranchId::new(), "app", false).await,
            RoleOutcome::NotReady
        );
        let pending = w
            .metadata
            .insert_database("p", cheladb_metadata::DatabaseStatus::Pending);
        assert_eq!(
            w.create_role(BranchId::from(pending), "app", false).await,
            RoleOutcome::NotReady
        );
        assert!(position(&journal, "metadata.lock_endpoint").is_none());

        // The compute doesn't answer the existence check.
        *w.computes.sql_failures_left.lock().unwrap() = 1;
        assert_eq!(
            w.create_role(branch, "app", false).await,
            RoleOutcome::NotReady
        );
        assert!(login(&w, &ep, "app").is_none());

        w.metadata.state().fail = Some("get_branch");
        assert!(matches!(
            w.create_role(branch, "app", false).await,
            RoleOutcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn create_sql_failure_is_not_applied_and_pending() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("CREATE ROLE".into(), "connection reset".into()));

        assert_eq!(
            w.create_role(branch, "app", false).await,
            RoleOutcome::NotApplied
        );

        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        assert!(pending && verifier.is_some() && sealed.is_some());
        assert!(position(&journal, "metadata.confirm_role").is_none());
        assert!(position(&journal, "cache.password_updated").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn create_publishes_twice() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(w.create_role(branch, "app", false).await, RoleOutcome::Ok);

        let entry = format!("cache.password_updated {ep} app");
        assert_eq!(count(&journal, &entry), 1);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(count(&journal, &entry), 2, "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn a_publish_failure_doesnt_fail_the_create() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = ready(&w, &journal).await;
        *w.cache.fail.lock().unwrap() = Some("valkey down".into());
        assert_eq!(w.create_role(branch, "app", false).await, RoleOutcome::Ok);
        *w.cache.fail.lock().unwrap() = None;
        *w.cache.receivers.lock().unwrap() = 0;
        assert_eq!(w.create_role(branch, "other", false).await, RoleOutcome::Ok);
    }

    // --- Reset ---------------------------------------------------------------------------

    #[tokio::test]
    async fn reset_rotates_only_that_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        assert_eq!(w.create_role(branch, "app", false).await, RoleOutcome::Ok);
        let owner_before = owner_secret(&w, branch, &ep);
        let (old_verifier, old_sealed, _) = login(&w, &ep, "app").unwrap();
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();

        assert_eq!(w.reset_role(branch, "app").await, RoleOutcome::Ok);

        assert_eq!(owner_secret(&w, branch, &ep), owner_before);
        let (verifier, sealed, pending) = login(&w, &ep, "app").unwrap();
        let verifier = verifier.unwrap();
        assert_ne!(Some(&verifier), old_verifier.as_ref());
        assert_ne!(sealed, old_sealed);
        assert!(!pending);
        assert_eq!(
            sql(&w),
            [format!("ALTER ROLE \"app\" PASSWORD '{verifier}';")]
        );
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, "metadata.replace_role_secret"),
            pos(&journal, &format!("compute.sql {ep} app ALTER ROLE")),
            pos(&journal, "metadata.confirm_role"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reset_of_an_sql_role_makes_it_revealable() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        sql_row(&w, &ep, "My \"Role\"", "SCRAM-SHA-256$old");

        assert_eq!(w.reset_role(branch, "My \"Role\"").await, RoleOutcome::Ok);

        let (verifier, sealed, pending) = login(&w, &ep, "My \"Role\"").unwrap();
        assert!(!pending);
        let password =
            open_role_password(&w.key, &branch.to_string(), "My \"Role\"", &sealed.unwrap())
                .unwrap();
        let verifier = verifier.unwrap();
        assert_ne!(verifier, "SCRAM-SHA-256$old");
        let params = verifier.split('$').nth(1).unwrap();
        let (iterations, salt) = params.split_once(':').unwrap();
        use base64::Engine;
        let salt = base64::engine::general_purpose::STANDARD
            .decode(salt)
            .unwrap();
        assert_eq!(
            cheladb_neon::scram::verifier_with_salt(&password, &salt, iterations.parse().unwrap()),
            verifier
        );
        assert_eq!(
            sql(&w),
            [format!(
                "ALTER ROLE \"My \"\"Role\"\"\" PASSWORD '{verifier}';"
            )]
        );
        assert_eq!(
            w.metadata.state().endpoints[&ep].logins["My \"Role\""].origin,
            RoleOrigin::Sql
        );
    }

    #[tokio::test]
    async fn reset_refusals() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;

        assert_eq!(w.reset_role(branch, "ghost").await, RoleOutcome::NotFound);
        assert_eq!(
            w.reset_role(branch, "authenticator").await,
            RoleOutcome::NotFound
        );
        assert!(refused("invalid_role_name")(
            &w.reset_role(branch, "").await
        ));
        assert!(refused("invalid_role_name")(
            &w.reset_role(branch, &"a".repeat(64)).await
        ));
        assert_eq!(
            w.reset_role(BranchId::new(), "app").await,
            RoleOutcome::NotReady
        );
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));

        // The role is gone from SQL: not found, and the stored password stays for the resync.
        sql_row(&w, &ep, "dropped", "SCRAM-SHA-256$x");
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "ALTER ROLE".into(),
            "role \"dropped\" does not exist".into(),
        ));
        assert_eq!(w.reset_role(branch, "dropped").await, RoleOutcome::NotFound);

        // Still there, but the compute failed: not applied, pending.
        w.computes.answer(EXISTS_SQL, "1\n");
        assert_eq!(
            w.reset_role(branch, "dropped").await,
            RoleOutcome::NotApplied
        );
        assert!(login(&w, &ep, "dropped").unwrap().2);
    }

    #[tokio::test]
    async fn reset_of_the_owner_name_uses_the_owner_reset() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let before = owner_secret(&w, branch, &ep);

        assert_eq!(w.reset_role(branch, OWNER_ROLE).await, RoleOutcome::Ok);

        assert!(position(&journal, "metadata.replace_owner_secret").is_some());
        assert!(position(&journal, "metadata.replace_role_secret").is_none());
        let after = owner_secret(&w, branch, &ep);
        assert_ne!(after.0, before.0);
        assert_ne!(after.1, before.1);

        *w.pusher.fail.lock().unwrap() = Some("compute_ctl said 500".into());
        assert_eq!(
            w.reset_role(branch, OWNER_ROLE).await,
            RoleOutcome::NotApplied
        );
        assert_eq!(
            w.reset_role(BranchId::new(), OWNER_ROLE).await,
            RoleOutcome::NotReady
        );
    }

    #[test]
    fn role_reply_codes() {
        let key = KeyRef {
            id: Uuid::nil(),
            name: "k".into(),
        };
        let ok = role_reply(RoleOutcome::Ok);
        assert!(ok.ok);
        assert_eq!((ok.code, ok.error), (None, None));
        let cases = [
            (
                RoleOutcome::Refused("invalid_role_name", "bad".into()),
                "invalid_role_name",
            ),
            (
                RoleOutcome::Refused("role_cap_reached", "full".into()),
                "role_cap_reached",
            ),
            (RoleOutcome::InUse(vec![key.clone()]), "role_in_use"),
            (RoleOutcome::NotReady, "branch_not_ready"),
            (RoleOutcome::NotFound, "not_found"),
            (RoleOutcome::NotApplied, "not_applied"),
            (RoleOutcome::Failed("boom".into()), "failed"),
        ];
        for (outcome, code) in cases {
            let in_use = matches!(outcome, RoleOutcome::InUse(_));
            let reply = role_reply(outcome);
            assert!(!reply.ok, "{code}");
            assert_eq!(reply.code.as_deref(), Some(code));
            assert!(reply.error.is_some(), "{code}");
            assert_eq!(
                reply.keys,
                if in_use { vec![key.clone()] } else { vec![] },
                "{code}"
            );
        }
        assert_eq!(
            role_reply(RoleOutcome::NotApplied).error.as_deref(),
            Some(RESET_NOT_APPLIED)
        );
    }

    // --- Delete --------------------------------------------------------------------------

    const EXISTS_APP: &str = "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app'";
    const TERMINATE: &str = "SET search_path = pg_catalog, pg_temp; ALTER ROLE \"app\" NOLOGIN; \
        SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = 'app';";
    const DATABASES: &str = "SET search_path = pg_catalog, pg_temp; SELECT \
        encode(convert_to(datname, 'UTF8'), 'hex') FROM pg_database WHERE datallowconn AND NOT \
        datistemplate ORDER BY datname;";
    const REASSIGN: &str = "REASSIGN OWNED BY \"app\" TO postgres; DROP OWNED BY \"app\";";
    const DROP: &str = "DROP ROLE IF EXISTS \"app\";";

    /// The databases as the listing prints them: one hex-encoded name per line.
    fn databases(names: &[&str]) -> String {
        names
            .iter()
            .map(|n| format!("{}\n", hex::encode(n)))
            .collect()
    }

    /// `(database, sql)` of every SQL text that reached the compute.
    fn sql_on(w: &FakeWorker) -> Vec<(String, String)> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .map(|(_, db, s)| (db.clone(), s.clone()))
            .collect()
    }

    /// `app` created (plain) on a ready branch, and the journal and SQL log emptied.
    async fn with_app(w: &FakeWorker, journal: &Journal) -> (BranchId, String, String) {
        let (branch, ep, db) = ready(w, journal).await;
        assert_eq!(w.create_role(branch, "app", false).await, RoleOutcome::Ok);
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        (branch, ep, db)
    }

    fn secret(branch: BranchId, role: &str, name: &str) -> cheladb_metadata::NewDataKey {
        cheladb_metadata::NewDataKey {
            id: Uuid::new_v4(),
            branch_id: branch,
            kind: cheladb_types::data_key::DataKeyKind::Secret,
            role: role.to_string(),
            name: name.to_string(),
            prefix: "sk_test".to_string(),
            sha256: [0; 32],
            sealed: None,
        }
    }

    #[tokio::test]
    async fn delete_runs_in_order_across_every_database() {
        let (w, journal) = worker();
        let (branch, ep, db) = with_app(&w, &journal).await;
        w.computes.answer(EXISTS_APP, "1\n");
        w.computes
            .answer("FROM pg_database", &databases(&["app", "other"]));

        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);

        let on = |d: &str, s: &str| (d.to_string(), s.to_string());
        assert_eq!(
            sql_on(&w),
            [
                on(&db, EXISTS_APP),
                on(&db, DATABASES),
                on(&db, TERMINATE),
                on("app", REASSIGN),
                on("other", REASSIGN),
                on(&db, DROP),
            ]
        );
        let order = [
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, &format!("compute.sql {ep} {db} DROP ROLE")),
            pos(&journal, "metadata.delete_role_copy"),
            pos(&journal, &format!("cache.password_updated {ep} app")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(login(&w, &ep, "app").is_none());
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
        assert_eq!(
            count(&journal, &format!("cache.password_updated_later {ep} app")),
            1
        );
    }

    #[tokio::test]
    async fn delete_accepts_quoted_names() {
        let (w, journal) = worker();
        let (branch, ep, db) = ready(&w, &journal).await;
        let name = "My \"Role\"";
        sql_row(&w, &ep, name, "SCRAM-SHA-256$x");
        w.computes.answer(
            "SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'My \"Role\"'",
            "1\n",
        );
        w.computes
            .answer("FROM pg_database", &databases(&["app", "my-app"]));

        assert_eq!(w.delete_role(branch, name).await, RoleOutcome::Ok);

        let ident = "\"My \"\"Role\"\"\"";
        let on = |d: &str, s: String| (d.to_string(), s);
        let reassign = format!("REASSIGN OWNED BY {ident} TO postgres; DROP OWNED BY {ident};");
        assert_eq!(
            sql_on(&w)[2..],
            [
                on(
                    &db,
                    format!(
                        "SET search_path = pg_catalog, pg_temp; ALTER ROLE {ident} NOLOGIN; \
                         SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = \
                         'My \"Role\"';"
                    )
                ),
                on("app", reassign.clone()),
                on("my-app", reassign),
                on(&db, format!("DROP ROLE IF EXISTS {ident};")),
            ]
        );
        assert!(login(&w, &ep, name).is_none());
        // The hyphenated database reached psql as a quoted conninfo.
        assert_eq!(
            cheladb_neon::backend::admin_psql_command("my-app")
                .unwrap()
                .iter()
                .filter(|a| a.as_str() == "dbname='my-app'")
                .count(),
            1
        );
    }

    // T4: the row gone between the check and the delete (a forwarded DROP ROLE removed it):
    // the role is gone either way, so the delete succeeded.
    #[tokio::test]
    async fn delete_whose_row_vanished_meanwhile_succeeds() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        w.metadata.state().role_row_vanishes = true;

        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert!(!w.metadata.state().endpoints[&ep].logins.contains_key("app"));
    }

    #[tokio::test]
    async fn delete_refuses_the_owner() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = with_app(&w, &journal).await;

        for (name, code) in [
            (OWNER_ROLE, "reserved_role_name"),
            ("authenticator", "reserved_role_name"),
            ("pg_x", "reserved_role_name"),
            ("", "invalid_role_name"),
        ] {
            let outcome = w.delete_role(branch, name).await;
            assert!(refused(code)(&outcome), "{name:?}: {outcome:?}");
        }
        assert!(position(&journal, "metadata.lock_endpoint").is_none());
        assert_eq!(w.delete_role(branch, "ghost").await, RoleOutcome::NotFound);
        assert_eq!(
            w.delete_role(BranchId::new(), "app").await,
            RoleOutcome::NotReady
        );
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));
    }

    #[tokio::test]
    async fn delete_refused_while_a_live_secret_key_uses_the_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        let live = secret(branch, "app", "backend");
        let revoked = secret(branch, "app", "old");
        let other = secret(branch, "reporting", "reports");
        {
            let mut state = w.metadata.state();
            state
                .data_keys
                .extend([live.clone(), revoked.clone(), other]);
            state.revoked_keys.insert(revoked.id);
        }

        assert_eq!(
            w.delete_role(branch, "app").await,
            RoleOutcome::InUse(vec![KeyRef {
                id: live.id,
                name: "backend".into()
            }])
        );
        assert!(sql(&w).is_empty(), "{:?}", sql(&w));
        assert!(login(&w, &ep, "app").is_some());

        w.metadata.state().revoked_keys.insert(live.id);
        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert!(login(&w, &ep, "app").is_none());
    }

    #[tokio::test]
    async fn delete_failure_keeps_the_row() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        w.computes.answer(EXISTS_APP, "1\n");
        w.computes.answer("FROM pg_database", &databases(&["app"]));
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("REASSIGN".into(), "objects depend on it".into()));

        let outcome = w.delete_role(branch, "app").await;

        assert!(matches!(outcome, RoleOutcome::Failed(_)), "{outcome:?}");
        assert!(login(&w, &ep, "app").is_some());
        assert!(position(&journal, "metadata.delete_role_copy").is_none());
        assert!(!sql(&w).iter().any(|s| s == DROP));

        *w.computes.fail_sql_containing.lock().unwrap() = None;
        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);
        assert!(login(&w, &ep, "app").is_none());
    }

    #[tokio::test]
    async fn delete_refuses_an_unpassable_database_before_touching_the_role() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        let long = "d".repeat(64);
        w.computes.answer(EXISTS_APP, "1\n");
        w.computes
            .answer("FROM pg_database", &databases(&["app", long.as_str()]));

        let outcome = w.delete_role(branch, "app").await;

        assert!(
            matches!(outcome, RoleOutcome::Failed(ref m) if m.contains(&long)),
            "{outcome:?}"
        );
        assert!(login(&w, &ep, "app").is_some());
        assert_eq!(sql(&w), [EXISTS_APP, DATABASES]);
    }

    #[tokio::test]
    async fn delete_of_a_role_missing_in_sql_still_removes_the_row() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;

        assert_eq!(w.delete_role(branch, "app").await, RoleOutcome::Ok);

        assert_eq!(sql(&w), [EXISTS_APP]);
        assert!(login(&w, &ep, "app").is_none());
        assert!(position(&journal, &format!("cache.password_updated {ep} app")).is_some());
    }

    // --- RPCs ----------------------------------------------------------------------------

    #[tokio::test]
    async fn role_rpcs_reply() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let deadline = Duration::from_secs(5);
        let request = |name: &str| {
            serde_json::to_vec(&RoleRequest {
                branch_id: branch,
                name: name.into(),
            })
            .unwrap()
        };
        let create = serde_json::to_vec(&RoleCreateRequest {
            branch_id: branch,
            name: "app".into(),
            owner_level: true,
        })
        .unwrap();

        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        assert!(sql(&w).last().unwrap().contains("GRANT neon_superuser"));
        let (_, sealed, _) = login(&w, &ep, "app").unwrap();
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_RESET, &request("app"), deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        assert_ne!(login(&w, &ep, "app").unwrap().1, sealed);
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_DELETE, &request("app"), deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        assert!(login(&w, &ep, "app").is_none());

        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_DELETE, &request("app"), deadline)
            .await;
        assert_eq!(reply.code.as_deref(), Some("not_found"));
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
            .await;
        assert!(reply.ok, "{reply:?}");
        let reply = w
            .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
            .await;
        assert_eq!(reply.code.as_deref(), Some("role_exists"), "{reply:?}");

        for (subject, payload) in [
            (SUBJECT_ROLE_CREATE, b"{".as_slice()),
            (SUBJECT_ROLE_RESET, b"{}".as_slice()),
            (SUBJECT_ROLE_DELETE, b"not json".as_slice()),
        ] {
            let reply = w.role_rpc_reply(subject, payload, deadline).await;
            assert!(!reply.ok);
            assert_eq!(reply.code.as_deref(), Some("failed"), "{subject}");
        }
    }

    #[tokio::test]
    async fn role_rpcs_reply_with_owner_level() {
        let (w, journal) = worker();
        let (branch, ep, _db) = ready(&w, &journal).await;
        let deadline = Duration::from_secs(5);
        for (name, owner_level) in [("reader", false), ("boss", true)] {
            let create = serde_json::to_vec(&RoleCreateRequest {
                branch_id: branch,
                name: name.into(),
                owner_level,
            })
            .unwrap();
            w.computes.sql.lock().unwrap().clear();

            let reply = w
                .role_rpc_reply(SUBJECT_ROLE_CREATE, &create, deadline)
                .await;

            assert!(reply.ok, "{name}: {reply:?}");
            assert_eq!(reply.code, None, "{name}");
            let script = sql(&w).concat();
            assert_eq!(
                script.contains(&format!("GRANT neon_superuser TO \"{name}\"")),
                owner_level,
                "{name}: {script}"
            );
            let state = w.metadata.state();
            let row = &state.endpoints[&ep].logins[name];
            assert_eq!(row.api_owner_level, owner_level, "{name}");
            let level = if owner_level {
                RoleLevel::OwnerLevel
            } else {
                RoleLevel::Plain
            };
            assert_eq!(row.level, level, "{name}");
        }
    }

    // A stuck operation answers within its deadline: create (before its row) and delete say
    // failed (a retry is safe), reset says not applied.
    #[tokio::test(start_paused = true)]
    async fn role_rpcs_past_their_deadline_reply() {
        let (w, journal) = worker();
        let (branch, ep, _db) = with_app(&w, &journal).await;
        let _held = w.metadata.lock_endpoint(&ep).await.unwrap();
        let request = serde_json::to_vec(&RoleRequest {
            branch_id: branch,
            name: "app".into(),
        })
        .unwrap();
        let create = serde_json::to_vec(&RoleCreateRequest {
            branch_id: branch,
            name: "other".into(),
            owner_level: true,
        })
        .unwrap();
        let deadline = Duration::from_secs(1);

        for (subject, payload, code) in [
            (SUBJECT_ROLE_CREATE, &create, "failed"),
            (SUBJECT_ROLE_RESET, &request, "not_applied"),
            (SUBJECT_ROLE_DELETE, &request, "failed"),
        ] {
            let reply = w.role_rpc_reply(subject, payload, deadline).await;
            assert_eq!(reply.code.as_deref(), Some(code), "{subject}: {reply:?}");
        }
        assert!(login(&w, &ep, "other").is_none());
    }

    #[tokio::test]
    async fn not_applied_wording_fits_the_operation() {
        let (w, journal) = worker();
        let (branch, _ep, _db) = with_app(&w, &journal).await;
        w.computes.answer(EXISTS_APP, "1\n");
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("ROLE \"".into(), "connection reset".into()));
        let deadline = Duration::from_secs(5);
        let request = serde_json::to_vec(&RoleRequest {
            branch_id: branch,
            name: "app".into(),
        })
        .unwrap();
        let create = serde_json::to_vec(&RoleCreateRequest {
            branch_id: branch,
            name: "other".into(),
            owner_level: false,
        })
        .unwrap();

        for (subject, payload, message) in [
            (SUBJECT_ROLE_CREATE, &create, ROLE_CREATE_NOT_APPLIED),
            (SUBJECT_ROLE_RESET, &request, RESET_NOT_APPLIED),
        ] {
            let reply = w.role_rpc_reply(subject, payload, deadline).await;
            assert_eq!(reply.code.as_deref(), Some("not_applied"), "{subject}");
            assert_eq!(reply.error.as_deref(), Some(message), "{subject}");
        }
    }
}
