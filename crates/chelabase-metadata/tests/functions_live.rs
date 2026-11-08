//! Edge functions m2: functions, deployments and per-branch function state in the metadata,
//! inherited by a new branch in its create transaction and purged with a branch or database
//! tombstone. Each test makes its own scratch database (`cm_…`, as `members_live` names and
//! sweeps them) on the workbench stack's metadata Postgres, so the shared metadata database is
//! never migrated.
//!
//! ```text
//! just workbench test -p chelabase-metadata --test functions_live -- --run-ignored only
//! ```

mod functions {
    use std::borrow::Cow;

    use chelabase_metadata::{
        BranchStatus, CreateBranchOutcome, DEMO_PROJECT_ID, DEMO_TENANT_ID, FunctionAuth, FunctionKind,
        MAX_BUNDLE_SIZE, MetadataClient, MetadataConfig, MetadataError, NewBranch, NewDataKey,
        KeyRoleFix, NewDeployment, NewSchedule, NewTrigger, RunAs, RunAsMint, RunAsUnmintable,
        TriggerDelivery, TriggerInstall, TriggerSource,
        TriggerState,
    };
    use chelabase_types::data_key::{self, DataKeyKind};
    use chelabase_secrets::SecretKey;
    use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
    use chrono::{NaiveDate, Utc};
    use sqlx::postgres::PgPoolOptions;
    use sqlx::{AssertSqlSafe, PgPool};
    use uuid::Uuid;

    /// The migration this milestone adds.
    const FUNCTIONS_MIGRATION: i64 = 20261107000002;

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

    struct Scratch {
        meta: PgPool,
        name: String,
        client: MetadataClient,
    }

    impl Scratch {
        /// An empty scratch database, not migrated yet.
        async fn empty() -> Scratch {
            let meta = PgPoolOptions::new()
                .max_connections(1)
                .connect(&url())
                .await
                .expect("metadata Postgres reachable (is the workbench stack up?)");
            let secs = Utc::now().timestamp() as u32;
            let name = format!("cm_{secs:08x}{}", &Uuid::new_v4().simple().to_string()[..4]);
            sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
                .execute(&meta)
                .await
                .unwrap();
            let client = MetadataClient::connect(&MetadataConfig {
                database_url: url_for(&name),
            })
            .await
            .unwrap()
            .with_secrets_key(SecretKey::generate());
            Scratch { meta, name, client }
        }

        async fn new() -> Scratch {
            let s = Scratch::empty().await;
            s.client.migrate().await.unwrap();
            s
        }

        fn pool(&self) -> &PgPool {
            self.client.pool()
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

        /// A database whose main branch is ready (children can be made from it).
        async fn database(&self) -> (DatabaseId, BranchId) {
            let id = DatabaseId::new();
            self.client
                .insert_database_pending(
                    id,
                    TenantId(DEMO_TENANT_ID),
                    ProjectId(DEMO_PROJECT_ID),
                    &format!("d-{id}"),
                )
                .await
                .unwrap();
            sqlx::query("UPDATE databases SET status = 'ready' WHERE id = $1")
                .bind(id.as_uuid())
                .execute(self.pool())
                .await
                .unwrap();
            let main = BranchId::from(id);
            self.ready(main).await;
            (id, main)
        }

        async fn ready(&self, branch: BranchId) {
            sqlx::query("UPDATE branches SET status = 'ready' WHERE id = $1")
                .bind(branch.as_uuid())
                .execute(self.pool())
                .await
                .unwrap();
        }

        async fn branch(&self, db: DatabaseId, parent: BranchId, name: &str) -> BranchId {
            let id = BranchId::new();
            let outcome = self
                .client
                .create_branch(&NewBranch {
                    id,
                    database_id: db,
                    name,
                    parent_branch_id: parent,
                    branched_at_time: None,
                    branched_at_lsn: None,
                })
                .await
                .unwrap();
            assert_eq!(outcome, CreateBranchOutcome::Created);
            self.ready(id).await;
            id
        }

        async fn function(&self, db: DatabaseId, name: &str) -> Uuid {
            self.client
                .create_function(db, name, FunctionKind::Function)
                .await
                .unwrap()
                .id
        }

        async fn deployment(&self, function: Uuid, sha: &str) -> Uuid {
            let spec = serde_json::json!({});
            self.client
                .create_deployment(&NewDeployment {
                    id: Uuid::new_v4(),
                    function_id: function,
                    bundle_sha256: Some(sha),
                    bundle_size: Some(1234),
                    spec: &spec,
                    compatibility_date: date(),
                    created_by: "member-1",
                    note: Some("first"),
                })
                .await
                .unwrap()
                .id
        }

        /// A function deployed and enabled on `branch` with two secrets, a schedule, a table
        /// trigger (installed by hand, as m8's worker would) and a storage trigger, run as
        /// `reporter` with a key.
        async fn full_function(
            &self,
            db: DatabaseId,
            branch: BranchId,
            name: &str,
        ) -> (Uuid, Uuid) {
            let f = self.function(db, name).await;
            let d = self.deployment(f, &sha('a')).await;
            let c = &self.client;
            c.set_active_deployment(branch, f, d).await.unwrap();
            assert!(c.set_enabled(branch, f, true).await.unwrap());
            c.put_secret(branch, f, "API_KEY", "one").await.unwrap();
            c.put_secret(branch, f, "TOKEN", "two").await.unwrap();
            c.add_schedule(&NewSchedule {
                branch_id: branch,
                function_id: f,
                cron: "*/5 * * * *",
                timezone: "Europe/Paris",
                next_run_at: None,
            })
            .await
            .unwrap();
            c.add_trigger(&table_trigger(branch, f, &["insert", "update"]))
                .await
                .unwrap();
            c.add_trigger(&storage_trigger(branch, f, &["object.created"]))
                .await
                .unwrap();
            sqlx::query(
                "UPDATE function_triggers SET state = 'installed'
                 WHERE branch_id = $1 AND source = 'table'",
            )
            .bind(branch.as_uuid())
            .execute(self.pool())
            .await
            .unwrap();
            let key = self.data_key(branch).await;
            sqlx::query(
                "UPDATE branch_functions SET run_as_role = 'reporter', run_as_key_id = $3
                 WHERE branch_id = $1 AND function_id = $2",
            )
            .bind(branch.as_uuid())
            .bind(f)
            .bind(key)
            .execute(self.pool())
            .await
            .unwrap();
            (f, d)
        }

        /// A live secret data key of the branch.
        async fn data_key(&self, branch: BranchId) -> Uuid {
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO data_api_keys (id, branch_id, kind, role, name, prefix, sha256)
                 VALUES ($1, $2, 'secret', 'reporter', 'fn', 'p', sha256($1::text::bytea))",
            )
            .bind(id)
            .bind(branch.as_uuid())
            .execute(self.pool())
            .await
            .unwrap();
            id
        }

        /// A function with a deployment active (and enabled) on `branch`.
        async fn active(&self, db: DatabaseId, branch: BranchId, name: &str) -> Uuid {
            let f = self.function(db, name).await;
            let d = self.deployment(f, &sha('b')).await;
            self.client.set_active_deployment(branch, f, d).await.unwrap();
            assert!(self.client.set_enabled(branch, f, true).await.unwrap());
            f
        }

        /// The live keys `function` manages: `(id, role, role_oid, sealed)`.
        async fn managed_keys(&self, function: Uuid) -> Vec<(Uuid, String, Option<i64>, Option<String>)> {
            sqlx::query_as(
                "SELECT id, role, role_oid, sealed FROM data_api_keys
                 WHERE managed_by_function_id = $1 AND revoked_at IS NULL ORDER BY created_at",
            )
            .bind(function)
            .fetch_all(self.pool())
            .await
            .unwrap()
        }

        /// `(revoked, sealed is null)` of a key.
        async fn key_state(&self, key: Uuid) -> (bool, bool) {
            sqlx::query_as(
                "SELECT revoked_at IS NOT NULL, sealed IS NULL FROM data_api_keys WHERE id = $1",
            )
            .bind(key)
            .fetch_one(self.pool())
            .await
            .unwrap()
        }

        /// `(run_as_role, run_as_key_id)` of the function on the branch.
        async fn run_as_columns(&self, branch: BranchId, f: Uuid) -> (Option<String>, Option<Uuid>) {
            let listed = self.client.list_for_branch(branch).await.unwrap();
            let row = listed.iter().find(|r| r.function_id == f).expect("branch state");
            (row.run_as_role.clone(), row.run_as_key_id)
        }

        /// A compute endpoint linked to `branch`, with its owner role and the roles `reporter`
        /// and `other` (SQL-made copies), as a ready branch has.
        async fn endpoint(&self, branch: BranchId) -> String {
            let ep = format!("ep-{}", &Uuid::new_v4().simple().to_string()[..12]);
            sqlx::query(
                "INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256)
                 VALUES ($1, 't', $2, 'aa')",
            )
            .bind(&ep)
            .bind(Uuid::new_v4().simple().to_string())
            .execute(self.pool())
            .await
            .unwrap();
            sqlx::query("UPDATE branches SET endpoint_id = $2 WHERE id = $1")
                .bind(branch.as_uuid())
                .bind(&ep)
                .execute(self.pool())
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO neon_roles (endpoint_id, name, origin, scram_verifier)
                 VALUES ($1, 'owner', 'owner', 'SCRAM-SHA-256$x')",
            )
            .bind(&ep)
            .execute(self.pool())
            .await
            .unwrap();
            for role in ["reporter", "other"] {
                sqlx::query(
                    "INSERT INTO neon_roles (endpoint_id, name, origin, login) VALUES ($1, $2, 'sql', false)",
                )
                .bind(&ep)
                .bind(role)
                .execute(self.pool())
                .await
                .unwrap();
            }
            ep
        }

        /// Rows in each per-branch function table for `branch`.
        async fn branch_rows(&self, branch: BranchId) -> [i64; 4] {
            let mut out = [0; 4];
            for (i, table) in [
                "branch_functions",
                "branch_function_secrets",
                "function_schedules",
                "function_triggers",
            ]
            .iter()
            .enumerate()
            {
                out[i] = sqlx::query_scalar(AssertSqlSafe(format!(
                    "SELECT count(*) FROM {table} WHERE branch_id = $1"
                )))
                .bind(branch.as_uuid())
                .fetch_one(self.pool())
                .await
                .unwrap();
            }
            out
        }
    }

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 1).unwrap()
    }

    fn sha(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    fn table_trigger<'a>(branch: BranchId, f: Uuid, events: &'a [&'a str]) -> NewTrigger<'a> {
        NewTrigger {
            branch_id: branch,
            function_id: f,
            source: TriggerSource::Table,
            pg_database: "app",
            schema_name: Some("public"),
            table_name: Some("orders"),
            bucket: None,
            prefix: None,
            events,
        }
    }

    fn storage_trigger<'a>(branch: BranchId, f: Uuid, events: &'a [&'a str]) -> NewTrigger<'a> {
        NewTrigger {
            branch_id: branch,
            function_id: f,
            source: TriggerSource::Storage,
            pg_database: "app",
            schema_name: None,
            table_name: None,
            bucket: Some("avatars"),
            prefix: Some("in/"),
            events,
        }
    }

    /// A run-as role with a freshly generated key; the plaintext is the key itself.
    fn run_as(role: &str, oid: i64) -> RunAs {
        let generated = data_key::generate(DataKeyKind::Secret);
        RunAs {
            role: role.to_string(),
            role_oid: oid,
            role_timeline: Some("tl-1".into()),
            role_bypass_rls: false,
            key_id: Uuid::new_v4(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            secret: generated.key,
        }
    }

    fn secrets(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[tokio::test]
    #[ignore]
    async fn inherits_on_branch_create() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (f, d) = s.full_function(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        let c = &s.client;

        let listed = c.list_for_branch(child).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (
                listed[0].function_id,
                listed[0].deployment_id,
                listed[0].enabled
            ),
            (f, d, true)
        );
        // The role carries over; the parent's key never does (m8 mints the child's own).
        assert_eq!(listed[0].run_as_role.as_deref(), Some("reporter"));
        assert_eq!(listed[0].run_as_key_id, None);
        let parent = c.list_for_branch(main).await.unwrap();
        assert!(
            parent[0].run_as_key_id.is_some(),
            "the parent keeps its key"
        );

        let resolved = c
            .resolve(child, "hello")
            .await
            .unwrap()
            .expect("resolves on the child");
        assert_eq!(
            (
                resolved.deployment_id,
                resolved.bundle_sha256.clone(),
                resolved.compatibility_date
            ),
            (d, Some(sha('a')), date())
        );
        assert!(resolved.enabled);

        assert_eq!(
            c.decrypt_secrets(child, f).await.unwrap(),
            secrets(&[("API_KEY", "one"), ("TOKEN", "two")])
        );
        assert_eq!(
            c.secret_names(child, f).await.unwrap(),
            vec!["API_KEY", "TOKEN"]
        );

        let schedules = c.list_schedules(child).await.unwrap();
        assert_eq!(schedules.len(), 1);
        assert_eq!(
            (
                schedules[0].function_id,
                schedules[0].cron.as_str(),
                schedules[0].timezone.as_str()
            ),
            (f, "*/5 * * * *", "Europe/Paris")
        );
        let parent_schedule = &c.list_schedules(main).await.unwrap()[0];
        assert_ne!(schedules[0].id, parent_schedule.id, "a copy has its own id");

        let triggers = c.list_triggers(child).await.unwrap();
        assert_eq!(triggers.len(), 2);
        let table = triggers
            .iter()
            .find(|t| t.source == TriggerSource::Table)
            .unwrap();
        assert_eq!(
            (
                table.schema_name.as_deref(),
                table.table_name.as_deref(),
                table.events.clone()
            ),
            (
                Some("public"),
                Some("orders"),
                vec!["insert".to_string(), "update".to_string()]
            )
        );
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn child_edit_isolated() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (f, d) = s.full_function(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        let c = &s.client;

        let d2 = s.deployment(f, &sha('b')).await;
        c.set_active_deployment(child, f, d2).await.unwrap();
        assert!(c.set_enabled(child, f, false).await.unwrap());
        c.put_secret(child, f, "API_KEY", "changed").await.unwrap();
        assert!(c.delete_secret(child, f, "TOKEN").await.unwrap());
        assert!(
            !c.delete_secret(child, f, "TOKEN").await.unwrap(),
            "already gone"
        );

        let on_main = c.resolve(main, "hello").await.unwrap().unwrap();
        assert_eq!((on_main.deployment_id, on_main.enabled), (d, true));
        assert_eq!(
            c.decrypt_secrets(main, f).await.unwrap(),
            secrets(&[("API_KEY", "one"), ("TOKEN", "two")])
        );
        let on_child = c.resolve(child, "hello").await.unwrap().unwrap();
        assert_eq!((on_child.deployment_id, on_child.enabled), (d2, false));
        assert_eq!(
            c.decrypt_secrets(child, f).await.unwrap(),
            secrets(&[("API_KEY", "changed")])
        );
        // A parent edit after the branch doesn't reach the child either.
        c.put_secret(main, f, "LATER", "x").await.unwrap();
        assert_eq!(c.secret_names(child, f).await.unwrap(), vec!["API_KEY"]);
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn cross_function_deployment_rejected() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (other_db, other_main) = s.database().await;
        let a = s.function(db, "a").await;
        let b = s.function(db, "b").await;
        let da = s.deployment(a, &sha('a')).await;
        let c = &s.client;

        let err = c.set_active_deployment(main, b, da).await.unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        // A function of another database can't be set up on this branch, nor its secrets.
        let foreign = s.function(other_db, "a").await;
        let df = s.deployment(foreign, &sha('c')).await;
        let err = c
            .set_active_deployment(main, foreign, df)
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::FunctionNotFound), "{err:?}");
        let err = c.put_secret(main, foreign, "K", "v").await.unwrap_err();
        assert!(matches!(err, MetadataError::FunctionNotFound), "{err:?}");
        c.set_active_deployment(other_main, foreign, df)
            .await
            .unwrap();
        // The schema refuses a mismatched pair too.
        let raw = sqlx::query(
            "INSERT INTO branch_functions (branch_id, function_id, deployment_id)
             VALUES ($1, $2, $3)",
        )
        .bind(main.as_uuid())
        .bind(b)
        .bind(da)
        .execute(s.pool())
        .await;
        assert!(
            raw.is_err(),
            "a deployment of another function is refused by the schema"
        );
        assert!(c.list_for_branch(main).await.unwrap().is_empty());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn secret_aad_bound_to_branch() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let g = s.function(db, "other").await;
        let c = &s.client;
        c.put_secret(main, f, "API_KEY", "one").await.unwrap();
        c.put_secret(main, f, "TOKEN", "two").await.unwrap();
        c.put_secret(main, g, "API_KEY", "g").await.unwrap();
        let child = s.branch(db, main, "dev").await;
        let raw: String = sqlx::query_scalar(
            "SELECT ciphertext FROM branch_function_secrets
             WHERE branch_id = $1 AND function_id = $2 AND name = 'API_KEY'",
        )
        .bind(main.as_uuid())
        .bind(f)
        .fetch_one(s.pool())
        .await
        .unwrap();
        assert!(!raw.contains("one"), "stored sealed");
        let child_raw: String = sqlx::query_scalar(
            "SELECT ciphertext FROM branch_function_secrets
             WHERE branch_id = $1 AND function_id = $2 AND name = 'API_KEY'",
        )
        .bind(child.as_uuid())
        .bind(f)
        .fetch_one(s.pool())
        .await
        .unwrap();
        assert_ne!(raw, child_raw, "the copy is sealed again for the child");

        // main's ciphertext on the child's row, another name's, another function's: none opens.
        for (branch, function, name) in [
            (child, f, "API_KEY"),
            (main, f, "TOKEN"),
            (main, g, "API_KEY"),
        ] {
            sqlx::query(
                "UPDATE branch_function_secrets SET ciphertext = $4
                 WHERE branch_id = $1 AND function_id = $2 AND name = $3",
            )
            .bind(branch.as_uuid())
            .bind(function)
            .bind(name)
            .bind(&raw)
            .execute(s.pool())
            .await
            .unwrap();
            assert!(
                c.decrypt_secrets(branch, function).await.is_err(),
                "{branch} {function} {name} must not open main's API_KEY"
            );
        }
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn size_and_name_limits() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let c = &s.client;
        let long = format!("a{}", "b".repeat(63));
        for bad in ["../x", "A", "", "-a", "1a", "a_b", "a.b", long.as_str()] {
            let err = c
                .create_function(db, bad, FunctionKind::Function)
                .await
                .unwrap_err();
            assert!(
                matches!(err, MetadataError::InvalidFunction(_)),
                "{bad:?}: {err:?}"
            );
        }
        c.create_function(db, &long[..63], FunctionKind::Function)
            .await
            .unwrap();
        let f = s.function(db, "a").await;
        let err = c
            .create_function(db, "a", FunctionKind::Service)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MetadataError::Conflict),
            "a taken name: {err:?}"
        );

        let spec = serde_json::json!({});
        let deploy = |size: i64, hash: Option<String>| {
            let spec = &spec;
            async move {
                c.create_deployment(&NewDeployment {
                    id: Uuid::new_v4(),
                    function_id: f,
                    bundle_sha256: hash.as_deref(),
                    bundle_size: Some(size),
                    spec,
                    compatibility_date: date(),
                    created_by: "m",
                    note: None,
                })
                .await
            }
        };
        let err = deploy(MAX_BUNDLE_SIZE + 1, Some(sha('a')))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let err = deploy(-1, Some(sha('a'))).await.unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let err = deploy(10, Some("../etc/passwd".into())).await.unwrap_err();
        assert!(
            matches!(err, MetadataError::InvalidFunction(_)),
            "a hash is 64 hex: {err:?}"
        );
        let err = deploy(10, None).await.unwrap_err();
        assert!(
            matches!(err, MetadataError::InvalidFunction(_)),
            "a function has a bundle: {err:?}"
        );
        assert_eq!(MAX_BUNDLE_SIZE, 10 * 1024 * 1024);
        deploy(MAX_BUNDLE_SIZE, Some(sha('a'))).await.unwrap();

        for bad in ["", "1A", "A-B", "a b"] {
            let err = c.put_secret(main, f, bad, "v").await.unwrap_err();
            assert!(
                matches!(err, MetadataError::InvalidFunction(_)),
                "secret {bad:?}: {err:?}"
            );
        }

        // The schema refuses them too.
        let raw_name = sqlx::query(
            "INSERT INTO functions (id, database_id, name, kind) VALUES ($1, $2, 'A', 'function')",
        )
        .bind(Uuid::new_v4())
        .bind(db.as_uuid())
        .execute(s.pool())
        .await;
        assert!(raw_name.is_err(), "the name CHECK");
        let raw_size = sqlx::query(
            "INSERT INTO function_deployments
                 (id, function_id, bundle_sha256, bundle_size, compatibility_date, created_by)
             VALUES ($1, $2, $3, $4, '2026-11-01', 'm')",
        )
        .bind(Uuid::new_v4())
        .bind(f)
        .bind(sha('a'))
        .bind(MAX_BUNDLE_SIZE + 1)
        .execute(s.pool())
        .await;
        assert!(raw_size.is_err(), "the size CHECK");
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn deployments_immutable() {
        let s = Scratch::new().await;
        let (db, _) = s.database().await;
        let f = s.function(db, "a").await;
        let d = s.deployment(f, &sha('a')).await;
        let updated = sqlx::query("UPDATE function_deployments SET note = 'x' WHERE id = $1")
            .bind(d)
            .execute(s.pool())
            .await;
        assert!(updated.is_err(), "a deployment never changes");
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn branch_create_rollback_leaves_nothing() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (f, _) = s.full_function(db, main, "hello").await;
        // The secrets copy is last: a parent secret that can't be opened fails the create after
        // the other rows were copied.
        sqlx::query(
            "UPDATE branch_function_secrets SET ciphertext = 'v1:AAAAAAAAAAAAAAAAAAAAAAAAAAAA'
             WHERE branch_id = $1 AND function_id = $2 AND name = 'TOKEN'",
        )
        .bind(main.as_uuid())
        .bind(f)
        .execute(s.pool())
        .await
        .unwrap();
        let id = BranchId::new();
        let created = s
            .client
            .create_branch(&NewBranch {
                id,
                database_id: db,
                name: "dev",
                parent_branch_id: main,
                branched_at_time: None,
                branched_at_lsn: None,
            })
            .await;
        assert!(created.is_err(), "{created:?}");
        assert_eq!(s.branch_rows(id).await, [0; 4]);
        assert!(
            s.client.get_branch(id).await.unwrap().is_none(),
            "no branch row either"
        );

        // A client without the secrets key can't copy secrets: refused, nothing written.
        let keyless = MetadataClient::connect(&MetadataConfig {
            database_url: url_for(&s.name),
        })
        .await
        .unwrap();
        let (db2, main2) = s.database().await;
        let (_, _) = s.full_function(db2, main2, "hello").await;
        let id2 = BranchId::new();
        let created = keyless
            .create_branch(&NewBranch {
                id: id2,
                database_id: db2,
                name: "dev",
                parent_branch_id: main2,
                branched_at_time: None,
                branched_at_lsn: None,
            })
            .await;
        assert!(created.is_err(), "{created:?}");
        assert_eq!(s.branch_rows(id2).await, [0; 4]);
        assert!(s.client.get_branch(id2).await.unwrap().is_none());
        keyless.pool().close().await;
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn referenced_bundles_keeps_shared() {
        let s = Scratch::new().await;
        let (db, _) = s.database().await;
        let f1 = s.function(db, "one").await;
        let f2 = s.function(db, "two").await;
        s.deployment(f1, &sha('a')).await;
        s.deployment(f2, &sha('a')).await;
        s.deployment(f2, &sha('b')).await;
        let svc = s
            .client
            .create_function(db, "svc", FunctionKind::Service)
            .await
            .unwrap()
            .id;
        let spec = serde_json::json!({"image": "ghcr.io/x/y:1", "port": 8080, "health": "/up"});
        s.client
            .create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: svc,
                bundle_sha256: None,
                bundle_size: None,
                spec: &spec,
                compatibility_date: date(),
                created_by: "m",
                note: None,
            })
            .await
            .unwrap();
        let mut got = s.client.referenced_bundles(db).await.unwrap();
        got.sort();
        assert_eq!(got, vec![sha('a'), sha('b')]);

        assert!(s.client.delete_function(db, f2).await.unwrap());
        assert!(
            !s.client.delete_function(db, f2).await.unwrap(),
            "already gone"
        );
        assert_eq!(
            s.client.referenced_bundles(db).await.unwrap(),
            vec![sha('a')]
        );

        assert!(s.client.live_database(db).await.unwrap());
        assert!(!s.client.live_database(DatabaseId::new()).await.unwrap());
        s.client.mark_deleted(db).await.unwrap();
        assert!(!s.client.live_database(db).await.unwrap());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn purge_on_branch_tombstone() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        s.full_function(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        assert_eq!(s.branch_rows(child).await, [1, 2, 1, 2]);

        s.client.mark_branch_deleted(child).await.unwrap();
        assert_eq!(s.branch_rows(child).await, [0; 4]);
        assert!(s.client.resolve(child, "hello").await.unwrap().is_none());
        assert_eq!(
            s.branch_rows(main).await,
            [1, 2, 1, 2],
            "the parent keeps its rows"
        );
        // A write to the tombstoned branch is refused, so nothing comes back.
        let f = s.client.list_for_branch(main).await.unwrap()[0].function_id;
        let err = s.client.put_secret(child, f, "K", "v").await.unwrap_err();
        assert!(matches!(err, MetadataError::FunctionNotFound), "{err:?}");
        assert_eq!(s.branch_rows(child).await, [0; 4]);
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn purge_on_database_delete() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (f, d) = s.full_function(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        let (other, other_main) = s.database().await;
        s.full_function(other, other_main, "hello").await;

        s.client.mark_deleted(db).await.unwrap();
        for branch in [main, child] {
            assert_eq!(s.branch_rows(branch).await, [0; 4], "{branch}");
            assert!(s.client.resolve(branch, "hello").await.unwrap().is_none());
        }
        let err = s
            .client
            .set_active_deployment(main, f, d)
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::FunctionNotFound), "{err:?}");
        assert_eq!(
            s.branch_rows(other_main).await,
            [1, 2, 1, 2],
            "another database keeps its rows"
        );
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn resolve_needs_a_live_branch_and_database() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        s.full_function(db, main, "hello").await;
        assert!(s.client.resolve(main, "hello").await.unwrap().is_some());
        assert!(s.client.resolve(main, "nope").await.unwrap().is_none());
        // A tombstone that skipped the purge (rows left by hand) still resolves to nothing.
        sqlx::query("UPDATE databases SET status = 'deleting' WHERE id = $1")
            .bind(db.as_uuid())
            .execute(s.pool())
            .await
            .unwrap();
        assert!(s.client.resolve(main, "hello").await.unwrap().is_none());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn copied_triggers_pending() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        s.full_function(db, main, "hello").await;
        let parent = s.client.list_triggers(main).await.unwrap();
        let parent_storage = parent
            .iter()
            .find(|t| t.source == TriggerSource::Storage)
            .unwrap();
        assert_eq!(
            parent_storage.state,
            TriggerState::Installed,
            "storage triggers start installed"
        );
        let before = Utc::now();
        let child = s.branch(db, main, "dev").await;
        let triggers = s.client.list_triggers(child).await.unwrap();
        let table = triggers
            .iter()
            .find(|t| t.source == TriggerSource::Table)
            .unwrap();
        assert_eq!(
            table.state,
            TriggerState::PendingInstall,
            "the parent's was installed"
        );
        let storage = triggers
            .iter()
            .find(|t| t.source == TriggerSource::Storage)
            .unwrap();
        assert_eq!(storage.state, TriggerState::Installed);
        assert!(
            storage.enabled_since >= before - chrono::Duration::seconds(5)
                && storage.enabled_since > parent_storage.enabled_since,
            "the child never replays the parent's events"
        );
        assert_eq!(
            (
                storage.bucket.as_deref(),
                storage.prefix.as_deref(),
                storage.events.clone()
            ),
            (
                Some("avatars"),
                Some("in/"),
                vec!["object.created".to_string()]
            )
        );
        s.finish().await;
    }

    /// Waits until `n` sessions of the scratch database wait on a lock.
    async fn lock_waiters(s: &Scratch, n: i64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(s.pool())
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

    /// A function delete in flight while a branch is created from the same parent: the create
    /// waits for it (both take the database row) and copies what's left, never failing on the
    /// deleted function's foreign key.
    #[tokio::test]
    #[ignore]
    async fn create_branch_races_delete_function() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (f, _) = s.full_function(db, main, "hello").await;
        // Holds the delete in its cascade, after it has deleted (and locked) the function row.
        let mut hold = s.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM branch_functions WHERE function_id = $1 FOR UPDATE")
            .bind(f)
            .execute(&mut *hold)
            .await
            .unwrap();
        let client = s.client.clone();
        let delete = tokio::spawn(async move { client.delete_function(db, f).await });
        lock_waiters(&s, 1).await;
        let client = s.client.clone();
        let child = BranchId::new();
        let create = tokio::spawn(async move {
            client
                .create_branch(&NewBranch {
                    id: child,
                    database_id: db,
                    name: "dev",
                    parent_branch_id: main,
                    branched_at_time: None,
                    branched_at_lsn: None,
                })
                .await
        });
        lock_waiters(&s, 2).await;
        hold.commit().await.unwrap();
        assert!(delete.await.unwrap().unwrap());
        assert_eq!(create.await.unwrap().unwrap(), CreateBranchOutcome::Created);
        assert_eq!(
            s.branch_rows(child).await,
            [0; 4],
            "the deleted function isn't copied"
        );
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn trigger_columns_checked() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let c = &s.client;
        let mut storage_with_schema = storage_trigger(main, f, &["object.created"]);
        storage_with_schema.schema_name = Some("public");
        let err = c.add_trigger(&storage_with_schema).await.unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let err = c
            .add_trigger(&table_trigger(main, f, &["object.created"]))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let err = c
            .add_trigger(&table_trigger(main, f, &[]))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        // A repeated event would install `AFTER INSERT OR INSERT` or deliver an event twice.
        let err = c
            .add_trigger(&table_trigger(main, f, &["insert", "insert"]))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let err = c
            .add_trigger(&storage_trigger(
                main,
                f,
                &["object.created", "object.created"],
            ))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let err = c
            .add_trigger(&storage_trigger(main, f, &["insert"]))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        let mut table_without_name = table_trigger(main, f, &["insert"]);
        table_without_name.table_name = None;
        assert!(c.add_trigger(&table_without_name).await.is_err());
        assert!(c.list_triggers(main).await.unwrap().is_empty());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn managed_key_revoked_only_through_function() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let key = s.data_key(main).await;
        sqlx::query("UPDATE data_api_keys SET managed_by_function_id = $2 WHERE id = $1")
            .bind(key)
            .bind(f)
            .execute(s.pool())
            .await
            .unwrap();
        let err = s.client.revoke_data_key(main, key).await.unwrap_err();
        assert!(
            matches!(err, MetadataError::ManagedKey),
            "a managed key isn't revoked directly: {err:?}"
        );
        // Deleting the function revokes its keys.
        assert!(s.client.delete_function(db, f).await.unwrap());
        let revoked: bool =
            sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM data_api_keys WHERE id = $1")
                .bind(key)
                .fetch_one(s.pool())
                .await
                .unwrap();
        assert!(revoked);
        s.finish().await;
    }

    /// The migration over a metadata database that already has rows (as the workbench's and a
    /// deployed one do): applied by the strict migrator, existing keys unmanaged.
    #[tokio::test]
    #[ignore]
    async fn migration_applies_over_existing_rows() {
        let s = Scratch::empty().await;
        let full = sqlx::migrate!("./migrations");
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = Cow::Owned(
            full.migrations
                .iter()
                .filter(|m| m.version < FUNCTIONS_MIGRATION)
                .cloned()
                .collect(),
        );
        assert!(
            full.migrations
                .iter()
                .any(|m| m.version == FUNCTIONS_MIGRATION),
            "the functions migration exists"
        );
        before.run(s.pool()).await.unwrap();
        let (_, main) = s.database().await;
        let key = s.data_key(main).await;
        s.client.migrate().await.unwrap();
        s.client.migrate().await.unwrap();
        let managed: Option<Uuid> =
            sqlx::query_scalar("SELECT managed_by_function_id FROM data_api_keys WHERE id = $1")
                .bind(key)
                .fetch_one(s.pool())
                .await
                .unwrap();
        assert_eq!(managed, None);
        s.finish().await;
    }

    /// m3a: the auth mode is per branch; a branch without the function's state changes nothing.
    #[tokio::test]
    #[ignore]
    async fn auth_set_per_branch() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        let bare = s.branch(db, main, "bare").await;
        let other = s.function(db, "other").await;
        let c = &s.client;
        assert!(c.set_function_auth(child, f, FunctionAuth::Public).await.unwrap());
        assert_eq!(c.list_for_branch(child).await.unwrap()[0].auth, FunctionAuth::Public);
        assert_eq!(c.list_for_branch(main).await.unwrap()[0].auth, FunctionAuth::Key);
        assert!(!c.set_function_auth(main, other, FunctionAuth::AppUser).await.unwrap());
        let _ = bare;
        // Another database's branch: not found, nothing written.
        let (_, other_main) = s.database().await;
        let err = c.set_function_auth(other_main, f, FunctionAuth::Public).await.unwrap_err();
        assert!(matches!(err, MetadataError::FunctionNotFound), "{err:?}");
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn run_as_creates_sealed_managed_key() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());

        let keys = s.managed_keys(f).await;
        assert_eq!(keys.len(), 1);
        let (id, role, oid, sealed) = &keys[0];
        assert_eq!((*id, role.as_str(), *oid), (ra.key_id, "reporter", Some(4242)));
        assert!(sealed.as_deref().is_some_and(|v| !v.contains(&ra.secret)), "sealed, not plain");
        assert_eq!(s.run_as_columns(main, f).await, (Some("reporter".into()), Some(ra.key_id)));
        assert_eq!(
            c.managed_run_as_key(main, f).await.unwrap(),
            Some((ra.key_id, ra.secret.clone()))
        );
        // The key works as any secret key, and the listing names its function.
        let looked_up = c.lookup_data_key(&ra.sha256).await.unwrap().expect("live key");
        assert_eq!(looked_up.role, "reporter");
        let listed = c.list_data_keys(main).await.unwrap();
        let managed = listed.iter().find(|k| k.id == ra.key_id).unwrap();
        assert_eq!(managed.kind, DataKeyKind::Secret);
        assert_eq!(managed.managed_by_function, Some((f, "hello".to_string())));
        assert!(listed.iter().filter(|k| k.id != ra.key_id).all(|k| k.managed_by_function.is_none()));
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn run_as_replace_revokes_old() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;
        let first = run_as("reporter", 4242);
        let second = run_as("writer", 5151);
        assert!(c.set_run_as(main, f, Some(&first)).await.unwrap());
        assert!(c.set_run_as(main, f, Some(&second)).await.unwrap());
        assert_eq!(s.key_state(first.key_id).await, (true, true), "revoked, sealed value gone");
        let keys = s.managed_keys(f).await;
        assert_eq!(keys.iter().map(|k| k.0).collect::<Vec<_>>(), vec![second.key_id]);
        assert_eq!(s.run_as_columns(main, f).await, (Some("writer".into()), Some(second.key_id)));
        assert_eq!(
            c.managed_run_as_key(main, f).await.unwrap(),
            Some((second.key_id, second.secret.clone()))
        );
        assert!(c.lookup_data_key(&first.sha256).await.unwrap().is_none());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn run_as_clear() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        assert!(c.set_run_as(main, f, None).await.unwrap());
        assert_eq!(s.key_state(ra.key_id).await, (true, true));
        assert!(s.managed_keys(f).await.is_empty());
        assert_eq!(s.run_as_columns(main, f).await, (None, None));
        assert_eq!(c.managed_run_as_key(main, f).await.unwrap(), None);
        // A function without state on the branch: false, and no key is minted.
        let other = s.function(db, "other").await;
        assert!(!c.set_run_as(main, other, Some(&run_as("reporter", 4242))).await.unwrap());
        assert!(s.managed_keys(other).await.is_empty());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn delete_and_kind_refused_on_protected_branch() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        let c = &s.client;
        c.protect_branch(main, "h", &[]).await.unwrap().expect("protected");

        let err = c.delete_function_unprotected(db, f).await.unwrap_err();
        assert!(matches!(err, MetadataError::FunctionActiveOnProtectedBranch), "{err:?}");
        let err = c.set_function_kind(db, f, FunctionKind::Service).await.unwrap_err();
        assert!(matches!(err, MetadataError::FunctionActiveOnProtectedBranch), "{err:?}");
        let listed = c.list_functions(db).await.unwrap();
        assert_eq!((listed.len(), listed[0].kind), (1, FunctionKind::Function), "nothing changed");
        assert_eq!(s.branch_rows(main).await[0], 1);
        assert_eq!(s.branch_rows(child).await[0], 1);

        // Unprotected: a function with deployments keeps its kind (its bundles are a function's).
        c.set_branch_protected(main, false).await.unwrap();
        let err = c.set_function_kind(db, f, FunctionKind::Service).await.unwrap_err();
        assert!(matches!(err, MetadataError::InvalidFunction(_)), "{err:?}");
        assert!(c.delete_function_unprotected(db, f).await.unwrap());
        assert!(c.list_functions(db).await.unwrap().is_empty());
        assert!(!c.delete_function_unprotected(db, f).await.unwrap(), "already gone");

        // No deployment yet: the kind changes; the same kind is a no-op.
        let g = s.function(db, "svc").await;
        assert!(c.set_function_kind(db, g, FunctionKind::Service).await.unwrap());
        assert!(c.set_function_kind(db, g, FunctionKind::Service).await.unwrap());
        assert_eq!(c.list_functions(db).await.unwrap()[0].kind, FunctionKind::Service);
        // Another database's function: false.
        let (other, _) = s.database().await;
        assert!(!c.set_function_kind(other, g, FunctionKind::Function).await.unwrap());
        assert!(!c.delete_function_unprotected(other, g).await.unwrap());
        s.finish().await;
    }

    /// m3a: protecting main while a delete of a function active there is in flight. Either
    /// the protect waits for the delete (which saw main unprotected), or the delete waits for
    /// the protect and is refused: never a deleted function with main protected unseen.
    #[tokio::test]
    #[ignore]
    async fn protect_races_delete_function() {
        // 1. The delete first: it holds main's row, the protect waits for it.
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let mut hold = s.pool().begin().await.unwrap();
        sqlx::query("SELECT 1 FROM functions WHERE id = $1 FOR UPDATE")
            .bind(f)
            .execute(&mut *hold)
            .await
            .unwrap();
        let client = s.client.clone();
        let delete = tokio::spawn(async move { client.delete_function_unprotected(db, f).await });
        lock_waiters(&s, 1).await;
        let client = s.client.clone();
        let protect = tokio::spawn(async move { client.protect_branch(main, "h", &[]).await });
        lock_waiters(&s, 2).await;
        hold.commit().await.unwrap();
        assert!(delete.await.unwrap().unwrap());
        assert!(protect.await.unwrap().unwrap().expect("main").protected);
        assert!(s.client.list_functions(db).await.unwrap().is_empty());
        s.finish().await;

        // 2. The protect first (uncommitted): the delete waits for it, then is refused.
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let mut protecting = s.pool().begin().await.unwrap();
        sqlx::query("UPDATE branches SET protected = true WHERE id = $1")
            .bind(main.as_uuid())
            .execute(&mut *protecting)
            .await
            .unwrap();
        let client = s.client.clone();
        let delete = tokio::spawn(async move { client.delete_function_unprotected(db, f).await });
        lock_waiters(&s, 1).await;
        protecting.commit().await.unwrap();
        let err = delete.await.unwrap().unwrap_err();
        assert!(matches!(err, MetadataError::FunctionActiveOnProtectedBranch), "{err:?}");
        assert_eq!(s.client.list_functions(db).await.unwrap().len(), 1);
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn managed_key_revoke_rotate_refused() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        let before = s.managed_keys(f).await;

        let err = c.revoke_data_key(main, ra.key_id).await.unwrap_err();
        assert!(matches!(err, MetadataError::ManagedKey), "{err:?}");
        let generated = data_key::generate(DataKeyKind::Secret);
        let replacement = NewDataKey {
            id: Uuid::new_v4(),
            branch_id: main,
            kind: DataKeyKind::Secret,
            role: "reporter".into(),
            name: "hello".into(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: None,
            databases: None,
            role_oid: Some(4242),
            role_timeline: Some("tl-1".into()),
            role_bypass_rls: false,
        };
        let err = c.rotate_secret_key(main, ra.key_id, &replacement).await.unwrap_err();
        assert!(matches!(err, MetadataError::ManagedKey), "{err:?}");
        let err = c.rotate_data_key(main, ra.key_id, &replacement).await.unwrap_err();
        assert!(matches!(err, MetadataError::ManagedKey), "{err:?}");
        assert_eq!(s.managed_keys(f).await, before, "the key is untouched");
        assert!(c.list_data_keys(main).await.unwrap().iter().all(|k| k.id != replacement.id));
        // An unknown key is still plain false.
        assert!(!c.revoke_data_key(main, Uuid::new_v4()).await.unwrap());
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn managed_key_aad_bound() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f1 = s.active(db, main, "one").await;
        let f2 = s.active(db, main, "two").await;
        let c = &s.client;
        let (k1, k2) = (run_as("reporter", 4242), run_as("reporter", 4242));
        assert!(c.set_run_as(main, f1, Some(&k1)).await.unwrap());
        assert!(c.set_run_as(main, f2, Some(&k2)).await.unwrap());

        // Another function's key named as this one's run-as key: not this function's.
        sqlx::query("UPDATE branch_functions SET run_as_key_id = $3 WHERE branch_id = $1 AND function_id = $2")
            .bind(main.as_uuid())
            .bind(f1)
            .bind(k2.key_id)
            .execute(s.pool())
            .await
            .unwrap();
        assert_eq!(c.managed_run_as_key(main, f1).await.unwrap(), None);
        sqlx::query("UPDATE branch_functions SET run_as_key_id = $3 WHERE branch_id = $1 AND function_id = $2")
            .bind(main.as_uuid())
            .bind(f1)
            .bind(k1.key_id)
            .execute(s.pool())
            .await
            .unwrap();

        // k2's sealed value copied onto k1's row: sealed for k2's id, it doesn't open as k1.
        sqlx::query(
            "UPDATE data_api_keys SET sealed = (SELECT sealed FROM data_api_keys WHERE id = $2)
             WHERE id = $1",
        )
        .bind(k1.key_id)
        .bind(k2.key_id)
        .execute(s.pool())
        .await
        .unwrap();
        let err = c.managed_run_as_key(main, f1).await.unwrap_err();
        assert!(matches!(err, MetadataError::Secret(_)), "{err:?}");
        assert_eq!(c.managed_run_as_key(main, f2).await.unwrap(), Some((k2.key_id, k2.secret.clone())));
        s.finish().await;
    }

    #[tokio::test]
    #[ignore]
    async fn managed_key_none_on_tombstone() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let child = s.branch(db, main, "dev").await;
        let c = &s.client;
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(child, f, Some(&ra)).await.unwrap());
        assert!(c.managed_run_as_key(child, f).await.unwrap().is_some());
        // A tombstone that hasn't purged yet (rows left by hand) already hides the key.
        sqlx::query("UPDATE branches SET status = 'deleting' WHERE id = $1")
            .bind(child.as_uuid())
            .execute(s.pool())
            .await
            .unwrap();
        assert_eq!(c.managed_run_as_key(child, f).await.unwrap(), None);
        sqlx::query("UPDATE branches SET status = 'ready' WHERE id = $1")
            .bind(child.as_uuid())
            .execute(s.pool())
            .await
            .unwrap();
        c.mark_branch_deleted(child).await.unwrap();
        assert_eq!(c.managed_run_as_key(child, f).await.unwrap(), None);
        // A deleting database hides it too.
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        sqlx::query("UPDATE databases SET status = 'deleting' WHERE id = $1")
            .bind(db.as_uuid())
            .execute(s.pool())
            .await
            .unwrap();
        assert_eq!(c.managed_run_as_key(main, f).await.unwrap(), None);
        s.finish().await;
    }

    /// The managed-key migration keeps the old rule for every other key: a public key is
    /// sealed, an unmanaged secret key never is.
    #[tokio::test]
    #[ignore]
    async fn only_managed_secret_keys_are_sealed() {
        let s = Scratch::new().await;
        let (_, main) = s.database().await;
        let key = s.data_key(main).await;
        let err = sqlx::query("UPDATE data_api_keys SET sealed = 'x' WHERE id = $1")
            .bind(key)
            .execute(s.pool())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("data_api_keys_sealed"), "{err}");
        s.finish().await;
    }

    /// m3a for m4: the functions service records a function's last error (at most 1,024
    /// characters) on a live branch; a tombstoned branch keeps what it had.
    #[tokio::test]
    #[ignore]
    async fn last_error_capped_on_live_branch() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;
        let last = || async {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT last_error FROM branch_functions WHERE branch_id = $1 AND function_id = $2",
            )
            .bind(main.as_uuid())
            .bind(f)
            .fetch_one(s.pool())
            .await
            .unwrap()
        };
        c.set_function_last_error(main, f, Some("boom")).await.unwrap();
        assert_eq!(last().await.as_deref(), Some("boom"));
        let long: String = std::iter::repeat_n('é', 2000).collect();
        c.set_function_last_error(main, f, Some(&long)).await.unwrap();
        assert_eq!(last().await.map(|e| e.chars().count()), Some(1024));
        c.set_function_last_error(main, f, None).await.unwrap();
        assert_eq!(last().await, None);
        sqlx::query("UPDATE branches SET status = 'deleting' WHERE id = $1")
            .bind(main.as_uuid())
            .execute(s.pool())
            .await
            .unwrap();
        c.set_function_last_error(main, f, Some("late")).await.unwrap();
        assert_eq!(last().await, None, "a deleting branch isn't written");
        s.finish().await;
    }

    /// Audit B1 (m3a fix): replacing a run-as key while the old role is dropped through the
    /// control plane (`delete_role_copy`: endpoint `FOR UPDATE`, then key rows). `set_run_as`
    /// takes the endpoint before it writes any key row, so the two never deadlock.
    #[tokio::test]
    #[ignore]
    async fn run_as_replace_waits_for_role_drop() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let ep = s.endpoint(main).await;
        let f = s.active(db, main, "hello").await;
        assert!(s.client.set_run_as(main, f, Some(&run_as("reporter", 4242))).await.unwrap());
        let mut drop_tx = s.pool().begin().await.unwrap();
        sqlx::query("SELECT id FROM neon_endpoints WHERE id = $1 FOR UPDATE")
            .bind(&ep)
            .execute(&mut *drop_tx)
            .await
            .unwrap();
        let client = s.client.clone();
        let replace =
            tokio::spawn(async move { client.set_run_as(main, f, Some(&run_as("writer", 5151))).await });
        lock_waiters(&s, 1).await;
        let dropped = sqlx::query(
            "UPDATE data_api_keys k SET revoked_at = now(),
                 sealed = CASE WHEN k.managed_by_function_id IS NULL THEN k.sealed END
             FROM branches b
             WHERE b.endpoint_id = $1 AND k.branch_id = b.id AND k.role = $2
               AND k.kind = 'secret' AND k.revoked_at IS NULL",
        )
        .bind(&ep)
        .bind("reporter")
        .execute(&mut *drop_tx)
        .await;
        let drop_ok = dropped.is_ok() && drop_tx.commit().await.is_ok();
        let replaced = replace.await.unwrap();
        assert!(drop_ok && replaced.is_ok(), "a deadlock aborted one side: drop {drop_ok}, replace {replaced:?}");
        assert_eq!(s.managed_keys(f).await.len(), 1);
        s.finish().await;
    }

    /// Audit S1 (m3a fix): every path that revokes a managed key drops its sealed value (the
    /// resync, a role dropped through the control plane, a dropped database emptying the key's
    /// list), and the schema refuses a revoked secret key that keeps one.
    #[tokio::test]
    #[ignore]
    async fn every_revoke_drops_a_managed_secret() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let ep = s.endpoint(main).await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;

        // The resync.
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        let fix = KeyRoleFix::Revoke { id: ra.key_id, role: "reporter".into(), oid: Some(4242) };
        assert_eq!(c.fix_key_roles(main, &[fix]).await.unwrap(), 1);
        assert_eq!(s.key_state(ra.key_id).await, (true, true), "resync");

        // A role dropped through the control plane.
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        assert!(c.delete_role_copy(&ep, "reporter").await.unwrap());
        assert_eq!(s.key_state(ra.key_id).await, (true, true), "role drop");

        // A dropped database that was the key's only one.
        let ra = run_as("other", 77);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        let mut ddl = c.begin_ddl(&ep).await.unwrap().expect("endpoint");
        ddl.upsert_database("app", "other").await.unwrap();
        ddl.commit().await.unwrap();
        sqlx::query("UPDATE data_api_keys SET databases = ARRAY['app'] WHERE id = $1")
            .bind(ra.key_id)
            .execute(s.pool())
            .await
            .unwrap();
        let mut ddl = c.begin_ddl(&ep).await.unwrap().expect("endpoint");
        assert!(ddl.remove_database("app").await.unwrap());
        ddl.commit().await.unwrap();
        assert_eq!(s.key_state(ra.key_id).await, (true, true), "database drop");

        // The schema: a revoked secret key never keeps a sealed value.
        let ra = run_as("other", 77);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        let err = sqlx::query("UPDATE data_api_keys SET revoked_at = now() WHERE id = $1")
            .bind(ra.key_id)
            .execute(s.pool())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("data_api_keys_revoked_unsealed"), "{err}");
        s.finish().await;
    }

    /// m3a fix: a managed key's database list is its function's to shape, as its revocation.
    #[tokio::test]
    #[ignore]
    async fn managed_key_databases_refused() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.active(db, main, "hello").await;
        let c = &s.client;
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
        let err = c
            .set_key_databases(main, ra.key_id, Some(vec!["app".into()]))
            .await
            .unwrap_err();
        assert!(matches!(err, MetadataError::ManagedKey), "{err:?}");
        let databases: Option<Vec<String>> =
            sqlx::query_scalar("SELECT databases FROM data_api_keys WHERE id = $1")
                .bind(ra.key_id)
                .fetch_one(s.pool())
                .await
                .unwrap();
        assert_eq!(databases, None, "untouched");
        assert!(!c.set_key_databases(main, Uuid::new_v4(), None).await.unwrap());
        s.finish().await;
    }

    /// The rows the lock-order harness contends on.
    #[derive(Clone)]
    struct Contended {
        db: DatabaseId,
        main: BranchId,
        ep: String,
        f: Uuid,
        deployment: Uuid,
        managed_key: Uuid,
        other_key: Uuid,
        schedule: Uuid,
        /// A second function whose run-as key is still to mint (role `minter`, OID 5252).
        g: Uuid,
    }

    impl Scratch {
        /// A database whose main branch has an endpoint, a function active with a run-as key
        /// and a secret, and an unmanaged secret key for `other`.
        async fn contended(&self) -> Contended {
            let (db, main) = self.database().await;
            let ep = self.endpoint(main).await;
            let f = self.function(db, "hello").await;
            let deployment = self.deployment(f, &sha('c')).await;
            let c = &self.client;
            c.set_active_deployment(main, f, deployment).await.unwrap();
            assert!(c.set_enabled(main, f, true).await.unwrap());
            let ra = run_as("reporter", 4242);
            assert!(c.set_run_as(main, f, Some(&ra)).await.unwrap());
            c.put_secret(main, f, "API_KEY", "v").await.unwrap();
            let other_key = Uuid::new_v4();
            c.create_secret_key(&secret_key(other_key, main, "other", 77)).await.unwrap();
            let schedule = c
                .add_schedule(&NewSchedule {
                    branch_id: main,
                    function_id: f,
                    cron: "* * * * *",
                    timezone: "UTC",
                    next_run_at: Some(Utc::now() - chrono::Duration::minutes(1)),
                })
                .await
                .unwrap();
            c.set_service_health(deployment, true).await.unwrap();
            c.add_trigger(&table_trigger(main, f, &["insert"])).await.unwrap();
            let g = self.function(db, "minted").await;
            let gd = self.deployment(g, &sha('d')).await;
            c.set_active_deployment(main, g, gd).await.unwrap();
            let minter = run_as("minter", 5252);
            assert!(c.set_run_as(main, g, Some(&minter)).await.unwrap());
            sqlx::query("UPDATE data_api_keys SET revoked_at = now(), sealed = NULL WHERE id = $1")
                .bind(minter.key_id)
                .execute(self.pool())
                .await
                .unwrap();
            sqlx::query("UPDATE branch_functions SET run_as_key_id = NULL WHERE function_id = $1")
                .bind(g)
                .execute(self.pool())
                .await
                .unwrap();
            Contended { db, main, ep, f, deployment, managed_key: ra.key_id, other_key, schedule, g }
        }
    }

    fn secret_key(id: Uuid, branch: BranchId, role: &str, oid: i64) -> NewDataKey {
        let generated = data_key::generate(DataKeyKind::Secret);
        NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Secret,
            role: role.into(),
            name: format!("{role}-key"),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: None,
            databases: None,
            role_oid: Some(oid),
            role_timeline: Some("tl-1".into()),
            role_bypass_rls: false,
        }
    }

    type Fut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), MetadataError>> + Send>>;
    type Op = fn(MetadataClient, Contended) -> Fut;

    /// The function writes (m2, m3a, m7's schedule writes and claim).
    fn function_writes() -> Vec<(&'static str, Op)> {
        vec![
            ("set_run_as", |c, x| Box::pin(async move { c.set_run_as(x.main, x.f, Some(&run_as("writer", 5151))).await.map(drop) })),
            ("clear_run_as", |c, x| Box::pin(async move { c.set_run_as(x.main, x.f, None).await.map(drop) })),
            ("set_function_auth", |c, x| Box::pin(async move { c.set_function_auth(x.main, x.f, FunctionAuth::Public).await.map(drop) })),
            ("set_function_kind", |c, x| Box::pin(async move { c.set_function_kind(x.db, x.f, FunctionKind::Service).await.map(drop) })),
            ("delete_function_unprotected", |c, x| Box::pin(async move { c.delete_function_unprotected(x.db, x.f).await.map(drop) })),
            ("put_secret", |c, x| Box::pin(async move { c.put_secret(x.main, x.f, "TOKEN", "t").await })),
            ("set_active_deployment", |c, x| Box::pin(async move { c.set_active_deployment(x.main, x.f, x.deployment).await })),
            ("set_function_last_error", |c, x| Box::pin(async move { c.set_function_last_error(x.main, x.f, Some("boom")).await })),
            ("update_schedule", |c, x| Box::pin(async move { c.update_schedule(x.main, x.f, x.schedule, Some("*/5 * * * *"), None, None).await.map(drop) })),
            ("delete_schedule", |c, x| Box::pin(async move { c.delete_schedule(x.main, x.f, x.schedule).await.map(drop) })),
            ("claim_due_schedules", |c, _| Box::pin(async move { c.claim_due_schedules(50).await.map(drop) })),
            ("set_service_signing_secret", |c, x| Box::pin(async move { c.set_service_signing_secret(x.db, x.f, "whsec_x").await.map(drop) })),
            ("set_service_health", |c, x| Box::pin(async move { c.set_service_health(x.deployment, false).await })),
            ("settle_run_as_mint", |c, x| Box::pin(async move {
                c.settle_run_as_mint(x.main, x.g, "minter", Some(5252), RunAsMint::Key(&run_as("minter", 5252))).await.map(drop)
            })),
            // m13: trigger enable/disable and delete (4b, then the drop 4c), the dispatcher's
            // record (4b alone) and the worker's clear (4c alone).
            ("update_trigger", |c, x| Box::pin(async move {
                let Some(t) = c.list_triggers(x.main).await?.into_iter().next() else { return Ok(()) };
                c.update_trigger(x.main, x.f, t.id, false).await.map(drop)
            })),
            ("delete_trigger", |c, x| Box::pin(async move {
                let Some(t) = c.list_triggers(x.main).await?.into_iter().next() else { return Ok(()) };
                c.delete_trigger(x.main, x.f, t.id).await.map(drop)
            })),
            ("record_trigger_deliveries", |c, x| Box::pin(async move {
                let deliveries: Vec<TriggerDelivery> = c
                    .list_triggers(x.main)
                    .await?
                    .into_iter()
                    .map(|t| TriggerDelivery { id: t.id, delivered: false, dead: 1, dropped: 0, error: Some("500".into()) })
                    .collect();
                c.record_trigger_deliveries(x.main, &deliveries).await.map(drop)
            })),
            ("clear_trigger_drops", |c, x| Box::pin(async move {
                let drops = c.trigger_drops(x.main).await?;
                c.clear_trigger_drops(x.main, &drops).await.map(drop)
            })),
            ("settle_trigger_installs", |c, x| Box::pin(async move {
                let installs: Vec<TriggerInstall> = c
                    .list_triggers(x.main)
                    .await?
                    .into_iter()
                    .map(|t| TriggerInstall { id: t.id, enabled_since: t.enabled_since, error: None })
                    .collect();
                c.settle_trigger_installs(x.main, &installs).await.map(drop)
            })),
        ]
    }

    /// The existing paths that take the same rows for update.
    fn row_writers() -> Vec<(&'static str, Op)> {
        vec![
            ("create_branch", |c, x| Box::pin(async move {
                c.create_branch(&NewBranch {
                    id: BranchId::new(),
                    database_id: x.db,
                    name: "dev",
                    parent_branch_id: x.main,
                    branched_at_time: None,
                    branched_at_lsn: None,
                })
                .await
                .map(drop)
            })),
            ("protect_branch", |c, x| Box::pin(async move { c.protect_branch(x.main, "h", &[]).await.map(drop) })),
            ("delete_role_copy", |c, x| Box::pin(async move { c.delete_role_copy(&x.ep, "reporter").await.map(drop) })),
            ("rename_role_copy", |c, x| Box::pin(async move {
                c.rename_role_copy(&x.ep, x.main, "reporter", "reporter2", None, None).await.map(drop)
            })),
            ("rotate_secret_key", |c, x| Box::pin(async move {
                c.rotate_secret_key(x.main, x.other_key, &secret_key(Uuid::new_v4(), x.main, "other", 77)).await.map(drop)
            })),
            ("mark_branch_deleted", |c, x| Box::pin(async move { c.mark_branch_deleted(x.main).await })),
            // m8: the switches pend the branch's table triggers. The branch is made resetting
            // with a pending timeline first, each a single statement on its row.
            ("switch_to_pending_timeline", |c, x| Box::pin(async move {
                if resetting(&c, x.main).await? {
                    c.switch_to_pending_timeline(x.main, "tl-pending", None, "0/1").await?;
                }
                Ok(())
            })),
            ("switch_to_restored_timeline", |c, x| Box::pin(async move {
                if resetting(&c, x.main).await? {
                    c.switch_to_restored_timeline(x.main, "tl-pending", BranchId::new(), "main-before", "0/1").await?;
                }
                Ok(())
            })),
        ]
    }

    /// Makes `branch` resetting with the pending timeline `tl-pending`; false when it no
    /// longer can be (deleted meanwhile).
    async fn resetting(c: &MetadataClient, branch: BranchId) -> Result<bool, MetadataError> {
        Ok(c.set_branch_status(branch, &[BranchStatus::Ready], BranchStatus::Resetting).await?
            && c.set_pending_timeline(branch, Some("tl-pending")).await?)
    }

    /// Refusals a write may answer when the other one changed its rows first.
    fn expected(result: &Result<(), MetadataError>) -> bool {
        matches!(
            result,
            Ok(())
                | Err(MetadataError::FunctionNotFound
                    | MetadataError::FunctionActiveOnProtectedBranch
                    | MetadataError::ManagedKey
                    | MetadataError::InvalidFunction(_))
        )
    }

    /// Waits until each task has finished or waits on a lock (the blocker never waits).
    async fn settle<T>(s: &Scratch, tasks: &[&tokio::task::JoinHandle<T>]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let n = tasks.iter().filter(|t| !t.is_finished()).count() as i64;
            if n == 0 {
                return;
            }
            let waiting: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(s.pool())
            .await
            .unwrap();
            if waiting >= n {
                return;
            }
            if std::time::Instant::now() >= deadline {
                let busy: Vec<(Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
                    "SELECT wait_event_type, wait_event, left(query, 120) FROM pg_stat_activity
                     WHERE datname = current_database() AND state <> 'idle' AND pid <> pg_backend_pid()",
                )
                .fetch_all(s.pool())
                .await
                .unwrap();
                panic!("{waiting} of {n} waiters; busy sessions: {busy:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// What must hold after any two writes: a branch state's run-as key is a key its function
    /// manages on that branch, and a live managed key on a live branch is its function's
    /// current run-as key. (A revoked managed key with a sealed value is the schema's CHECK.)
    async fn consistent(s: &Scratch) -> Result<(), String> {
        let dangling: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM branch_functions bf
             LEFT JOIN data_api_keys k ON k.id = bf.run_as_key_id AND k.branch_id = bf.branch_id
             WHERE bf.run_as_key_id IS NOT NULL
               AND k.managed_by_function_id IS DISTINCT FROM bf.function_id",
        )
        .fetch_one(s.pool())
        .await
        .unwrap();
        let orphans: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM data_api_keys k JOIN branches b ON b.id = k.branch_id
             WHERE k.managed_by_function_id IS NOT NULL AND k.revoked_at IS NULL
               AND b.status NOT IN ('deleting', 'deleted')
               AND NOT EXISTS (SELECT 1 FROM branch_functions bf WHERE bf.run_as_key_id = k.id)",
        )
        .fetch_one(s.pool())
        .await
        .unwrap();
        if dangling == 0 && orphans == 0 {
            Ok(())
        } else {
            Err(format!("{dangling} dangling run-as keys, {orphans} orphaned managed keys"))
        }
    }

    /// The invariant behind audit B1 and m2's delete fix: every function write and every
    /// existing writer of the same rows take them in one order (functions.rs module docs), so
    /// no two deadlock. Exhaustive at row granularity: for each ordered pair (one function
    /// write, one existing writer, either first), the first is stalled on each contended row
    /// in turn (a blocker holds it `FOR UPDATE`, so the first holds everything it took
    /// before), the second runs into it, then the blocker lets go. No side may fail with
    /// anything but a refusal (never 40P01), and the rows stay consistent.
    #[tokio::test]
    #[ignore]
    async fn function_writes_keep_the_lock_order() {
        let s = Scratch::new().await;
        // The rows a write can be stalled on, as SQL taking them FOR UPDATE.
        let stalls: [(&str, &str); 9] = [
            ("database", "SELECT 1 FROM databases WHERE id = $1 FOR UPDATE"),
            ("branch", "SELECT 1 FROM branches WHERE id = $2 FOR UPDATE"),
            ("endpoint", "SELECT 1 FROM neon_endpoints WHERE id = $3 FOR UPDATE"),
            ("function", "SELECT 1 FROM functions WHERE database_id = $1 FOR UPDATE"),
            ("branch state", "SELECT 1 FROM branch_functions WHERE branch_id = $2 FOR UPDATE"),
            ("schedule", "SELECT 1 FROM function_schedules WHERE function_id = $4 FOR UPDATE"),
            ("trigger", "SELECT 1 FROM function_triggers WHERE function_id = $4 FOR UPDATE"),
            ("managed key", "SELECT 1 FROM data_api_keys WHERE id = $5 FOR UPDATE"),
            ("service health", "SELECT 1 FROM service_health h JOIN function_deployments d ON d.id = h.deployment_id WHERE d.function_id = $4 FOR UPDATE OF h"),
        ];
        let mut pairs = Vec::new();
        for w in function_writes() {
            for p in row_writers() {
                pairs.push((w, p));
                pairs.push((p, w));
            }
        }
        let mut failures = Vec::new();
        for ((first_name, first), (second_name, second)) in pairs {
            for (stall_name, stall_sql) in stalls {
                let x = s.contended().await;
                let mut blocker = s.pool().begin().await.unwrap();
                sqlx::query(AssertSqlSafe(stall_sql))
                    .bind(x.db.as_uuid())
                    .bind(x.main.as_uuid())
                    .bind(&x.ep)
                    .bind(x.f)
                    .bind(x.managed_key)
                    .execute(&mut *blocker)
                    .await
                    .unwrap();
                let a = tokio::spawn(first(s.client.clone(), x.clone()));
                settle(&s, &[&a]).await;
                let b = tokio::spawn(second(s.client.clone(), x.clone()));
                settle(&s, &[&a, &b]).await;
                blocker.commit().await.unwrap();
                let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
                let case = format!("{first_name} stalled on {stall_name}, then {second_name}");
                if !expected(&ra) || !expected(&rb) {
                    failures.push(format!("{case}: {ra:?} / {rb:?}"));
                } else if let Err(e) = consistent(&s).await {
                    failures.push(format!("{case}: {e}"));
                }
            }
        }
        assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
        s.finish().await;
    }

    /// m7: a schedule's update recomputes `next_run_at` when its cron or time zone changes or it
    /// is turned on, keeps it otherwise, refuses a bad cron, and finds only the function's own
    /// schedule on the branch; a delete deletes once.
    #[tokio::test]
    #[ignore]
    async fn schedule_update_and_delete() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let g = s.function(db, "other").await;
        let c = &s.client;
        let past = chrono::SubsecRound::trunc_subsecs(Utc::now() - chrono::Duration::days(1), 6);
        let id = c
            .add_schedule(&NewSchedule {
                branch_id: main,
                function_id: f,
                cron: "* * * * *",
                timezone: "UTC",
                next_run_at: Some(past),
            })
            .await
            .unwrap();
        let off = c.update_schedule(main, f, id, None, None, Some(false)).await.unwrap().unwrap();
        assert_eq!((off.enabled, off.next_run_at), (false, Some(past)), "only the flag changes");
        let on = c.update_schedule(main, f, id, None, None, Some(true)).await.unwrap().unwrap();
        assert!(on.enabled && on.next_run_at.unwrap() > Utc::now(), "on again: the next tick");
        let moved = c
            .update_schedule(main, f, id, Some("0 9 * * *"), Some("Europe/London"), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((moved.cron.as_str(), moved.timezone.as_str()), ("0 9 * * *", "Europe/London"));
        assert_eq!(c.list_schedules(main).await.unwrap(), vec![moved.clone()]);
        let err = c.update_schedule(main, f, id, Some("nope"), None, None).await.unwrap_err();
        assert!(matches!(&err, MetadataError::InvalidFunction(m) if m.starts_with("cron: ")), "{err:?}");
        assert_eq!(c.update_schedule(main, g, id, None, None, Some(false)).await.unwrap(), None);
        assert!(!c.delete_schedule(main, g, id).await.unwrap());
        assert!(c.delete_schedule(main, f, id).await.unwrap());
        assert!(!c.delete_schedule(main, f, id).await.unwrap());
        assert!(c.list_schedules(main).await.unwrap().is_empty());
        s.finish().await;
    }

    /// m3: a function's deployments for the management API, newest first; another
    /// function's never appear.
    #[tokio::test]
    #[ignore]
    async fn deployments_listed_newest_first() {
        let s = Scratch::new().await;
        let (db, _) = s.database().await;
        let f = s.function(db, "hello").await;
        let g = s.function(db, "other").await;
        let first = s.deployment(f, &sha('1')).await;
        let second = s.deployment(f, &sha('2')).await;
        s.deployment(g, &sha('3')).await;
        let listed = s.client.list_deployments(f).await.unwrap();
        assert_eq!(listed.iter().map(|d| d.id).collect::<Vec<_>>(), vec![second, first]);
        assert_eq!(listed[0].bundle_sha256.as_deref(), Some(sha('2').as_str()));
        assert!(s.client.list_deployments(Uuid::new_v4()).await.unwrap().is_empty());
        s.finish().await;
    }
    /// m8: the worker settles only the generation of a row it read (`enabled_since`): a row
    /// pended again meanwhile (a reset or restore) stays pending; an install becomes
    /// `installed` (error cleared), a failure `failed` with its error; a settled row is never
    /// settled again, and other branches' rows are untouched.
    #[tokio::test]
    #[ignore]
    async fn trigger_installs_settle_only_the_read_generation() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let c = &s.client;
        let a = c.add_trigger(&table_trigger(main, f, &["insert"])).await.unwrap();
        let b = c.add_trigger(&table_trigger(main, f, &["delete"])).await.unwrap();
        let read = c.list_triggers(main).await.unwrap();
        let since = |id: Uuid| read.iter().find(|t| t.id == id).unwrap().enabled_since;
        assert_eq!(c.trigger_sweep_branches(10).await.unwrap(), vec![(main, true)]);
        let stale = since(a) - chrono::Duration::seconds(1);
        let settled = c
            .settle_trigger_installs(main, &[TriggerInstall { id: a, enabled_since: stale, error: None }])
            .await
            .unwrap();
        assert_eq!(settled, 0, "another generation");
        let settled = c
            .settle_trigger_installs(
                main,
                &[
                    TriggerInstall { id: a, enabled_since: since(a), error: None },
                    TriggerInstall { id: b, enabled_since: since(b), error: Some("table_missing".into()) },
                ],
            )
            .await
            .unwrap();
        assert_eq!(settled, 2);
        let after = c.list_triggers(main).await.unwrap();
        let row = |id: Uuid| after.iter().find(|t| t.id == id).unwrap().clone();
        assert_eq!((row(a).state, row(a).last_error), (TriggerState::Installed, None));
        assert_eq!((row(b).state, row(b).last_error.as_deref()), (TriggerState::Failed, Some("table_missing")));
        // Round 2: an installed row stays in the sweep (its drift is checked), not pending.
        assert_eq!(c.trigger_sweep_branches(10).await.unwrap(), vec![(main, false)]);
        let again = c
            .settle_trigger_installs(
                main,
                &[
                    TriggerInstall { id: b, enabled_since: since(b), error: None },
                    TriggerInstall { id: a, enabled_since: since(a), error: None },
                ],
            )
            .await
            .unwrap();
        assert_eq!(again, 0, "a failed row is never settled again; an installed one stays installed");
        // Round 2 (S4): an installed row whose database trigger can't be put back fails.
        let failed = c
            .settle_trigger_installs(main, &[TriggerInstall { id: a, enabled_since: since(a), error: Some("table_missing".into()) }])
            .await
            .unwrap();
        assert_eq!(failed, 1);
        let row_a = c.list_triggers(main).await.unwrap().into_iter().find(|t| t.id == a).unwrap();
        assert_eq!((row_a.state, row_a.last_error.as_deref()), (TriggerState::Failed, Some("table_missing")));
        assert!(c.trigger_sweep_branches(10).await.unwrap().is_empty(), "failed rows leave the sweep");
        let (_, other) = s.database().await;
        assert_eq!(
            c.settle_trigger_installs(other, &[TriggerInstall { id: a, enabled_since: since(a), error: None }])
                .await
                .unwrap(),
            0,
            "another branch's row"
        );
        s.finish().await;
    }

    /// m8: a reset's and a restore's switch pend every table trigger of the branch (installed
    /// or failed) in the switch's transaction, with a new `enabled_since` and no error, so the
    /// worker reinstalls them on the new timeline; storage triggers are left alone.
    #[tokio::test]
    #[ignore]
    async fn switch_pends_table_triggers() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        s.full_function(db, main, "hello").await;
        let c = &s.client;
        for restore in [false, true] {
            let before = c.list_triggers(main).await.unwrap();
            assert!(before.iter().all(|t| t.state == TriggerState::Installed), "{before:?}");
            assert!(c.set_branch_status(main, &[BranchStatus::Ready], BranchStatus::Resetting).await.unwrap());
            let pending = format!("tl-{restore}");
            assert!(c.set_pending_timeline(main, Some(&pending)).await.unwrap());
            let switched = if restore {
                c.switch_to_restored_timeline(main, &pending, BranchId::new(), "main-before", "0/1").await
            } else {
                c.switch_to_pending_timeline(main, &pending, None, "0/1").await
            };
            assert!(switched.unwrap());
            let after = c.list_triggers(main).await.unwrap();
            for t in &after {
                let old = before.iter().find(|o| o.id == t.id).unwrap();
                match t.source {
                    TriggerSource::Table => {
                        assert_eq!((t.state, t.last_error.as_deref()), (TriggerState::PendingInstall, None));
                        assert!(t.enabled_since > old.enabled_since, "a new generation");
                    }
                    TriggerSource::Storage => assert_eq!(t, old),
                }
            }
            sqlx::query("UPDATE function_triggers SET state = 'installed' WHERE branch_id = $1")
                .bind(main.as_uuid())
                .execute(s.pool())
                .await
                .unwrap();
            s.ready(main).await;
        }
        s.finish().await;
    }

    /// m9: a service's signing secret is sealed for its function (another function's id
    /// doesn't open it); the health checks poll each enabled external deployment on a live
    /// branch once, and what they record shows on the branch's listing.
    #[tokio::test]
    #[ignore]
    async fn external_services_secrets_targets_and_health() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (other_db, _) = s.database().await;
        let c = &s.client;
        let svc = c.create_function(db, "stub", FunctionKind::Service).await.unwrap().id;
        assert_eq!(c.service_signing_secret(svc).await.unwrap(), None);
        assert!(c.set_service_signing_secret(db, svc, "whsec_one").await.unwrap());
        assert!(!c.set_service_signing_secret(other_db, svc, "whsec_two").await.unwrap(), "another database's");
        assert_eq!(c.service_signing_secret(svc).await.unwrap().as_deref(), Some("whsec_one"));
        let sealed: String = sqlx::query_scalar("SELECT signing_secret FROM functions WHERE id = $1")
            .bind(svc)
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert!(!sealed.contains("whsec_one"));
        sqlx::query("UPDATE functions SET signing_secret = $2 WHERE id = $1")
            .bind(s.function(db, "other").await)
            .bind(&sealed)
            .execute(s.pool())
            .await
            .unwrap();
        let other: Uuid = sqlx::query_scalar("SELECT id FROM functions WHERE name = 'other' AND database_id = $1")
            .bind(db.as_uuid())
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert!(matches!(c.service_signing_secret(other).await, Err(MetadataError::Secret(_))));

        let spec = serde_json::json!({"backend": "external", "url": "https://svc.example/api", "health_path": "/healthz"});
        let deployment = c
            .create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: svc,
                bundle_sha256: None,
                bundle_size: None,
                spec: &spec,
                compatibility_date: date(),
                created_by: "member-1",
                note: None,
            })
            .await
            .unwrap()
            .id;
        c.set_active_deployment(main, svc, deployment).await.unwrap();
        assert!(c.list_service_targets().await.unwrap().iter().all(|t| t.deployment_id != deployment), "disabled");
        assert!(c.set_enabled(main, svc, true).await.unwrap());
        let child = s.branch(db, main, "dev").await;
        let targets: Vec<_> = c.list_service_targets().await.unwrap().into_iter().filter(|t| t.deployment_id == deployment).collect();
        assert_eq!(targets.len(), 1, "each deployment once, though two branches serve it");
        assert_eq!(targets[0].spec, spec);
        let health = |b| async move {
            c.list_for_branch(b).await.unwrap().into_iter().find(|f| f.function_id == svc).unwrap().health
        };
        assert_eq!(health(main).await, None);
        c.set_service_health(deployment, false).await.unwrap();
        assert_eq!(health(main).await.as_deref(), Some("unhealthy"));
        c.set_service_health(deployment, true).await.unwrap();
        assert_eq!(health(child).await.as_deref(), Some("healthy"));
        c.set_service_health(Uuid::new_v4(), true).await.unwrap();
        c.mark_branch_deleted(child).await.unwrap();
        assert!(c.set_enabled(main, svc, false).await.unwrap());
        assert!(c.list_service_targets().await.unwrap().iter().all(|t| t.deployment_id != deployment));
        s.finish().await;
    }

    /// m11: container targets are the enabled container services, one row per live branch;
    /// external services, disabled ones and deleted branches are left out.
    #[tokio::test]
    #[ignore]
    async fn container_targets_are_per_enabled_branch() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let c = &s.client;
        let svc = c.create_function(db, "echo", FunctionKind::Service).await.unwrap().id;
        let ext = c.create_function(db, "ext", FunctionKind::Service).await.unwrap().id;
        let deploy = |f: Uuid, spec: serde_json::Value| async move {
            c.create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: f,
                bundle_sha256: None,
                bundle_size: None,
                spec: &spec,
                compatibility_date: date(),
                created_by: "member-1",
                note: None,
            })
            .await
            .unwrap()
            .id
        };
        let spec = serde_json::json!({"backend": "container", "image": format!("ghcr.io/acme/echo@sha256:{}", "a".repeat(64)), "port": 8080, "health_path": "/"});
        let d = deploy(svc, spec.clone()).await;
        let e = deploy(ext, serde_json::json!({"backend": "external", "url": "https://svc.example", "health_path": "/"})).await;
        c.set_active_deployment(main, svc, d).await.unwrap();
        c.set_active_deployment(main, ext, e).await.unwrap();
        let mine = |db_branches: Vec<BranchId>| async move {
            c.list_container_targets()
                .await
                .unwrap()
                .into_iter()
                .filter(|t| db_branches.contains(&t.branch_id))
                .collect::<Vec<_>>()
        };
        assert!(mine(vec![main]).await.is_empty(), "disabled");
        assert!(c.set_enabled(main, svc, true).await.unwrap());
        assert!(c.set_enabled(main, ext, true).await.unwrap());
        let child = s.branch(db, main, "dev").await;
        let targets = mine(vec![main, child]).await;
        assert_eq!(targets.len(), 2, "{targets:?}");
        assert!(targets.iter().all(|t| t.function_id == svc && t.deployment_id == d && t.spec == spec));
        c.mark_branch_deleted(child).await.unwrap();
        assert_eq!(mine(vec![main, child]).await.len(), 1);
        s.finish().await;
    }


    /// m8 round 2 (S3): a child's run-as key is retried from state alone (a role and no key on
    /// a live normal branch), whatever its `last_error` says (an invocation error, say).
    #[tokio::test]
    #[ignore]
    async fn run_as_sweep_selects_by_state() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let (f, _) = s.full_function(db, main, "hello").await;
        let c = &s.client;
        assert!(c.pending_run_as_branches(10).await.unwrap().is_empty(), "main has its key");
        let child = s.branch(db, main, "dev").await;
        assert_eq!(c.pending_run_as_branches(10).await.unwrap(), vec![child]);
        c.set_function_last_error(child, f, Some("TypeError: boom")).await.unwrap();
        assert_eq!(c.pending_run_as_branches(10).await.unwrap(), vec![child], "an invocation error doesn't stop it");
        // Round 3 (B2): only the state decides, and a terminal outcome is state too.
        let st = c.list_for_branch(child).await.unwrap().into_iter().find(|b| b.function_id == f).unwrap();
        let settled = c
            .settle_run_as_mint(child, f, "reporter", st.run_as_role_oid, RunAsMint::Unmintable(RunAsUnmintable::Missing, "run_as_role_missing"))
            .await
            .unwrap();
        assert!(settled);
        assert!(c.pending_run_as_branches(10).await.unwrap().is_empty(), "a terminal outcome leaves the sweep");
        s.finish().await;
    }

    /// m8 round 3 (B1, B2): the invariant "a minted run-as key is always bound to the role OID a
    /// member approved, and the worker never overrides a member's run-as choice". `set_run_as`
    /// records the approved OID and a child copies it; the guarded mint writes only while the
    /// state row still names that role and OID with no key and no terminal outcome, and binds
    /// the key to that OID; anything else writes nothing.
    #[tokio::test]
    #[ignore]
    async fn run_as_mint_is_guarded_and_bound() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let d = s.deployment(f, &sha('a')).await;
        let c = &s.client;
        c.set_active_deployment(main, f, d).await.unwrap();
        assert!(c.set_run_as(main, f, Some(&run_as("reporter", 4242))).await.unwrap());
        let st = |b: BranchId| async move {
            c.list_for_branch(b).await.unwrap().into_iter().find(|x| x.function_id == f).unwrap()
        };
        assert_eq!(st(main).await.run_as_role_oid, Some(4242), "set_run_as records the approved OID");
        let child = s.branch(db, main, "dev").await;
        let copy = st(child).await;
        assert_eq!((copy.run_as_role.as_deref(), copy.run_as_role_oid, copy.run_as_key_id, copy.run_as_unmintable), (Some("reporter"), Some(4242), None, None));
        // Another role, another OID, or a key bound elsewhere: refused before anything is written.
        for (role, expected, key) in [("other", Some(4242), run_as("other", 4242)), ("reporter", Some(77), run_as("reporter", 77)), ("reporter", Some(4242), run_as("reporter", 77))] {
            let wrote = c.settle_run_as_mint(child, f, role, expected, RunAsMint::Key(&key)).await;
            assert!(!matches!(wrote, Ok(true)), "{role} {expected:?}: {wrote:?}");
        }
        assert_eq!(st(child).await.run_as_key_id, None);
        // The member clears the run-as role first: the mint writes nothing.
        let ra = run_as("reporter", 4242);
        assert!(c.set_run_as(child, f, None).await.unwrap());
        assert!(!c.settle_run_as_mint(child, f, "reporter", Some(4242), RunAsMint::Key(&ra)).await.unwrap());
        assert_eq!((st(child).await.run_as_role, st(child).await.run_as_key_id), (None, None));
        assert!(s.managed_keys(f).await.iter().all(|k| k.0 != ra.key_id), "the discarded key was never stored");
        // A fresh child: the mint binds the approved OID.
        let child2 = s.branch(db, main, "dev2").await;
        assert!(c.settle_run_as_mint(child2, f, "reporter", Some(4242), RunAsMint::Key(&ra)).await.unwrap());
        assert_eq!(st(child2).await.run_as_key_id, Some(ra.key_id));
        let bound: Option<i64> = sqlx::query_scalar("SELECT role_oid FROM data_api_keys WHERE id = $1")
            .bind(ra.key_id)
            .fetch_one(s.pool())
            .await
            .unwrap();
        assert_eq!(bound, Some(4242));
        assert!(!c.settle_run_as_mint(child2, f, "reporter", Some(4242), RunAsMint::Key(&run_as("reporter", 4242))).await.unwrap(), "once");
        // A terminal outcome stays until a member sets run-as again.
        let child3 = s.branch(db, main, "dev3").await;
        assert!(c.settle_run_as_mint(child3, f, "reporter", Some(4242), RunAsMint::Unmintable(RunAsUnmintable::Changed, "run_as_role_changed: x")).await.unwrap());
        let t = st(child3).await;
        assert_eq!((t.run_as_unmintable, t.last_error.as_deref()), (Some(RunAsUnmintable::Changed), Some("run_as_role_changed: x")));
        assert!(!c.settle_run_as_mint(child3, f, "reporter", Some(4242), RunAsMint::Key(&run_as("reporter", 4242))).await.unwrap());
        assert!(c.set_run_as(child3, f, Some(&run_as("reporter", 4242))).await.unwrap());
        assert_eq!(st(child3).await.run_as_unmintable, None, "a member's set_run_as clears it");
        s.finish().await;
    }

    /// m13: settles every table trigger read as installed (as m8's worker does).
    async fn install_all(c: &MetadataClient, branch: BranchId) {
        let installs: Vec<TriggerInstall> = c
            .list_triggers(branch)
            .await
            .unwrap()
            .into_iter()
            .map(|t| TriggerInstall { id: t.id, enabled_since: t.enabled_since, error: None })
            .collect();
        c.settle_trigger_installs(branch, &installs).await.unwrap();
    }

    /// m13 (#175, S2, S4): turning a table trigger off records a drop and keeps the row
    /// (disabled triggers are dropped from the database, so they queue nothing); turning it on
    /// pends it again; a delete records a drop ("drop, then forget"); a storage trigger never
    /// records one. Only the function's own trigger on the branch is found, and a cleared drop
    /// is only the one read.
    #[tokio::test]
    #[ignore]
    async fn trigger_update_and_delete_record_drops() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let g = s.function(db, "other").await;
        let c = &s.client;
        let t = c.add_trigger(&table_trigger(main, f, &["insert"])).await.unwrap();
        let st = c.add_trigger(&storage_trigger(main, f, &["object.created"])).await.unwrap();
        install_all(c, main).await;
        assert_eq!(c.trigger_sweep_branches(10).await.unwrap(), vec![(main, false)]);
        assert_eq!(c.update_trigger(main, g, t, false).await.unwrap(), None, "another function's");
        let off = c.update_trigger(main, f, t, false).await.unwrap().unwrap();
        assert_eq!((off.enabled, off.state), (false, TriggerState::Installed));
        let drops = c.trigger_drops(main).await.unwrap();
        assert_eq!(drops.len(), 1, "the disable records a drop");
        assert_eq!(c.trigger_sweep_branches(10).await.unwrap(), vec![(main, true)], "a drop is pending work");
        assert!(c.dispatch_targets(10).await.unwrap().is_empty(), "a disabled trigger is never dispatched");
        let on = c.update_trigger(main, f, t, true).await.unwrap().unwrap();
        assert_eq!((on.enabled, on.state, on.last_error), (true, TriggerState::PendingInstall, None));
        assert!(on.enabled_since > off.enabled_since, "a new generation");
        // A second drop while the first is still recorded; clearing the first keeps it.
        c.update_trigger(main, f, t, false).await.unwrap().unwrap();
        assert_eq!(c.clear_trigger_drops(main, &drops).await.unwrap(), 1);
        assert_eq!(c.trigger_drops(main).await.unwrap().len(), 1, "the newer drop stays");
        // A disabled pending row is no pending work of its own.
        let later = c.trigger_drops(main).await.unwrap();
        c.clear_trigger_drops(main, &later).await.unwrap();
        assert_eq!(c.trigger_sweep_branches(10).await.unwrap(), vec![(main, false)]);
        c.update_trigger(main, f, t, true).await.unwrap().unwrap();
        assert!(!c.delete_trigger(main, g, t).await.unwrap(), "another function's");
        assert!(c.delete_trigger(main, f, t).await.unwrap());
        assert!(!c.delete_trigger(main, f, t).await.unwrap(), "once");
        assert_eq!(c.trigger_drops(main).await.unwrap().len(), 1, "the delete records a drop");
        let none = c.trigger_drops(main).await.unwrap();
        c.clear_trigger_drops(main, &none).await.unwrap();
        assert!(c.update_trigger(main, f, st, false).await.unwrap().is_some());
        assert!(c.delete_trigger(main, f, st).await.unwrap());
        assert!(c.trigger_drops(main).await.unwrap().is_empty(), "a storage trigger has no database trigger");
        s.finish().await;
    }

    /// m13 (#175, S3): deleting a function records a drop for each of its table triggers on
    /// every branch before the cascade deletes the rows; a branch tombstone purges its drops.
    #[tokio::test]
    #[ignore]
    async fn function_delete_records_trigger_drops() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let c = &s.client;
        c.add_trigger(&table_trigger(main, f, &["insert"])).await.unwrap();
        c.add_trigger(&storage_trigger(main, f, &["object.created"])).await.unwrap();
        let child = s.branch(db, main, "dev").await;
        assert_eq!(c.list_triggers(child).await.unwrap().len(), 2);
        assert!(c.delete_function(db, f).await.unwrap());
        assert!(c.list_triggers(main).await.unwrap().is_empty());
        assert_eq!(c.trigger_drops(main).await.unwrap().len(), 1);
        assert_eq!(c.trigger_drops(child).await.unwrap().len(), 1);
        let mut sweep = c.trigger_sweep_branches(10).await.unwrap();
        sweep.sort_by_key(|(b, _)| b.as_uuid());
        let mut want = vec![(main, true), (child, true)];
        want.sort_by_key(|(b, _)| b.as_uuid());
        assert_eq!(sweep, want, "both branches still get synced");
        c.mark_branch_deleted(child).await.unwrap();
        assert!(c.trigger_drops(child).await.unwrap().is_empty(), "the tombstone purges them");
        s.finish().await;
    }

    /// m13: the dispatcher's record adds dead events, keeps the last failure until a delivery
    /// clears it, and stamps deliveries; a child's copy starts at zero. Dispatch targets are
    /// installed, enabled table triggers on ready branches, by Postgres database.
    #[tokio::test]
    #[ignore]
    async fn trigger_deliveries_recorded() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "hello").await;
        let c = &s.client;
        let t = c.add_trigger(&table_trigger(main, f, &["insert"])).await.unwrap();
        c.add_trigger(&storage_trigger(main, f, &["object.created"])).await.unwrap();
        assert!(c.dispatch_targets(10).await.unwrap().is_empty(), "pending rows aren't dispatched");
        install_all(c, main).await;
        assert_eq!(c.dispatch_targets(10).await.unwrap(), vec![(main, "app".to_string())]);
        let row = |list: Vec<chelabase_metadata::TriggerRecord>| list.into_iter().find(|r| r.id == t).unwrap();
        let fresh = row(c.list_triggers(main).await.unwrap());
        assert_eq!((fresh.dead_count, fresh.last_delivery_error, fresh.last_delivered_at), (0, None, None));
        assert_eq!(fresh.dropped_count, 0);
        let failed = TriggerDelivery { id: t, delivered: false, dead: 2, dropped: 4, error: Some("the function answered 500".into()) };
        assert_eq!(c.record_trigger_deliveries(main, std::slice::from_ref(&failed)).await.unwrap(), 1);
        let (_, other) = s.database().await;
        assert_eq!(c.record_trigger_deliveries(other, std::slice::from_ref(&failed)).await.unwrap(), 0, "another branch's");
        let after = row(c.list_triggers(main).await.unwrap());
        assert_eq!((after.dead_count, after.last_delivery_error.as_deref()), (2, Some("the function answered 500")));
        // m13b (B1): what the outbox bound removed is counted too.
        assert_eq!(after.dropped_count, 4);
        assert_eq!(after.last_delivered_at, None);
        c.record_trigger_deliveries(main, &[TriggerDelivery { id: t, delivered: true, dead: 1, dropped: 1, error: None }])
            .await
            .unwrap();
        let ok = row(c.list_triggers(main).await.unwrap());
        assert_eq!((ok.dead_count, ok.last_delivery_error, ok.dropped_count), (3, None, 5));
        assert!(ok.last_delivered_at.is_some());
        let child = s.branch(db, main, "dev").await;
        let copied = c.list_triggers(child).await.unwrap();
        assert!(copied.iter().all(|r| r.dead_count == 0 && r.dropped_count == 0 && r.last_delivered_at.is_none()), "{copied:?}");
        c.mark_branch_deleted(main).await.unwrap();
        assert!(c.dispatch_targets(10).await.unwrap().is_empty(), "a deleted branch is never dispatched");
        s.finish().await;
    }

    /// m14: the worker's storage targets are the enabled storage triggers of live branches of
    /// live databases, each with its function's name and database and its `enabled_since`; a
    /// disabled one, a table one, a deleted one and a tombstoned branch's are not.
    #[tokio::test]
    #[ignore]
    async fn storage_trigger_targets_are_enabled_live_storage_triggers() {
        let s = Scratch::new().await;
        let (db, main) = s.database().await;
        let f = s.function(db, "resize").await;
        let c = &s.client;
        let t = c.add_trigger(&storage_trigger(main, f, &["object.created"])).await.unwrap();
        let off = c.add_trigger(&storage_trigger(main, f, &["object.deleted"])).await.unwrap();
        c.update_trigger(main, f, off, false).await.unwrap().unwrap();
        c.add_trigger(&table_trigger(main, f, &["insert"])).await.unwrap();
        install_all(c, main).await;
        let mine = |list: Vec<chelabase_metadata::StorageTriggerTarget>| {
            list.into_iter().filter(|x| x.database_id == db).collect::<Vec<_>>()
        };
        let targets = mine(c.storage_trigger_targets().await.unwrap());
        assert_eq!(targets.len(), 1, "{targets:?}");
        let row = c.list_triggers(main).await.unwrap().into_iter().find(|r| r.id == t).unwrap();
        let got = &targets[0];
        assert_eq!(
            (got.id, got.branch_id, got.function_name.as_str(), got.pg_database.as_str()),
            (t, main, "resize", "app")
        );
        assert_eq!((got.bucket.as_deref(), got.prefix.as_deref()), (Some("avatars"), Some("in/")));
        assert_eq!((got.events.clone(), got.enabled_since), (vec!["object.created".to_string()], row.enabled_since));
        // Re-enabling the other one: a new generation.
        let on = c.update_trigger(main, f, off, true).await.unwrap().unwrap();
        let targets = mine(c.storage_trigger_targets().await.unwrap());
        assert_eq!(targets.iter().find(|x| x.id == off).map(|x| x.enabled_since), Some(on.enabled_since));
        let child = s.branch(db, main, "dev").await;
        assert_eq!(mine(c.storage_trigger_targets().await.unwrap()).len(), 4, "the child's copies");
        assert!(c.delete_trigger(main, f, t).await.unwrap());
        assert!(mine(c.storage_trigger_targets().await.unwrap()).iter().all(|x| x.id != t));
        c.mark_branch_deleted(child).await.unwrap();
        let left = mine(c.storage_trigger_targets().await.unwrap());
        assert!(left.iter().all(|x| x.branch_id == main), "{left:?}");
        sqlx::query("UPDATE databases SET status = 'deleting' WHERE id = $1")
            .bind(db.as_uuid())
            .execute(s.pool())
            .await
            .unwrap();
        assert!(mine(c.storage_trigger_targets().await.unwrap()).is_empty(), "a deleting database");
        s.finish().await;
    }
}
