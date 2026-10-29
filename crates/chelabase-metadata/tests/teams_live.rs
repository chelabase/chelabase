//! Members hub step 2, T1: the coder's edge tests for teams, beyond the RED set in
//! `members_live.rs` (team reads, unchanged writes, name bounds, invite edges, the check's read
//! and what a write reports). Each test makes its own scratch database on the workbench stack's
//! metadata Postgres.

use chelabase_metadata::members::{AccessWriteError, Admission, GrantRow, NewInvite, TeamSeen};
use chelabase_metadata::teams::{TeamChange, TeamWrite, TeamWriteError};
use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_types::access::PlatformRole;
use chelabase_types::{DatabaseId, ProjectId, TenantId};
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

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

/// A scratch database `cm_<12 hex>` with every metadata migration applied (the name pattern
/// `members_live`'s sweep drops when stale).
struct Scratch {
    meta: PgPool,
    name: String,
    client: MetadataClient,
}

/// A tenant with an active tenant admin, one project and one database in it.
struct Hub {
    t: TenantId,
    admin: Uuid,
    p: Uuid,
}

impl Scratch {
    async fn new() -> Scratch {
        let meta = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url())
            .await
            .expect("metadata Postgres reachable (is the workbench stack up?)");
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
            .bootstrap_admin(t, &format!("admin-{id}"), "")
            .await
            .unwrap()
            .id;
        let p = self.project(t).await;
        self.client
            .insert_database_pending(DatabaseId::new(), t, ProjectId(p), &format!("d-{p}"))
            .await
            .unwrap();
        Hub { t, admin, p }
    }

    async fn active(&self, h: &Hub, name: &str) -> Uuid {
        self.client
            .ensure_member_first_comer_for_tests(h.t, name, "", name, Admission::Active)
            .await
            .unwrap()
            .id
    }

    async fn delete_project(&self, project: Uuid) {
        sqlx::query("UPDATE projects SET status = 'deleted' WHERE id = $1")
            .bind(project)
            .execute(self.client.pool())
            .await
            .unwrap();
    }

    async fn event_count(&self, t: TenantId) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM access_events WHERE tenant_id = $1")
            .bind(t.as_uuid())
            .fetch_one(self.client.pool())
            .await
            .unwrap()
    }

    async fn memberships_of(&self, member: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT team_id FROM team_members WHERE member_id = $1 ORDER BY team_id")
            .bind(member)
            .fetch_all(self.client.pool())
            .await
            .unwrap()
    }

    async fn zitadel_of(&self, member: Uuid) -> String {
        self.client
            .member(member)
            .await
            .unwrap()
            .unwrap()
            .zitadel_user_id
    }
}

fn grant(role: PlatformRole, roles: &[&str]) -> GrantRow {
    GrantRow {
        platform_role: role,
        pg_roles: roles.iter().map(|s| s.to_string()).collect(),
    }
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v
}

/// `change` as a checked team write, with the check's read taken just before (I4).
async fn team_write(
    s: &Scratch,
    h: &Hub,
    change: &TeamChange,
) -> Result<TeamWrite, TeamWriteError> {
    let expect = s.client.team_change_observed(h.t, change).await?;
    s.client
        .write_team_checked(h.admin, h.t, change, &expect)
        .await
}

// P11, P14, P15 (the metadata half): the team reads order by lower(name), list only grants on
// non-deleted projects of the team's tenant and the team's members, and never answer another
// tenant's team.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_reads_list_grants_and_members_by_name() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let u = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let gone = s.project(h.t).await;
    let alice = s.active(&h, "alice").await;
    let bob = s.active(&h, "bob").await;
    let beta = c.create_team(h.admin, h.t, "beta", "").await.unwrap();
    let alpha = c
        .create_team(h.admin, h.t, "Alpha", "the first")
        .await
        .unwrap();
    let theirs = c.create_team(u.admin, u.t, "aaa", "").await.unwrap();
    let v = grant(PlatformRole::Viewer, &["app"]);
    let d = grant(PlatformRole::Developer, &[]);
    for change in [
        TeamChange::PutGrant {
            team: alpha.id,
            project: h.p,
            grant: v.clone(),
        },
        TeamChange::PutGrant {
            team: alpha.id,
            project: gone,
            grant: d.clone(),
        },
        TeamChange::PutGrant {
            team: beta.id,
            project: h.p,
            grant: d.clone(),
        },
        TeamChange::PutGrant {
            team: beta.id,
            project: q,
            grant: d.clone(),
        },
        TeamChange::AddMember {
            team: alpha.id,
            member: bob,
        },
        TeamChange::AddMember {
            team: alpha.id,
            member: alice,
        },
        TeamChange::AddMember {
            team: beta.id,
            member: alice,
        },
    ] {
        let w = team_write(&s, &h, &change).await;
        assert!(
            matches!(w, Ok(TeamWrite { changed: true, .. })),
            "{change:?}: {w:?}"
        );
    }
    s.delete_project(gone).await;

    let teams = c.list_teams(h.t).await.unwrap();
    let names: Vec<&str> = teams.iter().map(|d| d.team.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Alpha", "beta"],
        "by lower(name), another tenant's team absent"
    );
    assert_eq!(
        teams[0].grants,
        vec![(h.p, v.clone())],
        "Alpha: the deleted project's grant is hidden"
    );
    assert_eq!(teams[0].member_ids, sorted(vec![alice, bob]));
    assert_eq!(teams[0].team.description, "the first");
    let mut beta_grants = vec![(h.p, d.clone()), (q, d.clone())];
    beta_grants.sort_by_key(|(p, _)| *p);
    assert_eq!(teams[1].grants, beta_grants);

    assert_eq!(c.team(h.t, alpha.id).await.unwrap(), Some(teams[0].clone()));
    assert_eq!(
        c.team(h.t, theirs.id).await.unwrap(),
        None,
        "another tenant's team"
    );
    assert_eq!(c.team(h.t, Uuid::new_v4()).await.unwrap(), None);

    let alices: Vec<Uuid> = c
        .member_teams(alice)
        .await
        .unwrap()
        .iter()
        .map(|d| d.team.id)
        .collect();
    assert_eq!(alices, vec![alpha.id, beta.id]);
    let bobs: Vec<Uuid> = c
        .member_teams(bob)
        .await
        .unwrap()
        .iter()
        .map(|d| d.team.id)
        .collect();
    assert_eq!(bobs, vec![alpha.id]);

    let on_p = c.project_team_grants(h.p).await.unwrap();
    assert_eq!(
        on_p.iter()
            .map(|g| (g.name.as_str(), g.grant.clone(), g.member_ids.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("Alpha", v.clone(), sorted(vec![alice, bob])),
            ("beta", d.clone(), vec![alice])
        ]
    );
    assert!(
        c.project_team_grants(gone).await.unwrap().is_empty(),
        "a deleted project has none"
    );

    // `ProjectAccess.teams`: by team then member, and each member's `Observed` gets its own.
    let access = c.project_access(h.p).await.unwrap();
    let mut want = vec![
        (alpha.id, alice, v.clone()),
        (alpha.id, bob, v.clone()),
        (beta.id, alice, d.clone()),
    ];
    want.sort_by_key(|(t, m, _)| (*t, *m));
    assert_eq!(access.teams, want);
    assert_eq!(
        access.observed(h.p, alice).teams,
        [(alpha.id, v.clone()), (beta.id, d.clone())]
            .into_iter()
            .collect()
    );
    s.finish().await;
}

// P2, P17: a name is trimmed and 1–80 characters, a description at most 500 (`Conflict`); an
// unchanged write commits nothing and logs nothing (member already in or not in, grant
// already absent, same name and description); another tenant's team is `NotFound`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn team_bounds_and_unchanged_writes() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let u = s.hub().await;
    let c = &s.client;
    let x = c.create_team(h.admin, h.t, "  Ops  ", "").await.unwrap();
    assert_eq!(x.name, "Ops", "trimmed");
    assert_eq!(x.created_by, Some(h.admin));
    for (label, name, description) in [
        ("blank", "   ", ""),
        ("81 characters", &"n".repeat(81)[..], ""),
        ("a 501-character description", "Long", &"d".repeat(501)[..]),
    ] {
        let r = c.create_team(h.admin, h.t, name, description).await;
        assert!(
            matches!(r, Err(TeamWriteError::Conflict(_))),
            "{label}: {r:?}"
        );
    }
    assert!(
        c.create_team(h.admin, h.t, &"é".repeat(80), &"d".repeat(500))
            .await
            .is_ok(),
        "80 characters, not bytes"
    );
    let r = c.update_team(h.admin, h.t, x.id, Some(""), None).await;
    assert!(
        matches!(r, Err(TeamWriteError::Conflict(_))),
        "rename to blank: {r:?}"
    );

    let alice = s.active(&h, "alice").await;
    let bob = s.active(&h, "bob").await;
    let w = team_write(
        &s,
        &h,
        &TeamChange::AddMember {
            team: x.id,
            member: alice,
        },
    )
    .await;
    assert!(matches!(w, Ok(TeamWrite { changed: true, .. })), "{w:?}");
    let events = s.event_count(h.t).await;
    for change in [
        TeamChange::AddMember {
            team: x.id,
            member: alice,
        },
        TeamChange::DeleteGrant {
            team: x.id,
            project: h.p,
        },
    ] {
        let w = team_write(&s, &h, &change).await;
        assert!(
            matches!(w, Ok(TeamWrite { changed: false, .. })),
            "{change:?}: {w:?}"
        );
    }
    let w = team_write(
        &s,
        &h,
        &TeamChange::RemoveMember {
            team: x.id,
            member: bob,
        },
    )
    .await;
    assert!(
        matches!(w, Ok(TeamWrite { changed: false, .. })),
        "not in the team: {w:?}"
    );
    let same = c
        .update_team(h.admin, h.t, x.id, Some("Ops"), Some(""))
        .await
        .unwrap();
    assert_eq!(
        (same.id, same.name.as_str(), same.description.as_str()),
        (x.id, "Ops", ""),
        "returned as it is"
    );
    assert_eq!(
        s.event_count(h.t).await,
        events,
        "no event for an unchanged write"
    );

    let theirs = c.create_team(u.admin, u.t, "theirs", "").await.unwrap();
    let r = c
        .update_team(h.admin, h.t, theirs.id, Some("mine"), None)
        .await;
    assert!(matches!(r, Err(TeamWriteError::NotFound)), "{r:?}");
    let r = c
        .team_change_observed(h.t, &TeamChange::Delete { team: theirs.id })
        .await;
    assert!(matches!(r, Err(TeamWriteError::NotFound)), "{r:?}");
    let r = c
        .write_team_checked(h.admin, h.t, &TeamChange::Delete { team: theirs.id }, &[])
        .await;
    assert!(matches!(r, Err(TeamWriteError::NotFound)), "{r:?}");
    assert!(
        c.team(u.t, theirs.id).await.unwrap().is_some(),
        "their team is untouched"
    );
    s.finish().await;
}

// I4, MH-5: the check's read is every affected member × affected project, by member then
// project, each with the member's own grant and team grants; the committed write reports the
// affected members with their Zitadel users and the projects to reconcile.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_checks_read_and_the_writes_report() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let mut ab = [s.active(&h, "a").await, s.active(&h, "b").await];
    ab.sort();
    let [a, b] = ab;
    let x = c.create_team(h.admin, h.t, "x", "").await.unwrap().id;
    let v = grant(PlatformRole::Viewer, &[]);
    for change in [
        TeamChange::PutGrant {
            team: x,
            project: h.p,
            grant: v.clone(),
        },
        TeamChange::PutGrant {
            team: x,
            project: q,
            grant: v.clone(),
        },
        TeamChange::AddMember { team: x, member: b },
        TeamChange::AddMember { team: x, member: a },
    ] {
        team_write(&s, &h, &change).await.unwrap();
    }
    let own = grant(PlatformRole::Developer, &["app"]);
    c.put_project_grant(h.admin, a, q, &own).await.unwrap();

    let delete = TeamChange::Delete { team: x };
    let expect = c.team_change_observed(h.t, &delete).await.unwrap();
    let mut pq = [h.p, q];
    pq.sort();
    let keys: Vec<(Uuid, Uuid)> = expect.iter().map(|(m, o)| (*m, o.project)).collect();
    assert_eq!(keys, vec![(a, pq[0]), (a, pq[1]), (b, pq[0]), (b, pq[1])]);
    let aq = expect
        .iter()
        .find(|(m, o)| *m == a && o.project == q)
        .unwrap();
    assert_eq!(
        (aq.1.grant.clone(), aq.1.teams.get(&x).cloned()),
        (Some(own), Some(v.clone()))
    );

    let w = c
        .write_team_checked(h.admin, h.t, &delete, &expect)
        .await
        .unwrap();
    assert!(w.changed);
    assert_eq!(
        w.affected,
        vec![(a, s.zitadel_of(a).await), (b, s.zitadel_of(b).await)]
    );
    assert_eq!(w.projects, pq.to_vec());
    // A second delete finds no team.
    let r = c.team_change_observed(h.t, &delete).await;
    assert!(matches!(r, Err(TeamWriteError::NotFound)), "{r:?}");
    s.finish().await;
}

// P12, P26: an invite naming a team twice joins it once (one event); revoking the invite drops
// the invitee's memberships in its transaction and lists them in `invite_revoked`; inviting the
// removed person again starts with no team.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn invite_teams_are_deduplicated_and_revoking_drops_them() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let x = c.create_team(h.admin, h.t, "Data", "").await.unwrap().id;
    let w = team_write(
        &s,
        &h,
        &TeamChange::PutGrant {
            team: x,
            project: h.p,
            grant: grant(PlatformRole::Viewer, &[]),
        },
    )
    .await
    .unwrap();
    assert!(w.changed);
    let sx = TeamSeen {
        team: x,
        grants: vec![(h.p, grant(PlatformRole::Viewer, &[]))],
    };
    let teams = [sx.clone(), sx.clone()];
    let invite = NewInvite {
        zitadel_user_id: "zi",
        email: "i@x.com",
        expires_at: Utc::now() + Duration::days(7),
        token_hash: [7; 32],
        grants: &[],
        teams: &teams,
    };
    let (listing, invite_id) = c.create_invited_member(h.admin, h.t, invite).await.unwrap();
    assert!(listing.has_access, "a team with a grant gives access");
    assert_eq!(s.memberships_of(listing.id).await, vec![x]);
    let added = c
        .access_events(h.t, None, Some(listing.id), None, 100)
        .await
        .unwrap();
    assert_eq!(
        added
            .iter()
            .filter(|e| e.action == "team_member_added")
            .count(),
        1,
        "{added:?}"
    );

    c.revoke_invite(h.admin, invite_id).await.unwrap();
    assert!(
        s.memberships_of(listing.id).await.is_empty(),
        "the revoked invitee left the team"
    );
    let revoked = c
        .access_events(h.t, None, Some(listing.id), None, 1)
        .await
        .unwrap();
    assert_eq!(revoked[0].action, "invite_revoked");
    assert_eq!(
        revoked[0].before.as_ref().map(|b| b["teams"].clone()),
        Some(json!([{ "id": x, "name": "Data" }]))
    );

    let again = NewInvite {
        teams: &[],
        ..invite
    };
    let (back, _) = c.create_invited_member(h.admin, h.t, again).await.unwrap();
    assert_eq!(back.id, listing.id, "the removed row is reused");
    assert!(s.memberships_of(back.id).await.is_empty());
    assert!(!back.has_access);

    // A team id that isn't the tenant's is `Changed` on the invite path too.
    let stray = [TeamSeen {
        team: Uuid::new_v4(),
        grants: vec![],
    }];
    let r = c
        .add_existing_user(h.admin, h.t, "zs", "s@x.com", "S", &[], &stray)
        .await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "{r:?}");
    s.finish().await;
}

// P25 (audit): an invite of an existing member that only joins teams still reports the projects
// whose access changed (the joined teams' grant projects), so the caller reconciles them; an
// unchanged own grant adds nothing, and joining a team the member is already in reports nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn joining_teams_reports_their_projects_as_changed() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let x = c.create_team(h.admin, h.t, "Data", "").await.unwrap().id;
    for project in [h.p, q] {
        team_write(
            &s,
            &h,
            &TeamChange::PutGrant {
                team: x,
                project,
                grant: grant(PlatformRole::Developer, &[]),
            },
        )
        .await
        .unwrap();
    }
    let sx = TeamSeen {
        team: x,
        grants: sorted(vec![h.p, q])
            .into_iter()
            .map(|p| (p, grant(PlatformRole::Developer, &[])))
            .collect(),
    };
    let bob = s.active(&h, "bob").await;
    let v = grant(PlatformRole::Viewer, &[]);
    c.put_project_grants(h.admin, bob, &[(h.p, v.clone())])
        .await
        .unwrap();
    let expect =
        |access: chelabase_metadata::members::ProjectAccess, p: Uuid| access.observed(p, bob);
    let observed = vec![
        expect(c.project_access(h.p).await.unwrap(), h.p),
        expect(c.project_access(q).await.unwrap(), q),
    ];

    // The own grant is unchanged; the team reaches both projects.
    let changed = c
        .put_project_grants_checked(
            h.admin,
            bob,
            &[(h.p, v.clone())],
            std::slice::from_ref(&sx),
            &observed,
        )
        .await
        .unwrap();
    assert_eq!(sorted(changed), sorted(vec![h.p, q]));

    // Already in the team: nothing changed.
    let observed = vec![
        expect(c.project_access(h.p).await.unwrap(), h.p),
        expect(c.project_access(q).await.unwrap(), q),
    ];
    let changed = c
        .put_project_grants_checked(
            h.admin,
            bob,
            &[(h.p, v)],
            std::slice::from_ref(&sx),
            &observed,
        )
        .await
        .unwrap();
    assert!(changed.is_empty(), "{changed:?}");
    s.finish().await;
}

// T1 audit (b), I4: a checked write that joins teams compares the member's access on every
// project those teams reach; an `expect` missing one of them is refused with nothing written
// (the write can't know that project's access was checked). With every project it goes through.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn joining_teams_needs_an_expect_for_each_team_project() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let x = c.create_team(h.admin, h.t, "Data", "").await.unwrap().id;
    for project in [h.p, q] {
        team_write(
            &s,
            &h,
            &TeamChange::PutGrant {
                team: x,
                project,
                grant: grant(PlatformRole::Developer, &[]),
            },
        )
        .await
        .unwrap();
    }
    let sx = TeamSeen {
        team: x,
        grants: sorted(vec![h.p, q])
            .into_iter()
            .map(|p| (p, grant(PlatformRole::Developer, &[])))
            .collect(),
    };
    let bob = s.active(&h, "bob").await;
    let only_p = vec![c.project_access(h.p).await.unwrap().observed(h.p, bob)];
    let events = s.event_count(h.t).await;
    let r = c
        .put_project_grants_checked(h.admin, bob, &[], std::slice::from_ref(&sx), &only_p)
        .await;
    assert!(r.is_err(), "q's access was never checked: {r:?}");
    assert!(s.memberships_of(bob).await.is_empty(), "nothing written");
    assert_eq!(s.event_count(h.t).await, events, "no event");

    let both = vec![
        c.project_access(h.p).await.unwrap().observed(h.p, bob),
        c.project_access(q).await.unwrap().observed(q, bob),
    ];
    let changed = c
        .put_project_grants_checked(h.admin, bob, &[], std::slice::from_ref(&sx), &both)
        .await
        .unwrap();
    assert_eq!(sorted(changed), sorted(vec![h.p, q]));
    s.finish().await;
}

// Demotion (R17, P6, P25): `member_observed` reads everything the member keeps (own grants,
// overrides, team grants; deleted projects left out); a demotion with `expect` waits for the
// tenant lock and compares what it read under it: a team join or a grant change committed
// before it is `Changed`, nothing written. A fresh read goes through.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_demotion_compares_what_the_member_keeps_under_the_tenant_lock() {
    let s = Scratch::new().await;
    let h = s.hub().await;
    let c = &s.client;
    let q = s.project(h.t).await;
    let gone = s.project(h.t).await;
    let bob = s.active(&h, "bob").await;
    c.set_tenant_admin(Some(h.admin), bob, true).await.unwrap();
    let own = grant(PlatformRole::Developer, &["app"]);
    c.put_project_grant(h.admin, bob, q, &own).await.unwrap();
    c.put_project_grant(h.admin, bob, gone, &own).await.unwrap();
    s.delete_project(gone).await;
    let x = c.create_team(h.admin, h.t, "Data", "").await.unwrap().id;
    team_write(
        &s,
        &h,
        &TeamChange::PutGrant {
            team: x,
            project: h.p,
            grant: grant(PlatformRole::Viewer, &[]),
        },
    )
    .await
    .unwrap();

    let expect = c.member_observed(bob).await.unwrap();
    assert_eq!(
        expect.iter().map(|o| (o.project, o.grant.clone())).collect::<Vec<_>>(),
        vec![(q, Some(own.clone()))],
        "only q: the team isn't bob's yet and `gone` is deleted"
    );

    // A team join committed between the check and the write: `Changed`, nothing written.
    team_write(&s, &h, &TeamChange::AddMember { team: x, member: bob })
        .await
        .unwrap();
    let events = s.event_count(h.t).await;
    let r = c
        .set_tenant_admin_checked(Some(h.admin), bob, false, Some(&expect))
        .await;
    assert!(matches!(r, Err(AccessWriteError::Changed)), "{r:?}");
    assert!(c.member(bob).await.unwrap().unwrap().tenant_admin, "still a tenant admin");
    assert_eq!(s.event_count(h.t).await, events, "no event");

    // The comparison runs under the tenant lock: while another transaction holds it, the
    // demotion waits; that transaction's change (bob's own grant on q dropped) is seen.
    let expect = c.member_observed(bob).await.unwrap();
    assert_eq!(
        expect.iter().map(|o| o.project).collect::<Vec<_>>(),
        sorted(vec![h.p, q])
    );
    let mut holder = c.pool().begin().await.unwrap();
    sqlx::query("SELECT 1 FROM tenants WHERE id = $1 FOR NO KEY UPDATE")
        .bind(h.t.as_uuid())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    let demote = {
        let c = c.clone();
        let expect = expect.clone();
        let admin = h.admin;
        tokio::spawn(async move {
            c.set_tenant_admin_checked(Some(admin), bob, false, Some(&expect))
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!demote.is_finished(), "the demotion waits for the tenant lock");
    sqlx::query("DELETE FROM project_grants WHERE member_id = $1 AND project_id = $2")
        .bind(bob)
        .bind(q)
        .execute(&mut *holder)
        .await
        .unwrap();
    holder.commit().await.unwrap();
    let r = demote.await.unwrap();
    assert!(matches!(r, Err(AccessWriteError::Changed)), "{r:?}");
    assert!(c.member(bob).await.unwrap().unwrap().tenant_admin, "still a tenant admin");

    // Read again, the demotion goes through.
    let expect = c.member_observed(bob).await.unwrap();
    assert_eq!(expect.iter().map(|o| o.project).collect::<Vec<_>>(), vec![h.p]);
    c.set_tenant_admin_checked(Some(h.admin), bob, false, Some(&expect))
        .await
        .unwrap();
    assert!(!c.member(bob).await.unwrap().unwrap().tenant_admin);
    s.finish().await;
}
