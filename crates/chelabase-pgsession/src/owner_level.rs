//! What a role session needs to know about its role before it opens ([`OWNER_LEVEL_SQL`]),
//! asked before a request's transaction and cached 30 s per branch and role:
//!
//! - whether the role is owner-level: a secret key (and the console) may never run as one.
//!   The data API and storage refuse the same roles through these checks;
//! - which of the data API's two logins the session runs on
//!   ([`chelabase_query::roles::login_for`]; the invariant is written there).
//!
//! [`Pools::acquire_for_role`] is the one way to a connection a role session may open on: it
//! reads the facts (on an `authenticator` connection, which then goes back to its pool),
//! applies the caller's [`Guard`], and hands out a connection of the login the facts choose,
//! bound to that role. A cached answer that has gone stale fails in the session's preamble;
//! the session then drops the cached facts ([`Conn`]'s session end), so the next request
//! reads them anew. A connection of the elevated login is handed out only after the facts
//! were read again on it and still choose it. Nothing is retried on the other login.

use chelabase_auth::internal::DataIdentity;
use chelabase_query::QueryClient;
use chelabase_query::roles::{Login, OWNER_LEVEL_SQL, RoleFacts, login_for};
use chelabase_types::data_key::DataKeyKind;
use tokio_postgres::TransactionStatus;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::pools::{AcquireError, Conn, ConnSource, Pools, RESET_SQL};

/// Sent just before [`OWNER_LEVEL_SQL`], in the same round trip: the check runs before the
/// request's transaction, so it ends whatever an earlier request left open (a `ROLLBACK`,
/// unless the connection is idle: outside a transaction Postgres logs a warning for it) and
/// resets the session it may have changed (a `statement_timeout`, a `search_path`, a role).
pub fn owner_check_batch(status: TransactionStatus) -> String {
    if status == TransactionStatus::Idle {
        RESET_SQL.to_owned()
    } else {
        format!("ROLLBACK; {RESET_SQL}")
    }
}

/// Asks Postgres about `role` ([`RoleFacts`]; `None`: it doesn't exist), before the
/// request's transaction. A request dropped half-way may have left the connection inside
/// a transaction, maybe a failed one, so a `ROLLBACK` goes first (unless the connection is
/// idle), in the same round trip. When that clean-up batch fails the session may not be
/// clean: the connection is marked broken (discarded); the answer stays the check's own.
pub async fn fetch_role_facts<C: QueryClient + ?Sized>(
    client: &C,
    role: &str,
    request_id: &str,
) -> Result<Option<RoleFacts>, tokio_postgres::Error> {
    let params: [(&(dyn tokio_postgres::types::ToSql + Sync), Type); 1] = [(&role, Type::TEXT)];
    // Read now, while nothing is in flight.
    let clean = owner_check_batch(client.transaction_status());
    let c = client.client();
    // `biased`: the clean-up is sent first.
    let (cleaned, row) = tokio::join!(
        biased;
        c.batch_execute(&clean),
        c.query_typed_opt(OWNER_LEVEL_SQL, &params),
    );
    if let Err(e) = cleaned {
        tracing::warn!(
            request_id,
            "the owner check's clean-up failed; the connection is discarded: {e}"
        );
        client.mark_broken();
    }
    row.and_then(|row| row.as_ref().map(RoleFacts::from_row).transpose())
}

/// How long a role's facts are kept.
pub const OWNER_LEVEL_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Most answers kept.
const OWNER_LEVEL_CAPACITY: u64 = 10_000;

/// The roles' facts, by `(branch, role)`.
pub type OwnerLevelCache = moka::future::Cache<(Uuid, String), RoleFacts>;

pub fn owner_level_cache() -> OwnerLevelCache {
    moka::future::Cache::builder()
        .max_capacity(OWNER_LEVEL_CAPACITY)
        .time_to_live(OWNER_LEVEL_TTL)
        .support_invalidation_closures()
        .build()
}

/// Drops `branch`'s answers.
pub fn invalidate_branch(cache: &OwnerLevelCache, branch: Uuid) {
    let _ = cache.invalidate_entries_if(move |k, _| k.0 == branch);
}

/// Why a role was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleRefused {
    /// The role is owner-level.
    OwnerLevel(String),
    /// The role is one of Chelabase's own (`chelabase_…`, among them the function triggers'
    /// owner role, or a hidden `cb_m_…` member role, any case): nothing may run as it,
    /// whatever its attributes say. Refused before Postgres is asked.
    Reserved(String),
    /// The console's role doesn't exist.
    UnknownRole(String),
}

/// The check a role passes before a session opens as it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guard {
    /// None (a public key's `anon` or `authenticated`, the internal schema route's roles):
    /// the role is only routed to its login.
    None,
    /// A secret key's role: not reserved and not owner-level. A missing role counts as plain
    /// (the preamble reports it).
    SecretKey,
    /// The console's role: it must exist, not be reserved and not be owner-level (the secret
    /// key's 403).
    Console,
}

impl Guard {
    /// A key's guard: [`Guard::SecretKey`] for a secret key, none for a public one.
    pub fn of_key(data: &DataIdentity) -> Self {
        if data.kind == DataKeyKind::Secret {
            Guard::SecretKey
        } else {
            Guard::None
        }
    }

    /// The refusal by name alone, before Postgres is asked: a role no request may run as, as
    /// the worker's create-time check (`refuse_by_name`) refuses it, so a key minted before
    /// the name was reserved never runs either.
    pub fn check_name(self, role: &str) -> Result<(), RoleRefused> {
        if self != Guard::None && chelabase_types::role_name::has_reserved_prefix(role) {
            return Err(RoleRefused::Reserved(role.to_owned()));
        }
        Ok(())
    }

    /// The refusal from the role's facts (`None`: it doesn't exist).
    pub fn check_facts(self, role: &str, facts: Option<&RoleFacts>) -> Result<(), RoleRefused> {
        match (self, facts) {
            (Guard::None, _) | (Guard::SecretKey, None) => Ok(()),
            (Guard::Console, None) => Err(RoleRefused::UnknownRole(role.to_owned())),
            (_, Some(f)) if f.owner_level => Err(RoleRefused::OwnerLevel(role.to_owned())),
            (_, Some(_)) => Ok(()),
        }
    }
}

/// `role`'s facts on `branch` (cached), `None` when it doesn't exist (not cached: it may be
/// created a moment later). `fetch` asks Postgres on a miss.
pub async fn role_facts<F, Fut, E>(
    cache: &OwnerLevelCache,
    branch: Uuid,
    role: &str,
    fetch: F,
) -> Result<Option<RoleFacts>, E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<RoleFacts>, E>>,
{
    let key = (branch, role.to_owned());
    if let Some(v) = cache.get(&key).await {
        return Ok(Some(v));
    }
    let v = fetch().await?;
    if let Some(v) = v {
        cache.insert(key, v).await;
    }
    Ok(v)
}

/// Whether facts read now on the elevated login's own connection still choose that login for
/// `role`: what a session there is held to, whatever the cached facts said (rule 3 of
/// [`chelabase_query::roles`]'s invariant). A role that is gone doesn't.
pub fn still_on_elevated(role: &str, now: Option<&RoleFacts>) -> bool {
    now.is_some() && login_for(role, now) == Some(Login::Elevated)
}

/// Why no connection for a role session could be had.
#[derive(Debug, thiserror::Error)]
pub enum RoleAcquireError {
    /// The [`Guard`] refused the role.
    #[error("the role is refused: {0:?}")]
    Refused(RoleRefused),
    /// The role can't run on either login right now: it is elevated and the elevated login
    /// doesn't hold it yet, or the branch has no elevated login to connect as. Never run on
    /// `authenticator` instead.
    #[error("the role {0} can't be used right now")]
    Unavailable(String),
    #[error(transparent)]
    Acquire(#[from] AcquireError),
    /// Reading the role's facts failed.
    #[error(transparent)]
    Check(tokio_postgres::Error),
}

impl<S: ConnSource> Pools<S> {
    /// `role`'s facts on `branch` (cached; read on an `authenticator` connection of
    /// `database` on a miss), after `guard`: the check alone, for a caller that describes
    /// the role without running as it (rest's internal schema route).
    pub async fn check_role(
        &self,
        cache: &OwnerLevelCache,
        branch: Uuid,
        database: &str,
        role: &str,
        guard: Guard,
        request_id: &str,
    ) -> Result<Option<RoleFacts>, RoleAcquireError> {
        guard.check_name(role).map_err(RoleAcquireError::Refused)?;
        let facts = role_facts(cache, branch, role, || async {
            let conn = self
                .acquire(branch, database)
                .await
                .map_err(RoleAcquireError::Acquire)?;
            conn.read_role_facts(role, request_id)
                .await
                .map_err(RoleAcquireError::Check)
        })
        .await?;
        guard
            .check_facts(role, facts.as_ref())
            .map_err(RoleAcquireError::Refused)?;
        Ok(facts)
    }

    /// A connection to the branch's `database` for a session as `role`, on the login the
    /// role's facts choose ([`login_for`]), after `guard`. The facts are read on an
    /// `authenticator` connection when `cache` has none (that connection goes back to its
    /// pool before the session's is taken). A `Guard::None` session as `anon` or
    /// `authenticated` reads nothing: those are always `authenticator`'s.
    ///
    /// The connection is bound to `role`: [`Conn::with_role_session`] opens no other role on
    /// it. When that session's preamble finds the role can't be used on the chosen login (a
    /// cached answer older than a grant's move), the cached facts are dropped.
    ///
    /// A connection of the elevated login is handed out only after the facts were read again
    /// on it and still choose it ([`still_on_elevated`]) and still pass `guard`: that login
    /// may reach a role it no longer holds (through another role it holds), so the preamble
    /// alone doesn't stop a stale answer there. A different answer drops the cached facts
    /// and refuses the request (503); the next one reads anew.
    pub async fn acquire_for_role(
        &self,
        cache: &OwnerLevelCache,
        branch: Uuid,
        database: &str,
        role: &str,
        guard: Guard,
        request_id: &str,
    ) -> Result<Conn, RoleAcquireError> {
        if guard == Guard::None && chelabase_types::access::data_api_role(role) {
            let conn = self.acquire_login(branch, database, Login::Authenticator).await?;
            return Ok(conn.bound_to(role, None));
        }
        let facts = self
            .check_role(cache, branch, database, role, guard, request_id)
            .await?;
        let key = (branch, role.to_owned());
        let Some(login) = login_for(role, facts.as_ref()) else {
            // Not kept: the worker moves the grant, and the next request should see it.
            cache.invalidate(&key).await;
            tracing::warn!(request_id, branch_id = %branch, role, "an elevated role the elevated login doesn't hold: refused");
            return Err(RoleAcquireError::Unavailable(role.to_owned()));
        };
        match self.acquire_login(branch, database, login).await {
            Ok(mut conn) if login == Login::Elevated => {
                let refused = match conn.confirm_role_facts(role, request_id).await {
                    Err(e) => Some(RoleAcquireError::Check(e)),
                    Ok(now) => match guard.check_facts(role, now.as_ref()) {
                        Err(r) => Some(RoleAcquireError::Refused(r)),
                        Ok(()) if !still_on_elevated(role, now.as_ref()) => {
                            tracing::warn!(request_id, branch_id = %branch, role, "the role's facts no longer choose the elevated login: refused");
                            Some(RoleAcquireError::Unavailable(role.to_owned()))
                        }
                        Ok(()) => None,
                    },
                };
                match refused {
                    Some(e) => {
                        cache.invalidate(&key).await;
                        conn.release().await;
                        Err(e)
                    }
                    None => Ok(conn.bound_to(role, Some((cache.clone(), key)))),
                }
            }
            Ok(conn) => Ok(conn.bound_to(role, Some((cache.clone(), key)))),
            Err(AcquireError::NoElevatedLogin) => {
                tracing::warn!(request_id, branch_id = %branch, role, "the branch has no elevated login to run the role on: refused");
                Err(RoleAcquireError::Unavailable(role.to_owned()))
            }
            Err(e) => Err(RoleAcquireError::Acquire(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_postgres::Client;

    /// The owner-level check runs before the request's transaction, on a connection an
    /// earlier request may have left in a transaction or with session settings changed
    /// (`statement_timeout`, `search_path`, a role): it ends the one and resets the others
    /// first, in the same round trip.
    #[test]
    fn the_owner_check_starts_from_a_clean_session() {
        use tokio_postgres::TransactionStatus;
        // Idle: nothing to end (a `ROLLBACK` there only logs a warning).
        assert_eq!(owner_check_batch(TransactionStatus::Idle), RESET_SQL);
        assert!(RESET_SQL.starts_with("SET ROLE NONE; RESET ALL; "));
        for status in [
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
            TransactionStatus::Unknown,
        ] {
            assert_eq!(
                owner_check_batch(status),
                format!("ROLLBACK; {RESET_SQL}"),
                "{status:?}"
            );
        }
    }

    /// Edge functions m8 (round 4, B3): a key whose role is one of Chelabase's own
    /// (`chelabase_…`, among them the function triggers' owner role, or a hidden `cb_m_…`
    /// member role, any case) is refused on every request without asking Postgres, as the
    /// worker's create-time check refuses it; the console's role too. Other names go to the
    /// owner-level check as before.
    #[tokio::test]
    async fn a_reserved_role_is_refused_before_the_owner_level_check() {
        let cache = owner_level_cache();
        let identity = |role: &str| DataIdentity {
            branch_id: Uuid::nil(),
            key_id: Uuid::nil(),
            kind: DataKeyKind::Secret,
            role: role.to_owned(),
            claims: serde_json::Value::Object(Default::default()),
            databases: None,
            project_id: None,
            role_oid: None,
            server_vouched: false,
        };
        for role in ["chelabase_functions_owner", "Chelabase_Admin", "cb_m_0123", "CB_M_x"] {
            for guard in [Guard::SecretKey, Guard::Console] {
                assert_eq!(guard.check_name(role), Err(RoleRefused::Reserved(role.to_owned())), "{role}");
                // Refused before the branch's info, a connection or Postgres is asked for.
                let pools = Pools::new(crate::pools::test_support::RefusedSource, 1);
                let out = pools
                    .acquire_for_role(&cache, Uuid::nil(), "main", role, guard, "rid")
                    .await;
                assert!(
                    matches!(&out, Err(RoleAcquireError::Refused(RoleRefused::Reserved(r))) if r == role),
                    "{role}"
                );
            }
        }
        // A plain role is asked about as before, and a public key never is.
        assert_eq!(Guard::SecretKey.check_name("reporter"), Ok(()));
        assert_eq!(Guard::of_key(&identity("reporter")), Guard::SecretKey);
        let public = DataIdentity {
            kind: DataKeyKind::Public,
            ..identity("chelabase_functions_owner")
        };
        assert_eq!(Guard::of_key(&public), Guard::None);
        assert_eq!(Guard::None.check_name("chelabase_functions_owner"), Ok(()));
    }

    fn facts(owner_level: bool, elevated: bool, held_by_elevated: bool) -> RoleFacts {
        RoleFacts { owner_level, elevated, held_by_elevated }
    }

    /// A secret key's role and the console's are refused when owner-level; only the console's
    /// must exist; an unguarded role is never refused.
    #[test]
    fn guards_refuse_from_the_facts() {
        let owner = facts(true, true, false);
        let plain = facts(false, false, false);
        for guard in [Guard::SecretKey, Guard::Console] {
            assert_eq!(guard.check_facts("boss", Some(&owner)), Err(RoleRefused::OwnerLevel("boss".into())));
            assert_eq!(guard.check_facts("svc", Some(&plain)), Ok(()));
        }
        assert_eq!(Guard::SecretKey.check_facts("ghost", None), Ok(()));
        assert_eq!(Guard::Console.check_facts("ghost", None), Err(RoleRefused::UnknownRole("ghost".into())));
        for f in [None, Some(&owner), Some(&plain)] {
            assert_eq!(Guard::None.check_facts("x", f), Ok(()));
        }
    }

    /// Facts are cached per branch and role; a missing role isn't (created a moment later,
    /// it's found).
    #[tokio::test]
    async fn role_facts_are_cached_per_branch_and_role() {
        let cache = owner_level_cache();
        let branch = Uuid::new_v4();
        let queries = std::cell::Cell::new(0);
        let ask = |branch: Uuid, role: &'static str, answer: Option<RoleFacts>| {
            let (cache, queries) = (&cache, &queries);
            async move {
                role_facts(cache, branch, role, || async {
                    queries.set(queries.get() + 1);
                    Ok::<_, ()>(answer)
                })
                .await
                .unwrap()
            }
        };
        let plain = facts(false, false, false);
        assert_eq!(ask(branch, "svc", Some(plain)).await, Some(plain));
        assert_eq!(ask(branch, "svc", None).await, Some(plain));
        assert_eq!(queries.get(), 1);
        assert_eq!(ask(branch, "boss", Some(facts(true, true, false))).await.map(|f| f.owner_level), Some(true));
        assert_eq!(ask(Uuid::new_v4(), "svc", Some(plain)).await, Some(plain));
        assert_eq!(queries.get(), 3);
        assert_eq!(ask(branch, "ghost", None).await, None);
        assert_eq!(ask(branch, "ghost", Some(plain)).await, Some(plain));
        assert_eq!(queries.get(), 5);
    }

    /// The model's compute: whether the role is elevated, and which logins hold it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Catalog {
        elevated: bool,
        on_authenticator: bool,
        on_elevated: bool,
        /// The elevated login reaches the role without holding it: an elevated role it
        /// holds is a member of this one. A role switch on that login then passes whatever
        /// `on_elevated` says.
        reached_by_elevated: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        /// An Admin gives the role an elevated attribute; the grants stay where they are.
        MakeElevated,
        MakeOrdinary,
        /// The worker's step: the role is put on the one login it belongs to.
        Move,
        /// The cached facts expire (at any moment: the model has no clock).
        Expire,
        Request,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Outcome {
        Ran(Login),
        /// 503: refused before a connection, or in the preamble.
        Unavailable,
    }

    /// One request as [`Pools::acquire_for_role`] and the session's end make it, over the
    /// real cache, [`role_facts`] and [`login_for`]; Postgres is the model's: the preamble
    /// passes only on a login that holds the role. Returns the outcome and whether the facts
    /// were read now.
    async fn model_request(cache: &OwnerLevelCache, key: &(Uuid, String), cat: Catalog) -> (Outcome, bool) {
        let fresh = std::cell::Cell::new(false);
        let facts = role_facts(cache, key.0, &key.1, || async {
            fresh.set(true);
            Ok::<_, ()>(Some(RoleFacts {
                owner_level: false,
                elevated: cat.elevated,
                held_by_elevated: cat.on_elevated,
            }))
        })
        .await
        .unwrap();
        let outcome = match login_for(&key.1, facts.as_ref()) {
            None => {
                cache.invalidate(key).await;
                Outcome::Unavailable
            }
            // The elevated login's connection reads the facts again before its session.
            Some(Login::Elevated)
                if !still_on_elevated(
                    &key.1,
                    Some(&RoleFacts {
                        owner_level: false,
                        elevated: cat.elevated,
                        held_by_elevated: cat.on_elevated,
                    }),
                ) =>
            {
                cache.invalidate(key).await;
                Outcome::Unavailable
            }
            Some(login) => {
                // Postgres checks a role switch against everything the login reaches.
                let reaches = match login {
                    Login::Authenticator => cat.on_authenticator,
                    Login::Elevated => cat.on_elevated || cat.reached_by_elevated,
                };
                if reaches {
                    Outcome::Ran(login)
                } else {
                    // The preamble's refusal: the session's end drops the cached facts.
                    cache.invalidate(key).await;
                    Outcome::Unavailable
                }
            }
        };
        (outcome, fresh.get())
    }

    /// Every sequence of up to six role changes, grant moves, cache expiries and requests,
    /// from each state a branch can be in (settled either way; as the release before left
    /// it, on both logins; made elevated and not moved yet):
    ///
    /// - a request never runs on the elevated login unless that login holds the role and the
    ///   role is elevated at that moment, whatever facts were cached and whether or not the
    ///   elevated login also reaches the role through another role it holds;
    /// - a request whose facts were read now never runs an elevated role on `authenticator`;
    ///   one that does used facts cached before the change, with the grant not moved yet;
    /// - a request runs on at most one login and is never retried on the other (one outcome
    ///   per request), and a refusal leaves no cached facts;
    /// - once the worker has moved the grant, at most one request answers 503 before the
    ///   role runs on its login again, with no expiry needed.
    ///
    /// The model's assumptions: an expiry may come at any time (so the cache's lifetime is
    /// not relied on); the worker's move is atomic and leaves exactly one login holding the
    /// role (the worker's `the_plan_leaves_each_role_on_one_login`); the preamble passes
    /// exactly when the login reaches the role: `authenticator` only by holding it (the live
    /// `stale_login_answer…` test; the resync leaves it reaching no elevated role), the
    /// elevated login by holding it or through another role it holds (the live
    /// `a_stale_elevated_answer_through_another_role_fails_closed`); the facts read on the
    /// elevated login's connection and its session see the same catalog (a change between
    /// the two is a change during the request).
    #[tokio::test]
    async fn model_of_grant_moves_and_stale_answers() {
        use Event::*;
        let events = [MakeElevated, MakeOrdinary, Move, Expire, Request];
        let starts = [false, true].into_iter().flat_map(|reached_by_elevated| {
            [
                Catalog { elevated: false, on_authenticator: true, on_elevated: false, reached_by_elevated },
                Catalog { elevated: true, on_authenticator: false, on_elevated: true, reached_by_elevated },
                Catalog { elevated: true, on_authenticator: true, on_elevated: true, reached_by_elevated },
                Catalog { elevated: true, on_authenticator: true, on_elevated: false, reached_by_elevated },
            ]
        });
        const DEPTH: u32 = 6;
        let mut runs = 0u32;
        let mut stale_runs = 0u32;
        for start in starts {
            for mut code in 0..events.len().pow(DEPTH) {
                let cache = owner_level_cache();
                let key = (Uuid::nil(), "svc".to_owned());
                let mut cat = start;
                // The grants are where the role's current attributes put them, and how many
                // requests were made since they got there.
                let mut settled: Option<u32> =
                    (start.on_authenticator != start.on_elevated && start.on_elevated == start.elevated).then_some(0);
                let mut trace = Vec::new();
                for _ in 0..DEPTH {
                    let event = events[code % events.len()];
                    code /= events.len();
                    trace.push(event);
                    match event {
                        MakeElevated | MakeOrdinary => {
                            let elevated = event == MakeElevated;
                            if cat.elevated != elevated {
                                settled = None;
                            }
                            cat.elevated = elevated;
                        }
                        Move => {
                            let moved = Catalog {
                                elevated: cat.elevated,
                                on_authenticator: !cat.elevated,
                                on_elevated: cat.elevated,
                                reached_by_elevated: cat.reached_by_elevated,
                            };
                            if moved != cat || settled.is_none() {
                                settled = Some(0);
                            }
                            cat = moved;
                        }
                        Expire => cache.invalidate(&key).await,
                        Request => {
                            let (outcome, fresh) = model_request(&cache, &key, cat).await;
                            let case = format!("{start:?} {trace:?}: {outcome:?} fresh={fresh} {cat:?}");
                            runs += 1;
                            match outcome {
                                Outcome::Ran(Login::Elevated) => {
                                    // Rule 3 has no stale window: whatever the cached
                                    // facts said, the role is elevated and held now.
                                    assert!(cat.on_elevated && cat.elevated, "{case}");
                                }
                                Outcome::Ran(Login::Authenticator) => {
                                    assert!(cat.on_authenticator, "{case}");
                                    if cat.elevated {
                                        assert!(!fresh, "an elevated role on authenticator from facts read now; {case}");
                                        stale_runs += 1;
                                    }
                                }
                                Outcome::Unavailable => {
                                    assert!(cache.get(&key).await.is_none(), "a refusal keeps no facts; {case}");
                                }
                            }
                            if let Some(since) = settled {
                                if fresh || since >= 1 {
                                    let right = if cat.elevated { Login::Elevated } else { Login::Authenticator };
                                    assert_eq!(outcome, Outcome::Ran(right), "settled, and it doesn't run; {case}");
                                }
                                settled = Some(since + 1);
                            }
                        }
                    }
                }
            }
        }
        // The model reaches both the plain cases and the stale window it bounds.
        assert!(runs > 10_000 && stale_runs > 0, "{runs} {stale_runs}");
    }

    /// A session whose preamble finds the role unusable on the login its cached facts chose
    /// drops those facts, so the next request reads them anew; any other failure keeps them.
    #[tokio::test]
    async fn preamble_failure_drops_the_cached_login() {
        use crate::session::tests::recorder;
        let fake = recorder(|_, _| false).await;
        struct At(u16);
        impl ConnSource for At {
            async fn fetch(&self, _branch: Uuid) -> Result<crate::ConnInfo, crate::ConnError> {
                let mut info = crate::pools::test_support::two_databases();
                info.port = self.0;
                Ok(info)
            }
        }
        let pools = Pools::new(At(fake.port), 10);
        let cache = owner_level_cache();
        let branch = Uuid::new_v4();
        let key = (branch, "reader".to_owned());
        let claims = serde_json::json!({});
        for (failure, kept) in [
            (chelabase_query::QueryFailure::unavailable("the branch's database is unavailable"), true),
            (chelabase_query::engine::role_unusable("writer"), true),
            (chelabase_query::engine::role_unusable("reader"), false),
        ] {
            cache.insert(key.clone(), facts(false, false, false)).await;
            let conn = pools
                .acquire_for_role(&cache, branch, "main", "reader", Guard::SecretKey, "rid")
                .await
                .unwrap();
            assert_eq!(conn.login(), Login::Authenticator);
            conn.with_role_session("reader", &claims, None, async |s| {
                let _ = s.seen::<()>(Err(failure.clone()));
            })
            .await
            .unwrap();
            assert_eq!(cache.get(&key).await.is_some(), kept, "{failure:?}");
        }
    }

    /// A [`QueryClient`] over a connection whose server is gone, recording `mark_broken`.
    struct Gone {
        client: Client,
        broken: std::sync::atomic::AtomicBool,
    }

    impl QueryClient for Gone {
        fn client(&self) -> &Client {
            &self.client
        }
        async fn prepare_cached(
            &self,
            sql: &str,
            types: &[Type],
        ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
            self.client.prepare_typed(sql, types).await
        }
        fn mark_broken(&self) {
            self.broken
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A client whose server went away: it answers the startup message with
    /// `AuthenticationOk` and `ReadyForQuery`, then drops the socket.
    async fn closed_client() -> Client {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let len = socket.read_i32().await.unwrap();
            let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
            socket.read_exact(&mut startup).await.unwrap();
            socket
                .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'])
                .await
                .unwrap();
            socket.flush().await.unwrap();
        });
        let mut config = tokio_postgres::Config::new();
        config.host("127.0.0.1").port(port).user("x").dbname("x");
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .expect("the fake server's handshake");
        server.await.unwrap();
        let _ = connection.await;
        client
    }

    /// The owner check's clean-up batch (`ROLLBACK`/reset) failing means the session may not
    /// be clean: the connection is discarded, whatever the check itself answered.
    #[tokio::test]
    async fn a_failed_owner_check_cleanup_marks_the_lease_broken() {
        let gone = Gone {
            client: closed_client().await,
            broken: Default::default(),
        };
        let out = fetch_role_facts(&gone, "r", "rid-1").await;
        assert!(out.is_err());
        assert!(gone.broken.load(std::sync::atomic::Ordering::Relaxed));
    }
}
