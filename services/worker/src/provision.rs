//! Provisioning a database on Neon: an endpoint row, a tenant and timeline on the storage
//! controller, a compute, and the data API roles inside it.

use cheladb_metadata::{DatabaseStatus, NewEndpoint, ReadyDatabase};
use cheladb_neon::{ComputeBackend, NeonId};
use cheladb_secrets::{Purpose, seal_credential};
use cheladb_types::DatabaseId;
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::names::sanitize_db_name;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::reconcile::should_abort_provision;
use crate::{OWNER_ROLE, Worker, credentials};

/// The idempotent SQL run as cloud_admin in every new database: the data API's roles, and with
/// `owner_superuser` the owner made SUPERUSER. compute_ctl never drops roles it doesn't know,
/// so these survive later reconfigures.
pub fn bootstrap_sql(owner_superuser: bool) -> String {
    let mut sql = String::from(
        "DO $$
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
        match self.build(&rec).await {
            Ok(Built::Compute) => self.finish(id, &rec).await,
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

    /// Step 2: ids and secrets; the endpoint, its role and database, and the link, in one
    /// transaction.
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
        let verifier = cheladb_neon::scram::verifier(&rec.password);
        let token_hash = sha256_hex(&rec.compute_token);
        let endpoint = NewEndpoint {
            id: &rec.endpoint_id,
            tenant_id: rec.tenant.as_str(),
            timeline_id: rec.timeline.as_str(),
            compute_token_sha256: &token_hash,
            owner_role: OWNER_ROLE,
            owner_verifier: &verifier,
            database_name: &rec.database_name,
        };
        match self
            .metadata
            .create_endpoint_for_database(id, &endpoint)
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
    async fn build(&self, rec: &Recorded) -> Result<Built, String> {
        let ep = rec.endpoint_id.as_str();
        self.storage
            .create_tenant(&rec.tenant)
            .await
            .map_err(|e| format!("creating tenant: {e:#}"))?;
        self.storage
            .create_timeline(&rec.tenant, &rec.timeline)
            .await
            .map_err(|e| format!("creating timeline: {e:#}"))?;
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
        self.computes
            .create(ep, &rec.compute_token)
            .await
            .map_err(|e| format!("starting compute: {e:#}"))?;
        self.wait_until_ready(ep, &rec.database_name).await?;
        self.computes
            .admin_sql(ep, &rec.database_name, &bootstrap_sql(self.owner_superuser))
            .await
            .map_err(|e| format!("bootstrapping roles: {e:#}"))?;
        Ok(Built::Compute)
    }

    /// Step 5: the compute has applied its spec once the new database answers.
    async fn wait_until_ready(&self, ep: &str, database_name: &str) -> Result<(), String> {
        let deadline = Instant::now() + self.ready_timeout;
        loop {
            match self.computes.admin_sql(ep, database_name, "SELECT 1").await {
                Ok(_) => return Ok(()),
                Err(err) if Instant::now() >= deadline => {
                    return Err(format!("waiting for the compute: {err:#}"));
                }
                Err(_) => tokio::time::sleep(self.ready_poll).await,
            }
        }
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

    /// Step 7: re-check for a delete that landed meanwhile, then `set_ready`.
    async fn finish(&self, id: DatabaseId, rec: &Recorded) -> Result<(), String> {
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
        let sealed = seal_credential(&self.key, &id.to_string(), Purpose::Owner, &rec.password);
        let ready = ReadyDatabase {
            database_name: &rec.database_name,
            owner_role: OWNER_ROLE,
            owner_password_enc: &sealed,
        };
        // An error here may hide a commit, so nothing is torn down: the retried job either
        // skips a ready row, or fails to record a second endpoint and reconcile cleans up.
        let set = self
            .metadata
            .set_ready(id, &rec.endpoint_id, ready)
            .await
            .map_err(|e| e.to_string())?;
        if !set {
            info!(database_id = %id, "deleted just before ready; tearing down");
            return self.abandon(id, rec).await;
        }
        info!(database_id = %id, endpoint_id = %rec.endpoint_id, "database ready");
        Ok(())
    }

    /// The endpoint row vanished after step 2: a delete finished, or reconcile failed a
    /// stalled provision and tore its endpoint down. Tear down what this provision built since;
    /// mark the row deleted only for a delete. A failed row stays as reconcile left it.
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
            _ => self
                .metadata
                .mark_deleted(id)
                .await
                .map_err(|e| e.to_string()),
        }
    }

    /// A delete won the race: tear down, then mark the row deleted. A failed teardown leaves
    /// the row `deleting` for the delete job or reconcile to finish.
    async fn abandon(&self, id: DatabaseId, rec: &Recorded) -> Result<(), String> {
        if self.teardown_logged(rec).await {
            self.metadata
                .mark_deleted(id)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use cheladb_metadata::DatabaseStatus;
    use cheladb_secrets::{Purpose, open_credential};

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
        let expected = "DO $$
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
        assert_eq!(db.status, DatabaseStatus::Ready, "{:?}", db.error);
        assert_eq!(db.database_name.as_deref(), Some("web_1_prod"));
        assert_eq!(db.owner_role.as_deref(), Some("postgres"));
        let ep = db.endpoint_id.clone().unwrap();
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&ep));

        let state = w.metadata.state();
        let stored = &state.endpoints[&ep];
        assert_eq!(stored.roles.len(), 1);
        assert_eq!(stored.roles[0].name, "postgres");
        assert!(stored.roles[0].scram_verifier.starts_with("SCRAM-SHA-256$"));
        assert_eq!(stored.databases.len(), 1);
        assert_eq!(stored.databases[0].name, "web_1_prod");
        assert_eq!(stored.databases[0].owner, "postgres");
        assert!(w.storage.has_tenant(&stored.endpoint.tenant_id));
        let token = w.computes.token_of(&ep).unwrap();
        assert_eq!(sha256_hex(&token), stored.endpoint.compute_token_sha256);
        drop(state);

        let sql = w.computes.sql.lock().unwrap().clone();
        let bootstrap = sql
            .iter()
            .find(|(e, d, s)| e == &ep && d == "web_1_prod" && s.starts_with("DO $$"))
            .expect("bootstrap ran on the new database");
        assert!(bootstrap.2.contains("CREATE ROLE anon NOLOGIN"));
        assert!(!bootstrap.2.contains("SUPERUSER"));

        let password = open_credential(
            &w.key,
            &id.to_string(),
            Purpose::Owner,
            db.owner_password_enc.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(password.len(), 32);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));

        let order = [
            pos(&journal, "metadata.create_endpoint_for_database"),
            pos(&journal, "storage.create_tenant"),
            pos(&journal, "storage.create_timeline"),
            pos(&journal, "compute.create"),
            pos(&journal, &format!("compute.sql {ep} web_1_prod SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} web_1_prod DO $$")),
            pos(&journal, "metadata.set_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
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
                |w| w.metadata.state().fail = Some("create_endpoint_for_database"),
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
            assert!(!has(&journal, "metadata.set_ready"), "{step}");
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
        assert_eq!(db.status, DatabaseStatus::Deleted);
        assert_eq!(db.owner_password_enc, None);
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(w.storage.tenants.lock().unwrap().is_empty());
        let order = [
            pos(journal, "compute.delete"),
            pos(journal, "storage.delete_tenant"),
            pos(journal, "metadata.delete_endpoint"),
            pos(journal, "metadata.mark_deleted"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(journal));
        assert!(!has(journal, "metadata.set_failed"));
    }

    #[tokio::test]
    async fn a_delete_seen_at_the_recheck_tears_down_and_marks_deleted() {
        let (w, journal) = worker();
        // The first get_database is the skip check; the second is step 7's re-check.
        w.metadata.state().deleting_on_get = Some(2);
        let id = provision(&w, "app").await;
        assert!(w.computes.list().await.unwrap().is_empty());
        assert_torn_down_and_deleted(&w, &journal, id);
        assert!(!has(&journal, "metadata.set_ready"));
    }

    #[tokio::test]
    async fn a_delete_racing_set_ready_tears_down() {
        let (w, journal) = worker();
        w.metadata.state().deleting_before_set_ready = true;
        let id = provision(&w, "app").await;
        assert!(w.computes.list().await.unwrap().is_empty());
        assert_torn_down_and_deleted(&w, &journal, id);
        assert!(pos(&journal, "metadata.set_ready") < pos(&journal, "compute.delete"));
    }

    // A failed teardown inside abandon leaves the row `deleting` and the endpoint row, for the
    // delete job or reconcile to finish; the job itself is done.
    #[tokio::test]
    async fn a_teardown_failure_while_abandoning_leaves_the_row_deleting() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange); 2] = [
            ("re-check", |w| w.metadata.state().deleting_on_get = Some(2)),
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
            // What handle_delete leaves: no endpoint row, the row a tombstone.
            let mut state = state.lock().unwrap();
            let ep = state.databases[&id].endpoint_id.clone().unwrap();
            state.endpoints.remove(&ep);
            let db = state.databases.get_mut(&id).unwrap();
            db.endpoint_id = None;
            db.status = DatabaseStatus::Deleted;
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
        assert!(!has(&journal, "metadata.set_ready"));
    }

    // Reconcile failed a stalled provision and tore down its endpoint row before the
    // re-check: provision cleans up what it built, and the row stays failed.
    #[tokio::test]
    async fn an_endpoint_gone_after_reconcile_failed_the_row_keeps_it_failed() {
        let (w, journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        let state = w.metadata.state.clone();
        *w.storage.on_create_tenant.lock().unwrap() = Some(Box::new(move || {
            // What FailStale then RetryTeardown leave: the row failed, no endpoint row.
            let mut state = state.lock().unwrap();
            let ep = state.databases[&id].endpoint_id.clone().unwrap();
            state.endpoints.remove(&ep);
            let db = state.databases.get_mut(&id).unwrap();
            db.endpoint_id = None;
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

    // A job redelivered after an interrupted attempt finds the row already linked.
    #[tokio::test]
    async fn a_retried_provision_says_the_previous_attempt_is_cleaned_up() {
        let (w, _journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let ep = w.metadata.database(id).endpoint_id.unwrap();
        // The first attempt's set_ready error hid no commit: the row is still provisioning.
        w.metadata.state().databases.get_mut(&id).unwrap().status = DatabaseStatus::Provisioning;

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
        assert!(!has(&journal, "metadata.create_endpoint_for_database"));
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
}
