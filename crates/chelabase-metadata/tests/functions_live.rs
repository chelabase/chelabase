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
        CreateBranchOutcome, DEMO_PROJECT_ID, DEMO_TENANT_ID, FunctionKind, MAX_BUNDLE_SIZE,
        MetadataClient, MetadataConfig, MetadataError, NewBranch, NewDeployment, NewSchedule,
        NewTrigger, TriggerSource, TriggerState,
    };
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
        assert!(
            !s.client.revoke_data_key(main, key).await.unwrap(),
            "a managed key isn't revoked directly"
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
}
