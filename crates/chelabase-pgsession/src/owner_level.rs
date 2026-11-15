//! Whether a role is owner-level ([`OWNER_LEVEL_SQL`]), asked before a request's
//! transaction and cached 30 s per branch and role: a secret key (and the console) may never
//! run as one. The data API and storage refuse the same roles through these checks.

use chelabase_auth::internal::DataIdentity;
use chelabase_query::QueryClient;
use chelabase_query::roles::OWNER_LEVEL_SQL;
use chelabase_types::data_key::DataKeyKind;
use tokio_postgres::TransactionStatus;
use tokio_postgres::types::Type;
use uuid::Uuid;

use crate::pools::RESET_SQL;

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

/// Asks Postgres whether `role` is owner-level (`None`: it doesn't exist), before the
/// request's transaction. A request dropped half-way may have left the connection inside
/// a transaction, maybe a failed one, so a `ROLLBACK` goes first (unless the connection is
/// idle), in the same round trip. When that clean-up batch fails the session may not be
/// clean: the connection is marked broken (discarded); the answer stays the check's own.
pub async fn fetch_owner_level<C: QueryClient + ?Sized>(
    client: &C,
    role: &str,
    request_id: &str,
) -> Result<Option<bool>, tokio_postgres::Error> {
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
    row.and_then(|row| row.map(|r| r.try_get::<_, Option<bool>>(0)).transpose())
        .map(|level| level.map(|l| l.unwrap_or(false)))
}

/// How long an owner-level answer is kept.
pub const OWNER_LEVEL_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Most answers kept.
const OWNER_LEVEL_CAPACITY: u64 = 10_000;

/// The owner-level answers, by `(branch, role)`.
pub type OwnerLevelCache = moka::future::Cache<(Uuid, String), bool>;

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
pub enum RoleRefused<E> {
    /// The role is owner-level.
    OwnerLevel(String),
    /// The role is one of Chelabase's own (`chelabase_…`, among them the function triggers'
    /// owner role, or a hidden `cb_m_…` member role, any case): nothing may run as it,
    /// whatever its attributes say. Refused before Postgres is asked.
    Reserved(String),
    /// The console's role doesn't exist.
    UnknownRole(String),
    /// The check itself failed.
    Check(E),
}

/// A role no request may run as by name, as the worker's create-time check
/// (`refuse_by_name`) refuses it: a key or a console session that names one is refused on
/// every request, so a key minted before the name was reserved never runs either.
fn reserved(role: &str) -> bool {
    chelabase_types::role_name::has_reserved_prefix(role)
}

/// Whether `role` is owner-level on `branch` (cached), `None` when it doesn't exist (not
/// cached: it may be created a moment later). `fetch` asks Postgres on a miss.
pub async fn owner_level<F, Fut, E>(
    cache: &OwnerLevelCache,
    branch: Uuid,
    role: &str,
    fetch: F,
) -> Result<Option<bool>, E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<bool>, E>>,
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

/// For a secret key only: refuses a role that is owner-level. A missing role counts as
/// plain (the preamble reports it).
pub async fn ensure_not_owner_level<F, Fut, E>(
    cache: &OwnerLevelCache,
    data: &DataIdentity,
    fetch: F,
) -> Result<(), RoleRefused<E>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<bool>, E>>,
{
    if data.kind != DataKeyKind::Secret {
        return Ok(());
    }
    if reserved(&data.role) {
        return Err(RoleRefused::Reserved(data.role.clone()));
    }
    match owner_level(cache, data.branch_id, &data.role, fetch)
        .await
        .map_err(RoleRefused::Check)?
    {
        Some(true) => Err(RoleRefused::OwnerLevel(data.role.clone())),
        _ => Ok(()),
    }
}

/// The console's role must exist, not be reserved and not be owner-level (the secret key's
/// 403).
pub async fn ensure_console_role<F, Fut, E>(
    cache: &OwnerLevelCache,
    branch: Uuid,
    role: &str,
    fetch: F,
) -> Result<(), RoleRefused<E>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Option<bool>, E>>,
{
    if reserved(role) {
        return Err(RoleRefused::Reserved(role.to_owned()));
    }
    match owner_level(cache, branch, role, fetch)
        .await
        .map_err(RoleRefused::Check)?
    {
        None => Err(RoleRefused::UnknownRole(role.to_owned())),
        Some(true) => Err(RoleRefused::OwnerLevel(role.to_owned())),
        Some(false) => Ok(()),
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
        assert_eq!(
            owner_check_batch(TransactionStatus::Idle),
            "SET ROLE NONE; RESET ALL"
        );
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
        let never = || async { panic!("asked Postgres about a reserved role") };
        for role in ["chelabase_functions_owner", "Chelabase_Admin", "cb_m_0123", "CB_M_x"] {
            let r: Result<(), RoleRefused<()>> =
                ensure_not_owner_level(&cache, &identity(role), never).await;
            assert_eq!(r, Err(RoleRefused::Reserved(role.to_owned())), "{role}");
            let r: Result<(), RoleRefused<()>> =
                ensure_console_role(&cache, Uuid::nil(), role, never).await;
            assert_eq!(r, Err(RoleRefused::Reserved(role.to_owned())), "{role}");
        }
        // A plain role is asked about as before, and a public key never is.
        let r: Result<(), RoleRefused<()>> =
            ensure_not_owner_level(&cache, &identity("reporter"), || async { Ok(Some(false)) }).await;
        assert_eq!(r, Ok(()));
        let public = DataIdentity {
            kind: DataKeyKind::Public,
            ..identity("chelabase_functions_owner")
        };
        let r: Result<(), RoleRefused<()>> = ensure_not_owner_level(&cache, &public, never).await;
        assert_eq!(r, Ok(()));
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
        let out = fetch_owner_level(&gone, "r", "rid-1").await;
        assert!(out.is_err());
        assert!(gone.broken.load(std::sync::atomic::Ordering::Relaxed));
    }
}
