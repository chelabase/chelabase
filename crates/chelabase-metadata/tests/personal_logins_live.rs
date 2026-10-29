//! Edge cases of personal logins, suspension and revocation beyond `members_live`'s set (members
//! hub T3), each on its own scratch database of the metadata Postgres (the workbench stack):
//!
//! ```sh
//! cargo test -p chelabase-metadata --test personal_logins_live -- --ignored
//! ```
//!
//! Scratch databases are named `cp_<8 hex unix seconds><4 hex random>`; every run drops those a
//! dead run left once they are older than an hour.

use chelabase_metadata::members::{AccessWriteError, Admission, GrantRow, OverrideRow};
use chelabase_metadata::personal_logins::{LoginStatus, LoginWriteError};
use chelabase_metadata::{
    DatabaseStatus, MetadataClient, MetadataConfig, NewEndpoint, RoleWriteOutcome,
};
use chelabase_types::access::PlatformRole;
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{Duration, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

const V1: &str = "SCRAM-SHA-256$4096:c2FsdDE=$c3RvcmVkMQ==:c2VydmVyMQ==";
const V2: &str = "SCRAM-SHA-256$4096:c2FsdDI=$c3RvcmVkMg==:c2VydmVyMg==";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Scratch {
    meta: PgPool,
    name: String,
    client: MetadataClient,
}

/// A tenant with its claiming tenant admin, a project and a ready database whose main has an
/// endpoint.
struct Hub {
    t: TenantId,
    admin: Uuid,
    p: Uuid,
    db: Uuid,
    ep: String,
}

impl Scratch {
    async fn new() -> Scratch {
        let base = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
        });
        let meta = PgPoolOptions::new()
            .max_connections(1)
            .connect(&base)
            .await
            .expect("metadata Postgres reachable (is the workbench stack up?)");
        let stale: Vec<String> = sqlx::query_scalar(
            "SELECT datname FROM pg_catalog.pg_database WHERE datname ~ '^cp_[0-9a-f]{12}$'",
        )
        .fetch_all(&meta)
        .await
        .unwrap();
        for old in stale {
            if u64::from_str_radix(&old[3..11], 16).unwrap_or(u64::MAX)
                < now_secs().saturating_sub(3600)
            {
                let _ = sqlx::raw_sql(AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS {old} WITH (FORCE)"
                )))
                .execute(&meta)
                .await;
            }
        }
        let name = format!(
            "cp_{:08x}{}",
            now_secs() as u32,
            &Uuid::new_v4().simple().to_string()[..4]
        );
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&meta)
            .await
            .unwrap();
        let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
        let query = tail
            .split_once('?')
            .map(|(_, q)| format!("?{q}"))
            .unwrap_or_default();
        let client = MetadataClient::connect(&MetadataConfig {
            database_url: format!("{head}/{name}{query}"),
        })
        .await
        .unwrap();
        client.migrate().await.unwrap();
        Scratch { meta, name, client }
    }

    async fn finish(self) {
        self.client.pool().close().await;
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.meta)
        .await
        .unwrap();
    }

    async fn project(&self, t: TenantId) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects (id, tenant_id, slug, name) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(t.as_uuid())
            .bind(format!("p-{id}"))
            .execute(self.client.pool())
            .await
            .unwrap();
        id
    }

    /// A ready database in `p` with an endpoint on main: `(database, endpoint)`.
    async fn ready_database(&self, t: TenantId, p: Uuid) -> (Uuid, String) {
        let db = DatabaseId::new();
        self.client
            .insert_database_pending(db, t, ProjectId(p), &format!("d-{db}"))
            .await
            .unwrap();
        let ep = format!("ep-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let (tenant, timeline) = (
            Uuid::new_v4().simple().to_string(),
            Uuid::new_v4().simple().to_string(),
        );
        let new = NewEndpoint {
            id: &ep,
            tenant_id: &tenant,
            timeline_id: &timeline,
            compute_token_sha256: "cd",
            owner_role: "postgres",
            owner_verifier: "SCRAM-SHA-256$owner",
            database_name: "db",
            role_copies: &[],
            routable: true,
            static_lsn: None,
        };
        let branch = BranchId(db.as_uuid());
        assert!(
            self.client
                .create_endpoint_for_branch(branch, &timeline, &new, false)
                .await
                .unwrap()
        );
        self.client
            .set_status(db, DatabaseStatus::Provisioning)
            .await
            .unwrap();
        assert!(
            self.client
                .set_branch_ready(branch, "v1:owner", None, None)
                .await
                .unwrap()
        );
        (db.as_uuid(), ep)
    }

    async fn hub(&self) -> Hub {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, slug) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("t-{id}"))
            .execute(self.client.pool())
            .await
            .unwrap();
        let t = TenantId(id);
        let admin = self
            .client
            .ensure_member_first_comer_for_tests(t, "admin", "admin@x.com", "Admin", Admission::Active)
            .await
            .unwrap();
        let p = self.project(t).await;
        let (db, ep) = self.ready_database(t, p).await;
        Hub {
            t,
            admin: admin.id,
            p,
            db,
            ep,
        }
    }

    /// An active member with `role` on the hub's project.
    async fn granted(&self, h: &Hub, zid: &str, role: PlatformRole) -> Uuid {
        let m = self
            .client
            .ensure_member_first_comer_for_tests(h.t, zid, &format!("{zid}@x.com"), zid, Admission::Active)
            .await
            .unwrap();
        let g = GrantRow {
            platform_role: role,
            pg_roles: Vec::new(),
        };
        self.client
            .put_project_grant(h.admin, m.id, h.p, &g)
            .await
            .unwrap();
        m.id
    }

    async fn row(&self, ep: &str, name: &str) -> Option<(String, Option<String>)> {
        sqlx::query_as(
            "SELECT origin, scram_verifier FROM neon_roles WHERE endpoint_id = $1 AND name = $2",
        )
        .bind(ep)
        .bind(name)
        .fetch_optional(self.client.pool())
        .await
        .unwrap()
    }

    async fn events_of(
        &self,
        t: TenantId,
        action: &str,
    ) -> Vec<chelabase_metadata::members::AccessEvent> {
        let all = self
            .client
            .access_events(t, None, None, None, 1000)
            .await
            .unwrap();
        all.into_iter().filter(|e| e.action == action).collect()
    }
}

fn in_days(n: i64) -> chrono::DateTime<Utc> {
    Utc::now() + Duration::days(n)
}

// P8 race: the worker passes the verifier it read; a reset since then wins, so an old password
// is never written back to a proxy row.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_stale_verifier_never_overwrites_a_reset() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.create_personal_login(alice, h.db, "alice", V1, in_days(90))
        .await
        .unwrap();
    c.reset_personal_login(alice, h.db, V2, in_days(90))
        .await
        .unwrap();
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1)
        .await
        .unwrap();
    assert_eq!(
        s.row(&h.ep, "alice").await,
        Some(("personal".into(), Some(V2.into())))
    );
    assert_eq!(
        c.routable_role_secret(&h.ep, "alice").await.unwrap(),
        Some(Some(V2.into()))
    );
    s.finish().await;
}

// The worker's write never takes over a row of that name with another origin, and writes
// nothing for a name that isn't the record's.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_worker_never_takes_over_another_row_or_name() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.create_personal_login(alice, h.db, "alice", V1, in_days(90))
        .await
        .unwrap();
    // A customer's role of that name recorded on main after the create (a race the name check
    // can't close; the worker's CREATE ROLE then fails there).
    sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'alice', 'x', 'sql')")
        .bind(&h.ep)
        .execute(c.pool())
        .await
        .unwrap();
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1)
        .await
        .unwrap();
    assert_eq!(
        s.row(&h.ep, "alice").await,
        Some(("sql".into(), Some("x".into())))
    );
    c.upsert_personal_role_row(&h.ep, alice, h.db, "other", V1)
        .await
        .unwrap();
    assert_eq!(s.row(&h.ep, "other").await, None);
    s.finish().await;
}

// The API's by-name writers answer as if a personal row weren't there (S1).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn api_writers_answer_not_found_for_a_personal_row() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.create_personal_login(alice, h.db, "alice", V1, in_days(90))
        .await
        .unwrap();
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1)
        .await
        .unwrap();
    let stored = c
        .store_role_secret(&h.ep, "alice", "SCRAM-SHA-256$api", "v1:api", 100)
        .await
        .unwrap();
    assert!(matches!(stored, RoleWriteOutcome::NotFound), "{stored:?}");
    assert!(
        !c.replace_role_secret(&h.ep, "alice", "SCRAM-SHA-256$api", "v1:api")
            .await
            .unwrap()
    );
    assert!(!c.delete_role_copy(&h.ep, "alice").await.unwrap());
    assert_eq!(
        s.row(&h.ep, "alice").await,
        Some(("personal".into(), Some(V1.into())))
    );
    s.finish().await;
}

// P17: a redelivered offboarding webhook revokes nothing more and logs nothing, but still
// lists the member's rows so the proxy cache is cleared again.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_retried_offboarding_still_lists_the_rows() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.create_personal_login(alice, h.db, "alice", V1, in_days(90))
        .await
        .unwrap();
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1)
        .await
        .unwrap();
    let first = c
        .revoke_logins_for_zitadel_user(h.t, "alice")
        .await
        .unwrap();
    let events = s.events_of(h.t, "personal_logins_revoked").await.len();
    let again = c
        .revoke_logins_for_zitadel_user(h.t, "alice")
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(again, first);
    assert_eq!(
        s.events_of(h.t, "personal_logins_revoked").await.len(),
        events
    );
    s.finish().await;
}

// Renew (spec Part 6): a reset of an expired, unrevoked login makes it active again.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn reset_renews_an_expired_login() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.create_personal_login(alice, h.db, "alice", V1, in_days(90))
        .await
        .unwrap();
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1)
        .await
        .unwrap();
    sqlx::query("UPDATE personal_logins SET valid_until = now() - interval '1 second'")
        .execute(c.pool())
        .await
        .unwrap();
    assert_eq!(
        c.routable_role_secret(&h.ep, "alice").await.unwrap(),
        Some(None)
    );
    let (login, _) = c
        .reset_personal_login(alice, h.db, V2, in_days(90))
        .await
        .unwrap();
    assert_eq!(login.status, LoginStatus::Active);
    assert_eq!(
        c.routable_role_secret(&h.ep, "alice").await.unwrap(),
        Some(Some(V2.into()))
    );
    s.finish().await;
}

// "Revoke Postgres access now" is an admin's action: it is logged even with nothing to revoke.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn revoke_now_is_logged_without_logins() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    let touched = c.revoke_postgres_access(h.admin, bob).await.unwrap();
    assert_eq!(touched, vec![h.p]);
    let events = s.events_of(h.t, "personal_logins_revoked").await;
    assert_eq!(events.len(), 1);
    let after = events[0].after.clone().unwrap();
    assert_eq!(
        (after["count"].as_i64(), after["rotated_branches"].clone()),
        (Some(0), serde_json::json!([]))
    );
    let missing = c.revoke_postgres_access(h.admin, Uuid::new_v4()).await;
    assert!(
        matches!(missing, Err(AccessWriteError::NotFound)),
        "{missing:?}"
    );
    s.finish().await;
}

// P15: a tenant admin can't suspend themself, even with another admin left.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn suspending_yourself_is_refused() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let second = s.granted(&h, "second", PlatformRole::Viewer).await;
    c.set_tenant_admin(Some(h.admin), second, true)
        .await
        .unwrap();
    let refused = c.suspend_member(second, second).await;
    assert!(
        matches!(refused, Err(AccessWriteError::Conflict(_))),
        "{refused:?}"
    );
    assert!(
        c.suspend_member(Uuid::new_v4(), Uuid::new_v4())
            .await
            .is_err()
    );
    assert_eq!(
        c.member(second).await.unwrap().unwrap().status,
        chelabase_metadata::members::MemberStatus::Active
    );
    s.finish().await;
}

// D3 race: two members creating the same name at once: exactly one wins, the other is
// `NameTaken`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn concurrent_creates_of_one_name_let_one_win() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    for round in 0..10 {
        let a = s
            .granted(&h, &format!("a{round}"), PlatformRole::Developer)
            .await;
        let b = s
            .granted(&h, &format!("b{round}"), PlatformRole::Developer)
            .await;
        let name = format!("shared{round}");
        let (ra, rb) = tokio::join!(
            c.create_personal_login(a, h.db, &name, V1, in_days(90)),
            c.create_personal_login(b, h.db, &name, V1, in_days(90)),
        );
        let wins = [ra.is_ok(), rb.is_ok()].iter().filter(|w| **w).count();
        assert_eq!(wins, 1, "round {round}: {ra:?} {rb:?}");
        for r in [ra, rb] {
            assert!(
                matches!(r, Ok(_) | Err(LoginWriteError::NameTaken)),
                "{r:?}"
            );
        }
    }
    s.finish().await;
}

// A revocation on one database names its project and database, so the project's log shows it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_revocation_event_names_its_project_and_database() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.create_personal_login(alice, h.db, "alice", V1, in_days(90))
        .await
        .unwrap();
    let narrow = OverrideRow {
        platform_role: Some(PlatformRole::Viewer),
        pg_roles: None,
    };
    c.put_override(h.admin, alice, h.db, &narrow).await.unwrap();
    let in_project = c
        .access_events(h.t, Some(h.p), Some(alice), None, 10)
        .await
        .unwrap();
    let revoked = in_project
        .iter()
        .find(|e| e.action == "personal_logins_revoked")
        .expect("in the project's log");
    assert_eq!(
        (revoked.project_id, revoked.database_id),
        (Some(h.p), Some(h.db))
    );
    assert_eq!(revoked.actor_member_id, Some(h.admin));
    s.finish().await;
}
