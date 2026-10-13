//! Live tests of members, grants, invites and the access log, each on its own scratch database
//! of the metadata Postgres (the workbench stack), migrated from scratch:
//!
//! ```sh
//! cargo test -p chelabase-metadata --test members_live -- --ignored
//! ```
//!
//! Scratch databases are named `cm_<8 hex unix seconds><4 hex random>`; every run drops those a
//! dead run left once they are older than [`STALE_AFTER_SECS`].

use chelabase_metadata::members::{
    AccessWriteError, Admission, GrantRow, GrantRows, MemberBranchRole, MemberStatus, OverrideRow,
};
use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_types::access::PlatformRole;
use chelabase_types::{DatabaseId, ProjectId, TenantId};
use chrono::{Duration, Utc};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestCaseError, TestRunner};
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

/// Scratch databases older than this are dropped whoever made them.
const STALE_AFTER_SECS: u64 = 3600;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

/// `url()` pointing at database `name`.
fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    format!("{head}/{name}{query}")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A scratch database `cm_<12 hex>` with every metadata migration applied.
struct Scratch {
    meta: PgPool,
    name: String,
    client: MetadataClient,
}

impl Scratch {
    async fn new() -> Scratch {
        let meta = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url())
            .await
            .expect("metadata Postgres reachable (is the workbench stack up?)");
        sweep_stale(&meta).await;
        let random = Uuid::new_v4().simple().to_string();
        let name = format!("cm_{:08x}{}", now_secs() as u32, &random[..4]);
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&meta)
            .await
            .unwrap();
        let client = MetadataClient::connect(&MetadataConfig {
            database_url: url_for(&name),
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

    async fn tenant(&self) -> TenantId {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, slug) VALUES ($1, $2)")
            .bind(id)
            .bind(format!("t-{id}"))
            .execute(self.client.pool())
            .await
            .unwrap();
        TenantId(id)
    }

    async fn project(&self, tenant: TenantId) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects (id, tenant_id, slug, name) VALUES ($1, $2, $3, $3)")
            .bind(id)
            .bind(tenant.as_uuid())
            .bind(format!("p-{id}"))
            .execute(self.client.pool())
            .await
            .unwrap();
        id
    }

    /// A pending database (with its main branch) in `project`.
    async fn database(&self, tenant: TenantId, project: Uuid) -> Uuid {
        let id = DatabaseId::new();
        self.client
            .insert_database_pending(id, tenant, ProjectId(project), &format!("d-{id}"))
            .await
            .unwrap();
        id.as_uuid()
    }

    async fn event_count(&self, tenant: TenantId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM access_events WHERE tenant_id = $1")
            .bind(tenant.as_uuid())
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }

    /// A transaction on its own connection holding the tenant row as the API locks it.
    async fn hold_tenant(&self, tenant: TenantId) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let mut tx = self.client.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR NO KEY UPDATE")
            .bind(tenant.as_uuid())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx
    }

    /// Waits (10 s at most) until `n` sessions of the scratch database wait on a lock.
    async fn wait_for_lock_waiters(&self, n: i64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(self.client.pool())
            .await
            .unwrap();
            if waiting >= n {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{waiting} of {n} waiters"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn member_count(&self, tenant: TenantId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM members WHERE tenant_id = $1")
            .bind(tenant.as_uuid())
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }

    async fn last_action(&self, tenant: TenantId) -> String {
        self.client
            .access_events(tenant, None, None, 1)
            .await
            .unwrap()[0]
            .action
            .clone()
    }
}

/// Drops the `cm_` scratch databases of runs older than [`STALE_AFTER_SECS`].
async fn sweep_stale(meta: &PgPool) {
    let cutoff = now_secs().saturating_sub(STALE_AFTER_SECS);
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_catalog.pg_database WHERE datname ~ '^cm_[0-9a-f]{12}$'",
    )
    .fetch_all(meta)
    .await
    .unwrap();
    for name in names {
        let created = u64::from_str_radix(&name[3..11], 16).unwrap_or(u64::MAX);
        if created < cutoff {
            let _ = sqlx::raw_sql(AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
            )))
            .execute(meta)
            .await;
        }
    }
}

fn grant(role: PlatformRole, roles: &[&str]) -> GrantRow {
    GrantRow {
        platform_role: role,
        pg_roles: roles.iter().map(|s| s.to_string()).collect(),
    }
}

fn week() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::days(7)
}

/// An invite token's SHA-256 (any 32 bytes will do for these tests).
const TOKEN: [u8; 32] = [7; 32];

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn first_claim_is_atomic() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    assert!(!s.client.tenant_has_members(t).await.unwrap());
    let mut set = tokio::task::JoinSet::new();
    for i in 0..20 {
        let c = s.client.clone();
        set.spawn(async move {
            c.ensure_member(t, &format!("z{i}"), &format!("  User{i}@Example.COM "), "N", Admission::Active)
                .await
                .unwrap()
        });
    }
    let members: Vec<_> = set.join_all().await;
    assert_eq!(members.iter().filter(|m| m.tenant_admin).count(), 1);
    assert!(members.iter().all(|m| m.status == MemberStatus::Active));
    for m in &members {
        let i = m.zitadel_user_id.trim_start_matches('z');
        assert_eq!(
            m.email.as_deref(),
            Some(format!("user{i}@example.com").as_str())
        );
    }
    assert_eq!(s.client.active_tenant_admins(t).await.unwrap().len(), 1);
    assert_eq!(s.client.list_members(t).await.unwrap().len(), 20);
    assert!(s.client.tenant_has_members(t).await.unwrap());
    let events = s.client.access_events(t, None, None, 100).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.action == "admin_claimed")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.action == "member_joined")
            .count(),
        19
    );
    let claim = events.iter().find(|e| e.action == "admin_claimed").unwrap();
    assert_eq!(
        claim.actor_member_id, None,
        "the first claim is a system action"
    );
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn concurrent_demotions_keep_an_admin() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let b = s
        .client
        .ensure_member(t, "b", "b@x.com", "B", Admission::Active)
        .await
        .unwrap();
    assert!(a.tenant_admin && !b.tenant_admin);
    for _round in 0..10 {
        s.client
            .set_tenant_admin(Some(a.id), b.id, true)
            .await
            .unwrap();
        s.client
            .set_tenant_admin(Some(b.id), a.id, true)
            .await
            .unwrap();
        // A third connection holds the tenant row until both demotions wait on it, so every
        // round races for real once it lets go.
        let hold = s.hold_tenant(t).await;
        let (c1, c2) = (s.client.clone(), s.client.clone());
        let d1 = tokio::spawn(async move { c1.set_tenant_admin(Some(a.id), b.id, false).await });
        let d2 = tokio::spawn(async move { c2.set_tenant_admin(Some(b.id), a.id, false).await });
        s.wait_for_lock_waiters(2).await;
        hold.rollback().await.unwrap();
        let results = [d1.await.unwrap(), d2.await.unwrap()];
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "{results:?}"
        );
        assert!(
            results
                .iter()
                .any(|r| matches!(r, Err(AccessWriteError::LastTenantAdmin))),
            "{results:?}"
        );
        assert_eq!(s.client.active_tenant_admins(t).await.unwrap().len(), 1);
    }
    s.finish().await;
}

/// One operation of the last-admin model; indexes pick among the tenant's live members.
#[derive(Debug, Clone)]
enum Op {
    /// Invite a new person (by any member) and activate them.
    Add,
    /// A new person's first request.
    Claim,
    Demote(u8),
    Promote(u8),
    Remove(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        Just(Op::Add),
        Just(Op::Claim),
        any::<u8>().prop_map(Op::Demote),
        any::<u8>().prop_map(Op::Promote),
        any::<u8>().prop_map(Op::Remove),
    ]
}

/// Runs one generated history on a fresh tenant: each step's 3 operations run concurrently,
/// on distinct members. After every step a tenant with an active member has an active admin,
/// and every refusal was `LastTenantAdmin`.
async fn run_model(s: &Scratch, steps: Vec<Vec<Op>>) -> Result<(), String> {
    let t = s.tenant().await;
    for (n, step) in steps.into_iter().enumerate() {
        let members = s.client.list_members(t).await.map_err(|e| e.to_string())?;
        let mut used = std::collections::BTreeSet::new();
        let mut set = tokio::task::JoinSet::new();
        for op in step {
            // A targeted op takes the first member at or after its index nobody in this step
            // targets yet; with none left it becomes a claim.
            let target = |i: u8, used: &mut std::collections::BTreeSet<usize>| {
                (0..members.len())
                    .map(|k| (i as usize + k) % members.len())
                    .find(|k| !used.contains(k))
                    .inspect(|k| {
                        used.insert(*k);
                    })
                    .map(|k| members[k].id)
            };
            let op = match op {
                Op::Demote(i) | Op::Promote(i) | Op::Remove(i) => match target(i, &mut used) {
                    Some(id) => match op {
                        Op::Demote(_) => (Op::Demote(0), Some(id)),
                        Op::Promote(_) => (Op::Promote(0), Some(id)),
                        _ => (Op::Remove(0), Some(id)),
                    },
                    None => (Op::Claim, None),
                },
                Op::Add if members.is_empty() => (Op::Claim, None),
                other => (other, None),
            };
            let c = s.client.clone();
            let actor = members.first().map(|m| m.id);
            set.spawn(async move {
                let zid = Uuid::new_v4().to_string();
                let email = format!("{zid}@x.com");
                let result: Result<(), AccessWriteError> = match op {
                    (Op::Claim, _) => c
                        .ensure_member(t, &zid, &email, "n", Admission::Active)
                        .await
                        .map(|_| ())
                        .map_err(|e| AccessWriteError::Conflict(format!("ensure_member: {e}"))),
                    (Op::Add, _) => {
                        match c
                            .create_invited_member(actor.unwrap(), t, &zid, &email, week(), TOKEN, &[])
                            .await
                        {
                            Ok(_) => c
                                .ensure_member(t, &zid, &email, "n", Admission::Active)
                                .await
                                .map(|_| ())
                                .map_err(|e| {
                                    AccessWriteError::Conflict(format!("ensure_member: {e}"))
                                }),
                            Err(e) => Err(e),
                        }
                    }
                    (Op::Demote(_), Some(id)) => c.set_tenant_admin(None, id, false).await,
                    (Op::Promote(_), Some(id)) => c.set_tenant_admin(None, id, true).await,
                    (Op::Remove(_), Some(id)) => c.remove_member(id, id).await.map(|_| ()),
                    other => unreachable!("{other:?}"),
                };
                result
            });
        }
        for result in set.join_all().await {
            match result {
                Ok(()) | Err(AccessWriteError::LastTenantAdmin) => {}
                Err(e) => return Err(format!("step {n}: unexpected refusal {e:?}")),
            }
        }
        let live = s.client.list_members(t).await.map_err(|e| e.to_string())?;
        let any_active = live.iter().any(|m| m.status == MemberStatus::Active);
        let admins = s
            .client
            .active_tenant_admins(t)
            .await
            .map_err(|e| e.to_string())?;
        if any_active && admins.is_empty() {
            return Err(format!(
                "step {n}: active members but no active admin: {live:?}"
            ));
        }
    }
    Ok(())
}

#[test]
#[ignore = "needs the workbench stack's metadata Postgres"]
fn last_admin_model() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let s = rt.block_on(Scratch::new());
    let mut runner = TestRunner::new(Config {
        cases: 64,
        failure_persistence: None,
        ..Config::default()
    });
    let steps = prop::collection::vec(prop::collection::vec(op(), 3), 1..10);
    let result = runner.run(&steps, |steps| {
        rt.block_on(run_model(&s, steps))
            .map_err(TestCaseError::fail)
    });
    rt.block_on(s.finish());
    result.unwrap();
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn access_events_are_append_only() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    s.client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    assert_eq!(s.event_count(t).await, 1);
    for sql in [
        "UPDATE access_events SET action = 'tampered' WHERE tenant_id = $1",
        "DELETE FROM access_events WHERE tenant_id = $1",
    ] {
        let err = sqlx::query(sql)
            .bind(t.as_uuid())
            .execute(s.client.pool())
            .await
            .expect_err(sql);
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    // TRUNCATE fires no row trigger: a statement trigger refuses it.
    let err = sqlx::query("TRUNCATE access_events")
        .execute(s.client.pool())
        .await
        .expect_err("TRUNCATE");
    assert!(err.to_string().contains("append-only"), "TRUNCATE: {err}");
    assert_eq!(s.event_count(t).await, 1);
    assert_eq!(s.last_action(t).await, "admin_claimed");
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_is_never_stored() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    let before = s.event_count(t).await;
    let refused = s
        .client
        .put_project_grant(
            a.id,
            m.id,
            p,
            &grant(PlatformRole::Developer, &["app", "owner"]),
        )
        .await;
    assert!(
        matches!(refused, Err(AccessWriteError::Conflict(_))),
        "{refused:?}"
    );
    assert_eq!(
        s.event_count(t).await,
        before,
        "a refused write logs nothing"
    );
    assert!(s.client.member_projects(m.id).await.unwrap().is_empty());
    // An invite carrying such a grant is refused the same way, and creates nobody.
    let invite = s
        .client
        .create_invited_member(
            a.id,
            t,
            "zi",
            "i@x.com",
            week(),
            TOKEN,
            &[(p, grant(PlatformRole::Viewer, &["owner"]))],
        )
        .await;
    assert!(
        matches!(invite, Err(AccessWriteError::Conflict(_))),
        "{invite:?}"
    );
    assert!(s.client.member_by_zitadel(t, "zi").await.unwrap().is_none());

    // R7: Admin implies the owner, so naming it is refused even with Admin, on a grant, an
    // invite's grant and an override alike: no row, no event.
    let admin = grant(PlatformRole::Admin, &["owner", "app"]);
    assert!(matches!(
        s.client.put_project_grant(a.id, m.id, p, &admin).await,
        Err(AccessWriteError::Conflict(_))
    ));
    assert!(matches!(
        s.client
            .create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[(p, admin.clone())])
            .await,
        Err(AccessWriteError::Conflict(_))
    ));
    let o = OverrideRow {
        platform_role: Some(PlatformRole::Admin),
        pg_roles: Some(vec!["owner".into()]),
    };
    assert!(matches!(
        s.client.put_override(a.id, m.id, d, &o).await,
        Err(AccessWriteError::Conflict(_))
    ));
    assert!(s.client.member_projects(m.id).await.unwrap().is_empty());
    assert_eq!(
        s.client.grant_rows(m.id, p, Some(d)).await.unwrap(),
        GrantRows::default()
    );
    assert!(s.client.member_by_zitadel(t, "zi").await.unwrap().is_none());
    assert_eq!(s.event_count(t).await, before);

    // The CHECKs themselves refuse a stored `owner` in either table.
    for (sql, role) in [
        (
            "INSERT INTO project_grants (member_id, project_id, platform_role, pg_roles)
             VALUES ($1, $2, $3, ARRAY['owner'])",
            "admin",
        ),
        (
            "INSERT INTO database_grant_overrides (member_id, database_id, platform_role, pg_roles)
             VALUES ($1, $2, $3, ARRAY['owner'])",
            "admin",
        ),
    ] {
        let target = if sql.contains("project_grants") { p } else { d };
        let err = sqlx::query(sql)
            .bind(m.id)
            .bind(target)
            .bind(role)
            .execute(s.client.pool())
            .await
            .expect_err(sql);
        assert!(
            matches!(&err, sqlx::Error::Database(e) if e.is_check_violation()),
            "{err}"
        );
    }
    // A NULL override role set is fine (it doesn't narrow).
    sqlx::query(
        "INSERT INTO database_grant_overrides (member_id, database_id, platform_role, pg_roles)
         VALUES ($1, $2, 'viewer', NULL)",
    )
    .bind(m.id)
    .bind(d)
    .execute(s.client.pool())
    .await
    .unwrap();
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn invited_becomes_active_on_ensure() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let (m, invite) = s
        .client
        .create_invited_member(a.id, t, "zi", " Inv@X.com ", week(), TOKEN, &[(p, g.clone())])
        .await
        .unwrap();
    assert_eq!(m.status, MemberStatus::Invited);
    assert_eq!(m.email.as_deref(), Some("inv@x.com"));
    assert!(!m.tenant_admin);
    let (member, _, revoked, accepted) = s.client.invite(invite).await.unwrap().unwrap();
    assert_eq!((member, revoked, accepted), (m.id, None, None));

    let active = s
        .client
        .ensure_member(t, "zi", "inv@x.com", "Inv", Admission::Active)
        .await
        .unwrap();
    assert_eq!(active.id, m.id);
    assert_eq!(active.status, MemberStatus::Active);
    assert_eq!(active.display_name, "Inv");
    assert!(active.accepted_at.is_some());
    assert!(!active.tenant_admin);
    assert!(s.client.invite(invite).await.unwrap().unwrap().3.is_some());
    assert_eq!(s.client.member_projects(m.id).await.unwrap(), vec![(p, g)]);
    let accepted = &s.client.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!(accepted.action, "invite_accepted");
    assert_eq!(accepted.actor_member_id, Some(m.id));
    assert_eq!(accepted.target_member_id, Some(m.id));

    // The fast path: nothing changed, nothing written.
    let events = s.event_count(t).await;
    s.client
        .ensure_member(t, "zi", "inv@x.com", "Inv", Admission::Active)
        .await
        .unwrap();
    assert_eq!(s.event_count(t).await, events);
    // An accepted invite can't be resent or revoked.
    assert!(matches!(
        s.client.resend_invite(a.id, invite, week(), TOKEN).await,
        Err(AccessWriteError::Conflict(_))
    ));
    assert!(matches!(
        s.client.revoke_invite(a.id, invite).await,
        Err(AccessWriteError::Conflict(_))
    ));
    s.finish().await;
}

// I1: an invited member becomes active only while an open invite exists. After the invite
// expired, a sign-in (a password the person already had) changes nothing; a resend opens it
// again.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn expired_invite_does_not_activate() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let (m, invite) = s
        .client
        .create_invited_member(a.id, t, "zi", "inv@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    sqlx::query("UPDATE invites SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(invite)
        .execute(s.client.pool())
        .await
        .unwrap();
    let events = s.event_count(t).await;
    let seen = s
        .client
        .ensure_member(t, "zi", "inv@x.com", "Inv", Admission::Active)
        .await
        .unwrap();
    assert_eq!((seen.id, seen.status), (m.id, MemberStatus::Invited));
    assert!(seen.accepted_at.is_none());
    assert!(s.client.invite(invite).await.unwrap().unwrap().3.is_none(), "not accepted");
    assert_eq!(s.event_count(t).await, events, "nothing written");
    // The same with the pool's own clock out of the picture: a closed invite never counts.
    s.client.resend_invite(a.id, invite, week(), TOKEN).await.unwrap();
    let active = s
        .client
        .ensure_member(t, "zi", "inv@x.com", "Inv", Admission::Active)
        .await
        .unwrap();
    assert_eq!(active.status, MemberStatus::Active);
    s.finish().await;
}

// M7: the fresh-install race window reads the `admin_claimed` event: true after the claim, false
// for a tenant whose admin was set another way, and false once the window is past.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn admin_claimed_since_reads_the_claim_event() {
    let s = Scratch::new().await;
    let hour_ago = Utc::now() - Duration::hours(1);
    let claimed = s.tenant().await;
    assert!(!s.client.admin_claimed_since(claimed, hour_ago).await.unwrap());
    s.client.ensure_member(claimed, "first", "f@x.com", "F", Admission::Closed).await.unwrap();
    assert!(s.client.admin_claimed_since(claimed, hour_ago).await.unwrap());
    assert!(!s.client.admin_claimed_since(claimed, Utc::now() + Duration::minutes(1)).await.unwrap());
    let bootstrapped = s.tenant().await;
    s.client.bootstrap_admin(bootstrapped, "boss", "b@x.com").await.unwrap();
    assert!(!s.client.admin_claimed_since(bootstrapped, hour_ago).await.unwrap());
    s.finish().await;
}

// I4: a grant write and an override write, both checked against the same state, can't both
// land: under the member's lock the second finds the first's change (`Changed`) and writes
// nothing. Two connections race 20 times; and a stale observation is refused outright.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn grant_and_override_race_keeps_the_invariant() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let a = s.client.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = s.client.ensure_member(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
    let observe = || async { s.client.project_access(p).await.unwrap().observed(p, m.id) };

    // A stale observation is refused, and nothing is written.
    s.client.put_project_grant(a.id, m.id, p, &grant(PlatformRole::Admin, &["plain"])).await.unwrap();
    let stale = observe().await;
    s.client
        .put_override(a.id, m.id, d, &OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: None })
        .await
        .unwrap();
    let events = s.event_count(t).await;
    let refused = s
        .client
        .put_project_grant_checked(a.id, m.id, p, &grant(PlatformRole::Admin, &["plain", "boss"]), &stale)
        .await;
    assert!(matches!(refused, Err(AccessWriteError::Changed)), "{refused:?}");
    assert_eq!(s.event_count(t).await, events);
    assert_eq!(
        s.client.grant_rows(m.id, p, None).await.unwrap().project,
        Some(grant(PlatformRole::Admin, &["plain"]))
    );
    // A current one goes through.
    let now = observe().await;
    s.client
        .put_project_grant_checked(a.id, m.id, p, &grant(PlatformRole::Admin, &["plain", "boss"]), &now)
        .await
        .unwrap();

    // The race: both writes carry the same observation; exactly one lands.
    for round in 0..20 {
        s.client.delete_override(a.id, m.id, d).await.ok();
        s.client.put_project_grant(a.id, m.id, p, &grant(PlatformRole::Admin, &["plain"])).await.unwrap();
        let seen = observe().await;
        let (c1, c2) = (s.client.clone(), s.client.clone());
        let (seen1, seen2) = (seen.clone(), seen);
        let (actor, member) = (a.id, m.id);
        let grant_write = tokio::spawn(async move {
            c1.put_project_grant_checked(actor, member, p, &grant(PlatformRole::Admin, &["plain", "boss"]), &seen1).await
        });
        let override_write = tokio::spawn(async move {
            c2.put_override_checked(
                actor,
                member,
                d,
                &OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: None },
                Some(&seen2),
            )
            .await
        });
        let (g, o) = (grant_write.await.unwrap(), override_write.await.unwrap());
        let landed = [g.is_ok(), o.is_ok()].iter().filter(|ok| **ok).count();
        assert_eq!(landed, 1, "round {round}: {g:?} {o:?}");
        assert!(
            [&g.as_ref().err(), &o.as_ref().err()].iter().any(|r| matches!(r, Some(AccessWriteError::Changed))),
            "round {round}: the loser is told the access changed: {g:?} {o:?}"
        );
        // The state is never the pair neither check allowed: grant with `boss` and the override.
        let rows = s.client.grant_rows(m.id, p, Some(d)).await.unwrap();
        let has_boss = rows.project.as_ref().is_some_and(|g| g.pg_roles.contains(&"boss".to_string()));
        assert!(!(has_boss && rows.database.is_some()), "round {round}: {rows:?}");
    }
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn removed_stays_removed() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    s.client
        .put_project_grant(a.id, m.id, p, &grant(PlatformRole::Viewer, &[]))
        .await
        .unwrap();
    let o = OverrideRow {
        platform_role: Some(PlatformRole::Viewer),
        pg_roles: None,
    };
    s.client
        .put_override(a.id, m.id, d, &o)
        .await
        .unwrap();
    assert_eq!(s.client.remove_member(a.id, m.id).await.unwrap(), vec![p]);
    // The event keeps the access that was deleted, overrides included.
    let removed = &s.client.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!(removed.action, "member_removed");
    let before = removed.before.as_ref().unwrap();
    assert_eq!(
        before["grants"],
        serde_json::json!([{"project_id": p, "platform_role": "viewer", "pg_roles": []}])
    );
    assert_eq!(
        before["overrides"],
        serde_json::json!([{"database_id": d, "platform_role": "viewer", "pg_roles": null}])
    );
    assert!(s.client.member_projects(m.id).await.unwrap().is_empty());
    assert_eq!(
        s.client.grant_rows(m.id, p, Some(d)).await.unwrap(),
        GrantRows::default()
    );

    let events = s.event_count(t).await;
    let again = s
        .client
        .ensure_member(t, "m", "new@x.com", "New", Admission::Active)
        .await
        .unwrap();
    assert_eq!(again.id, m.id);
    assert_eq!(again.status, MemberStatus::Removed);
    assert_eq!(
        again.email.as_deref(),
        Some("m@x.com"),
        "a removed member is returned as is"
    );
    assert_eq!(s.event_count(t).await, events);
    assert!(
        s.client
            .list_members(t)
            .await
            .unwrap()
            .iter()
            .all(|x| x.id != m.id)
    );
    // Removing again changes nothing; a removed member can't be promoted or granted.
    assert_eq!(
        s.client.remove_member(a.id, m.id).await.unwrap(),
        Vec::<Uuid>::new()
    );
    assert!(matches!(
        s.client.set_tenant_admin(Some(a.id), m.id, true).await,
        Err(AccessWriteError::NotFound)
    ));
    assert!(matches!(
        s.client
            .put_project_grant(a.id, m.id, p, &grant(PlatformRole::Viewer, &[]))
            .await,
        Err(AccessWriteError::NotFound)
    ));
    assert_eq!(s.event_count(t).await, events);
    // The last admin can't remove or demote themselves, and the refusals log nothing.
    assert!(matches!(
        s.client.remove_member(a.id, a.id).await,
        Err(AccessWriteError::LastTenantAdmin)
    ));
    assert!(matches!(
        s.client.set_tenant_admin(Some(a.id), a.id, false).await,
        Err(AccessWriteError::LastTenantAdmin)
    ));
    assert_eq!(s.event_count(t).await, events);
    assert_eq!(s.client.active_tenant_admins(t).await.unwrap().len(), 1);
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn override_unique_per_member_and_database() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    let insert =
        "INSERT INTO database_grant_overrides (member_id, database_id, platform_role)
                  VALUES ($1, $2, 'viewer')";
    sqlx::query(insert)
        .bind(m.id)
        .bind(d)
        .execute(s.client.pool())
        .await
        .unwrap();
    let err = sqlx::query(insert)
        .bind(m.id)
        .bind(d)
        .execute(s.client.pool())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, sqlx::Error::Database(e) if e.is_unique_violation()),
        "{err}"
    );
    sqlx::query("DELETE FROM database_grant_overrides WHERE member_id = $1")
        .bind(m.id)
        .execute(s.client.pool())
        .await
        .unwrap();

    // Through the API a second put replaces the first, per (member, database).
    let count = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM database_grant_overrides WHERE member_id = $1",
        )
        .bind(m.id)
        .fetch_one(s.client.pool())
        .await
        .unwrap()
    };
    let first = OverrideRow {
        platform_role: Some(PlatformRole::Viewer),
        pg_roles: None,
    };
    let second = OverrideRow {
        platform_role: None,
        pg_roles: Some(vec!["b".into(), "a".into()]),
    };
    s.client
        .put_override(a.id, m.id, d, &first)
        .await
        .unwrap();
    s.client
        .put_override(a.id, m.id, d, &second)
        .await
        .unwrap();
    assert_eq!(count().await, 1);
    // The overrides table has no Postgres-database level (R17).
    let cols = sqlx::query_scalar::<_, String>(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name IN ('database_grant_overrides', 'member_branch_roles')
           AND column_name = 'pg_database'",
    )
    .fetch_all(s.client.pool())
    .await
    .unwrap();
    assert!(cols.is_empty(), "{cols:?}");
    // A put with neither field is refused.
    assert!(matches!(
        s.client
            .put_override(a.id, m.id, d, &OverrideRow::default())
            .await,
        Err(AccessWriteError::Conflict(_))
    ));
    s.client
        .put_project_grant(a.id, m.id, p, &grant(PlatformRole::Developer, &["a", "b"]))
        .await
        .unwrap();
    assert_eq!(
        s.client.grant_rows(m.id, p, Some(d)).await.unwrap(),
        GrantRows {
            project: Some(grant(PlatformRole::Developer, &["a", "b"])),
            database: Some(OverrideRow {
                platform_role: None,
                pg_roles: Some(vec!["a".into(), "b".into()])
            }),
        }
    );
    assert_eq!(
        s.client.grant_rows(m.id, p, None).await.unwrap().database,
        None
    );
    s.client.delete_override(a.id, m.id, d).await.unwrap();
    assert_eq!(count().await, 0);
    assert!(matches!(
        s.client.delete_override(a.id, m.id, d).await,
        Err(AccessWriteError::NotFound)
    ));
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn every_write_logs_one_event() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let c = &s.client;
    let mut n = s.event_count(t).await;
    macro_rules! one_event {
        ($action:expr, $write:expr) => {{
            let out = $write;
            n += 1;
            assert_eq!(s.event_count(t).await, n, "{}", $action);
            assert_eq!(s.last_action(t).await, $action);
            out
        }};
    }
    macro_rules! no_event {
        ($what:expr, $write:expr) => {{
            let out = $write;
            assert_eq!(s.event_count(t).await, n, "{}", $what);
            out
        }};
    }

    let a = one_event!(
        "admin_claimed",
        c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap()
    );
    let m = one_event!(
        "member_joined",
        c.ensure_member(t, "m", "m@x.com", "M", Admission::Active).await.unwrap()
    );
    no_event!(
        "unchanged claims",
        c.ensure_member(t, "m", "m@x.com", "M", Admission::Active).await.unwrap()
    );
    let refreshed = no_event!(
        "changed claims",
        c.ensure_member(t, "m", " M2@x.com", "M2", Admission::Active).await.unwrap()
    );
    assert_eq!(
        (refreshed.email.as_deref(), refreshed.display_name.as_str()),
        (Some("m2@x.com"), "M2")
    );
    one_event!(
        "admin_set",
        c.set_tenant_admin(Some(a.id), m.id, true).await.unwrap()
    );
    no_event!(
        "same flag",
        c.set_tenant_admin(Some(a.id), m.id, true).await.unwrap()
    );
    one_event!(
        "admin_unset",
        c.set_tenant_admin(Some(a.id), m.id, false).await.unwrap()
    );
    let g = grant(PlatformRole::Developer, &["app"]);
    one_event!(
        "grant_put",
        c.put_project_grant(a.id, m.id, p, &g).await.unwrap()
    );
    let put = &c.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!(put.before, None);
    assert_eq!(
        put.after,
        Some(serde_json::json!({"platform_role": "developer", "pg_roles": ["app"]}))
    );
    assert_eq!(
        (put.actor_member_id, put.target_member_id, put.project_id),
        (Some(a.id), Some(m.id), Some(p))
    );
    let o = OverrideRow {
        platform_role: Some(PlatformRole::Viewer),
        pg_roles: None,
    };
    one_event!(
        "override_put",
        c.put_override(a.id, m.id, d, &o).await.unwrap()
    );
    let put = &c.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!((put.project_id, put.database_id), (Some(p), Some(d)));
    one_event!(
        "override_deleted",
        c.delete_override(a.id, m.id, d).await.unwrap()
    );
    one_event!(
        "grant_deleted",
        c.delete_project_grant(a.id, m.id, p).await.unwrap()
    );
    let (_, invite) = one_event!(
        "invite_created",
        c.create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[(p, g.clone())])
            .await
            .unwrap()
    );
    one_event!(
        "invite_resent",
        c.resend_invite(a.id, invite, week() + Duration::days(1), TOKEN)
            .await
            .unwrap()
    );
    let zid = one_event!(
        "invite_revoked",
        c.revoke_invite(a.id, invite).await.unwrap()
    );
    assert_eq!(zid, "zi");
    assert_eq!(
        c.member_by_zitadel(t, "zi").await.unwrap().unwrap().status,
        MemberStatus::Removed
    );
    let (_, invite2) = one_event!(
        "invite_created",
        c.create_invited_member(a.id, t, "zj", "j@x.com", week(), TOKEN, &[])
            .await
            .unwrap()
    );
    one_event!(
        "invite_accepted",
        c.ensure_member(t, "zj", "j@x.com", "J", Admission::Active).await.unwrap()
    );
    assert!(c.invite(invite2).await.unwrap().unwrap().3.is_some());
    assert!(!c.open_signup(t).await.unwrap());
    one_event!(
        "open_signup_set",
        c.set_open_signup(a.id, t, true).await.unwrap()
    );
    assert!(c.open_signup(t).await.unwrap());
    no_event!(
        "same switch",
        c.set_open_signup(a.id, t, true).await.unwrap()
    );
    one_event!(
        "tenant_renamed",
        c.set_tenant_display_name(a.id, t, Some("Acme")).await.unwrap()
    );
    no_event!(
        "same name",
        c.set_tenant_display_name(a.id, t, Some("Acme")).await.unwrap()
    );
    let touched = one_event!("member_removed", c.remove_member(a.id, m.id).await.unwrap());
    assert!(touched.is_empty());
    let boot = one_event!(
        "admin_set",
        c.bootstrap_admin(t, "boot", " Boot@X.com").await.unwrap()
    );
    assert_eq!(
        (boot.status, boot.tenant_admin, boot.email.as_deref()),
        (MemberStatus::Active, true, Some("boot@x.com"))
    );
    assert_eq!(
        c.access_events(t, None, None, 1).await.unwrap()[0].actor_member_id,
        None
    );
    // A repeated bootstrap writes nothing at all (M4): the row isn't even touched.
    let stamp = || async {
        sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
            "SELECT updated_at FROM members WHERE id = $1",
        )
        .bind(boot.id)
        .fetch_one(c.pool())
        .await
        .unwrap()
    };
    let touched_at = stamp().await;
    no_event!(
        "bootstrap again",
        c.bootstrap_admin(t, "boot", "boot@x.com").await.unwrap()
    );
    assert_eq!(stamp().await, touched_at);

    // Newest first, paged by id, filtered by project.
    let all = c.access_events(t, None, None, 100).await.unwrap();
    assert_eq!(all.len() as i64, n);
    assert!(all.windows(2).all(|w| w[0].id > w[1].id));
    let page = c.access_events(t, None, Some(all[2].id), 2).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![all[3].id, all[4].id]
    );
    let in_project = c.access_events(t, Some(p), None, 100).await.unwrap();
    // An event is in the project when it names it, or when the grants it records (an invite
    // revoked, a member removed: no single project) include it.
    let grants_name = |v: &Option<serde_json::Value>| {
        v.as_ref()
            .and_then(|v| v.get("grants"))
            .and_then(|g| g.as_array())
            .is_some_and(|g| g.iter().any(|g| g["project_id"] == p.to_string()))
    };
    assert!(!in_project.is_empty());
    for e in &in_project {
        assert!(
            e.project_id == Some(p) || grants_name(&e.before) || grants_name(&e.after),
            "{} isn't in the project",
            e.action
        );
    }
    let revoked = in_project.iter().find(|e| e.action == "invite_revoked");
    assert!(revoked.is_some_and(|e| e.project_id.is_none()), "the revoke is found by its grants");
    s.finish().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn branch_roles_and_project_branches() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d1 = s.database(t, p).await;
    let d2 = s.database(t, p).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let mut branches = s.client.project_branches(p).await.unwrap();
    branches.sort();
    let mut expected = vec![d1, d2];
    expected.sort();
    assert_eq!(branches, expected, "each database's main branch");

    let mut role = MemberBranchRole {
        member_id: a.id,
        branch_id: d1,
        role_name: "cb_m_x".into(),
        password_enc: "sealed".into(),
        applied_roles: vec!["r1".into()],
    };
    s.client.upsert_member_branch_role(&role).await.unwrap();
    role.applied_roles = vec!["r1".into(), "r2".into()];
    role.password_enc = "sealed2".into();
    s.client.upsert_member_branch_role(&role).await.unwrap();
    assert_eq!(
        s.client.member_branch_roles(d1).await.unwrap(),
        vec![role.clone()]
    );
    assert!(s.client.member_branch_roles(d2).await.unwrap().is_empty());
    // R31: the last acted role is recorded on the row, kept across the reconciler's upserts,
    // and missing rows record nothing.
    assert!(s.client.member_acted_roles(d1).await.unwrap().is_empty());
    assert!(s.client.record_member_acted_role(a.id, d1, "r2").await.unwrap());
    assert!(!s.client.record_member_acted_role(a.id, d2, "r2").await.unwrap());
    s.client.upsert_member_branch_role(&role).await.unwrap();
    assert_eq!(
        s.client.member_acted_roles(d1).await.unwrap(),
        vec![(a.id, "r2".to_string())]
    );
    assert!(s.client.member_acted_roles(d2).await.unwrap().is_empty());
    // A tenant admin's touched projects include those it has member roles in.
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    s.client
        .set_tenant_admin(Some(a.id), m.id, true)
        .await
        .unwrap();
    s.client
        .upsert_member_branch_role(&MemberBranchRole {
            member_id: m.id,
            role_name: "cb_m_y".into(),
            ..role.clone()
        })
        .await
        .unwrap();
    assert_eq!(s.client.remove_member(a.id, m.id).await.unwrap(), vec![p]);
    s.client
        .delete_member_branch_role(a.id, d1)
        .await
        .unwrap();
    assert_eq!(s.client.member_branch_roles(d1).await.unwrap().len(), 1);
    s.finish().await;
}

// T8 fix round: what the member-role reconciler reads in one query per branch, the branches
// its sweep visits, and the rows a reset or restore forgets.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn reconciler_reads_grants_branches_and_forgets_rows() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d1 = s.database(t, p).await;
    let d2 = s.database(t, p).await;
    let a = s.client.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = s.client.ensure_member(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
    let o = s.client.ensure_member(t, "o", "o@x.com", "O", Admission::Active).await.unwrap();
    let _idle = s.client.ensure_member(t, "i", "i@x.com", "I", Admission::Active).await.unwrap();
    let dev = |roles: &[&str]| GrantRow {
        platform_role: PlatformRole::Developer,
        pg_roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    s.client.put_project_grant(a.id, m.id, p, &dev(&["r1", "r2"])).await.unwrap();
    let narrow = OverrideRow { platform_role: None, pg_roles: Some(vec!["r1".into()]) };
    s.client.put_override(a.id, m.id, d1, &narrow).await.unwrap();
    // An override without a project grant is still read (it grants nothing on its own).
    s.client.put_override(a.id, o.id, d1, &narrow).await.unwrap();

    let rows = s.client.database_grant_rows(p, d1).await.unwrap();
    let members: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    let mut expected = vec![m.id, o.id];
    expected.sort();
    assert_eq!(members, expected, "members with a grant or an override, by id");
    for (id, levels) in &rows {
        assert_eq!(*levels, s.client.grant_rows(*id, p, Some(d1)).await.unwrap());
    }
    let on_d2 = s.client.database_grant_rows(p, d2).await.unwrap();
    assert_eq!(on_d2.len(), 1, "only the project grant reaches d2");
    assert_eq!(on_d2[0].1.database, None);

    // The sweep visits ready normal branches of tenants with an active member.
    assert!(!s.client.member_role_branches().await.unwrap().contains(&d1), "pending");
    sqlx::query("UPDATE branches SET status = 'ready' WHERE id = $1")
        .bind(d1)
        .execute(s.client.pool())
        .await
        .unwrap();
    let swept = s.client.member_role_branches().await.unwrap();
    assert!(swept.contains(&d1));
    assert!(!swept.contains(&d2));

    for (member, name) in [(m.id, "cb_m_m"), (o.id, "cb_m_o")] {
        s.client
            .upsert_member_branch_role(&MemberBranchRole {
                member_id: member,
                branch_id: d1,
                role_name: name.into(),
                password_enc: "sealed".into(),
                applied_roles: vec![],
            })
            .await
            .unwrap();
    }
    assert_eq!(s.client.delete_branch_member_roles(d1).await.unwrap(), 2);
    assert!(s.client.member_branch_roles(d1).await.unwrap().is_empty());
    s.finish().await;
}

async fn email_of(s: &Scratch, t: TenantId, zid: &str) -> Option<String> {
    s.client
        .member_by_zitadel(t, zid)
        .await
        .unwrap()
        .unwrap()
        .email
}

// R8: a new member's claim takes an email a stale member still holds; it never fails.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn newest_email_claim_wins() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let a = s
        .client
        .ensure_member(t, "a", "x@x.com", "A", Admission::Active)
        .await
        .unwrap();
    assert_eq!(a.email.as_deref(), Some("x@x.com"));
    // A's Zitadel email moved on; Z2 signs up with A's old one.
    let events = s.event_count(t).await;
    let z2 = s
        .client
        .ensure_member(t, "z2", " X@x.com", "Z2", Admission::Active)
        .await
        .unwrap();
    assert_eq!(z2.email.as_deref(), Some("x@x.com"));
    assert_eq!(email_of(&s, t, "a").await, None, "the stale holder lost it");
    assert_eq!(s.event_count(t).await, events + 1, "only member_joined");
    assert_eq!(s.last_action(t).await, "member_joined");
    // A's next request brings its new email.
    let a = s
        .client
        .ensure_member(t, "a", "a2@x.com", "A", Admission::Active)
        .await
        .unwrap();
    assert_eq!(a.email.as_deref(), Some("a2@x.com"));
    assert_eq!(email_of(&s, t, "z2").await.as_deref(), Some("x@x.com"));
    // An empty claim is unknown: stored NULL, nobody else touched.
    let e = s.client.ensure_member(t, "e", "  ", "E", Admission::Active).await.unwrap();
    assert_eq!(e.email, None);
    assert_eq!(email_of(&s, t, "a").await.as_deref(), Some("a2@x.com"));
    s.finish().await;
}

// R8 on the refresh and activation paths: the claimed email moves to the claimant, with no
// event, and the name refreshes with it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn refresh_takes_email_from_stale_holder() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    s.client
        .ensure_member(t, "b", "b@x.com", "B", Admission::Active)
        .await
        .unwrap();
    let events = s.event_count(t).await;
    let b = s
        .client
        .ensure_member(t, "b", "A@x.com ", "B2", Admission::Active)
        .await
        .unwrap();
    assert_eq!(
        (b.email.as_deref(), b.display_name.as_str()),
        (Some("a@x.com"), "B2")
    );
    assert_eq!(email_of(&s, t, "a").await, None);
    // A name-only change refreshes the name and leaves the emails alone.
    let a2 = s.client.ensure_member(t, "a", "", "A2", Admission::Active).await;
    assert_eq!(a2.unwrap().display_name, "A2");
    assert_eq!(email_of(&s, t, "b").await.as_deref(), Some("a@x.com"));
    // Activation takes the email too.
    let (invited, _) = s
        .client
        .create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    let c = s
        .client
        .ensure_member(t, "c", "i@x.com", "C", Admission::Active)
        .await
        .unwrap();
    assert_eq!(c.email.as_deref(), Some("i@x.com"));
    assert_eq!(email_of(&s, t, "zi").await, None);
    let i = s
        .client
        .ensure_member(t, "zi", "i@x.com", "I", Admission::Active)
        .await
        .unwrap();
    assert_eq!(
        (i.id, i.status, i.email.as_deref()),
        (invited.id, MemberStatus::Active, Some("i@x.com"))
    );
    assert_eq!(email_of(&s, t, "c").await, None);
    // Only C's join and I's acceptance were logged.
    let actions: Vec<String> = s
        .client
        .access_events(t, None, None, 100)
        .await
        .unwrap()
        .into_iter()
        .take((s.event_count(t).await - events) as usize)
        .map(|e| e.action)
        .collect();
    assert_eq!(
        actions,
        vec!["invite_accepted", "member_joined", "invite_created"]
    );
    s.finish().await;
}

// R8 under concurrency: claims racing for one email leave exactly one holder, and none fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn concurrent_email_claims_leave_one_holder() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    for zid in ["e1", "e2"] {
        s.client
            .ensure_member(t, zid, &format!("{zid}@x.com"), "E", Admission::Active)
            .await
            .unwrap();
    }
    let hold = s.hold_tenant(t).await;
    let mut set = tokio::task::JoinSet::new();
    for zid in ["e1", "e2", "n1", "n2", "n3", "n4"] {
        let c = s.client.clone();
        set.spawn(async move { c.ensure_member(t, zid, "Same@x.com", "S", Admission::Active).await });
    }
    s.wait_for_lock_waiters(6).await;
    hold.rollback().await.unwrap();
    for result in set.join_all().await {
        result.unwrap();
    }
    let holders: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM members WHERE tenant_id = $1 AND email = 'same@x.com'",
    )
    .bind(t.as_uuid())
    .fetch_one(s.client.pool())
    .await
    .unwrap();
    assert_eq!(holders, 1);
    assert_eq!(s.member_count(t).await, 6);
    s.finish().await;
}

// M8: re-inviting a removed or revoked Zitadel user reuses its row, with nothing left over.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn reinvite_reuses_a_removed_row() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let (i, invite) = s
        .client
        .create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[(p, g.clone())])
        .await
        .unwrap();
    s.client.revoke_invite(a.id, invite).await.unwrap();
    let (again, invite2) = s
        .client
        .create_invited_member(a.id, t, "zi", "i2@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    assert_ne!(invite2, invite);
    assert_eq!(again.id, i.id);
    assert_eq!(
        (again.status, again.tenant_admin, again.email.as_deref()),
        (MemberStatus::Invited, false, Some("i2@x.com"))
    );
    assert!(s.client.member_projects(i.id).await.unwrap().is_empty());

    // A removed former admin comes back invited, not admin, with its old grants gone.
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    s.client
        .set_tenant_admin(Some(a.id), m.id, true)
        .await
        .unwrap();
    s.client.put_project_grant(a.id, m.id, p, &g).await.unwrap();
    s.client.remove_member(a.id, m.id).await.unwrap();
    let (back, _) = s
        .client
        .create_invited_member(a.id, t, "m", "m@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    assert_eq!(back.id, m.id);
    assert_eq!(
        (back.status, back.tenant_admin),
        (MemberStatus::Invited, false)
    );
    assert!(s.client.member_projects(m.id).await.unwrap().is_empty());
    // A live member can't be invited again.
    assert!(matches!(
        s.client
            .create_invited_member(a.id, t, "a", "a3@x.com", week(), TOKEN, &[])
            .await,
        Err(AccessWriteError::Conflict(_))
    ));
    s.finish().await;
}

// M8: another tenant's projects and databases are NotFound, with no row and no event.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn tenant_isolation_is_not_found() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let other = s.tenant().await;
    let p2 = s.project(other).await;
    let d2 = s.database(other, p2).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    let events = s.event_count(t).await;
    let g = grant(PlatformRole::Developer, &["app"]);
    assert!(matches!(
        s.client.put_project_grant(a.id, m.id, p2, &g).await,
        Err(AccessWriteError::NotFound)
    ));
    let o = OverrideRow {
        platform_role: Some(PlatformRole::Viewer),
        pg_roles: None,
    };
    assert!(matches!(
        s.client.put_override(a.id, m.id, d2, &o).await,
        Err(AccessWriteError::NotFound)
    ));
    assert!(matches!(
        s.client
            .create_invited_member(a.id, t, "zx", "x@x.com", week(), TOKEN, &[(p2, g)])
            .await,
        Err(AccessWriteError::NotFound)
    ));
    assert!(s.client.member_by_zitadel(t, "zx").await.unwrap().is_none());
    assert!(s.client.member_projects(m.id).await.unwrap().is_empty());
    assert_eq!(
        s.client.grant_rows(m.id, p2, Some(d2)).await.unwrap(),
        GrantRows::default()
    );
    assert_eq!(s.event_count(t).await, events);
    assert_eq!(s.event_count(other).await, 0);
    s.finish().await;
}

// M8 and M4: the bootstrap activates an invited member and takes a held email (R8).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_activates_an_invited_member() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let (i, invite) = s
        .client
        .create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    let boot = s.client.bootstrap_admin(t, "zi", "i@x.com").await.unwrap();
    assert_eq!(boot.id, i.id);
    assert_eq!(
        (boot.status, boot.tenant_admin),
        (MemberStatus::Active, true)
    );
    assert!(boot.accepted_at.is_some());
    assert!(s.client.invite(invite).await.unwrap().unwrap().3.is_some());
    let event = &s.client.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!(
        (event.action.as_str(), event.actor_member_id),
        ("admin_set", None)
    );
    assert_eq!(s.client.active_tenant_admins(t).await.unwrap().len(), 2);
    // An email another member holds moves to the bootstrapped admin.
    let boot2 = s.client.bootstrap_admin(t, "ops", "A@x.com").await.unwrap();
    assert_eq!(boot2.email.as_deref(), Some("a@x.com"));
    assert_eq!(email_of(&s, t, "a").await, None);
    s.finish().await;
}

// M2: concurrent identical first puts log one event and neither fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn concurrent_first_puts_log_one_event() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let a = s
        .client
        .ensure_member(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member(t, "m", "m@x.com", "M", Admission::Active)
        .await
        .unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let o = OverrideRow {
        platform_role: Some(PlatformRole::Viewer),
        pg_roles: None,
    };
    for kind in ["grant", "override"] {
        let events = s.event_count(t).await;
        // Hold the member row until both puts wait on it.
        let mut hold = s.client.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM members WHERE id = $1 FOR NO KEY UPDATE")
            .bind(m.id)
            .execute(&mut *hold)
            .await
            .unwrap();
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let (c, g, o) = (s.client.clone(), g.clone(), o.clone());
            set.spawn(async move {
                if kind == "grant" {
                    c.put_project_grant(a.id, m.id, p, &g).await
                } else {
                    c.put_override(a.id, m.id, d, &o).await
                }
            });
        }
        s.wait_for_lock_waiters(2).await;
        hold.rollback().await.unwrap();
        for result in set.join_all().await {
            result.unwrap();
        }
        assert_eq!(s.event_count(t).await, events + 1, "{kind}");
        let event = &s.client.access_events(t, None, None, 1).await.unwrap()[0];
        assert_eq!(event.before, None, "{kind}: the one event is the first put");
    }
    s.finish().await;
}

// T7: the member API's reads: by id, by email (trimmed and lowercased, never a NULL or removed
// one, R8), the open invites, and a project's grants with its databases' overrides.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_api_reads() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let other = s.tenant().await;
    let p = s.project(t).await;
    let p2 = s.project(t).await;
    let d = s.database(t, p).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = c.ensure_member(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
    c.ensure_member(other, "o", "o@x.com", "O", Admission::Active).await.unwrap();

    assert_eq!(c.member(m.id).await.unwrap().map(|r| r.id), Some(m.id));
    assert_eq!(c.member(Uuid::new_v4()).await.unwrap(), None);
    assert_eq!(
        c.member_by_email(t, "  M@X.com ").await.unwrap().map(|r| r.id),
        Some(m.id)
    );
    assert_eq!(c.member_by_email(t, "o@x.com").await.unwrap(), None);
    assert_eq!(c.member_by_email(t, "").await.unwrap(), None);

    let (i, invite) = c
        .create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[(p, grant(PlatformRole::Viewer, &[]))])
        .await
        .unwrap();
    let open = c.open_invites(t).await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!((open[0].id, open[0].member_id), (invite, i.id));
    assert!(c.open_invites(other).await.unwrap().is_empty());

    c.put_project_grant(a.id, m.id, p, &grant(PlatformRole::Developer, &["b", "a"]))
        .await
        .unwrap();
    c.put_project_grant(a.id, m.id, p2, &grant(PlatformRole::Viewer, &[]))
        .await
        .unwrap();
    let o = OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None };
    c.put_override(a.id, m.id, d, &o).await.unwrap();
    let access = c.project_access(p).await.unwrap();
    let mut grants = access.grants.clone();
    grants.sort_by_key(|(id, _)| *id);
    let mut want = vec![
        (i.id, grant(PlatformRole::Viewer, &[])),
        (m.id, grant(PlatformRole::Developer, &["a", "b"])),
    ];
    want.sort_by_key(|(id, _)| *id);
    assert_eq!(grants, want);
    assert_eq!(access.overrides, vec![(m.id, d, o)]);
    assert!(c.project_access(p2).await.unwrap().overrides.is_empty());

    // A removed member is found by id only.
    c.remove_member(a.id, m.id).await.unwrap();
    assert_eq!(c.member_by_email(t, "m@x.com").await.unwrap(), None);
    assert_eq!(
        c.member(m.id).await.unwrap().map(|r| r.status),
        Some(MemberStatus::Removed)
    );
    s.finish().await;
}

// T7: a person already in the tenant's org is added active with grants and one
// `member_added` event; a removed one comes back active (not admin, old grants gone); a live
// one is `Conflict`; the email is taken from any other live holder (R8).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn existing_user_is_added_active() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let stale = c.ensure_member(t, "s", "u@x.com", "S", Admission::Active).await.unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let events = s.event_count(t).await;
    let u = c
        .add_existing_user(a.id, t, "zu", " U@x.com", "U", &[(p, g.clone())])
        .await
        .unwrap();
    assert_eq!(
        (u.status, u.tenant_admin, u.email.as_deref(), u.display_name.as_str()),
        (MemberStatus::Active, false, Some("u@x.com"), "U")
    );
    assert_eq!(s.event_count(t).await, events + 1);
    assert_eq!(s.last_action(t).await, "member_added");
    assert_eq!(c.member_projects(u.id).await.unwrap(), vec![(p, g.clone())]);
    assert_eq!(c.member(stale.id).await.unwrap().unwrap().email, None);
    assert!(matches!(
        c.add_existing_user(a.id, t, "zu", "u@x.com", "U", &[]).await,
        Err(AccessWriteError::Conflict(_))
    ));
    // A project of another tenant is NotFound, with nothing written.
    let other = s.tenant().await;
    let foreign = s.project(other).await;
    let before = s.event_count(t).await;
    assert!(matches!(
        c.add_existing_user(a.id, t, "zv", "v@x.com", "V", &[(foreign, g.clone())]).await,
        Err(AccessWriteError::NotFound)
    ));
    assert_eq!(s.event_count(t).await, before);
    assert!(c.member_by_zitadel(t, "zv").await.unwrap().is_none());

    // R29: a removed member never comes back active this way; it is re-invited with a code.
    c.set_tenant_admin(Some(a.id), u.id, true).await.unwrap();
    c.remove_member(a.id, u.id).await.unwrap();
    assert!(matches!(
        c.add_existing_user(a.id, t, "zu", "u@x.com", "U2", &[]).await,
        Err(AccessWriteError::Conflict(_))
    ));
    let (back, _) = c
        .create_invited_member(a.id, t, "zu", "u@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    assert_eq!(back.id, u.id);
    assert_eq!((back.status, back.tenant_admin), (MemberStatus::Invited, false));
    assert!(c.member_projects(u.id).await.unwrap().is_empty());
    s.finish().await;
}

// T7 fix I3 and M6: an existing member's invite puts all its grants in one transaction (all or
// nothing, one event per changed grant); an unchanged grant writes nothing and says so.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn put_grants_is_all_or_nothing() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let p2 = s.project(t).await;
    let foreign = s.project(s.tenant().await).await;
    let d = s.database(t, p).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = c.ensure_member(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let events = s.event_count(t).await;
    assert!(matches!(
        c.put_project_grants(a.id, m.id, &[(p, g.clone()), (foreign, g.clone())]).await,
        Err(AccessWriteError::NotFound)
    ));
    assert!(c.member_projects(m.id).await.unwrap().is_empty());
    assert_eq!(s.event_count(t).await, events);
    let changed = c
        .put_project_grants(a.id, m.id, &[(p, g.clone()), (p2, g.clone())])
        .await
        .unwrap();
    assert_eq!(changed.len(), 2);
    assert_eq!(s.event_count(t).await, events + 2);
    let changed = c
        .put_project_grants(a.id, m.id, &[(p, g.clone()), (p2, grant(PlatformRole::Viewer, &[]))])
        .await
        .unwrap();
    assert_eq!(changed, vec![p2]);
    assert_eq!(s.event_count(t).await, events + 3);
    // Single writes say whether they wrote.
    assert!(!c.put_project_grant(a.id, m.id, p, &g).await.unwrap());
    let o = OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None };
    assert!(c.put_override(a.id, m.id, d, &o).await.unwrap());
    assert!(!c.put_override(a.id, m.id, d, &o).await.unwrap());
    s.finish().await;
}

// T7 fix M1 (R9): only an active member may become a tenant admin, checked under the lock.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn promoting_an_invited_member_conflicts() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let (i, _) = c
        .create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    let events = s.event_count(t).await;
    assert!(matches!(
        c.set_tenant_admin(Some(a.id), i.id, true).await,
        Err(AccessWriteError::Conflict(_))
    ));
    assert_eq!(s.event_count(t).await, events);
    s.finish().await;
}

// T7 fix M3: a project's access log also lists the events whose grants name the project
// (invites, additions and removals); an invite or addition with one grant carries its project.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn project_events_include_listed_grants() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let p2 = s.project(t).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let g = grant(PlatformRole::Viewer, &[]);
    c.create_invited_member(a.id, t, "z1", "one@x.com", week(), TOKEN, &[(p, g.clone())])
        .await
        .unwrap();
    let single = &c.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!((single.action.as_str(), single.project_id), ("invite_created", Some(p)));
    c.create_invited_member(a.id, t, "z2", "two@x.com", week(), TOKEN, &[(p, g.clone()), (p2, g.clone())])
        .await
        .unwrap();
    let u = c
        .add_existing_user(a.id, t, "z3", "three@x.com", "T", &[(p2, g.clone())])
        .await
        .unwrap();
    c.put_project_grant(a.id, u.id, p, &g).await.unwrap();
    c.remove_member(a.id, u.id).await.unwrap();
    let actions = |events: Vec<chelabase_metadata::members::AccessEvent>| -> Vec<String> {
        events.into_iter().map(|e| e.action).collect()
    };
    assert_eq!(
        actions(c.access_events(t, Some(p), None, 50).await.unwrap()),
        vec!["member_removed", "grant_put", "invite_created", "invite_created"]
    );
    assert_eq!(
        actions(c.access_events(t, Some(p2), None, 50).await.unwrap()),
        vec!["member_removed", "member_added", "invite_created"]
    );
    s.finish().await;
}

// T12x: the access log carries names, joined at read time. Removed people still resolve (their
// rows are kept), a soft-deleted project resolves to its last name, a project row that is gone
// to nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn access_events_join_names() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let (m, _) = c
        .create_invited_member(a.id, t, "zm", "m@x.com", week(), TOKEN, &[])
        .await
        .unwrap();
    let ev = &c.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!(ev.actor_email.as_deref(), Some("a@x.com"));
    assert_eq!(ev.target_email.as_deref(), Some("m@x.com"));
    assert_eq!(ev.project_name, None, "an event with no project has no project name");

    c.put_project_grant(a.id, m.id, p, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    let ev = &c.access_events(t, None, None, 1).await.unwrap()[0];
    assert_eq!(ev.action, "grant_put");
    assert_eq!(ev.project_name.as_deref(), Some(format!("p-{p}").as_str()));

    // The target is removed, the project soft-deleted: both still resolve.
    c.ensure_member(t, "zm", "m@x.com", "M", Admission::Active).await.unwrap();
    c.remove_member(a.id, m.id).await.unwrap();
    sqlx::query("UPDATE projects SET status = 'deleted' WHERE id = $1")
        .bind(p)
        .execute(c.pool())
        .await
        .unwrap();
    let all = c.access_events(t, None, None, 50).await.unwrap();
    let put = all.iter().find(|e| e.action == "grant_put").unwrap();
    assert_eq!(put.target_email.as_deref(), Some("m@x.com"));
    assert_eq!(put.project_name.as_deref(), Some(format!("p-{p}").as_str()));
    let removed = all.iter().find(|e| e.action == "member_removed").unwrap();
    assert_eq!(removed.target_email.as_deref(), Some("m@x.com"));
    assert_eq!(removed.actor_email.as_deref(), Some("a@x.com"));

    // A project row that is gone resolves to nothing; the event stays.
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(p)
        .execute(c.pool())
        .await
        .unwrap();
    let all = c.access_events(t, Some(p), None, 50).await.unwrap();
    let put = all.iter().find(|e| e.action == "grant_put").unwrap();
    assert_eq!((put.project_id, put.project_name.as_deref()), (Some(p), None));
    s.finish().await;
}

// T12x: `has_access` is a tenant admin or at least one project grant; an override alone doesn't
// count. `created_at` is the member row's.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_listings_report_has_access() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let d = s.database(t, p).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    assert!(a.tenant_admin);
    let granted = c.add_existing_user(a.id, t, "zg", "g@x.com", "G", &[(p, grant(PlatformRole::Viewer, &[]))]).await.unwrap();
    let bare = c.add_existing_user(a.id, t, "zb", "b@x.com", "B", &[]).await.unwrap();
    let list = c.list_member_listings(t).await.unwrap();
    let has = |id: Uuid| list.iter().find(|l| l.member.id == id).unwrap().has_access;
    assert_eq!((has(a.id), has(granted.id), has(bare.id)), (true, true, false));
    assert!(list.iter().all(|l| l.created_at <= Utc::now()));
    let one = c.member_listing(granted.id).await.unwrap().unwrap();
    assert_eq!((one.member.id, one.has_access), (granted.id, true));
    // An override alone is not access: the grant is what gives it.
    c.put_project_grant(a.id, bare.id, p, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    c.put_override(a.id, bare.id, d, &OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None }).await.unwrap();
    c.delete_project_grant(a.id, bare.id, p).await.unwrap();
    assert!(!c.member_listing(bare.id).await.unwrap().unwrap().has_access);
    // A grant on a soft-deleted project is no access (as in `member_projects`), in the list
    // and in the single read.
    let gone = s.project(t).await;
    c.put_project_grant(a.id, bare.id, gone, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    assert!(c.member_listing(bare.id).await.unwrap().unwrap().has_access);
    sqlx::query("UPDATE projects SET status = 'deleted' WHERE id = $1")
        .bind(gone)
        .execute(c.pool())
        .await
        .unwrap();
    assert!(!c.member_listing(bare.id).await.unwrap().unwrap().has_access);
    let list = c.list_member_listings(t).await.unwrap();
    assert!(!list.iter().find(|l| l.member.id == bare.id).unwrap().has_access);
    // Created in order: the rows' `created_at` follows insertion, and a write returns it.
    let (first, _) = c.create_invited_member(a.id, t, "zo1", "o1@x.com", week(), TOKEN, &[]).await.unwrap();
    let (second, _) = c
        .create_invited_member(a.id, t, "zo2", "o2@x.com", week(), TOKEN, &[(p, grant(PlatformRole::Viewer, &[]))])
        .await
        .unwrap();
    assert!(first.created_at < second.created_at);
    assert!(!first.has_access && second.has_access);
    assert!(granted.created_at <= first.created_at);
    let read = c.member_listing(second.member.id).await.unwrap().unwrap();
    assert_eq!((read.created_at, read.has_access), (second.created_at, true));
    s.finish().await;
}

// T12x: an invite is described by (Zitadel user, token hash) while it is open; the hash is
// replaced by a resend and every closed invite describes as nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn invite_description_follows_the_open_invite() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "Alice", Admission::Active).await.unwrap();
    assert!(c.open_invite_for_user("zi").await.unwrap().is_none());
    let (_, invite) = c.create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[]).await.unwrap();
    let d = c.open_invite_for_user("zi").await.unwrap().unwrap();
    assert_eq!(d.token_hash.as_deref(), Some(&TOKEN[..]));
    assert_eq!((d.inviter_name.as_str(), d.inviter_email.as_deref()), ("Alice", Some("a@x.com")));
    assert_eq!(d.tenant_slug, format!("t-{}", t.as_uuid()));
    assert_eq!(d.tenant_display_name, None);
    c.resend_invite(a.id, invite, week(), [9; 32]).await.unwrap();
    assert_eq!(c.open_invite_for_user("zi").await.unwrap().unwrap().token_hash.as_deref(), Some(&[9u8; 32][..]));
    // Expired, then revoked: nothing.
    sqlx::query("UPDATE invites SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(invite)
        .execute(c.pool())
        .await
        .unwrap();
    assert!(c.open_invite_for_user("zi").await.unwrap().is_none());
    c.resend_invite(a.id, invite, week(), [9; 32]).await.unwrap();
    assert!(c.open_invite_for_user("zi").await.unwrap().is_some());
    c.revoke_invite(a.id, invite).await.unwrap();
    assert!(c.open_invite_for_user("zi").await.unwrap().is_none());
    s.finish().await;
}

// R36: the display name is nullable, never empty or padded, and a change writes one event.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn tenant_display_name_round_trip() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "Alice", Admission::Active).await.unwrap();
    let name = || async { c.tenant_overview(t).await.unwrap().unwrap().display_name };
    assert_eq!(name().await, None);
    c.set_tenant_display_name(a.id, t, Some("Acme Corp")).await.unwrap();
    assert_eq!(name().await.as_deref(), Some("Acme Corp"));
    // The invite description carries the display name, and the slug to fall back to.
    c.create_invited_member(a.id, t, "zi", "i@x.com", week(), TOKEN, &[]).await.unwrap();
    let d = c.open_invite_for_user("zi").await.unwrap().unwrap();
    assert_eq!(d.tenant_display_name.as_deref(), Some("Acme Corp"));
    c.set_tenant_display_name(a.id, t, None).await.unwrap();
    assert_eq!(name().await, None);
    let d = c.open_invite_for_user("zi").await.unwrap().unwrap();
    assert_eq!((d.tenant_slug, d.tenant_display_name), (format!("t-{}", t.as_uuid()), None));
    let events: Vec<_> = c
        .access_events(t, None, None, 50)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "tenant_renamed")
        .collect();
    assert_eq!(events.len(), 2);
    // Newest first: the clear, then the set.
    assert_eq!(events[0].before, Some(serde_json::json!({ "display_name": "Acme Corp" })));
    assert_eq!(events[0].after, Some(serde_json::json!({ "display_name": null })));
    assert_eq!(events[1].before, Some(serde_json::json!({ "display_name": null })));
    assert_eq!(events[1].after, Some(serde_json::json!({ "display_name": "Acme Corp" })));
    // The column refuses what the API never sends.
    for bad in ["", " padded ", "a\nb", "a\u{7}b", &"x".repeat(81)] {
        let refused = sqlx::query("UPDATE tenants SET display_name = $2 WHERE id = $1")
            .bind(t.as_uuid())
            .bind(bad)
            .execute(c.pool())
            .await;
        assert!(refused.is_err(), "{bad:?}");
    }
    assert_eq!(name().await, None);
}

// R40: a person who signs up under open sign-up is pending; an admin approves or declines.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn pending_member_lifecycle() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let c = &s.client;
    let a = c.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    assert!(a.tenant_admin);
    let actions = |t: TenantId| async move {
        c.access_events(t, None, None, 100).await.unwrap().into_iter().rev().map(|e| e.action).collect::<Vec<_>>()
    };

    // Closed: no row, no event. Pending: a pending row and a `member_joined` event.
    let before = s.event_count(t).await;
    let closed = c.ensure_member(t, "c", "c@x.com", "C", Admission::Closed).await;
    assert!(matches!(closed, Err(chelabase_metadata::MetadataError::NotAdmitted(_))), "{closed:?}");
    assert!(c.member_by_zitadel(t, "c").await.unwrap().is_none());
    assert_eq!(s.event_count(t).await, before);
    let b = c.ensure_member(t, "b", "b@x.com", "B", Admission::Pending).await.unwrap();
    assert_eq!((b.status, b.tenant_admin), (MemberStatus::Pending, false));
    assert_eq!(actions(t).await.last().map(String::as_str), Some("member_joined"));
    // It stays pending whatever it asks for later, and is listed.
    for admission in [Admission::Active, Admission::Pending, Admission::Closed] {
        let again = c.ensure_member(t, "b", "b@x.com", "B", admission).await.unwrap();
        assert_eq!(again.status, MemberStatus::Pending);
    }
    let listed = c.list_members(t).await.unwrap();
    assert!(listed.iter().any(|m| m.id == b.id && m.status == MemberStatus::Pending));
    // A pending member can't become a tenant admin, and has no grants.
    assert!(matches!(
        c.set_tenant_admin(Some(a.id), b.id, true).await,
        Err(AccessWriteError::Conflict(_))
    ));

    // Approval: active, with the grants, one event, in one step.
    let events = s.event_count(t).await;
    let listing = c.approve_member(a.id, b.id, &[(p, grant(PlatformRole::Developer, &[]))]).await.unwrap();
    assert_eq!(listing.member.status, MemberStatus::Active);
    assert!(listing.has_access);
    assert_eq!(s.event_count(t).await, events + 1);
    let last = c.access_events(t, None, None, 1).await.unwrap().remove(0);
    assert_eq!((last.action.as_str(), last.project_id), ("member_approved", Some(p)));
    assert_eq!(
        c.grant_rows(b.id, p, None).await.unwrap().project.map(|g| g.platform_role),
        Some(PlatformRole::Developer)
    );
    assert!(matches!(c.approve_member(a.id, b.id, &[]).await, Err(AccessWriteError::Conflict(_))));

    // A grant naming the owner, or a project of another tenant, writes nothing.
    let d = c.ensure_member(t, "d", "d@x.com", "D", Admission::Pending).await.unwrap();
    let events = s.event_count(t).await;
    assert!(matches!(
        c.approve_member(a.id, d.id, &[(p, grant(PlatformRole::Developer, &["owner"]))]).await,
        Err(AccessWriteError::Conflict(_))
    ));
    let other = s.tenant().await;
    let foreign = s.project(other).await;
    assert!(matches!(
        c.approve_member(a.id, d.id, &[(foreign, grant(PlatformRole::Viewer, &[]))]).await,
        Err(AccessWriteError::NotFound)
    ));
    assert_eq!(c.member_by_zitadel(t, "d").await.unwrap().unwrap().status, MemberStatus::Pending);
    assert_eq!(s.event_count(t).await, events);

    // Decline: removed, one event, the Zitadel user to deactivate; not for the active.
    assert_eq!(c.decline_member(a.id, d.id).await.unwrap(), "d");
    assert_eq!(c.member_by_zitadel(t, "d").await.unwrap().unwrap().status, MemberStatus::Removed);
    assert_eq!(actions(t).await.last().map(String::as_str), Some("member_declined"));
    assert!(matches!(c.decline_member(a.id, d.id).await, Err(AccessWriteError::MemberNotFound)));
    assert!(matches!(c.decline_member(a.id, b.id).await, Err(AccessWriteError::Conflict(_))));
    assert!(matches!(c.approve_member(a.id, d.id, &[]).await, Err(AccessWriteError::MemberNotFound)));

    // A pending member never becomes the first admin; the next person to ask claims it.
    let t2 = s.tenant().await;
    sqlx::query(
        "INSERT INTO members (id, tenant_id, zitadel_user_id, email, status)
         VALUES (gen_random_uuid(), $1, 'pz', 'p@x.com', 'pending')",
    )
    .bind(t2.as_uuid())
    .execute(c.pool())
    .await
    .unwrap();
    let p2 = c.ensure_member(t2, "pz", "p@x.com", "P", Admission::Pending).await.unwrap();
    assert_eq!((p2.status, p2.tenant_admin), (MemberStatus::Pending, false));
    assert!(c.active_tenant_admins(t2).await.unwrap().is_empty());
    let first = c.ensure_member(t2, "first", "f@x.com", "F", Admission::Closed).await.unwrap();
    assert_eq!((first.status, first.tenant_admin), (MemberStatus::Active, true));
    assert_eq!(c.member_by_zitadel(t2, "pz").await.unwrap().unwrap().status, MemberStatus::Pending);
    s.finish().await;
}

// M2: the bootstrap's `NAME_SQL` (psql variables swapped for bind parameters) names a tenant
// once, trimmed and cut to the column's rules, and logs one event.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_names_the_tenant_once() {
    const NAME_SQL: &str = include_str!("../../../tools/cli/src/dev/tenant_name.sql");
    let s = Scratch::new().await;
    let sql = NAME_SQL.replace(":'tenant'", "$1").replace(":'name'", "$2");
    let run = |t: TenantId, name: &str| {
        let (sql, name, pool) = (sql.clone(), name.to_string(), s.client.pool().clone());
        async move {
            sqlx::query(AssertSqlSafe(sql)).bind(t.as_uuid()).bind(name).execute(&pool).await.unwrap();
        }
    };
    let name_of = |t: TenantId| {
        let pool = s.client.pool().clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>("SELECT display_name FROM tenants WHERE id = $1")
                .bind(t.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let t = s.tenant().await;
    run(t, "  Acme Corp  ").await;
    assert_eq!(name_of(t).await.as_deref(), Some("Acme Corp"));
    assert_eq!(s.event_count(t).await, 1);
    // A named tenant keeps its name, and a repeat writes nothing.
    run(t, "Other").await;
    assert_eq!(name_of(t).await.as_deref(), Some("Acme Corp"));
    assert_eq!(s.event_count(t).await, 1);
    // Too long: cut to 80 and trimmed (a space at the cut would break the column's check).
    let long = s.tenant().await;
    run(long, &format!("{} tail", "x".repeat(79))).await;
    assert_eq!(name_of(long).await.as_deref(), Some("x".repeat(79).as_str()));
    // Blank: nothing.
    let blank = s.tenant().await;
    run(blank, "   ").await;
    assert_eq!(name_of(blank).await, None);
    assert_eq!(s.event_count(blank).await, 0);
    s.finish().await;
}

// M5: the CLI's bootstrap SQL (`tenant_admin.sql`, psql variables replaced by quoted literals)
// writes exactly what `bootstrap_admin` writes, on every starting state: members and events
// alike (ids and times aside), and a repeat writes nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_admin_sql_matches_the_metadata_crate() {
    const ADMIN_SQL: &str = include_str!("../../../tools/cli/src/dev/tenant_admin.sql");
    let s = Scratch::new().await;
    let quote = |v: &str| format!("'{}'", v.replace('\'', "''"));
    let run_sql = |t: TenantId, zuid: &str, email: &str| {
        let sql = ADMIN_SQL
            .replace(":'tenant'", &quote(&t.as_uuid().to_string()))
            .replace(":'zuid'", &quote(zuid))
            .replace(":'email'", &quote(email));
        let pool = s.client.pool().clone();
        async move {
            sqlx::raw_sql(AssertSqlSafe(format!("BEGIN;\n{sql}\nCOMMIT;"))).execute(&pool).await.unwrap();
        }
    };
    type Snapshot = (Vec<(String, String, bool, Option<String>, bool)>, Vec<(String, bool, Option<Value>, Option<Value>)>);
    let snapshot = |t: TenantId| {
        let pool = s.client.pool().clone();
        async move {
            let members = sqlx::query_as::<_, (String, String, bool, Option<String>, bool)>(
                "SELECT zitadel_user_id, status, tenant_admin, email, accepted_at IS NOT NULL
                 FROM members WHERE tenant_id = $1 ORDER BY zitadel_user_id",
            )
            .bind(t.as_uuid())
            .fetch_all(&pool)
            .await
            .unwrap();
            let events = sqlx::query_as::<_, (String, bool, Option<Value>, Option<Value>)>(
                // Each tenant's setup minted its invite at its own instant: `expires_at` differs.
                "SELECT action, actor_member_id IS NULL, before - 'expires_at', after - 'expires_at'
                 FROM access_events WHERE tenant_id = $1 ORDER BY id",
            )
            .bind(t.as_uuid())
            .fetch_all(&pool)
            .await
            .unwrap();
            let open_invites: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM invites i JOIN members m ON m.id = i.member_id
                 WHERE m.tenant_id = $1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL",
            )
            .bind(t.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
            let snap: Snapshot = (members, events);
            (snap, open_invites)
        }
    };
    use serde_json::Value;
    // Each starting state is made twice, in two tenants: one for each path.
    type Setup = for<'a> fn(&'a Scratch, TenantId) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>;
    let setups: Vec<(&str, Setup)> = vec![
        ("empty tenant", |_, _| Box::pin(async {})),
        ("invited with an open invite", |s, t| {
            Box::pin(async move {
                let a = s.client.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
                s.client.create_invited_member(a.id, t, "boss", "boss@x.com", week(), TOKEN, &[]).await.unwrap();
            })
        }),
        ("removed", |s, t| {
            Box::pin(async move {
                let a = s.client.ensure_member(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
                s.client.ensure_member(t, "boss", "boss@x.com", "B", Admission::Active).await.unwrap();
                let b = s.client.member_by_zitadel(t, "boss").await.unwrap().unwrap();
                s.client.remove_member(a.id, b.id).await.unwrap();
            })
        }),
        ("the email held by another member", |s, t| {
            Box::pin(async move {
                s.client.ensure_member(t, "a", "boss@x.com", "A", Admission::Active).await.unwrap();
            })
        }),
        ("already the admin", |s, t| {
            Box::pin(async move {
                s.client.bootstrap_admin(t, "boss", "boss@x.com").await.unwrap();
            })
        }),
    ];
    for (name, setup) in setups {
        let (by_sql, by_crate) = (s.tenant().await, s.tenant().await);
        setup(&s, by_sql).await;
        setup(&s, by_crate).await;
        run_sql(by_sql, "boss", " Boss@X.com ").await;
        s.client.bootstrap_admin(by_crate, "boss", " Boss@X.com ").await.unwrap();
        assert_eq!(snapshot(by_sql).await, snapshot(by_crate).await, "{name}");
        // A repeat writes nothing, on either path.
        let events = snapshot(by_sql).await.0.1.len();
        run_sql(by_sql, "boss", " Boss@X.com ").await;
        s.client.bootstrap_admin(by_crate, "boss", " Boss@X.com ").await.unwrap();
        assert_eq!(snapshot(by_sql).await, snapshot(by_crate).await, "{name}: repeat");
        assert_eq!(snapshot(by_sql).await.0.1.len(), events, "{name}: the repeat wrote an event");
    }
    s.finish().await;
}

// The invariant behind R40 (I-A): a write on a member is judged by its status, under the member's
// lock, and the same way on every path. Exhaustive over status x write; the matrix is the spec.
//
//   outcome of the write on a member that is...  invited   active    pending   removed
//   grant put / grant delete                     ok        ok        conflict  not_found
//   override put / override delete               ok        ok        conflict  not_found
//   approve (pending -> active)                  conflict  conflict  ok        member_gone
//   decline (pending -> removed)                 conflict  conflict  ok        member_gone
//   promote to tenant admin                      conflict  ok        conflict  not_found
//   remove                                       ok        ok        ok        ok (nothing to do)
//   invite (an invite code for its user)         conflict  conflict  conflict  ok (invited again)
//
// A pending member has no grants or overrides until an admin approves it, so a write that would
// give it some is refused ("approve first"), even one that only deletes.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_status_write_matrix() {
    #[derive(Clone, Copy)]
    enum Write {
        GrantPut,
        GrantDelete,
        OverridePut,
        OverrideDelete,
        Approve,
        Decline,
        Promote,
        Remove,
        Invite,
    }
    use Write::*;
    const STATUSES: [&str; 4] = ["invited", "active", "pending", "removed"];
    const WRITES: [(Write, &str, [&str; 4]); 9] = [
        (GrantPut, "grant put", ["ok", "ok", "conflict", "not_found"]),
        (GrantDelete, "grant delete", ["ok", "ok", "conflict", "not_found"]),
        (OverridePut, "override put", ["ok", "ok", "conflict", "not_found"]),
        (OverrideDelete, "override delete", ["ok", "ok", "conflict", "not_found"]),
        (Approve, "approve", ["conflict", "conflict", "ok", "member_gone"]),
        (Decline, "decline", ["conflict", "conflict", "ok", "member_gone"]),
        (Promote, "promote", ["conflict", "ok", "conflict", "not_found"]),
        (Remove, "remove", ["ok", "ok", "ok", "ok"]),
        (Invite, "invite", ["conflict", "conflict", "conflict", "ok"]),
    ];
    fn name<T>(r: Result<T, AccessWriteError>) -> &'static str {
        match r {
            Ok(_) => "ok",
            Err(AccessWriteError::Conflict(_)) => "conflict",
            Err(AccessWriteError::NotFound) => "not_found",
            Err(AccessWriteError::MemberNotFound) => "member_gone",
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let db = s.database(t, p).await;
    let c = &s.client;
    let admin = c.ensure_member(t, "admin", "admin@x.com", "Admin", Admission::Active).await.unwrap();
    let mut failures = Vec::new();
    let mut n = 0;
    for (status_index, status) in STATUSES.into_iter().enumerate() {
        for (write, label, expected) in WRITES {
            n += 1;
            let zid = format!("z{n}");
            let email = format!("m{n}@x.com");
            // The member in `status`, holding a grant and an override (so a delete has something
            // to delete, and a refused write is seen to leave them alone).
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO members (id, tenant_id, zitadel_user_id, email, status)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(id)
            .bind(t.as_uuid())
            .bind(&zid)
            .bind(&email)
            .bind(status)
            .execute(c.pool())
            .await
            .unwrap();
            sqlx::query("INSERT INTO project_grants (member_id, project_id, platform_role) VALUES ($1, $2, 'viewer')")
                .bind(id)
                .bind(p)
                .execute(c.pool())
                .await
                .unwrap();
            sqlx::query("INSERT INTO database_grant_overrides (member_id, database_id, platform_role) VALUES ($1, $2, 'viewer')")
                .bind(id)
                .bind(db)
                .execute(c.pool())
                .await
                .unwrap();
            let narrower = OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: Some(vec![]) };
            let got = match write {
                GrantPut => name(c.put_project_grant(admin.id, id, p, &grant(PlatformRole::Developer, &[])).await),
                GrantDelete => name(c.delete_project_grant(admin.id, id, p).await),
                OverridePut => name(c.put_override(admin.id, id, db, &narrower).await),
                OverrideDelete => name(c.delete_override(admin.id, id, db).await),
                Approve => name(c.approve_member(admin.id, id, &[]).await),
                Decline => name(c.decline_member(admin.id, id).await),
                Promote => name(c.set_tenant_admin(Some(admin.id), id, true).await),
                Remove => name(c.remove_member(admin.id, id).await),
                Invite => name(c.create_invited_member(admin.id, t, &zid, &email, week(), TOKEN, &[]).await),
            };
            if got != expected[status_index] {
                failures.push(format!("{label} on a {status} member: {got}, want {}", expected[status_index]));
            }
            // A refused write changes nothing about the member's grants and overrides.
            if got == "conflict" || got == "not_found" || got == "member_gone" {
                let kept: i64 = sqlx::query_scalar(
                    "SELECT (SELECT count(*) FROM project_grants WHERE member_id = $1 AND platform_role = 'viewer')
                          + (SELECT count(*) FROM database_grant_overrides WHERE member_id = $1 AND platform_role = 'viewer')",
                )
                .bind(id)
                .fetch_one(c.pool())
                .await
                .unwrap();
                if kept != 2 && !matches!(write, Remove) {
                    failures.push(format!("{label} on a {status} member was refused but changed its grants"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    s.finish().await;
}
