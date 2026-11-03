//! App users in the management API: the settings (enable, disable) and the user admin routes,
//! against the identity service's fake (`chelabase_identity::mock::FakeIdentity`). Each
//! project's users live in its pool `app-<project>`.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_identity::mock::FakeIdentity;
use chelabase_identity::{Identity as IdentityClient, Pool};
use chelabase_query::app_users::{NewUserRow, PendingInsert, SyncTarget, UserRowError, UsersDb};

async fn send(
    app: &TestApp,
    method: Method,
    path: &str,
    org: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org(org));
    if let Some(body) = body {
        req = req.json(&body);
    }
    req.send().await.unwrap()
}

/// The error body of a response with the expected status.
async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {body}"))
}

fn message_of(body: &Value) -> &str {
    body["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("no error message in {body}"))
}

/// org-1's tenant with the app running.
async fn projects_app(jobs: FakeJobs) -> (TestApp, FakeMetadata, TenantId) {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata.clone(), jobs).await;
    (app, metadata, tenant)
}

fn project_database(tenant: TenantId, project: ProjectId, status: DatabaseStatus) -> Db {
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.status = status;
    db.main.status = BranchStatus::from(status);
    db
}

/// Collects what the orchestrator logs while a test runs (the test's runtime is
/// single-threaded, so the server's tasks log through this thread's subscriber).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || logs.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }
}

/// What `FakeUsersDb` does when asked to insert or to commit.
#[derive(Clone, Copy, PartialEq, Debug)]
enum UdbFault {
    Constraint,
    EmailTaken,
    CommitFails,
}

/// A users-table writer that journals what it was asked, and (with a probe) how many users
/// the project's pool had at each step.
#[derive(Clone, Default)]
struct FakeUsersDb {
    journal: Arc<Mutex<Vec<String>>>,
    targets: Arc<Mutex<Vec<SyncTarget>>>,
    rows: Arc<Mutex<Vec<NewUserRow>>>,
    fault: Arc<Mutex<Option<UdbFault>>>,
    probe: Arc<Mutex<Option<(IdentityClient, Pool)>>>,
}

impl FakeUsersDb {
    async fn pool_users(&self) -> u64 {
        let probe = self.probe.lock().unwrap().clone();
        match probe {
            Some((identity, pool)) => identity
                .list_users(&pool, 1, 0)
                .await
                .map_or(0, |p| p.total),
            None => 0,
        }
    }
    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }
}

struct FakePending {
    db: FakeUsersDb,
}

#[async_trait::async_trait]
impl PendingInsert for FakePending {
    async fn commit(self: Box<Self>) -> Result<(), UserRowError> {
        let users = self.db.pool_users().await;
        self.db
            .journal
            .lock()
            .unwrap()
            .push(format!("commit:users={users}"));
        if *self.db.fault.lock().unwrap() == Some(UdbFault::CommitFails) {
            return Err(UserRowError::Unavailable("commit failed".into()));
        }
        Ok(())
    }
    async fn rollback(self: Box<Self>) {
        self.db.journal.lock().unwrap().push("rollback".into());
    }
}

#[async_trait::async_trait]
impl UsersDb for FakeUsersDb {
    async fn begin_insert(
        &self,
        target: &SyncTarget,
        row: &NewUserRow,
    ) -> Result<Box<dyn PendingInsert>, UserRowError> {
        let users = self.pool_users().await;
        self.journal
            .lock()
            .unwrap()
            .push(format!("begin:{}:users={users}", row.email));
        self.targets.lock().unwrap().push(target.clone());
        self.rows.lock().unwrap().push(row.clone());
        match *self.fault.lock().unwrap() {
            Some(UdbFault::Constraint) => Err(UserRowError::Constraint {
                message: "null value in column \"nick\"".into(),
                constraint: Some("users_nick_check".into()),
            }),
            Some(UdbFault::EmailTaken) => Err(UserRowError::EmailTaken),
            _ => Ok(Box::new(FakePending { db: self.clone() })),
        }
    }
}

const AU_PASSWORD: &str = "Correct-horse-9";

/// A project with a ready database (main branch, Postgres database `web1` served by the data
/// API), org-1's tenant, and optionally the identity fake behind the orchestrator.
struct Au {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    tenant: TenantId,
    project: ProjectRecord,
    db: DatabaseId,
    main: BranchId,
    fake: Option<FakeIdentity>,
    udb: FakeUsersDb,
}

async fn au_env(with_identity: bool) -> Au {
    au_env_on(with_identity, None).await
}

/// [`au_env`] with the identity fake, whose orchestrator has `compute_tls`; also the main
/// branch's endpoint id.
async fn au_env_tls(compute_tls: Arc<orchestrator::compute_tls::ComputeTls>) -> (Au, String) {
    let au = au_env_on(true, Some(compute_tls)).await;
    let endpoint = au
        .metadata
        .branch(au.main)
        .endpoint_id
        .expect("the main branch has an endpoint");
    (au, endpoint)
}

async fn au_env_on(
    with_identity: bool,
    compute_tls: Option<Arc<orchestrator::compute_tls::ComputeTls>>,
) -> Au {
    let metadata = FakeMetadata::default();
    let jobs = FakeJobs::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let project = metadata.create_project(tenant, "Web").await.unwrap();
    let db = project_database(tenant, project.id, DatabaseStatus::Ready);
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    metadata.add_branch_db(main, "web1", "owner", DatabaseOrigin::Main);
    metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::On);
    let udb = FakeUsersDb::default();
    let fake = if with_identity {
        Some(FakeIdentity::start().await)
    } else {
        None
    };
    if let Some(fake) = &fake {
        *udb.probe.lock().unwrap() = Some((fake.client(), Pool::App(project.id.as_uuid())));
    }
    let app = spawn_app_identity(
        metadata.clone(),
        jobs.clone(),
        fake.as_ref().map(FakeIdentity::client),
        Arc::new(udb.clone()),
        compute_tls,
    )
    .await;
    Au {
        app,
        metadata,
        jobs,
        tenant,
        project,
        db: db_id,
        main,
        fake,
        udb,
    }
}

impl Au {
    fn path(&self, rest: &str) -> String {
        format!("/v1/projects/{}{rest}", self.project.id)
    }

    async fn call(&self, method: Method, rest: &str, body: Option<Value>) -> (u16, Value) {
        let resp = send(&self.app, method, &self.path(rest), "org-1", body).await;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (
            status,
            if text.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text).unwrap()
            },
        )
    }

    /// The project's `project_auth` row in `status`; when enabled its pool exists.
    fn set_auth(&self, status: AppUsersStatus) {
        let now = chrono::Utc::now();
        self.metadata.project_auths.lock().unwrap().insert(
            self.project.id,
            ProjectAuth {
                project_id: self.project.id,
                status,
                error: None,
                database_id: self.db,
                pg_database: "web1".into(),
                schema_name: "public".into(),
                table_name: "users".into(),
                email_verification: false,
                on_delete: OnDelete::Delete,
                created_at: now,
                updated_at: now,
            },
        );
        if let Some(fake) = &self.fake {
            fake.with_pool(&self.pool());
        }
    }

    fn auth(&self) -> ProjectAuth {
        self.metadata
            .project_auths
            .lock()
            .unwrap()
            .get(&self.project.id)
            .cloned()
            .expect("project_auth row")
    }

    /// The sync role's sealed password on the auth database's main branch.
    fn seal_sync_role(&self) {
        let sealed = chelabase_secrets::seal_role_password(
            &self.app.key,
            &self.main.to_string(),
            "chelabase_auth_sync",
            "sync-pw",
        );
        self.metadata.branch_syncs.lock().unwrap().insert(
            self.main,
            BranchAuthSync {
                branch_id: self.main,
                pg_database: "web1".into(),
                password_sealed: sealed,
            },
        );
        *self.app.locator.addr.lock().unwrap() = Some("127.0.0.1:55432".parse().unwrap());
    }

    /// The project's pool.
    fn pool(&self) -> Pool {
        Pool::App(self.project.id.as_uuid())
    }

    fn fake(&self) -> &FakeIdentity {
        self.fake.as_ref().expect("the identity fake")
    }

    /// A verified user with a password in the project's pool.
    fn add_user(&self, email: &str) -> uuid::Uuid {
        self.add_user_in(&self.pool(), email)
    }

    fn add_user_in(&self, pool: &Pool, email: &str) -> uuid::Uuid {
        self.fake()
            .with_user(pool, email, AU_PASSWORD)
            .parse()
            .unwrap()
    }

    /// The user as the pool has it, or `None`.
    async fn user(&self, id: uuid::Uuid) -> Option<chelabase_identity::User> {
        match self
            .fake()
            .client()
            .get_user(&self.pool(), &id.to_string())
            .await
        {
            Ok(user) => Some(user),
            Err(chelabase_identity::IdentityError::NotFound(_)) => None,
            Err(e) => panic!("{e}"),
        }
    }

    /// The user with this email in the pool, or `None`.
    async fn user_by_email(&self, email: &str) -> Option<chelabase_identity::User> {
        let page = self
            .fake()
            .client()
            .search_users(&self.pool(), email, 200, 0)
            .await
            .unwrap();
        page.users.into_iter().find(|u| u.email == email)
    }

    /// An identity session for the user, and a live app session (in metadata) behind it.
    fn add_session(&self, user: uuid::Uuid) -> String {
        let (identity_session, _) = self.fake().with_session(&self.pool(), &user.to_string());
        let now = chrono::Utc::now();
        self.metadata.app_sessions.lock().unwrap().push(AppSession {
            id: uuid::Uuid::new_v4(),
            project_id: self.project.id,
            user_id: user,
            identity_session_id: identity_session.clone(),
            created_at: now,
            last_used_at: now,
            expires_at: now + chrono::Duration::days(30),
            revoked_at: None,
        });
        identity_session
    }

    fn live_sessions(&self, user: uuid::Uuid) -> usize {
        self.metadata
            .app_sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.user_id == user && s.revoked_at.is_none())
            .count()
    }

    /// The identity calls on the project's pool, `"METHOD rest"` with the pool's prefix elided.
    fn pool_calls(&self) -> Vec<String> {
        let prefix = format!("/internal/v1/pools/{}", self.pool());
        self.fake()
            .calls()
            .iter()
            .filter_map(|c| {
                let (method, path) = c.split_once(' ')?;
                Some(format!("{method} {}", path.strip_prefix(&prefix)?))
            })
            .collect()
    }

    /// The pool's calls that change something (every one but a read).
    fn pool_writes(&self) -> Vec<String> {
        self.pool_calls()
            .into_iter()
            .filter(|c| !c.starts_with("GET "))
            .collect()
    }

    fn syncs(&self) -> Vec<AppUsersSyncJob> {
        self.jobs.app_users_sync_jobs.lock().unwrap().clone()
    }

    fn jobs_published(&self) -> Vec<AppUsersJob> {
        self.jobs.app_users_jobs.lock().unwrap().clone()
    }
}

// ---- Settings: enable and disable ----------------------------------------------------

fn put_body(au: &Au) -> Value {
    json!({ "database_id": au.db })
}

#[tokio::test]
async fn get_app_users_404_until_enabled_then_the_settings() {
    let au = au_env(false).await;
    let (status, body) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(
        (status, code_of(&body)),
        (404, "app_users_disabled"),
        "{body}"
    );
    au.set_auth(AppUsersStatus::Enabled);
    let (status, body) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "enabled");
    assert_eq!(body["database_id"], au.db.to_string());
    assert_eq!(
        (
            body["pg_database"].as_str(),
            body["schema"].as_str(),
            body["table"].as_str()
        ),
        (Some("web1"), Some("public"), Some("users"))
    );
    assert_eq!(
        (
            body["email_verification"].as_bool(),
            body["on_delete"].as_str()
        ),
        (Some(false), Some("delete"))
    );
    // No org field: app users live in the identity pool `app-<project>`.
    assert!(body.as_object().unwrap().keys().all(|k| !k.ends_with("_org_id")), "{body}");
    assert!(body["error"].is_null());
}

#[tokio::test]
async fn app_users_routes_refuse_a_deleting_project_and_another_tenants() {
    let au = au_env(false).await;
    let theirs = au
        .metadata
        .create_project(TenantId::new(), "Theirs")
        .await
        .unwrap();
    let resp = send(
        &au.app,
        Method::GET,
        &format!("/v1/projects/{}/app-users", theirs.id),
        "org-1",
        None,
    )
    .await;
    assert_eq!(resp.status(), 404);
    au.metadata
        .begin_project_delete(au.tenant, au.project.id)
        .await
        .unwrap();
    for (method, rest) in [
        (Method::GET, "/app-users"),
        (Method::DELETE, "/app-users"),
        (Method::GET, "/users"),
    ] {
        let (status, body) = au.call(method, rest, None).await;
        assert_eq!(
            (status, code_of(&body)),
            (409, "project_deleting"),
            "{rest}: {body}"
        );
    }
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "project_deleting"),
        "{body}"
    );
}

#[tokio::test]
async fn enable_app_users_202_publishes() {
    let au = au_env(true).await;
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "enabling");
    assert_eq!(
        (
            body["pg_database"].as_str(),
            body["schema"].as_str(),
            body["table"].as_str()
        ),
        (Some("web1"), Some("public"), Some("users"))
    );
    assert_eq!(
        (
            body["email_verification"].as_bool(),
            body["on_delete"].as_str()
        ),
        (Some(false), Some("delete"))
    );
    assert_eq!(
        au.jobs_published(),
        vec![AppUsersJob::Enable {
            project_id: au.project.id
        }]
    );
    assert_eq!(
        *au.jobs.app_users_checks.lock().unwrap(),
        vec!["web1.public.users".to_string()]
    );
    assert_eq!(au.auth().status, AppUsersStatus::Enabling);
}

#[tokio::test]
async fn put_while_enabling_republishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabling);
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(
        au.jobs_published(),
        vec![AppUsersJob::Enable {
            project_id: au.project.id
        }]
    );
    assert!(
        au.jobs.app_users_checks.lock().unwrap().is_empty(),
        "an existing location isn't checked again"
    );
}

#[tokio::test]
async fn delete_while_disabling_republishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(
        au.jobs_published(),
        vec![AppUsersJob::Disable {
            project_id: au.project.id
        }]
    );
}

#[tokio::test]
async fn reserved_schema_is_422() {
    let au = au_env(true).await;
    for schema in ["chelabase", "information_schema", "pg_x", "pg_catalog"] {
        let mut body = put_body(&au);
        body["schema"] = json!(schema);
        let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
        assert_eq!(status, 422, "{schema}: {answer}");
    }
    assert!(au.jobs_published().is_empty());
    assert!(au.metadata.project_auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn app_users_enable_refusals() {
    let au = au_env(true).await;
    let put = |extra: Value| {
        let mut body = put_body(&au);
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    // Identifiers and on_delete.
    for bad in [
        json!({"schema": "Bad-Name"}),
        json!({"table": "1x"}),
        json!({"table": ""}),
        json!({"on_delete": "keep"}),
        json!({"pg_database": ""}),
    ] {
        let (status, body) = au
            .call(Method::PUT, "/app-users", Some(put(bad.clone())))
            .await;
        assert_eq!(status, 422, "{bad}: {body}");
    }
    // Another project's database (same tenant) is unknown to this one.
    let other = au
        .metadata
        .create_project(au.tenant, "Other")
        .await
        .unwrap();
    let foreign = project_database(au.tenant, other.id, DatabaseStatus::Ready);
    let foreign_id = foreign.database_id;
    au.metadata.insert_database(foreign);
    let (status, body) = au
        .call(
            Method::PUT,
            "/app-users",
            Some(json!({"database_id": foreign_id})),
        )
        .await;
    assert_eq!((status, code_of(&body)), (404, "not_found"), "{body}");
    // Another tenant's.
    let theirs = project_database(TenantId::new(), ProjectId::new(), DatabaseStatus::Ready);
    let theirs_id = theirs.database_id;
    au.metadata.insert_database(theirs);
    let (status, _) = au
        .call(
            Method::PUT,
            "/app-users",
            Some(json!({"database_id": theirs_id})),
        )
        .await;
    assert_eq!(status, 404);
    // Not ready.
    let pending = project_database(au.tenant, au.project.id, DatabaseStatus::Provisioning);
    let pending_id = pending.database_id;
    au.metadata.insert_database(pending);
    let (status, body) = au
        .call(
            Method::PUT,
            "/app-users",
            Some(json!({"database_id": pending_id})),
        )
        .await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "database_not_ready"),
        "{body}"
    );
    // The data API is off on a child branch.
    let child = au.metadata.add_branch(
        au.db,
        "dev",
        au.main,
        BranchKind::Normal,
        BranchStatus::Ready,
    );
    au.metadata
        .add_branch_db(child, "web1", "owner", DatabaseOrigin::Main);
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "data_api_not_enabled"),
        "{body}"
    );
    au.metadata
        .set_data_api(child, "web1", chelabase_metadata::DataApiState::On);
    // The table exists.
    *au.jobs.app_users_check_reply.lock().unwrap() = Some(AppUsersCheckReply {
        ok: false,
        code: Some("app_users_table_exists".into()),
        error: Some("exists".into()),
    });
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "app_users_table_exists"),
        "{body}"
    );
    // Any other worker failure is a 503.
    *au.jobs.app_users_check_reply.lock().unwrap() = Some(AppUsersCheckReply {
        ok: false,
        code: Some("boom".into()),
        error: Some("no".into()),
    });
    let (status, _) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(status, 503);
    assert!(au.jobs_published().is_empty());
    assert!(au.metadata.project_auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn put_again_updates_settings_200() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let mut body = put_body(&au);
    body["email_verification"] = json!(true);
    body["on_delete"] = json!("mark");
    let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        (
            answer["email_verification"].as_bool(),
            answer["on_delete"].as_str()
        ),
        (Some(true), Some("mark"))
    );
    assert_eq!(answer["status"], "enabled");
    assert_eq!(au.auth().on_delete, OnDelete::Mark);
    assert!(au.jobs_published().is_empty());
}

#[tokio::test]
async fn put_with_other_location_409_location_fixed() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    for extra in [
        json!({"schema": "app"}),
        json!({"table": "people"}),
        json!({"pg_database": "web2"}),
    ] {
        let mut body = put_body(&au);
        body[extra.as_object().unwrap().keys().next().unwrap()] =
            extra.as_object().unwrap().values().next().unwrap().clone();
        let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
        assert_eq!(
            (status, code_of(&answer)),
            (409, "app_users_location_fixed"),
            "{extra}: {answer}"
        );
    }
    let other = project_database(au.tenant, au.project.id, DatabaseStatus::Ready);
    let other_id = other.database_id;
    au.metadata.insert_database(other);
    let (status, answer) = au
        .call(
            Method::PUT,
            "/app-users",
            Some(json!({"database_id": other_id})),
        )
        .await;
    assert_eq!(
        (status, code_of(&answer)),
        (409, "app_users_location_fixed"),
        "{answer}"
    );
    assert_eq!(au.auth().database_id, au.db);
}

#[tokio::test]
async fn retry_after_failed_202() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Failed);
    au.metadata
        .project_auths
        .lock()
        .unwrap()
        .get_mut(&au.project.id)
        .unwrap()
        .error = Some("boom".into());
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "enabling");
    assert!(body["error"].is_null());
    assert_eq!(
        au.jobs_published(),
        vec![AppUsersJob::Enable {
            project_id: au.project.id
        }]
    );
}

#[tokio::test]
async fn disable_refused_while_users_exist() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.add_user("a@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "app_users_has_users"),
        "{body}"
    );
    assert!(au.jobs_published().is_empty());
    assert_eq!(au.auth().status, AppUsersStatus::Enabled);
}

#[tokio::test]
async fn disable_202_publishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    // A user of another project's pool doesn't count.
    au.add_user_in(&Pool::App(uuid::Uuid::new_v4()), "elsewhere@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "disabling");
    assert_eq!(
        au.jobs_published(),
        vec![AppUsersJob::Disable {
            project_id: au.project.id
        }]
    );
    assert_eq!(au.auth().status, AppUsersStatus::Disabling);
}

#[tokio::test]
async fn data_api_disable_refused_on_auth_database() {
    let au = au_env(false).await;
    au.metadata
        .add_branch_db(au.main, "web2", "owner", DatabaseOrigin::Sql);
    au.metadata
        .set_data_api(au.main, "web2", chelabase_metadata::DataApiState::On);
    au.set_auth(AppUsersStatus::Enabled);
    au.metadata
        .project_auths
        .lock()
        .unwrap()
        .get_mut(&au.project.id)
        .unwrap()
        .pg_database = "web2".into();
    let base = format!(
        "/v1/databases/{}/branches/{}/pg-databases/web2/data-api",
        au.db, au.main
    );
    for query in ["", "?remove=true"] {
        let resp = send(
            &au.app,
            Method::DELETE,
            &format!("{base}{query}"),
            "org-1",
            None,
        )
        .await;
        let body = error_of(resp, 409).await;
        assert_eq!(code_of(&body), "conflict", "{body}");
        assert_eq!(
            message_of(&body),
            "app users use this database: disable app users first"
        );
    }
    assert!(au.jobs.data_api_calls.lock().unwrap().is_empty());
    // Another Postgres database of the same Chelabase database is free to go.
    au.metadata
        .add_branch_db(au.main, "web3", "owner", DatabaseOrigin::Sql);
    let resp = send(
        &au.app,
        Method::DELETE,
        &format!(
            "/v1/databases/{}/branches/{}/pg-databases/web3/data-api",
            au.db, au.main
        ),
        "org-1",
        None,
    )
    .await;
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn list_projects_batches_the_auth_read() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mut ids = vec![];
    for name in ["A", "B", "C"] {
        ids.push(metadata.create_project(tenant, name).await.unwrap().id);
    }
    metadata.project_auths.lock().unwrap().insert(
        ids[1],
        ProjectAuth {
            project_id: ids[1],
            status: AppUsersStatus::Enabled,
            error: None,
            database_id: DatabaseId::new(),
            pg_database: "web1".into(),
            schema_name: "public".into(),
            table_name: "users".into(),
            email_verification: false,
            on_delete: OnDelete::Delete,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    );
    let body: Value = send(&app, Method::GET, "/v1/projects", "org-1", None)
        .await
        .json()
        .await
        .unwrap();
    let states: Vec<_> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["app_users"].as_str())
        .collect();
    assert_eq!(states, vec![None, Some("enabled"), None]);
    assert_eq!(metadata.project_auth_reads.load(Ordering::SeqCst), 0);
    assert_eq!(metadata.project_auth_batch_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn put_while_disabling_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    let (status, body) = au
        .call(Method::PUT, "/app-users", Some(put_body(&au)))
        .await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
    assert!(au.jobs_published().is_empty());
    assert_eq!(au.auth().status, AppUsersStatus::Disabling);
}

#[tokio::test]
async fn disable_retry_while_disabling_skips_the_users_check() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    // The pool still lists a user (or is gone): the retry must not look.
    au.add_user("a@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(
        au.jobs_published(),
        vec![AppUsersJob::Disable {
            project_id: au.project.id
        }]
    );
    assert!(au.fake().calls().is_empty(), "{:?}", au.fake().calls());
}

// ---- User admin ---------------------------------------------------------------------

/// The identity user first, then the row (the table's constraints speak), then the commit.
#[tokio::test]
async fn create_user_creates_identity_then_inserts_then_commits() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "new@x.test", "password": AU_PASSWORD})),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        (
            body["email"].as_str(),
            body["email_verified"].as_bool(),
            body["state"].as_str()
        ),
        (Some("new@x.test"), Some(true), Some("active"))
    );
    let id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    // The pool had the user before the row went in.
    assert_eq!(
        au.udb.journal(),
        vec![
            "begin:new@x.test:users=1".to_string(),
            "commit:users=1".to_string()
        ]
    );
    let user = au
        .user(id)
        .await
        .expect("the identity user, with the row's id");
    assert_eq!(
        (user.email.as_str(), user.email_verified, user.banned),
        ("new@x.test", true, false)
    );
    let target = au.udb.targets.lock().unwrap()[0].clone();
    let loopback = std::net::Ipv4Addr::LOCALHOST.to_string();
    assert_eq!(
        (
            target.branch_id,
            target.host.as_str(),
            target.port,
            target.database.as_str(),
            target.user.as_str(),
            target.password.as_str()
        ),
        (
            au.main.as_uuid(),
            loopback.as_str(),
            55432,
            "web1",
            "chelabase_auth_sync",
            "sync-pw"
        )
    );
    assert_eq!(
        (target.schema.as_str(), target.table.as_str()),
        ("public", "users")
    );
    let row = au.udb.rows.lock().unwrap()[0].clone();
    assert_eq!((row.id, row.email_verified), (id, true));
    assert!(row.data.is_empty());
    let syncs = au.syncs();
    assert_eq!(syncs.len(), 1);
    assert_eq!(
        (
            syncs[0].user_id,
            syncs[0].origin_branch_id,
            syncs[0].data.is_none()
        ),
        (id, Some(au.main), true)
    );
    assert_eq!(syncs[0].project_id, au.project.id);
}

// ---- T10 I3: the app-users sync reaches the compute's control address over TLS ----

/// A compute client pair written into a fresh directory (kept alive with the guard).
fn written_client_pair() -> (
    tempfile::TempDir,
    Arc<orchestrator::compute_tls::ComputeTls>,
) {
    use chelabase_neon::compute_tls::{ComputeCa, ORCHESTRATOR_CN, write_client_pair};
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
    let tls = Arc::new(orchestrator::compute_tls::ComputeTls::new(
        dir.path().to_path_buf(),
    ));
    (dir, tls)
}

// The sync's target carries the same compute TLS as every other compute connection: the
// compute's certificate name and the loaded client pair (taken from one snapshot).
#[tokio::test]
async fn app_user_sync_reaches_the_compute_over_tls() {
    let (_dir, compute_tls) = written_client_pair();
    let fingerprint = compute_tls.get().expect("the pair is loaded").fingerprint();
    let (au, endpoint) = au_env_tls(compute_tls).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "new@x.test", "password": AU_PASSWORD})),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let target = au.udb.targets.lock().unwrap()[0].clone();
    let tls = target
        .tls
        .as_ref()
        .expect("the sync uses TLS to the compute");
    assert_eq!(
        tls.server_name,
        chelabase_neon::compute_tls::server_name(&endpoint)
    );
    assert_eq!(tls.fingerprint, fingerprint);
    // The address stays the compute's (control) address.
    assert_eq!(
        (target.host.as_str(), target.port),
        (std::net::Ipv4Addr::LOCALHOST.to_string().as_str(), 55432)
    );
}

// R-T10a: no client pair loaded yet is 503 `compute_tls_failed`, never a plaintext
// connection; the log names the branch and the endpoint (RT3-7, T10 m2). No identity user
// is made either: the target is resolved first.
#[tokio::test]
async fn app_user_sync_without_a_client_pair_is_compute_tls_failed() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let empty = tempfile::tempdir().unwrap();
    let compute_tls = Arc::new(orchestrator::compute_tls::ComputeTls::new(
        empty.path().to_path_buf(),
    ));
    let (au, endpoint) = au_env_tls(compute_tls).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "new@x.test", "password": AU_PASSWORD})),
        )
        .await;
    assert_eq!(
        (status, code_of(&body)),
        (503, "compute_tls_failed"),
        "{body}"
    );
    assert!(
        au.udb.journal().is_empty(),
        "no connection was tried: {:?}",
        au.udb.journal()
    );
    assert!(au.user_by_email("new@x.test").await.is_none());
    let text = logs.text();
    let line = text
        .lines()
        .find(|l| l.contains("no compute client certificate loaded"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains(&format!("branch_id={}", au.main)), "{line}");
    assert!(line.contains(&format!("endpoint_id={endpoint}")), "{line}");
}

#[tokio::test]
async fn create_user_email_verified_false_is_passed_on() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "u@x.test", "password": AU_PASSWORD, "email_verified": false})),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["email_verified"], false);
    assert!(!au.udb.rows.lock().unwrap()[0].email_verified);
    assert!(!au.user_by_email("u@x.test").await.unwrap().email_verified);
}

/// The row refused (its constraints, its unique email, or the commit): the identity user made
/// for it is deleted again, and nothing is synced.
#[tokio::test]
async fn create_user_row_refused_deletes_the_identity_user() {
    for (fault, status, code) in [
        (UdbFault::CommitFails, 503, "unavailable"),
        (UdbFault::Constraint, 422, "constraint_violation"),
        (UdbFault::EmailTaken, 409, "email_taken"),
    ] {
        let au = au_env(true).await;
        au.set_auth(AppUsersStatus::Enabled);
        au.seal_sync_role();
        *au.udb.fault.lock().unwrap() = Some(fault);
        let (got, body) = au
            .call(
                Method::POST,
                "/users",
                Some(json!({"email": "n@x.test", "password": AU_PASSWORD})),
            )
            .await;
        assert_eq!((got, code_of(&body)), (status, code), "{fault:?}: {body}");
        if fault == UdbFault::Constraint {
            assert_eq!(body["error"]["constraint"], "users_nick_check");
        }
        assert!(au.user_by_email("n@x.test").await.is_none(), "{fault:?}");
        assert!(
            au.pool_writes()
                .iter()
                .any(|c| c.starts_with("DELETE /users/")),
            "{fault:?}: {:?}",
            au.pool_calls()
        );
        assert!(au.syncs().is_empty(), "{fault:?}");
    }
}

#[tokio::test]
async fn create_user_email_taken_and_weak_password() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    // The pool has the email already (whatever the table says): taken, and no row is tried.
    au.add_user("dup@x.test");
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "DUP@x.test", "password": AU_PASSWORD})),
        )
        .await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert!(au.udb.journal().is_empty(), "{:?}", au.udb.journal());
    // The identity service's password rule.
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "w@x.test", "password": "short"})),
        )
        .await;
    assert_eq!((status, code_of(&body)), (422, "weak_password"), "{body}");
    assert!(au.udb.journal().is_empty());
    assert!(au.user_by_email("w@x.test").await.is_none());
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn list_users_pages_and_searches() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    for i in 0..55 {
        au.add_user(&format!("user{i:02}@x.test"));
    }
    au.add_user_in(&Pool::App(uuid::Uuid::new_v4()), "stranger@x.test");
    let (status, page) = au.call(Method::GET, "/users", None).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["users"].as_array().unwrap().len(), 50);
    let token = page["next_page_token"]
        .as_str()
        .expect("next token")
        .to_string();
    assert_eq!(page["users"][0]["state"], "active");
    let (status, page2) = au
        .call(Method::GET, &format!("/users?page_token={token}"), None)
        .await;
    assert_eq!(status, 200, "{page2}");
    assert_eq!(page2["users"].as_array().unwrap().len(), 5);
    assert!(page2["next_page_token"].is_null());
    let (_, found) = au.call(Method::GET, "/users?search=USER07", None).await;
    assert_eq!(found["users"].as_array().unwrap().len(), 1);
    assert_eq!(found["users"][0]["email"], "user07@x.test");
    assert!(found["next_page_token"].is_null());
    let (_, none) = au.call(Method::GET, "/users?search=stranger", None).await;
    assert!(
        none["users"].as_array().unwrap().is_empty(),
        "another pool's user: {none}"
    );
    let (status, bad) = au
        .call(Method::GET, "/users?page_token=not-a-token", None)
        .await;
    assert_eq!(status, 422, "{bad}");
}

#[tokio::test]
async fn get_user_and_sessions() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    let (status, body) = au.call(Method::GET, &format!("/users/{id}"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["id"].as_str(), body["email"].as_str()),
        (Some(id.to_string().as_str()), Some("a@x.test"))
    );
    let (status, sessions) = au
        .call(Method::GET, &format!("/users/{id}/sessions"), None)
        .await;
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    for key in ["id", "created_at", "last_used_at", "expires_at"] {
        assert!(sessions[0][key].is_string(), "{key}");
    }
    assert!(sessions[0].get("identity_session_id").is_none());
    let (status, _) = au
        .call(
            Method::GET,
            &format!("/users/{}", uuid::Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!(status, 404);
}

/// `password` is retired from the update: the orchestrator never changes a password after
/// create (users reset their own). Sending it is a 422 and changes nothing.
#[tokio::test]
async fn update_with_a_password_is_422_and_changes_nothing() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    for patch in [
        json!({"password": "brand-new-pw-1"}),
        json!({"email": "new@x.test", "password": "brand-new-pw-1"}),
    ] {
        let (status, body) = au
            .call(Method::PATCH, &format!("/users/{id}"), Some(patch.clone()))
            .await;
        assert_eq!(
            (status, code_of(&body)),
            (422, "invalid_request"),
            "{patch}: {body}"
        );
    }
    assert_eq!(au.user(id).await.unwrap().email, "a@x.test");
    assert_eq!(au.live_sessions(id), 1);
    assert!(au.pool_writes().is_empty(), "{:?}", au.pool_calls());
    assert!(au.syncs().is_empty());
}

/// A new email ends the user's sessions: the app sessions first, then the identity sessions,
/// then the sync.
#[tokio::test]
async fn update_email_and_flag() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{id}"),
            Some(json!({"email": "b@x.test"})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let user = au.user(id).await.unwrap();
    assert_eq!(
        (user.email.as_str(), user.email_verified),
        ("b@x.test", true)
    );
    assert_eq!(au.live_sessions(id), 0);
    assert_eq!(au.fake().session_count(&au.pool()), 0);
    assert_eq!(
        au.pool_writes(),
        [
            format!("PUT /users/{id}/email"),
            format!("DELETE /users/{id}/sessions")
        ]
    );
    // A new address can be set unverified.
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{id}"),
            Some(json!({"email": "c@x.test", "email_verified": false})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let user = au.user(id).await.unwrap();
    assert_eq!(
        (user.email.as_str(), user.email_verified),
        ("c@x.test", false)
    );
    assert_eq!(body["email_verified"], false);
    assert_eq!(au.syncs().len(), 2);
}

/// The Studio drawer's "Email verified" toggle: the flag alone sets it on the current address,
/// and ends no session.
#[tokio::test]
async fn update_email_verified_alone_marks_the_current_address() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    au.fake()
        .client()
        .set_email(&au.pool(), &id.to_string(), "a@x.test", false)
        .await
        .unwrap();
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{id}"),
            Some(json!({"email_verified": true})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["email"].as_str(), body["email_verified"].as_bool()),
        (Some("a@x.test"), Some(true))
    );
    assert!(au.user(id).await.unwrap().email_verified);
    assert_eq!(au.live_sessions(id), 1, "the address didn't change");
    assert_eq!(au.syncs().len(), 1);
    // Again: nothing to change, still 200.
    let (status, body) = au
        .call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email_verified": true})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["email_verified"], true);
    assert_eq!(au.live_sessions(id), 1);
    assert_eq!(au.syncs().len(), 1, "nothing changed, nothing to sync");
    // With the address spelled out it's still 200, and revokes: it may be a retry (B2).
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{id}"),
            Some(json!({"email": "a@x.test", "email_verified": true})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(au.live_sessions(id), 0);
    assert_eq!(au.syncs().len(), 1);
    // The verification isn't taken back without a new address: 422, nothing changes.
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{id}"),
            Some(json!({"email_verified": false})),
        )
        .await;
    assert_eq!((status, code_of(&body)), (422, "invalid_request"), "{body}");
    assert!(au.user(id).await.unwrap().email_verified);
    // An unverified user asked to stay unverified: nothing to do, 200.
    let other = au.add_user("b@x.test");
    au.fake()
        .client()
        .set_email(&au.pool(), &other.to_string(), "b@x.test", false)
        .await
        .unwrap();
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{other}"),
            Some(json!({"email_verified": false})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["email_verified"], false);
}

/// Already in the state asked for is a 409, never a 503.
#[tokio::test]
async fn state_changes_already_done_are_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    let (status, body) = au
        .call(Method::POST, &format!("/users/{id}/reactivate"), None)
        .await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
    let (status, body) = au
        .call(Method::POST, &format!("/users/{id}/deactivate"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = au
        .call(Method::POST, &format!("/users/{id}/deactivate"), None)
        .await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
}

#[tokio::test]
async fn update_email_taken_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let a = au.add_user("a@x.test");
    au.add_user("b@x.test");
    au.add_session(a);
    let (status, body) = au
        .call(
            Method::PATCH,
            &format!("/users/{a}"),
            Some(json!({"email": "B@x.test"})),
        )
        .await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert_eq!(au.user(a).await.unwrap().email, "a@x.test");
    assert_eq!(au.live_sessions(a), 1, "a refused change ends no session");
    assert!(au.syncs().is_empty());
    // The user's own address isn't "taken".
    let (status, _) = au
        .call(
            Method::PATCH,
            &format!("/users/{a}"),
            Some(json!({"email": "a@x.test"})),
        )
        .await;
    assert_eq!(status, 200);
}

/// AC 3: deactivate is a ban. The user's app sessions are revoked first (so their refresh
/// tokens are refused), the ban ends their identity sessions and refuses sign-in, and the sync
/// job follows. Reactivate unbans.
#[tokio::test]
async fn deactivate_revokes_refresh() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    let other = au.add_user("b@x.test");
    au.add_session(id);
    au.add_session(id);
    au.add_session(other);
    let (status, body) = au
        .call(Method::POST, &format!("/users/{id}/deactivate"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "inactive");
    assert!(au.user(id).await.unwrap().banned);
    assert_eq!(
        au.live_sessions(id),
        0,
        "the refresh tokens' sessions are revoked"
    );
    assert_eq!(au.live_sessions(other), 1);
    assert_eq!(
        au.fake().session_count(&au.pool()),
        1,
        "only the other user's identity session is left"
    );
    assert_eq!(au.pool_writes(), [format!("POST /users/{id}/ban")]);
    assert_eq!(au.syncs().len(), 1);
    assert_eq!(au.syncs()[0].user_id, id);
    let refused = au
        .fake()
        .client()
        .sign_in(&au.pool(), "a@x.test", AU_PASSWORD)
        .await
        .unwrap_err();
    assert_eq!(refused, chelabase_identity::IdentityError::Banned);

    let (status, body) = au
        .call(Method::POST, &format!("/users/{id}/reactivate"), None)
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "active");
    assert!(!au.user(id).await.unwrap().banned);
    assert_eq!(au.syncs().len(), 2);
}

/// AC 5: with the identity service down, a deactivate answers 503 `identity_unavailable`, never
/// success, and the user's app sessions were revoked before it was asked.
#[tokio::test]
async fn identity_down_503_after_metadata_revoke() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    au.fake().set_down(true);
    for (method, rest) in [
        (Method::POST, format!("/users/{id}/deactivate")),
        (Method::DELETE, format!("/users/{id}")),
        (Method::DELETE, format!("/users/{id}/sessions")),
    ] {
        au.metadata
            .app_sessions
            .lock()
            .unwrap()
            .iter_mut()
            .for_each(|s| s.revoked_at = None);
        let (status, body) = au.call(method.clone(), &rest, None).await;
        assert_eq!(
            (status, code_of(&body)),
            (503, "identity_unavailable"),
            "{method} {rest}: {body}"
        );
        assert_eq!(
            au.live_sessions(id),
            0,
            "{method} {rest}: the metadata revoke ran first"
        );
    }
    assert!(au.syncs().is_empty());
    au.fake().set_down(false);
    let user = au.user(id).await.unwrap();
    assert!(!user.banned, "nothing changed in the pool");
}

#[tokio::test]
async fn revoke_sessions_is_204() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    let (status, _) = au
        .call(Method::DELETE, &format!("/users/{id}/sessions"), None)
        .await;
    assert_eq!(status, 204);
    assert_eq!(au.live_sessions(id), 0);
    assert_eq!(au.fake().session_count(&au.pool()), 0);
    assert_eq!(au.pool_writes(), [format!("DELETE /users/{id}/sessions")]);
}

/// AC 4: a deleted user is gone from the pool and from `app_sessions`, and the sync job that
/// removes the row from every branch's users table is published.
#[tokio::test]
async fn delete_clears_everywhere() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    let other = au.add_user("b@x.test");
    au.add_session(id);
    au.add_session(other);
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(status, 204);
    assert!(au.user(id).await.is_none());
    assert!(au.user(other).await.is_some());
    assert_eq!(au.live_sessions(id), 0);
    assert_eq!(au.live_sessions(other), 1);
    assert_eq!(au.fake().session_count(&au.pool()), 1);
    assert_eq!(au.syncs().len(), 1);
    assert_eq!(
        (au.syncs()[0].user_id, au.syncs()[0].data.is_none()),
        (id, true)
    );
    // Again: the user is gone, 404.
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(status, 404);
}

/// The app sessions are revoked first: when that fails, the user is not deleted.
#[tokio::test]
async fn delete_user_metadata_revoke_failure_deletes_nothing() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    *au.metadata.fail_revoke.lock().unwrap() = true;
    for (method, rest) in [
        (Method::DELETE, format!("/users/{id}")),
        (Method::POST, format!("/users/{id}/deactivate")),
    ] {
        let (status, body) = au.call(method.clone(), &rest, None).await;
        assert_eq!(
            (status, code_of(&body)),
            (500, "internal"),
            "{method} {rest}: {body}"
        );
    }
    let user = au.user(id).await.expect("not deleted");
    assert!(!user.banned);
    assert!(au.pool_writes().is_empty(), "{:?}", au.pool_calls());
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn other_pool_user_404() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let elsewhere = Pool::App(uuid::Uuid::new_v4());
    let id = au.add_user_in(&elsewhere, "stranger@x.test");
    for (method, rest, body) in [
        (Method::GET, format!("/users/{id}"), None),
        (
            Method::PATCH,
            format!("/users/{id}"),
            Some(json!({"email": "new@x.test"})),
        ),
        (Method::POST, format!("/users/{id}/deactivate"), None),
        (Method::POST, format!("/users/{id}/reactivate"), None),
        (Method::GET, format!("/users/{id}/sessions"), None),
        (Method::DELETE, format!("/users/{id}/sessions"), None),
        (Method::DELETE, format!("/users/{id}"), None),
    ] {
        let (status, answer) = au.call(method.clone(), &rest, body).await;
        assert_eq!(status, 404, "{method} {rest}: {answer}");
    }
    assert!(
        au.fake()
            .calls()
            .iter()
            .all(|c| !c.contains(&elsewhere.to_string()))
    );
    let user = au
        .fake()
        .client()
        .get_user(&elsewhere, &id.to_string())
        .await
        .unwrap();
    assert_eq!(
        (user.email.as_str(), user.banned),
        ("stranger@x.test", false)
    );
    // Only the DELETE publishes, for this project: its sync finds no such user here.
    assert_eq!(au.syncs().len(), 1);
}

#[tokio::test]
async fn user_routes_need_enabled_app_users() {
    let au = au_env(true).await;
    let (status, body) = au.call(Method::GET, "/users", None).await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "app_users_disabled"),
        "{body}"
    );
    au.set_auth(AppUsersStatus::Enabling);
    let (status, body) = au
        .call(
            Method::POST,
            "/users",
            Some(json!({"email": "a@x.test", "password": AU_PASSWORD})),
        )
        .await;
    assert_eq!(
        (status, code_of(&body)),
        (409, "app_users_disabled"),
        "{body}"
    );
}

#[tokio::test]
async fn routes_503_without_identity() {
    let au = au_env(false).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = uuid::Uuid::new_v4();
    for (method, rest, body) in [
        (Method::GET, "/users".to_string(), None),
        (
            Method::POST,
            "/users".to_string(),
            Some(json!({"email": "a@x.test", "password": AU_PASSWORD})),
        ),
        (Method::GET, format!("/users/{id}"), None),
        (
            Method::PATCH,
            format!("/users/{id}"),
            Some(json!({"email_verified": true})),
        ),
        (Method::DELETE, format!("/users/{id}"), None),
        (Method::DELETE, "/app-users".to_string(), None),
    ] {
        let (status, answer) = au.call(method.clone(), &rest, body).await;
        assert_eq!(
            (status, code_of(&answer)),
            (503, "identity_unavailable"),
            "{method} {rest}: {answer}"
        );
    }
    // Reading the settings needs no identity service.
    let (status, _) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(status, 200);
}

/// A refusal that isn't the identity service's own (a proxy's page, a code this client has no
/// name for) is 503 `identity_unavailable`, never 404 or 409, and the user is untouched.
#[tokio::test]
async fn a_foreign_refusal_is_503() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    for (what, method, path) in [
        (
            "deactivate",
            Method::POST,
            format!("/users/{id}/deactivate"),
        ),
        ("delete", Method::DELETE, format!("/users/{id}")),
    ] {
        for status in [404, 400] {
            // The route's first call on the user answers with a code the client has no name for.
            au.fake().fail(&format!("/users/{id}"), status, "odd", 1);
            let (got, answer) = au.call(method.clone(), &path, None).await;
            assert_eq!(
                (got, code_of(&answer)),
                (503, "identity_unavailable"),
                "{what} {status}: {answer}"
            );
        }
    }
    let user = au.user(id).await.expect("the user is untouched");
    assert!(!user.banned);
}

/// A member lives in the members pool, never a project's: the app-user routes can't reach it.
#[tokio::test]
async fn app_user_routes_cannot_reach_a_member() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let member = au.add_user_in(&Pool::Members, "m@x.test");
    au.fake().ban(&Pool::Members, &member.to_string());
    let (status, body) = au
        .call(Method::POST, &format!("/users/{member}/reactivate"), None)
        .await;
    assert_eq!((status, code_of(&body)), (404, "not_found"), "{body}");
    assert!(
        au.fake()
            .calls()
            .iter()
            .all(|c| !c.contains("/pools/members")),
        "{:?}",
        au.fake().calls()
    );
    let user = au
        .fake()
        .client()
        .get_user(&Pool::Members, &member.to_string())
        .await
        .unwrap();
    assert!(user.banned, "still banned");
}

impl FakeMetadata {
    fn set_data_api(&self, branch: BranchId, name: &str, state: chelabase_metadata::DataApiState) {
        let mut all = self.branch_dbs.lock().unwrap();
        let row = all
            .get_mut(&branch)
            .unwrap()
            .iter_mut()
            .find(|d| d.name == name)
            .unwrap();
        row.data_api = state;
        // As the store does: any state but off means an enable began.
        row.data_api_bootstrapped |= state != chelabase_metadata::DataApiState::Off;
    }
}

// --- White-room audit of m7: every access-ending path finishes or can be finished ------------

/// B2: a metadata revoke that failed after the address changed is finished by the retry, and
/// it didn't stop the identity revoke either.
#[tokio::test]
async fn email_change_retry_revokes_after_metadata_failure() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    *au.metadata.fail_revoke.lock().unwrap() = true;
    let (status, body) = au
        .call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "new@x.test"})))
        .await;
    assert_eq!(status, 500, "{body}");
    assert_eq!(au.fake().session_count(&au.pool()), 0, "the identity revoke still ran");
    *au.metadata.fail_revoke.lock().unwrap() = false;
    let (status, body) = au
        .call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "new@x.test"})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(au.live_sessions(id), 0, "retry said 200; the refresh token of the old address still works");
}

/// B2: the same for an identity revoke that failed.
#[tokio::test]
async fn email_change_retry_revokes_after_identity_failure() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_session(id);
    au.fake().fail(&format!("DELETE /internal/v1/pools/{}/users/{id}/sessions", au.pool()), 503, "internal", 1);
    let (status, body) = au
        .call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "new@x.test"})))
        .await;
    assert_eq!((status, code_of(&body)), (503, "identity_unavailable"), "{body}");
    let (status, body) = au
        .call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "new@x.test"})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(au.fake().session_count(&au.pool()), 0, "retry said 200; the identity session is alive");
}

/// B2: an address the identity service refuses changes nothing, sessions included.
#[tokio::test]
async fn a_refused_email_change_keeps_the_sessions() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("a@x.test");
    au.add_user("taken@x.test");
    au.add_session(id);
    let (status, body) = au
        .call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "taken@x.test"})))
        .await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert_eq!(au.live_sessions(id), 1);
}

/// B3: every route that promises "a 503 still ended them" does, with no client at all.
#[tokio::test]
async fn a_503_without_a_client_still_revoked_sessions() {
    let au = au_env(false).await;
    au.set_auth(AppUsersStatus::Enabled);
    for (method, rest) in [
        (Method::POST, "/deactivate"),
        (Method::DELETE, "/sessions"),
        (Method::DELETE, ""),
    ] {
        let id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();
        au.metadata.app_sessions.lock().unwrap().push(AppSession {
            id: uuid::Uuid::new_v4(),
            project_id: au.project.id,
            user_id: id,
            identity_session_id: "s".into(),
            created_at: now,
            last_used_at: now,
            expires_at: now + chrono::Duration::days(30),
            revoked_at: None,
        });
        let (status, body) = au.call(method.clone(), &format!("/users/{id}{rest}"), None).await;
        assert_eq!((status, code_of(&body)), (503, "identity_unavailable"), "{method} {rest}: {body}");
        assert_eq!(au.live_sessions(id), 0, "{method} {rest}: 503, but the refresh token still works");
    }
}

/// S4: a delete whose answer or sync was lost: the retry's 404 still publishes the sync, so the
/// row can't outlive the user just because reconcile's mass-delete guard trips.
#[tokio::test]
async fn delete_retry_of_a_gone_user_publishes_the_sync() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = uuid::Uuid::new_v4();
    let (status, body) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!((status, code_of(&body)), (404, "not_found"), "{body}");
    assert_eq!(au.syncs().len(), 1);
}
