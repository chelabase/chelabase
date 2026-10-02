//! Resetting the owner's password, in step 3's order: metadata first, then the running
//! compute, then the proxies' caches, then the flag.

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use cheladb_metadata::BranchStatus;
use cheladb_nats::RESET_NOT_APPLIED;
use cheladb_neon::ComputeBackend;
use cheladb_secrets::{Purpose, seal_credential};
use cheladb_types::BranchId;
#[cfg(test)]
use cheladb_types::DatabaseId;
use tracing::{info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{OWNER_ROLE, Worker, credentials};

/// How long `apply_pending` waits for Docker to find the compute, while it holds the
/// endpoint's lock.
const DOCKER_TIMEOUT: Duration = Duration::from_secs(5);

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Replaces the owner's password. The errors are the reply to the orchestrator and never
    /// contain a password. Once the new password is stored, a failure to apply it answers
    /// [`RESET_NOT_APPLIED`]: reveal already shows the new password, and reconcile retries.
    /// The branch's own owner password; main's branch id is its database's.
    pub async fn reset_password(&self, id: BranchId) -> Result<(), String> {
        let main = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("not found")?;
        if main.status != BranchStatus::Ready {
            return Err("not ready".into());
        }
        let endpoint_id = main.endpoint_id.ok_or("credentials unavailable")?;

        // Step 1: the sealed password, the verifier and the flag, in one transaction.
        let password = credentials::generate_password();
        let verifier = cheladb_neon::scram::verifier(&password);
        let sealed = seal_credential(&self.key, &main.id.to_string(), Purpose::Owner, &password);
        let replaced = self
            .metadata
            .replace_owner_secret(main.id, &endpoint_id, OWNER_ROLE, &verifier, &sealed)
            .await
            .map_err(|e| e.to_string())?;
        if !replaced {
            return Err("not ready".into());
        }

        // Steps 2-4.
        if let Err(err) = self.apply_pending(&endpoint_id).await {
            warn!(branch_id = %id, %endpoint_id, "reset stored but not applied; reconcile will retry: {err:#}");
            return Err(RESET_NOT_APPLIED.into());
        }
        info!(branch_id = %id, %endpoint_id, "owner password reset");
        Ok(())
    }

    /// Reset steps 2–4 for one endpoint; reconcile calls it too.
    pub async fn apply_pending(&self, endpoint_id: &str) -> anyhow::Result<()> {
        // One at a time per endpoint, from the read to the clear. Otherwise a push holding an
        // older verifier (reconcile's, say) could land after a newer reset's push and clear:
        // the compute would keep the old password with nothing left to retry.
        let _lock = self
            .metadata
            .lock_endpoint(endpoint_id)
            .await
            .context("locking the endpoint")?;
        self.apply_pending_locked(endpoint_id, &[]).await
    }

    /// [`Self::apply_pending`] for a caller that already holds `endpoint_id`'s lock. Publishes
    /// `/password_updated` for every role row on the endpoint plus `also_publish` (a deleted
    /// role's name), since a retry doesn't know which role changed.
    pub(crate) async fn apply_pending_locked(
        &self,
        endpoint_id: &str,
        also_publish: &[&str],
    ) -> anyhow::Result<()> {
        let Some(endpoint) = self
            .metadata
            .neon_endpoint(endpoint_id)
            .await
            .context("loading the endpoint")?
        else {
            // Deleted meanwhile: nothing left to apply.
            return Ok(());
        };
        let (roles, applied_version) = self
            .metadata
            .neon_roles_versioned(endpoint_id)
            .await
            .context("loading the roles")?;
        // The flag is cleared only if the roles are still at this version once applied.
        if !roles.iter().any(|r| r.name == OWNER_ROLE) {
            bail!("the endpoint has no {OWNER_ROLE} role");
        }
        let role_names: Vec<String> = roles.iter().map(|r| r.name.clone()).collect();

        // Step 2: a compute that isn't running pulls the current spec when it boots.
        // Bounded: a hung Docker daemon would otherwise keep the endpoint's lock forever.
        let control_addr =
            tokio::time::timeout(DOCKER_TIMEOUT, self.computes.control_addr(endpoint_id))
                .await
                .map_err(|_| {
                    anyhow!(
                        "Docker didn't answer within {}s while finding the compute for \
                         {endpoint_id}",
                        DOCKER_TIMEOUT.as_secs()
                    )
                })?
                .with_context(|| format!("finding the compute for {endpoint_id}"))?;
        if let Some(control_addr) = control_addr {
            let databases = self
                .metadata
                .neon_databases(endpoint_id)
                .await
                .context("loading the databases")?;
            let config = cheladb_neon::spec::stored_endpoint_config(
                &self.storage,
                &endpoint,
                roles,
                databases,
                self.pusher.jwks(),
                Some(cheladb_neon::spec::ddl_url(
                    &self.control_plane_uri,
                    endpoint_id,
                )),
            )
            .await
            .context("building the spec")?;
            self.pusher
                .push(control_addr, endpoint_id, &config)
                .await
                .context("pushing the spec")?;
        }

        // Step 3.
        for role in role_names
            .iter()
            .map(String::as_str)
            .chain(also_publish.iter().copied())
        {
            let receivers = self
                .cache
                .password_updated(endpoint_id, role)
                .await
                .with_context(|| format!("publishing password_updated for {role}"))?;
            if receivers == 0 {
                bail!(
                    "no proxy received /password_updated for {role}; the old password stays \
                     cached until its TTL runs out"
                );
            }
        }

        // Step 4. `false` means a newer reset landed meanwhile; its flag stays for it.
        let cleared = self
            .metadata
            .clear_needs_reconfigure(endpoint_id, applied_version)
            .await
            .context("clearing needs_reconfigure")?;
        if !cleared {
            info!(%endpoint_id, "a newer role write is pending; leaving its flag set");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use cheladb_metadata::DatabaseStatus;
    use cheladb_secrets::open_credential;
    use tokio::sync::oneshot;

    use super::*;
    use crate::fakes::{FakeState, FakeWorker, Journal, entries, position, worker};

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    /// A ready database, provisioned through the fakes; the journal starts empty.
    async fn ready(w: &FakeWorker, journal: &Journal) -> (DatabaseId, String) {
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        journal.lock().unwrap().clear();
        (id, ep)
    }

    /// `(sealed password, owner verifier, needs_reconfigure)` as metadata holds them.
    fn secret(w: &FakeWorker, id: DatabaseId, ep: &str) -> (String, String, bool) {
        let sealed = w.metadata.main(id).owner_password_enc.unwrap();
        let state = w.metadata.state();
        let endpoint = &state.endpoints[ep];
        let role = endpoint
            .roles
            .iter()
            .find(|r| r.name == OWNER_ROLE)
            .unwrap();
        (
            sealed,
            role.scram_verifier.clone(),
            endpoint.needs_reconfigure,
        )
    }

    /// The stored sealed password opens, and hashes to the stored verifier under its salt.
    fn assert_consistent(w: &FakeWorker, id: DatabaseId, ep: &str) {
        let (sealed, verifier, _) = secret(w, id, ep);
        // AAD = main's id, which is the database's.
        let password = open_credential(
            &w.key,
            &BranchId::from(id).to_string(),
            Purpose::Owner,
            &sealed,
        )
        .unwrap();
        let params = verifier.split('$').nth(1).unwrap();
        let (iterations, salt) = params.split_once(':').unwrap();
        let salt = STANDARD.decode(salt).unwrap();
        assert_eq!(
            cheladb_neon::scram::verifier_with_salt(&password, &salt, iterations.parse().unwrap()),
            verifier
        );
    }

    #[tokio::test]
    async fn reset_runs_in_order() {
        let (w, journal) = worker();
        let (id, ep) = ready(&w, &journal).await;
        let (old_sealed, old_verifier, _) = secret(&w, id, &ep);

        w.reset_password(BranchId::from(id)).await.unwrap();

        let order = [
            pos(&journal, "metadata.replace_owner_secret"),
            pos(&journal, "metadata.lock_endpoint"),
            pos(&journal, &format!("metadata.neon_roles_versioned {ep}")),
            pos(&journal, &format!("pusher.push {ep}")),
            pos(&journal, &format!("cache.password_updated {ep}")),
            pos(&journal, "metadata.clear_needs_reconfigure"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        let (sealed, verifier, flag) = secret(&w, id, &ep);
        assert!(!flag);
        assert_ne!(sealed, old_sealed);
        assert_ne!(verifier, old_verifier);
        assert_consistent(&w, id, &ep);
        // The compute got the new verifier.
        let pushed = w.pusher.pushed.lock().unwrap();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].0, ep);
        assert!(pushed[0].1.to_string().contains(&verifier));
    }

    #[tokio::test]
    async fn reset_without_a_running_compute_publishes_without_pushing() {
        let (w, journal) = worker();
        let (id, ep) = ready(&w, &journal).await;
        w.computes.set_running(&ep, false);

        w.reset_password(BranchId::from(id)).await.unwrap();

        assert!(position(&journal, "pusher.push").is_none());
        assert!(
            pos(&journal, &format!("cache.password_updated {ep}"))
                < pos(&journal, "metadata.clear_needs_reconfigure")
        );
        assert!(!secret(&w, id, &ep).2);
        assert_consistent(&w, id, &ep);
    }

    #[tokio::test]
    async fn a_failed_push_or_publish_keeps_the_flag_and_the_new_password() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange); 3] = [
            ("push", |w| {
                *w.pusher.fail.lock().unwrap() = Some("compute_ctl said 500".into())
            }),
            ("publish", |w| {
                *w.cache.fail.lock().unwrap() = Some("valkey is down".into())
            }),
            ("no receivers", |w| *w.cache.receivers.lock().unwrap() = 0),
        ];
        for (case, arrange) in cases {
            let (w, journal) = worker();
            let (id, ep) = ready(&w, &journal).await;
            let (old_sealed, old_verifier, _) = secret(&w, id, &ep);
            arrange(&w);

            let err = w.reset_password(BranchId::from(id)).await.unwrap_err();

            assert_eq!(err, RESET_NOT_APPLIED, "{case}");
            let (sealed, verifier, flag) = secret(&w, id, &ep);
            assert!(flag, "{case}");
            assert_ne!(sealed, old_sealed, "{case}");
            assert_ne!(verifier, old_verifier, "{case}");
            assert_consistent(&w, id, &ep);
            assert!(
                position(&journal, "metadata.clear_needs_reconfigure").is_none(),
                "{case}: {:?}",
                entries(&journal)
            );
        }
    }

    #[tokio::test]
    async fn reset_refuses_missing_not_ready_and_unlinked() {
        let (w, journal) = worker();
        assert_eq!(
            w.reset_password(BranchId::new()).await.unwrap_err(),
            "not found"
        );

        let pending = w.metadata.insert_database("p", DatabaseStatus::Pending);
        assert_eq!(w.reset_password(BranchId::from(pending)).await.unwrap_err(), "not ready");

        let unlinked = w.metadata.insert_database("u", DatabaseStatus::Ready);
        assert_eq!(
            w.reset_password(BranchId::from(unlinked)).await.unwrap_err(),
            "credentials unavailable"
        );

        // Linked, but the endpoint row is gone: replace_owner_secret refuses.
        let gone = w.metadata.insert_database("g", DatabaseStatus::Ready);
        w.metadata
            .state()
            .branches
            .get_mut(&BranchId::from(gone))
            .unwrap()
            .endpoint_id = Some("ep-000000000000".into());
        assert_eq!(w.reset_password(BranchId::from(gone)).await.unwrap_err(), "not ready");

        assert!(position(&journal, "pusher.push").is_none());
        assert!(position(&journal, "cache.password_updated").is_none());
    }

    // Review Focus 1.
    #[tokio::test]
    async fn apply_pending_keeps_the_flag_when_a_newer_reset_landed() {
        let (w, journal) = worker();
        let (id, ep) = ready(&w, &journal).await;
        // A second reset commits while the first one's push is in flight.
        let state = w.metadata.state.clone();
        let hook_ep = ep.clone();
        *w.pusher.on_push.lock().unwrap() = Some(Box::new(move || {
            store_verifier(
                &state,
                &hook_ep,
                &cheladb_neon::scram::verifier("the-newer-password"),
            );
            Box::pin(async {})
        }));

        w.reset_password(BranchId::from(id)).await.unwrap();

        assert!(position(&journal, "metadata.clear_needs_reconfigure").is_some());
        assert!(secret(&w, id, &ep).2, "the newer reset's flag was cleared");
    }

    /// What a reset's metadata transaction does: a new owner verifier, and the flag.
    fn store_verifier(state: &Mutex<FakeState>, ep: &str, verifier: &str) {
        let mut state = state.lock().unwrap();
        let endpoint = state.endpoints.get_mut(ep).unwrap();
        let role = endpoint
            .roles
            .iter_mut()
            .find(|r| r.name == OWNER_ROLE)
            .unwrap();
        role.scram_verifier = verifier.to_string();
        endpoint.needs_reconfigure = true;
        endpoint.roles_version += 1;
    }

    // Review I1: a stale push (reconcile holding verifier A) must not land after a newer
    // reset's push of verifier B.
    #[tokio::test]
    async fn a_stale_apply_pending_never_lands_after_a_newer_one() {
        let (w, journal) = worker();
        let (id, ep) = ready(&w, &journal).await;
        let (va, vb) = (
            cheladb_neon::scram::verifier("password-a"),
            cheladb_neon::scram::verifier("password-b"),
        );
        // Reset A committed; reconcile's apply_pending (below, `a`) reads A and pushes it.
        store_verifier(&w.metadata.state, &ep, &va);
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let (state, hook_ep, hook_vb) = (w.metadata.state.clone(), ep.clone(), vb.clone());
        *w.pusher.on_push.lock().unwrap() = Some(Box::new(move || {
            Box::pin(async move {
                // Reset B commits while A's push is in flight...
                store_verifier(&state, &hook_ep, &hook_vb);
                entered_tx.send(()).unwrap();
                // ...and A's push stalls until B's apply_pending has had its chance to run.
                release_rx.await.unwrap();
            })
        }));

        let a = w.apply_pending(&ep);
        let b = async {
            entered_rx.await.unwrap();
            let release = async {
                tokio::task::yield_now().await;
                release_tx.send(()).unwrap();
            };
            tokio::join!(w.apply_pending(&ep), release).0
        };
        let (a, b) = tokio::join!(a, b);
        a.unwrap();
        b.unwrap();

        let pushed = w.pusher.pushed.lock().unwrap();
        let last = pushed.last().unwrap().1.to_string();
        assert!(
            last.contains(&vb) && !last.contains(&va),
            "the compute was left with a stale verifier: {:?}",
            entries(&journal)
        );
        drop(pushed);
        let (_, stored, flag) = secret(&w, id, &ep);
        assert_eq!(stored, vb);
        assert!(!flag, "B was applied, so its flag is cleared");
    }

    // A hung Docker daemon must not stall apply_pending while it holds the endpoint's lock.
    #[tokio::test(start_paused = true)]
    async fn a_hung_docker_times_out_and_keeps_the_flag() {
        let (w, journal) = worker();
        let (id, ep) = ready(&w, &journal).await;
        *w.computes.hang_control_addr.lock().unwrap() = true;

        let err = tokio::time::timeout(std::time::Duration::from_secs(60), w.reset_password(BranchId::from(id)))
            .await
            .expect("apply_pending gave up on Docker")
            .unwrap_err();

        assert_eq!(err, RESET_NOT_APPLIED);
        assert!(secret(&w, id, &ep).2);
        let err = w.apply_pending(&ep).await.unwrap_err();
        assert!(format!("{err:#}").contains(&ep), "{err:#}");
        assert!(position(&journal, "pusher.push").is_none());
        // The lock was released: nothing waits on it once Docker answers again.
        *w.computes.hang_control_addr.lock().unwrap() = false;
        w.apply_pending(&ep).await.unwrap();
        assert!(!secret(&w, id, &ep).2);
    }

    // R3: every role row on the endpoint, plus the extra names (a deleted role's).
    #[tokio::test]
    async fn apply_pending_locked_publishes_for_every_role_and_the_extras() {
        let (w, journal) = worker();
        let (_id, ep) = ready(&w, &journal).await;
        w.metadata
            .state()
            .endpoints
            .get_mut(&ep)
            .unwrap()
            .roles
            .push(cheladb_metadata::NeonRole {
                name: "app".into(),
                scram_verifier: cheladb_neon::scram::verifier("x"),
            });

        let _lock = w.metadata.lock_endpoint(&ep).await.unwrap();
        w.apply_pending_locked(&ep, &["gone"]).await.unwrap();

        for role in [OWNER_ROLE, "app", "gone"] {
            pos(&journal, &format!("cache.password_updated {ep} {role}"));
        }
    }

    #[tokio::test]
    async fn apply_pending_for_a_deleted_endpoint_is_ok() {
        let (w, _journal) = worker();
        w.apply_pending("ep-000000000000").await.unwrap();
    }
}
