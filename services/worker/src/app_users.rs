//! App users jobs: enabling and disabling a project's app users (its identity pool
//! `app-<project>`), syncing users from the pool (the source of truth) into the users table on
//! every live branch of the auth database, reconcile, and the location check the orchestrator
//! asks for before an enable.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use chelabase_identity::{Identity, IdentityError, Missing, Pool, User};
use chelabase_metadata::{
    AppUsersStatus, BranchKind, BranchRecord, BranchStatus, OnDelete, ProjectAuth,
};
use chelabase_nats::{AppUsersCheckReply, AppUsersCheckRequest, AppUsersJob, AppUsersSyncJob};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::{open_role_password, seal_role_password};
use chelabase_types::{BranchId, DatabaseId, ProjectId};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::app_users_sql::{
    USERS_OUTDATED_SQL, USERS_ROWS_SQL, USERS_SYNC_ROLE, USERS_TEARDOWN_SQL, claim_helpers_sql,
    gone_call_sql, upsert_call_sql, users_bootstrap_sql, users_table_exists_sql,
};
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{OWNER_ROLE, Worker, credentials};

/// Users per `list_users` page during reconcile.
const PAGE: u32 = 100;
/// An `enabling`/`disabling` row or a `deleting` project untouched this long is run again by
/// the periodic pass (I6): the worker has no publisher, so running the job is the republish.
pub const STALE_TRANSITION: Duration = Duration::from_secs(5 * 60);
/// Sessions revoked or expired this long ago are purged.
pub const SESSION_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// One row of the users table as `chelabase.app_users_rows()` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UserRow {
    pub id: Uuid,
    pub email: String,
    pub email_verified: bool,
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub banned_at: Option<DateTime<Utc>>,
}

/// The project's app-user pool. Only ever built from the project's own id, so a drop can't
/// name another project's pool.
pub(crate) fn app_pool(project: ProjectId) -> Pool {
    Pool::App(project.as_uuid())
}

/// The users whose row on one branch differs from the pool, in order: users without a row or
/// with a stale email, verified flag or ban (the pool's order), then rows whose user is gone
/// (`delete`: every such row; `mark`: those not marked deleted yet). Users whose id isn't a
/// uuid (a pool made before ids were uuids) are skipped (P15).
pub fn plan_app_users_sync(users: &[User], rows: &[UserRow], on_delete: OnDelete) -> Vec<Uuid> {
    let mut plan = Vec::new();
    let mut known = HashSet::new();
    for user in users {
        let Ok(id) = Uuid::parse_str(&user.id) else {
            continue;
        };
        known.insert(id);
        let current = rows
            .iter()
            .find(|r| r.id == id)
            .is_some_and(|r| {
                r.email == user.email
                    && r.email_verified == user.email_verified
                    && r.banned_at.is_some() == user.banned
            });
        if !current {
            plan.push(id);
        }
    }
    for row in rows {
        if known.contains(&row.id) {
            continue;
        }
        let pending = match on_delete {
            OnDelete::Delete => true,
            OnDelete::Mark => row.deleted_at.is_none(),
        };
        if pending {
            plan.push(row.id);
        }
    }
    plan
}

/// `chelabase.app_users_rows()`'s jsonb array, from psql's output.
fn parse_rows(output: &str) -> anyhow::Result<Vec<UserRow>> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('['))
        .context("no rows array in the output")?;
    serde_json::from_str(line).context("reading the users table's rows")
}

/// Drops the project's pool. Idempotent: a pool already gone is done.
pub(crate) async fn drop_pool(identity: &Identity, project: ProjectId) -> Result<(), String> {
    match identity.drop_pool(&app_pool(project)).await {
        Ok(()) | Err(IdentityError::NotFound(Missing::Pool)) => Ok(()),
        Err(e) => Err(format!("dropping the app-user pool: {e}")),
    }
}

/// Enabled or still enabling: the users table is (being) installed.
fn active(auth: &ProjectAuth) -> bool {
    matches!(
        auth.status,
        AppUsersStatus::Enabled | AppUsersStatus::Enabling
    )
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The identity client; its absence fails the job, which is retried (P2).
    pub(crate) fn identity(&self) -> Result<&Identity, String> {
        self.identity.as_ref().ok_or_else(|| {
            "the identity service isn't configured (CHELABASE_IDENTITY_URL and CHELABASE_IDENTITY_TOKEN_FILE)"
                .to_string()
        })
    }

    /// A database's live branches with their endpoints: ready `normal` branches only (P16).
    pub(crate) async fn live_branches(
        &self,
        db: DatabaseId,
    ) -> Result<Vec<(BranchRecord, String)>, String> {
        Ok(self
            .metadata
            .list_branches(db)
            .await
            .map_err(|e| format!("listing the branches: {e}"))?
            .into_iter()
            .filter(|b| b.kind == BranchKind::Normal && b.status == BranchStatus::Ready)
            .filter_map(|b| b.endpoint_id.clone().map(|ep| (b, ep)))
            .collect())
    }

    /// The users bootstrap on one branch of the auth database: the branch's
    /// `chelabase_auth_sync` password (the stored one when it was made for this Postgres
    /// database; otherwise a new one, claimed before the script sets it, so a concurrent
    /// bootstrap of the branch sets the same one), then the script. The script carries the
    /// password's SCRAM verifier, never the password (N7).
    pub(crate) async fn bootstrap_users_on(
        &self,
        branch: &BranchRecord,
        ep: &str,
        auth: &ProjectAuth,
    ) -> anyhow::Result<()> {
        let aad = branch.id.to_string();
        let open = |sealed: &str| {
            open_role_password(&self.key, &aad, USERS_SYNC_ROLE, sealed)
                .inspect_err(|e| {
                    warn!(branch_id = %branch.id, "the stored sync password doesn't open; a new one is set: {e}");
                })
                .ok()
        };
        let stored = self
            .metadata
            .branch_auth_sync(branch.id)
            .await
            .context("reading the sync password")?
            .filter(|row| row.pg_database == auth.pg_database);
        let password = match stored {
            Some(row) => match open(&row.password_sealed) {
                Some(password) => password,
                // Unreadable (another secrets key): replaced only while it still holds that
                // value (M3), so a concurrent replacement wins once.
                None => {
                    let password = credentials::generate_password();
                    let sealed = seal_role_password(&self.key, &aad, USERS_SYNC_ROLE, &password);
                    let kept = self
                        .metadata
                        .swap_branch_auth_sync(
                            branch.id,
                            &auth.pg_database,
                            &row.password_sealed,
                            &sealed,
                        )
                        .await
                        .context("storing the sync password")?;
                    if kept == sealed {
                        password
                    } else {
                        open_role_password(&self.key, &aad, USERS_SYNC_ROLE, &kept)
                            .context("opening the sync password a concurrent bootstrap stored")?
                    }
                }
            },
            None => {
                let password = credentials::generate_password();
                let sealed = seal_role_password(&self.key, &aad, USERS_SYNC_ROLE, &password);
                let kept = self
                    .metadata
                    .claim_branch_auth_sync(branch.id, &auth.pg_database, &sealed)
                    .await
                    .context("storing the sync password")?;
                if kept == sealed {
                    password
                } else {
                    // A concurrent bootstrap stored its own first: set that one.
                    open_role_password(&self.key, &aad, USERS_SYNC_ROLE, &kept)
                        .context("opening the sync password a concurrent bootstrap stored")?
                }
            }
        };
        let verifier = chelabase_neon::scram::verifier(&password);
        let script =
            users_bootstrap_sql(&auth.schema_name, &auth.table_name, &verifier, OWNER_ROLE);
        self.computes
            .admin_sql(ep, &auth.pg_database, &script)
            .await
            .map(drop)
    }

    /// The branch hook (create, reset, restore, compute re-creation): the users bootstrap when
    /// `database`'s app users are enabled or enabling and `branch` is a `normal` branch.
    /// The project, when it ran.
    pub(crate) async fn bootstrap_users_for(
        &self,
        branch: BranchId,
        database: DatabaseId,
        ep: &str,
    ) -> anyhow::Result<Option<ProjectId>> {
        let auth = self
            .metadata
            .app_auth_for_database(database)
            .await
            .context("reading the app users settings")?;
        let Some(auth) = auth.filter(active) else {
            return Ok(None);
        };
        let record = self
            .metadata
            .get_branch(branch)
            .await
            .context("reading the branch")?;
        let Some(record) = record.filter(|b| b.kind == BranchKind::Normal) else {
            return Ok(None);
        };
        self.bootstrap_users_on(&record, ep, &auth).await?;
        Ok(Some(auth.project_id))
    }

    pub async fn handle_app_users_job(&self, job: &AppUsersJob) -> Result<(), String> {
        match job {
            AppUsersJob::Enable { project_id } => self.enable_app_users(*project_id).await,
            AppUsersJob::Disable { project_id } => self.disable_app_users(*project_id).await,
            AppUsersJob::Reconcile { project_id } => self.reconcile_app_users(*project_id).await,
        }
    }

    /// Enables app users on an `enabling` row. Any failure ends it `failed` and acks (P27);
    /// only a missing identity client or metadata trouble retries.
    async fn enable_app_users(&self, project: ProjectId) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(|a| a.status == AppUsersStatus::Enabling) else {
            return Ok(());
        };
        let identity = self.identity()?;
        let outcome = self.enable_steps(&auth, identity).await;
        match &outcome {
            Ok(()) => info!(%project, "app users enabled"),
            Err(e) => warn!(%project, "enabling app users failed: {e}"),
        }
        self.metadata
            .finish_enable_app_users(project, outcome)
            .await
            .map(drop)
            .map_err(|e| e.to_string())
    }

    async fn enable_steps(&self, auth: &ProjectAuth, identity: &Identity) -> Result<(), String> {
        let project = auth.project_id;
        // Idempotent: an enable run again keeps the pool and its users.
        identity
            .create_pool(&app_pool(project))
            .await
            .map_err(|e| format!("creating the app-user pool: {e}"))?;
        // The project's delete removes the row, then drops the pool: if the row went while this
        // create was on its way, the pool it made is this job's to drop. Should that fail the
        // pool stays, empty: nothing can have signed up, and no row names it.
        let current = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        if current.is_none() {
            drop_pool(identity, project).await?;
            return Err("the app users settings were deleted while enabling".to_string());
        }
        for (branch, ep) in self.live_branches(auth.database_id).await? {
            self.bootstrap_users_on(&branch, &ep, auth)
                .await
                .map_err(|e| format!("bootstrapping app users on branch {}: {e:#}", branch.name))?;
        }
        // The claim helpers in every other database the data API serves in the project (the
        // users bootstrap installed them in the auth Postgres database).
        let databases = self
            .metadata
            .project_databases(project)
            .await
            .map_err(|e| format!("listing the project's databases: {e}"))?;
        let helpers = claim_helpers_sql();
        for (db, _, _) in databases {
            for (branch, ep) in self.live_branches(db).await? {
                let served = self
                    .metadata
                    .served_databases(branch.id)
                    .await
                    .map_err(|e| format!("listing the served databases: {e}"))?;
                for database in served {
                    if db == auth.database_id && database.name == auth.pg_database {
                        continue;
                    }
                    self.computes
                        .admin_sql(&ep, &database.name, &helpers)
                        .await
                        .map_err(|e| {
                            format!(
                                "installing the claim helpers on branch {}, database {}: {e:#}",
                                branch.name, database.name
                            )
                        })?;
                }
            }
        }
        Ok(())
    }

    /// Disables app users on a `disabling` row: the teardown on every live branch, the
    /// sessions revoked, the pool dropped (idempotent), then the row. Any failure is retried.
    /// The row goes last, so a new enable can't start until the drop is done. Disables can run
    /// twice at once (the consumer, the periodic pass, a repeated request): the row is read
    /// again before the drop and only a `disabling` row is deleted, so a slow copy leaves an
    /// enable that started after the other copy finished alone.
    async fn disable_app_users(&self, project: ProjectId) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(|a| a.status == AppUsersStatus::Disabling) else {
            return Ok(());
        };
        let identity = self.identity()?;
        for (branch, ep) in self.live_branches(auth.database_id).await? {
            self.computes
                .admin_sql(&ep, &auth.pg_database, USERS_TEARDOWN_SQL)
                .await
                .map_err(|e| format!("removing app users from branch {}: {e:#}", branch.name))?;
        }
        self.metadata
            .revoke_project_sessions(project)
            .await
            .map_err(|e| format!("revoking the app sessions: {e}"))?;
        let current = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        if current.is_none_or(|a| a.status != AppUsersStatus::Disabling) {
            return Ok(());
        }
        drop_pool(identity, project).await?;
        let finished = self
            .metadata
            .finish_disable_app_users(project)
            .await
            .map_err(|e| e.to_string())?;
        if finished {
            info!(%project, "app users disabled");
        } else {
            // The residual window: a finished copy plus a new enable took the row between the
            // re-read and the drop, so the drop may have hit the enable's pool. Reconcile
            // records a pool gone from an `enabled` row (`pool_gone`).
            warn!(%project, "the app users row changed under the disable after its re-read; its pool drop may have hit a newer enable's pool");
        }
        Ok(())
    }

    /// An `enabled` project whose pool is gone from the identity service (the disable/enable
    /// window above, or a pool dropped by hand): no retry can bring it back, so the job is
    /// acked with the error on the row, where `GET …/app-users` and Studio show it, and in
    /// the log. A pool back in place clears it on the next reconcile pass.
    async fn pool_gone(&self, project: ProjectId) -> Result<(), String> {
        let error = format!(
            "the app-user pool {} is gone from the identity service; its users can't sign in or be synced",
            app_pool(project)
        );
        error!(%project, "{error}");
        self.metadata
            .set_app_users_error(project, Some(&error))
            .await
            .map_err(|e| e.to_string())
    }

    /// Applies one user's state in the pool to the users table on every live branch of the
    /// auth database (state-based, so duplicates and reordering are harmless). A branch that
    /// fails is logged and left to reconcile; only identity or metadata trouble retries.
    pub async fn handle_app_users_sync(&self, job: &AppUsersSyncJob) -> Result<(), String> {
        self.sync_user(job, None).await
    }

    /// [`Worker::handle_app_users_sync`], on the live branches in `only` when given.
    async fn sync_user(
        &self,
        job: &AppUsersSyncJob,
        only: Option<&HashSet<BranchId>>,
    ) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(job.project_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(active) else {
            return Ok(());
        };
        let identity = self.identity()?;
        let user_id = job.user_id;
        // Read in this project's pool only: a user of another pool is not found here.
        let user = match identity
            .get_user(&app_pool(job.project_id), &user_id.to_string())
            .await
        {
            Ok(user) => Some(user),
            Err(IdentityError::NotFound(Missing::User)) => None,
            // Before the enable made the pool there is nothing to sync yet; once enabled a
            // missing pool is trouble, never "every user is gone".
            Err(IdentityError::NotFound(Missing::Pool)) => {
                return if auth.status == AppUsersStatus::Enabling {
                    Ok(())
                } else {
                    self.pool_gone(job.project_id).await
                };
            }
            Err(e) => return Err(format!("reading user {user_id}: {e}")),
        };
        // Banned or gone (found by reconcile, or a job for an admin delete; N1): their app
        // sessions end with it. The ban or delete already ended their identity sessions.
        if user.as_ref().is_none_or(|u| u.banned) {
            self.revoke_user(job.project_id, user_id).await?;
        }
        let gone = gone_call_sql(user_id, auth.on_delete == OnDelete::Mark);
        for (branch, ep) in self.live_branches(auth.database_id).await? {
            if only.is_some_and(|only| !only.contains(&branch.id)) {
                continue;
            }
            let sql = match &user {
                Some(u) => {
                    let data = job
                        .data
                        .as_ref()
                        .filter(|_| job.origin_branch_id != Some(branch.id));
                    upsert_call_sql(user_id, &u.email, u.email_verified, u.banned, u.created_at, data)
                }
                None => gone.clone(),
            };
            if let Err(e) = self.computes.admin_sql(&ep, &auth.pg_database, &sql).await {
                warn!(branch = %branch.id, project = %job.project_id, %user_id, "syncing an app user; reconcile retries: {e:#}");
            }
        }
        Ok(())
    }

    /// A banned or deleted user: their app sessions revoked. (The worker may not revoke
    /// identity sessions; a ban or delete in the pool already did.)
    async fn revoke_user(&self, project: ProjectId, user: Uuid) -> Result<(), String> {
        self.metadata
            .revoke_user_sessions(project, user, None, None)
            .await
            .map(drop)
            .map_err(|e| format!("revoking the user's sessions: {e}"))
    }

    /// Whether `schema.table` is free on every live branch of the database (enable's
    /// precondition).
    pub async fn check_app_users_location(&self, req: &AppUsersCheckRequest) -> AppUsersCheckReply {
        let unavailable = |error: String| AppUsersCheckReply {
            ok: false,
            code: Some("unavailable".to_string()),
            error: Some(error),
        };
        let branches = match self.live_branches(req.database_id).await {
            Ok(branches) => branches,
            Err(e) => return unavailable(e),
        };
        let sql = users_table_exists_sql(&req.schema, &req.table);
        for (branch, ep) in branches {
            match self.computes.admin_sql(&ep, &req.pg_database, &sql).await {
                Ok(out) if out.lines().any(|l| l.trim() == "t") => {
                    return AppUsersCheckReply {
                        ok: false,
                        code: Some("app_users_table_exists".to_string()),
                        error: Some(format!(
                            "{}.{} already exists on branch {}",
                            req.schema, req.table, branch.name
                        )),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(branch = %branch.id, "checking the app users location: {e:#}");
                    return unavailable(format!("checking branch {}: {e:#}", branch.name));
                }
            }
        }
        AppUsersCheckReply {
            ok: true,
            code: None,
            error: None,
        }
    }

    /// Compares every live branch's users table with the pool's users and syncs each
    /// difference inline. A branch whose rows can't be read is logged and skipped, and one
    /// still on an older users bootstrap is brought up to date first ([`USERS_OUTDATED_SQL`]). A
    /// user a concurrent change shifted out of the paged listing is re-read by its sync before
    /// any delete, so it is never removed by mistake.
    ///
    /// Every banned user's app sessions are revoked on every pass, matching row or not: a
    /// sign-in that raced the ban leaves a session no row difference shows.
    ///
    /// What a pass can't put right is on the row's `error` (`GET …/app-users`, Studio): a
    /// pool gone from the identity service ([`Worker::pool_gone`]), or the first branch it
    /// couldn't upgrade (a `banned_at` column of another type, say). A pass that upgrades
    /// every branch clears it.
    pub async fn reconcile_app_users(&self, project: ProjectId) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(active) else {
            return Ok(());
        };
        let identity = self.identity()?;
        let pool = app_pool(project);
        let mut users = Vec::new();
        let mut offset = 0u32;
        loop {
            let page = match identity.list_users(&pool, PAGE, offset).await {
                Ok(page) => page,
                // Before the enable made the pool there is nothing to reconcile yet.
                Err(IdentityError::NotFound(Missing::Pool)) => {
                    return if auth.status == AppUsersStatus::Enabling {
                        Ok(())
                    } else {
                        self.pool_gone(project).await
                    };
                }
                Err(e) => return Err(format!("listing the pool's users: {e}")),
            };
            let n = page.users.len() as u32;
            users.extend(page.users);
            offset += n;
            if n == 0 || u64::from(offset) >= page.total {
                break;
            }
        }
        let listed: HashSet<Uuid> = users
            .iter()
            .filter_map(|u| Uuid::parse_str(&u.id).ok())
            .collect();
        for user in users.iter().filter(|u| u.banned) {
            if let Ok(id) = Uuid::parse_str(&user.id) {
                self.revoke_user(project, id).await?;
            }
        }
        // Listed users that differ somewhere are synced everywhere; a gone user only on the
        // branches whose own plan deletes them (M1).
        let mut seen = HashSet::new();
        let mut stale = Vec::new();
        let mut gone: Vec<(Uuid, HashSet<BranchId>)> = Vec::new();
        // The first branch the pass couldn't upgrade, for the row.
        let mut not_upgraded: Option<String> = None;
        for (branch, ep) in self.live_branches(auth.database_id).await? {
            if let Err(e) = self.upgrade_users_on(&branch, &ep, &auth).await {
                warn!(branch = %branch.id, %project, "upgrading the users machinery; skipped: {e:#}");
                not_upgraded.get_or_insert_with(|| {
                    format!(
                        "upgrading the users machinery on branch {}: {e:#}",
                        branch.name
                    )
                });
                continue;
            }
            let rows = self
                .computes
                .admin_sql(&ep, &auth.pg_database, USERS_ROWS_SQL)
                .await
                .and_then(|out| parse_rows(&out));
            let rows = match rows {
                Ok(rows) => rows,
                Err(e) => {
                    warn!(branch = %branch.id, %project, "reading the users table; skipped: {e:#}");
                    continue;
                }
            };
            let (present, mut missing): (Vec<Uuid>, Vec<Uuid>) =
                plan_app_users_sync(&users, &rows, auth.on_delete)
                    .into_iter()
                    .partition(|id| listed.contains(id));
            // N4: a listing that would remove most of the table (an empty pool, say one
            // recreated by mistake) isn't trusted for this branch's deletes; the admin API still
            // deletes one by one. In `mark` mode only the rows not marked yet count.
            let live_rows = match auth.on_delete {
                OnDelete::Mark => rows.iter().filter(|r| r.deleted_at.is_none()).count(),
                OnDelete::Delete => rows.len(),
            };
            if !missing.is_empty() && missing.len() * 2 > live_rows {
                warn!(branch = %branch.id, %project, gone = missing.len(), rows = live_rows, "reconcile would remove most of the users table; its deletes are skipped");
                missing.clear();
            }
            for id in present {
                if seen.insert(id) {
                    stale.push(id);
                }
            }
            for id in missing {
                match gone.iter_mut().find(|(g, _)| *g == id) {
                    Some((_, branches)) => {
                        branches.insert(branch.id);
                    }
                    None => gone.push((id, HashSet::from([branch.id]))),
                }
            }
        }
        if not_upgraded.is_some() || auth.error.is_some() {
            self.metadata
                .set_app_users_error(project, not_upgraded.as_deref())
                .await
                .map_err(|e| format!("recording the reconcile outcome: {e}"))?;
        }
        let job = |user_id| AppUsersSyncJob {
            project_id: project,
            user_id,
            data: None,
            origin_branch_id: None,
        };
        for user_id in stale {
            self.handle_app_users_sync(&job(user_id)).await?;
        }
        for (user_id, branches) in gone {
            self.sync_user(&job(user_id), Some(&branches)).await?;
        }
        Ok(())
    }

    /// The users bootstrap again on a branch that's still on an older version of it. Branches
    /// otherwise run it only when created or when their compute is re-created.
    async fn upgrade_users_on(
        &self,
        branch: &BranchRecord,
        ep: &str,
        auth: &ProjectAuth,
    ) -> anyhow::Result<()> {
        let out = self
            .computes
            .admin_sql(ep, &auth.pg_database, USERS_OUTDATED_SQL)
            .await?;
        if out.lines().any(|l| l.trim() == "t") {
            info!(branch = %branch.id, project = %auth.project_id, "upgrading the users machinery");
            self.bootstrap_users_on(branch, ep, auth).await?;
        }
        Ok(())
    }

    /// The periodic pass (I6): every enabled project reconciled, old sessions purged, and
    /// stale enables, disables and project deletes run again.
    pub async fn app_users_reconcile_once(&self) {
        match self.metadata.enabled_app_projects().await {
            Ok(projects) => {
                for auth in projects {
                    if let Err(e) = self.reconcile_app_users(auth.project_id).await {
                        warn!(project = %auth.project_id, "reconciling app users: {e}");
                    }
                }
            }
            Err(e) => warn!("listing the projects with app users: {e}"),
        }
        match self.metadata.purge_app_sessions(SESSION_RETENTION).await {
            Ok(0) => {}
            Ok(purged) => info!(purged, "purged old app sessions"),
            Err(e) => warn!("purging old app sessions: {e}"),
        }
        match self
            .metadata
            .app_users_in_transition(STALE_TRANSITION)
            .await
        {
            Ok(rows) => {
                for auth in rows {
                    let project_id = auth.project_id;
                    let job = match auth.status {
                        AppUsersStatus::Enabling => AppUsersJob::Enable { project_id },
                        AppUsersStatus::Disabling => AppUsersJob::Disable { project_id },
                        _ => continue,
                    };
                    if let Err(e) = self.handle_app_users_job(&job).await {
                        warn!(project = %project_id, "running a stale app users job again: {e}");
                    }
                }
            }
            Err(e) => warn!("listing stale app users jobs: {e}"),
        }
        match self.metadata.deleting_projects(STALE_TRANSITION).await {
            Ok(projects) => {
                for project in projects {
                    if let Err(e) = self.handle_project_delete(project).await {
                        warn!(%project, "deleting a project again: {e}");
                    }
                }
            }
            Err(e) => warn!("listing stale project deletes: {e}"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use chelabase_identity::mock::FakeIdentity;
    use chelabase_identity::{IdentityError, Missing, Pool};
    use chelabase_metadata::{AppUsersStatus, BranchKind, DataApiState, ProjectStatus};
    use chelabase_secrets::{open_role_password, seal_role_password};
    use chelabase_types::DatabaseId;
    use serde_json::json;

    use super::*;
    use crate::app_users_sql::{
        USERS_ROWS_SQL, USERS_TEARDOWN_SQL, claim_helpers_sql, gone_call_sql, upsert_call_sql,
        users_bootstrap_sql,
    };
    use crate::fakes::{
        FakeDbRow, FakeWorker, Journal, entries, ready_branch, ready_database, worker,
    };

    pub(crate) const BOOTSTRAP: &str = "-- chelabase:users-bootstrap";
    const HELPERS: &str = "-- chelabase:claim-helpers";

    pub(crate) struct Setup {
        pub w: FakeWorker,
        pub journal: Journal,
        pub fake: FakeIdentity,
        pub project: ProjectId,
        pub db: DatabaseId,
        pub main: BranchRecord,
        pub dev: BranchRecord,
    }

    impl Setup {
        /// The project's app-user pool.
        pub fn pool(&self) -> Pool {
            Pool::App(self.project.as_uuid())
        }

        /// The project's pool exists in the identity service.
        pub fn with_pool(&self) {
            self.fake.with_pool(&self.pool());
        }

        /// A verified user in the project's pool.
        pub fn add_user(&self, email: &str) -> Uuid {
            self.add_user_in(&self.pool(), email)
        }

        pub fn add_user_in(&self, pool: &Pool, email: &str) -> Uuid {
            self.fake
                .with_user(pool, email, "Passw0rd!")
                .parse()
                .unwrap()
        }

        /// Whether the project's pool exists.
        pub async fn pool_exists(&self) -> bool {
            match self.fake.client().list_users(&self.pool(), 1, 0).await {
                Ok(_) => true,
                Err(IdentityError::NotFound(Missing::Pool)) => false,
                Err(e) => panic!("{e}"),
            }
        }

        /// The user's upsert as the sync writes it, with the pool's `created_at`.
        pub async fn upsert(
            &self,
            user: Uuid,
            email: &str,
            data: Option<&serde_json::Value>,
        ) -> String {
            let u = self
                .fake
                .client()
                .get_user(&self.pool(), &user.to_string())
                .await
                .unwrap();
            upsert_call_sql(user, email, u.email_verified, u.banned, u.created_at, data)
        }

        /// The identity calls on the project's pool, `"METHOD path"` with the pool elided.
        pub fn pool_calls(&self) -> Vec<String> {
            let prefix = format!("/internal/v1/pools/{}", self.pool());
            self.fake
                .calls()
                .iter()
                .filter_map(|c| {
                    let (method, path) = c.split_once(' ')?;
                    Some(format!("{method} {}", path.strip_prefix(&prefix)?))
                })
                .collect()
        }

        pub fn set_on_delete(&self, on_delete: OnDelete) {
            self.w
                .metadata
                .state()
                .project_auth
                .get_mut(&self.project)
                .unwrap()
                .on_delete = on_delete;
        }
    }

    /// A project whose database `app` (main and `dev`, both ready) is its auth database, with
    /// a `project_auth` row in `status`; the identity service is the fake. The journal and the
    /// SQL log start empty.
    pub(crate) async fn setup(status: AppUsersStatus) -> Setup {
        let (mut w, journal) = worker();
        let fake = FakeIdentity::start().await;
        w.identity = Some(fake.client());
        let project = w.metadata.insert_project();
        let db = ready_database(&w).await;
        w.metadata.move_to_project(db, project);
        let main = w.metadata.main(db);
        let dev = ready_branch(&w, db, "dev", main.id).await;
        let dev = w.metadata.branch(dev);
        w.metadata.insert_auth(project, db, status);
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        Setup {
            w,
            journal,
            fake,
            project,
            db,
            main,
            dev,
        }
    }

    /// Every script run on `ep` in `database` whose first line starts with `prefix`.
    pub(crate) fn scripts(w: &FakeWorker, ep: &str, database: &str, prefix: &str) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, d, s)| e == ep && d == database && s.starts_with(prefix))
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    pub(crate) fn ep(b: &BranchRecord) -> String {
        b.endpoint_id.clone().unwrap()
    }

    /// The branch's stored sync password, opened.
    pub(crate) fn sync_password(w: &FakeWorker, branch: &BranchRecord) -> (String, String) {
        let row = w.metadata.state().auth_sync[&branch.id].clone();
        let pw = open_role_password(
            &w.key,
            &branch.id.to_string(),
            "chelabase_auth_sync",
            &row.password_sealed,
        )
        .unwrap();
        (row.pg_database, pw)
    }

    /// `scripts` is exactly one users bootstrap for `public.users` (owner `postgres`) whose
    /// sync role password is a SCRAM verifier of `password`, never `password` itself (N7).
    pub(crate) fn assert_bootstraps(scripts: &[String], password: &str) {
        let [script] = scripts else {
            panic!("one bootstrap expected: {scripts:?}")
        };
        assert!(!script.contains(password), "the plain password is in the script");
        let verifier = script
            .split("PASSWORD '")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("a PASSWORD literal");
        let params = verifier.split('$').nth(1).expect("SCRAM-SHA-256$<iterations>:<salt>$…");
        let (iterations, salt) = params.split_once(':').unwrap();
        let salt = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, salt).unwrap();
        assert_eq!(
            chelabase_neon::scram::verifier_with_salt(password, &salt, iterations.parse().unwrap()),
            verifier,
            "the verifier is the stored password's"
        );
        assert_eq!(
            script,
            &users_bootstrap_sql("public", "users", verifier, "postgres")
        );
    }

    fn count(journal: &Journal, entry: &str) -> usize {
        entries(journal).iter().filter(|e| *e == entry).count()
    }

    /// Everything logged on this thread while the guard lives, as plain text (no colours).
    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Logs;
        fn make_writer(&'a self) -> Logs {
            self.clone()
        }
    }

    impl std::fmt::Display for Logs {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&String::from_utf8_lossy(&self.0.lock().unwrap()))
        }
    }

    impl Logs {
        /// `#[tokio::test]` runs on one thread, so a thread-local default sees the whole test.
        fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
            let logs = Logs::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .finish();
            (logs, tracing::subscriber::set_default(subscriber))
        }

        /// The lines at `level` containing every one of `needles`.
        fn lines_with(&self, level: &str, needles: &[&str]) -> usize {
            self.to_string()
                .lines()
                .filter(|l| l.contains(level) && needles.iter().all(|n| l.contains(n)))
                .count()
        }
    }

    fn enable(s: &Setup) -> AppUsersJob {
        AppUsersJob::Enable {
            project_id: s.project,
        }
    }

    fn sync_job(s: &Setup, user: Uuid) -> AppUsersSyncJob {
        AppUsersSyncJob {
            project_id: s.project,
            user_id: user,
            data: None,
            origin_branch_id: None,
        }
    }

    fn created() -> DateTime<Utc> {
        "2026-10-03T10:00:00Z".parse().unwrap()
    }

    // --- Enable -----------------------------------------------------------------------

    /// AC 1: enabling creates pool `app-<project>`; enabling again is a no-op on the pool.
    #[tokio::test]
    async fn enable_creates_pool_idempotent() {
        let s = setup(AppUsersStatus::Enabling).await;
        assert!(!s.pool_exists().await);

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Enabled, "{:?}", auth.error);
        assert!(s.pool_exists().await);
        let writes = |s: &Setup| -> Vec<String> {
            s.pool_calls()
                .into_iter()
                .filter(|c| !c.starts_with("GET "))
                .collect()
        };
        assert_eq!(writes(&s), ["POST "]);
        // No org is recorded any more.
        assert_eq!(auth.zitadel_org_id, None);

        // Enabling again (a redelivery, or the periodic pass) keeps the pool and its users.
        let user = s.add_user("ada@example.com");
        s.w.metadata
            .state()
            .project_auth
            .get_mut(&s.project)
            .unwrap()
            .status = AppUsersStatus::Enabling;
        s.w.handle_app_users_job(&enable(&s)).await.unwrap();
        assert_eq!(
            s.w.metadata.auth(s.project).unwrap().status,
            AppUsersStatus::Enabled
        );
        assert_eq!(writes(&s), ["POST ", "POST "]);
        s.fake
            .client()
            .get_user(&s.pool(), &user.to_string())
            .await
            .unwrap();
        // Another project's pool is never named.
        assert!(
            s.fake
                .calls()
                .iter()
                .all(|c| c.contains(&s.pool().to_string())),
            "{:?}",
            s.fake.calls()
        );
    }

    #[tokio::test]
    async fn enable_creates_pool_bootstraps_every_branch_and_helpers() {
        let s = setup(AppUsersStatus::Enabling).await;
        // Another served database on the auth database's main branch, and a second database
        // of the project.
        s.w.metadata
            .state()
            .db_rows
            .get_mut(&s.main.id)
            .unwrap()
            .push(FakeDbRow::new("extra", DataApiState::On, "api"));
        s.w.metadata
            .state()
            .db_rows
            .get_mut(&s.main.id)
            .unwrap()
            .push(FakeDbRow::new("quiet", DataApiState::Off, "public"));
        let other = ready_database(&s.w).await;
        s.w.metadata.move_to_project(other, s.project);
        let other = s.w.metadata.main(other);
        s.w.computes.sql.lock().unwrap().clear();

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        assert!(s.pool_exists().await);
        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Enabled, "{:?}", auth.error);
        for b in [&s.main, &s.dev] {
            let (pg_database, pw) = sync_password(&s.w, b);
            assert_eq!(pg_database, "app");
            assert_bootstraps(&scripts(&s.w, &ep(b), "app", BOOTSTRAP), &pw);
            // The users bootstrap installs the helpers in the auth Postgres database itself.
            assert!(scripts(&s.w, &ep(b), "app", HELPERS).is_empty());
        }
        assert_ne!(
            sync_password(&s.w, &s.main).1,
            sync_password(&s.w, &s.dev).1
        );
        // Every other served database of the project gets the helpers.
        assert_eq!(
            scripts(&s.w, &ep(&s.main), "extra", HELPERS),
            [claim_helpers_sql()]
        );
        assert!(scripts(&s.w, &ep(&s.main), "quiet", HELPERS).is_empty());
        assert_eq!(
            scripts(&s.w, &ep(&other), "app", HELPERS),
            [claim_helpers_sql()]
        );
        assert!(scripts(&s.w, &ep(&other), "app", BOOTSTRAP).is_empty());
        // The password is stored before the script that sets it runs.
        let journal = entries(&s.journal);
        let stored = journal
            .iter()
            .position(|e| e == "metadata.claim_branch_auth_sync")
            .unwrap();
        let ran = journal
            .iter()
            .position(|e| e.starts_with(&format!("compute.sql {} app {BOOTSTRAP}", ep(&s.main))))
            .unwrap();
        assert!(stored < ran, "{journal:?}");
    }

    #[tokio::test]
    async fn enable_failure_sets_failed_and_acks() {
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.computes.fail_when(BOOTSTRAP);
        s.w.handle_app_users_job(&enable(&s)).await.unwrap();
        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Failed);
        let error = auth.error.unwrap();
        assert!(error.contains("bootstrapping app users"), "{error}");
        assert!(error.contains("injected failure"), "{error}");

        // The pool create failing is final too.
        let s = setup(AppUsersStatus::Enabling).await;
        s.fake.set_down(true);
        s.w.handle_app_users_job(&enable(&s)).await.unwrap();
        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Failed);
        assert!(scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP).is_empty());

        // Without the identity client the job is retried and the row stays enabling.
        let mut s = setup(AppUsersStatus::Enabling).await;
        s.w.identity = None;
        s.w.handle_app_users_job(&enable(&s)).await.unwrap_err();
        assert_eq!(
            s.w.metadata.auth(s.project).unwrap().status,
            AppUsersStatus::Enabling
        );

        // A row that isn't enabling (or none) is left alone.
        for status in [
            AppUsersStatus::Enabled,
            AppUsersStatus::Failed,
            AppUsersStatus::Disabling,
        ] {
            let s = setup(status).await;
            s.w.handle_app_users_job(&enable(&s)).await.unwrap();
            assert_eq!(s.w.metadata.auth(s.project).unwrap().status, status);
            assert!(s.w.computes.sql.lock().unwrap().is_empty());
            assert!(s.fake.calls().is_empty());
        }
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.handle_app_users_job(&AppUsersJob::Enable {
            project_id: ProjectId::new(),
        })
        .await
        .unwrap();
        assert!(s.fake.calls().is_empty());
    }

    #[tokio::test]
    async fn enable_reuses_the_stored_password() {
        let s = setup(AppUsersStatus::Enabling).await;
        let kept = "KeptPasswordKeptPasswordKeptPass";
        s.w.metadata.state().auth_sync.insert(
            s.main.id,
            chelabase_metadata::BranchAuthSync {
                branch_id: s.main.id,
                pg_database: "app".into(),
                password_sealed: seal_role_password(
                    &s.w.key,
                    &s.main.id.to_string(),
                    "chelabase_auth_sync",
                    kept,
                ),
            },
        );
        // A row for another Postgres database isn't reused.
        let stale = "StalePasswordStalePasswordStaleP";
        s.w.metadata.state().auth_sync.insert(
            s.dev.id,
            chelabase_metadata::BranchAuthSync {
                branch_id: s.dev.id,
                pg_database: "elsewhere".into(),
                password_sealed: seal_role_password(
                    &s.w.key,
                    &s.dev.id.to_string(),
                    "chelabase_auth_sync",
                    stale,
                ),
            },
        );

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        assert_bootstraps(&scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP), kept);
        assert_eq!(
            sync_password(&s.w, &s.main),
            ("app".to_string(), kept.to_string())
        );
        let (pg_database, pw) = sync_password(&s.w, &s.dev);
        assert_eq!(pg_database, "app");
        assert_ne!(pw, stale);
        assert_bootstraps(&scripts(&s.w, &ep(&s.dev), "app", BOOTSTRAP), &pw);
        assert_eq!(count(&s.journal, "metadata.claim_branch_auth_sync"), 1);
        assert_eq!(count(&s.journal, "metadata.set_branch_auth_sync"), 0);
    }

    /// T8 race: a bootstrap that loses the claim to a concurrent one sets the winner's
    /// password, so the role and the stored password agree.
    #[tokio::test]
    async fn bootstrap_uses_the_password_a_racing_bootstrap_stored() {
        let s = setup(AppUsersStatus::Enabling).await;
        let winner = "WinnerPasswordWinnerPasswordWinn";
        s.w.metadata.state().auth_sync_racer = Some((
            "app".into(),
            seal_role_password(&s.w.key, &s.main.id.to_string(), "chelabase_auth_sync", winner),
        ));

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        assert_eq!(
            sync_password(&s.w, &s.main),
            ("app".to_string(), winner.to_string())
        );
        assert_bootstraps(&scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP), winner);
    }

    /// A stored password that no longer opens (another secrets key) is replaced, not claimed.
    #[tokio::test]
    async fn bootstrap_replaces_a_stored_password_that_doesnt_open() {
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.metadata.state().auth_sync.insert(
            s.main.id,
            chelabase_metadata::BranchAuthSync {
                branch_id: s.main.id,
                pg_database: "app".into(),
                password_sealed: "v1:garbage".into(),
            },
        );

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        let (pg_database, pw) = sync_password(&s.w, &s.main);
        assert_eq!(pg_database, "app");
        assert_bootstraps(&scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP), &pw);
        assert_eq!(count(&s.journal, "metadata.swap_branch_auth_sync"), 1);
        assert_eq!(count(&s.journal, "metadata.set_branch_auth_sync"), 0);
    }

    /// M3: replacing an unreadable password is a compare-and-swap too: a bootstrap that loses
    /// it to a concurrent one sets the winner's password.
    #[tokio::test]
    async fn replacing_an_unreadable_password_yields_to_a_racer() {
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.metadata.state().auth_sync.insert(
            s.main.id,
            chelabase_metadata::BranchAuthSync {
                branch_id: s.main.id,
                pg_database: "app".into(),
                password_sealed: "v1:garbage".into(),
            },
        );
        let winner = "WinnerPasswordWinnerPasswordWinn";
        s.w.metadata.state().auth_sync_racer = Some((
            "app".into(),
            seal_role_password(&s.w.key, &s.main.id.to_string(), "chelabase_auth_sync", winner),
        ));

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        assert_eq!(
            sync_password(&s.w, &s.main),
            ("app".to_string(), winner.to_string())
        );
        assert_bootstraps(&scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP), winner);
    }

    // --- Disable ----------------------------------------------------------------------

    #[tokio::test]
    async fn disable_tears_down_revokes_drops_pool_and_row() {
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_pool();
        let session = s.w.metadata.add_session(s.project, Uuid::new_v4(), "zs1");

        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap();

        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "-- chelabase:users-teardown"),
                [USERS_TEARDOWN_SQL]
            );
        }
        {
            let state = s.w.metadata.state();
            assert!(
                state
                    .app_sessions
                    .iter()
                    .find(|x| x.id == session)
                    .unwrap()
                    .revoked_at
                    .is_some()
            );
            assert!(!state.project_auth.contains_key(&s.project));
        }
        assert!(!s.pool_exists().await);
        assert!(s.pool_calls().contains(&"DELETE ".to_string()));
        let journal = entries(&s.journal);
        let revoked = journal
            .iter()
            .position(|e| e == "metadata.revoke_project_sessions")
            .unwrap();
        let deleted = journal
            .iter()
            .position(|e| e == "metadata.finish_disable_app_users")
            .unwrap();
        assert!(revoked < deleted);

        // A teardown failure is retried, and the row and the pool stay.
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_pool();
        s.w.computes.fail_when("-- chelabase:users-teardown");
        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap_err();
        assert_eq!(
            s.w.metadata.auth(s.project).unwrap().status,
            AppUsersStatus::Disabling
        );
        assert!(s.pool_exists().await);

        // The drop is idempotent: a pool already gone (a redelivery after the drop) still
        // finishes the disable.
        let s = setup(AppUsersStatus::Disabling).await;
        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap();
        assert!(s.w.metadata.auth(s.project).is_none());

        // The identity service down: retried, the row stays.
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_pool();
        s.fake.set_down(true);
        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap_err();
        assert_eq!(
            s.w.metadata.auth(s.project).unwrap().status,
            AppUsersStatus::Disabling
        );
        s.fake.set_down(false);
        assert!(s.pool_exists().await);

        // Only a disabling row is torn down.
        let s = setup(AppUsersStatus::Enabled).await;
        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());
        assert!(s.w.metadata.auth(s.project).is_some());
    }

    /// S1: a duplicate disable already past its status check leaves a new enable alone: the
    /// row is read again before the drop, and only a `disabling` row is deleted.
    #[tokio::test]
    async fn a_stalled_disable_leaves_a_new_enable_alone() {
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_pool();
        // Between the first read and the drop, another copy finished and an enable began.
        s.w.metadata.state().after_auth_read = Some(Some(AppUsersStatus::Enabling));

        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap();

        assert_eq!(
            s.w.metadata.auth(s.project).map(|a| a.status),
            Some(AppUsersStatus::Enabling)
        );
        assert!(s.pool_exists().await, "the new enable's pool was dropped");
    }

    /// Re-audit S1: the residual window is between the re-read and the drop. A copy overtaken
    /// there drops the new enable's pool and `finish_disable_app_users` finds no `disabling`
    /// row; the disable says so at warn, naming the project, instead of "app users disabled".
    #[tokio::test]
    async fn a_disable_overtaken_after_its_reread_warns() {
        let (logs, _guard) = Logs::capture();
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_pool();
        // The first read and the re-read both see `disabling`; the row changes just after.
        {
            let mut state = s.w.metadata.state();
            state.after_auth_read = Some(Some(AppUsersStatus::Enabling));
            state.after_auth_read_skip = 1;
        }

        s.w.handle_app_users_job(&AppUsersJob::Disable {
            project_id: s.project,
        })
        .await
        .unwrap();

        assert_eq!(
            s.w.metadata.auth(s.project).map(|a| a.status),
            Some(AppUsersStatus::Enabling),
            "the enable's row is left alone"
        );
        assert!(
            !s.pool_exists().await,
            "the window: the enable's pool is dropped"
        );
        assert_eq!(
            logs.lines_with(
                "WARN",
                &[&s.project.to_string(), "changed under the disable"]
            ),
            1,
            "{logs}"
        );
        assert_eq!(logs.lines_with("INFO", &["app users disabled"]), 0, "{logs}");
    }

    /// Re-audit S2 (the auditor's `adv_enabled_without_a_pool_is_silent_and_permanent`,
    /// inverted): an `enabled` project whose pool is gone has the error recorded on its row,
    /// at error level in the log, and its jobs acked rather than redelivered for ever. A pool
    /// back in place clears it on the next pass.
    #[tokio::test]
    async fn an_enabled_project_whose_pool_is_gone_records_the_error() {
        let (logs, _guard) = Logs::capture();
        let s = setup(AppUsersStatus::Enabled).await;
        s.w.computes.answer("chelabase.app_users_rows()", "[]");

        s.w.app_users_reconcile_once().await;
        s.w.app_users_reconcile_once().await;

        assert!(!s.pool_exists().await);
        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Enabled);
        let error = auth.error.expect("the gone pool is on the row");
        assert!(error.contains(&s.pool().to_string()), "{error}");
        assert_eq!(
            logs.lines_with("ERROR", &[&s.project.to_string(), &s.pool().to_string()]),
            2,
            "{logs}"
        );
        // The jobs are acked: nothing a redelivery could do.
        s.w.handle_app_users_job(&AppUsersJob::Reconcile {
            project_id: s.project,
        })
        .await
        .unwrap();
        s.w.handle_app_users_sync(&sync_job(&s, Uuid::new_v4()))
            .await
            .unwrap();
        assert_eq!(s.w.metadata.auth(s.project).unwrap().error, Some(error));

        // The pool back (an operator's repair): the next pass clears the error.
        s.with_pool();
        s.w.reconcile_app_users(s.project).await.unwrap();
        assert_eq!(s.w.metadata.auth(s.project).unwrap().error, None);
    }

    /// Re-audit S2: a branch reconcile can't upgrade (a `banned_at` collision, say) is on the
    /// row, naming the branch, until a pass upgrades it.
    #[tokio::test]
    async fn a_branch_reconcile_cannot_upgrade_is_on_the_row() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.w.computes.answer("chelabase.app_users_rows()", "[]");
        s.w.computes.answer_at(&ep(&s.dev), "to_regprocedure", "t\n");
        *s.w.computes.fail_sql_containing.lock().unwrap() = Some((
            BOOTSTRAP.into(),
            "column \"banned_at\" of relation \"users\" already exists".into(),
        ));

        s.w.reconcile_app_users(s.project).await.unwrap();

        let error =
            s.w.metadata
                .auth(s.project)
                .unwrap()
                .error
                .expect("the skipped branch is on the row");
        assert!(
            error.contains("dev") && error.contains("upgrading") && error.contains("banned_at"),
            "{error}"
        );

        // The upgrade goes through: cleared.
        *s.w.computes.fail_sql_containing.lock().unwrap() = None;
        s.w.reconcile_app_users(s.project).await.unwrap();
        assert_eq!(s.w.metadata.auth(s.project).unwrap().error, None);
        // The failed script isn't on the fake's log; the one that went through is.
        assert_eq!(scripts(&s.w, &ep(&s.dev), "app", BOOTSTRAP).len(), 1);
    }

    /// S2: an enable whose row went while it made the pool (the project's delete ran) drops
    /// what it made, so no pool outlives the project.
    #[tokio::test]
    async fn an_enable_racing_a_delete_drops_the_pool_it_made() {
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.metadata.state().after_auth_read = Some(None);

        s.w.handle_app_users_job(&enable(&s)).await.unwrap();

        assert!(!s.pool_exists().await, "the pool outlives the project");
        assert!(scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP).is_empty());
    }

    // --- Sync -------------------------------------------------------------------------

    #[tokio::test]
    async fn sync_upserts_on_every_branch_with_data_except_origin() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        let data = json!({"name": "Ada"});
        let job = AppUsersSyncJob {
            data: Some(data.clone()),
            origin_branch_id: Some(s.dev.id),
            ..sync_job(&s, user)
        };

        s.w.handle_app_users_sync(&job).await.unwrap();

        assert_eq!(
            scripts(&s.w, &ep(&s.main), "app", "BEGIN;"),
            [s.upsert(user, "ada@example.com", Some(&data)).await]
        );
        assert_eq!(
            scripts(&s.w, &ep(&s.dev), "app", "BEGIN;"),
            [s.upsert(user, "ada@example.com", None).await]
        );

        // An enabling project is synced too (the enable may be filling other branches).
        let s = setup(AppUsersStatus::Enabling).await;
        s.with_pool();
        let user = s.add_user("bob@example.com");
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap();
        assert_eq!(scripts(&s.w, &ep(&s.main), "app", "BEGIN;").len(), 1);
    }

    #[tokio::test]
    async fn sync_gone_deletes_or_marks() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let gone = Uuid::new_v4();
        s.w.handle_app_users_sync(&sync_job(&s, gone))
            .await
            .unwrap();
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [gone_call_sql(gone, false)]
            );
        }
        s.w.computes.sql.lock().unwrap().clear();
        s.set_on_delete(OnDelete::Mark);
        s.w.handle_app_users_sync(&sync_job(&s, gone))
            .await
            .unwrap();
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [gone_call_sql(gone, true)]
            );
        }
    }

    /// A banned user's app sessions are revoked (the ban already ended their identity
    /// sessions); the row still follows the pool.
    #[tokio::test]
    async fn sync_banned_revokes_sessions() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        let other = s.add_user("bob@example.com");
        s.fake.ban(&s.pool(), &user.to_string());
        let a = s.w.metadata.add_session(s.project, user, "s1");
        let b = s.w.metadata.add_session(s.project, user, "s2");
        let c = s.w.metadata.add_session(s.project, other, "s3");

        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap();

        let revoked = |id: Uuid| {
            s.w.metadata
                .state()
                .app_sessions
                .iter()
                .find(|x| x.id == id)
                .unwrap()
                .revoked_at
                .is_some()
        };
        assert!(revoked(a) && revoked(b) && !revoked(c));
        // Once, not per branch; and the row is marked banned on every branch (AC 3).
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 1);
        let upsert = s.upsert(user, "ada@example.com", None).await;
        assert!(upsert.contains(", true, true, "), "{upsert}");
        for b in [&s.main, &s.dev] {
            assert_eq!(scripts(&s.w, &ep(b), "app", "BEGIN;"), std::slice::from_ref(&upsert));
        }

        // Reactivated: the same call says so, and sessions aren't touched.
        s.fake.client().unban(&s.pool(), &user.to_string()).await.unwrap();
        s.w.computes.sql.lock().unwrap().clear();
        s.w.handle_app_users_sync(&sync_job(&s, user)).await.unwrap();
        let upsert = s.upsert(user, "ada@example.com", None).await;
        assert!(upsert.contains(", true, false, "), "{upsert}");
        assert_eq!(scripts(&s.w, &ep(&s.main), "app", "BEGIN;"), [upsert]);
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 1);
    }

    /// N1: a user gone from the pool loses their sessions too, not only their row.
    #[tokio::test]
    async fn sync_gone_user_revokes_sessions() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let gone = Uuid::new_v4();
        let other = s.add_user("bob@example.com");
        let a = s.w.metadata.add_session(s.project, gone, "s1");
        let b = s.w.metadata.add_session(s.project, other, "s2");

        s.w.handle_app_users_sync(&sync_job(&s, gone)).await.unwrap();

        let revoked = |id: Uuid| {
            s.w.metadata
                .state()
                .app_sessions
                .iter()
                .find(|x| x.id == id)
                .unwrap()
                .revoked_at
                .is_some()
        };
        assert!(revoked(a) && !revoked(b));
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 1);
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [gone_call_sql(gone, false)]
            );
        }
    }

    /// A user of another project's pool is not this project's: read in this pool only, it is
    /// gone here, and the other pool is never asked.
    #[tokio::test]
    async fn sync_user_of_another_pool_is_gone_here() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let elsewhere = Pool::App(Uuid::new_v4());
        let stranger = s.add_user_in(&elsewhere, "eve@example.com");
        s.w.handle_app_users_sync(&sync_job(&s, stranger))
            .await
            .unwrap();
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [gone_call_sql(stranger, false)]
            );
        }
        assert!(
            s.fake
                .calls()
                .iter()
                .all(|c| !c.contains(&elsewhere.to_string()))
        );

        // Any job while app users aren't enabled or enabling is dropped.
        for status in [AppUsersStatus::Disabling, AppUsersStatus::Failed] {
            let s = setup(status).await;
            s.with_pool();
            let user = s.add_user("ada@example.com");
            s.w.handle_app_users_sync(&sync_job(&s, user))
                .await
                .unwrap();
            assert!(s.w.computes.sql.lock().unwrap().is_empty());
            assert!(s.fake.calls().is_empty(), "{status:?}");
        }
    }

    /// No pool: while enabling (before the enable made it) there is nothing to sync yet; once
    /// enabled it is an error, retried, and never read as "every user is gone".
    #[tokio::test]
    async fn sync_without_a_pool() {
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.handle_app_users_sync(&sync_job(&s, Uuid::new_v4()))
            .await
            .unwrap();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());

        // Enabled without a pool: acked with the error on the row; no session is revoked and
        // no row is touched (the user isn't "gone", the pool is).
        let s = setup(AppUsersStatus::Enabled).await;
        let user = Uuid::new_v4();
        s.w.metadata.add_session(s.project, user, "s1");
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 0);
        assert!(s.w.metadata.auth(s.project).unwrap().error.is_some());
    }

    #[tokio::test]
    async fn sync_branch_failure_is_logged_not_retried() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        // The first branch's call fails; the second still gets the row.
        *s.w.computes.sql_failures_left.lock().unwrap() = 1;
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap();
        let ran: Vec<String> = [&s.main, &s.dev]
            .iter()
            .flat_map(|b| scripts(&s.w, &ep(b), "app", "BEGIN;"))
            .collect();
        assert_eq!(ran, [s.upsert(user, "ada@example.com", None).await]);
    }

    #[tokio::test]
    async fn sync_identity_unavailable_naks() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        s.fake.set_down(true);
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap_err();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());

        let mut s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        s.w.identity = None;
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap_err();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());
    }

    // --- Reconcile --------------------------------------------------------------------

    fn zuser(id: &str, email: &str, verified: bool) -> chelabase_identity::User {
        chelabase_identity::User {
            id: id.into(),
            email: email.into(),
            name: String::new(),
            email_verified: verified,
            banned: false,
            created_at: created(),
            updated_at: created(),
        }
    }

    fn row(id: Uuid, email: &str, verified: bool, deleted: bool) -> UserRow {
        UserRow {
            id,
            email: email.into(),
            email_verified: verified,
            deleted_at: deleted.then(created),
            banned_at: None,
        }
    }

    #[test]
    fn plan_app_users_sync_cases() {
        let [
            missing,
            stale_email,
            stale_verified,
            equal,
            gone,
            gone_marked,
        ] = std::array::from_fn(|_| Uuid::new_v4());
        let users = [
            zuser(&missing.to_string(), "m@x.io", true),
            zuser(&stale_email.to_string(), "new@x.io", true),
            zuser(&stale_verified.to_string(), "v@x.io", true),
            zuser(&equal.to_string(), "e@x.io", false),
            // Not a uuid (a pool made before ids were uuids): skipped (P15).
            zuser("312345678901234567", "console@x.io", true),
        ];
        let rows = [
            row(stale_email, "old@x.io", true, false),
            row(stale_verified, "v@x.io", false, false),
            row(equal, "e@x.io", false, false),
            row(gone, "g@x.io", true, false),
            row(gone_marked, "gm@x.io", true, true),
        ];
        assert_eq!(
            plan_app_users_sync(&users, &rows, OnDelete::Delete),
            [missing, stale_email, stale_verified, gone, gone_marked]
        );
        // `mark` leaves rows already marked deleted alone.
        assert_eq!(
            plan_app_users_sync(&users, &rows, OnDelete::Mark),
            [missing, stale_email, stale_verified, gone]
        );
        assert!(plan_app_users_sync(&users[3..4], &rows[2..3], OnDelete::Delete).is_empty());
        assert!(plan_app_users_sync(&[], &[], OnDelete::Mark).is_empty());
    }

    /// AC 3: the ban is part of what's compared, both ways.
    #[test]
    fn plan_app_users_sync_compares_the_ban() {
        let (banned, reactivated, settled) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let ban = |id: Uuid, email: &str| chelabase_identity::User {
            banned: true,
            ..zuser(&id.to_string(), email, true)
        };
        let users = [
            ban(banned, "b@x.io"),
            zuser(&reactivated.to_string(), "r@x.io", true),
            ban(settled, "s@x.io"),
        ];
        let rows = [
            row(banned, "b@x.io", true, false),
            UserRow { banned_at: Some(created()), ..row(reactivated, "r@x.io", true, false) },
            UserRow { banned_at: Some(created()), ..row(settled, "s@x.io", true, false) },
        ];
        assert_eq!(
            plan_app_users_sync(&users, &rows, OnDelete::Delete),
            [banned, reactivated]
        );
    }

    /// B1: reconcile brings a branch on an older bootstrap up to date before reading its rows,
    /// and a branch already current isn't touched.
    #[tokio::test]
    async fn reconcile_upgrades_a_branch_on_an_older_bootstrap() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.w.computes.answer("chelabase.app_users_rows()", "[]");
        s.w.computes.answer_at(&ep(&s.dev), "to_regprocedure", "t\n");

        s.w.reconcile_app_users(s.project).await.unwrap();

        assert_eq!(scripts(&s.w, &ep(&s.dev), "app", BOOTSTRAP).len(), 1);
        assert!(scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP).is_empty());
        let journal = entries(&s.journal);
        let at = |needle: String| journal.iter().position(|e| e.starts_with(&needle)).unwrap();
        assert!(
            at(format!("compute.sql {} app {BOOTSTRAP}", ep(&s.dev)))
                < at(format!("compute.sql {} app BEGIN;", ep(&s.dev))),
            "{journal:?}"
        );

        // An upgrade that fails skips that branch only.
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.w.computes.answer("chelabase.app_users_rows()", "[]");
        s.w.computes.answer_at(&ep(&s.dev), "to_regprocedure", "t\n");
        s.w.computes.fail_when(BOOTSTRAP);
        s.w.reconcile_app_users(s.project).await.unwrap();
        assert!(scripts(&s.w, &ep(&s.dev), "app", USERS_ROWS_SQL).is_empty());
        assert_eq!(scripts(&s.w, &ep(&s.main), "app", USERS_ROWS_SQL).len(), 1);
    }

    /// AC 6: the reconcile reads every page of the pool and makes each branch's table match
    /// it: missing and stale users upserted, gone ones deleted, equal rows left alone.
    #[tokio::test]
    async fn reconcile_from_pool() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let users: Vec<Uuid> = (0..101)
            .map(|i| s.add_user(&format!("u{i}@example.com")))
            .collect();
        // Another project's pool is never read.
        let elsewhere = Pool::App(Uuid::new_v4());
        s.add_user_in(&elsewhere, "other@example.com");
        let ghost = Uuid::new_v4();
        let rows = json!([
            {"id": users[0], "email": "u0@example.com", "email_verified": true, "deleted_at": null},
            {"id": users[1], "email": "stale@example.com", "email_verified": true, "deleted_at": null},
            {"id": ghost, "email": "ghost@example.com", "email_verified": false,
             "deleted_at": "2026-10-03T10:00:00.123456+00:00"},
        ]);
        s.w.computes
            .answer("chelabase.app_users_rows()", &rows.to_string());

        s.w.reconcile_app_users(s.project).await.unwrap();

        let lists = s.pool_calls().iter().filter(|c| *c == "GET /users").count();
        assert_eq!(lists, 2, "two pages of 100");
        assert!(
            s.fake
                .calls()
                .iter()
                .all(|c| !c.contains(&elsewhere.to_string()))
        );
        let u1 = s.upsert(users[1], "u1@example.com", None).await;
        for b in [&s.main, &s.dev] {
            assert_eq!(scripts(&s.w, &ep(b), "app", USERS_ROWS_SQL).len(), 1);
            let ran = scripts(
                &s.w,
                &ep(b),
                "app",
                "BEGIN; SET LOCAL ROLE chelabase_auth_sync; SELECT chelabase.app_users_",
            );
            let upserts: Vec<&String> = ran
                .iter()
                .filter(|x| x.contains("app_users_upsert"))
                .collect();
            assert_eq!(upserts.len(), 100, "every user but the equal one");
            assert!(!ran.iter().any(|x| x.contains(&users[0].to_string())));
            assert!(ran.contains(&u1));
            assert!(ran.contains(&gone_call_sql(ghost, false)));
        }

        // A pool that is gone while the project is enabled is an error on the row, not an
        // empty listing: no row is touched.
        let s = setup(AppUsersStatus::Enabled).await;
        s.w.computes.answer(
            "chelabase.app_users_rows()",
            &json!([{"id": Uuid::new_v4(), "email": "a@x.io", "email_verified": true, "deleted_at": null}]).to_string(),
        );
        s.w.reconcile_app_users(s.project).await.unwrap();
        assert!(scripts(&s.w, &ep(&s.main), "app", "BEGIN;").is_empty());
        assert!(s.w.metadata.auth(s.project).unwrap().error.is_some());
    }

    /// N4: a listing that would delete most rows (an empty pool) deletes nothing; stale rows
    /// of listed users still sync.
    #[tokio::test]
    async fn reconcile_leaves_mass_deletes_to_the_admin_api() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let kept = s.add_user("kept@example.com");
        let (g1, g2) = (Uuid::new_v4(), Uuid::new_v4());
        let rows = json!([
            {"id": kept, "email": "stale@example.com", "email_verified": true, "deleted_at": null},
            {"id": g1, "email": "g1@example.com", "email_verified": true, "deleted_at": null},
            {"id": g2, "email": "g2@example.com", "email_verified": true, "deleted_at": null},
        ]);
        s.w.computes
            .answer("chelabase.app_users_rows()", &rows.to_string());

        s.w.reconcile_app_users(s.project).await.unwrap();

        for b in [&s.main, &s.dev] {
            let ran = scripts(
                &s.w,
                &ep(b),
                "app",
                "BEGIN; SET LOCAL ROLE chelabase_auth_sync; SELECT chelabase.app_users_",
            );
            assert!(ran.contains(&s.upsert(kept, "kept@example.com", None).await));
            assert!(
                !ran.iter().any(|x| x.contains("app_users_gone")),
                "two of three rows would go: {ran:?}"
            );
        }

        // A pool listing no one at all deletes nothing either.
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let rows = json!([
            {"id": g1, "email": "g1@example.com", "email_verified": true, "deleted_at": null},
        ]);
        s.w.computes
            .answer("chelabase.app_users_rows()", &rows.to_string());
        s.w.reconcile_app_users(s.project).await.unwrap();
        for b in [&s.main, &s.dev] {
            assert!(scripts(&s.w, &ep(b), "app", "BEGIN;").iter().all(|x| !x.contains("app_users_gone")));
        }
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 0);
    }

    /// M1: each branch's deletes are planned on its own rows: a branch whose guard trips
    /// deletes nothing, even for an id another branch deletes.
    #[tokio::test]
    async fn reconcile_plans_deletes_per_branch() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let kept = s.add_user("kept@example.com");
        let also = s.add_user("also@example.com");
        let (g1, g2) = (Uuid::new_v4(), Uuid::new_v4());
        let row = |id: Uuid, email: &str| json!({"id": id, "email": email, "email_verified": true, "deleted_at": null});
        // main: 2 of 3 rows would go (guard trips); dev: 1 of 3 (deletes g1).
        let main_rows = json!([row(kept, "kept@example.com"), row(g1, "g1@x"), row(g2, "g2@x")]);
        let dev_rows = json!([row(kept, "kept@example.com"), row(also, "also@example.com"), row(g1, "g1@x")]);
        s.w.computes
            .answer_at(&ep(&s.main), "chelabase.app_users_rows()", &main_rows.to_string());
        s.w.computes
            .answer_at(&ep(&s.dev), "chelabase.app_users_rows()", &dev_rows.to_string());

        s.w.reconcile_app_users(s.project).await.unwrap();

        let gone = |b: &BranchRecord| -> Vec<String> {
            scripts(&s.w, &ep(b), "app", "BEGIN;")
                .into_iter()
                .filter(|x| x.contains("app_users_gone"))
                .collect()
        };
        assert!(gone(&s.main).is_empty(), "{:?}", gone(&s.main));
        assert_eq!(gone(&s.dev), [gone_call_sql(g1, false)]);
    }

    /// M1: in `mark` mode, rows already marked deleted don't count toward the threshold.
    #[tokio::test]
    async fn reconcile_threshold_counts_only_unmarked_rows_in_mark_mode() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.set_on_delete(OnDelete::Mark);
        let kept = s.add_user("kept@example.com");
        let marked = |id: Uuid| json!({"id": id, "email": "m@x", "email_verified": true, "deleted_at": "2026-10-03T10:00:00+00:00"});
        let live = |id: Uuid, email: &str| json!({"id": id, "email": email, "email_verified": true, "deleted_at": null});
        // 2 of the 3 unmarked rows would be marked: the guard trips (6 rows in all wouldn't).
        let rows = json!([
            live(kept, "kept@example.com"), live(Uuid::new_v4(), "g1@x"), live(Uuid::new_v4(), "g2@x"),
            marked(Uuid::new_v4()), marked(Uuid::new_v4()), marked(Uuid::new_v4()),
        ]);
        s.w.computes
            .answer("chelabase.app_users_rows()", &rows.to_string());

        s.w.reconcile_app_users(s.project).await.unwrap();

        for b in [&s.main, &s.dev] {
            assert!(scripts(&s.w, &ep(b), "app", "BEGIN;").iter().all(|x| !x.contains("app_users_gone")));
        }
    }

    /// B5: a sign-in that raced the ban and a lost sync job don't survive reconcile, even when
    /// the row itself already matches the pool.
    #[tokio::test]
    async fn reconcile_revokes_a_banned_users_session() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        s.fake.ban(&s.pool(), &user.to_string());
        let raced = s.w.metadata.add_session(s.project, user, "s1");
        let rows = json!([{"id": user, "email": "ada@example.com", "email_verified": true,
            "deleted_at": null, "banned_at": "2026-10-03T10:00:00+00:00"}]);
        s.w.computes
            .answer("chelabase.app_users_rows()", &rows.to_string());

        s.w.reconcile_app_users(s.project).await.unwrap();

        let state = s.w.metadata.state();
        let session = state.app_sessions.iter().find(|x| x.id == raced).unwrap();
        assert!(
            session.revoked_at.is_some(),
            "a banned user's raced session survives every reconcile"
        );
    }

    #[tokio::test]
    async fn check_location_reports_existing_table() {
        let s = setup(AppUsersStatus::Enabled).await;
        let req = AppUsersCheckRequest {
            database_id: s.db,
            pg_database: "app".into(),
            schema: "public".into(),
            table: "users".into(),
        };
        assert_eq!(
            s.w.check_app_users_location(&req).await,
            AppUsersCheckReply {
                ok: true,
                code: None,
                error: None
            }
        );
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "SET search_path"),
                [crate::app_users_sql::users_table_exists_sql(
                    "public", "users"
                )]
            );
        }

        *s.w.computes.fail_sql_containing.lock().unwrap() =
            Some(("to_regclass".into(), "connection refused".into()));
        let reply = s.w.check_app_users_location(&req).await;
        assert!(!reply.ok);
        assert_eq!(reply.code.as_deref(), Some("unavailable"));

        *s.w.computes.fail_sql_containing.lock().unwrap() = None;
        s.w.computes.answer("to_regclass", "t\n");
        let reply = s.w.check_app_users_location(&req).await;
        assert_eq!(
            reply,
            AppUsersCheckReply {
                ok: false,
                code: Some("app_users_table_exists".into()),
                error: Some("public.users already exists on branch main".into()),
            }
        );
    }

    // --- Branch hooks -----------------------------------------------------------------

    #[tokio::test]
    async fn branch_create_bootstraps_users_on_auth_database_branch() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        // Every branch's table is empty.
        s.w.computes.answer("chelabase.app_users_rows()", "[]");

        let feat = ready_branch(&s.w, s.db, "feat", s.main.id).await;
        let feat = s.w.metadata.branch(feat);

        let (pg_database, pw) = sync_password(&s.w, &feat);
        assert_eq!(pg_database, "app");
        assert_bootstraps(&scripts(&s.w, &ep(&feat), "app", BOOTSTRAP), &pw);
        let journal = entries(&s.journal);
        let at = |prefix: &str| {
            journal
                .iter()
                .position(|e| e.starts_with(prefix))
                .unwrap_or_else(|| panic!("{prefix} not in {journal:?}"))
        };
        let order = [
            at(&format!("compute.sql {} app BEGIN;", ep(&feat))),
            at(&format!("compute.sql {} app {BOOTSTRAP}", ep(&feat))),
            at("metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{journal:?}");
        // Then the users are reconciled onto it.
        assert!(
            scripts(&s.w, &ep(&feat), "app", "BEGIN;")
                .contains(&s.upsert(user, "ada@example.com", None).await)
        );

        // A failing users bootstrap fails the branch.
        s.w.computes.fail_when(BOOTSTRAP);
        let broken = crate::fakes::pending_branch(&s.w, s.db, "broken", s.main.id);
        s.w.handle_branch_create(broken, None).await.unwrap();
        let broken = s.w.metadata.branch(broken);
        assert_eq!(broken.status, chelabase_metadata::BranchStatus::Failed);
        let error = broken.error.unwrap();
        assert!(error.starts_with("bootstrapping app users: "), "{error}");
    }

    #[tokio::test]
    async fn branch_create_skips_other_databases_and_backups() {
        let s = setup(AppUsersStatus::Enabled).await;
        // A branch of another database of the project.
        let other = ready_database(&s.w).await;
        s.w.metadata.move_to_project(other, s.project);
        let other_main = s.w.metadata.main(other);
        let side = ready_branch(&s.w, other, "side", other_main.id).await;
        let side = s.w.metadata.branch(side);
        assert!(scripts(&s.w, &ep(&side), "app", BOOTSTRAP).is_empty());
        assert!(!s.w.metadata.state().auth_sync.contains_key(&side.id));

        // While app users are disabling, new branches aren't bootstrapped.
        s.w.metadata
            .state()
            .project_auth
            .get_mut(&s.project)
            .unwrap()
            .status = AppUsersStatus::Disabling;
        let late = ready_branch(&s.w, s.db, "late", s.main.id).await;
        let late = s.w.metadata.branch(late);
        assert!(scripts(&s.w, &ep(&late), "app", BOOTSTRAP).is_empty());

        // Backup branches are never bootstrapped (P16), nor are branches not ready.
        s.w.metadata
            .state()
            .project_auth
            .get_mut(&s.project)
            .unwrap()
            .status = AppUsersStatus::Enabling;
        let backup = BranchRecord {
            id: chelabase_types::BranchId::new(),
            kind: BranchKind::Backup,
            name: "backup".into(),
            endpoint_id: Some("ep-backup".into()),
            ..s.dev.clone()
        };
        s.w.metadata.state().branches.insert(backup.id, backup);
        s.w.metadata
            .state()
            .branches
            .get_mut(&late.id)
            .unwrap()
            .status = chelabase_metadata::BranchStatus::Resetting;
        s.w.computes.sql.lock().unwrap().clear();
        s.w.handle_app_users_job(&AppUsersJob::Enable {
            project_id: s.project,
        })
        .await
        .unwrap();
        assert_eq!(
            s.w.metadata.auth(s.project).unwrap().status,
            AppUsersStatus::Enabled
        );
        let eps: Vec<String> =
            s.w.computes
                .sql
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, _, x)| x.starts_with(BOOTSTRAP))
                .map(|(e, _, _)| e.clone())
                .collect();
        let mut expected = vec![ep(&s.main), ep(&s.dev)];
        expected.sort();
        let mut eps = eps;
        eps.sort();
        assert_eq!(eps, expected);
    }

    #[tokio::test]
    async fn recreate_compute_rebootstraps_users() {
        let s = setup(AppUsersStatus::Enabled).await;
        let kept = "KeptPasswordKeptPasswordKeptPass";
        s.w.metadata
            .set_branch_auth_sync(
                s.dev.id,
                "app",
                &seal_role_password(&s.w.key, &s.dev.id.to_string(), "chelabase_auth_sync", kept),
            )
            .await
            .unwrap();
        s.w.computes.remove(&ep(&s.dev));
        s.w.computes.sql.lock().unwrap().clear();
        s.journal.lock().unwrap().clear();

        s.w.recreate_compute(s.dev.id, &ep(&s.dev)).await.unwrap();

        // The timeline may have brought back another password: the stored one is set again.
        assert_bootstraps(&scripts(&s.w, &ep(&s.dev), "app", BOOTSTRAP), kept);
        let journal = entries(&s.journal);
        let data_api = journal
            .iter()
            .position(|e| e.starts_with(&format!("compute.sql {} app BEGIN;", ep(&s.dev))))
            .unwrap();
        let users = journal
            .iter()
            .position(|e| e.starts_with(&format!("compute.sql {} app {BOOTSTRAP}", ep(&s.dev))))
            .unwrap();
        assert!(data_api < users, "{journal:?}");

        // Another database's compute isn't touched.
        let other = ready_database(&s.w).await;
        let other = s.w.metadata.main(other);
        s.w.computes.remove(&ep(&other));
        s.w.recreate_compute(other.id, &ep(&other)).await.unwrap();
        assert!(scripts(&s.w, &ep(&other), "app", BOOTSTRAP).is_empty());
    }

    // --- The periodic pass ------------------------------------------------------------

    fn age_auth(s: &Setup, project: ProjectId, minutes: i64) {
        s.w.metadata
            .state()
            .project_auth
            .get_mut(&project)
            .unwrap()
            .updated_at = Utc::now() - chrono::Duration::minutes(minutes);
    }

    #[tokio::test]
    async fn reconcile_reruns_stale_transitions_and_project_deletes() {
        // An enabled project with a user missing from its (empty) table.
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        let user = s.add_user("ada@example.com");
        s.w.computes.answer("chelabase.app_users_rows()", "[]");

        // A stale enable and a fresh one (each with its own auth database id, no branches).
        let stale_enable = s.w.metadata.insert_project();
        s.w.metadata
            .insert_auth(stale_enable, DatabaseId::new(), AppUsersStatus::Enabling);
        age_auth(&s, stale_enable, 10);
        let fresh_enable = s.w.metadata.insert_project();
        s.w.metadata
            .insert_auth(fresh_enable, DatabaseId::new(), AppUsersStatus::Enabling);
        // A stale disable.
        let stale_disable = s.w.metadata.insert_project();
        s.w.metadata
            .insert_auth(stale_disable, DatabaseId::new(), AppUsersStatus::Disabling);
        age_auth(&s, stale_disable, 10);
        // A stale project delete with nothing left, and a fresh one.
        let deleting = s.w.metadata.insert_project();
        let fresh_deleting = s.w.metadata.insert_project();
        for (p, minutes) in [(deleting, 10), (fresh_deleting, 0)] {
            let mut state = s.w.metadata.state();
            let row = state.projects.get_mut(&p).unwrap();
            row.status = ProjectStatus::Deleting;
            row.updated_at = Utc::now() - chrono::Duration::minutes(minutes);
        }
        // Sessions revoked 8 days ago are purged; one revoked an hour ago stays.
        let old = s.w.metadata.add_session(s.project, user, "zs-old");
        let recent = s.w.metadata.add_session(s.project, user, "zs-recent");
        for (id, ago) in [
            (old, chrono::Duration::days(8)),
            (recent, chrono::Duration::hours(1)),
        ] {
            s.w.metadata
                .state()
                .app_sessions
                .iter_mut()
                .find(|x| x.id == id)
                .unwrap()
                .revoked_at = Some(Utc::now() - ago);
        }

        s.w.app_users_reconcile_once().await;

        let upsert = s.upsert(user, "ada@example.com", None).await;
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [USERS_ROWS_SQL.to_string(), upsert.clone()]
            );
        }
        let state = s.w.metadata.state();
        assert_eq!(
            state.project_auth[&stale_enable].status,
            AppUsersStatus::Enabled
        );
        assert_eq!(
            state.project_auth[&fresh_enable].status,
            AppUsersStatus::Enabling
        );
        assert!(!state.project_auth.contains_key(&stale_disable));
        assert_eq!(state.projects[&deleting].status, ProjectStatus::Deleted);
        assert_eq!(
            state.projects[&fresh_deleting].status,
            ProjectStatus::Deleting
        );
        let ids: Vec<Uuid> = state.app_sessions.iter().map(|x| x.id).collect();
        assert_eq!(ids, [recent]);
    }
}
