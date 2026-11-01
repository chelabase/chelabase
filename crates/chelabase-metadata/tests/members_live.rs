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
    AccessAction, AccessEvent, AccessWriteError, Admission, GrantRow, MemberBranchRole,
    MemberStatus, NewInvite, Observed, OverrideRow, TeamSeen,
};
use chelabase_metadata::personal_logins::{
    AccessEventInput, GrantVia, LoginEndpoint, LoginStatus, LoginWriteError, MemberProjectRole,
    PersonalLogin, RevokeReason,
};
use chelabase_metadata::teams::{TeamChange, TeamWrite, TeamWriteError};
use chelabase_metadata::{
    BranchStatus, CreateBranchOutcome, DatabaseStatus, MetadataClient, MetadataConfig, NewBranch,
    NewEndpoint, RenameCopyOutcome,
};
use chelabase_types::access::{GrantSources, PlatformRole, qualifies, tenant_admin_effective};
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use serde_json::json;
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
            .access_events(tenant, None, None, None, 1)
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

/// An invite of `zitadel_user_id` at `email`, open for a week, with `TOKEN` and `grants`.
fn new_invite<'a>(zitadel_user_id: &'a str, email: &'a str, grants: &'a [(Uuid, GrantRow)]) -> NewInvite<'a> {
    NewInvite { zitadel_user_id, email, expires_at: week(), token_hash: TOKEN, grants, teams: &[] }
}

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
            c.ensure_member_first_comer_for_tests(t, &format!("z{i}"), &format!("  User{i}@Example.COM "), "N", Admission::Active)
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
    let events = s.client.access_events(t, None, None, None, 100).await.unwrap();
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let b = s
        .client
        .ensure_member_first_comer_for_tests(t, "b", "b@x.com", "B", Admission::Active)
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
                        .ensure_member_first_comer_for_tests(t, &zid, &email, "n", Admission::Active)
                        .await
                        .map(|_| ())
                        .map_err(|e| AccessWriteError::Conflict(format!("ensure_member: {e}"))),
                    (Op::Add, _) => {
                        match c
                            .create_invited_member(actor.unwrap(), t, new_invite(&zid, &email, &[]))
                            .await
                        {
                            Ok(_) => c
                                .ensure_member_first_comer_for_tests(t, &zid, &email, "n", Admission::Active)
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
        .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[(p, grant(PlatformRole::Viewer, &["owner"]))]))
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
            .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[(p, admin.clone())]))
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
        s.client.grant_sources(m.id, p, Some(d)).await.unwrap(),
        GrantSources::default()
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let (m, invite) = s
        .client
        .create_invited_member(a.id, t, new_invite("zi", " Inv@X.com ", &[(p, g.clone())]))
        .await
        .unwrap();
    assert_eq!(m.status, MemberStatus::Invited);
    assert_eq!(m.email.as_deref(), Some("inv@x.com"));
    assert!(!m.tenant_admin);
    let (member, _, revoked, accepted) = s.client.invite(invite).await.unwrap().unwrap();
    assert_eq!((member, revoked, accepted), (m.id, None, None));

    let active = s
        .client
        .ensure_member_first_comer_for_tests(t, "zi", "inv@x.com", "Inv", Admission::Active)
        .await
        .unwrap();
    assert_eq!(active.id, m.id);
    assert_eq!(active.status, MemberStatus::Active);
    assert_eq!(active.display_name, "Inv");
    assert!(active.accepted_at.is_some());
    assert!(!active.tenant_admin);
    assert!(s.client.invite(invite).await.unwrap().unwrap().3.is_some());
    assert_eq!(s.client.member_projects(m.id).await.unwrap(), vec![(p, g)]);
    let accepted = &s.client.access_events(t, None, None, None, 1).await.unwrap()[0];
    assert_eq!(accepted.action, "invite_accepted");
    assert_eq!(accepted.actor_member_id, Some(m.id));
    assert_eq!(accepted.target_member_id, Some(m.id));

    // The fast path: nothing changed, nothing written.
    let events = s.event_count(t).await;
    s.client
        .ensure_member_first_comer_for_tests(t, "zi", "inv@x.com", "Inv", Admission::Active)
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let (m, invite) = s
        .client
        .create_invited_member(a.id, t, new_invite("zi", "inv@x.com", &[]))
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
        .ensure_member_first_comer_for_tests(t, "zi", "inv@x.com", "Inv", Admission::Active)
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
        .ensure_member_first_comer_for_tests(t, "zi", "inv@x.com", "Inv", Admission::Active)
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
    s.client.ensure_member_first_comer_for_tests(claimed, "first", "f@x.com", "F", Admission::Closed).await.unwrap();
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
    let a = s.client.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = s.client.ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
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
        s.client.member_projects(m.id).await.unwrap(),
        vec![(p, grant(PlatformRole::Admin, &["plain"]))]
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
        let grants = s.client.member_projects(m.id).await.unwrap();
        let overrides = s.client.member_overrides(m.id).await.unwrap();
        let has_boss = grants.iter().any(|(gp, g)| *gp == p && g.pg_roles.contains(&"boss".to_string()));
        let has_override = overrides.iter().any(|(_, od, _)| *od == d);
        assert!(!(has_boss && has_override), "round {round}: {grants:?} {overrides:?}");
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
    let removed = &s.client.access_events(t, None, None, None, 1).await.unwrap()[0];
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
        s.client.grant_sources(m.id, p, Some(d)).await.unwrap(),
        GrantSources::default()
    );

    let events = s.event_count(t).await;
    let again = s
        .client
        .ensure_member_first_comer_for_tests(t, "m", "new@x.com", "New", Admission::Active)
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
        s.client.grant_sources(m.id, p, Some(d)).await.unwrap(),
        GrantSources::new(
            Some(grant(PlatformRole::Developer, &["a", "b"]).to_grant()),
            Vec::new(),
            Some(OverrideRow { platform_role: None, pg_roles: Some(vec!["a".into(), "b".into()]) }.to_narrowing()),
        )
    );
    assert_eq!(
        s.client.grant_sources(m.id, p, None).await.unwrap(),
        GrantSources::new(Some(grant(PlatformRole::Developer, &["a", "b"]).to_grant()), Vec::new(), None)
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
        c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap()
    );
    let m = one_event!(
        "member_joined",
        c.ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active).await.unwrap()
    );
    no_event!(
        "unchanged claims",
        c.ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active).await.unwrap()
    );
    let refreshed = no_event!(
        "changed claims",
        c.ensure_member_first_comer_for_tests(t, "m", " M2@x.com", "M2", Admission::Active).await.unwrap()
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
    let put = &c.access_events(t, None, None, None, 1).await.unwrap()[0];
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
    let put = &c.access_events(t, None, None, None, 1).await.unwrap()[0];
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
        c.create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[(p, g.clone())]))
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
        c.create_invited_member(a.id, t, new_invite("zj", "j@x.com", &[]))
            .await
            .unwrap()
    );
    one_event!(
        "invite_accepted",
        c.ensure_member_first_comer_for_tests(t, "zj", "j@x.com", "J", Admission::Active).await.unwrap()
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
        c.access_events(t, None, None, None, 1).await.unwrap()[0].actor_member_id,
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
    let all = c.access_events(t, None, None, None, 100).await.unwrap();
    assert_eq!(all.len() as i64, n);
    assert!(all.windows(2).all(|w| w[0].id > w[1].id));
    let page = c.access_events(t, None, None, Some(all[2].id), 2).await.unwrap();
    assert_eq!(
        page.iter().map(|e| e.id).collect::<Vec<_>>(),
        vec![all[3].id, all[4].id]
    );
    let in_project = c.access_events(t, Some(p), None, None, 100).await.unwrap();
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
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
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
    let a = s.client.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = s.client.ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
    let o = s.client.ensure_member_first_comer_for_tests(t, "o", "o@x.com", "O", Admission::Active).await.unwrap();
    let _idle = s.client.ensure_member_first_comer_for_tests(t, "i", "i@x.com", "I", Admission::Active).await.unwrap();
    let dev = |roles: &[&str]| GrantRow {
        platform_role: PlatformRole::Developer,
        pg_roles: roles.iter().map(|r| r.to_string()).collect(),
    };
    s.client.put_project_grant(a.id, m.id, p, &dev(&["r1", "r2"])).await.unwrap();
    let narrow = OverrideRow { platform_role: None, pg_roles: Some(vec!["r1".into()]) };
    s.client.put_override(a.id, m.id, d1, &narrow).await.unwrap();
    // An override without a project grant is still read (it grants nothing on its own).
    s.client.put_override(a.id, o.id, d1, &narrow).await.unwrap();

    let rows = s.client.database_grant_sources(p, d1).await.unwrap();
    let members: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    let mut expected = vec![m.id, o.id];
    expected.sort();
    assert_eq!(members, expected, "members with a grant or an override, by id");
    for (id, sources) in &rows {
        assert_eq!(*sources, s.client.grant_sources(*id, p, Some(d1)).await.unwrap());
    }
    // The override alone gives no access (MH-2: never a default level).
    let lonely = rows.iter().find(|(id, _)| *id == o.id).unwrap();
    assert_eq!(lonely.1.effective(), None);
    let on_d2 = s.client.database_grant_sources(p, d2).await.unwrap();
    assert_eq!(on_d2.len(), 1, "only the project grant reaches d2");
    assert_eq!(on_d2[0].1, GrantSources::new(Some(dev(&["r1", "r2"]).to_grant()), Vec::new(), None));

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
        .ensure_member_first_comer_for_tests(t, "a", "x@x.com", "A", Admission::Active)
        .await
        .unwrap();
    assert_eq!(a.email.as_deref(), Some("x@x.com"));
    // A's Zitadel email moved on; Z2 signs up with A's old one.
    let events = s.event_count(t).await;
    let z2 = s
        .client
        .ensure_member_first_comer_for_tests(t, "z2", " X@x.com", "Z2", Admission::Active)
        .await
        .unwrap();
    assert_eq!(z2.email.as_deref(), Some("x@x.com"));
    assert_eq!(email_of(&s, t, "a").await, None, "the stale holder lost it");
    assert_eq!(s.event_count(t).await, events + 1, "only member_joined");
    assert_eq!(s.last_action(t).await, "member_joined");
    // A's next request brings its new email.
    let a = s
        .client
        .ensure_member_first_comer_for_tests(t, "a", "a2@x.com", "A", Admission::Active)
        .await
        .unwrap();
    assert_eq!(a.email.as_deref(), Some("a2@x.com"));
    assert_eq!(email_of(&s, t, "z2").await.as_deref(), Some("x@x.com"));
    // An empty claim is unknown: stored NULL, nobody else touched.
    let e = s.client.ensure_member_first_comer_for_tests(t, "e", "  ", "E", Admission::Active).await.unwrap();
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    s.client
        .ensure_member_first_comer_for_tests(t, "b", "b@x.com", "B", Admission::Active)
        .await
        .unwrap();
    let events = s.event_count(t).await;
    let b = s
        .client
        .ensure_member_first_comer_for_tests(t, "b", "A@x.com ", "B2", Admission::Active)
        .await
        .unwrap();
    assert_eq!(
        (b.email.as_deref(), b.display_name.as_str()),
        (Some("a@x.com"), "B2")
    );
    assert_eq!(email_of(&s, t, "a").await, None);
    // A name-only change refreshes the name and leaves the emails alone.
    let a2 = s.client.ensure_member_first_comer_for_tests(t, "a", "", "A2", Admission::Active).await;
    assert_eq!(a2.unwrap().display_name, "A2");
    assert_eq!(email_of(&s, t, "b").await.as_deref(), Some("a@x.com"));
    // Activation takes the email too.
    let (invited, _) = s
        .client
        .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[]))
        .await
        .unwrap();
    let c = s
        .client
        .ensure_member_first_comer_for_tests(t, "c", "i@x.com", "C", Admission::Active)
        .await
        .unwrap();
    assert_eq!(c.email.as_deref(), Some("i@x.com"));
    assert_eq!(email_of(&s, t, "zi").await, None);
    let i = s
        .client
        .ensure_member_first_comer_for_tests(t, "zi", "i@x.com", "I", Admission::Active)
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
        .access_events(t, None, None, None, 100)
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
            .ensure_member_first_comer_for_tests(t, zid, &format!("{zid}@x.com"), "E", Admission::Active)
            .await
            .unwrap();
    }
    let hold = s.hold_tenant(t).await;
    let mut set = tokio::task::JoinSet::new();
    for zid in ["e1", "e2", "n1", "n2", "n3", "n4"] {
        let c = s.client.clone();
        set.spawn(async move { c.ensure_member_first_comer_for_tests(t, zid, "Same@x.com", "S", Admission::Active).await });
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let (i, invite) = s
        .client
        .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[(p, g.clone())]))
        .await
        .unwrap();
    s.client.revoke_invite(a.id, invite).await.unwrap();
    let (again, invite2) = s
        .client
        .create_invited_member(a.id, t, new_invite("zi", "i2@x.com", &[]))
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
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
        .create_invited_member(a.id, t, new_invite("m", "m@x.com", &[]))
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
            .create_invited_member(a.id, t, new_invite("a", "a3@x.com", &[]))
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
            .create_invited_member(a.id, t, new_invite("zx", "x@x.com", &[(p2, g)]))
            .await,
        Err(AccessWriteError::NotFound)
    ));
    assert!(s.client.member_by_zitadel(t, "zx").await.unwrap().is_none());
    assert!(s.client.member_projects(m.id).await.unwrap().is_empty());
    assert_eq!(
        s.client.grant_sources(m.id, p2, Some(d2)).await.unwrap(),
        GrantSources::default()
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let (i, invite) = s
        .client
        .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[]))
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
    let event = &s.client.access_events(t, None, None, None, 1).await.unwrap()[0];
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
        .ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active)
        .await
        .unwrap();
    let m = s
        .client
        .ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active)
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
        let event = &s.client.access_events(t, None, None, None, 1).await.unwrap()[0];
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = c.ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
    c.ensure_member_first_comer_for_tests(other, "o", "o@x.com", "O", Admission::Active).await.unwrap();

    assert_eq!(c.member(m.id).await.unwrap().map(|r| r.id), Some(m.id));
    assert_eq!(c.member(Uuid::new_v4()).await.unwrap(), None);
    assert_eq!(
        c.member_by_email(t, "  M@X.com ").await.unwrap().map(|r| r.id),
        Some(m.id)
    );
    assert_eq!(c.member_by_email(t, "o@x.com").await.unwrap(), None);
    assert_eq!(c.member_by_email(t, "").await.unwrap(), None);

    let (i, invite) = c
        .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[(p, grant(PlatformRole::Viewer, &[]))]))
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let stale = c.ensure_member_first_comer_for_tests(t, "s", "u@x.com", "S", Admission::Active).await.unwrap();
    let g = grant(PlatformRole::Developer, &["app"]);
    let events = s.event_count(t).await;
    let u = c
        .add_existing_user(a.id, t, "zu", " U@x.com", "U", &[(p, g.clone())], &[])
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
        c.add_existing_user(a.id, t, "zu", "u@x.com", "U", &[], &[]).await,
        Err(AccessWriteError::Conflict(_))
    ));
    // A project of another tenant is NotFound, with nothing written.
    let other = s.tenant().await;
    let foreign = s.project(other).await;
    let before = s.event_count(t).await;
    assert!(matches!(
        c.add_existing_user(a.id, t, "zv", "v@x.com", "V", &[(foreign, g.clone())], &[]).await,
        Err(AccessWriteError::NotFound)
    ));
    assert_eq!(s.event_count(t).await, before);
    assert!(c.member_by_zitadel(t, "zv").await.unwrap().is_none());

    // R29: a removed member never comes back active this way; it is re-invited with a code.
    c.set_tenant_admin(Some(a.id), u.id, true).await.unwrap();
    c.remove_member(a.id, u.id).await.unwrap();
    assert!(matches!(
        c.add_existing_user(a.id, t, "zu", "u@x.com", "U2", &[], &[]).await,
        Err(AccessWriteError::Conflict(_))
    ));
    let (back, _) = c
        .create_invited_member(a.id, t, new_invite("zu", "u@x.com", &[]))
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let m = c.ensure_member_first_comer_for_tests(t, "m", "m@x.com", "M", Admission::Active).await.unwrap();
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let (i, _) = c
        .create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[]))
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let g = grant(PlatformRole::Viewer, &[]);
    c.create_invited_member(a.id, t, new_invite("z1", "one@x.com", &[(p, g.clone())]))
        .await
        .unwrap();
    let single = &c.access_events(t, None, None, None, 1).await.unwrap()[0];
    assert_eq!((single.action.as_str(), single.project_id), ("invite_created", Some(p)));
    c.create_invited_member(a.id, t, new_invite("z2", "two@x.com", &[(p, g.clone()), (p2, g.clone())]))
        .await
        .unwrap();
    let u = c
        .add_existing_user(a.id, t, "z3", "three@x.com", "T", &[(p2, g.clone())], &[])
        .await
        .unwrap();
    c.put_project_grant(a.id, u.id, p, &g).await.unwrap();
    c.remove_member(a.id, u.id).await.unwrap();
    let actions = |events: Vec<chelabase_metadata::members::AccessEvent>| -> Vec<String> {
        events.into_iter().map(|e| e.action).collect()
    };
    assert_eq!(
        actions(c.access_events(t, Some(p), None, None, 50).await.unwrap()),
        vec!["member_removed", "grant_put", "invite_created", "invite_created"]
    );
    assert_eq!(
        actions(c.access_events(t, Some(p2), None, None, 50).await.unwrap()),
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    let (m, _) = c
        .create_invited_member(a.id, t, new_invite("zm", "m@x.com", &[]))
        .await
        .unwrap();
    let ev = &c.access_events(t, None, None, None, 1).await.unwrap()[0];
    assert_eq!(ev.actor_email.as_deref(), Some("a@x.com"));
    assert_eq!(ev.target_email.as_deref(), Some("m@x.com"));
    assert_eq!(ev.project_name, None, "an event with no project has no project name");

    c.put_project_grant(a.id, m.id, p, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    let ev = &c.access_events(t, None, None, None, 1).await.unwrap()[0];
    assert_eq!(ev.action, "grant_put");
    assert_eq!(ev.project_name.as_deref(), Some(format!("p-{p}").as_str()));

    // The target is removed, the project soft-deleted: both still resolve.
    c.ensure_member_first_comer_for_tests(t, "zm", "m@x.com", "M", Admission::Active).await.unwrap();
    c.remove_member(a.id, m.id).await.unwrap();
    sqlx::query("UPDATE projects SET status = 'deleted' WHERE id = $1")
        .bind(p)
        .execute(c.pool())
        .await
        .unwrap();
    let all = c.access_events(t, None, None, None, 50).await.unwrap();
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
    let all = c.access_events(t, Some(p), None, None, 50).await.unwrap();
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    assert!(a.tenant_admin);
    let granted = c.add_existing_user(a.id, t, "zg", "g@x.com", "G", &[(p, grant(PlatformRole::Viewer, &[]))], &[]).await.unwrap();
    let bare = c.add_existing_user(a.id, t, "zb", "b@x.com", "B", &[], &[]).await.unwrap();
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
    let (first, _) = c.create_invited_member(a.id, t, new_invite("zo1", "o1@x.com", &[])).await.unwrap();
    let (second, _) = c
        .create_invited_member(a.id, t, new_invite("zo2", "o2@x.com", &[(p, grant(PlatformRole::Viewer, &[]))]))
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "Alice", Admission::Active).await.unwrap();
    assert!(c.open_invite_for_user("zi").await.unwrap().is_none());
    let (_, invite) = c.create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[])).await.unwrap();
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

// #76: the stored expiry of a user's latest invite that isn't revoked or accepted, while its
// member is still invited, whether or not it has expired.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn invite_expiry_follows_the_unclosed_invite() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let c = &s.client;
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "Alice", Admission::Active).await.unwrap();
    assert_eq!(c.invite_expiry_for_user("zi").await.unwrap(), None);
    let (_, invite) = c.create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[])).await.unwrap();
    let stored = |sql: &'static str| async move {
        sqlx::query_scalar::<_, chrono::DateTime<Utc>>(sql).bind(invite).fetch_one(c.pool()).await.unwrap()
    };
    let open = stored("SELECT expires_at FROM invites WHERE id = $1").await;
    assert!(open > Utc::now());
    assert_eq!(c.invite_expiry_for_user("zi").await.unwrap(), Some((open, true)));
    // Expired: still answered, with its past expiry, not open (by the database's clock).
    let past = stored("UPDATE invites SET expires_at = now() - interval '1 minute' WHERE id = $1 RETURNING expires_at").await;
    assert_eq!(c.invite_expiry_for_user("zi").await.unwrap(), Some((past, false)));
    assert!(c.open_invite_for_user("zi").await.unwrap().is_none());
    // Another user's invite isn't this user's.
    assert_eq!(c.invite_expiry_for_user("zother").await.unwrap(), None);
    // Revoked: nothing.
    c.resend_invite(a.id, invite, week(), [9; 32]).await.unwrap();
    assert!(c.invite_expiry_for_user("zi").await.unwrap().unwrap().1);
    c.revoke_invite(a.id, invite).await.unwrap();
    assert_eq!(c.invite_expiry_for_user("zi").await.unwrap(), None);
    s.finish().await;
}

// An open invite in one tenant wins over a newer expired one in another: the user still has an
// invite to accept (#78's reactivation gate reads this).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn invite_expiry_prefers_an_open_invite_to_a_newer_expired_one() {
    let s = Scratch::new().await;
    let c = &s.client;
    let zid = format!("z-{}", uuid::Uuid::new_v4().simple());
    let (t1, t2) = (s.tenant().await, s.tenant().await);
    let a1 = c.ensure_member_first_comer_for_tests(t1, "a1", "a1@x.com", "A1", Admission::Active).await.unwrap();
    let a2 = c.ensure_member_first_comer_for_tests(t2, "a2", "a2@x.com", "A2", Admission::Active).await.unwrap();
    let (_, older) = c.create_invited_member(a1.id, t1, new_invite(&zid, "i@x.com", &[])).await.unwrap();
    let (_, newer) = c.create_invited_member(a2.id, t2, new_invite(&zid, "i@x.com", &[])).await.unwrap();
    // The newer one is the latest by creation time, whatever the clock's resolution.
    sqlx::query("UPDATE invites SET created_at = created_at + interval '1 minute' WHERE id = $1")
        .bind(newer)
        .execute(c.pool())
        .await
        .unwrap();
    let open: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT expires_at FROM invites WHERE id = $1")
        .bind(older)
        .fetch_one(c.pool())
        .await
        .unwrap();
    let past: chrono::DateTime<Utc> = sqlx::query_scalar(
        "UPDATE invites SET expires_at = now() - interval '1 minute' WHERE id = $1 RETURNING expires_at",
    )
    .bind(newer)
    .fetch_one(c.pool())
    .await
    .unwrap();
    assert_eq!(c.invite_expiry_for_user(&zid).await.unwrap(), Some((open, true)));
    // With both expired, the latest is answered.
    sqlx::query("UPDATE invites SET expires_at = now() - interval '2 minutes' WHERE id = $1")
        .bind(older)
        .execute(c.pool())
        .await
        .unwrap();
    assert_eq!(c.invite_expiry_for_user(&zid).await.unwrap(), Some((past, false)));
    s.finish().await;
}

// R36: the display name is nullable, never empty or padded, and a change writes one event.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn tenant_display_name_round_trip() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let c = &s.client;
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "Alice", Admission::Active).await.unwrap();
    let name = || async { c.tenant_overview(t).await.unwrap().unwrap().display_name };
    assert_eq!(name().await, None);
    c.set_tenant_display_name(a.id, t, Some("Acme Corp")).await.unwrap();
    assert_eq!(name().await.as_deref(), Some("Acme Corp"));
    // The invite description carries the display name, and the slug to fall back to.
    c.create_invited_member(a.id, t, new_invite("zi", "i@x.com", &[])).await.unwrap();
    let d = c.open_invite_for_user("zi").await.unwrap().unwrap();
    assert_eq!(d.tenant_display_name.as_deref(), Some("Acme Corp"));
    c.set_tenant_display_name(a.id, t, None).await.unwrap();
    assert_eq!(name().await, None);
    let d = c.open_invite_for_user("zi").await.unwrap().unwrap();
    assert_eq!((d.tenant_slug, d.tenant_display_name), (format!("t-{}", t.as_uuid()), None));
    let events: Vec<_> = c
        .access_events(t, None, None, None, 50)
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
    let a = c.ensure_member_first_comer_for_tests(t, "a", "a@x.com", "A", Admission::Active).await.unwrap();
    assert!(a.tenant_admin);
    let actions = |t: TenantId| async move {
        c.access_events(t, None, None, None, 100).await.unwrap().into_iter().rev().map(|e| e.action).collect::<Vec<_>>()
    };

    // Closed: no row, no event. Pending: a pending row and a `member_joined` event.
    let before = s.event_count(t).await;
    let closed = c.ensure_member_first_comer_for_tests(t, "c", "c@x.com", "C", Admission::Closed).await;
    assert!(matches!(closed, Err(chelabase_metadata::MetadataError::NotAdmitted(_))), "{closed:?}");
    assert!(c.member_by_zitadel(t, "c").await.unwrap().is_none());
    assert_eq!(s.event_count(t).await, before);
    let b = c.ensure_member_first_comer_for_tests(t, "b", "b@x.com", "B", Admission::Pending).await.unwrap();
    assert_eq!((b.status, b.tenant_admin), (MemberStatus::Pending, false));
    assert_eq!(actions(t).await.last().map(String::as_str), Some("member_joined"));
    // It stays pending whatever it asks for later, and is listed.
    for admission in [Admission::Active, Admission::Pending, Admission::Closed] {
        let again = c.ensure_member_first_comer_for_tests(t, "b", "b@x.com", "B", admission).await.unwrap();
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
    let listing = c.approve_member(a.id, b.id, &[(p, grant(PlatformRole::Developer, &[]))], &[]).await.unwrap();
    assert_eq!(listing.member.status, MemberStatus::Active);
    assert!(listing.has_access);
    assert_eq!(s.event_count(t).await, events + 1);
    let last = c.access_events(t, None, None, None, 1).await.unwrap().remove(0);
    assert_eq!((last.action.as_str(), last.project_id), ("member_approved", Some(p)));
    assert_eq!(
        c.grant_sources(b.id, p, None).await.unwrap().effective().map(|e| e.platform_role),
        Some(PlatformRole::Developer)
    );
    assert!(matches!(c.approve_member(a.id, b.id, &[], &[]).await, Err(AccessWriteError::Conflict(_))));

    // A grant naming the owner, or a project of another tenant, writes nothing.
    let d = c.ensure_member_first_comer_for_tests(t, "d", "d@x.com", "D", Admission::Pending).await.unwrap();
    let events = s.event_count(t).await;
    assert!(matches!(
        c.approve_member(a.id, d.id, &[(p, grant(PlatformRole::Developer, &["owner"]))], &[]).await,
        Err(AccessWriteError::Conflict(_))
    ));
    let other = s.tenant().await;
    let foreign = s.project(other).await;
    assert!(matches!(
        c.approve_member(a.id, d.id, &[(foreign, grant(PlatformRole::Viewer, &[]))], &[]).await,
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
    assert!(matches!(c.approve_member(a.id, d.id, &[], &[]).await, Err(AccessWriteError::MemberNotFound)));

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
    let p2 = c.ensure_member_first_comer_for_tests(t2, "pz", "p@x.com", "P", Admission::Pending).await.unwrap();
    assert_eq!((p2.status, p2.tenant_admin), (MemberStatus::Pending, false));
    assert!(c.active_tenant_admins(t2).await.unwrap().is_empty());
    let first = c.ensure_member_first_comer_for_tests(t2, "first", "f@x.com", "F", Admission::Closed).await.unwrap();
    assert_eq!((first.status, first.tenant_admin), (MemberStatus::Active, true));
    assert_eq!(c.member_by_zitadel(t2, "pz").await.unwrap().unwrap().status, MemberStatus::Pending);
    s.finish().await;
}

// M2: the bootstrap's `NAME_SQL` (`chelabase-init`'s `tenant_name.sql`) names a tenant
// once, trimmed and cut to the column's rules, and logs one event.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_names_the_tenant_once() {
    const NAME_SQL: &str = include_str!("../../chelabase-init/src/bootstrap/tenant_name.sql");
    let s = Scratch::new().await;
    // The bootstrap's own SQL (`chelabase_init::bootstrap::tenant`), already `$1`/`$2`.
    let sql = NAME_SQL.to_string();
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
    let admin = c.ensure_member_first_comer_for_tests(t, "admin", "admin@x.com", "Admin", Admission::Active).await.unwrap();
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
                Approve => name(c.approve_member(admin.id, id, &[], &[]).await),
                Decline => name(c.decline_member(admin.id, id).await),
                Promote => name(c.set_tenant_admin(Some(admin.id), id, true).await),
                Remove => name(c.remove_member(admin.id, id).await),
                Invite => name(c.create_invited_member(admin.id, t, new_invite(&zid, &email, &[])).await),
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

// ---- Members hub step 1 (T3): personal logins, suspension, revocation and the log ----------
//
// Spec Part 6 (`docs/superpowers/specs/2026-10-23-members-hub-design.md`), plan T3 and its seam
// rulings (S1, S2, S6, S7, S10, S12, S18). Each test runs on its own scratch database.

/// Two SCRAM verifiers as the proxy stores them (the metadata never checks their shape).
const V1: &str = "SCRAM-SHA-256$4096:c2FsdDE=$c3RvcmVkMQ==:c2VydmVyMQ==";
const V2: &str = "SCRAM-SHA-256$4096:c2FsdDI=$c3RvcmVkMg==:c2VydmVyMg==";

fn in_days(n: i64) -> chrono::DateTime<Utc> {
    Utc::now() + Duration::days(n)
}

/// A tenant with its first (claiming) tenant admin, one project and a ready database whose main
/// branch has an endpoint.
struct Hub {
    t: TenantId,
    admin: Uuid,
    p: Uuid,
    db: Uuid,
    /// Main's endpoint.
    ep: String,
}

impl Scratch {
    /// A member row in any status (status writes are under test elsewhere). A suspended row gets
    /// its `suspended_at` (`members_suspended_has_at`).
    async fn member_row(&self, t: TenantId, zid: &str, status: &str, tenant_admin: bool) -> Uuid {
        self.insert_member_row(
            "INSERT INTO members (id, tenant_id, zitadel_user_id, email, status, tenant_admin, suspended_at)
             VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $5 = 'suspended' THEN now() END)",
            t,
            zid,
            status,
            tenant_admin,
        )
        .await
    }

    /// [`Self::member_row`] on a database whose `members` has no D10 columns (the backfill test
    /// drops `suspended_at` and `suspended_by` first).
    async fn member_row_before_d10(&self, t: TenantId, zid: &str, status: &str) -> Uuid {
        self.insert_member_row(
            "INSERT INTO members (id, tenant_id, zitadel_user_id, email, status, tenant_admin)
             VALUES ($1, $2, $3, $4, $5, $6)",
            t,
            zid,
            status,
            false,
        )
        .await
    }

    async fn insert_member_row(
        &self,
        sql: &'static str,
        t: TenantId,
        zid: &str,
        status: &str,
        tenant_admin: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(sql)
        .bind(id)
        .bind(t.as_uuid())
        .bind(zid)
        .bind(format!("{zid}@x.com"))
        .bind(status)
        .bind(tenant_admin)
        .execute(self.client.pool())
        .await
        .unwrap();
        id
    }

    /// Makes a pending database ready with an endpoint on main; returns the endpoint.
    async fn ready(&self, db: Uuid) -> String {
        let ep = self.link(BranchId(db)).await;
        self.client.set_status(DatabaseId(db), DatabaseStatus::Provisioning).await.unwrap();
        assert!(self.client.set_branch_ready(BranchId(db), "v1:owner", None, None).await.unwrap());
        ep
    }

    /// A new endpoint (owner `postgres`) linked to `branch`.
    async fn link(&self, branch: BranchId) -> String {
        let ep = format!("ep-{}", &Uuid::new_v4().simple().to_string()[..12]);
        let tenant = Uuid::new_v4().simple().to_string();
        let timeline = Uuid::new_v4().simple().to_string();
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
        assert!(self.client.create_endpoint_for_branch(branch, &timeline, &new, false).await.unwrap());
        ep
    }

    /// A ready child of `db`'s main with its own endpoint: `(branch, endpoint)`.
    async fn child(&self, db: Uuid, name: &str) -> (Uuid, String) {
        let b = NewBranch {
            id: BranchId::new(),
            database_id: DatabaseId(db),
            name,
            parent_branch_id: BranchId(db),
            branched_at_time: None,
            branched_at_lsn: None,
        };
        assert_eq!(self.client.create_branch(&b).await.unwrap(), CreateBranchOutcome::Created);
        let ep = self.link(b.id).await;
        assert!(self
            .client
            .set_branch_status(b.id, &[BranchStatus::Pending], BranchStatus::Provisioning)
            .await
            .unwrap());
        assert!(self.client.set_branch_ready(b.id, "v1:branch", Some("0/1A2B3C"), None).await.unwrap());
        (b.id.as_uuid(), ep)
    }

    /// A ready database in `p`: `(database, main's endpoint)`.
    async fn ready_database(&self, t: TenantId, p: Uuid) -> (Uuid, String) {
        let db = self.database(t, p).await;
        let ep = self.ready(db).await;
        (db, ep)
    }

    async fn hub(&self) -> Hub {
        let t = self.tenant().await;
        let admin = self
            .client
            .ensure_member_first_comer_for_tests(t, "admin", "admin@x.com", "Admin", Admission::Active)
            .await
            .unwrap();
        assert!(admin.tenant_admin, "the first member claims the tenant");
        let p = self.project(t).await;
        let (db, ep) = self.ready_database(t, p).await;
        Hub { t, admin: admin.id, p, db, ep }
    }

    /// An active member (Zitadel user and display name `zid`, email `<zid>@x.com`).
    async fn active(&self, h: &Hub, zid: &str) -> Uuid {
        self.client
            .ensure_member_first_comer_for_tests(h.t, zid, &format!("{zid}@x.com"), zid, Admission::Active)
            .await
            .unwrap()
            .id
    }

    /// An active member with `role` on the hub's project.
    async fn granted(&self, h: &Hub, zid: &str, role: PlatformRole) -> Uuid {
        let id = self.active(h, zid).await;
        self.client.put_project_grant(h.admin, id, h.p, &grant(role, &[])).await.unwrap();
        id
    }

    /// `member`'s login `name` on `db` with [`V1`], valid 90 days.
    async fn login(&self, member: Uuid, db: Uuid, name: &str) -> PersonalLogin {
        self.client
            .create_personal_login(member, db, name, V1, in_days(90))
            .await
            .unwrap_or_else(|e| panic!("create {name}: {e:?}"))
    }

    /// The personal `neon_roles` rows named `name`: `(endpoint, verifier)` by endpoint.
    async fn personal_rows(&self, name: &str) -> Vec<(String, Option<String>)> {
        sqlx::query_as(
            "SELECT endpoint_id, scram_verifier FROM neon_roles
             WHERE name = $1 AND origin = 'personal' ORDER BY endpoint_id",
        )
        .bind(name)
        .fetch_all(self.client.pool())
        .await
        .unwrap()
    }

    /// The stored verifier of `member`'s record on `db` (`None` when revoked); panics without one.
    async fn record_verifier(&self, member: Uuid, db: Uuid) -> Option<String> {
        sqlx::query_scalar("SELECT scram_verifier FROM personal_logins WHERE member_id = $1 AND database_id = $2")
            .bind(member)
            .bind(db)
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }

    /// Every event of the tenant, oldest first.
    async fn events(&self, t: TenantId) -> Vec<AccessEvent> {
        let mut all = self.client.access_events(t, None, None, None, 1000).await.unwrap();
        all.reverse();
        all
    }

    async fn events_of(&self, t: TenantId, action: &str) -> Vec<AccessEvent> {
        self.events(t).await.into_iter().filter(|e| e.action == action).collect()
    }

    /// The SQL predicate (P2) for `member` on `db`.
    async fn sql_qualifies(&self, member: Uuid, db: Uuid) -> bool {
        sqlx::query_scalar("SELECT chelabase_personal_login_qualifies($1, $2)")
            .bind(member)
            .bind(db)
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }

    async fn status_of(&self, member: Uuid) -> MemberStatus {
        self.client.member(member).await.unwrap().unwrap().status
    }
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v
}

fn endpoint(branch: Uuid, ep: &str, role: &str) -> LoginEndpoint {
    LoginEndpoint { branch_id: branch, endpoint_id: ep.to_string(), role_name: role.to_string() }
}

/// The newest event's id (0 without one).
async fn last_event_id(s: &Scratch, t: TenantId) -> i64 {
    s.events(t).await.last().map_or(0, |e| e.id)
}

/// The actions logged after event `since`, oldest first.
async fn actions_since(s: &Scratch, t: TenantId, since: i64) -> Vec<String> {
    s.events(t).await.into_iter().filter(|e| e.id > since).map(|e| e.action).collect()
}

/// The logins of `member`, by database.
async fn logins_of(s: &Scratch, member: Uuid) -> Vec<PersonalLogin> {
    let mut all = s.client.member_personal_logins(member).await.unwrap();
    all.sort_by_key(|l| l.database_id);
    all
}

// P2 / S20: the SQL level function and its predicate agree with `access::qualifies` over
// `GrantSources::effective()` (a tenant admin judged with `tenant_admin_effective()`, as
// `authorize` does) on every small state: status × tenant admin × grant level × override.
// A deleted database never qualifies, nor does another tenant's member.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_level_function_agrees_with_qualifies_exhaustively() {
    #[derive(Clone, Copy, Debug)]
    enum Ov {
        None,
        Level(PlatformRole),
        /// Roles only: no level cap.
        RolesOnly,
    }
    const STATUSES: [&str; 5] = ["invited", "pending", "active", "suspended", "removed"];
    let levels = [None, Some(PlatformRole::Viewer), Some(PlatformRole::Developer), Some(PlatformRole::Admin)];
    let overrides = [
        Ov::None,
        Ov::Level(PlatformRole::Viewer),
        Ov::Level(PlatformRole::Developer),
        Ov::Level(PlatformRole::Admin),
        Ov::RolesOnly,
    ];
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let db = s.database(t, p).await;
    let gone = s.database(t, p).await;
    sqlx::query("UPDATE databases SET status = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(gone)
        .execute(s.client.pool())
        .await
        .unwrap();
    let pool = s.client.pool();
    let mut failures = Vec::new();
    let mut n = 0;
    for status in STATUSES {
        for tenant_admin in [false, true] {
            for g in levels {
                for o in overrides {
                    n += 1;
                    let id = s.member_row(t, &format!("z{n}"), status, tenant_admin).await;
                    let grant_row = g.map(|g| GrantRow::new(g, &["app".to_string()]));
                    if let Some(row) = &grant_row {
                        sqlx::query(
                            "INSERT INTO project_grants (member_id, project_id, platform_role, pg_roles)
                             VALUES ($1, $2, $3, $4)",
                        )
                        .bind(id)
                        .bind(p)
                        .bind(row.platform_role.as_str())
                        .bind(&row.pg_roles)
                        .execute(pool)
                        .await
                        .unwrap();
                    }
                    let override_row = match o {
                        Ov::None => None,
                        Ov::Level(l) => Some(OverrideRow { platform_role: Some(l), pg_roles: None }),
                        Ov::RolesOnly => Some(OverrideRow { platform_role: None, pg_roles: Some(vec!["app".into()]) }),
                    };
                    for target in [db, gone] {
                        if let Some(row) = &override_row {
                            sqlx::query(
                                "INSERT INTO database_grant_overrides (member_id, database_id, platform_role, pg_roles)
                                 VALUES ($1, $2, $3, $4)",
                            )
                            .bind(id)
                            .bind(target)
                            .bind(row.platform_role.map(|r| r.as_str()))
                            .bind(&row.pg_roles)
                            .execute(pool)
                            .await
                            .unwrap();
                        }
                    }
                    let active = status == "active";
                    let effective = if tenant_admin {
                        Some(tenant_admin_effective())
                    } else {
                        GrantSources::new(
                            grant_row.as_ref().map(GrantRow::to_grant),
                            Vec::new(),
                            override_row.as_ref().map(OverrideRow::to_narrowing),
                        )
                        .effective()
                    };
                    let want_q = qualifies(active, effective.as_ref());
                    let want_level = if active { effective.map(|e| e.platform_role.as_str().to_string()) } else { None };
                    let case = format!("{status} admin={tenant_admin} grant={g:?} override={o:?}");
                    let (level, q): (Option<String>, bool) = sqlx::query_as(
                        "SELECT chelabase_member_level($1, $2), chelabase_personal_login_qualifies($1, $2)",
                    )
                    .bind(id)
                    .bind(db)
                    .fetch_one(pool)
                    .await
                    .unwrap_or_else(|e| panic!("{case}: {e}"));
                    if (level.as_deref(), q) != (want_level.as_deref(), want_q) {
                        failures.push(format!("{case}: SQL ({level:?}, {q}), Rust ({want_level:?}, {want_q})"));
                    }
                    let (level, q): (Option<String>, bool) = sqlx::query_as(
                        "SELECT chelabase_member_level($1, $2), chelabase_personal_login_qualifies($1, $2)",
                    )
                    .bind(id)
                    .bind(gone)
                    .fetch_one(pool)
                    .await
                    .unwrap();
                    if level.is_some() || q {
                        failures.push(format!("{case}: a deleted database answered ({level:?}, {q})"));
                    }
                }
            }
        }
    }
    // Another tenant's tenant admin reaches nothing here.
    let other = s.tenant().await;
    let stranger = s.member_row(other, "stranger", "active", true).await;
    if s.sql_qualifies(stranger, db).await {
        failures.push("another tenant's admin qualifies".into());
    }
    assert!(failures.is_empty(), "{} of {n} cases:\n{}", failures.len(), failures.join("\n"));
    s.finish().await;
}

/// One write of the revocation model (P3); indexes pick among the pool of members.
#[derive(Debug, Clone)]
enum W {
    /// Member, level (0 viewer, 1 developer, 2 admin).
    PutGrant(u8, u8),
    DeleteGrant(u8),
    /// Member, database, level.
    PutOverride(u8, u8, u8),
    DeleteOverride(u8, u8),
    SetAdmin(u8, bool),
    Remove(u8),
    Suspend(u8),
    Reinstate(u8),
    RevokeNow(u8),
    /// A removed member invited again with a Developer grant, then signing in.
    Reinvite(u8),
    /// A new person signs up (pending) and is approved with a Developer grant.
    Approve,
    /// The member creates its login on a database (and the worker writes its proxy row).
    Login(u8, u8),
}

fn w() -> impl Strategy<Value = W> {
    let level = 0u8..3;
    prop_oneof![
        (any::<u8>(), level.clone()).prop_map(|(m, l)| W::PutGrant(m, l)),
        any::<u8>().prop_map(W::DeleteGrant),
        (any::<u8>(), 0u8..2, level).prop_map(|(m, d, l)| W::PutOverride(m, d, l)),
        (any::<u8>(), 0u8..2).prop_map(|(m, d)| W::DeleteOverride(m, d)),
        (any::<u8>(), any::<bool>()).prop_map(|(m, b)| W::SetAdmin(m, b)),
        any::<u8>().prop_map(W::Remove),
        any::<u8>().prop_map(W::Suspend),
        any::<u8>().prop_map(W::Reinstate),
        any::<u8>().prop_map(W::RevokeNow),
        any::<u8>().prop_map(W::Reinvite),
        Just(W::Approve),
        (any::<u8>(), 0u8..2).prop_map(|(m, d)| W::Login(m, d)),
        (any::<u8>(), 0u8..2).prop_map(|(m, d)| W::Login(m, d)),
    ]
}

fn level_of(l: u8) -> PlatformRole {
    [PlatformRole::Viewer, PlatformRole::Developer, PlatformRole::Admin][l as usize % 3]
}

/// The invariant after every write (P3, MH-9): no live login of a member who doesn't qualify
/// (judged in Rust from the stored grant, override, status and tenant-admin flag), no verifier
/// on any proxy row of a revoked login, and no revoked login live again (reinstate and
/// re-grants never bring one back).
async fn check_logins(
    s: &Scratch,
    t: TenantId,
    revoked_seen: &mut std::collections::BTreeSet<(Uuid, Uuid, chrono::DateTime<Utc>)>,
) -> Result<(), String> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT pl.member_id, pl.database_id, pl.created_at, pl.revoked_at IS NULL AS live,
                m.status, m.tenant_admin, g.platform_role AS g_role, g.pg_roles AS g_roles,
                o.member_id IS NOT NULL AS o_found, o.platform_role AS o_role, o.pg_roles AS o_roles
         FROM personal_logins pl
         JOIN members m ON m.id = pl.member_id
         JOIN databases d ON d.id = pl.database_id
         LEFT JOIN project_grants g ON g.member_id = pl.member_id AND g.project_id = d.project_id
         LEFT JOIN database_grant_overrides o ON o.member_id = pl.member_id AND o.database_id = pl.database_id
         WHERE m.tenant_id = $1",
    )
    .bind(t.as_uuid())
    .fetch_all(s.client.pool())
    .await
    .map_err(|e| e.to_string())?;
    for r in rows {
        let member: Uuid = r.get("member_id");
        let database: Uuid = r.get("database_id");
        let created: chrono::DateTime<Utc> = r.get("created_at");
        let live: bool = r.get("live");
        let status: String = r.get("status");
        let tenant_admin: bool = r.get("tenant_admin");
        let own = r.get::<Option<String>, _>("g_role").map(|role| {
            GrantRow { platform_role: PlatformRole::parse(&role).unwrap(), pg_roles: r.get("g_roles") }.to_grant()
        });
        let narrowing = r.get::<bool, _>("o_found").then(|| {
            OverrideRow {
                platform_role: r.get::<Option<String>, _>("o_role").map(|x| PlatformRole::parse(&x).unwrap()),
                pg_roles: r.get("o_roles"),
            }
            .to_narrowing()
        });
        let effective = if tenant_admin {
            Some(tenant_admin_effective())
        } else {
            GrantSources::new(own, Vec::new(), narrowing).effective()
        };
        let q = qualifies(status == "active", effective.as_ref());
        if live && !q {
            return Err(format!("a live login of {member} on {database} ({status}, admin={tenant_admin}) doesn't qualify"));
        }
        if live && revoked_seen.contains(&(member, database, created)) {
            return Err(format!("a revoked login of {member} on {database} is live again"));
        }
        if !live {
            revoked_seen.insert((member, database, created));
        }
    }
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM neon_roles r
         JOIN personal_logins pl ON pl.member_id = r.member_id AND pl.database_id = r.personal_database_id
                                AND pl.role_name = r.name
         JOIN members m ON m.id = pl.member_id
         WHERE m.tenant_id = $1 AND pl.revoked_at IS NOT NULL AND r.scram_verifier IS NOT NULL",
    )
    .bind(t.as_uuid())
    .fetch_one(s.client.pool())
    .await
    .map_err(|e| e.to_string())?;
    if leaked != 0 {
        return Err(format!("{leaked} proxy row(s) of revoked logins keep a verifier"));
    }
    Ok(())
}

/// Runs one generated history on a fresh tenant: three active Developers with a login on each
/// of two databases, then the writes in order, checking [`check_logins`] after each.
async fn run_revocation_model(s: &Scratch, steps: Vec<W>) -> Result<(), String> {
    let h = s.hub().await;
    let (db2, ep2) = s.ready_database(h.t, h.p).await;
    let dbs = [(h.db, h.ep.clone()), (db2, ep2)];
    let c = &s.client;
    let mut pool: Vec<(Uuid, String)> = Vec::new();
    let mut names = 0u32;
    let login = |member: Uuid, d: usize, n: u32| {
        let (db, ep) = dbs[d].clone();
        async move {
            let name = format!("l{n}");
            match c.create_personal_login(member, db, &name, V1, in_days(90)).await {
                Ok(_) => c
                    .upsert_personal_role_row(&ep, member, db, &name, V1)
                    .await
                    .map_err(|e| format!("upsert {name}: {e}")),
                Err(LoginWriteError::NotQualified | LoginWriteError::Exists) => Ok(()),
                Err(e) => Err(format!("create {name}: {e:?}")),
            }
        }
    };
    for i in 0..3 {
        let zid = format!("m{i}-{}", Uuid::new_v4().simple());
        let id = s.granted(&h, &zid, PlatformRole::Developer).await;
        pool.push((id, zid));
        for d in 0..2 {
            names += 1;
            login(id, d, names).await?;
        }
    }
    let mut revoked_seen = std::collections::BTreeSet::new();
    check_logins(s, h.t, &mut revoked_seen).await.map_err(|e| format!("setup: {e}"))?;
    for (n, step) in steps.into_iter().enumerate() {
        let pick = |i: u8| pool[i as usize % pool.len()].clone();
        let result: Result<(), AccessWriteError> = match step.clone() {
            W::PutGrant(i, l) => c.put_project_grant(h.admin, pick(i).0, h.p, &grant(level_of(l), &[])).await.map(|_| ()),
            W::DeleteGrant(i) => c.delete_project_grant(h.admin, pick(i).0, h.p).await,
            W::PutOverride(i, d, l) => {
                let o = OverrideRow { platform_role: Some(level_of(l)), pg_roles: None };
                c.put_override(h.admin, pick(i).0, dbs[d as usize].0, &o).await.map(|_| ())
            }
            W::DeleteOverride(i, d) => c.delete_override(h.admin, pick(i).0, dbs[d as usize].0).await,
            W::SetAdmin(i, on) => c.set_tenant_admin(Some(h.admin), pick(i).0, on).await,
            W::Remove(i) => c.remove_member(h.admin, pick(i).0).await.map(|_| ()),
            W::Suspend(i) => c.suspend_member(h.admin, pick(i).0).await.map(|_| ()),
            W::Reinstate(i) => c.reinstate_member(h.admin, pick(i).0).await.map(|_| ()),
            W::RevokeNow(i) => c.revoke_postgres_access(h.admin, pick(i).0).await.map(|_| ()),
            W::Reinvite(i) => {
                let (_, zid) = pick(i);
                let email = format!("{zid}@x.com");
                let grants = [(h.p, grant(PlatformRole::Developer, &[]))];
                match c.create_invited_member(h.admin, h.t, new_invite(&zid, &email, &grants)).await {
                    Ok(_) => c
                        .ensure_member_first_comer_for_tests(h.t, &zid, &email, "n", Admission::Active)
                        .await
                        .map(|_| ())
                        .map_err(|e| AccessWriteError::Conflict(format!("ensure_member: {e}"))),
                    Err(e) => Err(e),
                }
            }
            W::Approve => {
                let zid = format!("a-{}", Uuid::new_v4().simple());
                let email = format!("{zid}@x.com");
                let m = c
                    .ensure_member_first_comer_for_tests(h.t, &zid, &email, "n", Admission::Pending)
                    .await
                    .map_err(|e| format!("step {n}: sign-up: {e}"))?;
                let grants = [(h.p, grant(PlatformRole::Developer, &[]))];
                let approved = c.approve_member(h.admin, m.id, &grants, &[]).await.map(|_| ());
                pool.push((m.id, zid));
                approved
            }
            W::Login(i, d) => {
                names += 1;
                login(pick(i).0, d as usize, names).await.map_err(|e| format!("step {n}: {e}"))?;
                Ok(())
            }
        };
        match result {
            Ok(())
            | Err(AccessWriteError::Conflict(_))
            | Err(AccessWriteError::NotFound)
            | Err(AccessWriteError::MemberNotFound)
            | Err(AccessWriteError::LastTenantAdmin) => {}
            Err(e) => return Err(format!("step {n} {step:?}: {e:?}")),
        }
        check_logins(s, h.t, &mut revoked_seen)
            .await
            .map_err(|e| format!("after step {n} {step:?}: {e}"))?;
    }
    Ok(())
}

// P3 (invariant): whatever sequence of access writes runs (grant and override puts and
// deletes, tenant admin set and cleared, remove, suspend, reinstate, revoke-now, invites with
// grants, approvals, new logins), no live login of a non-qualifying member survives any write,
// and no proxy row of a revoked login holds a verifier.
#[test]
#[ignore = "needs the workbench stack's metadata Postgres"]
fn every_access_write_revokes_unqualified_logins() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let s = rt.block_on(Scratch::new());
    let mut runner = TestRunner::new(Config {
        cases: 48,
        failure_persistence: None,
        ..Config::default()
    });
    let steps = prop::collection::vec(w(), 1..14);
    let result = runner.run(&steps, |steps| {
        rt.block_on(run_revocation_model(&s, steps)).map_err(TestCaseError::fail)
    });
    rt.block_on(s.finish());
    result.unwrap();
}

// D3: the name is unique across every branch of the database and against every login of it,
// revoked ones included; another database's roles don't count; a refusal writes nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn create_refuses_a_name_taken_on_any_branch_or_by_another_login() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'app_role', 'x', 'sql')")
        .bind(&child_ep)
        .execute(c.pool())
        .await
        .unwrap();
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    let carol = s.granted(&h, "carol", PlatformRole::Developer).await;
    // A role on a child branch only, and the owner on main.
    let on_child = c.create_personal_login(alice, h.db, "app_role", V1, in_days(90)).await;
    assert!(matches!(on_child, Err(LoginWriteError::NameTaken)), "{on_child:?}");
    let owner = c.create_personal_login(alice, h.db, "postgres", V1, in_days(90)).await;
    assert!(matches!(owner, Err(LoginWriteError::NameTaken)), "{owner:?}");
    // Another member's login.
    s.login(bob, h.db, "bob_login").await;
    let theirs = c.create_personal_login(alice, h.db, "bob_login", V1, in_days(90)).await;
    assert!(matches!(theirs, Err(LoginWriteError::NameTaken)), "{theirs:?}");
    // A revoked login still holds its name.
    c.revoke_postgres_access(h.admin, bob).await.unwrap();
    let revoked = c.create_personal_login(carol, h.db, "bob_login", V1, in_days(90)).await;
    assert!(matches!(revoked, Err(LoginWriteError::NameTaken)), "{revoked:?}");
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM personal_logins WHERE member_id = ANY($1)")
        .bind(vec![alice, carol])
        .fetch_one(c.pool())
        .await
        .unwrap();
    assert_eq!(none, 0, "a refused create writes nothing");
    // Another database's roles don't count.
    let (_db2, ep2) = s.ready_database(h.t, h.p).await;
    sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'elsewhere', 'x', 'sql')")
        .bind(&ep2)
        .execute(c.pool())
        .await
        .unwrap();
    let made = s.login(alice, h.db, "elsewhere").await;
    assert_eq!((made.role_name.as_str(), made.status, made.database_id), ("elsewhere", LoginStatus::Active, h.db));
    assert_eq!(made.project_id, h.p);
    assert!(c.is_personal_login_name(h.db, "elsewhere").await.unwrap());
    s.finish().await;
}

// P6 / S10: a live login refuses a second create (`Exists`, whatever the name); a revoked or
// expired one is replaced (DELETE + INSERT), the same name included while the revoked login's
// proxy rows remain: they go with it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn create_replaces_a_revoked_record_and_refuses_a_live_one() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let first = s.login(alice, h.db, "alice").await;
    assert_eq!((first.status, first.revoked_at, first.revoked_reason), (LoginStatus::Active, None, None));
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&child_ep, alice, h.db, "alice", V1).await.unwrap();
    for name in ["alice", "alice2"] {
        let again = c.create_personal_login(alice, h.db, name, V2, in_days(90)).await;
        assert!(matches!(again, Err(LoginWriteError::Exists)), "{name}: {again:?}");
    }
    assert_eq!(s.record_verifier(alice, h.db).await.as_deref(), Some(V1), "a refused create changes nothing");

    c.revoke_postgres_access(h.admin, alice).await.unwrap();
    assert_eq!(
        s.personal_rows("alice").await,
        sorted(vec![(h.ep.clone(), None), (child_ep.clone(), None)]),
        "the revoked login's rows stay (no verifier) until the worker drops the role"
    );
    // S10: the same name, while those rows remain.
    let replaced = c
        .create_personal_login(alice, h.db, "alice", V2, in_days(90))
        .await
        .expect("a revoked login is replaced, its own rows don't block its name");
    assert_eq!((replaced.status, replaced.revoked_at), (LoginStatus::Active, None));
    assert_eq!(s.personal_rows("alice").await, vec![], "the old record's rows went with it");
    assert_eq!(s.record_verifier(alice, h.db).await.as_deref(), Some(V2));

    // An expired login is replaced too, under a new name.
    sqlx::query("UPDATE personal_logins SET valid_until = now() - interval '1 second' WHERE member_id = $1")
        .bind(alice)
        .execute(c.pool())
        .await
        .unwrap();
    let renamed = s.login(alice, h.db, "alice_new").await;
    assert_eq!((renamed.role_name.as_str(), renamed.status), ("alice_new", LoginStatus::Active));
    let records: i64 = sqlx::query_scalar("SELECT count(*) FROM personal_logins WHERE member_id = $1")
        .bind(alice)
        .fetch_one(c.pool())
        .await
        .unwrap();
    assert_eq!(records, 1, "one record per (member, database)");
    assert!(!c.is_personal_login_name(h.db, "alice").await.unwrap(), "the old name is free again");
    assert_eq!(s.events_of(h.t, "personal_login_created").await.len(), 3);
    s.finish().await;
}

// P8: a reset replaces the record's verifier and every proxy row's at once, renews
// `valid_until`, and returns where the rows are. No live login: `NotFound`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn reset_sets_every_proxy_row_to_the_new_verifier() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&child_ep, alice, h.db, "alice", V1).await.unwrap();
    let until = in_days(120);
    let (login, eps) = c.reset_personal_login(alice, h.db, V2, until).await.unwrap();
    assert!((login.valid_until - until).num_milliseconds().abs() < 1, "{} vs {until}", login.valid_until);
    assert_eq!(login.status, LoginStatus::Active);
    let want = sorted(vec![endpoint(h.db, &h.ep, "alice"), endpoint(child, &child_ep, "alice")]);
    assert_eq!(sorted(eps), want);
    assert_eq!(sorted(c.member_login_endpoints(alice).await.unwrap()), want);
    assert_eq!(s.record_verifier(alice, h.db).await.as_deref(), Some(V2));
    assert_eq!(
        s.personal_rows("alice").await,
        sorted(vec![(h.ep.clone(), Some(V2.to_string())), (child_ep.clone(), Some(V2.to_string()))])
    );
    assert_eq!(c.routable_role_secret(&child_ep, "alice").await.unwrap(), Some(Some(V2.to_string())));
    // No login, or a revoked one: nothing to reset.
    let missing = c.reset_personal_login(bob, h.db, V2, until).await;
    assert!(matches!(missing, Err(LoginWriteError::NotFound)), "{missing:?}");
    c.revoke_postgres_access(h.admin, alice).await.unwrap();
    let revoked = c.reset_personal_login(alice, h.db, V1, until).await;
    assert!(matches!(revoked, Err(LoginWriteError::NotFound)), "{revoked:?}");
    assert_eq!(s.record_verifier(alice, h.db).await, None, "a reset never revives a revoked login");
    assert_eq!(s.events_of(h.t, "personal_login_reset").await.len(), 1);
    s.finish().await;
}

// A delete removes the record, its proxy rows go with it (FK cascade), and it returns where
// they were, for the proxy cache. Deleting again is `NotFound`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn delete_cascades_the_proxy_rows_and_returns_their_endpoints() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&child_ep, alice, h.db, "alice", V1).await.unwrap();
    let eps = c.delete_personal_login(h.admin, alice, h.db).await.unwrap();
    assert_eq!(sorted(eps), sorted(vec![endpoint(h.db, &h.ep, "alice"), endpoint(child, &child_ep, "alice")]));
    assert_eq!(s.personal_rows("alice").await, vec![]);
    assert_eq!(c.routable_role_secret(&h.ep, "alice").await.unwrap(), Some(None));
    assert!(logins_of(&s, alice).await.is_empty());
    assert!(!c.is_personal_login_name(h.db, "alice").await.unwrap());
    let again = c.delete_personal_login(h.admin, alice, h.db).await;
    assert!(matches!(again, Err(LoginWriteError::NotFound)), "{again:?}");
    let deleted = s.events_of(h.t, "personal_login_deleted").await;
    assert_eq!(deleted.len(), 1);
    assert_eq!((deleted[0].actor_member_id, deleted[0].target_member_id), (Some(h.admin), Some(alice)));
    s.finish().await;
}

// MH-9: the worker's row write stores a verifier only for a live record, in the same
// statement: a revoked login never gets one back, on a branch it was on or a new one.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn revoked_record_never_gets_a_verifier_back() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    assert_eq!(s.personal_rows("alice").await, vec![(h.ep.clone(), Some(V1.to_string()))]);
    c.revoke_postgres_access(h.admin, alice).await.unwrap();
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&child_ep, alice, h.db, "alice", V1).await.unwrap();
    for (ep, verifier) in s.personal_rows("alice").await {
        assert_eq!(verifier, None, "{ep}: a revoked login's row got a verifier");
    }
    assert_eq!(s.record_verifier(alice, h.db).await, None);
    assert_eq!(c.routable_role_secret(&child_ep, "alice").await.unwrap().flatten(), None);
    s.finish().await;
}

/// Waits (10 s at most, no sleep) until `task` is finished or a session of the scratch database
/// that isn't in `known` waits on a lock; returns every waiting session's pid. The interleavings
/// below use it to know a statement has reached its lock (or needed none) before going on.
async fn blocked_or_done<T>(s: &Scratch, known: &[i32], task: &tokio::task::JoinHandle<T>) -> Vec<i32> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: Vec<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock' ORDER BY pid",
        )
        .fetch_all(s.client.pool())
        .await
        .unwrap();
        if task.is_finished() || waiting.iter().any(|pid| !known.contains(pid)) {
            return waiting;
        }
        assert!(std::time::Instant::now() < deadline, "neither blocked nor done: {waiting:?}");
        tokio::task::yield_now().await;
    }
}

/// A transaction holding `access_events` in SHARE mode: a writer that logs an event stops at
/// its event, after its other writes, until this commits.
async fn hold_the_log(s: &Scratch) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = s.client.pool().begin().await.unwrap();
    sqlx::query("LOCK TABLE access_events IN SHARE MODE").execute(&mut *tx).await.unwrap();
    tx
}

// I1 (P8): the proxy answers a personal row with its record's verifier, never the row's own:
// after a reset, a row still holding an older verifier (or none, the outcome of a worker write
// racing the reset) answers the new one on every branch at once.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_personal_row_answers_its_records_verifier() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&child_ep, alice, h.db, "alice", V1).await.unwrap();
    c.reset_personal_login(alice, h.db, V2, in_days(90)).await.unwrap();
    // The race's outcome: the child's row stale, main's without a verifier.
    sqlx::query("UPDATE neon_roles SET scram_verifier = $2 WHERE endpoint_id = $1 AND name = 'alice'")
        .bind(&child_ep)
        .bind(V1)
        .execute(c.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE neon_roles SET scram_verifier = NULL WHERE endpoint_id = $1 AND name = 'alice'")
        .bind(&h.ep)
        .execute(c.pool())
        .await
        .unwrap();
    assert_eq!(
        c.routable_role_secret(&child_ep, "alice").await.unwrap(),
        Some(Some(V2.to_string())),
        "a stale row answers the old password"
    );
    assert_eq!(c.routable_role_secret(&h.ep, "alice").await.unwrap(), Some(Some(V2.to_string())));
    assert_eq!(c.neon_role_secret(&child_ep, "alice").await.unwrap(), Some(Some(V2.to_string())));
    s.finish().await;
}

// I1 (P8): the worker's row write racing a reset. The reset has written V2 into the record and
// its rows and is held before its commit (at its event); the worker's writes start then, for
// main's existing row and for the child's new one, read V1 if they don't wait for the record,
// and finish after the reset commits. Every row and every proxy answer ends at V2.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_worker_write_racing_a_reset_ends_at_the_new_verifier() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    let log = hold_the_log(&s).await;
    let reset = tokio::spawn({
        let c = c.clone();
        async move { c.reset_personal_login(alice, h.db, V2, in_days(90)).await }
    });
    let known = blocked_or_done(&s, &[], &reset).await;
    assert!(!reset.is_finished(), "the reset is held at its event");
    let on_main = tokio::spawn({
        let (c, ep) = (c.clone(), h.ep.clone());
        async move { c.upsert_personal_role_row(&ep, alice, h.db, "alice", V1).await }
    });
    let known = blocked_or_done(&s, &known, &on_main).await;
    let on_child = tokio::spawn({
        let (c, ep) = (c.clone(), child_ep.clone());
        async move { c.upsert_personal_role_row(&ep, alice, h.db, "alice", V1).await }
    });
    blocked_or_done(&s, &known, &on_child).await;
    log.commit().await.unwrap();
    reset.await.unwrap().unwrap();
    on_main.await.unwrap().unwrap();
    on_child.await.unwrap().unwrap();
    assert_eq!(
        s.personal_rows("alice").await,
        sorted(vec![(h.ep.clone(), Some(V2.to_string())), (child_ep.clone(), Some(V2.to_string()))]),
        "a worker write put the old verifier back after the reset"
    );
    assert_eq!(s.record_verifier(alice, h.db).await.as_deref(), Some(V2));
    for ep in [&h.ep, &child_ep] {
        assert_eq!(c.routable_role_secret(ep, "alice").await.unwrap(), Some(Some(V2.to_string())), "{ep}");
    }
    s.finish().await;
}

// I1 (MH-9): the worker's row write racing a revocation. The revocation has revoked the record
// and cleared its rows and is held before its commit (at its event); the worker's writes start
// then, for main's existing row and for the child's new one. No row of the revoked record ends
// with a verifier, and the proxy answers none.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_worker_write_racing_a_revocation_leaves_no_verifier() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    let log = hold_the_log(&s).await;
    let revoke = tokio::spawn({
        let c = c.clone();
        async move { c.revoke_postgres_access(h.admin, alice).await }
    });
    let known = blocked_or_done(&s, &[], &revoke).await;
    assert!(!revoke.is_finished(), "the revocation is held at its event");
    let on_main = tokio::spawn({
        let (c, ep) = (c.clone(), h.ep.clone());
        async move { c.upsert_personal_role_row(&ep, alice, h.db, "alice", V1).await }
    });
    let known = blocked_or_done(&s, &known, &on_main).await;
    let on_child = tokio::spawn({
        let (c, ep) = (c.clone(), child_ep.clone());
        async move { c.upsert_personal_role_row(&ep, alice, h.db, "alice", V1).await }
    });
    blocked_or_done(&s, &known, &on_child).await;
    log.commit().await.unwrap();
    revoke.await.unwrap().unwrap();
    on_main.await.unwrap().unwrap();
    on_child.await.unwrap().unwrap();
    for (ep, verifier) in s.personal_rows("alice").await {
        assert_eq!(verifier, None, "{ep}: a revoked login's row got a verifier");
    }
    assert_eq!(s.record_verifier(alice, h.db).await, None);
    for ep in [&h.ep, &child_ep] {
        assert_eq!(c.routable_role_secret(ep, "alice").await.unwrap().flatten(), None, "{ep}");
    }
    s.finish().await;
}

// I1 (MH-9), the other order: the worker's write has read the live record and is about to
// insert the child's new row (held there by a test trigger) when a revocation starts; the
// revocation must still clear that row once it lands, whatever its own statements saw first.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_revocation_clears_a_row_the_worker_was_inserting() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    // A personal row's insert waits for advisory lock 4242 (the scratch database only).
    sqlx::raw_sql(
        "CREATE FUNCTION test_hold_personal_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock_shared(4242); RETURN NEW; END $$;
         CREATE TRIGGER test_hold_personal_insert BEFORE INSERT ON neon_roles FOR EACH ROW
           WHEN (NEW.origin = 'personal') EXECUTE FUNCTION test_hold_personal_insert();",
    )
    .execute(c.pool())
    .await
    .unwrap();
    let mut gate = c.pool().begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(4242)").execute(&mut *gate).await.unwrap();
    let on_child = tokio::spawn({
        let (c, ep) = (c.clone(), child_ep.clone());
        async move { c.upsert_personal_role_row(&ep, alice, h.db, "alice", V1).await }
    });
    let known = blocked_or_done(&s, &[], &on_child).await;
    assert!(!on_child.is_finished(), "the worker's insert is held by the trigger");
    let revoke = tokio::spawn({
        let c = c.clone();
        async move { c.revoke_postgres_access(h.admin, alice).await }
    });
    blocked_or_done(&s, &known, &revoke).await;
    gate.commit().await.unwrap();
    on_child.await.unwrap().unwrap();
    revoke.await.unwrap().unwrap();
    for (ep, verifier) in s.personal_rows("alice").await {
        assert_eq!(verifier, None, "{ep}: a revoked login's row kept a verifier");
    }
    assert_eq!(s.record_verifier(alice, h.db).await, None);
    assert_eq!(c.routable_role_secret(&child_ep, "alice").await.unwrap().flatten(), None);
    s.finish().await;
}

// D5: suspend revokes every live login (`suspended`) and keeps the grants; reinstate makes the
// member active again with the logins still revoked (the member makes new ones). Suspend and
// reinstate refuse members that aren't active or suspended.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn suspend_revokes_and_reinstate_keeps_them_revoked() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (db2, ep2) = s.ready_database(h.t, h.p).await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    s.login(alice, db2, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&ep2, alice, db2, "alice", V1).await.unwrap();

    let touched = c.suspend_member(h.admin, alice).await.unwrap();
    assert!(touched.contains(&h.p), "{touched:?}");
    assert_eq!(s.status_of(alice).await, MemberStatus::Suspended);
    for l in logins_of(&s, alice).await {
        assert_eq!((l.status, l.revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::Suspended)), "{l:?}");
        assert!(l.revoked_at.is_some());
    }
    assert!(s.personal_rows("alice").await.iter().all(|(_, v)| v.is_none()));
    assert_eq!(c.member_projects(alice).await.unwrap().len(), 1, "grants are kept");
    assert!(!s.sql_qualifies(alice, h.db).await);
    let suspended = s.events_of(h.t, "member_suspended").await;
    assert_eq!(suspended.len(), 1);
    assert_eq!((suspended[0].actor_member_id, suspended[0].target_member_id), (Some(h.admin), Some(alice)));

    c.reinstate_member(h.admin, alice).await.unwrap();
    assert_eq!(s.status_of(alice).await, MemberStatus::Active);
    assert!(logins_of(&s, alice).await.iter().all(|l| l.status == LoginStatus::Revoked), "reinstate recreates nothing");
    assert!(s.personal_rows("alice").await.iter().all(|(_, v)| v.is_none()));
    assert!(s.sql_qualifies(alice, h.db).await);
    assert_eq!(s.events_of(h.t, "member_reinstated").await.len(), 1);
    // The member makes a new one.
    assert_eq!(s.login(alice, h.db, "alice").await.status, LoginStatus::Active);

    // Neither applies to an invited, pending or removed member.
    let (invited, _) = c
        .create_invited_member(h.admin, h.t, new_invite("inv", "inv@x.com", &[]))
        .await
        .unwrap();
    let pending = c.ensure_member_first_comer_for_tests(h.t, "pend", "pend@x.com", "P", Admission::Pending).await.unwrap();
    assert_eq!(pending.status, MemberStatus::Pending);
    let removed = s.granted(&h, "gone", PlatformRole::Developer).await;
    c.remove_member(h.admin, removed).await.unwrap();
    for (label, id) in [("invited", invited.id), ("pending", pending.id), ("removed", removed)] {
        let suspend = c.suspend_member(h.admin, id).await;
        assert!(matches!(suspend, Err(AccessWriteError::Conflict(_))), "suspend {label}: {suspend:?}");
        let reinstate = c.reinstate_member(h.admin, id).await;
        assert!(matches!(reinstate, Err(AccessWriteError::Conflict(_))), "reinstate {label}: {reinstate:?}");
    }
    s.finish().await;
}

// P15: the last active tenant admin can't be suspended; a suspended admin doesn't count as
// active. A refusal writes nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_last_active_tenant_admin_cannot_be_suspended() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let dev = s.granted(&h, "dev", PlatformRole::Developer).await;
    let before = s.event_count(h.t).await;
    let refused = c.suspend_member(dev, h.admin).await;
    assert!(matches!(refused, Err(AccessWriteError::LastTenantAdmin)), "{refused:?}");
    assert_eq!(s.status_of(h.admin).await, MemberStatus::Active);
    assert_eq!(s.event_count(h.t).await, before, "nothing written");
    let second = s.active(&h, "second").await;
    c.set_tenant_admin(Some(h.admin), second, true).await.unwrap();
    c.suspend_member(h.admin, second).await.unwrap();
    assert_eq!(s.status_of(second).await, MemberStatus::Suspended);
    let last = c.suspend_member(dev, h.admin).await;
    assert!(matches!(last, Err(AccessWriteError::LastTenantAdmin)), "the suspended admin doesn't count: {last:?}");
    assert_eq!(c.active_tenant_admins(h.t).await.unwrap().len(), 1);
    s.finish().await;
}

// S7: suspending a suspended member and reinstating an active one succeed and write nothing
// (a retry after a failed Zitadel lock or unlock redoes only that step).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn suspend_and_reinstate_are_idempotent() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    let first = sorted(c.suspend_member(h.admin, alice).await.unwrap());
    let written = s.event_count(h.t).await;
    let again = sorted(c.suspend_member(h.admin, alice).await.unwrap());
    assert_eq!(again, first, "the retry returns the same touched projects");
    assert_eq!(s.event_count(h.t).await, written, "and writes nothing");
    assert_eq!(s.status_of(alice).await, MemberStatus::Suspended);
    c.reinstate_member(h.admin, alice).await.unwrap();
    let written = s.event_count(h.t).await;
    c.reinstate_member(h.admin, alice).await.unwrap();
    assert_eq!(s.event_count(h.t).await, written);
    assert_eq!(s.status_of(alice).await, MemberStatus::Active);
    s.finish().await;
}

/// `(suspended_at, suspended_by)` as the `members` row stores them (D10).
async fn suspension_of(s: &Scratch, member: Uuid) -> (Option<chrono::DateTime<Utc>>, Option<Uuid>) {
    sqlx::query_as("SELECT suspended_at, suspended_by FROM members WHERE id = $1")
        .bind(member)
        .fetch_one(s.client.pool())
        .await
        .unwrap_or_else(|e| panic!("reading the suspension columns: {e}"))
}

// D10: suspend writes `suspended_at` and `suspended_by` (the acting member) in its transaction,
// reinstate clears both, a second suspension records its own actor; the record carries them.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn suspend_records_when_and_by_whom_and_reinstate_clears() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let second = s.active(&h, "second").await;
    c.set_tenant_admin(Some(h.admin), second, true).await.unwrap();
    assert_eq!(suspension_of(&s, alice).await, (None, None), "an active member has neither");

    let before = Utc::now() - Duration::seconds(5);
    c.suspend_member(h.admin, alice).await.unwrap();
    let after = Utc::now() + Duration::seconds(5);
    let (at, by) = suspension_of(&s, alice).await;
    let at = at.expect("suspended_at is set");
    assert!(before <= at && at <= after, "{at} not within {before}..{after}");
    assert_eq!(by, Some(h.admin));
    let record = c.member(alice).await.unwrap().unwrap();
    assert_eq!((record.suspended_at, record.suspended_by), (Some(at), Some(h.admin)), "the record carries them");

    c.reinstate_member(second, alice).await.unwrap();
    assert_eq!(suspension_of(&s, alice).await, (None, None), "reinstate clears both");
    let record = c.member(alice).await.unwrap().unwrap();
    assert_eq!((record.suspended_at, record.suspended_by), (None, None));

    c.suspend_member(second, alice).await.unwrap();
    let (at2, by2) = suspension_of(&s, alice).await;
    assert_eq!(by2, Some(second), "a new suspension records its own actor");
    assert!(at2.unwrap() >= at);
    s.finish().await;
}

// D10 with S7: a retried suspend (even by another admin) keeps the first values and writes
// nothing; a retried reinstate leaves both cleared. Removing or offboarding an active member
// doesn't write them.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_retried_suspend_keeps_the_first_values_and_other_writes_leave_them_null() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let second = s.active(&h, "second").await;
    c.set_tenant_admin(Some(h.admin), second, true).await.unwrap();
    c.suspend_member(h.admin, alice).await.unwrap();
    let first = suspension_of(&s, alice).await;
    assert_eq!(first.1, Some(h.admin));
    let written = s.event_count(h.t).await;
    c.suspend_member(second, alice).await.unwrap();
    c.suspend_member(h.admin, alice).await.unwrap();
    assert_eq!(suspension_of(&s, alice).await, first, "the original values stay");
    assert_eq!(s.event_count(h.t).await, written);
    c.reinstate_member(h.admin, alice).await.unwrap();
    c.reinstate_member(h.admin, alice).await.unwrap();
    assert_eq!(suspension_of(&s, alice).await, (None, None));

    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    c.revoke_logins_for_zitadel_user(h.t, "bob").await.unwrap();
    assert_eq!(suspension_of(&s, bob).await, (None, None), "offboarding doesn't touch them");
    c.remove_member(h.admin, bob).await.unwrap();
    assert_eq!(suspension_of(&s, bob).await, (None, None), "nor does removal of an active member");
    s.finish().await;
}

// D10: the CHECKs. `suspended_by` needs `suspended_at`; a suspended member has `suspended_at`;
// `suspended_by` may be null there (the backfill, a deleted actor is not possible: FK).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_suspension_columns_are_checked() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let active = s.member_row(h.t, "act", "active", false).await;
    let write = |sql: &'static str, id: Uuid, by: Option<Uuid>| {
        let pool = s.client.pool().clone();
        async move { sqlx::query(sql).bind(id).bind(by).execute(&pool).await }
    };
    // Cannot name an actor without a time.
    let r = write("UPDATE members SET suspended_by = $2 WHERE id = $1", active, Some(h.admin)).await;
    assert!(r.is_err(), "suspended_by without suspended_at is refused");
    // Cannot be suspended without a time.
    let r = write("UPDATE members SET status = 'suspended' WHERE id = $1 AND $2::uuid IS NULL", active, None).await;
    assert!(r.is_err(), "a suspended member without suspended_at is refused");
    // A suspended member with a time and no actor is allowed (backfilled rows); with the FK
    // enforced for an actor.
    let r = write(
        "UPDATE members SET status = 'suspended', suspended_at = now() WHERE id = $1 AND $2::uuid IS NULL",
        active,
        None,
    )
    .await;
    assert!(r.is_ok(), "{r:?}");
    let r = write("UPDATE members SET suspended_by = $2 WHERE id = $1", active, Some(Uuid::new_v4())).await;
    assert!(r.is_err(), "suspended_by references members(id)");
    let r = write("UPDATE members SET suspended_by = $2 WHERE id = $1", active, Some(h.admin)).await;
    assert!(r.is_ok(), "{r:?}");
    s.finish().await;
}

// D10: the migration backfills existing suspended rows: `suspended_at` from the latest
// `member_suspended` event (its actor as `suspended_by`), else `updated_at` with no actor;
// members that aren't suspended stay null. Run on a database that has the columns dropped,
// as before the migration.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_suspended_by_migration_backfills_suspended_members() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let pool = s.client.pool();
    sqlx::raw_sql("ALTER TABLE members DROP COLUMN IF EXISTS suspended_by, DROP COLUMN IF EXISTS suspended_at")
        .execute(pool)
        .await
        .unwrap();
    let with_event = s.member_row_before_d10(h.t, "ev", "suspended").await;
    let without = s.member_row_before_d10(h.t, "noev", "suspended").await;
    let active = s.member_row_before_d10(h.t, "act", "active").await;
    let old = Utc::now() - Duration::days(30);
    let newer = Utc::now() - Duration::days(10);
    let stamp = Utc::now() - Duration::days(3);
    sqlx::query("UPDATE members SET updated_at = $2 WHERE id = $1")
        .bind(without)
        .bind(stamp)
        .execute(pool)
        .await
        .unwrap();
    for (at, actor) in [(old, None), (newer, Some(h.admin))] {
        sqlx::query(
            "INSERT INTO access_events (tenant_id, actor_member_id, action, target_member_id, at)
             VALUES ($1, $2, 'member_suspended', $3, $4)",
        )
        .bind(h.t.as_uuid())
        .bind(actor)
        .bind(with_event)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
    }
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/migrations/20261023000003_members_suspended_by.sql"
    );
    let sql = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    sqlx::raw_sql(AssertSqlSafe(sql)).execute(pool).await.unwrap();

    let (at, by) = suspension_of(&s, with_event).await;
    let at = at.expect("backfilled from the event");
    assert!((at - newer).num_milliseconds().abs() < 1000, "the latest event wins: {at}");
    assert_eq!(by, Some(h.admin));
    let (at, by) = suspension_of(&s, without).await;
    assert_eq!(at.map(|t| (t - stamp).num_milliseconds().abs() < 1000), Some(true), "updated_at: {at:?}");
    assert_eq!(by, None);
    assert_eq!(suspension_of(&s, active).await, (None, None));
    s.finish().await;
}

// S2 / P16 (b): revoke-now and suspend mark the member's `cb_m_*` rows for rotation
// (`password_enc = ''`; the rows and R31's `last_acted_role` stay) in their transaction, and
// return the touched projects: the `remove_member` union plus the projects of the member's
// personal logins.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn revoke_now_and_suspend_mark_member_roles_for_rotation() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let backup = s.active(&h, "backup").await;
    c.set_tenant_admin(Some(h.admin), backup, true).await.unwrap();
    // Alice: a tenant admin with a grant on the hub's project (her member role there) and a
    // login on another project's database, where she has no grant and no member role.
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.set_tenant_admin(Some(h.admin), alice, true).await.unwrap();
    let p3 = s.project(h.t).await;
    let (db3, ep3) = s.ready_database(h.t, p3).await;
    s.login(alice, db3, "alice").await;
    c.upsert_personal_role_row(&ep3, alice, db3, "alice", V1).await.unwrap();
    let role = |member: Uuid, name: &str| MemberBranchRole {
        member_id: member,
        branch_id: h.db,
        role_name: name.into(),
        password_enc: "v1:sealed".into(),
        applied_roles: vec!["app".into()],
    };
    c.upsert_member_branch_role(&role(alice, "cb_m_alice")).await.unwrap();
    assert!(c.record_member_acted_role(alice, h.db, "app").await.unwrap());

    let touched = sorted(c.revoke_postgres_access(h.admin, alice).await.unwrap());
    assert_eq!(touched, sorted(vec![h.p, p3]));
    let rows = c.member_branch_roles(h.db).await.unwrap();
    let row = rows.iter().find(|r| r.member_id == alice).expect("the row stays");
    assert_eq!((row.role_name.as_str(), row.password_enc.as_str()), ("cb_m_alice", ""), "marked for rotation");
    assert_eq!(row.applied_roles, vec!["app".to_string()]);
    assert!(c.member_acted_roles(h.db).await.unwrap().contains(&(alice, "app".to_string())), "R31 survives");
    let login = &logins_of(&s, alice).await[0];
    assert_eq!((login.status, login.revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::RevokedNow)));
    assert_eq!(s.personal_rows("alice").await, vec![(ep3.clone(), None)]);
    assert_eq!(s.status_of(alice).await, MemberStatus::Active, "Studio access is untouched");
    let revoked = s.events_of(h.t, "personal_logins_revoked").await;
    let last = revoked.last().expect("one event");
    let after = last.after.clone().unwrap();
    assert_eq!((after["count"].as_i64(), after["reason"].as_str()), (Some(1), Some("revoked_now")), "{after}");
    assert!(after.get("rotated_branches").is_some(), "{after}");
    assert_eq!((last.actor_member_id, last.target_member_id), (Some(h.admin), Some(alice)));

    // Suspend marks them too.
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    c.upsert_member_branch_role(&role(bob, "cb_m_bob")).await.unwrap();
    let touched = c.suspend_member(h.admin, bob).await.unwrap();
    assert!(touched.contains(&h.p), "{touched:?}");
    let rows = c.member_branch_roles(h.db).await.unwrap();
    assert_eq!(rows.iter().find(|r| r.member_id == bob).map(|r| r.password_enc.as_str()), Some(""));
    // Another member's row is left alone.
    assert!(rows.iter().all(|r| r.member_id == alice || r.member_id == bob));
    s.finish().await;
}

// S6: a sign-in never activates a suspended member, with changed claims or an open invite;
// only the operator's bootstrap (the recovery path) does.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_suspended_member_is_never_reactivated_by_sign_in() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    c.suspend_member(h.admin, alice).await.unwrap();
    let same = c.ensure_member_first_comer_for_tests(h.t, "alice", "alice@x.com", "alice", Admission::Active).await.unwrap();
    assert_eq!((same.id, same.status), (alice, MemberStatus::Suspended));
    let changed = c.ensure_member_first_comer_for_tests(h.t, "alice", "new@x.com", "New Name", Admission::Active).await.unwrap();
    assert_eq!(changed.status, MemberStatus::Suspended);
    let invite = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO invites (id, member_id, zitadel_user_id, invited_by, expires_at, token_hash)
         VALUES ($1, $2, 'alice', $3, now() + interval '1 day', $4)",
    )
    .bind(invite)
    .bind(alice)
    .bind(h.admin)
    .bind(&TOKEN[..])
    .execute(c.pool())
    .await
    .unwrap();
    let with_invite = c.ensure_member_first_comer_for_tests(h.t, "alice", "other@x.com", "Other", Admission::Active).await.unwrap();
    assert_eq!(with_invite.status, MemberStatus::Suspended, "an open invite doesn't reactivate");
    assert_eq!(s.status_of(alice).await, MemberStatus::Suspended);
    let (_, _, _, accepted) = c.invite(invite).await.unwrap().unwrap();
    assert_eq!(accepted, None, "the invite isn't accepted");
    assert!(s.events_of(h.t, "invite_accepted").await.is_empty());
    // The operator's recovery path still reactivates.
    let boot = c.bootstrap_admin(h.t, "alice", "alice@x.com").await.unwrap();
    assert_eq!((boot.status, boot.tenant_admin), (MemberStatus::Active, true));
    s.finish().await;
}

// S12: every personal-login name (any status) of every database of the project; nothing from
// another project. `is_personal_login_name` is per database.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn personal_login_names_cover_every_database_of_the_project() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let (db2, _) = s.ready_database(h.t, h.p).await;
    let p3 = s.project(h.t).await;
    let (db3, _) = s.ready_database(h.t, p3).await;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    let carol = s.granted(&h, "carol", PlatformRole::Developer).await;
    let dave = s.active(&h, "dave").await;
    c.put_project_grant(h.admin, dave, p3, &grant(PlatformRole::Developer, &[])).await.unwrap();
    s.login(alice, h.db, "a1").await;
    s.login(bob, db2, "b2").await;
    s.login(carol, h.db, "c1").await;
    c.revoke_postgres_access(h.admin, carol).await.unwrap();
    s.login(dave, db3, "d3").await;
    let names = c.personal_login_names(h.p).await.unwrap();
    assert_eq!(names, ["a1", "b2", "c1"].iter().map(|n| n.to_string()).collect());
    assert_eq!(c.personal_login_names(p3).await.unwrap(), ["d3".to_string()].into_iter().collect());
    assert!(c.personal_login_names(s.project(h.t).await).await.unwrap().is_empty());
    for (db, name, want) in [(h.db, "a1", true), (h.db, "b2", false), (h.db, "c1", true), (db2, "b2", true), (h.db, "nope", false)] {
        assert_eq!(c.is_personal_login_name(db, name).await.unwrap(), want, "{name} on {db}");
    }
    s.finish().await;
}

// MH-37 / MH-15: events carry `branch_id` (written by `record_access_event`); the personal-login
// events name the database and never carry a secret; the log stays append-only.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn events_carry_branch_and_never_a_secret() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (child, _) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.reset_personal_login(alice, h.db, V2, in_days(90)).await.unwrap();
    c.delete_personal_login(h.admin, alice, h.db).await.unwrap();
    let mut seen = 0;
    for e in s.events(h.t).await {
        let text = format!("{} {}", json!(e.before), json!(e.after)).to_lowercase();
        assert!(!text.contains(&V1.to_lowercase()) && !text.contains(&V2.to_lowercase()), "{}: {text}", e.action);
        assert!(!text.contains("scram") && !text.contains("verifier") && !text.contains("password"), "{}: {text}", e.action);
        if e.action.starts_with("personal_login_") {
            seen += 1;
            assert_eq!((e.database_id, e.target_member_id), (Some(h.db), Some(alice)), "{}", e.action);
            let named = e.after.as_ref().or(e.before.as_ref()).unwrap();
            assert_eq!(named["role_name"], "alice", "{}: {named}", e.action);
        }
    }
    assert_eq!(seen, 3, "created, reset, deleted");
    let created = &s.events_of(h.t, "personal_login_created").await[0];
    let after = created.after.as_ref().unwrap();
    assert_eq!(created.actor_member_id, Some(alice), "the member makes their own login");
    assert_eq!(after["level"], "developer", "{after}");
    assert!(after.get("valid_until").is_some(), "{after}");

    let after = json!({ "role": "app", "login": false, "attributes": {} });
    c.record_access_event(&AccessEventInput {
        tenant: h.t,
        actor: Some(h.admin),
        action: AccessAction::RoleCreated,
        target: None,
        project: Some(h.p),
        database: Some(h.db),
        branch: Some(child),
        before: None,
        after: Some(&after),
    })
    .await
    .unwrap();
    let last = s.events(h.t).await.pop().unwrap();
    assert_eq!(last.action, "role_created");
    assert_eq!((last.project_id, last.database_id, last.branch_id), (Some(h.p), Some(h.db), Some(child)));
    assert_eq!(last.after, Some(after));
    // The append-only triggers still hold, for the new column too.
    let changed = sqlx::query("UPDATE access_events SET branch_id = NULL WHERE id = $1")
        .bind(last.id)
        .execute(c.pool())
        .await;
    assert!(changed.is_err(), "access_events stays append-only");
    s.finish().await;
}

// Spec Part 3: the log's `member_id` filter keeps the events whose target or actor is the
// member, with the project filter and paging as before.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_filter_matches_target_or_actor() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    let carol = s.granted(&h, "carol", PlatformRole::Developer).await;
    // Alice acts on Bob.
    c.put_project_grant(alice, bob, h.p, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    let p2 = s.project(h.t).await;
    c.put_project_grant(h.admin, alice, p2, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    let mine = c.access_events(h.t, None, Some(alice), None, 100).await.unwrap();
    assert!(!mine.is_empty());
    for e in &mine {
        assert!(e.actor_member_id == Some(alice) || e.target_member_id == Some(alice), "{e:?}");
    }
    assert!(mine.iter().any(|e| e.actor_member_id == Some(alice) && e.target_member_id == Some(bob)), "as actor");
    assert!(mine.iter().any(|e| e.action == "grant_put" && e.target_member_id == Some(alice)), "as target");
    assert!(!mine.iter().any(|e| e.target_member_id == Some(carol)), "carol's events are left out");
    let all_of_alice = s
        .events(h.t)
        .await
        .into_iter()
        .filter(|e| e.actor_member_id == Some(alice) || e.target_member_id == Some(alice))
        .count();
    assert_eq!(mine.len(), all_of_alice);
    // With the project filter: both apply.
    let in_p2 = c.access_events(h.t, Some(p2), Some(alice), None, 100).await.unwrap();
    assert_eq!(in_p2.len(), 1, "{in_p2:?}");
    assert_eq!(in_p2[0].project_id, Some(p2));
    // Paging.
    let page = c.access_events(h.t, None, Some(alice), Some(mine[0].id), 100).await.unwrap();
    assert_eq!(page.len(), mine.len() - 1);
    s.finish().await;
}

// The plan's `remove_member` change: the touched projects include those of the member's
// personal logins (their branches must drop the role), and removal revokes the logins
// (`member_removed`) in the same transaction.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn remove_member_touches_personal_login_projects() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let backup = s.active(&h, "backup").await;
    c.set_tenant_admin(Some(h.admin), backup, true).await.unwrap();
    // A tenant admin without grants, overrides or member roles, holding one login.
    let alice = s.active(&h, "alice").await;
    c.set_tenant_admin(Some(h.admin), alice, true).await.unwrap();
    let p2 = s.project(h.t).await;
    let (db2, ep2) = s.ready_database(h.t, p2).await;
    s.login(alice, db2, "alice").await;
    c.upsert_personal_role_row(&ep2, alice, db2, "alice", V1).await.unwrap();
    let touched = c.remove_member(h.admin, alice).await.unwrap();
    assert_eq!(touched, vec![p2]);
    let login = &logins_of(&s, alice).await[0];
    assert_eq!((login.status, login.revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::MemberRemoved)));
    assert_eq!(s.personal_rows("alice").await, vec![(ep2, None)]);
    s.finish().await;
}

// D4: the webhook's and the sweep's write revokes every live login of the tenant's member with
// that Zitadel user (`identity_offboarded`) and returns where their rows are; one event per
// member, none when nothing was live (S18); an unknown user or another tenant's is untouched.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn zitadel_offboarding_revokes_by_user() {
    let s = Scratch::new().await;
    let other = s.hub().await;
    let h = s.hub().await;
    let (child, child_ep) = s.child(h.db, "dev").await;
    let (db2, ep2) = s.ready_database(h.t, h.p).await;
    let c = &s.client;
    // The same Zitadel user id in another tenant, with a live login there.
    let twin = s.granted(&other, "alice", PlatformRole::Developer).await;
    s.login(twin, other.db, "alice").await;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    s.login(alice, db2, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&child_ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&ep2, alice, db2, "alice", V1).await.unwrap();
    let live = c.members_with_live_logins().await.unwrap();
    assert!(live.contains(&(h.t, alice, "alice".to_string())), "{live:?}");
    assert!(live.contains(&(other.t, twin, "alice".to_string())), "{live:?}");

    let eps = c.revoke_logins_for_zitadel_user(h.t, "alice").await.unwrap();
    assert_eq!(
        sorted(eps),
        sorted(vec![endpoint(h.db, &h.ep, "alice"), endpoint(child, &child_ep, "alice"), endpoint(db2, &ep2, "alice")])
    );
    for l in logins_of(&s, alice).await {
        assert_eq!((l.status, l.revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::IdentityOffboarded)));
    }
    assert!(s.personal_rows("alice").await.iter().all(|(_, v)| v.is_none()), "the proxy refuses at once");
    let revoked = s.events_of(h.t, "personal_logins_revoked").await;
    assert_eq!(revoked.len(), 1, "one event for the member");
    let after = revoked[0].after.clone().unwrap();
    assert_eq!((after["count"].as_i64(), after["reason"].as_str()), (Some(2), Some("identity_offboarded")));
    let live = c.members_with_live_logins().await.unwrap();
    assert!(!live.iter().any(|(_, m, _)| *m == alice), "{live:?}");
    assert!(live.contains(&(other.t, twin, "alice".to_string())), "another tenant's member is untouched");
    assert_eq!(logins_of(&s, twin).await[0].status, LoginStatus::Active);

    // S18: nothing live, nothing written; an unknown user is nobody.
    let written = s.event_count(h.t).await;
    c.revoke_logins_for_zitadel_user(h.t, "alice").await.unwrap();
    assert!(c.revoke_logins_for_zitadel_user(h.t, "nobody").await.unwrap().is_empty());
    assert_eq!(s.event_count(h.t).await, written);
    s.finish().await;
}

// The proxy row removal the worker uses (S16) deletes only a personal row.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn delete_personal_role_row_deletes_only_personal_rows() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin) VALUES ($1, 'plain', 'x', 'sql')")
        .bind(&h.ep)
        .execute(c.pool())
        .await
        .unwrap();
    c.delete_personal_role_row(&h.ep, "plain").await.unwrap();
    c.delete_personal_role_row(&h.ep, "postgres").await.unwrap();
    c.delete_personal_role_row(&h.ep, "alice").await.unwrap();
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM neon_roles WHERE endpoint_id = $1 ORDER BY name")
        .bind(&h.ep)
        .fetch_all(c.pool())
        .await
        .unwrap();
    assert_eq!(names, vec!["plain".to_string(), "postgres".to_string()]);
    assert_eq!(logins_of(&s, alice).await[0].status, LoginStatus::Active, "the record stays");
    s.finish().await;
}

// MH-17 / R9: only a qualifying member gets a login: not a Viewer, not one narrowed below
// Developer on that database, not an invited or suspended member, not on a deleted database
// or another tenant's; a tenant admin without grants does. A refusal writes nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn create_needs_a_qualifying_member() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let other = s.hub().await;
    let (db2, _) = s.ready_database(h.t, h.p).await;
    let c = &s.client;
    let viewer = s.granted(&h, "viewer", PlatformRole::Viewer).await;
    let narrowed = s.granted(&h, "narrowed", PlatformRole::Developer).await;
    c.put_override(h.admin, narrowed, h.db, &OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None })
        .await
        .unwrap();
    let grants = [(h.p, grant(PlatformRole::Developer, &[]))];
    let (invited, _) = c.create_invited_member(h.admin, h.t, new_invite("inv", "inv@x.com", &grants)).await.unwrap();
    let suspended = s.granted(&h, "susp", PlatformRole::Developer).await;
    c.suspend_member(h.admin, suspended).await.unwrap();
    let dev = s.granted(&h, "dev", PlatformRole::Developer).await;
    let gone = s.database(h.t, h.p).await;
    sqlx::query("UPDATE databases SET status = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(gone)
        .execute(c.pool())
        .await
        .unwrap();
    for (label, member, db) in [
        ("viewer", viewer, h.db),
        ("narrowed below developer", narrowed, h.db),
        ("invited", invited.id, h.db),
        ("suspended", suspended, h.db),
        ("deleted database", dev, gone),
        ("another tenant's database", dev, other.db),
    ] {
        let r = c.create_personal_login(member, db, "nope", V1, in_days(90)).await;
        assert!(matches!(r, Err(LoginWriteError::NotQualified)), "{label}: {r:?}");
    }
    let refused: i64 = sqlx::query_scalar("SELECT count(*) FROM personal_logins").fetch_one(c.pool()).await.unwrap();
    assert_eq!(refused, 0);
    // The narrowing is per database; a tenant admin needs no grant.
    assert_eq!(s.login(narrowed, db2, "narrowed").await.status, LoginStatus::Active);
    assert_eq!(s.login(h.admin, h.db, "admin_login").await.status, LoginStatus::Active);
    assert_eq!(s.login(dev, h.db, "dev").await.status, LoginStatus::Active);
    s.finish().await;
}

// MH-9 / P3 / S18: an access-reducing write revokes the logins it disqualifies in its own
// transaction (`access_lost`, one `personal_logins_revoked` event), only on the databases it
// reaches; a write that revokes nothing logs no such event; a later re-grant revives nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn access_loss_revokes_in_the_same_write_and_logs_only_a_nonzero_count() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (db2, ep2) = s.ready_database(h.t, h.p).await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    s.login(alice, db2, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    c.upsert_personal_role_row(&ep2, alice, db2, "alice", V1).await.unwrap();
    // Bob has no login: his downgrade logs only the grant.
    let mark = last_event_id(&s, h.t).await;
    c.delete_project_grant(h.admin, bob, h.p).await.unwrap();
    assert_eq!(actions_since(&s, h.t, mark).await, vec!["grant_deleted"]);

    // An override below Developer on one database revokes only that database's login.
    let mark = last_event_id(&s, h.t).await;
    c.put_override(h.admin, alice, h.db, &OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None })
        .await
        .unwrap();
    assert_eq!(actions_since(&s, h.t, mark).await, vec!["override_put", "personal_logins_revoked"]);
    let revoked = s.events_of(h.t, "personal_logins_revoked").await;
    let after = revoked[0].after.clone().unwrap();
    assert_eq!((after["count"].as_i64(), after["reason"].as_str()), (Some(1), Some("access_lost")), "{after}");
    let logins = logins_of(&s, alice).await;
    let on = |db: Uuid| logins.iter().find(|l| l.database_id == db).unwrap().clone();
    assert_eq!((on(h.db).status, on(h.db).revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::AccessLost)));
    assert_eq!(on(db2).status, LoginStatus::Active);
    assert_eq!(c.routable_role_secret(&h.ep, "alice").await.unwrap(), Some(None), "the proxy refuses at once");
    assert_eq!(c.routable_role_secret(&ep2, "alice").await.unwrap(), Some(Some(V1.to_string())));

    // Lifting the override revives nothing.
    c.delete_override(h.admin, alice, h.db).await.unwrap();
    assert_eq!(logins_of(&s, alice).await.iter().find(|l| l.database_id == h.db).unwrap().status, LoginStatus::Revoked);

    // A downgrade to Viewer revokes the rest.
    c.put_project_grant(h.admin, alice, h.p, &grant(PlatformRole::Viewer, &[])).await.unwrap();
    assert!(logins_of(&s, alice).await.iter().all(|l| l.status == LoginStatus::Revoked));
    assert_eq!(c.routable_role_secret(&ep2, "alice").await.unwrap(), Some(None));

    // A tenant admin cleared, holding no grant.
    let carol = s.active(&h, "carol").await;
    c.set_tenant_admin(Some(h.admin), carol, true).await.unwrap();
    s.login(carol, h.db, "carol").await;
    c.set_tenant_admin(Some(h.admin), carol, false).await.unwrap();
    assert_eq!(logins_of(&s, carol).await[0].status, LoginStatus::Revoked);
    s.finish().await;
}

// P24 and the worker's read: the lists report each login's status (active, expired, revoked
// with its reason) and its project; the worker's records carry the SQL predicate and no
// verifier for a revoked login. The roster reads list suspended members and their own grants.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn personal_login_reads_report_status_and_qualifies() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (db2, _) = s.ready_database(h.t, h.p).await;
    let other = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    let carol = s.granted(&h, "carol", PlatformRole::Admin).await;
    s.login(alice, h.db, "alice").await;
    s.login(alice, db2, "alice").await;
    s.login(bob, h.db, "bob").await;
    c.revoke_postgres_access(h.admin, bob).await.unwrap();
    s.login(carol, h.db, "carol").await;
    sqlx::query("UPDATE personal_logins SET valid_until = now() - interval '1 second' WHERE member_id = $1")
        .bind(carol)
        .execute(c.pool())
        .await
        .unwrap();

    let listed = c.database_personal_logins(h.db).await.unwrap();
    let by = |m: Uuid| listed.iter().find(|l| l.member_id == m).unwrap_or_else(|| panic!("{m} listed"));
    assert_eq!(listed.len(), 3);
    assert_eq!((by(alice).status, by(alice).revoked_reason), (LoginStatus::Active, None));
    assert_eq!((by(bob).status, by(bob).revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::RevokedNow)));
    assert!(by(bob).revoked_at.is_some());
    assert_eq!(by(carol).status, LoginStatus::Expired);
    assert!(listed.iter().all(|l| l.project_id == h.p && l.database_id == h.db));
    assert_eq!(logins_of(&s, alice).await.len(), 2);

    // The worker's records: the predicate, from SQL; a suspended member (written directly
    // here, bypassing the revoking write) doesn't qualify.
    sqlx::query("UPDATE members SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(carol)
        .execute(c.pool())
        .await
        .unwrap();
    let records = c.personal_login_records(h.db).await.unwrap();
    let rec = |m: Uuid| records.iter().find(|r| r.member_id == m).unwrap().clone();
    assert_eq!((rec(alice).scram_verifier.as_deref(), rec(alice).revoked, rec(alice).qualifies), (Some(V1), false, true));
    assert_eq!((rec(bob).scram_verifier, rec(bob).revoked, rec(bob).qualifies), (None, true, true));
    assert_eq!((rec(carol).revoked, rec(carol).qualifies), (false, false));
    assert_eq!(rec(alice).role_name, "alice");

    // The roster: suspended members are listed; every own grant of the tenant, theirs too.
    let listings = c.list_member_listings(h.t).await.unwrap();
    assert_eq!(listings.iter().find(|l| l.id == carol).map(|l| l.status), Some(MemberStatus::Suspended));
    assert_eq!(c.member_listing(carol).await.unwrap().map(|l| l.status), Some(MemberStatus::Suspended));
    let roles = c.member_project_roles(h.t).await.unwrap();
    let own = |member, platform_role| MemberProjectRole { member, project: h.p, platform_role, via: GrantVia::Own };
    assert!(roles.contains(&own(alice, PlatformRole::Developer)), "{roles:?}");
    assert!(roles.contains(&own(carol, PlatformRole::Admin)), "{roles:?}");
    assert!(!roles.iter().any(|r| r.member == other.admin), "another tenant's members");
    s.finish().await;
}

// ---- Members hub step 1, final fix round, area A -------------------------------------------

// D7: the webhook's and the sweep's write marks the member offboarded (whether or not a login
// was live), independently of its status; an offboarded member doesn't qualify for a login.
// Only Zitadel showing the user active again clears the mark (`clear_offboarded`), never a
// sign-in, and the logins stay revoked. Another tenant's member with the same Zitadel user is
// untouched.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn offboarding_marks_the_member_until_its_user_is_active_again() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    let elsewhere = s.tenant().await;
    let twin = c
        .ensure_member_first_comer_for_tests(elsewhere, "alice", "alice@x.com", "alice", Admission::Active)
        .await
        .unwrap()
        .id;

    c.revoke_logins_for_zitadel_user(h.t, "alice").await.unwrap();
    let marked = c.member(alice).await.unwrap().unwrap();
    assert!(marked.offboarded, "the webhook's write marks the member");
    assert_eq!(marked.status, MemberStatus::Active, "the mark isn't a status");
    c.revoke_logins_for_zitadel_user(h.t, "bob").await.unwrap();
    assert!(c.member(bob).await.unwrap().unwrap().offboarded, "marked without a live login too");
    assert!(!c.member(twin).await.unwrap().unwrap().offboarded, "another tenant's member");
    assert!(
        c.member_listing(bob).await.unwrap().unwrap().offboarded,
        "the roster reads the mark"
    );

    // MH-17 with D7: no login while marked, whatever the grant.
    assert!(!s.sql_qualifies(alice, h.db).await);
    let refused = c.create_personal_login(alice, h.db, "alice2", V2, in_days(90)).await;
    assert!(matches!(refused, Err(LoginWriteError::NotQualified)), "{refused:?}");
    // By tenant, then member.
    let mut want = vec![(h.t, alice, "alice".to_string()), (h.t, bob, "bob".to_string())];
    want.sort_by_key(|(_, m, _)| *m);
    assert_eq!(c.offboarded_members().await.unwrap(), want);

    // A sign-in never clears it.
    c.ensure_member_first_comer_for_tests(h.t, "bob", "bob@x.com", "bob", Admission::Active).await.unwrap();
    assert!(c.member(bob).await.unwrap().unwrap().offboarded, "a sign-in cleared the mark");

    assert!(c.clear_offboarded(h.t, "alice").await.unwrap());
    assert!(!c.member(alice).await.unwrap().unwrap().offboarded);
    assert!(!c.clear_offboarded(h.t, "alice").await.unwrap(), "no mark left to clear");
    assert!(!c.clear_offboarded(h.t, "nobody").await.unwrap());
    assert!(s.sql_qualifies(alice, h.db).await, "qualifies again");
    let logins = c.member_personal_logins(alice).await.unwrap();
    assert_eq!(
        (logins[0].status, logins[0].revoked_reason),
        (LoginStatus::Revoked, Some(RevokeReason::IdentityOffboarded)),
        "the login stays revoked"
    );
    assert_eq!(
        c.offboarded_members().await.unwrap(),
        vec![(h.t, bob, "bob".to_string())]
    );
    s.finish().await;
}

// EOS-1: the reconcile's row write is a compare-and-set on the password it read. A rotation
// mark (`''`) written by revoke-now (or a suspension) after the read survives; a write that
// read the mark replaces it; `None` writes only where there is no row.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_rotation_mark_survives_a_reconcile_that_read_the_row_before_it() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let row = |member: Uuid, password: &str| MemberBranchRole {
        member_id: member,
        branch_id: h.db,
        role_name: format!("cb_m_{}", member.simple()),
        password_enc: password.into(),
        applied_roles: Vec::new(),
    };
    let stored = |member: Uuid| {
        let c = c.clone();
        async move {
            c.member_branch_roles(h.db)
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.member_id == member)
                .map(|r| r.password_enc)
        }
    };
    c.upsert_member_branch_role(&row(alice, "v1:read")).await.unwrap();
    c.revoke_postgres_access(h.admin, alice).await.unwrap();
    assert_eq!(stored(alice).await.as_deref(), Some(""), "revoke-now marked the row");

    assert!(
        !c.upsert_member_branch_role_if(&row(alice, "v1:stale"), Some("v1:read")).await.unwrap(),
        "a reconcile that read the row before the mark overwrote it"
    );
    assert_eq!(stored(alice).await.as_deref(), Some(""), "the mark survives");
    assert!(c.upsert_member_branch_role_if(&row(alice, "v1:fresh"), Some("")).await.unwrap());
    assert_eq!(stored(alice).await.as_deref(), Some("v1:fresh"));

    let bob = s.granted(&h, "bob", PlatformRole::Developer).await;
    assert!(
        !c.upsert_member_branch_role_if(&row(bob, "v1:b"), Some("v1:gone")).await.unwrap(),
        "a row deleted since the read isn't made again"
    );
    assert_eq!(stored(bob).await, None);
    assert!(c.upsert_member_branch_role_if(&row(bob, "v1:b"), None).await.unwrap());
    assert!(
        !c.upsert_member_branch_role_if(&row(bob, "v1:other"), None).await.unwrap(),
        "`None` expects no row"
    );
    assert_eq!(stored(bob).await.as_deref(), Some("v1:b"));
    s.finish().await;
}

// EOS-6, T3-M3 (the invariant, MH-9 with P2): a login never outlives its database or project.
// `chelabase_personal_login_qualifies` is false for a database that is deleting or deleted, or
// whose project is (every combination of the two statuses), and every write that ends a
// database's or a project's life revokes its live logins (`access_lost`) in its own
// transaction, so the proxy refuses them at once.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn no_login_outlives_its_database_or_project() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    let ended = |status: &str| matches!(status, "deleting" | "deleted");
    for db_status in ["pending", "provisioning", "ready", "failed", "deleting", "deleted"] {
        for project_status in ["active", "deleting", "deleted"] {
            for (sql, value) in [
                ("UPDATE databases SET status = $2 WHERE id = $1", (h.db, db_status)),
                ("UPDATE projects SET status = $2 WHERE id = $1", (h.p, project_status)),
            ] {
                sqlx::query(sql)
                    .bind(value.0)
                    .bind(value.1)
                    .execute(c.pool())
                    .await
                    .unwrap();
            }
            assert_eq!(
                s.sql_qualifies(alice, h.db).await,
                !ended(db_status) && !ended(project_status),
                "database {db_status}, project {project_status}"
            );
        }
    }
    s.finish().await;

    // The writes, each on a fresh hub with a live login and its proxy row.
    for write in ["begin_delete", "mark_deleted", "begin_project_delete", "finish_project_delete"] {
        let s = Scratch::new().await;
        let h = s.hub().await;
        let c = &s.client;
        let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
        s.login(alice, h.db, "alice").await;
        c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
        match write {
            "begin_delete" => {
                c.begin_delete(DatabaseId(h.db)).await.unwrap();
            }
            "mark_deleted" => c.mark_deleted(DatabaseId(h.db)).await.unwrap(),
            "begin_project_delete" => {
                c.begin_project_delete(h.t, ProjectId(h.p)).await.unwrap();
            }
            _ => c.finish_project_delete(ProjectId(h.p)).await.unwrap(),
        }
        let logins = c.member_personal_logins(alice).await.unwrap();
        assert_eq!(
            (logins[0].status, logins[0].revoked_reason),
            (LoginStatus::Revoked, Some(RevokeReason::AccessLost)),
            "{write}: the login outlived its database"
        );
        assert_eq!(s.record_verifier(alice, h.db).await, None, "{write}");
        assert_eq!(
            c.routable_role_secret(&h.ep, "alice").await.unwrap().flatten(),
            None,
            "{write}: the proxy still answers the login"
        );
        assert_eq!(s.events_of(h.t, "personal_logins_revoked").await.len(), 1, "{write}");
        s.finish().await;
    }
}

// T8-8: a delete takes the record's lock before it reads where the login's rows are, so a row
// the worker was inserting when the delete began (its insert held here by a test trigger, the
// record read `FOR SHARE`) is among the endpoints it returns: the proxy cache is told of it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_login_delete_lists_a_row_the_worker_was_inserting() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (child, child_ep) = s.child(h.db, "dev").await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    sqlx::raw_sql(
        "CREATE FUNCTION test_hold_personal_insert() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock_shared(4243); RETURN NEW; END $$;
         CREATE TRIGGER test_hold_personal_insert BEFORE INSERT ON neon_roles FOR EACH ROW
           WHEN (NEW.origin = 'personal') EXECUTE FUNCTION test_hold_personal_insert();",
    )
    .execute(c.pool())
    .await
    .unwrap();
    let mut gate = c.pool().begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(4243)").execute(&mut *gate).await.unwrap();
    let on_child = tokio::spawn({
        let (c, ep) = (c.clone(), child_ep.clone());
        async move { c.upsert_personal_role_row(&ep, alice, h.db, "alice", V1).await }
    });
    let known = blocked_or_done(&s, &[], &on_child).await;
    assert!(!on_child.is_finished(), "the worker's insert is held by the trigger");
    let delete = tokio::spawn({
        let c = c.clone();
        async move { c.delete_personal_login(h.admin, alice, h.db).await }
    });
    blocked_or_done(&s, &known, &delete).await;
    gate.commit().await.unwrap();
    on_child.await.unwrap().unwrap();
    let rows = delete.await.unwrap().unwrap();
    assert!(
        rows.contains(&endpoint(child, &child_ep, "alice")),
        "the row landed under the delete isn't listed, so no proxy forgets it: {rows:?}"
    );
    assert!(rows.contains(&endpoint(h.db, &h.ep, "alice")), "{rows:?}");
    assert!(s.personal_rows("alice").await.is_empty());
    s.finish().await;
}

// T3-M2: the worker's write puts a login's row only on an endpoint of the login's own
// database's branches; another database's endpoint (of the same project) gets nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_login_row_lands_only_on_its_databases_endpoints() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let (_db2, ep2) = s.ready_database(h.t, h.p).await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&ep2, alice, h.db, "alice", V1).await.unwrap();
    assert_eq!(
        s.personal_rows("alice").await,
        Vec::<(String, Option<String>)>::new(),
        "a row on another database's endpoint"
    );
    assert_eq!(c.routable_role_secret(&ep2, "alice").await.unwrap().flatten(), None);
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    assert_eq!(s.personal_rows("alice").await, vec![(h.ep.clone(), Some(V1.to_string()))]);
    s.finish().await;
}

// T3-M1: a personal row is its record's: a rename of it is refused, never answered as done
// (`Renamed { row: false }` told the caller its keys moved); the row and its name stay.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_personal_row_is_never_renamed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.granted(&h, "alice", PlatformRole::Developer).await;
    s.login(alice, h.db, "alice").await;
    c.upsert_personal_role_row(&h.ep, alice, h.db, "alice", V1).await.unwrap();
    let out = c
        .rename_role_copy(&h.ep, BranchId(h.db), "alice", "renamed", None, None)
        .await
        .unwrap();
    assert!(!matches!(out, RenameCopyOutcome::Renamed { .. }), "{out:?}");
    assert_eq!(s.personal_rows("alice").await, vec![(h.ep.clone(), Some(V1.to_string()))]);
    assert!(s.personal_rows("renamed").await.is_empty());
    s.finish().await;
}

// ---- Members hub step 2, T1: teams ----------------------------------------------------------
//
// Setup writes teams, team grants and memberships with plain SQL (`raw_*`), so each test's
// first failure is the read or write under test, never its setup.

impl Scratch {
    /// A team of `t` named `name`.
    async fn raw_team(&self, t: TenantId, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO teams (id, tenant_id, name) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(t.as_uuid())
            .bind(name)
            .execute(self.client.pool())
            .await
            .unwrap();
        id
    }

    /// Sets `team`'s grant on `project` to `g` as stored (no normalising).
    async fn raw_team_grant(&self, team: Uuid, project: Uuid, g: &GrantRow) {
        sqlx::query(
            "INSERT INTO team_grants (team_id, project_id, platform_role, pg_roles) VALUES ($1, $2, $3, $4)
             ON CONFLICT (team_id, project_id)
             DO UPDATE SET platform_role = EXCLUDED.platform_role, pg_roles = EXCLUDED.pg_roles",
        )
        .bind(team)
        .bind(project)
        .bind(g.platform_role.as_str())
        .bind(&g.pg_roles)
        .execute(self.client.pool())
        .await
        .unwrap();
    }

    async fn raw_team_member(&self, team: Uuid, member: Uuid) {
        sqlx::query("INSERT INTO team_members (team_id, member_id) VALUES ($1, $2)")
            .bind(team)
            .bind(member)
            .execute(self.client.pool())
            .await
            .unwrap();
    }

    /// Sets `member`'s own grant on `project` to `g` as stored.
    async fn raw_grant(&self, member: Uuid, project: Uuid, g: &GrantRow) {
        sqlx::query(
            "INSERT INTO project_grants (member_id, project_id, platform_role, pg_roles) VALUES ($1, $2, $3, $4)
             ON CONFLICT (member_id, project_id)
             DO UPDATE SET platform_role = EXCLUDED.platform_role, pg_roles = EXCLUDED.pg_roles",
        )
        .bind(member)
        .bind(project)
        .bind(g.platform_role.as_str())
        .bind(&g.pg_roles)
        .execute(self.client.pool())
        .await
        .unwrap();
    }

    async fn raw_override(&self, member: Uuid, db: Uuid, o: &OverrideRow) {
        sqlx::query(
            "INSERT INTO database_grant_overrides (member_id, database_id, platform_role, pg_roles)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(member)
        .bind(db)
        .bind(o.platform_role.map(|r| r.as_str()))
        .bind(&o.pg_roles)
        .execute(self.client.pool())
        .await
        .unwrap();
    }

    /// A live login record of `member` on `db` named `name` (V1, 90 days), inserted without the
    /// qualification check: the team writes under test must revoke it.
    async fn raw_login(&self, member: Uuid, db: Uuid, name: &str) {
        sqlx::query(
            "INSERT INTO personal_logins (member_id, database_id, role_name, scram_verifier, valid_until)
             VALUES ($1, $2, $3, $4, now() + interval '90 days')",
        )
        .bind(member)
        .bind(db)
        .bind(name)
        .bind(V1)
        .execute(self.client.pool())
        .await
        .unwrap();
    }

    async fn delete_project(&self, project: Uuid) {
        sqlx::query("UPDATE projects SET status = 'deleted' WHERE id = $1")
            .bind(project)
            .execute(self.client.pool())
            .await
            .unwrap();
    }

    /// The teams `member` is in, by id.
    async fn memberships_of(&self, member: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT team_id FROM team_members WHERE member_id = $1 ORDER BY team_id")
            .bind(member)
            .fetch_all(self.client.pool())
            .await
            .unwrap()
    }

    /// `team`'s grant on `project` as stored.
    async fn team_grant_row(&self, team: Uuid, project: Uuid) -> Option<GrantRow> {
        let row: Option<(String, Vec<String>)> = sqlx::query_as(
            "SELECT platform_role, pg_roles FROM team_grants WHERE team_id = $1 AND project_id = $2",
        )
        .bind(team)
        .bind(project)
        .fetch_optional(self.client.pool())
        .await
        .unwrap();
        row.map(|(role, pg_roles)| GrantRow { platform_role: PlatformRole::parse(&role).unwrap(), pg_roles })
    }

    /// `chelabase_member_level(member, db)`.
    async fn level(&self, member: Uuid, db: Uuid) -> Option<String> {
        sqlx::query_scalar("SELECT chelabase_member_level($1, $2)")
            .bind(member)
            .bind(db)
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }

    /// How many rows of `table` reference `team`.
    async fn team_rows(&self, table: &str, team: Uuid) -> i64 {
        let column = if table == "teams" { "id" } else { "team_id" };
        sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {table} WHERE {column} = $1")))
            .bind(team)
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }
}

/// `change` as a checked team write, with the check's read taken just before (I4).
async fn team_write(s: &Scratch, h: &Hub, change: &TeamChange) -> Result<TeamWrite, TeamWriteError> {
    let expect = s.client.team_change_observed(h.t, change).await?;
    s.client.write_team_checked(h.admin, h.t, change, &expect).await
}

fn changed(w: &Result<TeamWrite, TeamWriteError>) -> bool {
    matches!(w, Ok(TeamWrite { changed: true, .. }))
}

/// Grants as `GrantSources` takes them (team grants by team id).
fn grants_of(teams: &[(Uuid, GrantRow)]) -> Vec<chelabase_types::access::Grant> {
    let mut teams = teams.to_vec();
    teams.sort_by_key(|(id, _)| *id);
    teams.iter().map(|(_, g)| g.to_grant()).collect()
}

/// Lowercase hyphenated ids, sorted, as JSON (P38).
fn id_strings(ids: &[Uuid]) -> serde_json::Value {
    let mut ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    ids.sort();
    json!(ids)
}

// P5, P28 (MH-2): the SQL level function and its predicate agree with `GrantSources::effective`
// and `access::qualifies` over own grant × two team grants × override, for active and suspended
// members, tenant admins included. Level compared for active members, `qualifies` for every row.
// The override-only rows catch the `LEAST` trap (no source must stay no level).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_level_agrees_with_rust_over_teams() {
    #[derive(Clone, Copy, Debug)]
    enum Ov {
        None,
        Level(PlatformRole),
        RolesOnly,
    }
    use PlatformRole::{Admin, Developer, Viewer};
    let levels = [None, Some(Viewer), Some(Developer), Some(Admin)];
    let overrides = [Ov::None, Ov::Level(Viewer), Ov::Level(Developer), Ov::Level(Admin), Ov::RolesOnly];
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let p = s.project(t).await;
    let db = s.database(t, p).await;
    // Two teams per level (slots 0 and 1), each granting `p` and naming its own role.
    let mut teams = std::collections::BTreeMap::new();
    for slot in 0..2 {
        for l in [Viewer, Developer, Admin] {
            let id = s.raw_team(t, &format!("t{slot}-{}", l.as_str())).await;
            let g = GrantRow::new(l, &[format!("t{slot}")]);
            s.raw_team_grant(id, p, &g).await;
            teams.insert((slot, l), (id, g));
        }
    }
    let mut failures = Vec::new();
    let mut n = 0;
    for status in ["active", "suspended"] {
        for tenant_admin in [false, true] {
            for own in levels {
                for t0 in levels {
                    for t1 in levels {
                        for o in overrides {
                            n += 1;
                            let id = s.member_row(t, &format!("z{n}"), status, tenant_admin).await;
                            let own_row = own.map(|l| GrantRow::new(l, &["app".to_string()]));
                            if let Some(g) = &own_row {
                                s.raw_grant(id, p, g).await;
                            }
                            let mut joined = Vec::new();
                            for (slot, l) in [(0, t0), (1, t1)] {
                                if let Some(l) = l {
                                    let (team, g) = teams[&(slot, l)].clone();
                                    s.raw_team_member(team, id).await;
                                    joined.push((team, g));
                                }
                            }
                            let override_row = match o {
                                Ov::None => None,
                                Ov::Level(l) => Some(OverrideRow { platform_role: Some(l), pg_roles: None }),
                                Ov::RolesOnly => Some(OverrideRow { platform_role: None, pg_roles: Some(vec!["app".into()]) }),
                            };
                            if let Some(row) = &override_row {
                                s.raw_override(id, db, row).await;
                            }
                            let effective = if tenant_admin {
                                Some(tenant_admin_effective())
                            } else {
                                GrantSources::new(
                                    own_row.as_ref().map(GrantRow::to_grant),
                                    grants_of(&joined),
                                    override_row.as_ref().map(OverrideRow::to_narrowing),
                                )
                                .effective()
                            };
                            let active = status == "active";
                            let want_q = qualifies(active, effective.as_ref());
                            let want_level = effective.map(|e| e.platform_role.as_str().to_string());
                            let case = format!("{status} admin={tenant_admin} own={own:?} teams=({t0:?}, {t1:?}) override={o:?}");
                            let (level, q): (Option<String>, bool) = sqlx::query_as(
                                "SELECT chelabase_member_level($1, $2), chelabase_personal_login_qualifies($1, $2)",
                            )
                            .bind(id)
                            .bind(db)
                            .fetch_one(s.client.pool())
                            .await
                            .unwrap_or_else(|e| panic!("{case}: {e}"));
                            if active && level != want_level {
                                failures.push(format!("{case}: level SQL {level:?}, Rust {want_level:?}"));
                            }
                            if q != want_q {
                                failures.push(format!("{case}: qualifies SQL {q}, Rust {want_q}"));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} failures over {n} members:\n{}", failures.len(), failures.join("\n"));
    s.finish().await;
}

// P4 (MH-2): `grant_sources` reads the member's team grants on the project (by team id) with the
// own grant and the override; teams without a grant there, and teams the member isn't in, don't
// count.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn grant_sources_include_team_grants() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let alice = s.active(&h, "alice").await;
    let bob = s.active(&h, "bob").await;
    let own = grant(PlatformRole::Viewer, &["app"]);
    s.raw_grant(alice, h.p, &own).await;
    let narrow = OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None };
    s.raw_override(alice, h.db, &narrow).await;
    let (x, y) = (s.raw_team(h.t, "x").await, s.raw_team(h.t, "y").await);
    let gx = grant(PlatformRole::Developer, &["etl"]);
    let gy = grant(PlatformRole::Admin, &[]);
    s.raw_team_grant(x, h.p, &gx).await;
    s.raw_team_grant(y, h.p, &gy).await;
    // A team granting `p` that alice isn't in, and one of hers granting only `q`.
    let z = s.raw_team(h.t, "z").await;
    s.raw_team_grant(z, h.p, &grant(PlatformRole::Admin, &[])).await;
    let w = s.raw_team(h.t, "w").await;
    s.raw_team_grant(w, q, &grant(PlatformRole::Admin, &[])).await;
    for team in [x, y, w] {
        s.raw_team_member(team, alice).await;
    }
    s.raw_team_member(x, bob).await;
    let teams = grants_of(&[(x, gx.clone()), (y, gy.clone())]);
    assert_eq!(
        c.grant_sources(alice, h.p, Some(h.db)).await.unwrap(),
        GrantSources::new(Some(own.to_grant()), teams.clone(), Some(narrow.to_narrowing())),
        "alice on the database: own grant, both teams' grants on p, the override"
    );
    assert_eq!(
        c.grant_sources(alice, h.p, None).await.unwrap(),
        GrantSources::new(Some(own.to_grant()), teams, None),
        "alice at project level"
    );
    assert_eq!(
        c.grant_sources(bob, h.p, Some(h.db)).await.unwrap(),
        GrantSources::new(None, vec![gx.to_grant()], None),
        "bob holds p through x only"
    );
    s.finish().await;
}

// P4 (MH-2): the worker's per-branch read lists a member whose only source is a team grant on the
// project; a member whose team grants another project isn't listed.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn database_grant_sources_list_team_only_members() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let alice = s.granted(&h, "alice", PlatformRole::Viewer).await;
    let bob = s.active(&h, "bob").await;
    let carol = s.active(&h, "carol").await;
    let x = s.raw_team(h.t, "x").await;
    let gx = grant(PlatformRole::Developer, &["etl"]);
    s.raw_team_grant(x, h.p, &gx).await;
    s.raw_team_member(x, bob).await;
    let w = s.raw_team(h.t, "w").await;
    s.raw_team_grant(w, q, &grant(PlatformRole::Admin, &[])).await;
    s.raw_team_member(w, carol).await;
    let rows = c.database_grant_sources(h.p, h.db).await.unwrap();
    let of = |m: Uuid| rows.iter().find(|(id, _)| *id == m).map(|(_, g)| g.clone());
    assert_eq!(of(bob), Some(GrantSources::new(None, vec![gx.to_grant()], None)), "bob, through x only: {rows:?}");
    assert_eq!(
        of(alice),
        Some(GrantSources::new(Some(grant(PlatformRole::Viewer, &[]).to_grant()), vec![], None))
    );
    assert_eq!(of(carol), None, "carol's team grants another project");
    let ids: Vec<Uuid> = rows.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, sorted(ids.clone()), "by member id");
    s.finish().await;
}

// P4 (MH-2, T2-4): `/v1/me`'s and the project list's read lists every non-deleted project where
// the member has an own grant or a team grant, with the sources at project level; a member with
// only an override has no row (empty sources stay `None`).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_grant_sources_list_team_only_projects() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let gone = s.project(h.t).await;
    let alice = s.active(&h, "alice").await;
    let own = grant(PlatformRole::Viewer, &[]);
    s.raw_grant(alice, h.p, &own).await;
    let x = s.raw_team(h.t, "x").await;
    let gx = grant(PlatformRole::Developer, &["etl"]);
    s.raw_team_grant(x, q, &gx).await;
    s.raw_team_grant(x, gone, &gx).await;
    s.raw_team_member(x, alice).await;
    s.delete_project(gone).await;
    let mut want = vec![
        (h.p, GrantSources::new(Some(own.to_grant()), vec![], None)),
        (q, GrantSources::new(None, vec![gx.to_grant()], None)),
    ];
    want.sort_by_key(|(p, _)| *p);
    assert_eq!(c.member_grant_sources(h.t, alice).await.unwrap(), want, "p (own) and q (team), not the deleted project");
    // An override alone is no access.
    let carol = s.active(&h, "carol").await;
    s.raw_override(carol, h.db, &OverrideRow { platform_role: Some(PlatformRole::Admin), pg_roles: None }).await;
    assert_eq!(c.member_grant_sources(h.t, carol).await.unwrap(), vec![]);
    s.finish().await;
}

// P4: the member list's `has_access` counts a team grant on a non-deleted project.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn has_access_counts_team_grants() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let gone = s.project(h.t).await;
    let bob = s.active(&h, "bob").await;
    let carol = s.active(&h, "carol").await;
    let dave = s.active(&h, "dave").await;
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    s.raw_team_member(x, bob).await;
    let y = s.raw_team(h.t, "y").await;
    s.raw_team_grant(y, gone, &grant(PlatformRole::Admin, &[])).await;
    s.raw_team_member(y, carol).await;
    s.delete_project(gone).await;
    let z = s.raw_team(h.t, "z").await;
    s.raw_team_member(z, dave).await;
    let has = |l: Option<chelabase_metadata::members::MemberListing>| l.map(|l| l.has_access);
    assert_eq!(has(c.member_listing(bob).await.unwrap()), Some(true), "bob has p through x");
    assert_eq!(has(c.member_listing(carol).await.unwrap()), Some(false), "a deleted project is no access");
    assert_eq!(has(c.member_listing(dave).await.unwrap()), Some(false), "a team without grants is no access");
    let listings = c.list_member_listings(h.t).await.unwrap();
    let listed = |m: Uuid| listings.iter().find(|l| l.id == m).map(|l| l.has_access);
    assert_eq!((listed(bob), listed(carol), listed(dave)), (Some(true), Some(false), Some(false)));
    s.finish().await;
}

// P13: the roster's levels are the best among the sources per (member, project), with `via`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_project_roles_report_own_team_and_both() {
    use PlatformRole::{Admin, Developer, Viewer};
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let gone = s.project(h.t).await;
    let [alice, bob, carol, dave, erin, frank] = [
        s.active(&h, "alice").await,
        s.active(&h, "bob").await,
        s.active(&h, "carol").await,
        s.active(&h, "dave").await,
        s.active(&h, "erin").await,
        s.active(&h, "frank").await,
    ];
    let team = |name: &'static str, level: PlatformRole, project: Uuid| {
        let s = &s;
        async move {
            let id = s.raw_team(h.t, name).await;
            s.raw_team_grant(id, project, &grant(level, &[])).await;
            id
        }
    };
    let tv = team("tv", Viewer, h.p).await;
    let td = team("td", Developer, h.p).await;
    let ta = team("ta", Admin, h.p).await;
    let tgone = team("tgone", Admin, gone).await;
    s.raw_grant(alice, h.p, &grant(Viewer, &[])).await; // own only
    s.raw_team_member(td, bob).await; // team only
    s.raw_grant(carol, h.p, &grant(Viewer, &[])).await; // both, the team better
    s.raw_team_member(ta, carol).await;
    s.raw_grant(dave, h.p, &grant(Developer, &[])).await; // both, own better
    s.raw_team_member(tv, dave).await;
    s.raw_team_member(tv, erin).await; // two teams
    s.raw_team_member(td, erin).await;
    s.raw_team_member(tgone, frank).await; // a deleted project only
    s.delete_project(gone).await;
    let ours = [alice, bob, carol, dave, erin, frank];
    let got: Vec<MemberProjectRole> =
        c.member_project_roles(h.t).await.unwrap().into_iter().filter(|r| ours.contains(&r.member)).collect();
    let row = |member, platform_role, via| MemberProjectRole { member, project: h.p, platform_role, via };
    let mut want = vec![
        row(alice, Viewer, GrantVia::Own),
        row(bob, Developer, GrantVia::Team),
        row(carol, Admin, GrantVia::Both),
        row(dave, Developer, GrantVia::Both),
        row(erin, Developer, GrantVia::Team),
    ];
    want.sort_by_key(|r| (r.member, r.project));
    assert_eq!(got, want);
    s.finish().await;
}

// P4, P27 (MH-2): crafted cross-tenant rows never count, in any read: a member of this tenant in
// another tenant's team, a team of this tenant granting another tenant's project, and another
// tenant's member in this tenant's team.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn another_tenants_team_never_counts() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let u = s.hub().await;
    let c = &s.client;
    let alice = s.active(&h, "alice").await;
    let legit = s.raw_team(h.t, "legit").await;
    let gv = grant(PlatformRole::Viewer, &[]);
    s.raw_team_grant(legit, h.p, &gv).await;
    s.raw_team_member(legit, alice).await;
    // Crafted: another tenant's team granting this tenant's project, with alice in it.
    let foreign = s.raw_team(u.t, "foreign").await;
    s.raw_team_grant(foreign, h.p, &grant(PlatformRole::Admin, &[])).await;
    s.raw_team_member(foreign, alice).await;
    // Crafted: this tenant's team granting another tenant's project, with alice in it.
    let crafted = s.raw_team(h.t, "crafted").await;
    s.raw_team_grant(crafted, u.p, &grant(PlatformRole::Admin, &[])).await;
    s.raw_team_member(crafted, alice).await;
    // Crafted: another tenant's member in this tenant's team.
    let mallory = s.active(&u, "mallory").await;
    s.raw_team_member(legit, mallory).await;

    let only_legit = GrantSources::new(None, vec![gv.to_grant()], None);
    assert_eq!(c.grant_sources(alice, h.p, Some(h.db)).await.unwrap(), only_legit, "only legit counts on p");
    assert_eq!(s.level(alice, h.db).await.as_deref(), Some("viewer"), "the level function counts only legit");
    assert!(!s.sql_qualifies(alice, h.db).await);
    assert_eq!(c.grant_sources(alice, u.p, None).await.unwrap(), GrantSources::default(), "u's project");
    assert_eq!(c.member_grant_sources(h.t, alice).await.unwrap(), vec![(h.p, only_legit.clone())]);
    let rows = c.database_grant_sources(h.p, h.db).await.unwrap();
    assert_eq!(rows.iter().find(|(m, _)| *m == alice).map(|(_, g)| g.clone()), Some(only_legit));
    assert!(!rows.iter().any(|(m, _)| *m == mallory), "another tenant's member in legit: {rows:?}");
    let roles = c.member_project_roles(h.t).await.unwrap();
    let alices: Vec<_> = roles.iter().filter(|r| r.member == alice).cloned().collect();
    assert_eq!(
        alices,
        vec![MemberProjectRole { member: alice, project: h.p, platform_role: PlatformRole::Viewer, via: GrantVia::Team }]
    );
    assert!(!roles.iter().any(|r| r.member == mallory));
    // Mallory reaches nothing through legit.
    assert_eq!(c.grant_sources(mallory, h.p, Some(h.db)).await.unwrap(), GrantSources::default());
    assert_eq!(s.level(mallory, h.db).await, None);
    assert_eq!(c.member_grant_sources(u.t, mallory).await.unwrap(), vec![]);
    assert_eq!(c.member_listing(mallory).await.unwrap().map(|l| l.has_access), Some(false));
    s.finish().await;
}

// P25: an invite or approval with teams re-reads the teams under the tenant lock; a team grant
// changed since its check read them (or a team gone, or of another tenant) is `Changed` with
// nothing written, on all four paths.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_invite_with_teams_sees_a_team_grant_change_as_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    let seen = [TeamSeen { team: x, grants: vec![(h.p, grant(PlatformRole::Viewer, &[]))] }];
    let pending = c.ensure_member_first_comer_for_tests(h.t, "pend", "pend@x.com", "P", Admission::Pending).await.unwrap();
    let bob = s.active(&h, "bob").await;
    let bob_seen = c.project_access(h.p).await.unwrap().observed(h.p, bob);
    // The team grant changes after the checks read it.
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Developer, &["etl"])).await;
    let members = s.member_count(h.t).await;
    let events = s.event_count(h.t).await;

    let invite = NewInvite { teams: &seen, ..new_invite("zi", "i@x.com", &[]) };
    let r = c.create_invited_member(h.admin, h.t, invite).await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "invite: {r:?}");
    let r = c.add_existing_user(h.admin, h.t, "ze", "e@x.com", "E", &[], &seen).await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "add existing user: {r:?}");
    let r = c.approve_member(h.admin, pending.id, &[], &seen).await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "approve: {r:?}");
    let r = c
        .put_project_grants_checked(h.admin, bob, &[], &seen, std::slice::from_ref(&bob_seen))
        .await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "invite of an existing member: {r:?}");
    assert_eq!(s.member_count(h.t).await, members, "no member written");
    assert_eq!(s.event_count(h.t).await, events, "no event written");
    assert_eq!(s.status_of(pending.id).await, MemberStatus::Pending);
    assert!(s.memberships_of(bob).await.is_empty() && s.memberships_of(pending.id).await.is_empty());

    // A team gone, or another tenant's, is `Changed` too.
    let u = s.hub().await;
    let theirs = s.raw_team(u.t, "theirs").await;
    let gone = s.raw_team(h.t, "gone").await;
    sqlx::query("DELETE FROM teams WHERE id = $1").bind(gone).execute(c.pool()).await.unwrap();
    for (label, team) in [("another tenant's team", theirs), ("a deleted team", gone)] {
        let seen = [TeamSeen { team, grants: vec![] }];
        let r = c.approve_member(h.admin, pending.id, &[], &seen).await;
        assert!(matches!(r, Err(AccessWriteError::Changed)), "{label}: {r:?}");
    }
    assert_eq!(s.status_of(pending.id).await, MemberStatus::Pending);
    s.finish().await;
}

/// Runs `write` while another session holds the tenant row; panics unless it waits for it.
/// Returns the write's result once the holder commits.
async fn waits_for_the_tenant<T, F>(s: &Scratch, t: TenantId, label: &str, write: F) -> T
where
    T: std::fmt::Debug + Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let hold = s.hold_tenant(t).await;
    let task = tokio::spawn(write);
    blocked_or_done(s, &[], &task).await;
    if task.is_finished() {
        let r = task.await.unwrap();
        panic!("{label} didn't wait for the tenant row: {r:?}");
    }
    hold.commit().await.unwrap();
    task.await.unwrap()
}

// P26: every writer of `team_members` holds the tenant row first, so a team's member set is
// stable under the tenant lock: team member add and remove, team delete, the invite paths with
// teams, and `remove_member`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn every_team_members_writer_holds_the_tenant() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = s.client.clone();
    let (t, admin) = (h.t, h.admin);
    let x = s.raw_team(t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    let seen = vec![TeamSeen { team: x, grants: vec![(h.p, grant(PlatformRole::Viewer, &[]))] }];
    let alice = s.active(&h, "alice").await;

    let team_change = |change: TeamChange| {
        let c = c.clone();
        async move {
            let expect = c.team_change_observed(t, &change).await?;
            c.write_team_checked(admin, t, &change, &expect).await
        }
    };
    let w = waits_for_the_tenant(&s, t, "team member add", team_change(TeamChange::AddMember { team: x, member: alice })).await;
    assert!(changed(&w), "{w:?}");
    let w = waits_for_the_tenant(&s, t, "team member remove", team_change(TeamChange::RemoveMember { team: x, member: alice })).await;
    assert!(changed(&w), "{w:?}");

    let invited = waits_for_the_tenant(&s, t, "invite with teams", {
        let (c, seen) = (c.clone(), seen.clone());
        async move {
            let invite = NewInvite { teams: &seen, ..new_invite("zi", "i@x.com", &[]) };
            c.create_invited_member(admin, t, invite).await.map(|(l, _)| l.id)
        }
    })
    .await
    .unwrap();
    assert_eq!(s.memberships_of(invited).await, vec![x]);
    let added = waits_for_the_tenant(&s, t, "add existing user with teams", {
        let (c, seen) = (c.clone(), seen.clone());
        async move { c.add_existing_user(admin, t, "ze", "e@x.com", "E", &[], &seen).await.map(|l| l.id) }
    })
    .await
    .unwrap();
    assert_eq!(s.memberships_of(added).await, vec![x]);
    let pending = c.ensure_member_first_comer_for_tests(t, "pend", "pend@x.com", "P", Admission::Pending).await.unwrap().id;
    waits_for_the_tenant(&s, t, "approve with teams", {
        let (c, seen) = (c.clone(), seen.clone());
        async move { c.approve_member(admin, pending, &[], &seen).await.map(|_| ()) }
    })
    .await
    .unwrap();
    assert_eq!(s.memberships_of(pending).await, vec![x]);
    let bob = s.active(&h, "bob").await;
    let bob_seen = c.project_access(h.p).await.unwrap().observed(h.p, bob);
    waits_for_the_tenant(&s, t, "invite of an existing member with teams", {
        let (c, seen) = (c.clone(), seen.clone());
        async move { c.put_project_grants_checked(admin, bob, &[], &seen, &[bob_seen]).await.map(|_| ()) }
    })
    .await
    .unwrap();
    assert_eq!(s.memberships_of(bob).await, vec![x]);
    waits_for_the_tenant(&s, t, "remove_member", {
        let c = c.clone();
        async move { c.remove_member(admin, bob).await.map(|_| ()) }
    })
    .await
    .unwrap();
    assert!(s.memberships_of(bob).await.is_empty());
    let w = waits_for_the_tenant(&s, t, "team delete", team_change(TeamChange::Delete { team: x })).await;
    assert!(changed(&w), "{w:?}");
    s.finish().await;
}

// MH-9 via P3: a team write that makes `chelabase_personal_login_qualifies` false revokes in its
// own transaction (verifier gone when the write returns, one `personal_logins_revoked` for the
// member): a grant downgraded to Viewer, a grant deleted, a member removed, a team deleted. An
// unaffected login stays live.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_writes_revoke_logins_that_stop_qualifying() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let keep = s.granted(&h, "keep", PlatformRole::Developer).await;
    s.login(keep, h.db, "keep").await;
    let labels = ["downgrade to viewer", "grant delete", "member removed", "team delete"];
    for (i, label) in labels.into_iter().enumerate() {
        let team = s.raw_team(h.t, label).await;
        s.raw_team_grant(team, h.p, &grant(PlatformRole::Developer, &[])).await;
        let m = s.active(&h, &format!("m{i}")).await;
        s.raw_team_member(team, m).await;
        s.raw_login(m, h.db, &format!("m{i}")).await;
        let change = match i {
            0 => TeamChange::PutGrant { team, project: h.p, grant: grant(PlatformRole::Viewer, &[]) },
            1 => TeamChange::DeleteGrant { team, project: h.p },
            2 => TeamChange::RemoveMember { team, member: m },
            _ => TeamChange::Delete { team },
        };
        let since = last_event_id(&s, h.t).await;
        let w = team_write(&s, &h, &change).await;
        assert!(changed(&w), "{label}: {w:?}");
        assert_eq!(s.record_verifier(m, h.db).await, None, "{label}: the login outlived the write");
        let revoked: Vec<AccessEvent> = s
            .events_of(h.t, "personal_logins_revoked")
            .await
            .into_iter()
            .filter(|e| e.id > since)
            .collect();
        assert_eq!(revoked.len(), 1, "{label}: {revoked:?}");
        assert_eq!(revoked[0].target_member_id, Some(m), "{label}");
    }
    assert_eq!(s.record_verifier(keep, h.db).await.as_deref(), Some(V1), "an unaffected login stays");
    s.finish().await;
}

// I4 (MH-4b): a team write whose check read is stale (the member's own grant, override or
// another team's grant changed since) is `Changed`, and nothing is written.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_team_write_refuses_a_stale_expect_with_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.active(&h, "alice").await;
    let x = s.raw_team(h.t, "x").await;
    let y = s.raw_team(h.t, "y").await;
    let dev = grant(PlatformRole::Developer, &[]);
    s.raw_team_grant(x, h.p, &dev).await;
    s.raw_team_member(x, alice).await;
    s.raw_team_member(y, alice).await;
    let change = TeamChange::PutGrant { team: x, project: h.p, grant: grant(PlatformRole::Viewer, &[]) };
    for label in ["own grant", "override", "another team's grant"] {
        let expect = c.team_change_observed(h.t, &change).await.unwrap();
        assert_eq!(
            expect.iter().map(|(m, o)| (*m, o.project)).collect::<Vec<_>>(),
            vec![(alice, h.p)],
            "{label}: the check reads alice on p"
        );
        match label {
            "own grant" => s.raw_grant(alice, h.p, &grant(PlatformRole::Admin, &[])).await,
            "override" => {
                s.raw_override(alice, h.db, &OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None })
                    .await
            }
            _ => s.raw_team_grant(y, h.p, &grant(PlatformRole::Admin, &[])).await,
        }
        let events = s.event_count(h.t).await;
        let w = c.write_team_checked(h.admin, h.t, &change, &expect).await;
        assert!(matches!(w, Err(TeamWriteError::Changed)), "{label}: {w:?}");
        assert_eq!(s.team_grant_row(x, h.p).await, Some(dev.clone()), "{label}: nothing written");
        assert_eq!(s.event_count(h.t).await, events, "{label}: no event");
    }
    s.finish().await;
}

// I4 (MH-4b): the affected set is part of the check: a member added to the team, or a team grant
// added, since the check read it is `Changed` with nothing written.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_team_write_whose_affected_set_changed_is_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let alice = s.active(&h, "alice").await;
    let bob = s.active(&h, "bob").await;
    let carol = s.active(&h, "carol").await;
    let x = s.raw_team(h.t, "x").await;
    let dev = grant(PlatformRole::Developer, &[]);
    s.raw_team_grant(x, h.p, &dev).await;
    s.raw_team_member(x, alice).await;

    let put = TeamChange::PutGrant { team: x, project: h.p, grant: grant(PlatformRole::Viewer, &[]) };
    let expect = c.team_change_observed(h.t, &put).await.unwrap();
    assert_eq!(expect.len(), 1, "the check reads alice on p: {expect:?}");
    s.raw_team_member(x, bob).await;
    let w = c.write_team_checked(h.admin, h.t, &put, &expect).await;
    assert!(matches!(w, Err(TeamWriteError::Changed)), "a member joined since the check: {w:?}");
    assert_eq!(s.team_grant_row(x, h.p).await, Some(dev.clone()));

    let add = TeamChange::AddMember { team: x, member: carol };
    let expect = c.team_change_observed(h.t, &add).await.unwrap();
    assert_eq!(expect.iter().map(|(m, o)| (*m, o.project)).collect::<Vec<_>>(), vec![(carol, h.p)]);
    s.raw_team_grant(x, q, &dev).await;
    let w = c.write_team_checked(h.admin, h.t, &add, &expect).await;
    assert!(matches!(w, Err(TeamWriteError::Changed)), "a grant was added since the check: {w:?}");
    assert!(s.memberships_of(carol).await.is_empty(), "nothing written");
    s.finish().await;
}

// P6, P7: an own-grant or override write's `Observed` includes the member's team grants, so a
// team grant changed between its check and its write is `Changed`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_own_grant_write_sees_a_team_change_as_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let bob = s.active(&h, "bob").await;
    let own = grant(PlatformRole::Viewer, &[]);
    s.raw_grant(bob, h.p, &own).await;
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    s.raw_team_member(x, bob).await;
    let observed = c.project_access(h.p).await.unwrap().observed(h.p, bob);
    assert_eq!(
        observed.teams,
        [(x, grant(PlatformRole::Viewer, &[]))].into_iter().collect(),
        "the check's read includes bob's team grant"
    );
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Developer, &["etl"])).await;
    let r = c
        .put_project_grants_checked(h.admin, bob, &[(h.p, grant(PlatformRole::Developer, &[]))], &[], std::slice::from_ref(&observed))
        .await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "grant: {r:?}");
    assert_eq!(c.member_projects(bob).await.unwrap(), vec![(h.p, own)], "nothing written");
    let narrow = OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None };
    let r = c.put_override_checked(h.admin, bob, h.db, &narrow, Some(&observed)).await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "override: {r:?}");
    // The project-level reads agree with what the write re-reads.
    let fresh: Observed = c.project_access(h.p).await.unwrap().observed(h.p, bob);
    assert_eq!(fresh.teams.get(&x), Some(&grant(PlatformRole::Developer, &["etl"])));
    s.finish().await;
}

// I4, MH-4 (T23 audit): an own-grant delete compares what its check read, team grants
// included: a team grant written since is `Changed` with nothing deleted; the read as it stands
// deletes.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_own_grant_delete_sees_a_team_change_as_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let bob = s.active(&h, "bob").await;
    let own = grant(PlatformRole::Admin, &[]);
    s.raw_grant(bob, h.p, &own).await;
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    s.raw_team_member(x, bob).await;
    let observed = c.project_access(h.p).await.unwrap().observed(h.p, bob);
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Developer, &["etl"])).await;
    let events = s.event_count(h.t).await;
    let r = c.delete_project_grant_checked(h.admin, bob, h.p, Some(&observed)).await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "{r:?}");
    assert_eq!(c.member_projects(bob).await.unwrap(), vec![(h.p, own)], "nothing deleted");
    assert_eq!(s.event_count(h.t).await, events, "no event");
    // Another project's read is `Changed` too, whatever it holds.
    let elsewhere = Observed { project: Uuid::new_v4(), ..Default::default() };
    let r = c.delete_project_grant_checked(h.admin, bob, h.p, Some(&elsewhere)).await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "{r:?}");
    let fresh = c.project_access(h.p).await.unwrap().observed(h.p, bob);
    c.delete_project_grant_checked(h.admin, bob, h.p, Some(&fresh)).await.unwrap();
    assert!(c.member_projects(bob).await.unwrap().is_empty(), "deleted");
    s.finish().await;
}

// I4, MH-4 (T23 audit): adding a member compares the team's grants as its check read them, not
// only which projects they're on: a grant changed since (same project, other roles) is
// `Changed` with nothing written.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn adding_a_member_sees_a_team_grant_change_as_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let carol = s.active(&h, "carol").await;
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    let add = TeamChange::AddMember { team: x, member: carol };
    let expect = c.team_change_observed(h.t, &add).await.unwrap();
    assert_eq!(
        expect.iter().map(|(m, o)| (*m, o.project, o.teams.get(&x).cloned())).collect::<Vec<_>>(),
        vec![(carol, h.p, Some(grant(PlatformRole::Viewer, &[])))],
        "the check's read holds what the team would give carol"
    );
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Developer, &["etl"])).await;
    let events = s.event_count(h.t).await;
    let w = c.write_team_checked(h.admin, h.t, &add, &expect).await;
    assert!(matches!(w, Err(TeamWriteError::Changed)), "the team's grant changed since the check: {w:?}");
    assert!(s.memberships_of(carol).await.is_empty(), "nothing written");
    assert_eq!(s.event_count(h.t).await, events, "no event");
    let expect = c.team_change_observed(h.t, &add).await.unwrap();
    let w = c.write_team_checked(h.admin, h.t, &add, &expect).await;
    assert!(matches!(w, Ok(TeamWrite { changed: true, .. })), "{w:?}");
    s.finish().await;
}

/// Whether a `FOR NO KEY UPDATE NOWAIT` of `table`'s row `id` gets the lock now (`false` when
/// another session holds it: 55P03).
async fn row_free(s: &Scratch, table: &str, id: Uuid) -> bool {
    let mut tx = s.client.pool().begin().await.unwrap();
    let r = sqlx::query(AssertSqlSafe(format!("SELECT 1 FROM {table} WHERE id = $1 FOR NO KEY UPDATE NOWAIT")))
        .bind(id)
        .execute(&mut *tx)
        .await;
    tx.rollback().await.unwrap();
    match r {
        Ok(_) => true,
        Err(e) if e.as_database_error().and_then(|d| d.code()).as_deref() == Some("55P03") => false,
        Err(e) => panic!("probe {table} {id}: {e}"),
    }
}

// P6 (MH-4): a team write takes the tenant row, then the affected members in id order. Another
// session holds B (the higher id): the write waits on B's holder alone, already holding the
// tenant and A. A member the write doesn't reach stays free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_writes_lock_the_tenant_then_members_in_id_order() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = s.client.clone();
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    let mut ab = [s.active(&h, "a").await, s.active(&h, "b").await];
    ab.sort();
    let [a, b] = ab;
    let other = s.active(&h, "other").await;
    for m in [a, b] {
        s.raw_team_member(x, m).await;
    }
    let change = TeamChange::PutGrant { team: x, project: h.p, grant: grant(PlatformRole::Developer, &[]) };
    let expect = c.team_change_observed(h.t, &change).await.unwrap();
    let mut hold = c.pool().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM members WHERE id = $1 FOR NO KEY UPDATE")
        .bind(b)
        .execute(&mut *hold)
        .await
        .unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *hold).await.unwrap();
    let task = tokio::spawn({
        let (c, change) = (c.clone(), change.clone());
        let (admin, t) = (h.admin, h.t);
        async move { c.write_team_checked(admin, t, &change, &expect).await }
    });
    let waiting = blocked_or_done(&s, &[], &task).await;
    if task.is_finished() {
        let r = task.await.unwrap();
        panic!("the team write didn't wait for B's row: {r:?}");
    }
    assert_eq!(waiting.len(), 1, "one waiter: {waiting:?}");
    let blockers: Vec<i32> = sqlx::query_scalar("SELECT unnest(pg_blocking_pids($1))")
        .bind(waiting[0])
        .fetch_all(c.pool())
        .await
        .unwrap();
    assert_eq!(blockers, vec![holder], "the write waits on B's holder");
    assert!(!row_free(&s, "tenants", h.t.as_uuid()).await, "the write holds the tenant while it waits for B");
    assert!(!row_free(&s, "members", a).await, "the write holds A (the lower id) before B");
    assert!(row_free(&s, "members", other).await, "a member the write doesn't reach is free");
    hold.commit().await.unwrap();
    let w = task.await.unwrap();
    assert!(changed(&w), "{w:?}");
    s.finish().await;
}

// P9, P38 (MH-7): team events carry the team, the grants concerned and the affected members;
// the member filter finds an event through `affected_member_ids` (in `after`, or `before` for a
// removal), and a member add or remove targets that member.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_events_carry_affected_members_and_the_member_filter_finds_them() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let alice = s.active(&h, "alice").await;
    let bob = s.active(&h, "bob").await;
    let carol = s.active(&h, "carol").await;
    let x = s.raw_team(h.t, "Data").await;
    s.raw_team_member(x, alice).await;
    s.raw_team_member(x, bob).await;
    let found = |events: Vec<AccessEvent>, id: i64| events.iter().any(|e| e.id == id);

    let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project: h.p, grant: grant(PlatformRole::Developer, &["app"]) }).await;
    assert!(changed(&w), "{w:?}");
    let put = s.events_of(h.t, "team_grant_put").await;
    assert_eq!(put.len(), 1, "{put:?}");
    let put = &put[0];
    let after = put.after.clone().unwrap();
    assert_eq!(after["team"], json!({ "id": x, "name": "Data" }));
    assert_eq!(after["grants"], json!([{ "project_id": h.p, "platform_role": "developer", "pg_roles": ["app"] }]));
    assert_eq!(after["affected_member_ids"], id_strings(&[alice, bob]));
    assert_eq!((put.actor_member_id, put.target_member_id, put.project_id), (Some(h.admin), None, Some(h.p)));
    assert!(found(c.access_events(h.t, None, Some(bob), None, 100).await.unwrap(), put.id), "bob's log");
    assert!(!found(c.access_events(h.t, None, Some(carol), None, 100).await.unwrap(), put.id), "carol's log");

    let w = team_write(&s, &h, &TeamChange::AddMember { team: x, member: carol }).await;
    assert!(changed(&w), "{w:?}");
    let added = s.events_of(h.t, "team_member_added").await;
    assert_eq!(added.len(), 1, "{added:?}");
    assert_eq!(added[0].target_member_id, Some(carol));
    assert!(found(c.access_events(h.t, None, Some(carol), None, 100).await.unwrap(), added[0].id));

    let w = team_write(&s, &h, &TeamChange::RemoveMember { team: x, member: alice }).await;
    assert!(changed(&w), "{w:?}");
    let removed = s.events_of(h.t, "team_member_removed").await;
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0].target_member_id, Some(alice));

    let w = team_write(&s, &h, &TeamChange::DeleteGrant { team: x, project: h.p }).await;
    assert!(changed(&w), "{w:?}");
    let deleted = s.events_of(h.t, "team_grant_deleted").await;
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0].project_id, Some(h.p));
    // Bob and carol are in the team now; alice isn't.
    for (m, want) in [(bob, true), (carol, true), (alice, false)] {
        let log = c.access_events(h.t, None, Some(m), None, 100).await.unwrap();
        assert_eq!(found(log, deleted[0].id), want, "member {m}'s log and the grant delete");
    }
    s.finish().await;
}

// P9 (MH-7): the project log shows a team's grant events on that project (through `project_id`
// or `grants`) and a member added to a team that grants it, never another project's grant event.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_grant_events_are_in_the_project_log() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let bob = s.active(&h, "bob").await;
    let x = s.raw_team(h.t, "x").await;
    for project in [h.p, q] {
        let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project, grant: grant(PlatformRole::Viewer, &[]) }).await;
        assert!(changed(&w), "{w:?}");
    }
    let w = team_write(&s, &h, &TeamChange::AddMember { team: x, member: bob }).await;
    assert!(changed(&w), "{w:?}");
    let log = c.access_events(h.t, Some(h.p), None, None, 100).await.unwrap();
    let puts: Vec<Option<Uuid>> = log.iter().filter(|e| e.action == "team_grant_put").map(|e| e.project_id).collect();
    assert_eq!(puts, vec![Some(h.p)], "p's log: its team grant, not q's");
    assert!(log.iter().any(|e| e.action == "team_member_added"), "p's log: bob joined a team granting p");
    s.finish().await;
}

// P10 (MH-5): removing a member deletes its team memberships in the same transaction, its
// touched projects include its teams' projects, and `member_removed` lists the teams.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn remove_member_drops_team_memberships_and_touches_team_projects() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let alice = s.active(&h, "alice").await;
    let x = s.raw_team(h.t, "Ops").await;
    s.raw_team_grant(x, q, &grant(PlatformRole::Developer, &[])).await;
    s.raw_team_member(x, alice).await;
    let touched = c.remove_member(h.admin, alice).await.unwrap();
    assert!(touched.contains(&q), "the team's project is touched: {touched:?}");
    assert!(s.memberships_of(alice).await.is_empty(), "memberships deleted");
    let removed = s.events_of(h.t, "member_removed").await;
    assert_eq!(removed.last().and_then(|e| e.before.clone()).map(|b| b["teams"].clone()), Some(json!([{ "id": x, "name": "Ops" }])));
    assert!(s.events_of(h.t, "team_member_removed").await.is_empty(), "no separate team events (P10)");
    s.finish().await;
}

// P16: reinstate returns the touched projects, team grant projects included (also on the
// already-active no-op); suspend's include them too.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn reinstate_returns_team_projects() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let alice = s.granted(&h, "alice", PlatformRole::Viewer).await;
    let x = s.raw_team(h.t, "x").await;
    s.raw_team_grant(x, q, &grant(PlatformRole::Developer, &[])).await;
    s.raw_team_member(x, alice).await;
    let suspended = c.suspend_member(h.admin, alice).await.unwrap();
    let reinstated = c.reinstate_member(h.admin, alice).await.unwrap();
    assert_eq!(sorted(reinstated.clone()), sorted(vec![h.p, q]), "own and team projects");
    assert_eq!(sorted(c.reinstate_member(h.admin, alice).await.unwrap()), sorted(vec![h.p, q]), "the no-op too");
    assert!(suspended.contains(&q), "suspend: {suspended:?}");
    s.finish().await;
}

// P2: team names are unique per tenant, case-insensitively, on create and rename.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_names_are_unique_per_tenant_case_insensitively() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let u = s.hub().await;
    let c = &s.client;
    let data = c.create_team(h.admin, h.t, "Data", "the data team").await;
    assert!(data.is_ok(), "create: {data:?}");
    let data = data.unwrap();
    assert_eq!((data.name.as_str(), data.description.as_str(), data.tenant_id), ("Data", "the data team", h.t));
    let r = c.create_team(h.admin, h.t, "data", "").await;
    assert!(matches!(r, Err(TeamWriteError::NameTaken)), "{r:?}");
    let r = c.create_team(u.admin, u.t, "DATA", "").await;
    assert!(r.is_ok(), "another tenant may use the name: {r:?}");
    let ops = c.create_team(h.admin, h.t, "Ops", "").await.unwrap();
    let r = c.update_team(h.admin, h.t, ops.id, Some("dAtA"), None).await;
    assert!(matches!(r, Err(TeamWriteError::NameTaken)), "rename: {r:?}");
    let r = c.update_team(h.admin, h.t, ops.id, Some("OPS"), Some("ops")).await.unwrap();
    assert_eq!((r.name.as_str(), r.description.as_str()), ("OPS", "ops"), "a team may change its own name's case");
    assert_eq!(s.events_of(h.t, "team_created").await.len(), 2);
    assert_eq!(s.events_of(h.t, "team_updated").await.len(), 1);
    s.finish().await;
}

// P17: a team grant on another tenant's project, or on a deleted one, is `ProjectNotFound`;
// another tenant's team is `NotFound`. Nothing is written.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_team_grant_on_another_tenants_project_is_project_not_found() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let u = s.hub().await;
    let gone = s.project(h.t).await;
    s.delete_project(gone).await;
    let x = s.raw_team(h.t, "x").await;
    let v = grant(PlatformRole::Viewer, &[]);
    for (label, project) in [("another tenant's project", u.p), ("a deleted project", gone)] {
        let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project, grant: v.clone() }).await;
        assert!(matches!(w, Err(TeamWriteError::ProjectNotFound)), "{label}: {w:?}");
        assert_eq!(s.team_grant_row(x, project).await, None, "{label}");
    }
    let theirs = s.raw_team(u.t, "theirs").await;
    let w = team_write(&s, &h, &TeamChange::PutGrant { team: theirs, project: h.p, grant: v }).await;
    assert!(matches!(w, Err(TeamWriteError::NotFound)), "another tenant's team: {w:?}");
    assert_eq!(s.team_grant_row(theirs, h.p).await, None);
    s.finish().await;
}

// P17: a pending sign-up can't join a team (`MemberPending`); a removed member or another
// tenant's is `MemberNotFound`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn adding_a_pending_member_is_member_pending() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let u = s.hub().await;
    let c = &s.client;
    let x = s.raw_team(h.t, "x").await;
    let pending = c.ensure_member_first_comer_for_tests(h.t, "pend", "pend@x.com", "P", Admission::Pending).await.unwrap().id;
    let w = team_write(&s, &h, &TeamChange::AddMember { team: x, member: pending }).await;
    assert!(matches!(w, Err(TeamWriteError::MemberPending)), "pending: {w:?}");
    let removed = s.active(&h, "gone").await;
    c.remove_member(h.admin, removed).await.unwrap();
    let stranger = s.active(&u, "stranger").await;
    for (label, member) in [("removed", removed), ("another tenant's", stranger)] {
        let w = team_write(&s, &h, &TeamChange::AddMember { team: x, member }).await;
        assert!(matches!(w, Err(TeamWriteError::MemberNotFound)), "{label}: {w:?}");
    }
    for m in [pending, removed, stranger] {
        assert!(s.memberships_of(m).await.is_empty());
    }
    s.finish().await;
}

// P3 (R17, R7): a team grant is written as a grant is: Admin names no roles, roles below Admin
// sorted and deduplicated, `owner` refused (`Conflict`).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_admin_team_grant_names_no_roles() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let q = s.project(h.t).await;
    let x = s.raw_team(h.t, "x").await;
    let admin = GrantRow { platform_role: PlatformRole::Admin, pg_roles: vec!["app".into()] };
    let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project: h.p, grant: admin }).await;
    assert!(changed(&w), "{w:?}");
    assert_eq!(s.team_grant_row(x, h.p).await, Some(grant(PlatformRole::Admin, &[])), "Admin names no roles");
    let messy = GrantRow { platform_role: PlatformRole::Developer, pg_roles: vec!["b".into(), "a".into(), "b".into()] };
    let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project: q, grant: messy }).await;
    assert!(changed(&w), "{w:?}");
    assert_eq!(s.team_grant_row(x, q).await, Some(grant(PlatformRole::Developer, &["a", "b"])));
    let owner = grant(PlatformRole::Developer, &["owner"]);
    let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project: q, grant: owner }).await;
    assert!(matches!(w, Err(TeamWriteError::Conflict(_))), "owner: {w:?}");
    assert_eq!(s.team_grant_row(x, q).await, Some(grant(PlatformRole::Developer, &["a", "b"])));
    // The same grant again commits nothing.
    let w = team_write(&s, &h, &TeamChange::PutGrant { team: x, project: q, grant: grant(PlatformRole::Developer, &["a", "b"]) }).await;
    assert!(matches!(w, Ok(TeamWrite { changed: false, .. })), "unchanged: {w:?}");
    s.finish().await;
}

// P12, P25 (metadata half): every invite path with teams adds the memberships in the grants'
// transaction, one `team_member_added` (targeting the member) per team; a refused write adds
// none.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn invites_and_approvals_add_teams_in_the_same_transaction() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let x = s.raw_team(h.t, "x").await;
    let y = s.raw_team(h.t, "y").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &[])).await;
    s.raw_team_grant(y, q, &grant(PlatformRole::Developer, &[])).await;
    let sx = TeamSeen { team: x, grants: vec![(h.p, grant(PlatformRole::Viewer, &[]))] };
    let sy = TeamSeen { team: y, grants: vec![(q, grant(PlatformRole::Developer, &[]))] };
    let joined = |since: i64, member: Uuid| {
        let s = &s;
        async move {
            s.events_of(h.t, "team_member_added")
                .await
                .into_iter()
                .filter(|e| e.id > since && e.target_member_id == Some(member))
                .count()
        }
    };

    let since = last_event_id(&s, h.t).await;
    let teams = [sx.clone()];
    let (invited, _) = c
        .create_invited_member(h.admin, h.t, NewInvite { teams: &teams, ..new_invite("zi", "i@x.com", &[]) })
        .await
        .unwrap();
    assert_eq!(s.memberships_of(invited.id).await, vec![x], "the invitee joins x");
    assert_eq!(joined(since, invited.id).await, 1);

    let since = last_event_id(&s, h.t).await;
    let added = c.add_existing_user(h.admin, h.t, "ze", "e@x.com", "E", &[], &[sx.clone(), sy.clone()]).await.unwrap();
    assert_eq!(s.memberships_of(added.id).await, sorted(vec![x, y]));
    assert_eq!(joined(since, added.id).await, 2);

    let since = last_event_id(&s, h.t).await;
    let pending = c.ensure_member_first_comer_for_tests(h.t, "pend", "pend@x.com", "P", Admission::Pending).await.unwrap();
    c.approve_member(h.admin, pending.id, &[], std::slice::from_ref(&sy)).await.unwrap();
    assert_eq!(s.memberships_of(pending.id).await, vec![y]);
    assert_eq!(joined(since, pending.id).await, 1);

    let since = last_event_id(&s, h.t).await;
    let bob = s.active(&h, "bob").await;
    let expect = [c.project_access(h.p).await.unwrap().observed(h.p, bob)];
    c.put_project_grants_checked(h.admin, bob, &[], std::slice::from_ref(&sx), &expect).await.unwrap();
    assert_eq!(s.memberships_of(bob).await, vec![x]);
    assert_eq!(joined(since, bob).await, 1);

    // A refused approval (bob isn't pending) adds no membership.
    let r = c.approve_member(h.admin, bob, &[], std::slice::from_ref(&sy)).await;
    assert!(matches!(r, Err(AccessWriteError::Conflict(_))), "{r:?}");
    assert_eq!(s.memberships_of(bob).await, vec![x]);
    s.finish().await;
}

// P27, the checklist: a deleted project's team grant is no access in any read, and a suspended
// or offboarded member reaches nothing through a team.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_deleted_projects_team_grant_is_no_access() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let gone = s.project(h.t).await;
    let gone_db = s.database(h.t, gone).await;
    let alice = s.active(&h, "alice").await;
    let x = s.raw_team(h.t, "x").await;
    let v = grant(PlatformRole::Viewer, &[]);
    let d = grant(PlatformRole::Developer, &[]);
    s.raw_team_grant(x, h.p, &v).await;
    s.raw_team_grant(x, gone, &d).await;
    s.raw_team_member(x, alice).await;
    s.delete_project(gone).await;

    assert_eq!(
        c.grant_sources(alice, h.p, None).await.unwrap(),
        GrantSources::new(None, vec![v.to_grant()], None),
        "a live project's team grant counts"
    );
    assert_eq!(c.grant_sources(alice, gone, Some(gone_db)).await.unwrap(), GrantSources::default());
    assert_eq!(c.member_grant_sources(h.t, alice).await.unwrap(), vec![(h.p, GrantSources::new(None, vec![v.to_grant()], None))]);
    assert!(!c.database_grant_sources(gone, gone_db).await.unwrap().iter().any(|(m, _)| *m == alice));
    assert!(!s.sql_qualifies(alice, gone_db).await);
    assert!(!c.member_project_roles(h.t).await.unwrap().iter().any(|r| r.project == gone));

    // Through a Developer team on p: an active member qualifies, a suspended or offboarded one
    // doesn't.
    let y = s.raw_team(h.t, "y").await;
    s.raw_team_grant(y, h.p, &d).await;
    let dave = s.active(&h, "dave").await;
    let bob = s.active(&h, "bob").await;
    let carol = s.active(&h, "carol").await;
    for m in [dave, bob, carol] {
        s.raw_team_member(y, m).await;
    }
    sqlx::query("UPDATE members SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(bob)
        .execute(c.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE members SET offboarded_at = now() WHERE id = $1").bind(carol).execute(c.pool()).await.unwrap();
    assert!(s.sql_qualifies(dave, h.db).await, "an active Developer through y");
    assert_eq!(s.level(bob, h.db).await, None, "suspended");
    assert!(!s.sql_qualifies(bob, h.db).await, "suspended");
    assert!(!s.sql_qualifies(carol, h.db).await, "offboarded");
    s.finish().await;
}

// P9: deleting a team cascades its grants and memberships and logs exactly one event, which
// lists what the team gave and whom it reached.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn deleting_a_team_cascades_and_logs_one_event() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let alice = s.active(&h, "alice").await;
    let bob = s.active(&h, "bob").await;
    let x = s.raw_team(h.t, "Gone").await;
    s.raw_team_grant(x, h.p, &grant(PlatformRole::Viewer, &["app"])).await;
    s.raw_team_member(x, alice).await;
    s.raw_team_member(x, bob).await;
    let since = last_event_id(&s, h.t).await;
    let w = team_write(&s, &h, &TeamChange::Delete { team: x }).await;
    assert!(changed(&w), "{w:?}");
    assert_eq!(actions_since(&s, h.t, since).await, vec!["team_deleted".to_string()]);
    for table in ["teams", "team_grants", "team_members"] {
        assert_eq!(s.team_rows(table, x).await, 0, "{table}");
    }
    let e = s.events_of(h.t, "team_deleted").await.pop().unwrap();
    let before = e.before.unwrap();
    assert_eq!(before["team"], json!({ "id": x, "name": "Gone" }));
    assert_eq!(before["grants"], json!([{ "project_id": h.p, "platform_role": "viewer", "pg_roles": ["app"] }]));
    assert_eq!(before["affected_member_ids"], id_strings(&[alice, bob]));
    assert_eq!(e.target_member_id, None);
    s.finish().await;
}

// Selfhost-install m3: the fresh-install claim is bound to the account a sign-up with the
// setup code created (`setup_claims`); `admit_member` is what the orchestrator runs.

/// A fresh setup-claim nonce hash for `t`, live for five minutes.
async fn setup_nonce(s: &Scratch, t: TenantId) -> [u8; 32] {
    let hash = *Uuid::new_v4().as_bytes();
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(&hash);
    h[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    s.client
        .insert_setup_nonce(t, h, std::time::Duration::from_secs(300))
        .await
        .unwrap();
    h
}

async fn setup_claim_rows(s: &Scratch, t: TenantId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM setup_claims WHERE tenant_id = $1")
        .bind(t.as_uuid())
        .fetch_one(s.client.pool())
        .await
        .unwrap()
}

fn not_admitted(r: Result<chelabase_metadata::members::MemberRecord, chelabase_metadata::MetadataError>) -> bool {
    matches!(r, Err(chelabase_metadata::MetadataError::NotAdmitted(_)))
}

// AC 5 (metadata side): without a claim row, nobody claims the admin of a member-less tenant.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_none_means_no_admin() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    assert!(not_admitted(s.client.admit_member(t, "first", "f@x.com", "F", Admission::Closed).await));
    let p = s.client.admit_member(t, "second", "s@x.com", "S", Admission::Pending).await.unwrap();
    assert_eq!((p.status, p.tenant_admin), (MemberStatus::Pending, false));
    let a = s.client.admit_member(t, "third", "t@x.com", "T", Admission::Active).await.unwrap();
    assert_eq!((a.status, a.tenant_admin), (MemberStatus::Active, false));
    assert!(s.client.active_tenant_admins(t).await.unwrap().is_empty());
    assert!(s.client.access_events(t, None, None, None, 10).await.unwrap().iter().all(|e| e.action != "admin_claimed"));
    s.finish().await;
}

// AC 3: B, another user of the org, asks first: not admin; A, the claim's user, still is.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_admit_bound_to_setup_claim_user() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let n = setup_nonce(&s, t).await;
    assert_eq!(
        s.client.claim_setup(t, n, "user-a", None).await.unwrap(),
        chelabase_metadata::members::SetupClaimOutcome::Bound
    );
    assert!(not_admitted(s.client.admit_member(t, "user-b", "b@x.com", "B", Admission::Closed).await));
    let b = s.client.admit_member(t, "user-b", "b@x.com", "B", Admission::Pending).await.unwrap();
    assert_eq!((b.status, b.tenant_admin), (MemberStatus::Pending, false));
    let c = s.client.admit_member(t, "machine", "", "M", Admission::Active).await.unwrap();
    assert!(!c.tenant_admin, "an Active admission is no claim either");
    let a = s.client.admit_member(t, "user-a", "a@x.com", "A", Admission::Closed).await.unwrap();
    assert_eq!((a.status, a.tenant_admin), (MemberStatus::Active, true));
    assert_eq!(s.last_action(t).await, "admin_claimed");
    let admins = s.client.active_tenant_admins(t).await.unwrap();
    assert_eq!(admins.iter().map(|m| m.zitadel_user_id.as_str()).collect::<Vec<_>>(), vec!["user-a"]);
    s.finish().await;
}

// AC 9: a claimed tenant whose only active admin is suspended: nobody else is made admin, an
// existing member or a newcomer; the claim's own user (suspended) stays suspended.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_no_active_admin_requires_bound_user() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let n = setup_nonce(&s, t).await;
    s.client.claim_setup(t, n, "user-a", None).await.unwrap();
    let a = s.client.admit_member(t, "user-a", "a@x.com", "A", Admission::Closed).await.unwrap();
    assert!(a.tenant_admin);
    let m = s.client.add_existing_user(a.id, t, "user-m", "m@x.com", "M", &[], &[]).await.unwrap();
    sqlx::query("UPDATE members SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(a.id)
        .execute(s.client.pool())
        .await
        .unwrap();
    assert!(s.client.active_tenant_admins(t).await.unwrap().is_empty());
    let again = s.client.admit_member(t, "user-m", "m2@x.com", "M2", Admission::Active).await.unwrap();
    assert_eq!((again.id, again.tenant_admin), (m.id, false));
    for admission in [Admission::Active, Admission::Pending] {
        let c = s.client.admit_member(t, &format!("new-{admission:?}"), "", "C", admission).await.unwrap();
        assert!(!c.tenant_admin, "{admission:?}");
    }
    assert!(not_admitted(s.client.admit_member(t, "new-closed", "", "C", Admission::Closed).await));
    let back = s.client.admit_member(t, "user-a", "a@x.com", "A", Admission::Active).await.unwrap();
    assert_eq!((back.status, back.tenant_admin), (MemberStatus::Suspended, true));
    assert!(s.client.active_tenant_admins(t).await.unwrap().is_empty());
    s.finish().await;
}

// The nonce AC (metadata side): no nonce, another tenant's, an expired one and a reused one
// are refused and write no claim.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_nonce_refusals() {
    use chelabase_metadata::members::SetupClaimOutcome::{Bound, Refused};
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let other = s.tenant().await;
    assert_eq!(s.client.claim_setup(t, [9; 32], "u1", None).await.unwrap(), Refused);
    let foreign = setup_nonce(&s, other).await;
    assert_eq!(s.client.claim_setup(t, foreign, "u1", None).await.unwrap(), Refused);
    let expired = setup_nonce(&s, t).await;
    sqlx::query("UPDATE setup_claim_nonces SET created_at = now() - interval '10 minutes', expires_at = now() - interval '1 second' WHERE nonce_hash = $1")
        .bind(&expired[..])
        .execute(s.client.pool())
        .await
        .unwrap();
    assert_eq!(s.client.claim_setup(t, expired, "u1", None).await.unwrap(), Refused);
    assert_eq!(setup_claim_rows(&s, t).await, 0);
    assert_eq!(setup_claim_rows(&s, other).await, 0);
    // The other tenant's nonce is still its own.
    assert_eq!(s.client.claim_setup(other, foreign, "o1", None).await.unwrap(), Bound);
    let n = setup_nonce(&s, t).await;
    assert_eq!(s.client.claim_setup(t, n, "u1", None).await.unwrap(), Bound);
    assert_eq!(s.client.claim_setup(t, n, "u2", None).await.unwrap(), Refused, "reused");
    assert_eq!(s.client.setup_claim(t).await.unwrap().unwrap().zitadel_user_id, "u1");
    assert_eq!(setup_claim_rows(&s, t).await, 1);
    s.finish().await;
}

// AC 6: two sign-ups with the right code at once: exactly one claim row; whichever order the
// first requests come in, only the bound user is admin and the other waits as pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_race() {
    use chelabase_metadata::members::SetupClaimOutcome::{AlreadyClaimed, Bound};
    let s = Scratch::new().await;
    for round in 0..8 {
        let t = s.tenant().await;
        let (na, nb) = (setup_nonce(&s, t).await, setup_nonce(&s, t).await);
        let (ca, cb) = (s.client.clone(), s.client.clone());
        let ja = tokio::spawn(async move { ca.claim_setup(t, na, "user-a", None).await.unwrap() });
        let jb = tokio::spawn(async move { cb.claim_setup(t, nb, "user-b", None).await.unwrap() });
        let (oa, ob) = (ja.await.unwrap(), jb.await.unwrap());
        let mut outcomes = vec![oa, ob];
        outcomes.sort_by_key(|o| *o == Bound);
        assert_eq!(outcomes, vec![AlreadyClaimed, Bound], "round {round}");
        assert_eq!(setup_claim_rows(&s, t).await, 1);
        let winner = if oa == Bound { "user-a" } else { "user-b" };
        assert_eq!(s.client.setup_claim(t).await.unwrap().unwrap().zitadel_user_id, winner);
        let (ma, mb) = (s.client.clone(), s.client.clone());
        let ra = tokio::spawn(async move { ma.admit_member(t, "user-a", "a@x.com", "A", Admission::Pending).await.unwrap() });
        let rb = tokio::spawn(async move { mb.admit_member(t, "user-b", "b@x.com", "B", Admission::Pending).await.unwrap() });
        let members = [ra.await.unwrap(), rb.await.unwrap()];
        for m in &members {
            let bound = m.zitadel_user_id == winner;
            assert_eq!(m.tenant_admin, bound, "round {round}: {m:?}");
            let want = if bound { MemberStatus::Active } else { MemberStatus::Pending };
            assert_eq!(m.status, want, "round {round}");
        }
    }
    s.finish().await;
}

// AC 11: a claim whose user is gone (no member row; Zitadel checked by the caller) is rebound
// by a new code-gated sign-up, which then becomes admin. A claim whose user has a member row,
// or another user than the caller checked, is not replaced.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_stale_replaced() {
    use chelabase_metadata::members::SetupClaimOutcome::{AlreadyClaimed, Bound};
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let n = setup_nonce(&s, t).await;
    assert_eq!(s.client.claim_setup(t, n, "gone", None).await.unwrap(), Bound);
    let claim = s.client.setup_claim(t).await.unwrap().unwrap();
    assert_eq!((claim.zitadel_user_id.as_str(), claim.has_member_row), ("gone", false));
    let n = setup_nonce(&s, t).await;
    assert_eq!(s.client.claim_setup(t, n, "new", Some("someone-else")).await.unwrap(), AlreadyClaimed);
    let n = setup_nonce(&s, t).await;
    assert_eq!(s.client.claim_setup(t, n, "new", Some("gone")).await.unwrap(), Bound);
    assert_eq!(s.client.setup_claim(t).await.unwrap().unwrap().zitadel_user_id, "new");
    let replaced: Option<String> = sqlx::query_scalar("SELECT replaced_user_id FROM setup_claims WHERE tenant_id = $1")
        .bind(t.as_uuid())
        .fetch_one(s.client.pool())
        .await
        .unwrap();
    assert_eq!(replaced.as_deref(), Some("gone"));
    assert!(not_admitted(s.client.admit_member(t, "gone", "", "G", Admission::Closed).await));
    let m = s.client.admit_member(t, "new", "n@x.com", "N", Admission::Closed).await.unwrap();
    assert!(m.tenant_admin);
    // Now the bound user has a row (and is admin): nothing replaces the claim.
    assert!(s.client.setup_claim(t).await.unwrap().unwrap().has_member_row);
    let n = setup_nonce(&s, t).await;
    assert_eq!(s.client.claim_setup(t, n, "third", Some("new")).await.unwrap(), AlreadyClaimed);
    assert_eq!(s.client.setup_claim(t).await.unwrap().unwrap().zitadel_user_id, "new");
    s.finish().await;
}

// A tenant that already has an active admin (bootstrapped, say) takes no claim at all.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_refused_once_an_admin_exists() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    s.client.bootstrap_admin(t, "boot", "boot@x.com").await.unwrap();
    let n = setup_nonce(&s, t).await;
    assert_eq!(
        s.client.claim_setup(t, n, "late", None).await.unwrap(),
        chelabase_metadata::members::SetupClaimOutcome::AlreadyClaimed
    );
    assert_eq!(setup_claim_rows(&s, t).await, 0);
    let late = s.client.admit_member(t, "late", "", "L", Admission::Active).await.unwrap();
    assert!(!late.tenant_admin);
    s.finish().await;
}

// AC 6 (reworded after the m3 re-audit, B3): the race window opens at the bound user's first
// request (`admin_claimed`), not when the claim is bound. Before that, a newcomer is refused.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_window_opens_at_the_first_admin_claim() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let hour_ago = Utc::now() - Duration::hours(1);
    let n = setup_nonce(&s, t).await;
    s.client.claim_setup(t, n, "user-a", None).await.unwrap();
    assert!(!s.client.admin_claimed_since(t, hour_ago).await.unwrap(), "binding opens no window");
    let a = s.client.admit_member(t, "user-a", "a@x.com", "A", Admission::Closed).await.unwrap();
    assert!(a.tenant_admin);
    assert!(s.client.admin_claimed_since(t, hour_ago).await.unwrap(), "A's first request opens it");
    s.finish().await;
}

// B3 (m3 re-audit): a bystander's request inside the hour after the claim is bound, before A's
// first request, writes nothing. So the gate stays open, and once A is gone the stale claim can
// still be replaced (AC 11): no lockout.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_bystander_never_blocks_stale_replacement() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let n = setup_nonce(&s, t).await;
    s.client.claim_setup(t, n, "user-a", None).await.unwrap();
    // The orchestrator's `admit`, step by step, for bystander B: refused at `Closed`, and the
    // window that would make B pending is shut, so nothing is written.
    assert!(not_admitted(s.client.admit_member(t, "user-b", "b@x.com", "B", Admission::Closed).await));
    assert!(
        !s.client.admin_claimed_since(t, Utc::now() - Duration::hours(1)).await.unwrap(),
        "no window before A's first request: B is not made pending"
    );
    assert_eq!(s.member_count(t).await, 0);
    assert!(!s.client.tenant_has_members(t).await.unwrap(), "the gate stays open");
    // A never signed in and is gone from Zitadel: a new code-gated sign-up replaces the claim.
    assert!(!s.client.setup_claim(t).await.unwrap().unwrap().has_member_row);
    let n = setup_nonce(&s, t).await;
    assert_eq!(
        s.client.claim_setup(t, n, "user-c", Some("user-a")).await.unwrap(),
        chelabase_metadata::members::SetupClaimOutcome::Bound
    );
    let c = s.client.admit_member(t, "user-c", "c@x.com", "C", Admission::Closed).await.unwrap();
    assert!(c.tenant_admin);
    s.finish().await;
}

// N3 (m3 audit), the "removed" half of AC 9: the sole admin can't be removed
// (`last_tenant_admin`); and should their row end up removed anyway (by hand), nobody else is
// made admin: neither an existing member nor a newcomer, and the removed admin stays removed.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn setup_claim_removed_admin_needs_bound_user() {
    let s = Scratch::new().await;
    let t = s.tenant().await;
    let n = setup_nonce(&s, t).await;
    s.client.claim_setup(t, n, "user-a", None).await.unwrap();
    let a = s.client.admit_member(t, "user-a", "a@x.com", "A", Admission::Closed).await.unwrap();
    assert!(a.tenant_admin);
    let m = s.client.add_existing_user(a.id, t, "user-m", "m@x.com", "M", &[], &[]).await.unwrap();
    assert!(matches!(s.client.remove_member(m.id, a.id).await, Err(AccessWriteError::LastTenantAdmin)));
    sqlx::query("UPDATE members SET status = 'removed' WHERE id = $1")
        .bind(a.id)
        .execute(s.client.pool())
        .await
        .unwrap();
    assert!(s.client.active_tenant_admins(t).await.unwrap().is_empty());
    let again = s.client.admit_member(t, "user-m", "m@x.com", "M", Admission::Active).await.unwrap();
    assert_eq!((again.id, again.tenant_admin), (m.id, false));
    for admission in [Admission::Active, Admission::Pending] {
        let c = s.client.admit_member(t, &format!("new-{admission:?}"), "", "C", admission).await.unwrap();
        assert!(!c.tenant_admin, "{admission:?}");
    }
    let back = s.client.admit_member(t, "user-a", "a@x.com", "A", Admission::Active).await.unwrap();
    assert_eq!(back.status, MemberStatus::Removed);
    assert!(s.client.active_tenant_admins(t).await.unwrap().is_empty());
    s.finish().await;
}
