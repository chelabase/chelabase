//! m16: `GET /v1/databases/{id}/branches/{branch_id}/data-api/types`, Studio's types
//! download. Who may call it is exactly who may create a secret key for the role (createKey's
//! `Project(Developer)`, MH-12 and the key-role check), in every member state; it passes rest's
//! internal types route through byte for byte (body, `ETag`, `Content-Type`, 304); and a
//! refusal never reaches rest, so no schema leaks.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_metadata::DataApiState;
use chelabase_types::access::RoleSet;

/// One request rest's mock saw.
#[derive(Clone, Debug)]
struct Seen {
    path: String,
    query: String,
    if_none_match: Option<String>,
    bearer: Option<String>,
}

/// What rest's mock answers instead of its default (status, headers, body).
type Override = Option<(u16, Vec<(&'static str, String)>, Vec<u8>)>;

/// The module the mock serves: non-ASCII and a trailing newline, to see bytes pass unchanged.
const MODULE: &str = "export type Database = { \"größe\": 1 };\r\n\n";
const MAP: &str = "{\"defaultSchema\":\"public\",\"schemas\":{}}";
const TS_TAG: &str = "\"ts-tag\"";
const JSON_TAG: &str = "\"json-tag\"";

struct World {
    w: MemberWorld,
    seen: Arc<Mutex<Vec<Seen>>>,
    answer: Arc<Mutex<Override>>,
}

/// Rest's internal types route as m15's contract has it: the module or map, a strong ETag,
/// `no-store`, and a 304 when `If-None-Match` names the tag.
async fn mock_rest(seen: Arc<Mutex<Vec<Seen>>>, answer: Arc<Mutex<Override>>) -> String {
    use axum::http::{HeaderMap, StatusCode, Uri};
    use axum::response::IntoResponse;
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/types/{branch}",
        get(move |uri: Uri, headers: HeaderMap| {
            let (seen, answer) = (seen.clone(), answer.clone());
            async move {
                let header =
                    |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_string());
                let query = uri.query().unwrap_or_default().to_string();
                seen.lock().unwrap().push(Seen {
                    path: uri.path().to_string(),
                    query: query.clone(),
                    if_none_match: header("if-none-match"),
                    bearer: header("authorization"),
                });
                if let Some((status, extra, body)) = answer.lock().unwrap().clone() {
                    let mut resp = (StatusCode::from_u16(status).unwrap(), body).into_response();
                    for (k, v) in extra {
                        resp.headers_mut().insert(k, v.parse().unwrap());
                    }
                    return resp;
                }
                let json = query.split('&').any(|p| p == "format=json");
                let (tag, body, ty) = if json {
                    (JSON_TAG, MAP, "application/json")
                } else {
                    (TS_TAG, MODULE, "text/typescript; charset=utf-8")
                };
                let cache = [("etag", tag), ("cache-control", "no-store")];
                if header("if-none-match").as_deref() == Some(tag) {
                    return (StatusCode::NOT_MODIFIED, cache).into_response();
                }
                (cache, [("content-type", ty)], body.as_bytes().to_vec()).into_response()
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

/// `member_world`, with rest's mock behind it, `main` and `analytics` served on `dev` and
/// `archive` not.
async fn world() -> World {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let answer = Arc::new(Mutex::new(None));
    let rest = mock_rest(seen.clone(), answer.clone()).await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_in_mode(metadata.clone(), jobs.clone(), &rest, Mode::SelfHosted).await;
    let project = metadata.create_project(tenant, "Billing").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    let dev = metadata.add_branch(db_id, "dev", main, BranchKind::Normal, BranchStatus::Ready);
    metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    metadata.add_branch_db(dev, "analytics", "postgres", DatabaseOrigin::Sql);
    metadata.add_branch_db(dev, "archive", "postgres", DatabaseOrigin::Sql);
    for d in metadata.branch_dbs.lock().unwrap().get_mut(&dev).unwrap() {
        if d.name != "archive" {
            d.data_api = DataApiState::On;
        }
    }
    let w = MemberWorld {
        app,
        metadata,
        tenant,
        project,
        db: db_id,
        dev,
        merge: uuid::Uuid::nil(),
        jobs,
    };
    World { w, seen, answer }
}

impl World {
    fn base(&self, db: DatabaseId, branch: BranchId) -> String {
        format!(
            "/v1/databases/{}/branches/{}",
            db.as_uuid(),
            branch.as_uuid()
        )
    }

    fn types(&self, query: &str) -> String {
        format!("{}/data-api/types{query}", self.base(self.w.db, self.w.dev))
    }

    fn rest_calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    async fn get(&self, sub: &str, path: &str, headers: &[(&str, &str)]) -> reqwest::Response {
        let mut req = chelabase_http::client_builder()
            .build()
            .unwrap()
            .get(format!("{}{path}", self.w.app.base))
            .header(IDENTITY_HEADER, self.w.app.token_for(sub, "org-1"));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.send().await.unwrap()
    }
}

/// An answer as the access comparison reads it.
async fn answer_of(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let body = resp.json::<Value>().await.unwrap_or(Value::Null);
    (status, body)
}

/// The caller kinds: every member state, every way to hold (or not) a role, and the owner.
fn callers(w: &MemberWorld) -> Vec<&'static str> {
    let t = w.tenant;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "reporting", &[]);
    w.metadata.add_group_role(&ep, "other", &[]);
    w.metadata.add_group_role(&ep, "billing", &[]);
    w.metadata
        .add_group_role(&ep, "billing_admin", &["billing"]);
    w.metadata.add_group_role(&ep, "authenticated", &[]);
    w.metadata
        .add_role(&ep, "power", RoleOrigin::Sql, true, false);
    let other_project = ProjectId::new();

    // Not active: refused whatever their grant.
    for (sub, status) in [
        ("pending", MemberStatus::Pending),
        ("invited", MemberStatus::Invited),
        ("removed", MemberStatus::Removed),
        ("suspended", MemberStatus::Suspended),
    ] {
        let id = w.metadata.add_member(t, sub, status, false);
        w.metadata
            .grant(id, w.project, PlatformRole::Admin, &["authenticated"]);
    }
    let off = w.metadata.add_granted(
        t,
        "offboarded",
        w.project,
        PlatformRole::Admin,
        &["authenticated"],
    );
    w.metadata
        .set_member(t, "offboarded", |m| m.offboarded = true);
    let _ = off;
    let removed_admin = w
        .metadata
        .add_member(t, "removed_tadmin", MemberStatus::Removed, true);
    let _ = removed_admin;
    // Active, with too little.
    w.metadata
        .add_member(t, "nogrant", MemberStatus::Active, false);
    w.metadata.add_granted(
        t,
        "outsider",
        other_project,
        PlatformRole::Admin,
        &["authenticated"],
    );
    w.metadata.add_granted(
        t,
        "viewer",
        w.project,
        PlatformRole::Viewer,
        &["authenticated", "reporting"],
    );
    let narrowed = w.metadata.add_granted(
        t,
        "narrowed",
        w.project,
        PlatformRole::Developer,
        &["authenticated", "reporting"],
    );
    w.metadata.narrow(
        narrowed,
        w.db,
        OverrideRow {
            platform_role: Some(PlatformRole::Viewer),
            pg_roles: None,
        },
    );
    let teamviewer = w
        .metadata
        .add_member(t, "teamviewer", MemberStatus::Active, false);
    w.metadata.team_grant(
        teamviewer,
        w.project,
        Grant {
            platform_role: PlatformRole::Viewer,
            roles: RoleSet::Only([PgRole::parse("reporting")].into()),
        },
    );
    // Developers: by role held, by chain, by team.
    w.metadata.add_granted(
        t,
        "dan",
        w.project,
        PlatformRole::Developer,
        &["reporting", "power"],
    );
    w.metadata.add_granted(
        t,
        "chain",
        w.project,
        PlatformRole::Developer,
        &["billing_admin"],
    );
    let teamdev = w
        .metadata
        .add_member(t, "teamdev", MemberStatus::Active, false);
    w.metadata.team_grant(
        teamdev,
        w.project,
        Grant {
            platform_role: PlatformRole::Developer,
            roles: RoleSet::Only([PgRole::parse("authenticated")].into()),
        },
    );
    // Admins.
    w.metadata
        .add_granted(t, "ada", w.project, PlatformRole::Admin, &[]);
    w.metadata
        .add_member(t, "tadmin", MemberStatus::Active, true);
    // A member with a personal login on the database (MH-12: never a key's role).
    let pat = w
        .metadata
        .add_granted(t, "pat", w.project, PlatformRole::Admin, &[]);
    w.metadata.seed_personal_login(pat, w.db, "pat_login");

    vec![
        "stranger",
        "pending",
        "invited",
        "removed",
        "suspended",
        "offboarded",
        "removed_tadmin",
        "nogrant",
        "outsider",
        "viewer",
        "narrowed",
        "teamviewer",
        "dan",
        "chain",
        "teamdev",
        "ada",
        "tadmin",
        "pat",
    ]
}

/// The roles a caller can ask for: the default (none given), held, unheld, reached through a
/// chain, owner-level, the database's owner, a personal login, and one that doesn't exist.
const ROLES: [Option<&str>; 9] = [
    None,
    Some("authenticated"),
    Some("reporting"),
    Some("other"),
    Some("billing"),
    Some("power"),
    Some("postgres"),
    Some("pat_login"),
    Some("ghost"),
];

/// The invariant: for every caller and role, the types route answers createKey's access
/// answer. Where createKey creates, types serves (200); where it refuses, types refuses with
/// the same status and body, and rest is never asked.
#[tokio::test]
async fn types_route_refuses_without_key_access() {
    let world = world().await;
    let w = &world.w;
    let subs = callers(w);
    let keys = format!("{}/keys", world.base(w.db, w.dev));
    let mut n = 0;
    let (mut served, mut refused) = (0, 0);
    for sub in subs {
        for role in ROLES {
            n += 1;
            let body = match role {
                Some(role) => json!({ "name": format!("k{n}"), "role": role }),
                None => json!({ "name": format!("k{n}") }),
            };
            let create = answer_of(
                w.app
                    .send_as(sub, "org-1", Method::POST, &keys, Some(body))
                    .await,
            )
            .await;
            let before = world.rest_calls();
            let query = role.map(|r| format!("?role={r}")).unwrap_or_default();
            let resp = world.get(sub, &world.types(&query), &[]).await;
            let status = resp.status().as_u16();
            if create.0 == 201 {
                assert_eq!(
                    status, 200,
                    "{sub} {role:?}: createKey created, types answered {status}"
                );
                assert_eq!(world.rest_calls(), before + 1, "{sub} {role:?}");
                let seen = world.seen.lock().unwrap().last().unwrap().clone();
                let want = role.unwrap_or("authenticated");
                assert!(
                    seen.query.starts_with(&format!("role={want}&")),
                    "{sub} {role:?}: {seen:?}"
                );
                served += 1;
            } else {
                let types = answer_of(resp).await;
                assert_eq!(
                    types, create,
                    "{sub} {role:?}: types and createKey disagree"
                );
                assert_eq!(
                    world.rest_calls(),
                    before,
                    "{sub} {role:?}: a refusal asked rest"
                );
                refused += 1;
            }
        }
    }
    // Both sides of the line are exercised.
    assert!(
        served >= 10 && refused >= 50,
        "served {served}, refused {refused}"
    );
}

/// Another tenant's database, a branch of another database, and a missing one: the key
/// routes' 404, for a tenant admin of either tenant, and rest is never asked.
#[tokio::test]
async fn types_route_keeps_to_the_callers_tenant_and_database() {
    let world = world().await;
    let w = &world.w;
    w.metadata
        .add_member(w.tenant, "tadmin", MemberStatus::Active, true);
    // A second tenant with its own database and admin.
    let other = TenantId::new();
    w.metadata.bind_org("org-2", other);
    w.metadata
        .add_member(other, "boss2", MemberStatus::Active, true);
    let theirs = sample_database(other);
    let (their_db, their_main) = (theirs.database_id, theirs.main.id);
    w.metadata.insert_database(theirs);
    // A second database of the same tenant.
    let mut sibling = sample_database(w.tenant);
    sibling.project_id = w.project;
    let (sib_db, sib_main) = (sibling.database_id, sibling.main.id);
    w.metadata.insert_database(sibling);

    let cases = [
        ("tadmin", "org-1", world.base(their_db, their_main)),
        ("tadmin", "org-1", world.base(w.db, sib_main)),
        ("tadmin", "org-1", world.base(sib_db, w.dev)),
        ("tadmin", "org-1", world.base(DatabaseId::new(), w.dev)),
        ("tadmin", "org-1", world.base(w.db, BranchId::new())),
        ("boss2", "org-2", world.base(w.db, w.dev)),
    ];
    for (sub, org, base) in cases {
        let create = answer_of(
            w.app
                .send_as(
                    sub,
                    org,
                    Method::POST,
                    &format!("{base}/keys"),
                    Some(json!({ "name": "k" })),
                )
                .await,
        )
        .await;
        let types = answer_of(
            w.app
                .send_as(
                    sub,
                    org,
                    Method::GET,
                    &format!("{base}/data-api/types"),
                    None,
                )
                .await,
        )
        .await;
        assert_eq!(create.0, 404, "{sub} {base}: {create:?}");
        assert_eq!(types, create, "{sub} {base}");
    }
    assert_eq!(world.rest_calls(), 0);
}

/// The body, `Content-Type`, `ETag` and `Cache-Control` are rest's, byte for byte, for each
/// option; the options reach rest as given, with the key's role, and rest's bearer.
#[tokio::test]
async fn types_route_matches_the_data_api_output() {
    let world = world().await;
    let w = &world.w;
    w.metadata
        .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    for (query, forwarded, body, ty, tag) in [
        (
            "",
            "role=authenticated&format=ts",
            MODULE,
            "text/typescript; charset=utf-8",
            TS_TAG,
        ),
        (
            "?format=ts",
            "role=authenticated&format=ts",
            MODULE,
            "text/typescript; charset=utf-8",
            TS_TAG,
        ),
        (
            "?format=json",
            "role=authenticated&format=json",
            MAP,
            "application/json",
            JSON_TAG,
        ),
        (
            "?database=analytics",
            "role=authenticated&format=ts&database=analytics",
            MODULE,
            "text/typescript; charset=utf-8",
            TS_TAG,
        ),
        (
            "?all_databases=true&format=json",
            "role=authenticated&format=json&all_databases=true",
            MAP,
            "application/json",
            JSON_TAG,
        ),
        (
            "?all_databases=false",
            "role=authenticated&format=ts",
            MODULE,
            "text/typescript; charset=utf-8",
            TS_TAG,
        ),
        (
            "?role=reporting&database=web1",
            "role=reporting&format=ts&database=web1",
            MODULE,
            "text/typescript; charset=utf-8",
            TS_TAG,
        ),
        (
            "?role=a%20b%26c",
            "role=a%20b%26c&format=ts",
            MODULE,
            "text/typescript; charset=utf-8",
            TS_TAG,
        ),
    ] {
        let resp = world.get("ada", &world.types(query), &[]).await;
        assert_eq!(resp.status(), 200, "{query}");
        let headers = resp.headers().clone();
        assert_eq!(headers["content-type"], ty, "{query}");
        assert_eq!(headers["etag"], tag, "{query}");
        assert_eq!(headers["cache-control"], "no-store", "{query}");
        assert_eq!(
            resp.bytes().await.unwrap().as_ref(),
            body.as_bytes(),
            "{query}"
        );
        let seen = world.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(
            seen.path,
            format!("/internal/types/{}", w.dev.as_uuid()),
            "{query}"
        );
        assert_eq!(seen.query, forwarded, "{query}");
        assert_eq!(
            seen.bearer.as_deref(),
            Some(format!("Bearer {REST_TOKEN}").as_str())
        );
        assert_eq!(seen.if_none_match, None);
    }
}

/// `If-None-Match` reaches rest; its 304 comes back with the tag, `no-store` and no body.
#[tokio::test]
async fn types_route_etag_and_304() {
    let world = world().await;
    let w = &world.w;
    w.metadata
        .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    let resp = world
        .get("ada", &world.types(""), &[("if-none-match", TS_TAG)])
        .await;
    assert_eq!(resp.status(), 304);
    assert_eq!(resp.headers()["etag"], TS_TAG);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.bytes().await.unwrap().len(), 0);
    assert_eq!(
        world
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .if_none_match
            .as_deref(),
        Some(TS_TAG)
    );
    // A stale tag, or the other format's: the body.
    for (query, tag) in [("", "\"old\""), ("", JSON_TAG), ("?format=json", TS_TAG)] {
        let resp = world
            .get("ada", &world.types(query), &[("if-none-match", tag)])
            .await;
        assert_eq!(resp.status(), 200, "{query} {tag}");
    }
    let resp = world
        .get(
            "ada",
            &world.types("?format=json"),
            &[("if-none-match", JSON_TAG)],
        )
        .await;
    assert_eq!(resp.status(), 304);
    assert_eq!(resp.headers()["etag"], JSON_TAG);
}

/// Options the data API refuses, unknown or repeated ones, and an empty role: 422
/// `invalid_request` before rest is asked.
#[tokio::test]
async fn types_route_refuses_bad_options_before_rest() {
    let world = world().await;
    let w = &world.w;
    w.metadata
        .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    for query in [
        "?role=",
        "?role=a&role=b",
        "?format=xml",
        "?format=TS",
        "?format=ts&format=json",
        "?all_databases=yes",
        "?database=analytics&all_databases=true",
        "?database=",
        "?branch=x",
        "?allDatabases=true",
    ] {
        let (status, body) = answer_of(world.get("ada", &world.types(query), &[]).await).await;
        assert_eq!(
            (status, body["error"]["code"].as_str()),
            (422, Some("invalid_request")),
            "{query}: {body}"
        );
    }
    assert_eq!(world.rest_calls(), 0);
}

/// The branch and database states, the worker's refusal, and what rest answers besides the
/// types: each as the key and schema routes answer it.
#[tokio::test]
async fn types_route_answers_branch_database_and_rest_states() {
    let world = world().await;
    let w = &world.w;
    w.metadata
        .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    let code = |b: &Value| b["error"]["code"].as_str().map(str::to_string);

    // Databases: unknown, not served; checked before rest.
    for (query, want) in [
        ("?database=ghost", (404, "not_found")),
        ("?database=archive", (409, "data_api_disabled")),
    ] {
        let (status, body) = answer_of(world.get("ada", &world.types(query), &[]).await).await;
        assert_eq!(
            (status, code(&body)),
            (want.0, Some(want.1.to_string())),
            "{query}: {body}"
        );
    }
    // Branches: a backup, one not ready.
    let backup = w.metadata.add_branch(
        w.db,
        "dev-before-restore-20260101T000000Z",
        w.dev,
        BranchKind::Backup,
        BranchStatus::Ready,
    );
    let creating = w.metadata.add_branch(
        w.db,
        "qa",
        w.dev,
        BranchKind::Normal,
        BranchStatus::Provisioning,
    );
    for (branch, want) in [(backup, "backup_branch"), (creating, "branch_not_ready")] {
        let path = format!("{}/data-api/types", world.base(w.db, branch));
        let (status, body) = answer_of(world.get("ada", &path, &[]).await).await;
        assert_eq!(
            (status, code(&body)),
            (409, Some(want.to_string())),
            "{body}"
        );
    }
    assert_eq!(world.rest_calls(), 0);
    // The worker refuses the role: createKey's 422.
    *w.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply {
        ok: false,
        kind: Some("refused".into()),
        error: Some("role \"x\" is owner-level".into()),
        ..CheckDataRoleReply::ok()
    }));
    let (status, body) = answer_of(world.get("ada", &world.types(""), &[]).await).await;
    assert_eq!(
        (status, code(&body)),
        (422, Some("role_refused".into())),
        "{body}"
    );
    assert_eq!(world.rest_calls(), 0);
    *w.jobs.role_check.lock().unwrap() = None;

    // Rest's other answers.
    let json = |v: Value| v.to_string().into_bytes();
    let ct = vec![("content-type", "application/json".to_string())];
    for (status, headers, body, want_status, want_code) in [
        (
            409,
            ct.clone(),
            json(json!({ "status": "resetting" })),
            409,
            "branch_not_ready",
        ),
        (
            404,
            ct.clone(),
            json(json!({ "error": { "code": "unknown_branch", "message": "x" } })),
            404,
            "not_found",
        ),
        (
            404,
            ct.clone(),
            json(json!({ "error": { "code": "unknown_database", "message": "x" } })),
            404,
            "not_found",
        ),
        (
            409,
            ct.clone(),
            json(json!({ "error": { "code": "conflict", "message": "a schema has a dot" } })),
            409,
            "conflict",
        ),
        (
            503,
            vec![
                ("content-type", "application/json".into()),
                ("retry-after", "2".into()),
            ],
            json(json!({ "error": { "code": "unavailable", "message": "try again" } })),
            503,
            "unavailable",
        ),
        (
            302,
            vec![("location", "http://elsewhere.test/".into())],
            Vec::new(),
            503,
            "unavailable",
        ),
        (
            200,
            vec![("etag", "\"t\"".into())],
            b"x".to_vec(),
            502,
            "unavailable",
        ),
    ] {
        *world.answer.lock().unwrap() = Some((status, headers, body));
        let resp = world.get("ada", &world.types(""), &[]).await;
        let retry = resp
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_string());
        let (got, body) = answer_of(resp).await;
        assert_eq!(
            (got, code(&body)),
            (want_status, Some(want_code.to_string())),
            "rest {status}: {body}"
        );
        if status == 503 {
            assert_eq!(retry.as_deref(), Some("2"));
        }
    }
}
