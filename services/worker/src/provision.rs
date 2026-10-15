//! Provisioning a database on Neon: an endpoint row, a tenant and timeline on the storage
//! controller, a compute, and the data API roles and bootstrap inside it.

use chelabase_metadata::{DatabaseStatus, NewEndpoint};
use anyhow::anyhow;
use chelabase_neon::reconfigure::ComputeCtlStatus;
use chelabase_neon::{ComputeBackend, ComputeStatus, NeonId};
use chelabase_secrets::{Purpose, seal_credential};
use chelabase_types::{BranchId, DatabaseId};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::names::sanitize_db_name;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::reconcile::should_abort_provision;
use crate::{OWNER_ROLE, Worker, credentials};

/// How long a new compute may take to answer (the worker's `ready_timeout` in production).
pub const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The most a wait may stretch to while compute_ctl reports a live state (`init`,
/// `configuration_pending`, …): longer than RT3-14's 2-minute migration timeout, so a compute
/// that is migrating isn't given up on at [`READY_TIMEOUT`].
pub const READY_CAP: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether compute_ctl's `failed` error is a migration failure that no retry fixes
/// (`migration N failed: …`, `migrations could not run: …`, U3): the wait ends at once. Any
/// other failure (a transient start error, which the container's `unless-stopped` restart
/// recovers) is polled on until the deadline.
pub fn is_fatal_migration_error(error: &str) -> bool {
    error.contains("migrations could not run")
        || error.split("migration ").skip(1).any(|rest| {
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with(" failed")
        })
}

/// The idempotent SQL run as cloud_admin in every new database: the data API's roles, and with
/// `owner_superuser` the owner made SUPERUSER. compute_ctl never drops roles it doesn't know,
/// so these survive later reconfigures. The search path is pinned first: on a branch's copy
/// the owner may have set the database's `search_path` and created operators.
pub fn bootstrap_sql(owner_superuser: bool) -> String {
    let mut sql = String::from(
        "SET search_path = pg_catalog, pg_temp; DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'anon') THEN
    CREATE ROLE anon NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'authenticated') THEN
    CREATE ROLE authenticated NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'authenticator') THEN
    CREATE ROLE authenticator NOLOGIN NOINHERIT;
  END IF;
END
$$;
GRANT anon, authenticated TO authenticator;
",
    );
    if owner_superuser {
        sql.push_str(&format!("ALTER ROLE {OWNER_ROLE} SUPERUSER;\n"));
    }
    sql
}

/// Why [`Worker::wait_until_ready`]'s look found the compute not ready.
enum NotReady {
    /// compute_ctl reported `failed` with this error.
    Failed(String),
    /// compute_ctl reports a live, non-failed state (migrating, configuring): the wait may
    /// stretch to [`READY_CAP`].
    Live(anyhow::Error),
    /// Not yet (or compute_ctl or the database can't be reached yet).
    Yet(anyhow::Error),
}

/// `ep-` + 12 random lowercase hex characters.
pub fn new_endpoint_id() -> String {
    format!("ep-{}", hex::encode(rand::random::<[u8; 6]>()))
}

/// 32 random bytes as hex. Only its [`sha256_hex`] is stored.
pub fn new_compute_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Milliseconds since `since`, for the timing logs.
pub(crate) fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Logs (at debug) that `phase` of a provision or branch create took since `since`.
pub(crate) fn phase_done(endpoint_id: &str, phase: &str, since: Instant) {
    debug!(
        endpoint_id,
        phase,
        elapsed_ms = elapsed_ms(since),
        "phase done"
    );
}

/// Everything step 2 generated and recorded. Once it exists, a failure tears down.
struct Recorded {
    endpoint_id: String,
    tenant: NeonId,
    timeline: NeonId,
    /// Only ever in memory and in the container's environment.
    compute_token: String,
    database_name: String,
    password: String,
}

/// How [`Worker::build`] ended when nothing failed.
enum Built {
    /// Steps 3-6 done.
    Compute,
    /// A delete removed the endpoint row meanwhile; stopped after the timeline.
    EndpointGone,
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Provisions `id`. A failure is recorded on the row (and everything built is torn down);
    /// `Err` only means metadata trouble, and the job is retried.
    pub async fn handle_provision(&self, id: DatabaseId, name: &str) -> Result<(), String> {
        let started = Instant::now();
        info!(database_id = %id, %name, "provisioning database");
        match self.metadata.get_database(id).await {
            Ok(Some(existing)) if existing.status == DatabaseStatus::Ready => {
                info!(database_id = %id, "already ready; skipping");
                return Ok(());
            }
            Ok(Some(existing)) if should_abort_provision(existing.status) => {
                info!(database_id = %id, status = existing.status.as_str(), "deleted before provisioning; skipping");
                return Ok(());
            }
            Ok(None) => {
                warn!(database_id = %id, "no metadata row; dropping provision job");
                return Ok(());
            }
            Ok(Some(_)) => {}
            Err(err) => return Err(err.to_string()),
        }
        let claimed = self
            .metadata
            .set_status(id, DatabaseStatus::Provisioning)
            .await
            .map_err(|e| e.to_string())?;
        if !claimed {
            info!(database_id = %id, "deleted before provisioning; skipping");
            return Ok(());
        }

        let database_name = match sanitize_db_name(name) {
            Ok(database_name) => database_name,
            Err(err) => return self.fail(id, &err).await,
        };
        // Step 2. Nothing exists yet when it fails, so there's nothing to clean.
        let rec = match self.record_endpoint(id, database_name).await {
            Ok(rec) => rec,
            Err(err) => return self.fail(id, &err).await,
        };
        // Steps 3-6.
        match self.build(id, &rec).await {
            Ok(Built::Compute) => self.finish(id, &rec, started).await,
            Ok(Built::EndpointGone) => {
                info!(database_id = %id, "endpoint row gone while provisioning; tearing down");
                self.abandon_gone(id, &rec).await
            }
            Err(err) => {
                self.teardown_logged(&rec).await;
                self.fail(id, &err).await
            }
        }
    }

    /// Records a failure on the row (a no-op once a delete began) and acks the job: retrying a
    /// broken provision forever helps nobody.
    async fn fail(&self, id: DatabaseId, err: &str) -> Result<(), String> {
        warn!(database_id = %id, "provision failed: {err}");
        self.metadata
            .set_failed(id, err)
            .await
            .map_err(|e| e.to_string())
    }

    /// Step 2: ids and secrets; the endpoint, its role and database, and the link to main, in
    /// one transaction.
    async fn record_endpoint(
        &self,
        id: DatabaseId,
        database_name: String,
    ) -> Result<Recorded, String> {
        let rec = Recorded {
            endpoint_id: new_endpoint_id(),
            tenant: NeonId::new(),
            timeline: NeonId::new(),
            compute_token: new_compute_token(),
            database_name,
            password: credentials::generate_password(),
        };
        let verifier = chelabase_neon::scram::verifier(&rec.password);
        let token_hash = sha256_hex(&rec.compute_token);
        let endpoint = NewEndpoint {
            id: &rec.endpoint_id,
            tenant_id: rec.tenant.as_str(),
            timeline_id: rec.timeline.as_str(),
            compute_token_sha256: &token_hash,
            owner_role: OWNER_ROLE,
            owner_verifier: &verifier,
            database_name: &rec.database_name,
            role_copies: &[],
            routable: true,
            static_lsn: None,
        };
        match self
            .metadata
            .create_endpoint_for_branch(BranchId::from(id), rec.timeline.as_str(), &endpoint, false)
            .await
        {
            Ok(true) => Ok(rec),
            // The row was claimed just before, so it's linked to an earlier attempt's endpoint
            // (a delete landing meanwhile also refuses, but then `set_failed` writes nothing).
            // Reconcile tears down the endpoint of a failed database.
            Ok(false) => Err(
                "recording the endpoint: a previous attempt was interrupted; its \
                 resources will be cleaned up"
                    .to_string(),
            ),
            Err(err) => Err(format!("recording the endpoint: {err}")),
        }
    }

    /// Steps 3-6: tenant, timeline, compute, readiness, bootstrap.
    async fn build(&self, id: DatabaseId, rec: &Recorded) -> Result<Built, String> {
        let ep = rec.endpoint_id.as_str();
        // The database's branches all live in this tenant. False means a delete began: the
        // checks below and in step 7 notice it.
        self.metadata
            .set_neon_tenant(id, rec.tenant.as_str())
            .await
            .map_err(|e| format!("recording the tenant: {e}"))?;
        let phase = Instant::now();
        self.storage
            .create_tenant(&rec.tenant)
            .await
            .map_err(|e| format!("creating tenant: {e:#}"))?;
        phase_done(ep, "create_tenant", phase);
        // The history window (`pitr_interval`). Reconcile patches it if this fails.
        if let Err(err) = self.handle_database_settings(id).await {
            warn!(database_id = %id, "applying the history window failed; reconcile retries: {err}");
        }
        let phase = Instant::now();
        self.storage
            .create_timeline(&rec.tenant, &rec.timeline)
            .await
            .map_err(|e| format!("creating timeline: {e:#}"))?;
        phase_done(ep, "create_timeline", phase);
        // A delete that finished since step 2 found no tenant to delete, and took the
        // endpoint row with it: nothing else would ever remove this tenant.
        let endpoint = self
            .metadata
            .neon_endpoint(ep)
            .await
            .map_err(|e| format!("checking the endpoint: {e}"))?;
        if endpoint.is_none() {
            return Ok(Built::EndpointGone);
        }
        let phase = Instant::now();
        self.computes
            .create(ep, &rec.compute_token)
            .await
            .map_err(|e| format!("starting compute: {e:#}"))?;
        phase_done(ep, "computes.create", phase);
        let phase = Instant::now();
        self.wait_until_ready(ep, &rec.database_name).await?;
        phase_done(ep, "wait_until_ready", phase);
        let phase = Instant::now();
        self.computes
            .admin_sql(ep, &rec.database_name, &bootstrap_sql(self.owner_superuser))
            .await
            .map_err(|e| format!("bootstrapping roles: {e:#}"))?;
        self.bootstrap_served_on(BranchId::from(id), ep, &rec.database_name, &[])
            .await
            .map(|_| ())
            .map_err(|e| format!("bootstrapping the data API: {e:#}"))?;
        phase_done(ep, "bootstrap", phase);
        self.record_compute_settings(ep).await;
        // Lists the branch's databases (the system `postgres` one included) from the start. A
        // failure only logs: reconcile's resync catches up.
        let phase = Instant::now();
        if let Err(err) = self.resync(BranchId::from(id)).await {
            warn!(database_id = %id, endpoint_id = %ep, "{err}; reconcile retries");
        }
        phase_done(ep, "resync", phase);
        Ok(Built::Compute)
    }

    /// Step 5: the compute is ready when compute_ctl says `running` (its migrations are done,
    /// D6) and the database answers. Only a fatal migration error from compute_ctl
    /// ([`is_fatal_migration_error`]) ends the wait at once; any other `failed` keeps the
    /// polling going to the deadline (and is reported if it is the last thing seen), as does a
    /// status that can't be read. While compute_ctl reports a live state the deadline is
    /// `max(ready_cap, ready_timeout)` from the start rather than `ready_timeout`. A compute
    /// whose container exited (or is gone) never will be ready: that ends the wait at once
    /// (T14 M5), rather than at the deadline.
    pub(crate) async fn wait_until_ready(&self, ep: &str, database_name: &str) -> Result<(), String> {
        let started = Instant::now();
        // Once compute_ctl has been seen in a live state the stretch holds, whatever the
        // later looks say (a status read that fails mid-migration doesn't end it).
        let mut seen_live = false;
        loop {
            let (err, live) = match self.ready_once(ep, database_name).await {
                Ok(()) => return Ok(()),
                Err(NotReady::Failed(error)) if is_fatal_migration_error(&error) => {
                    return Err(format!("waiting for the compute: compute_ctl failed: {error}"));
                }
                Err(NotReady::Failed(error)) => (anyhow!("compute_ctl failed: {error}"), false),
                Err(NotReady::Live(err)) => (err, true),
                Err(NotReady::Yet(err)) => (err, false),
            };
            match self.computes.status(ep).await {
                Ok(None) => {
                    return Err(format!(
                        "waiting for the compute: its container no longer exists: {err:#}"
                    ));
                }
                Ok(Some(ComputeStatus::NotRunning(state)))
                    if matches!(state.as_str(), "exited" | "dead") =>
                {
                    return Err(format!(
                        "waiting for the compute: its container {state}: {err:#}"
                    ));
                }
                // Running, starting, or unknown: keep polling until the deadline.
                _ => {}
            }
            // The wait stretches to the cap only while compute_ctl reports a live state.
            seen_live |= live;
            let wait = if seen_live {
                self.ready_cap.max(self.ready_timeout)
            } else {
                self.ready_timeout
            };
            if started.elapsed() >= wait {
                return Err(format!("waiting for the compute: {err:#}"));
            }
            tokio::time::sleep(self.ready_poll).await;
        }
    }

    /// One look: compute_ctl `running`, then `SELECT 1`.
    async fn ready_once(&self, ep: &str, database_name: &str) -> Result<(), NotReady> {
        let control_addr = self
            .computes
            .control_addr(ep)
            .await
            .map_err(|e| NotReady::Yet(e.context("finding compute_ctl")))?;
        let Some(control_addr) = control_addr else {
            // Not running: the database's own error is the useful one.
            return Err(NotReady::Yet(
                match self.computes.admin_sql(ep, database_name, "SELECT 1").await {
                    Ok(_) => anyhow!("compute_ctl isn't reachable (the compute isn't running)"),
                    Err(e) => e,
                },
            ));
        };
        match self
            .pusher
            .status(control_addr, ep)
            .await
            .map_err(|e| NotReady::Yet(e.context("reading compute_ctl's status")))?
        {
            ComputeCtlStatus::Running => {}
            ComputeCtlStatus::Failed(error) => return Err(NotReady::Failed(error)),
            // compute_ctl's states that lead to `running`: initialising and configuring.
            // `empty` and the termination states never do.
            ComputeCtlStatus::Other(state)
                if matches!(state.as_str(), "init" | "configuration_pending" | "configuration") =>
            {
                return Err(NotReady::Live(anyhow!("compute_ctl is {state}, not running")));
            }
            ComputeCtlStatus::Other(state) => {
                return Err(NotReady::Yet(anyhow!("compute_ctl is {state}, not running")));
            }
        }
        self.computes
            .admin_sql(ep, database_name, "SELECT 1")
            .await
            .map(|_| ())
            .map_err(NotReady::Yet)
    }

    /// Provision's teardown. A failure is logged only: the endpoint row stays, and reconcile
    /// retries the teardown.
    async fn teardown_logged(&self, rec: &Recorded) -> bool {
        match self.teardown(&rec.endpoint_id, rec.tenant.as_str()).await {
            Ok(()) => true,
            Err(err) => {
                warn!(endpoint_id = %rec.endpoint_id, "teardown failed; reconcile will retry: {err}");
                false
            }
        }
    }

    /// Step 7: re-check for a delete that landed meanwhile, then `set_branch_ready` (which sets
    /// the database ready too).
    async fn finish(&self, id: DatabaseId, rec: &Recorded, started: Instant) -> Result<(), String> {
        let current = match self.metadata.get_database(id).await {
            Ok(current) => current,
            Err(err) => {
                // Not ready yet, so tearing down is safe; the retried job starts afresh.
                self.teardown_logged(rec).await;
                return Err(err.to_string());
            }
        };
        let deleted = match current {
            None => true,
            Some(row) => should_abort_provision(row.status),
        };
        if deleted {
            info!(database_id = %id, "deleted while provisioning; tearing down");
            return self.abandon(id, rec).await;
        }
        // Main's id is the database's: the AAD is the branch id, as for every branch.
        let main = BranchId::from(id);
        let sealed = seal_credential(&self.key, &main.to_string(), Purpose::Owner, &rec.password);
        // An error here may hide a commit, so nothing is torn down: the retried job either
        // skips a ready row, or fails to record a second endpoint and reconcile cleans up.
        let set = self
            .metadata
            .set_branch_ready(main, &sealed, None, None)
            .await
            .map_err(|e| e.to_string())?;
        if !set {
            // A delete landed just before, or reconcile failed a stale provision: tear down,
            // and mark the rows deleted only for a delete.
            info!(database_id = %id, "no longer provisioning just before ready; tearing down");
            return self.abandon_gone(id, rec).await;
        }
        info!(database_id = %id, endpoint_id = %rec.endpoint_id, elapsed_ms = elapsed_ms(started), "database ready");
        Ok(())
    }

    /// The endpoint row vanished after step 2, or `set_branch_ready` refused: a delete landed,
    /// or reconcile failed a stalled provision (and may have torn its endpoint down). Tear down
    /// what this provision built; mark the rows deleted only for a delete. A failed row stays
    /// as reconcile left it.
    async fn abandon_gone(&self, id: DatabaseId, rec: &Recorded) -> Result<(), String> {
        if !self.teardown_logged(rec).await {
            return Ok(());
        }
        let current = match self.metadata.get_database(id).await {
            Ok(current) => current,
            Err(err) => {
                // Acked, not retried: a redelivered job would provision a failed row afresh.
                // A delete's row stays `deleting` for the delete job or reconcile.
                warn!(database_id = %id, "re-reading the row after teardown failed: {err}");
                return Ok(());
            }
        };
        match current {
            Some(row) if !should_abort_provision(row.status) => {
                info!(
                    database_id = %id,
                    status = row.status.as_str(),
                    "endpoint was removed by reconcile, not a delete; leaving the row as it is"
                );
                Ok(())
            }
            _ => self.mark_all_deleted(id).await,
        }
    }

    /// Main (the only branch a provisioning database has), then the database.
    async fn mark_all_deleted(&self, id: DatabaseId) -> Result<(), String> {
        self.metadata
            .mark_branch_deleted(BranchId::from(id))
            .await
            .map_err(|e| e.to_string())?;
        self.metadata
            .mark_deleted(id)
            .await
            .map_err(|e| e.to_string())
    }

    /// A delete won the race: tear down, then mark the row deleted. A failed teardown leaves
    /// the row `deleting` for the delete job or reconcile to finish.
    async fn abandon(&self, id: DatabaseId, rec: &Recorded) -> Result<(), String> {
        if self.teardown_logged(rec).await {
            self.mark_all_deleted(id).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::DatabaseStatus;
    use chelabase_secrets::{Purpose, open_credential};

    use std::time::{Duration, Instant};

    use super::*;
    use crate::fakes::{FakeWorker, Journal, STORAGE_CAUSE, entries, position, worker};

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn has(journal: &Journal, prefix: &str) -> bool {
        position(journal, prefix).is_some()
    }

    async fn provision(w: &FakeWorker, name: &str) -> DatabaseId {
        let id = w.metadata.insert_database(name, DatabaseStatus::Pending);
        w.handle_provision(id, name).await.unwrap();
        id
    }

    /// T14 M5: a compute that exited (or vanished) is waited for no longer; one still starting
    /// is polled until the deadline.
    #[tokio::test]
    async fn waiting_for_a_compute_that_exited_stops_at_once() {
        let (mut w, journal) = worker();
        w.ready_timeout = Duration::from_millis(200);
        w.computes.create("ep-exited", "t").await.unwrap();
        w.computes.fail_when("SELECT 1");
        let attempts = |journal: &Journal| {
            entries(journal)
                .iter()
                .filter(|e| e.contains(" app SELECT 1"))
                .count()
        };

        // Still running: polled until the deadline.
        let err = w.wait_until_ready("ep-exited", "app").await.unwrap_err();
        assert!(err.contains("injected failure"), "{err}");
        assert!(attempts(&journal) > 1, "{:?}", entries(&journal));

        // Exited: one attempt, and the error says so.
        journal.lock().unwrap().clear();
        w.computes.set_running("ep-exited", false);
        let started = Instant::now();
        let err = w.wait_until_ready("ep-exited", "app").await.unwrap_err();
        assert!(err.contains("exited"), "{err}");
        assert_eq!(attempts(&journal), 1);
        assert!(started.elapsed() < Duration::from_millis(150));

        // Gone: the same.
        journal.lock().unwrap().clear();
        w.computes.remove("ep-exited");
        let err = w.wait_until_ready("ep-exited", "app").await.unwrap_err();
        assert!(err.contains("no longer exists"), "{err}");
        assert_eq!(attempts(&journal), 1);
    }

    #[test]
    fn ids_have_their_shapes() {
        let ep = new_endpoint_id();
        let hex = ep.strip_prefix("ep-").unwrap();
        assert_eq!(hex.len(), 12);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_ne!(ep, new_endpoint_id());
        let token = new_compute_token();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(token, new_compute_token());
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn bootstrap_sql_is_exact() {
        // Fix round 1 (I3): on a branch copy the owner may have set the database's
        // search_path and created operators; the superuser session searches only pg_catalog.
        let expected = "SET search_path = pg_catalog, pg_temp; DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'anon') THEN
    CREATE ROLE anon NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'authenticated') THEN
    CREATE ROLE authenticated NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'authenticator') THEN
    CREATE ROLE authenticator NOLOGIN NOINHERIT;
  END IF;
END
$$;
GRANT anon, authenticated TO authenticator;
";
        assert_eq!(bootstrap_sql(false), expected);
        assert_eq!(
            bootstrap_sql(true),
            format!("{expected}ALTER ROLE postgres SUPERUSER;\n")
        );
    }

    #[tokio::test]
    async fn provision_happy_path() {
        let (w, journal) = worker();
        let id = provision(&w, "web-1 prod").await;

        let db = w.metadata.database(id);
        let main = w.metadata.main(id);
        assert_eq!(db.status, DatabaseStatus::Ready, "{:?}", db.error);
        assert_eq!(db.database_name.as_deref(), Some("web_1_prod"));
        assert_eq!(db.owner_role.as_deref(), Some("postgres"));
        assert_eq!(main.status, chelabase_metadata::BranchStatus::Ready);
        let ep = main.endpoint_id.clone().unwrap();
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&ep));

        let state = w.metadata.state();
        let stored = &state.endpoints[&ep];
        assert_eq!(stored.roles.len(), 1);
        assert_eq!(stored.roles[0].name, "postgres");
        assert!(stored.roles[0].scram_verifier.starts_with("SCRAM-SHA-256$"));
        assert_eq!(stored.databases.len(), 1);
        assert_eq!(stored.databases[0].name, "web_1_prod");
        assert_eq!(stored.databases[0].owner, "postgres");
        assert_eq!(
            stored.compute_settings_version,
            crate::COMPUTE_SETTINGS_VERSION
        );
        assert!(w.storage.has_tenant(&stored.endpoint.tenant_id));
        assert_eq!(db.neon_tenant_id.as_ref(), Some(&stored.endpoint.tenant_id));
        assert_eq!(main.timeline_id.as_ref(), Some(&stored.endpoint.timeline_id));
        let token = w.computes.token_of(&ep).unwrap();
        assert_eq!(sha256_hex(&token), stored.endpoint.compute_token_sha256);
        drop(state);

        let sql = w.computes.sql.lock().unwrap().clone();
        let bootstrap = sql
            .iter()
            .find(|(e, d, s)| e == &ep && d == "web_1_prod" && s.starts_with("SET search_path = pg_catalog, pg_temp; DO $$"))
            .expect("bootstrap ran on the new database");
        assert!(bootstrap.2.contains("CREATE ROLE anon NOLOGIN"));
        assert!(!bootstrap.2.contains("SUPERUSER"));

        let password = open_credential(
            &w.key,
            &main.id.to_string(),
            Purpose::Owner,
            main.owner_password_enc.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(password.len(), 32);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));

        let order = [
            pos(&journal, "metadata.create_endpoint_for_branch"),
            pos(&journal, "storage.create_tenant"),
            pos(&journal, "storage.create_timeline"),
            pos(&journal, "compute.create"),
            pos(&journal, &format!("compute.sql {ep} web_1_prod SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} web_1_prod SET search_path = pg_catalog, pg_temp; DO $$")),
            pos(&journal, "metadata.set_compute_settings_version"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
    }

    /// Q8: the system `postgres` database is listed from the start, so the provision ends with
    /// a resync (before the branch is ready) rather than leaving it to reconcile's round robin.
    #[tokio::test]
    async fn provision_resyncs_so_the_system_database_is_listed() {
        let (w, journal) = worker();
        let line = |name: &str, owner: &str, system: bool| {
            format!(
                "d|{}||{}|{}|||||||\n",
                hex::encode(name),
                hex::encode(owner),
                if system { "t" } else { "f" }
            )
        };
        w.computes.answer(
            "FROM pg_authid",
            &format!(
                "{}{}",
                line("app", "postgres", false),
                line("postgres", "cloud_admin", true)
            ),
        );
        let id = provision(&w, "app").await;
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        let state = w.metadata.state();
        let listed: Vec<(&str, bool)> = state.endpoints[&ep]
            .branch_databases
            .iter()
            .map(|d| (d.name.as_str(), d.system))
            .collect();
        assert_eq!(listed, [("app", false), ("postgres", true)]);
        drop(state);
        let order = [
            pos(&journal, "metadata.set_compute_settings_version"),
            pos(&journal, "metadata.apply_resync"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
    }

    /// A failed resync doesn't fail the provision: reconcile's resync catches up.
    #[tokio::test]
    async fn a_failed_resync_still_provisions() {
        let (w, _journal) = worker();
        w.computes.answer("FROM pg_authid", "garbage\n");
        let id = provision(&w, "app").await;
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
    }

    #[tokio::test]
    async fn superuser_switch_appends_alter_role() {
        let (mut w, _journal) = worker();
        w.owner_superuser = true;
        let id = provision(&w, "app").await;
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            sql.iter()
                .any(|(_, d, s)| d == "app" && s.contains("ALTER ROLE postgres SUPERUSER;"))
        );
    }

    #[tokio::test]
    async fn waits_for_readiness() {
        let (w, _journal) = worker();
        *w.computes.sql_failures_left.lock().unwrap() = 3;
        let id = provision(&w, "app").await;
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
    }

    #[tokio::test]
    async fn a_failure_at_each_step_cleans_up_and_names_the_step() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange, &str); 6] = [
            (
                "create_endpoint",
                |w| w.metadata.state().fail = Some("create_endpoint_for_branch"),
                "recording the endpoint: ",
            ),
            (
                "create_tenant",
                |w| *w.storage.fail.lock().unwrap() = Some("create_tenant"),
                "creating tenant: ",
            ),
            (
                "create_timeline",
                |w| *w.storage.fail.lock().unwrap() = Some("create_timeline"),
                "creating timeline: ",
            ),
            (
                "compute create",
                |w| *w.computes.fail_create.lock().unwrap() = Some("no docker".into()),
                "starting compute: ",
            ),
            (
                "readiness",
                |w| *w.computes.sql_failures_left.lock().unwrap() = u32::MAX,
                "waiting for the compute: ",
            ),
            (
                "bootstrap",
                |w| {
                    *w.computes.fail_sql_containing.lock().unwrap() =
                        Some(("DO $$".into(), "syntax error".into()))
                },
                "bootstrapping roles: ",
            ),
        ];
        for (step, arrange, prefix) in cases {
            let (w, journal) = worker();
            arrange(&w);
            let id = provision(&w, "app").await;
            let db = w.metadata.database(id);
            assert_eq!(db.status, DatabaseStatus::Failed, "{step}");
            let error = db.error.unwrap_or_default();
            assert!(error.starts_with(prefix), "{step}: {error:?}");
            if prefix.contains("tenant") || prefix.contains("timeline") {
                assert!(error.ends_with(STORAGE_CAUSE), "{step}: {error:?}");
            }
            assert!(w.metadata.endpoint_ids().is_empty(), "{step}");
            assert!(w.storage.tenants.lock().unwrap().is_empty(), "{step}");
            assert!(w.computes.list().await.unwrap().is_empty(), "{step}");
            let torn_down = [
                "compute.delete",
                "storage.delete_tenant",
                "metadata.delete_endpoint",
            ];
            for entry in torn_down {
                assert_eq!(
                    has(&journal, entry),
                    step != "create_endpoint",
                    "{step}: {entry} in {:?}",
                    entries(&journal)
                );
            }
            if step != "create_endpoint" {
                let order = torn_down.map(|entry| pos(&journal, entry));
                assert!(order.is_sorted(), "{step}: {:?}", entries(&journal));
                assert!(
                    pos(&journal, "metadata.delete_endpoint")
                        < pos(&journal, "metadata.set_failed"),
                    "{step}"
                );
            }
            assert!(!has(&journal, "metadata.set_branch_ready"), "{step}");
        }
    }

    #[tokio::test]
    async fn a_teardown_failure_leaves_the_endpoint_for_reconcile() {
        let (w, journal) = worker();
        *w.storage.fail.lock().unwrap() = Some("create_timeline");
        *w.computes.fail_delete.lock().unwrap() = Some("docker is stuck".into());
        let id = provision(&w, "app").await;
        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Failed);
        assert!(db.error.unwrap().starts_with("creating timeline: "));
        assert_eq!(w.metadata.endpoint_ids().len(), 1);
        assert!(!has(&journal, "metadata.delete_endpoint"));
    }

    fn assert_torn_down_and_deleted(w: &FakeWorker, journal: &Journal, id: DatabaseId) {
        let db = w.metadata.database(id);
        let main = w.metadata.main(id);
        assert_eq!(db.status, DatabaseStatus::Deleted);
        assert_eq!(main.status, chelabase_metadata::BranchStatus::Deleted);
        assert_eq!(main.owner_password_enc, None);
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(w.storage.tenants.lock().unwrap().is_empty());
        let order = [
            pos(journal, "compute.delete"),
            pos(journal, "storage.delete_tenant"),
            pos(journal, "metadata.delete_endpoint"),
            pos(journal, "metadata.mark_branch_deleted"),
            pos(journal, "metadata.mark_deleted"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(journal));
        assert!(!has(journal, "metadata.set_failed"));
    }

    #[tokio::test]
    async fn a_delete_seen_at_the_recheck_tears_down_and_marks_deleted() {
        let (w, journal) = worker();
        // The first get_database is the skip check, the second the history window's read after
        // create_tenant, and the third step 7's re-check.
        w.metadata.state().deleting_on_get = Some(3);
        let id = provision(&w, "app").await;
        assert!(w.computes.list().await.unwrap().is_empty());
        assert_torn_down_and_deleted(&w, &journal, id);
        assert!(!has(&journal, "metadata.set_branch_ready"));
    }

    #[tokio::test]
    async fn a_delete_racing_set_ready_tears_down() {
        let (w, journal) = worker();
        w.metadata.state().deleting_before_set_ready = true;
        let id = provision(&w, "app").await;
        assert!(w.computes.list().await.unwrap().is_empty());
        assert_torn_down_and_deleted(&w, &journal, id);
        assert!(pos(&journal, "metadata.set_branch_ready") < pos(&journal, "compute.delete"));
    }

    // A failed teardown inside abandon leaves the row `deleting` and the endpoint row, for the
    // delete job or reconcile to finish; the job itself is done.
    #[tokio::test]
    async fn a_teardown_failure_while_abandoning_leaves_the_row_deleting() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange); 2] = [
            ("re-check", |w| w.metadata.state().deleting_on_get = Some(3)),
            ("set_ready", |w| {
                w.metadata.state().deleting_before_set_ready = true
            }),
        ];
        for (case, arrange) in cases {
            let (w, journal) = worker();
            arrange(&w);
            *w.storage.fail.lock().unwrap() = Some("delete_tenant");
            let id = w.metadata.insert_database("app", DatabaseStatus::Pending);

            w.handle_provision(id, "app").await.unwrap();

            let db = w.metadata.database(id);
            assert_eq!(db.status, DatabaseStatus::Deleting, "{case}");
            assert_eq!(w.metadata.endpoint_ids().len(), 1, "{case}");
            assert!(has(&journal, "storage.delete_tenant"), "{case}");
            assert!(!has(&journal, "metadata.delete_endpoint"), "{case}");
            assert!(!has(&journal, "metadata.mark_deleted"), "{case}");
            assert!(!has(&journal, "metadata.set_failed"), "{case}");
        }
    }

    // Ruling 8: a delete that finished between step 2 and create_tenant found no tenant to
    // delete. Provision must notice the endpoint row is gone and remove what it built since.
    #[tokio::test]
    async fn a_delete_finished_before_create_tenant_leaves_no_tenant() {
        let (w, journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        let state = w.metadata.state.clone();
        *w.storage.on_create_tenant.lock().unwrap() = Some(Box::new(move || {
            // What handle_delete leaves: no endpoint row, the rows tombstones.
            let mut state = state.lock().unwrap();
            let main = state.branches.get_mut(&BranchId::from(id)).unwrap();
            let ep = main.endpoint_id.take().unwrap();
            main.status = chelabase_metadata::BranchStatus::Deleted;
            state.endpoints.remove(&ep);
            state.databases.get_mut(&id).unwrap().status = DatabaseStatus::Deleted;
        }));

        w.handle_provision(id, "app").await.unwrap();

        assert!(
            w.storage.tenants.lock().unwrap().is_empty(),
            "orphan tenant"
        );
        assert!(w.computes.list().await.unwrap().is_empty());
        assert!(w.metadata.endpoint_ids().is_empty());
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Deleted);
        assert!(
            pos(&journal, "storage.create_timeline") < pos(&journal, "storage.delete_tenant"),
            "{:?}",
            entries(&journal)
        );
        assert!(has(&journal, "metadata.mark_deleted"));
        assert!(!has(&journal, "metadata.set_failed"));
        assert!(!has(&journal, "compute.create"));
        assert!(!has(&journal, "metadata.set_branch_ready"));
    }

    // Reconcile failed a stalled provision and tore down its endpoint row before the
    // re-check: provision cleans up what it built, and the row stays failed.
    #[tokio::test]
    async fn an_endpoint_gone_after_reconcile_failed_the_row_keeps_it_failed() {
        let (w, journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        let state = w.metadata.state.clone();
        *w.storage.on_create_tenant.lock().unwrap() = Some(Box::new(move || {
            // What FailStale then RetryTeardown leave: the rows failed, no endpoint row.
            let mut state = state.lock().unwrap();
            let main = state.branches.get_mut(&BranchId::from(id)).unwrap();
            let ep = main.endpoint_id.take().unwrap();
            main.status = chelabase_metadata::BranchStatus::Failed;
            state.endpoints.remove(&ep);
            let db = state.databases.get_mut(&id).unwrap();
            db.status = DatabaseStatus::Failed;
            db.error = Some("provisioning timed out".into());
        }));

        w.handle_provision(id, "app").await.unwrap();

        assert!(
            w.storage.tenants.lock().unwrap().is_empty(),
            "orphan tenant"
        );
        assert!(
            pos(&journal, "storage.create_timeline") < pos(&journal, "storage.delete_tenant"),
            "{:?}",
            entries(&journal)
        );
        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Failed);
        assert_eq!(db.error.as_deref(), Some("provisioning timed out"));
        assert!(!has(&journal, "metadata.mark_deleted"));
        assert!(!has(&journal, "compute.create"));
    }

    // Fix round 1, I1: reconcile failed a provision that ran past the stale limit after its
    // endpoint check (the endpoint row still there). set_branch_ready then refuses because main
    // is failed, not because of a delete: provision tears down what it built, and the rows stay
    // failed instead of becoming tombstones.
    #[tokio::test]
    async fn a_stale_failure_before_set_ready_keeps_the_row_failed() {
        let (w, journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        let state = w.metadata.state.clone();
        *w.storage.on_create_tenant.lock().unwrap() = Some(Box::new(move || {
            // What FailStale does (before its teardown reaches this endpoint).
            let mut state = state.lock().unwrap();
            let db = state.databases.get_mut(&id).unwrap();
            db.status = DatabaseStatus::Failed;
            db.error = Some("provisioning timed out".into());
            let main = state.branches.get_mut(&BranchId::from(id)).unwrap();
            main.status = chelabase_metadata::BranchStatus::Failed;
            main.error = Some("provisioning timed out".into());
        }));

        w.handle_provision(id, "app").await.unwrap();

        let (db, main) = (w.metadata.database(id), w.metadata.main(id));
        assert_eq!(db.status, DatabaseStatus::Failed, "{:?}", entries(&journal));
        assert_eq!(db.error.as_deref(), Some("provisioning timed out"));
        assert_eq!(main.status, chelabase_metadata::BranchStatus::Failed);
        assert!(has(&journal, "metadata.set_branch_ready"));
        assert!(!has(&journal, "metadata.mark_deleted"));
        assert!(!has(&journal, "metadata.mark_branch_deleted"));
        assert!(!has(&journal, "metadata.set_failed"));
        // What it built is gone.
        assert!(w.computes.list().await.unwrap().is_empty());
        assert!(w.storage.tenants.lock().unwrap().is_empty());
        assert!(w.metadata.endpoint_ids().is_empty());
    }

    // A job redelivered after an interrupted attempt finds the row already linked.
    #[tokio::test]
    async fn a_retried_provision_says_the_previous_attempt_is_cleaned_up() {
        let (w, _journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        // The first attempt's set_branch_ready error hid no commit: the rows are still
        // provisioning.
        {
            let mut state = w.metadata.state();
            state.databases.get_mut(&id).unwrap().status = DatabaseStatus::Provisioning;
            state.branches.get_mut(&BranchId::from(id)).unwrap().status =
                chelabase_metadata::BranchStatus::Provisioning;
        }

        w.handle_provision(id, "app").await.unwrap();

        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Failed);
        assert_eq!(
            db.error.as_deref(),
            Some(
                "recording the endpoint: a previous attempt was interrupted; its resources will \
                 be cleaned up"
            )
        );
        // Left for reconcile, which tears down the endpoint of a failed database.
        assert_eq!(w.metadata.endpoint_ids(), [ep]);
    }

    #[tokio::test]
    async fn already_ready_deleted_or_missing_rows_are_skipped() {
        for status in [
            DatabaseStatus::Ready,
            DatabaseStatus::Deleting,
            DatabaseStatus::Deleted,
        ] {
            let (w, journal) = worker();
            let id = w.metadata.insert_database("app", status);
            w.handle_provision(id, "app").await.unwrap();
            assert_eq!(w.metadata.database(id).status, status);
            assert!(
                entries(&journal).is_empty(),
                "{status:?}: {:?}",
                entries(&journal)
            );
        }
        let (w, journal) = worker();
        w.handle_provision(DatabaseId::new(), "app").await.unwrap();
        assert!(entries(&journal).is_empty());
    }

    #[tokio::test]
    async fn a_reserved_name_fails_without_creating_anything() {
        let (w, journal) = worker();
        let id = provision(&w, "template1").await;
        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Failed);
        assert!(db.error.unwrap().contains("reserved"));
        assert!(!has(&journal, "metadata.create_endpoint_for_branch"));
        assert!(!has(&journal, "storage."));
        assert!(!has(&journal, "compute."));
    }

    #[tokio::test]
    async fn metadata_trouble_is_retried() {
        let (w, _journal) = worker();
        w.metadata.state().fail = Some("get_database");
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        assert!(w.handle_provision(id, "app").await.is_err());
    }

    // D6: ready means compute_ctl says running AND the database answers.
    #[tokio::test]
    async fn ready_needs_running_as_well_as_select_1() {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_millis(60);
        w.computes.create("ep-s", "t").await.unwrap();
        w.pusher.set_status("ep-s", chelabase_neon::reconfigure::ComputeCtlStatus::Other("init".into()));
        let err = w.wait_until_ready("ep-s", "app").await.unwrap_err();
        assert!(err.contains("init"), "{err}");
        w.pusher.set_status("ep-s", chelabase_neon::reconfigure::ComputeCtlStatus::Running);
        w.wait_until_ready("ep-s", "app").await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_compute_ctl_ends_the_wait_at_once() {
        let (mut w, _journal) = worker();
        w.ready_timeout = Duration::from_secs(5);
        w.computes.create("ep-f", "t").await.unwrap();
        w.pusher.set_status(
            "ep-f",
            chelabase_neon::reconfigure::ComputeCtlStatus::Failed("migration 2 failed: boom".into()),
        );
        let started = Instant::now();
        let err = w.wait_until_ready("ep-f", "app").await.unwrap_err();
        assert!(err.contains("migration 2 failed: boom"), "{err}");
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    }
}
