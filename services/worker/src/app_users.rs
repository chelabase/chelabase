//! App users jobs: enabling and disabling a project's app users, syncing users from Zitadel
//! (the source of truth) into the users table on every live branch of the auth database,
//! reconcile, and the location check the orchestrator asks for before an enable.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use chelabase_metadata::{
    AppUsersStatus, BranchKind, BranchRecord, BranchStatus, OnDelete, ProjectAuth,
};
use chelabase_nats::{AppUsersCheckReply, AppUsersCheckRequest, AppUsersJob, AppUsersSyncJob};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::{open_role_password, seal_role_password};
use chelabase_types::{BranchId, DatabaseId, ProjectId};
use chelabase_zitadel::client::{UserState, ZError, ZUser, Zitadel};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::app_users_sql::{
    USERS_ROWS_SQL, USERS_SYNC_ROLE, USERS_TEARDOWN_SQL, claim_helpers_sql, gone_call_sql,
    upsert_call_sql, users_bootstrap_sql, users_table_exists_sql,
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
}

/// The users whose row on one branch differs from Zitadel, in order: users without a row or
/// with a stale email or verified flag (Zitadel's order), then rows whose user is gone
/// (`delete`: every such row; `mark`: those not marked deleted yet). Users whose id isn't a
/// uuid were made outside Chelabase and are skipped (P15).
pub fn plan_app_users_sync(users: &[ZUser], rows: &[UserRow], on_delete: OnDelete) -> Vec<Uuid> {
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
            .is_some_and(|r| r.email == user.email && r.email_verified == user.email_verified);
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
    /// The admin Zitadel client; its absence fails the job, which is retried (P2).
    fn admin_zitadel(&self) -> Result<&Zitadel, String> {
        self.zitadel.as_ref().ok_or_else(|| {
            "Zitadel admin access isn't configured (CHELABASE_AUTH_HOST and CHELABASE_ZITADEL_ADMIN_PAT_FILE)"
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
    /// only a missing Zitadel client or metadata trouble retries.
    async fn enable_app_users(&self, project: ProjectId) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(|a| a.status == AppUsersStatus::Enabling) else {
            return Ok(());
        };
        let zitadel = self.admin_zitadel()?;
        let outcome = self.enable_steps(&auth, zitadel).await;
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

    async fn enable_steps(&self, auth: &ProjectAuth, zitadel: &Zitadel) -> Result<(), String> {
        let project = auth.project_id;
        let org = project.to_string();
        zitadel
            .ensure_org(&org, &format!("chelabase-app-{org}"))
            .await
            .map_err(|e| format!("creating the Zitadel organization: {e}"))?;
        self.metadata
            .set_app_users_org(project, &org)
            .await
            .map_err(|e| format!("recording the Zitadel organization: {e}"))?;
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
    /// sessions revoked, the org deleted, then the row. Any failure is retried.
    async fn disable_app_users(&self, project: ProjectId) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(|a| a.status == AppUsersStatus::Disabling) else {
            return Ok(());
        };
        let zitadel = match &auth.zitadel_org_id {
            Some(_) => Some(self.admin_zitadel()?),
            None => None,
        };
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
        if let (Some(org), Some(zitadel)) = (&auth.zitadel_org_id, zitadel) {
            zitadel
                .delete_org(org)
                .await
                .map_err(|e| format!("deleting the Zitadel organization: {e}"))?;
        }
        self.metadata
            .delete_project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        info!(%project, "app users disabled");
        Ok(())
    }

    /// Applies one user's Zitadel state to the users table on every live branch of the auth
    /// database (state-based, so duplicates and reordering are harmless). A branch that fails
    /// is logged and left to reconcile; only Zitadel or metadata trouble retries.
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
        let Some(org) = auth.zitadel_org_id.as_deref() else {
            return Ok(());
        };
        let zitadel = self.admin_zitadel()?;
        let user_id = job.user_id;
        let user = zitadel
            .get_user(&user_id.to_string())
            .await
            .map_err(|e| format!("reading user {user_id}: {e}"))?;
        // A user of another org never reaches this project's table.
        if let Some(other) = user.as_ref().filter(|u| u.org_id != org) {
            warn!(project = %job.project_id, %user_id, org = %other.org_id, "sync for a user of another organization; dropped");
            return Ok(());
        }
        // Deactivated, locked or gone (deleted in the console, by the webhook or found by
        // reconcile; N1): their sessions end with it.
        if user
            .as_ref()
            .is_none_or(|u| matches!(u.state, UserState::Inactive | UserState::Locked))
        {
            self.revoke_user(job.project_id, user_id, zitadel).await?;
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
                    upsert_call_sql(
                        user_id,
                        &u.email,
                        u.email_verified,
                        u.created_at.unwrap_or_else(Utc::now),
                        data,
                    )
                }
                None => gone.clone(),
            };
            if let Err(e) = self.computes.admin_sql(&ep, &auth.pg_database, &sql).await {
                warn!(branch = %branch.id, project = %job.project_id, %user_id, "syncing an app user; reconcile retries: {e:#}");
            }
        }
        Ok(())
    }

    /// A deactivated, locked or deleted user: their app sessions revoked and their Zitadel
    /// sessions deleted (best effort).
    async fn revoke_user(
        &self,
        project: ProjectId,
        user: Uuid,
        zitadel: &Zitadel,
    ) -> Result<(), String> {
        let revoked = self
            .metadata
            .revoke_user_sessions(project, user, None, None)
            .await
            .map_err(|e| format!("revoking the user's sessions: {e}"))?;
        for session in revoked {
            if let Err(e) = zitadel
                .delete_session_admin(&session.zitadel_session_id)
                .await
            {
                warn!(%project, %user, session = %session.id, "deleting the Zitadel session: {e}");
            }
        }
        Ok(())
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

    /// Compares every live branch's users table with the org's users and syncs each
    /// difference inline. A branch whose rows can't be read is logged and skipped.
    pub async fn reconcile_app_users(&self, project: ProjectId) -> Result<(), String> {
        let auth = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        let Some(auth) = auth.filter(active) else {
            return Ok(());
        };
        let Some(org) = auth.zitadel_org_id.as_deref() else {
            return Ok(());
        };
        let zitadel = self.admin_zitadel()?;
        let mut users = Vec::new();
        let mut offset = 0;
        loop {
            let page = zitadel
                .list_users(org, None, offset, PAGE)
                .await
                .map_err(|e: ZError| format!("listing the org's users: {e}"))?;
            users.extend(page.users);
            match page.next_offset {
                Some(next) => offset = next,
                None => break,
            }
        }
        let listed: HashSet<Uuid> = users
            .iter()
            .filter_map(|u| Uuid::parse_str(&u.id).ok())
            .collect();
        // Listed users that differ somewhere are synced everywhere; a gone user only on the
        // branches whose own plan deletes them (M1).
        let mut seen = HashSet::new();
        let mut stale = Vec::new();
        let mut gone: Vec<(Uuid, HashSet<BranchId>)> = Vec::new();
        for (branch, ep) in self.live_branches(auth.database_id).await? {
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
            // N4: a listing that would remove most of the table (an empty org, or an admin PAT
            // that lost its rights and sees a filtered listing) isn't trusted for this branch's
            // deletes; the webhook and the API path still delete one by one. In `mark` mode
            // only the rows not marked yet count.
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
    use chelabase_metadata::{AppUsersStatus, BranchKind, DataApiState, ProjectStatus};
    use chelabase_secrets::{open_role_password, seal_role_password};
    use chelabase_types::DatabaseId;
    use chelabase_zitadel::client::{UserState, Zitadel};
    use chelabase_zitadel::mock::{ADMIN_TOKEN, MockZitadel, User};
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
        pub mock: MockZitadel,
        pub project: ProjectId,
        pub db: DatabaseId,
        pub main: BranchRecord,
        pub dev: BranchRecord,
    }

    impl Setup {
        pub fn org(&self) -> String {
            self.project.to_string()
        }

        /// The org exists in Zitadel and is recorded on the row.
        pub fn with_org(&self) {
            self.mock.add_org(&self.org());
            self.w
                .metadata
                .state()
                .project_auth
                .get_mut(&self.project)
                .unwrap()
                .zitadel_org_id = Some(self.org());
        }

        /// A user in the project's org with a uuid id.
        pub fn add_user(&self, email: &str) -> Uuid {
            self.add_user_in(&self.org(), email)
        }

        pub fn add_user_in(&self, org: &str, email: &str) -> Uuid {
            let id = Uuid::new_v4();
            self.mock.inner.lock().unwrap().users.push(User {
                id: id.to_string(),
                email: email.into(),
                password: "Passw0rd!".into(),
                verified: true,
                locked: false,
                org_id: org.into(),
                username: id.to_string(),
                display_name: String::new(),
                inactive: false,
                created_at: "2026-10-03T10:00:00Z".into(),
            });
            id
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
    /// a `project_auth` row in `status`; Zitadel is the mock with the admin token. The journal
    /// and the SQL log start empty.
    pub(crate) async fn setup(status: AppUsersStatus) -> Setup {
        let (mut w, journal) = worker();
        let mock = MockZitadel::start().await;
        w.zitadel = Some(Zitadel::new(&mock.base, ADMIN_TOKEN));
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
            mock,
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

    #[tokio::test]
    async fn enable_creates_org_bootstraps_every_branch_and_helpers() {
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

        assert!(s.mock.orgs().contains(&s.org()), "{:?}", s.mock.orgs());
        assert!(
            s.mock
                .calls()
                .contains(&"POST /v2/organizations".to_string())
        );
        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Enabled, "{:?}", auth.error);
        assert_eq!(auth.zitadel_org_id.as_deref(), Some(s.org().as_str()));
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

        // Zitadel's org call failing is final too.
        let s = setup(AppUsersStatus::Enabling).await;
        s.mock.set_down(true);
        s.w.handle_app_users_job(&enable(&s)).await.unwrap();
        let auth = s.w.metadata.auth(s.project).unwrap();
        assert_eq!(auth.status, AppUsersStatus::Failed);
        assert!(scripts(&s.w, &ep(&s.main), "app", BOOTSTRAP).is_empty());

        // Without the admin client the job is retried and the row stays enabling.
        let mut s = setup(AppUsersStatus::Enabling).await;
        s.w.zitadel = None;
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
            assert!(s.mock.calls().is_empty());
        }
        let s = setup(AppUsersStatus::Enabling).await;
        s.w.handle_app_users_job(&AppUsersJob::Enable {
            project_id: ProjectId::new(),
        })
        .await
        .unwrap();
        assert!(s.mock.calls().is_empty());
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
    async fn disable_tears_down_revokes_deletes_org_and_row() {
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_org();
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
        assert!(!s.mock.orgs().contains(&s.org()));
        let journal = entries(&s.journal);
        let revoked = journal
            .iter()
            .position(|e| e == "metadata.revoke_project_sessions")
            .unwrap();
        let deleted = journal
            .iter()
            .position(|e| e == "metadata.delete_project_auth")
            .unwrap();
        assert!(revoked < deleted);

        // A teardown failure is retried, and the row stays.
        let s = setup(AppUsersStatus::Disabling).await;
        s.with_org();
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
        assert!(s.mock.orgs().contains(&s.org()));

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

    // --- Sync -------------------------------------------------------------------------

    #[tokio::test]
    async fn sync_upserts_on_every_branch_with_data_except_origin() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
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
            [upsert_call_sql(
                user,
                "ada@example.com",
                true,
                created(),
                Some(&data)
            )]
        );
        assert_eq!(
            scripts(&s.w, &ep(&s.dev), "app", "BEGIN;"),
            [upsert_call_sql(
                user,
                "ada@example.com",
                true,
                created(),
                None
            )]
        );

        // An enabling project is synced too (the enable may be filling other branches).
        let s = setup(AppUsersStatus::Enabling).await;
        s.with_org();
        let user = s.add_user("bob@example.com");
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap();
        assert_eq!(scripts(&s.w, &ep(&s.main), "app", "BEGIN;").len(), 1);
    }

    #[tokio::test]
    async fn sync_gone_deletes_or_marks() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
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

    #[tokio::test]
    async fn sync_inactive_revokes_sessions() {
        for state in [UserState::Inactive, UserState::Locked] {
            let s = setup(AppUsersStatus::Enabled).await;
            s.with_org();
            let user = s.add_user("ada@example.com");
            let other = s.add_user("bob@example.com");
            {
                let mut inner = s.mock.inner.lock().unwrap();
                let u = inner
                    .users
                    .iter_mut()
                    .find(|u| u.id == user.to_string())
                    .unwrap();
                match state {
                    UserState::Inactive => u.inactive = true,
                    _ => u.locked = true,
                }
                inner
                    .sessions
                    .insert("zs1".into(), (user.to_string(), "t1".into()));
                inner
                    .sessions
                    .insert("zs2".into(), (user.to_string(), "t2".into()));
                inner
                    .sessions
                    .insert("zs3".into(), (other.to_string(), "t3".into()));
            }
            let a = s.w.metadata.add_session(s.project, user, "zs1");
            let b = s.w.metadata.add_session(s.project, user, "zs2");
            let c = s.w.metadata.add_session(s.project, other, "zs3");

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
            assert!(revoked(a) && revoked(b) && !revoked(c), "{state:?}");
            let sessions = s.mock.inner.lock().unwrap().sessions.clone();
            assert!(!sessions.contains_key("zs1") && !sessions.contains_key("zs2"));
            assert!(sessions.contains_key("zs3"));
            // Once, not per branch; and the row still follows Zitadel.
            assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 1);
            for b in [&s.main, &s.dev] {
                assert_eq!(
                    scripts(&s.w, &ep(b), "app", "BEGIN;"),
                    [upsert_call_sql(
                        user,
                        "ada@example.com",
                        true,
                        created(),
                        None
                    )]
                );
            }
        }
    }

    /// N1: a user deleted outside Chelabase's API (Zitadel console, webhook, reconcile) loses
    /// their sessions too, not only their row.
    #[tokio::test]
    async fn sync_gone_user_revokes_sessions() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
        let gone = Uuid::new_v4();
        let other = s.add_user("bob@example.com");
        {
            let mut inner = s.mock.inner.lock().unwrap();
            inner
                .sessions
                .insert("zs1".into(), (gone.to_string(), "t1".into()));
            inner
                .sessions
                .insert("zs2".into(), (other.to_string(), "t2".into()));
        }
        let a = s.w.metadata.add_session(s.project, gone, "zs1");
        let b = s.w.metadata.add_session(s.project, other, "zs2");

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
        let sessions = s.mock.inner.lock().unwrap().sessions.clone();
        assert!(!sessions.contains_key("zs1") && sessions.contains_key("zs2"));
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 1);
        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [gone_call_sql(gone, false)]
            );
        }
    }

    #[tokio::test]
    async fn sync_other_org_user_is_dropped() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
        let stranger = s.add_user_in("o1", "eve@example.com");
        s.w.handle_app_users_sync(&sync_job(&s, stranger))
            .await
            .unwrap();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());
        assert_eq!(count(&s.journal, "metadata.revoke_user_sessions"), 0);

        // So is any job while app users aren't enabled or enabling, or without an org.
        for status in [AppUsersStatus::Disabling, AppUsersStatus::Failed] {
            let s = setup(status).await;
            s.with_org();
            let user = s.add_user("ada@example.com");
            s.w.handle_app_users_sync(&sync_job(&s, user))
                .await
                .unwrap();
            assert!(s.w.computes.sql.lock().unwrap().is_empty());
            assert!(s.mock.calls().is_empty(), "{status:?}");
        }
        let s = setup(AppUsersStatus::Enabled).await;
        let user = s.add_user("ada@example.com");
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());
        assert!(s.mock.calls().is_empty());
    }

    #[tokio::test]
    async fn sync_branch_failure_is_logged_not_retried() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
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
        assert_eq!(
            ran,
            [upsert_call_sql(
                user,
                "ada@example.com",
                true,
                created(),
                None
            )]
        );
    }

    #[tokio::test]
    async fn sync_zitadel_unavailable_naks() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
        let user = s.add_user("ada@example.com");
        s.mock.set_down(true);
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap_err();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());

        let mut s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
        let user = s.add_user("ada@example.com");
        s.w.zitadel = None;
        s.w.handle_app_users_sync(&sync_job(&s, user))
            .await
            .unwrap_err();
        assert!(s.w.computes.sql.lock().unwrap().is_empty());
    }

    // --- Reconcile --------------------------------------------------------------------

    fn zuser(id: &str, email: &str, verified: bool) -> ZUser {
        ZUser {
            id: id.into(),
            org_id: "org".into(),
            email: email.into(),
            email_verified: verified,
            display_name: String::new(),
            state: UserState::Active,
            created_at: None,
            has_password: true,
        }
    }

    fn row(id: Uuid, email: &str, verified: bool, deleted: bool) -> UserRow {
        UserRow {
            id,
            email: email.into(),
            email_verified: verified,
            deleted_at: deleted.then(created),
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
            // Not a uuid: made outside Chelabase, skipped (P15).
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

    #[tokio::test]
    async fn reconcile_pages_through_zitadel() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
        let users: Vec<Uuid> = (0..101)
            .map(|i| s.add_user(&format!("u{i}@example.com")))
            .collect();
        // A user made in the console with a non-uuid id is skipped.
        s.mock
            .add_user_in(&s.org(), "console@example.com", "Passw0rd!", true);
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

        let lists = s
            .mock
            .calls()
            .iter()
            .filter(|c| *c == "POST /v2/users")
            .count();
        assert_eq!(lists, 2, "two pages of 100");
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
            assert!(ran.contains(&upsert_call_sql(
                users[1],
                "u1@example.com",
                true,
                created(),
                None
            )));
            assert!(ran.contains(&gone_call_sql(ghost, false)));
        }
    }

    /// N4: a listing that would delete most rows (an empty org, or a PAT that lost its rights
    /// and sees a filtered listing) deletes nothing; stale rows of listed users still sync.
    #[tokio::test]
    async fn reconcile_leaves_mass_deletes_to_the_webhook() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
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
            let ran = scripts(&s.w, &ep(b), "app", "BEGIN; SET LOCAL ROLE chelabase_auth_sync; SELECT chelabase.app_users_");
            assert!(ran.contains(&upsert_call_sql(kept, "kept@example.com", true, created(), None)));
            assert!(
                !ran.iter().any(|x| x.contains("app_users_gone")),
                "two of three rows would go: {ran:?}"
            );
        }

        // An org listing no one at all deletes nothing either.
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
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
        s.with_org();
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
        s.with_org();
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
        s.with_org();
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
            scripts(&s.w, &ep(&feat), "app", "BEGIN;").contains(&upsert_call_sql(
                user,
                "ada@example.com",
                true,
                created(),
                None
            ))
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
        s.with_org();
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

        for b in [&s.main, &s.dev] {
            assert_eq!(
                scripts(&s.w, &ep(b), "app", "BEGIN;"),
                [
                    USERS_ROWS_SQL.to_string(),
                    upsert_call_sql(user, "ada@example.com", true, created(), None)
                ]
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
