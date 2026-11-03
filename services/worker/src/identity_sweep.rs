//! T7: the identity sweep (members hub spec Part 6, "Identity-provider offboarding"). Every
//! `CHELABASE_IDENTITY_SWEEP_SECS` the Zitadel state of each member holding a live personal
//! login is read; a deactivated, locked or deleted user has every login revoked through the
//! same metadata write the Actions webhook uses (which marks the member offboarded, D7). It is
//! the backstop for a missed webhook, both ways: a marked member whose user reads active again
//! loses the mark.
//!
//! Only Zitadel-era members are swept. A member whose id comes from the identity service (a
//! UUID; Zitadel's ids are digits) is skipped: Zitadel would answer 404 and the sweep would strip
//! their logins. Their removal and suspension revoke inline (m9), so nothing is lost; m13 removes
//! the sweep with Zitadel.

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use chelabase_metadata::personal_logins::LoginEndpoint;
use chelabase_neon::ComputeBackend;
use chelabase_types::BranchId;
use chelabase_zitadel::client::UserState;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::Ordering;
use tracing::{info, warn};
use uuid::Uuid;

/// A Zitadel user as one pass read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Active,
    /// Deactivated or locked.
    Inactive,
    /// A 404.
    Missing,
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// One pass: `(checked, revoked)`, the members holding a live login whose Zitadel state was
    /// read, and the members offboarded.
    ///
    /// A lookup that fails (Zitadel down, a 5xx, a refusal) revokes nothing for that member and
    /// is retried next pass. EOS-H2: a 404 is believed only in a pass that found some user;
    /// a pass of nothing but 404s (beside failures) is a misconfigured instance or token, not
    /// every member deleted at once, so it revokes nothing (a lone deleted member is left to the
    /// webhook and `VALID UNTIL`). D7: the members marked offboarded are looked up too, and one
    /// whose user reads active again (reactivated or unlocked while no webhook arrived) loses
    /// the mark; its logins stay revoked. Without a Zitadel client the pass does nothing and
    /// says so once (EOS-4).
    pub async fn sweep_identities_once(&self) -> (usize, usize) {
        let Some(zitadel) = self.zitadel.as_ref() else {
            if !self.identity_sweep_warned.swap(true, Ordering::Relaxed) {
                warn!("identity sweep: no Zitadel client (CHELABASE_ZITADEL_ADMIN_PAT_FILE and CHELABASE_AUTH_HOST); offboarded users keep their personal logins until the Actions webhook or VALID UNTIL ends them");
            }
            return (0, 0);
        };
        let members = match self.metadata.members_with_live_logins().await {
            Ok(members) => zitadel_members(members),
            Err(e) => {
                warn!("identity sweep: reading the members with live logins: {e}");
                return (0, 0);
            }
        };
        let marked = match self.metadata.offboarded_members().await {
            Ok(marked) => zitadel_members(marked),
            Err(e) => {
                warn!("identity sweep: reading the offboarded members (their marks are checked next pass): {e}");
                Vec::new()
            }
        };
        // One lookup per Zitadel user, even when it is a member of several tenants.
        let mut states: HashMap<String, Option<Seen>> = HashMap::new();
        for (_, member, zitadel_user) in members.iter().chain(marked.iter()) {
            if states.contains_key(zitadel_user) {
                continue;
            }
            let state = match zitadel.get_user(zitadel_user).await {
                Ok(Some(user)) if matches!(user.state, UserState::Inactive | UserState::Locked) => {
                    Some(Seen::Inactive)
                }
                Ok(Some(_)) => Some(Seen::Active),
                Ok(None) => Some(Seen::Missing),
                Err(e) => {
                    warn!(%member, "identity sweep: reading the Zitadel user failed; retried next pass: {e}");
                    None
                }
            };
            states.insert(zitadel_user.clone(), state);
        }
        let found_a_user = states
            .values()
            .any(|s| matches!(s, Some(Seen::Active | Seen::Inactive)));
        let mut missing_kept = 0usize;
        let (mut checked, mut revoked) = (0, 0);
        for (tenant, member, zitadel_user) in &members {
            let Some(state) = states.get(zitadel_user).copied().flatten() else {
                continue;
            };
            checked += 1;
            match state {
                Seen::Active => continue,
                Seen::Missing if !found_a_user => {
                    missing_kept += 1;
                    continue;
                }
                Seen::Inactive | Seen::Missing => {}
            }
            match self
                .metadata
                .revoke_logins_for_identity_user(*tenant, zitadel_user)
                .await
            {
                Ok(endpoints) => {
                    revoked += 1;
                    self.publish_revocation(endpoints).await;
                }
                Err(e) => {
                    warn!(%member, "identity sweep: revoking the logins failed; retried next pass: {e}");
                }
            }
        }
        if missing_kept > 0 {
            warn!(missing = missing_kept, "identity sweep: Zitadel found none of the users this pass looked up; nothing is revoked for a missing user until a pass finds some (check the instance and the admin token)");
        }
        for (tenant, member, zitadel_user) in &marked {
            if states.get(zitadel_user).copied().flatten() != Some(Seen::Active) {
                continue;
            }
            match self.metadata.clear_offboarded(*tenant, zitadel_user).await {
                Ok(true) => info!(%member, "identity sweep: the member's Zitadel user is active again; the offboarded mark is cleared"),
                Ok(false) => {}
                Err(e) => {
                    warn!(%member, "identity sweep: clearing the offboarded mark failed; retried next pass: {e}");
                }
            }
        }
        (checked, revoked)
    }

    /// Tells the proxy cache (now and again after the republish delay) about every role row of
    /// the revoked member and requests a reconcile of each branch holding one.
    async fn publish_revocation(&self, endpoints: Vec<LoginEndpoint>) {
        let mut roles: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut branches: BTreeSet<Uuid> = BTreeSet::new();
        for e in endpoints {
            let names = roles.entry(e.endpoint_id).or_default();
            if !names.contains(&e.role_name) {
                names.push(e.role_name);
            }
            branches.insert(e.branch_id);
        }
        for (endpoint, names) in &roles {
            self.publish_twice(endpoint, names).await;
        }
        for branch in branches {
            self.member_role_requests.request(BranchId::from(branch));
        }
    }
}

/// An identity-service user id: a UUID. Zitadel's are digits.
fn is_identity_service_id(id: &str) -> bool {
    Uuid::parse_str(id).is_ok()
}

/// The rows whose user is Zitadel's: the only ones the sweep may look up.
fn zitadel_members<T>(rows: Vec<(T, Uuid, String)>) -> Vec<(T, Uuid, String)> {
    rows.into_iter()
        .filter(|(_, _, user)| !is_identity_service_id(user))
        .collect()
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::members::{MemberRecord, MemberStatus};
    use chelabase_metadata::personal_logins::{LoginEndpoint, PersonalLoginRecord};
    use chelabase_types::{BranchId, DatabaseId, TenantId};
    use chelabase_zitadel::client::Zitadel;
    use chelabase_zitadel::mock::{ADMIN_TOKEN, MockZitadel};
    use chrono::{DateTime, Utc};
    use uuid::Uuid;

    use crate::fakes::{
        FakeWorker, Journal, entries, position, ready_branch, ready_database, worker,
    };

    struct Setup {
        w: FakeWorker,
        journal: Journal,
        mock: MockZitadel,
        db: DatabaseId,
        main: BranchId,
        dev: BranchId,
        n: u32,
        /// Reconcile requests recorded before the test began (setup's branch creates).
        requests_before: usize,
    }

    /// A ready database `app` with a main and a `dev` branch, Zitadel as the mock with the
    /// admin token, and no member yet. The journal starts empty.
    async fn setup() -> Setup {
        let (mut w, journal) = worker();
        let mock = MockZitadel::start().await;
        w.zitadel = Some(Zitadel::new(&mock.base, ADMIN_TOKEN));
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let dev = ready_branch(&w, db, "dev", main).await;
        journal.lock().unwrap().clear();
        let requests_before = w.member_role_requests.sent().len();
        Setup {
            w,
            journal,
            mock,
            db,
            main,
            dev,
            n: 0,
            requests_before,
        }
    }

    fn far() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2099, 1, 2, 3, 4, 5).unwrap()
    }

    /// A member's one login on `db`, live (`revoked` false) with a verifier, or revoked.
    fn login(db: DatabaseId, member: Uuid, name: &str, revoked: bool) -> PersonalLoginRecord {
        PersonalLoginRecord {
            member_id: member,
            database_id: db.as_uuid(),
            role_name: name.into(),
            scram_verifier: (!revoked).then(|| "SCRAM-SHA-256$verifier".to_string()),
            valid_until: far(),
            revoked,
            qualifies: true,
        }
    }

    /// Who a test member is.
    struct M {
        member: Uuid,
        zitadel: String,
        role: String,
    }

    impl Setup {
        fn ep(&self, branch: BranchId) -> String {
            self.w.metadata.branch(branch).endpoint_id.clone().unwrap()
        }

        fn tenant(&self) -> TenantId {
            self.w.metadata.database(self.db).tenant_id
        }

        /// An active member of the tenant holding one live login, with a Zitadel user in the
        /// mock (active) and the login's rows on the main and `dev` endpoints.
        fn member(&mut self) -> M {
            self.n += 1;
            let zitadel = self
                .mock
                .add_user(&format!("m{}@example.test", self.n), "pw", true);
            let tenant = self.tenant();
            self.member_in(tenant, &zitadel)
        }

        /// As `member`, for a Zitadel user that already exists, in `tenant`.
        fn member_in(&mut self, tenant: TenantId, zitadel: &str) -> M {
            self.n += 1;
            let member = Uuid::new_v4();
            let role = format!("login_{}", self.n);
            let endpoints: Vec<LoginEndpoint> = [self.main, self.dev]
                .into_iter()
                .map(|b| LoginEndpoint {
                    branch_id: b.as_uuid(),
                    endpoint_id: self.ep(b),
                    role_name: role.clone(),
                })
                .collect();
            let mut state = self.w.metadata.state();
            state.members.push(MemberRecord {
                id: member,
                tenant_id: tenant,
                identity_user_id: zitadel.to_string(),
                email: None,
                display_name: "m".into(),
                tenant_admin: false,
                status: MemberStatus::Active,
                accepted_at: None,
                offboarded: false, suspended_at: None, suspended_by: None,
            });
            state
                .personal_logins
                .push(login(self.db, member, &role, false));
            state.login_endpoints.insert(member, endpoints);
            M {
                member,
                zitadel: zitadel.to_string(),
                role,
            }
        }

        fn revoked(&self, m: &M) -> bool {
            self.w
                .metadata
                .state()
                .personal_logins
                .iter()
                .filter(|l| l.member_id == m.member)
                .all(|l| l.revoked && l.scram_verifier.is_none())
        }

        /// The `GET /v2/users/<id>` calls the mock saw for the user.
        fn lookups(&self, zitadel: &str) -> usize {
            let call = format!("GET /v2/users/{zitadel}");
            self.mock.calls().iter().filter(|c| **c == call).count()
        }

        /// The reconcile requests the sweep made.
        fn requests(&self) -> Vec<BranchId> {
            self.w
                .member_role_requests
                .sent()
                .split_off(self.requests_before)
        }

        fn revoke_writes(&self) -> usize {
            entries(&self.journal)
                .iter()
                .filter(|e| e.starts_with("metadata.revoke_logins_for_identity_user"))
                .count()
        }
    }

    /// A deactivated, a locked and a deleted user are offboarded and an active one is kept;
    /// the pass reports every member it looked up and every one it revoked.
    #[tokio::test]
    async fn inactive_locked_or_missing_users_are_offboarded() {
        let mut s = setup().await;
        let inactive = s.member();
        let locked = s.member();
        let deleted = s.member();
        let active = s.member();
        s.mock.set_inactive(&inactive.zitadel, true);
        s.mock.lock(&locked.zitadel);
        s.mock.remove_user(&deleted.zitadel);

        let (checked, revoked) = s.w.sweep_identities_once().await;

        assert_eq!((checked, revoked), (4, 3));
        assert!(s.revoked(&inactive), "a deactivated user keeps a login");
        assert!(s.revoked(&locked), "a locked user keeps a login");
        assert!(s.revoked(&deleted), "a deleted user keeps a login");
        assert!(!s.revoked(&active), "an active user lost a login");
        assert_eq!(s.revoke_writes(), 3, "{:?}", entries(&s.journal));
    }

    /// An unreadable state revokes nothing (revoking on an outage would lock everyone out):
    /// Zitadel down, a 5xx, or a refusal. The member is left alone, the others are still
    /// swept, and the next pass retries the one that failed.
    #[tokio::test]
    async fn active_users_and_unreadable_states_are_kept() {
        let mut s = setup().await;
        let active = s.member();
        let down = s.member();
        s.mock.set_inactive(&down.zitadel, true);

        // Zitadel down: the offboarded user's login stays, nothing is written or published.
        s.mock.set_down(true);
        let (_, revoked) = s.w.sweep_identities_once().await;
        assert_eq!(revoked, 0);
        assert!(!s.revoked(&down), "revoked on a Zitadel outage");
        assert!(!s.revoked(&active));
        assert_eq!(s.revoke_writes(), 0);
        assert!(
            !entries(&s.journal).iter().any(|e| e.starts_with("cache.")),
            "the cache was told: {:?}",
            entries(&s.journal)
        );
        assert!(s.requests().is_empty());

        // A 5xx and a 403 on that one user's lookup, another user still swept in the same pass.
        s.mock.set_down(false);
        let other = s.member();
        s.mock.set_inactive(&other.zitadel, true);
        s.mock.fail(
            &format!("GET /v2/users/{}", down.zitadel),
            500,
            "Errors.Internal",
            1,
        );
        let (_, revoked) = s.w.sweep_identities_once().await;
        assert_eq!(revoked, 1, "the failing lookup stopped the others");
        assert!(!s.revoked(&down), "revoked on a 5xx");
        assert!(s.revoked(&other));

        let third = s.member();
        s.mock.set_inactive(&third.zitadel, true);
        s.mock.fail(
            &format!("GET /v2/users/{}", third.zitadel),
            403,
            "Errors.PermissionDenied",
            1,
        );
        let (_, revoked) = s.w.sweep_identities_once().await;
        assert_eq!(
            revoked, 1,
            "only the previously failed `down` user is revoked now"
        );
        assert!(!s.revoked(&third), "revoked on a refusal");

        // The next pass retries both.
        let (_, revoked) = s.w.sweep_identities_once().await;
        assert_eq!(revoked, 1);
        assert!(s.revoked(&down), "a failure wasn't retried");
        assert!(s.revoked(&third), "a failure wasn't retried");
        assert!(!s.revoked(&active));
    }

    /// A revocation tells the proxy cache now and again after the republish delay, for every
    /// role row on every endpoint, only once the write is done, and asks for a reconcile of
    /// each branch holding a row.
    #[tokio::test]
    async fn a_revocation_clears_the_cache_twice_and_requests_reconciles() {
        let mut s = setup().await;
        let gone = s.member();
        s.mock.set_inactive(&gone.zitadel, true);
        let (main_ep, dev_ep) = (s.ep(s.main), s.ep(s.dev));

        assert_eq!(s.w.sweep_identities_once().await, (1, 1));

        let journal = entries(&s.journal);
        for ep in [&main_ep, &dev_ep] {
            let now = format!("cache.password_updated {ep} {}", gone.role);
            let later = format!("cache.password_updated_later {ep} {}", gone.role);
            assert!(journal.contains(&now), "{now} missing: {journal:?}");
            assert!(journal.contains(&later), "{later} missing: {journal:?}");
            assert!(
                position(&s.journal, "metadata.revoke_logins_for_identity_user")
                    < position(&s.journal, &now),
                "the cache was told before the revocation was written: {journal:?}"
            );
        }
        let mut asked = s.requests();
        asked.sort_by_key(|b| b.as_uuid());
        asked.dedup();
        let mut want = vec![s.main, s.dev];
        want.sort_by_key(|b| b.as_uuid());
        assert_eq!(asked, want);
    }

    /// Only members holding a live login are looked up: a suspended member's logins are
    /// already revoked, so no Zitadel call is made for them; a member with two live logins is
    /// looked up once.
    #[tokio::test]
    async fn only_members_with_live_logins_are_looked_up() {
        let mut s = setup().await;
        let suspended = s.member();
        s.mock.set_inactive(&suspended.zitadel, true);
        {
            let mut state = s.w.metadata.state();
            state
                .members
                .iter_mut()
                .find(|m| m.id == suspended.member)
                .unwrap()
                .status = MemberStatus::Suspended;
            for l in state
                .personal_logins
                .iter_mut()
                .filter(|l| l.member_id == suspended.member)
            {
                l.revoked = true;
                l.scram_verifier = None;
            }
        }
        let twice = s.member();
        s.w.metadata
            .state()
            .personal_logins
            .push(login(s.db, twice.member, "second_login", false));
        let member_without = Uuid::new_v4();
        let tenant = s.tenant();
        s.w.metadata.state().members.push(MemberRecord {
            id: member_without,
            tenant_id: tenant,
            identity_user_id: "z-no-logins".into(),
            email: None,
            display_name: "n".into(),
            tenant_admin: false,
            status: MemberStatus::Active,
            accepted_at: None,
            offboarded: false, suspended_at: None, suspended_by: None,
        });

        let (checked, revoked) = s.w.sweep_identities_once().await;

        assert_eq!((checked, revoked), (1, 0));
        assert_eq!(
            s.lookups(&suspended.zitadel),
            0,
            "a revoked member was looked up"
        );
        assert_eq!(
            s.lookups("z-no-logins"),
            0,
            "a member without logins was looked up"
        );
        assert_eq!(
            s.lookups(&twice.zitadel),
            1,
            "a member was looked up once per login"
        );
    }

    /// The same Zitadel user can be a member of two tenants: each tenant's member is
    /// offboarded by its own write.
    #[tokio::test]
    async fn a_user_in_two_tenants_is_offboarded_in_both() {
        let mut s = setup().await;
        let first = s.member();
        let other_tenant = TenantId(Uuid::new_v4());
        let second = s.member_in(other_tenant, &first.zitadel);
        s.mock.set_inactive(&first.zitadel, true);

        let (_, revoked) = s.w.sweep_identities_once().await;

        assert_eq!(revoked, 2);
        assert!(s.revoked(&first));
        assert!(s.revoked(&second));
    }

    /// Re-running finds nothing live: no write, no cache message, no reconcile request beyond
    /// the first pass's. The revoked member is marked offboarded (D7), so it is looked up once
    /// more, only to see whether the mark can go.
    #[tokio::test]
    async fn a_second_pass_does_nothing() {
        let mut s = setup().await;
        let gone = s.member();
        let kept = s.member();
        s.mock.set_inactive(&gone.zitadel, true);

        assert_eq!(s.w.sweep_identities_once().await, (2, 1));
        let journal_after_first = entries(&s.journal);
        let requests_after_first = s.requests();
        let lookups_of_gone = s.lookups(&gone.zitadel);

        assert_eq!(s.w.sweep_identities_once().await, (1, 0));

        assert_eq!(
            s.lookups(&gone.zitadel),
            lookups_of_gone + 1,
            "the marked member is looked up once, for its mark"
        );
        assert_eq!(s.revoke_writes(), 1);
        assert_eq!(s.requests(), requests_after_first);
        let journal_after_second = entries(&s.journal);
        let cache = |j: &[String]| j.iter().filter(|e| e.starts_with("cache.")).count();
        assert_eq!(cache(&journal_after_second), cache(&journal_after_first));
        assert!(!s.revoked(&kept));
    }

    /// A marked member (no login left) with its user still offboarded.
    fn marked(s: &mut Setup, zitadel: &str) -> Uuid {
        let member = Uuid::new_v4();
        let tenant = s.tenant();
        s.w.metadata.state().members.push(MemberRecord {
            id: member,
            tenant_id: tenant,
            identity_user_id: zitadel.to_string(),
            email: None,
            display_name: "m".into(),
            tenant_admin: false,
            status: MemberStatus::Active,
            accepted_at: None,
            offboarded: true, suspended_at: None, suspended_by: None,
        });
        member
    }

    fn is_marked(s: &Setup, member: Uuid) -> bool {
        s.w.metadata.state().members.iter().any(|m| m.id == member && m.offboarded)
    }

    /// D7 (coder's edge): a marked member whose user is gone (404) keeps the mark; a 404 never
    /// clears it.
    #[tokio::test]
    async fn a_marked_member_whose_user_is_gone_stays_marked() {
        let mut s = setup().await;
        let kept = s.member();
        let gone = s.mock.add_user("gone@example.test", "pw", true);
        let m = marked(&mut s, &gone);
        s.mock.remove_user(&gone);
        s.w.sweep_identities_once().await;
        assert!(is_marked(&s, m), "a 404 cleared the mark");
        assert!(!s.revoked(&kept));
    }

    /// EOS-H2 with D7 (coder's edge): a marked member's lookup that finds its user is a user
    /// found, so a lone deleted live-login member in the same pass is believed and revoked.
    #[tokio::test]
    async fn a_marked_members_lookup_counts_as_a_user_found() {
        let mut s = setup().await;
        let deleted = s.member();
        s.mock.remove_user(&deleted.zitadel);
        let back = s.mock.add_user("back@example.test", "pw", true);
        let m = marked(&mut s, &back);
        let (_, revoked) = s.w.sweep_identities_once().await;
        assert_eq!(revoked, 1);
        assert!(s.revoked(&deleted));
        assert!(!is_marked(&s, m), "the active user's mark stays");
    }

    /// #124: a member whose id comes from the identity service (a UUID) is never looked up in
    /// Zitadel, which would answer 404 and offboard them: their logins stay and their mark is
    /// left alone (removal and suspension revoke inline since m9). A Zitadel member in the same
    /// pass is still swept, and a pass of identity members only reads nothing.
    #[tokio::test]
    async fn identity_service_members_are_skipped() {
        let mut s = setup().await;
        let tenant = s.tenant();
        let identity_user = Uuid::new_v4().to_string();
        let kept = s.member_in(tenant, &identity_user);
        let marked_user = Uuid::new_v4().to_string();
        let m = marked(&mut s, &marked_user);
        let (checked, revoked) = s.w.sweep_identities_once().await;
        assert_eq!((checked, revoked), (0, 0));
        assert!(!s.revoked(&kept), "an identity-service member lost a login");
        assert!(is_marked(&s, m));
        assert_eq!((s.lookups(&identity_user), s.lookups(&marked_user)), (0, 0));
        // With a deleted Zitadel member beside them, the Zitadel one is revoked, as before.
        let deleted = s.member();
        let found = s.member();
        s.mock.remove_user(&deleted.zitadel);
        let (checked, revoked) = s.w.sweep_identities_once().await;
        assert_eq!((checked, revoked), (2, 1));
        assert!(s.revoked(&deleted) && !s.revoked(&found));
        assert!(!s.revoked(&kept));
        assert_eq!(s.lookups(&identity_user), 0);
    }

    /// Without a Zitadel client nothing can be read: no lookup, nothing revoked, no panic.
    #[tokio::test]
    async fn without_zitadel_the_pass_revokes_nothing() {
        let mut s = setup().await;
        let m = s.member();
        s.mock.set_inactive(&m.zitadel, true);
        s.w.zitadel = None;

        assert_eq!(s.w.sweep_identities_once().await, (0, 0));

        assert!(!s.revoked(&m));
        assert_eq!(s.revoke_writes(), 0);
        assert!(s.mock.calls().iter().all(|c| !c.contains("/v2/users/")));
    }
}
