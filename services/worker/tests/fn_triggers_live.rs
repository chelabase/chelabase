//! Edge functions m8 against real computes: the function triggers' install (idempotent, closed
//! to the data API's roles, immune to a planted `public.pg_notify`, any table name), invisible
//! to the merge's catalog, the digest and the data API's introspection; a branch create's
//! truncated outbox and the child's own run-as keys; a restore's and a reset's reinstall.
//!
//! `shared-state`: provisions databases and branches on the workbench stack, deleted
//! afterwards pass or fail.
//!
//! ```text
//! just workbench test -p worker --test fn_triggers_live -- --run-ignored only
//! ```

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

mod fn_triggers {
    use std::time::{Duration, Instant};
    use chelabase_time::{Date, DateExt, Timestamp, TimestampExt};

    use chelabase_metadata::{
        BranchRecord, BranchStatus, CreateBranchOutcome, DEMO_TENANT_ID, DatabaseStatus,
        FunctionKind, MetadataClient, NewBranch, NewDeployment, NewTrigger, RunAs, TriggerRecord,
        TriggerSource, TriggerState,
    };
    use chelabase_nats::{BranchJob, PointInTime};
    use chelabase_neon::{ComputeBackend, SqlOpts};
    use chelabase_schema::catalog::{CATALOG_SQL, parse_catalog};
    use chelabase_schema::plan::admin_digest_sql;
    use chelabase_types::data_key::{self, DataKeyKind};
    use chelabase_types::{BranchId, DatabaseId, ProjectId};
    use uuid::Uuid;
    use worker::config::RealWorker;
    use worker::fn_triggers::{OWNER, RUN_AS_ROLE_MISSING, SYNC_LOCK_KEY, trigger_name};

    use crate::common::{cleanup, setup};

    const QUOTED: &str = r#"Ord"er s"#;

    /// m17: the run-as role every fixture function gets ([`Fx::function`]): BYPASSRLS, with
    /// `SELECT` on every table in `public` made by the admin or `postgres`, so a trigger
    /// installs as before the read rule. Made at provision, before any restore point.
    const RUNNER: &str = "fn_runner";

    struct Fx {
        worker: RealWorker,
        metadata: MetadataClient,
        project: ProjectId,
        id: DatabaseId,
        main: BranchId,
        database: String,
    }

    /// Panics, saying to rebuild, unless the shared metadata database has every migration of
    /// this checkout (`crates/chelabase-metadata/migrations`): this suite's in-process worker
    /// runs this checkout's queries.
    async fn stack_is_migrated(metadata: &MetadataClient) {
        let dir = crate::common::workspace_root().join("crates/chelabase-metadata/migrations");
        let newest: i64 = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .filter_map(|f| {
                let name = f.ok()?.file_name().into_string().ok()?;
                name.strip_suffix(".sql")?.split('_').next()?.parse().ok()
            })
            .max()
            .expect("a metadata migration");
        let applied: Option<i64> =
            sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE success")
                .fetch_one(metadata.pool())
                .await
                .expect("reading the metadata database's migrations");
        assert!(
            applied.is_some_and(|v| v >= newest),
            "the stack is behind this checkout: its metadata database's newest migration is \
             {applied:?}, this checkout's {newest}. Rebuild it (`just workbench up`)."
        );
    }

    impl Fx {
        async fn provision() -> Fx {
            let (worker, metadata, _config) = setup().await;
            // `set_run_as` seals the key, as the orchestrator's client does.
            let metadata = metadata.with_secrets_key(worker.key.clone());
            // The stack's orchestrator migrates the shared metadata database; this suite never
            // does, it only refuses to run against one behind this checkout.
            stack_is_migrated(&metadata).await;
            // #337: a live-test id, so the stack's worker leaves the database (and its
            // branches) to this suite.
            let id = worker::live_test::database_id();
            let name = format!("fn-{}", &id.to_string()[..8]);
            let project = metadata
                .create_project(DEMO_TENANT_ID.into(), &name)
                .await
                .expect("create a project")
                .id;
            metadata
                .insert_database_pending(id, DEMO_TENANT_ID.into(), project, &name)
                .await
                .expect("insert pending database");
            let provisioned =
                tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, &name))
                    .await;
            let ready = match provisioned {
                Ok(Ok(())) => metadata.get_database(id).await.ok().flatten(),
                _ => None,
            };
            let Some(record) = ready.filter(|r| r.status == DatabaseStatus::Ready) else {
                cleanup(&worker, &metadata, id).await;
                let _ = metadata.finish_project_delete(project).await;
                panic!("provision failed: {provisioned:?}");
            };
            let fx = Fx {
                database: record.database_name.clone().expect("database name"),
                main: BranchId::from(id),
                worker,
                metadata,
                project,
                id,
            };
            fx.sql(
                fx.main,
                &format!(
                    "CREATE ROLE {RUNNER} NOLOGIN BYPASSRLS;
                     ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO {RUNNER};
                     ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT SELECT ON TABLES TO {RUNNER};"
                ),
            )
            .await;
            fx
        }

        /// A role's OID on `branch`.
        async fn role_oid(&self, branch: BranchId, role: &str) -> i64 {
            self.value(branch, &format!("SELECT oid FROM pg_roles WHERE rolname = '{role}'"))
                .await
                .parse()
                .unwrap_or_else(|e| panic!("no role {role}: {e}"))
        }

        /// m17: sets `f`'s run-as role on `branch` to `role` (its OID there).
        async fn set_run_as(&self, branch: BranchId, f: Uuid, role: &str) {
            let oid = self.role_oid(branch, role).await;
            assert!(self.metadata.set_run_as(branch, f, Some(&run_as(role, oid))).await.unwrap());
        }

        async fn teardown(&self) {
            let _ = self.metadata.finish_project_delete(self.project).await;
            cleanup(&self.worker, &self.metadata, self.id).await;
        }

        async fn branch(&self, id: BranchId) -> BranchRecord {
            self.metadata
                .get_branch(id)
                .await
                .unwrap()
                .expect("branch row")
        }

        async fn on(&self, branch: BranchId, sql: &str) -> Result<String, String> {
            let ep = self.branch(branch).await.endpoint_id.expect("endpoint");
            self.worker
                .computes
                .admin_sql(&ep, &self.database, &crate::common::tenant_sql(sql))
                .await
                .map_err(|e| format!("{e:#}"))
        }

        /// `sql` in a session of the login role `user` (the compute trusts local connections),
        /// so the session user is a tenant's, not the superuser.
        async fn on_as(&self, branch: BranchId, user: &str, sql: &str) -> Result<String, String> {
            let ep = self.branch(branch).await.endpoint_id.expect("endpoint");
            let opts = SqlOpts {
                user: Some(user.to_string()),
                ..SqlOpts::admin_default()
            };
            self.worker
                .computes
                .admin_sql_with(&ep, &self.database, sql, &opts)
                .await
                .map_err(|e| format!("{e:#}"))
        }

        async fn sql(&self, branch: BranchId, sql: &str) -> String {
            self.on(branch, sql)
                .await
                .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e}"))
        }

        async fn value(&self, branch: BranchId, sql: &str) -> String {
            self.sql(branch, sql)
                .await
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("")
                .to_string()
        }

        /// A function with a deployment, active on main, running as [`RUNNER`].
        async fn function(&self, name: &str) -> Uuid {
            let f = self.function_without_run_as(name).await;
            self.set_run_as(self.main, f, RUNNER).await;
            f
        }

        /// A function with a deployment, active on main, with no run-as role.
        async fn function_without_run_as(&self, name: &str) -> Uuid {
            let f = self
                .metadata
                .create_function(self.id, name, FunctionKind::Function)
                .await
                .unwrap()
                .id;
            let spec = serde_json::json!({});
            let sha: String = std::iter::repeat_n('a', 64).collect();
            let d = self
                .metadata
                .create_deployment(&NewDeployment {
                    id: Uuid::new_v4(),
                    function_id: f,
                    bundle_sha256: Some(&sha),
                    bundle_size: Some(1),
                    spec: &spec,
                    compatibility_date: Date::ymd(2026, 11, 1).unwrap(),
                    created_by: "member-1",
                    note: None,
                })
                .await
                .unwrap()
                .id;
            self.metadata
                .set_active_deployment(self.main, f, d)
                .await
                .unwrap();
            f
        }

        async fn trigger(&self, branch: BranchId, f: Uuid, table: &str, events: &[&str]) -> Uuid {
            self.metadata
                .add_trigger(&NewTrigger {
                    branch_id: branch,
                    function_id: f,
                    source: TriggerSource::Table,
                    pg_database: &self.database,
                    schema_name: Some("public"),
                    table_name: Some(table),
                    bucket: None,
                    prefix: None,
                    events,
                })
                .await
                .unwrap()
        }

        async fn triggers(&self, branch: BranchId) -> Vec<TriggerRecord> {
            self.metadata.list_triggers(branch).await.unwrap()
        }

        /// The `chelabase_fn_*` trigger names in the database, sorted.
        async fn db_triggers(&self, branch: BranchId) -> String {
            self.value(
                branch,
                "SELECT coalesce(string_agg(tgname || '@' || tgrelid::regclass::text || '#' || oid, ',' ORDER BY tgname), '')
                 FROM pg_trigger WHERE tgname LIKE 'chelabase\\_fn\\_%'",
            )
            .await
        }

        async fn outbox(&self, branch: BranchId) -> i64 {
            self.value(branch, "SELECT count(*) FROM chelabase_functions.outbox")
                .await
                .parse()
                .unwrap()
        }

        async fn create(&self, name: &str) -> BranchId {
            let id = BranchId::new();
            let outcome = self
                .metadata
                .create_branch(&NewBranch {
                    id,
                    database_id: self.id,
                    name,
                    parent_branch_id: self.main,
                    branched_at_time: None,
                    branched_at_lsn: None,
                })
                .await
                .unwrap();
            assert_eq!(outcome, CreateBranchOutcome::Created);
            self.worker
                .handle_branch_job(BranchJob::Create {
                    branch_id: id,
                    at: None,
                })
                .await
                .expect("create job");
            assert_eq!(self.branch(id).await.status, BranchStatus::Ready);
            id
        }

        async fn claim(&self, id: BranchId) {
            assert!(
                self.metadata
                    .set_branch_status(id, &[BranchStatus::Ready], BranchStatus::Resetting)
                    .await
                    .unwrap()
            );
        }
    }

    fn names(ids: &[Uuid]) -> Vec<String> {
        let mut names: Vec<String> = ids.iter().map(|id| trigger_name(*id)).collect();
        names.sort();
        names
    }

    fn trigger_names(db_triggers: &str) -> Vec<String> {
        db_triggers
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.split('@').next().unwrap().to_string())
            .collect()
    }

    /// Runs `story` on the fixture, tearing it down pass or fail.
    async fn with_fx<F>(story: impl FnOnce(&'static Fx) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let fx: &'static Fx = Box::leak(Box::new(Fx::provision().await));
        let outcome = tokio::spawn(story(fx)).await;
        fx.teardown().await;
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic.into_panic());
        }
    }

    /// AC 1, 2, 3, 6, 7, 8: install, idempotence, the privilege matrix, the search_path hijack,
    /// quoted identifiers, and the catalog paths (model, digest, introspection) unchanged.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn install_is_idempotent_closed_and_invisible() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(
                main,
                r#"CREATE TABLE public.orders (id int PRIMARY KEY, note text);
                   CREATE TABLE public."Ord""er s" (id int PRIMARY KEY);
                   CREATE TABLE public.bystander (id int);
                   CREATE FUNCTION public.pg_notify(text, text) RETURNS void LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'hijacked pg_notify'; END$$;
                   CREATE FUNCTION public.jsonb_build_object(VARIADIC text[]) RETURNS jsonb LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'hijacked jsonb_build_object'; END$$;
                   CREATE FUNCTION public.to_jsonb(anyelement) RETURNS jsonb LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'hijacked to_jsonb'; END$$;"#,
            )
            .await;
            let catalog = |out: String| parse_catalog(out.trim()).unwrap();
            let model = catalog(fx.sql(main, CATALOG_SQL).await);
            let digest = fx.value(main, &admin_digest_sql()).await;
            let f = fx.function("hello").await;
            let orders = fx.trigger(main, f, "orders", &["insert"]).await;
            let quoted = fx.trigger(main, f, QUOTED, &["insert", "update"]).await;
            let missing = fx.trigger(main, f, "nope", &["delete"]).await;

            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let rows = fx.triggers(main).await;
            let state = |id: Uuid| {
                let t = rows.iter().find(|t| t.id == id).unwrap();
                (t.state, t.last_error.clone())
            };
            assert_eq!(state(orders), (TriggerState::Installed, None));
            assert_eq!(state(quoted), (TriggerState::Installed, None));
            assert_eq!(state(missing), (TriggerState::Failed, Some("table_missing".into())));
            let installed = fx.db_triggers(main).await;
            assert_eq!(trigger_names(&installed), names(&[orders, quoted]));
            assert!(installed.contains(r#"@"Ord""er s"#), "on the quoted table: {installed}");
            // Again: nothing changes (same trigger OIDs, same states).
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(fx.db_triggers(main).await, installed);
            assert_eq!(fx.triggers(main).await, rows);

            // Events land in the outbox through the catalog's functions, not the planted ones.
            fx.sql(main, r#"INSERT INTO public.orders VALUES (1, 'a'); INSERT INTO public."Ord""er s" VALUES (7); UPDATE public."Ord""er s" SET id = 8; INSERT INTO public.bystander VALUES (1);"#).await;
            let events = fx
                .value(
                    main,
                    "SELECT string_agg(trigger_id || ' ' || (payload->>'type') || ' ' || (payload->>'table') || ' ' || coalesce(payload->'record'->>'id', '-') || ' ' || (timeline_id = current_setting('neon.timeline_id')), ',' ORDER BY id) FROM chelabase_functions.outbox",
                )
                .await;
            assert_eq!(
                events,
                format!("{orders} INSERT orders 1 true,{quoted} INSERT {QUOTED} 7 true,{quoted} UPDATE {QUOTED} 8 true")
            );

            // The data API's roles: no schema, no outbox, no function.
            for role in ["anon", "authenticated", "authenticator"] {
                for statement in [
                    "SELECT * FROM chelabase_functions.outbox",
                    "INSERT INTO chelabase_functions.outbox (trigger_id, payload) VALUES (gen_random_uuid(), '{}')",
                    "SELECT chelabase_functions.enqueue()",
                    "CREATE TEMP TABLE x (id int); CREATE TRIGGER t AFTER INSERT ON x FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue('00000000-0000-0000-0000-000000000000')",
                ] {
                    let err = fx
                        .on(main, &format!("BEGIN; SET LOCAL ROLE {role}; {statement}; ROLLBACK;"))
                        .await
                        .expect_err(&format!("{role}: {statement}"));
                    assert!(err.contains("permission denied"), "{role}: {statement}: {err}");
                }
                let usage = fx
                    .value(
                        main,
                        &format!(
                            "SELECT has_schema_privilege('{role}', 'chelabase_functions', 'USAGE')::text
                             || has_function_privilege('{role}', 'chelabase_functions.enqueue()', 'EXECUTE')::text
                             || has_table_privilege('{role}', 'chelabase_functions.outbox', 'SELECT,INSERT,UPDATE,DELETE')::text"
                        ),
                    )
                    .await;
                assert_eq!(usage, "falsefalsefalse", "{role}");
            }
            let owner = fx
                .value(
                    main,
                    "SELECT rolcanlogin::text || rolsuper::text || rolbypassrls::text || (SELECT count(*) FROM pg_auth_members WHERE roleid = r.oid OR member = r.oid)
                     FROM pg_roles r WHERE rolname = 'chelabase_functions_owner'",
                )
                .await;
            assert_eq!(owner, "falsefalsefalse0");
            let definer = fx
                .value(
                    main,
                    "SELECT prosecdef::text || ' ' || proconfig::text || ' ' || pg_get_userbyid(proowner) FROM pg_proc WHERE oid = 'chelabase_functions.enqueue()'::regprocedure",
                )
                .await;
            assert_eq!(definer, r#"true {"search_path=pg_catalog, chelabase_functions, pg_temp"} chelabase_functions_owner"#);

            // The merge's catalog and the digest don't see any of it; neither does the data API.
            assert_eq!(catalog(fx.sql(main, CATALOG_SQL).await), model, "the model changed");
            assert_eq!(fx.value(main, &admin_digest_sql()).await, digest, "the digest changed");
            let reserved: Vec<String> = chelabase_query::schema::RESERVED_SCHEMAS
                .iter()
                .map(|s| format!("'{s}'"))
                .collect();
            let introspect = chelabase_query::schema::INTROSPECT_SQL
                .replace("$1", &format!("ARRAY[{}]::text[]", reserved.join(", ")));
            let schema = fx.value(main, &introspect).await;
            // Types are read whole (and filtered by schema later); schemas, relations and
            // functions are filtered in the SQL.
            let schema: serde_json::Value = serde_json::from_str(&schema).unwrap();
            for key in ["namespaces", "relations", "functions"] {
                let part = schema[key].to_string();
                assert!(!part.contains("chelabase_functions"), "{key}: {part}");
            }
            assert!(schema["relations"].to_string().contains("\"orders\""));

            // A trigger whose row is gone is dropped by the next sync.
            sqlx_delete_trigger(&fx.metadata, quoted).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[orders]));
        })
        .await;
    }

    /// The metadata has no trigger delete yet (m13's endpoint); the test deletes the row.
    async fn sqlx_delete_trigger(metadata: &MetadataClient, id: Uuid) {
        sqlx::query("DELETE FROM function_triggers WHERE id = $1")
            .bind(id)
            .execute(metadata.pool())
            .await
            .unwrap();
    }

    fn run_as(role: &str, oid: i64) -> RunAs {
        let generated = data_key::generate(DataKeyKind::Secret);
        RunAs {
            role: role.to_string(),
            role_oid: oid,
            role_timeline: None,
            role_bypass_rls: false,
            key_id: Uuid::new_v4(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            secret: generated.key,
        }
    }

    /// AC 4, 5, 9: a create truncates the child's outbox before its rows are installed (main's
    /// untouched) and mints the child's own run-as key (a role missing there is
    /// `run_as_role_missing`); a restore re-installs a trigger made after the point and drops
    /// one deleted after it, truncating; a reset re-installs and truncates too.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn create_restore_and_reset_reinstall_and_truncate() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(
                main,
                "CREATE TABLE public.orders (id int); CREATE TABLE public.later (id int);
                 CREATE ROLE reporter NOLOGIN; CREATE ROLE ghost NOLOGIN;
                 GRANT SELECT ON public.orders, public.later TO reporter;",
            )
            .await;
            let oid = |name: &'static str| async move {
                fx.value(
                    main,
                    &format!("SELECT oid FROM pg_roles WHERE rolname = '{name}'"),
                )
                .await
                .parse::<i64>()
                .unwrap()
            };
            let f = fx.function("hello").await;
            let g = fx.function("haunted").await;
            let main_key = run_as("reporter", oid("reporter").await);
            assert!(
                fx.metadata
                    .set_run_as(main, f, Some(&main_key))
                    .await
                    .unwrap()
            );
            assert!(
                fx.metadata
                    .set_run_as(main, g, Some(&run_as("ghost", oid("ghost").await)))
                    .await
                    .unwrap()
            );
            fx.sql(main, "DROP ROLE ghost").await;
            let orders = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2)")
                .await;
            assert_eq!(fx.outbox(main).await, 2);

            let dev = fx.create("dev").await;
            assert_eq!(fx.outbox(dev).await, 0, "the child's outbox starts empty");
            assert_eq!(fx.outbox(main).await, 2, "main's rows are untouched");
            let rows = fx.triggers(dev).await;
            assert!(
                rows.iter().all(|t| t.state == TriggerState::Installed),
                "{rows:?}"
            );
            let dev_orders = rows[0].id;
            assert_ne!(dev_orders, orders);
            assert_eq!(
                trigger_names(&fx.db_triggers(dev).await),
                names(&[dev_orders])
            );
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[orders]));
            // The child's own key, bound to the role's OID on the child.
            let state = fx.metadata.list_for_branch(dev).await.unwrap();
            let hello = state.iter().find(|s| s.function_id == f).unwrap();
            let (key_id, _) = fx
                .metadata
                .managed_run_as_key(dev, f)
                .await
                .unwrap()
                .expect("a child key");
            assert_eq!(hello.run_as_key_id, Some(key_id));
            assert_ne!(key_id, main_key.key_id);
            let bound: (Option<i64>, String) =
                sqlx::query_as("SELECT role_oid, role FROM data_api_keys WHERE id = $1")
                    .bind(key_id)
                    .fetch_one(fx.metadata.pool())
                    .await
                    .unwrap();
            let child_oid: i64 = fx
                .value(dev, "SELECT oid FROM pg_roles WHERE rolname = 'reporter'")
                .await
                .parse()
                .unwrap();
            assert_eq!(bound, (Some(child_oid), "reporter".to_string()));
            let haunted = state.iter().find(|s| s.function_id == g).unwrap();
            assert_eq!(
                (haunted.run_as_key_id, haunted.last_error.as_deref()),
                (None, Some(RUN_AS_ROLE_MISSING))
            );

            // Restore point T: dev has only `orders`. Then a trigger made after T, one deleted
            // after T, and events.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let noted = Timestamp::now();
            tokio::time::sleep(Duration::from_secs(2)).await;
            let later = fx.trigger(dev, f, "later", &["insert"]).await;
            sqlx_delete_trigger(&fx.metadata, dev_orders).await;
            fx.worker.sync_fn_triggers(dev, false).await.unwrap();
            assert_eq!(trigger_names(&fx.db_triggers(dev).await), names(&[later]));
            fx.sql(dev, "INSERT INTO public.later VALUES (1)").await;
            assert_eq!(fx.outbox(dev).await, 1);
            fx.sql(dev, "INSERT INTO public.orders VALUES (3)").await;
            fx.claim(dev).await;
            let started = Instant::now();
            fx.worker
                .handle_branch_job(BranchJob::Restore {
                    branch_id: dev,
                    backup_id: BranchId::new(),
                    backup_name: "dev-before".into(),
                    at: PointInTime::Timestamp(noted),
                })
                .await
                .expect("restore job");
            println!("restore took {:?}", started.elapsed());
            assert_eq!(fx.branch(dev).await.status, BranchStatus::Ready);
            let rows = fx.triggers(dev).await;
            assert_eq!(
                rows.iter().map(|t| (t.id, t.state)).collect::<Vec<_>>(),
                vec![(later, TriggerState::Installed)]
            );
            assert_eq!(
                trigger_names(&fx.db_triggers(dev).await),
                names(&[later]),
                "re-installed; the resurrected one dropped"
            );
            assert_eq!(fx.outbox(dev).await, 0, "truncated");

            // Reset to main: main's database (main-named trigger, two events) comes back.
            fx.sql(dev, "INSERT INTO public.later VALUES (2)").await;
            fx.claim(dev).await;
            fx.worker
                .handle_branch_job(BranchJob::ResetToParent { branch_id: dev })
                .await
                .expect("reset job");
            assert_eq!(fx.branch(dev).await.status, BranchStatus::Ready);
            assert!(
                fx.triggers(dev)
                    .await
                    .iter()
                    .all(|t| t.state == TriggerState::Installed)
            );
            assert_eq!(trigger_names(&fx.db_triggers(dev).await), names(&[later]));
            assert_eq!(fx.outbox(dev).await, 0, "main's events never reach dev");
            assert_eq!(fx.outbox(main).await, 2);
        })
        .await;
    }
    /// Audit B1: no column value is evaluated with a user's cast in `enqueue()`'s definer
    /// context. An enum with casts to json and text, a domain over it, a composite holding it
    /// and an array of it are written as their type output (strings); built-in types keep
    /// their JSON types. No hostile function runs, as anyone.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn hostile_casts_never_run_in_enqueue() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(
                main,
                r#"CREATE TABLE public.seen (who text); GRANT INSERT ON public.seen TO PUBLIC;
                   CREATE TYPE public.mood AS ENUM ('ok');
                   CREATE FUNCTION public.mood_json(public.mood) RETURNS json LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN '"x"'::json; END$$;
                   CREATE CAST (public.mood AS json) WITH FUNCTION public.mood_json(public.mood);
                   CREATE FUNCTION public.mood_text(public.mood) RETURNS text LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN 'x'; END$$;
                   CREATE CAST (public.mood AS text) WITH FUNCTION public.mood_text(public.mood);
                   CREATE FUNCTION public.mood_jsonb(public.mood) RETURNS jsonb LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN '"x"'::jsonb; END$$;
                   CREATE CAST (public.mood AS jsonb) WITH FUNCTION public.mood_jsonb(public.mood);
                   CREATE DOMAIN public.dmood AS public.mood;
                   CREATE TYPE public.pair AS (m public.mood, n int);
                   CREATE TABLE public.orders (id int, m public.mood, d public.dmood, p public.pair, a public.mood[], j jsonb, t text, z public.mood);"#,
            )
            .await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert", "update", "delete"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(
                main,
                r#"INSERT INTO public.orders VALUES (1, 'ok', 'ok', ROW('ok', 2), ARRAY['ok']::public.mood[], '{"k": [1]}', 'hi', NULL);
                   UPDATE public.orders SET id = 2; DELETE FROM public.orders;"#,
            )
            .await;
            let who = fx.value(main, "SELECT coalesce(string_agg(who, ','), '') FROM public.seen").await;
            assert_ne!(who, "chelabase_functions_owner", "user cast function ran as the owner role");
            assert_eq!(who, "", "a user cast ran");
            let record = fx
                .value(main, "SELECT payload->'record' FROM chelabase_functions.outbox ORDER BY id LIMIT 1")
                .await;
            let record: serde_json::Value = serde_json::from_str(&record).unwrap();
            assert_eq!(
                record,
                serde_json::json!({"id": 1, "m": "ok", "d": "ok", "p": "(ok,2)", "a": "{ok}", "j": {"k": [1]}, "t": "hi", "z": null})
            );
            let ops = fx
                .value(main, "SELECT string_agg((payload->>'type') || ':' || coalesce(payload->'old_record'->>'id', '-'), ',' ORDER BY id) FROM chelabase_functions.outbox")
                .await;
            assert_eq!(ops, "INSERT:-,UPDATE:1,DELETE:2");
        })
        .await;
    }

    /// Audit B2 and the worker's cleanup: grants to named roles on the schema, the outbox, its
    /// sequence and `enqueue()` are revoked on every run; a tampered outbox (a TRUNCATE
    /// trigger, a rule, a policy) is rebuilt before anything runs on it, so the cleanup never
    /// runs a planted function; a stray function in the schema is dropped.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn named_grants_and_a_tampered_outbox_are_repaired() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE TABLE public.seen (who text); GRANT INSERT ON public.seen TO PUBLIC;").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let outbox_oid = "SELECT 'chelabase_functions.outbox'::regclass::oid";
            let untouched = fx.value(main, outbox_oid).await;
            fx.sql(
                main,
                "GRANT USAGE ON SCHEMA chelabase_functions TO authenticated;
                 GRANT SELECT ON chelabase_functions.outbox TO authenticated;
                 GRANT USAGE ON SEQUENCE chelabase_functions.outbox_id_seq TO authenticated;
                 GRANT EXECUTE ON FUNCTION chelabase_functions.enqueue() TO authenticated;",
            )
            .await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let grants = "SELECT has_schema_privilege('authenticated', 'chelabase_functions', 'USAGE')::text
                 || has_table_privilege('authenticated', 'chelabase_functions.outbox', 'SELECT')::text
                 || has_sequence_privilege('authenticated', 'chelabase_functions.outbox_id_seq', 'USAGE')::text
                 || has_function_privilege('authenticated', 'chelabase_functions.enqueue()', 'EXECUTE')::text";
            assert_eq!(fx.value(main, grants).await, "falsefalsefalsefalse", "a named grant survived the install");
            assert_eq!(fx.value(main, outbox_oid).await, untouched, "an untampered outbox is kept");

            // An event before the tampering (the planted policy would run on any insert; the
            // test plants it as the superuser, which no member can be).
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            fx.sql(
                main,
                r#"CREATE FUNCTION public.spy() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN NULL; END$$;
                   CREATE TRIGGER spy AFTER TRUNCATE ON chelabase_functions.outbox FOR EACH STATEMENT EXECUTE FUNCTION public.spy();
                   CREATE TRIGGER spy_rows AFTER DELETE ON chelabase_functions.outbox FOR EACH ROW EXECUTE FUNCTION public.spy();
                   CREATE RULE spy_rule AS ON DELETE TO chelabase_functions.outbox DO ALSO INSERT INTO public.seen VALUES ('rule:' || current_user);
                   CREATE FUNCTION public.spy_policy() RETURNS boolean LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES ('policy:' || current_user); RETURN true; END$$;
                   ALTER TABLE chelabase_functions.outbox ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
                   CREATE POLICY spy ON chelabase_functions.outbox USING (public.spy_policy());"#,
            )
            .await;
            fx.worker.sync_fn_triggers(main, true).await.unwrap();
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(fx.value(main, "SELECT coalesce(string_agg(who, ','), '') FROM public.seen").await, "", "a planted function ran");
            let shape = fx
                .value(
                    main,
                    "SELECT (SELECT count(*) FROM pg_trigger WHERE tgrelid = 'chelabase_functions.outbox'::regclass)
                     + (SELECT count(*) FROM pg_rewrite WHERE ev_class = 'chelabase_functions.outbox'::regclass)
                     + (SELECT count(*) FROM pg_policy WHERE polrelid = 'chelabase_functions.outbox'::regclass)
                     + (SELECT count(*) FROM pg_class WHERE oid = 'chelabase_functions.outbox'::regclass AND relrowsecurity)
                     + (SELECT count(*) FROM pg_proc WHERE pronamespace = 'chelabase_functions'::regnamespace AND proname <> 'enqueue')",
                )
                .await;
            assert_eq!(shape, "0", "the outbox kept a planted object");
            assert_eq!(fx.value(main, grants).await, "falsefalsefalsefalse");
            fx.sql(main, "INSERT INTO public.orders VALUES (2)").await;
            assert_eq!(fx.outbox(main).await, 1, "events land in the rebuilt outbox");
        })
        .await;
    }

    /// Audit S1: when a create's own sync failed, the sweep (no truncate) still leaves the
    /// child's outbox without the rows its inherited parent triggers wrote on its timeline,
    /// and keeps the rows of its own triggers.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn sweep_after_a_failed_create_sync_drops_foreign_events() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int)").await;
            let f = fx.function("hello").await;
            let parent = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let dev = fx.create("dev").await;
            let mine = fx.triggers(dev).await[0].id;
            // The state a failed first sync leaves: the parent's trigger still in the child's
            // database, writing on the child's timeline, and the child's rows pending.
            let run_as = worker::fn_triggers::run_as_oids(&fx.metadata.list_for_branch(main).await.unwrap());
            let parent_wanted = worker::fn_triggers::wanted_in(&fx.triggers(main).await, &fx.database, &run_as);
            let oid = |name: &'static str| async move {
                fx.value(dev, &format!("SELECT oid FROM pg_roles WHERE rolname = '{name}'")).await.parse::<u32>().ok()
            };
            let role = worker::fn_triggers::Roles {
                owner: oid("chelabase_functions_owner").await,
                dispatch: oid("chelabase_functions_dispatch").await,
            };
            fx.sql(dev, &worker::fn_triggers::sync_sql(&parent_wanted, false, role))
                .await;
            fx.sql(dev, "INSERT INTO public.orders VALUES (1)").await;
            sqlx::query("UPDATE function_triggers SET state = 'pending_install' WHERE branch_id = $1")
                .bind(dev.as_uuid())
                .execute(fx.metadata.pool())
                .await
                .unwrap();
            fx.worker.sync_pending_fn_triggers().await;
            assert!(fx.triggers(dev).await.iter().all(|t| t.state == TriggerState::Installed));
            fx.sql(dev, "INSERT INTO public.orders VALUES (2)").await;
            let ids = fx
                .value(dev, "SELECT coalesce(string_agg(DISTINCT trigger_id::text, ','), '') FROM chelabase_functions.outbox")
                .await;
            assert_eq!(ids, mine.to_string(), "the parent's {parent} events stayed");
        })
        .await;
    }

    /// Audit S3: a child's run-as key that can't be minted (the all-rows check can't read a
    /// database) is recorded as `run_as_key_pending: …`, and a later reconcile pass mints it
    /// once the check runs, clearing the error.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn child_run_as_key_retried_until_minted() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE reporter NOLOGIN").await;
            let oid: i64 = fx.value(main, "SELECT oid FROM pg_roles WHERE rolname = 'reporter'").await.parse().unwrap();
            let f = fx.function("hello").await;
            assert!(fx.metadata.set_run_as(main, f, Some(&run_as("reporter", oid))).await.unwrap());
            // A database no session can open: the all-rows check fails on it.
            fx.sql(main, "CREATE DATABASE fn_broken").await;
            fx.sql(main, "ALTER DATABASE fn_broken SET session_preload_libraries = 'chelabase_no_such_library'").await;
            let dev = fx.create("dev").await;
            let state = |b: BranchId| async move {
                let all = fx.metadata.list_for_branch(b).await.unwrap();
                let s = all.into_iter().find(|s| s.function_id == f).unwrap();
                (s.run_as_key_id, s.last_error)
            };
            let (key, error) = state(dev).await;
            assert_eq!(key, None);
            assert!(error.as_deref().is_some_and(|e| e.starts_with("run_as_key_pending: ")), "{error:?}");
            fx.worker.sync_pending_run_as().await;
            assert_eq!(state(dev).await.0, None, "still pending while the check can't run");
            // Round 2 (S3): an invocation error recorded meanwhile doesn't stop the retry, and
            // isn't cleared (it isn't the mint's).
            fx.metadata
                .set_function_last_error(dev, f, Some("TypeError: boom"))
                .await
                .unwrap();
            fx.sql(dev, "ALTER DATABASE fn_broken RESET session_preload_libraries").await;
            fx.worker.sync_pending_run_as().await;
            let (key, error) = state(dev).await;
            assert!(key.is_some(), "minted on a later pass");
            assert_eq!(error.as_deref(), Some("TypeError: boom"));
        })
        .await;
    }

    /// Where a move is set up, and when.
    #[derive(Clone, Copy, PartialEq)]
    enum At {
        /// The shared database, before the first sync.
        SharedBefore,
        /// The shared database, after the first sync, before the sweep.
        SharedBetween,
        /// A database of its own, before its first sync (the compute's roles are its own).
        Own,
    }

    /// What a move must end in.
    enum Expect {
        /// The table's trigger is installed and its events reach the outbox.
        Flows(&'static str),
        /// The table's trigger row is `failed` with this error prefix (nothing installed).
        Failed(&'static str, &'static str),
        /// The attacker's statement itself is refused.
        Refused,
    }

    /// The invariant (`worker::fn_triggers` module docs): no object a database member can
    /// create or alter ever runs with the rights of `chelabase_functions_owner` or the worker's
    /// superuser connection, and the install never adopts an object it didn't create. Each
    /// attacker move is set up before or between syncs; afterwards the outbox's owner and ACL
    /// and `enqueue()` are as installed, no planted function ran (each logs `current_user`
    /// into `public.seen`), and events flow or the row is `failed`. `owner` setups run as the
    /// database owner (`postgres`), the others as the superuser (a state from the past).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn attack_matrix_keeps_the_invariant() {
        const SPY: &str = r#"CREATE TABLE IF NOT EXISTS public.seen (who text); GRANT INSERT ON public.seen TO PUBLIC;
            CREATE OR REPLACE FUNCTION public.spy() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN NULL; END$$;
            CREATE OR REPLACE FUNCTION public.spy_bool() RETURNS boolean LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN true; END$$;"#;
        // (name, where, as the owner, setup SQL, table to trigger on, expected outcome)
        let moves: [(&str, At, bool, &str, &str, Expect); 23] = [
            ("hostile casts", At::SharedBefore, true,
             r#"CREATE TYPE public.mood AS ENUM ('ok');
                CREATE FUNCTION public.mood_json(public.mood) RETURNS json LANGUAGE plpgsql AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN '"x"'::json; END$$;
                CREATE CAST (public.mood AS json) WITH FUNCTION public.mood_json(public.mood);
                CREATE DOMAIN public.dmood AS public.mood; CREATE TYPE public.pair AS (m public.mood, n int);
                CREATE TABLE public.casts (id int, m public.mood, d public.dmood, p public.pair, a public.mood[]);"#,
             "casts", Expect::Flows("casts")),
            ("partitioned table", At::SharedBefore, true,
             "CREATE TABLE public.parts (id int) PARTITION BY RANGE (id); CREATE TABLE public.parts_1 PARTITION OF public.parts FOR VALUES FROM (0) TO (10);",
             "parts", Expect::Failed("parts", "function triggers support plain tables only")),
            ("named grants", At::SharedBetween, false,
             "GRANT USAGE ON SCHEMA chelabase_functions TO authenticated; GRANT SELECT, INSERT ON chelabase_functions.outbox TO authenticated;
              GRANT USAGE ON SEQUENCE chelabase_functions.outbox_id_seq TO authenticated; GRANT EXECUTE ON FUNCTION chelabase_functions.enqueue() TO authenticated;",
             "", Expect::Flows("plain")),
            ("tampered outbox", At::SharedBetween, false,
             "CREATE TRIGGER spy AFTER DELETE ON chelabase_functions.outbox FOR EACH ROW EXECUTE FUNCTION public.spy();
              CREATE RULE spy_rule AS ON DELETE TO chelabase_functions.outbox DO ALSO INSERT INTO public.seen VALUES ('rule:' || current_user);
              ALTER TABLE chelabase_functions.outbox ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
              CREATE POLICY spy ON chelabase_functions.outbox USING (public.spy_bool());
              ALTER TABLE chelabase_functions.outbox ADD COLUMN extra text DEFAULT 'x';",
             "", Expect::Flows("plain")),
            ("trigger dropped", At::SharedBetween, true, "DROP TRIGGER {drop} ON public.dropped;", "dropped", Expect::Flows("dropped")),
            ("trigger disabled", At::SharedBetween, true, "ALTER TABLE public.disabled DISABLE TRIGGER USER;", "disabled", Expect::Flows("disabled")),
            ("ON TRUNCATE trigger", At::SharedBetween, true,
             "CREATE TRIGGER spy_truncate AFTER TRUNCATE ON chelabase_functions.outbox FOR EACH STATEMENT EXECUTE FUNCTION public.spy();",
             "", Expect::Refused),
            ("pre-created role and schema", At::Own, true,
             "CREATE ROLE chelabase_functions_owner NOLOGIN; GRANT chelabase_functions_owner TO postgres WITH SET TRUE;
              CREATE SCHEMA chelabase_functions AUTHORIZATION chelabase_functions_owner;
              GRANT CREATE ON SCHEMA public TO chelabase_functions_owner;
              CREATE FUNCTION public.leftover() RETURNS text LANGUAGE sql SECURITY DEFINER AS 'SELECT current_user::text';
              ALTER FUNCTION public.leftover() OWNER TO chelabase_functions_owner;",
             "orders", Expect::Failed("orders", "chelabase_functions_foreign: the role chelabase_functions_owner has attributes")),
            ("role owning an object in another database", At::Own, false,
             "CREATE ROLE chelabase_functions_owner NOLOGIN NOINHERIT CONNECTION LIMIT 0; CREATE DATABASE fn_other;",
             "orders", Expect::Failed("orders", "chelabase_functions_foreign: the role chelabase_functions_owner owns objects outside")),
            // Round 4 (B2): names the owner role where it gives nobody anything; never a refusal.
            ("default privileges to the owner role, by authenticated", At::SharedBetween, false,
             "BEGIN; SET LOCAL ROLE authenticated; ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO chelabase_functions_owner; COMMIT;",
             "", Expect::Flows("plain")),
            ("large object granted to the owner role, by authenticated", At::SharedBetween, false,
             "BEGIN; SET LOCAL ROLE authenticated; SELECT lo_create(4243); GRANT SELECT ON LARGE OBJECT 4243 TO chelabase_functions_owner; COMMIT;",
             "", Expect::Flows("plain")),
            ("policy naming the owner role", At::SharedBetween, true,
             "ALTER TABLE public.plain ENABLE ROW LEVEL SECURITY; CREATE POLICY spy_policy ON public.plain TO chelabase_functions_owner USING (public.spy_bool());",
             "", Expect::Flows("plain")),
            // Round 4 (B3): an Admin names the owner role as a key's or a function's run-as role.
            ("owner role as a key role", At::SharedBetween, false, "@check_data_role chelabase_functions_owner", "", Expect::Refused),
            // Drift of the machinery itself (a state from the past): enqueue() replaced by a
            // superuser is put back by the catalog comparison, and the spy never runs.
            ("replaced enqueue()", At::SharedBetween, false,
             "CREATE OR REPLACE FUNCTION chelabase_functions.enqueue() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER AS $$BEGIN INSERT INTO public.seen VALUES (current_user); RETURN NULL; END$$;",
             "", Expect::Flows("plain")),
            // m13b: the dispatch login's grants are an exact allowlist; anything more is revoked.
            ("grants to the dispatch role", At::SharedBetween, false,
             "GRANT SELECT ON public.plain TO chelabase_functions_dispatch;
              GRANT INSERT, TRUNCATE ON chelabase_functions.outbox TO chelabase_functions_dispatch;
              GRANT UPDATE (payload) ON chelabase_functions.outbox TO chelabase_functions_dispatch;
              GRANT SELECT ON chelabase_functions.outbox TO chelabase_functions_dispatch WITH GRANT OPTION;
              GRANT USAGE ON SEQUENCE chelabase_functions.outbox_id_seq TO chelabase_functions_dispatch;
              GRANT EXECUTE ON FUNCTION chelabase_functions.enqueue() TO chelabase_functions_dispatch;
              GRANT CREATE ON SCHEMA chelabase_functions TO chelabase_functions_dispatch;",
             "", Expect::Flows("plain")),
            ("default privileges to the dispatch role, by authenticated", At::SharedBetween, false,
             "BEGIN; SET LOCAL ROLE authenticated; ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO chelabase_functions_dispatch; COMMIT;",
             "", Expect::Flows("plain")),
            ("dispatch role as a key role", At::SharedBetween, false, "@check_data_role chelabase_functions_dispatch", "", Expect::Refused),
            // m13b: any attribute or membership on the dispatch login refuses the install.
            ("pre-made dispatch role with an attribute", At::Own, false,
             "CREATE ROLE chelabase_functions_dispatch LOGIN NOINHERIT CREATEDB CONNECTION LIMIT 40;",
             "orders", Expect::Failed("orders", "chelabase_functions_foreign: the role chelabase_functions_dispatch has attributes")),
            ("pre-made dispatch role with a membership", At::Own, false,
             "CREATE ROLE chelabase_functions_dispatch LOGIN NOINHERIT CONNECTION LIMIT 40; GRANT pg_read_all_data TO chelabase_functions_dispatch;",
             "orders", Expect::Failed("orders", "chelabase_functions_foreign: the role chelabase_functions_dispatch has members or memberships")),
            // m17: the read rule. `noselect` and `revoked` are the shared function's (run as
            // RUNNER, BYPASSRLS); `rlsnew` and `forced` the `rls` function's (run as
            // `rls_reader`: no BYPASSRLS, SELECT on `rlsnew`, owner of `forced`).
            ("run-as role without SELECT", At::SharedBefore, false,
             "REVOKE SELECT ON public.noselect FROM fn_runner;",
             "noselect", Expect::Failed("noselect", "run_as_role_cannot_read_table")),
            ("SELECT revoked between syncs", At::SharedBetween, false,
             "REVOKE SELECT ON public.revoked FROM fn_runner;",
             "", Expect::Failed("revoked", "run_as_role_cannot_read_table")),
            ("RLS enabled between syncs", At::SharedBetween, false,
             "ALTER TABLE public.rlsnew ENABLE ROW LEVEL SECURITY;",
             "", Expect::Failed("rlsnew", "run_as_role_subject_to_rls")),
            ("RLS forced on an owner run-as role", At::SharedBetween, false,
             "ALTER TABLE public.forced ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;",
             "", Expect::Failed("forced", "run_as_role_subject_to_rls")),
        ];
        let shared: &'static Fx = Box::leak(Box::new(Fx::provision().await));
        let mut own: Vec<&'static Fx> = Vec::new();
        for _ in moves.iter().filter(|m| m.1 == At::Own) {
            own.push(Box::leak(Box::new(Fx::provision().await)));
        }
        let all: Vec<&'static Fx> = std::iter::once(shared).chain(own.iter().copied()).collect();
        let every = all.clone();
        let outcome = tokio::spawn(async move {
            let all = every;
            let as_owner = |sql: &str| format!("BEGIN; SET LOCAL ROLE postgres; {sql}; COMMIT;");
            for fx in std::iter::once(shared).chain(own.iter().copied()) {
                fx.sql(fx.main, SPY).await;
                fx.sql(fx.main, &as_owner("CREATE TABLE public.orders (id int); CREATE TABLE public.plain (id int); CREATE TABLE public.dropped (id int); CREATE TABLE public.disabled (id int)")).await;
            }
            shared
                .sql(shared.main, "CREATE TABLE public.noselect (id int); CREATE TABLE public.revoked (id int);
                    CREATE ROLE rls_reader NOLOGIN; CREATE TABLE public.rlsnew (id int); GRANT SELECT ON public.rlsnew TO rls_reader;
                    CREATE TABLE public.forced (id int); ALTER TABLE public.forced OWNER TO rls_reader;")
                .await;
            let mut own_iter = own.iter().copied();
            let mut placed: Vec<(&Fx, usize)> = Vec::new();
            for (i, (name, at, owner, sql, table, _)) in moves.iter().enumerate() {
                let fx = if *at == At::Own { own_iter.next().unwrap() } else { shared };
                placed.push((fx, i));
                if *at != At::SharedBetween {
                    let sql = if *owner { as_owner(sql) } else { sql.to_string() };
                    fx.on(fx.main, &sql).await.unwrap_or_else(|e| panic!("{name}: {e}"));
                    if name.contains("another database") {
                        let ep = fx.branch(fx.main).await.endpoint_id.unwrap();
                        fx.worker
                            .computes
                            .admin_sql(&ep, "fn_other", "CREATE TABLE public.theirs (id int); ALTER TABLE public.theirs OWNER TO chelabase_functions_owner;")
                            .await
                            .unwrap();
                    }
                }
                let _ = table;
            }
            // Triggers on every table a move names, plus the shared `plain` one and the
            // drift targets, then the first sync.
            let f = shared.function("hello").await;
            let mut ids = std::collections::HashMap::new();
            for t in ["casts", "parts", "plain", "dropped", "disabled", "noselect", "revoked"] {
                ids.insert(t, shared.trigger(shared.main, f, t, &["insert"]).await);
            }
            let g = shared.function_without_run_as("rls").await;
            shared.set_run_as(shared.main, g, "rls_reader").await;
            for t in ["rlsnew", "forced"] {
                ids.insert(t, shared.trigger(shared.main, g, t, &["insert"]).await);
            }
            for fx in &own {
                let g = fx.function("hello").await;
                fx.trigger(fx.main, g, "orders", &["insert"]).await;
            }
            for fx in &all {
                let _ = fx.worker.sync_fn_triggers(fx.main, false).await;
            }
            let acl = "SELECT (SELECT relowner::regrole::text || ' ' || coalesce(relacl::text, '') FROM pg_class WHERE oid = 'chelabase_functions.outbox'::regclass)
                || ' | ' || (SELECT coalesce(relacl::text, '') FROM pg_class WHERE oid = 'chelabase_functions.outbox_id_seq'::regclass)
                || ' | ' || (SELECT nspowner::regrole::text || ' ' || coalesce(nspacl::text, '') FROM pg_namespace WHERE nspname = 'chelabase_functions')
                || ' | ' || (SELECT proowner::regrole::text || ' ' || coalesce(proacl::text, '') || ' ' || md5(prosrc) || ' ' || prosecdef || ' ' || proconfig::text
                     FROM pg_proc WHERE oid = 'chelabase_functions.enqueue()'::regprocedure)";
            let installed = shared.value(shared.main, acl).await;
            assert!(installed.starts_with("chelabase_functions_owner "), "{installed}");
            // Between syncs.
            let mut refused = Vec::new();
            for (i, (name, at, owner, sql, _, expect)) in moves.iter().enumerate() {
                if *at != At::SharedBetween {
                    continue;
                }
                if let Some(role) = sql.strip_prefix("@check_data_role ") {
                    // The worker's key role check, as a data key's create or a run-as PATCH.
                    let check = shared.worker.check_data_role(shared.main, role).await;
                    assert!(matches!(expect, Expect::Refused), "{name}");
                    assert!(matches!(check, worker::data_api::RoleCheck::Refused(_)), "{name}: {check:?}");
                    refused.push(i);
                    continue;
                }
                let sql = sql.replace("{drop}", &trigger_name(ids["dropped"]));
                let sql = if *owner { as_owner(&sql) } else { sql };
                let result = shared.on(shared.main, &sql).await;
                match expect {
                    Expect::Refused => {
                        let err = result.expect_err(name);
                        assert!(err.contains("permission denied"), "{name}: {err}");
                        refused.push(i);
                    }
                    _ => {
                        result.unwrap_or_else(|e| panic!("{name}: {e}"));
                    }
                }
            }
            // The owner can still add and attach partitions (round 2, B1).
            shared
                .sql(shared.main, &as_owner("CREATE TABLE public.parts_2 PARTITION OF public.parts FOR VALUES FROM (10) TO (20); CREATE TABLE public.parts_3 (id int); ALTER TABLE public.parts ATTACH PARTITION public.parts_3 FOR VALUES FROM (20) TO (30)"))
                .await;
            // What the reconcile sweep runs for each branch: repair, revoke, drift.
            for fx in &all {
                let _ = fx.worker.sync_fn_triggers(fx.main, false).await;
            }
            // Round 4 (B1): with everything in place, a tenant transaction that wrote to a
            // triggered table stays open through the next sweep (the steady state, every 30 s);
            // another tenant writer must not queue behind the sweep.
            let long = tokio::spawn(async move {
                shared.on(shared.main, "BEGIN; INSERT INTO public.plain VALUES (0); SELECT pg_sleep(8); COMMIT;").await
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
            for fx in &all {
                let _ = fx.worker.sync_fn_triggers(fx.main, false).await;
            }
            let other = shared
                .on(shared.main, &as_owner("SET LOCAL statement_timeout = '3s'; INSERT INTO public.plain VALUES (-1)"))
                .await;
            assert!(other.is_ok(), "a tenant write queued behind the sweep: {other:?}");
            let _ = long.await;
            assert_eq!(shared.value(shared.main, acl).await, installed, "the outbox, its ACL or enqueue() changed");
            for (fx, i) in &placed {
                let (name, _, _, _, _, expect) = &moves[*i];
                let rows = fx.triggers(fx.main).await;
                match expect {
                    Expect::Flows(table) => {
                        let row = rows.iter().find(|t| t.table_name.as_deref() == Some(*table)).unwrap();
                        assert_eq!((row.state, row.last_error.as_deref()), (TriggerState::Installed, None), "{name}");
                        let before = fx.outbox(fx.main).await;
                        let insert = match *table {
                            "casts" => "INSERT INTO public.casts VALUES (1, 'ok', 'ok', ROW('ok', 1), ARRAY['ok']::public.mood[])".to_string(),
                            t => format!("INSERT INTO public.{t} (id) VALUES (1)"),
                        };
                        fx.sql(fx.main, &as_owner(&insert)).await;
                        assert_eq!(fx.outbox(fx.main).await, before + 1, "{name}: no event");
                    }
                    Expect::Failed(table, prefix) => {
                        let row = rows.iter().find(|t| t.table_name.as_deref() == Some(*table)).unwrap();
                        assert_eq!(row.state, TriggerState::Failed, "{name}");
                        assert!(row.last_error.as_deref().is_some_and(|e| e.starts_with(prefix)), "{name}: {:?}", row.last_error);
                        assert!(!fx.db_triggers(fx.main).await.contains(&trigger_name(row.id)), "{name}: installed");
                    }
                    Expect::Refused => assert!(refused.contains(i), "{name}"),
                }
            }
            for fx in &own {
                // Never adopted: no enqueue() was made in a schema it didn't create.
                assert_eq!(fx.value(fx.main, "SELECT to_regprocedure('chelabase_functions.enqueue()') IS NULL").await, "t");
            }
            for fx in &all {
                assert_eq!(fx.value(fx.main, "SELECT coalesce(string_agg(who, ','), '') FROM public.seen").await, "", "a planted function ran");
            }
            assert_eq!(
                shared.value(shared.main, "SELECT has_table_privilege('authenticated', 'chelabase_functions.outbox', 'SELECT')::text || has_function_privilege('authenticated', 'chelabase_functions.enqueue()', 'EXECUTE')::text").await,
                "falsefalse"
            );
            // m13b: after the sweep, the dispatch login holds exactly its allowlist.
            assert_eq!(
                shared.value(shared.main, "SELECT concat_ws(',',
                    has_schema_privilege('chelabase_functions_dispatch', 'chelabase_functions', 'USAGE'),
                    has_schema_privilege('chelabase_functions_dispatch', 'chelabase_functions', 'CREATE'),
                    has_table_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox', 'SELECT'),
                    has_table_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox', 'UPDATE'),
                    has_table_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox', 'DELETE'),
                    has_table_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox', 'INSERT'),
                    has_table_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox', 'TRUNCATE'),
                    has_table_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox', 'SELECT WITH GRANT OPTION'),
                    (SELECT attacl IS NULL FROM pg_attribute WHERE attrelid = 'chelabase_functions.outbox'::regclass AND attname = 'payload'),
                    has_sequence_privilege('chelabase_functions_dispatch', 'chelabase_functions.outbox_id_seq', 'USAGE'),
                    has_function_privilege('chelabase_functions_dispatch', 'chelabase_functions.enqueue()', 'EXECUTE'),
                    has_table_privilege('chelabase_functions_dispatch', 'public.plain', 'SELECT'),
                    (SELECT count(*) FROM pg_default_acl a, aclexplode(a.defaclacl) x
                      WHERE x.grantee = 'chelabase_functions_dispatch'::regrole))").await,
                "t,f,t,t,t,f,f,f,t,f,f,f,0"
            );
        })
        .await;
        for fx in all {
            fx.teardown().await;
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic.into_panic());
        }
    }

    /// Round 3 (S1): a `failed` row is never wanted. Once its table appears, the sweep (kept
    /// there by another installed trigger) doesn't install its database trigger.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn a_failed_row_is_never_installed() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int)").await;
            let f = fx.function("hello").await;
            let a = fx.trigger(main, f, "orders", &["insert"]).await;
            let b = fx.trigger(main, f, "later", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let st = |rows: Vec<TriggerRecord>, id: Uuid| {
                rows.into_iter().find(|t| t.id == id).unwrap().state
            };
            assert_eq!(st(fx.triggers(main).await, b), TriggerState::Failed);
            fx.sql(main, "CREATE TABLE public.later (id int)").await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.later VALUES (1)").await;
            assert_eq!(st(fx.triggers(main).await, b), TriggerState::Failed);
            assert_eq!(
                (
                    trigger_names(&fx.db_triggers(main).await),
                    fx.outbox(main).await
                ),
                (names(&[a]), 0),
                "a failed row's trigger was installed and is capturing rows"
            );
        })
        .await;
    }

    /// Round 3 (S2): a refusal found after the install (here an object outside the schema
    /// owned by the owner role, a state from the past) fails the rows and drops every database
    /// trigger running `enqueue()`, so nothing keeps filling an outbox nobody reads.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn refusal_after_install_drops_the_triggers() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int)").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "CREATE TABLE public.theirs (id int); ALTER TABLE public.theirs OWNER TO chelabase_functions_owner;").await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let rows = fx.triggers(main).await;
            assert_eq!(rows[0].state, TriggerState::Failed);
            assert!(rows[0].last_error.as_deref().is_some_and(|e| e.starts_with("chelabase_functions_foreign")), "{rows:?}");
            assert_eq!(trigger_names(&fx.db_triggers(main).await), Vec::<String>::new(), "a refused database kept its triggers");
        })
        .await;
    }

    /// Round 3 (S2): a table owner granting the owner role rights on their own table gives
    /// nobody the role's rights (nobody can act as it, and `enqueue()` reads no table), so it
    /// doesn't refuse the install; the grant is revoked.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn a_grant_to_the_owner_role_is_revoked_not_refused() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); ALTER TABLE public.orders OWNER TO postgres").await;
            let f = fx.function("hello").await;
            let a = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "BEGIN; SET LOCAL ROLE postgres; GRANT SELECT ON public.orders TO chelabase_functions_owner; GRANT CREATE ON SCHEMA public TO chelabase_functions_owner; COMMIT;").await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let rows = fx.triggers(main).await;
            assert_eq!((rows[0].state, rows[0].last_error.as_deref()), (TriggerState::Installed, None));
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[a]));
            assert_eq!(
                fx.value(main, "SELECT has_table_privilege('chelabase_functions_owner', 'public.orders', 'SELECT')::text || has_schema_privilege('chelabase_functions_owner', 'public', 'CREATE')::text").await,
                "falsefalse",
                "the grants to the owner role were revoked"
            );
        })
        .await;
    }

    /// Round 3 audit's escalation matrix: the database owner can't reach the owner role or
    /// change the machinery.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn database_owner_cannot_escalate() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); ALTER TABLE public.orders OWNER TO postgres").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let mut ok = Vec::new();
            for stmt in [
                "GRANT chelabase_functions_owner TO postgres",
                "ALTER ROLE chelabase_functions_owner LOGIN PASSWORD 'x'",
                "ALTER ROLE chelabase_functions_owner SET search_path = public",
                "ALTER FUNCTION chelabase_functions.enqueue() SECURITY INVOKER",
                "CREATE OR REPLACE FUNCTION chelabase_functions.enqueue() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NULL; END$$",
                "CREATE FUNCTION chelabase_functions.x() RETURNS int LANGUAGE sql AS 'SELECT 1'",
                "ALTER TABLE chelabase_functions.outbox ADD COLUMN x int",
                "CREATE TRIGGER t2 AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION chelabase_functions.enqueue('00000000-0000-0000-0000-000000000000')",
                "ALTER SCHEMA chelabase_functions OWNER TO postgres",
                "DROP SCHEMA chelabase_functions CASCADE",
                "CREATE TRIGGER spy AFTER DELETE ON chelabase_functions.outbox FOR EACH ROW EXECUTE FUNCTION pg_catalog.suppress_redundant_updates_trigger()",
                "COMMENT ON ROLE chelabase_functions_owner IS 'x'",
                "ALTER DEFAULT PRIVILEGES FOR ROLE chelabase_functions_owner GRANT SELECT ON TABLES TO postgres",
                "REASSIGN OWNED BY chelabase_functions_owner TO postgres",
                "DROP OWNED BY chelabase_functions_owner",
                "SECURITY LABEL ON ROLE chelabase_functions_owner IS 'x'",
            ] {
                if fx.on(main, &format!("BEGIN; SET LOCAL ROLE postgres; {stmt}; ROLLBACK;")).await.is_ok() {
                    ok.push(stmt);
                }
            }
            assert!(ok.is_empty(), "the database owner could: {ok:#?}");
        })
        .await;
    }

    /// Round 3 (B1), the invariant's second half: a member who clears the child's run-as role
    /// while the worker checks it keeps it cleared; the mint writes nothing.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn run_as_mint_never_overrides_a_concurrent_change() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE reporter NOLOGIN").await;
            let oid: i64 = fx.value(main, "SELECT oid FROM pg_roles WHERE rolname = 'reporter'").await.parse().unwrap();
            let f = fx.function("hello").await;
            assert!(fx.metadata.set_run_as(main, f, Some(&run_as("reporter", oid))).await.unwrap());
            let dev = fx.create("dev").await;
            // The state a first mint that couldn't run leaves.
            sqlx::query("UPDATE data_api_keys SET revoked_at = now(), sealed = NULL WHERE branch_id = $1 AND managed_by_function_id = $2")
                .bind(dev.as_uuid()).bind(f).execute(fx.metadata.pool()).await.unwrap();
            sqlx::query("UPDATE branch_functions SET run_as_key_id = NULL WHERE branch_id = $1")
                .bind(dev.as_uuid()).execute(fx.metadata.pool()).await.unwrap();
            let mut tx = fx.metadata.pool().begin().await.unwrap();
            sqlx::query("SELECT 1 FROM branch_functions WHERE branch_id = $1 AND function_id = $2 FOR UPDATE")
                .bind(dev.as_uuid()).bind(f).execute(&mut *tx).await.unwrap();
            let mint = tokio::spawn(async move { fx.worker.sync_pending_run_as().await });
            let mut waited = false;
            for _ in 0..300 {
                let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' AND query LIKE '%FOR NO KEY UPDATE OF bf%'")
                    .fetch_one(fx.metadata.pool()).await.unwrap();
                if n > 0 {
                    waited = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(waited, "the mint never reached its write");
            // The member clears the run-as role (what set_run_as(None) leaves).
            sqlx::query("UPDATE branch_functions SET run_as_role = NULL, run_as_key_id = NULL, run_as_role_oid = NULL WHERE branch_id = $1 AND function_id = $2")
                .bind(dev.as_uuid()).bind(f).execute(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            mint.await.unwrap();
            let s = fx.metadata.list_for_branch(dev).await.unwrap().into_iter().find(|s| s.function_id == f).unwrap();
            assert_eq!((s.run_as_role.as_deref(), s.run_as_key_id), (None, None), "the worker put a cleared run-as role back");
        })
        .await;
    }

    /// Round 3 (B2), the invariant's first half: a child's key is bound only to the role OID
    /// the member approved on the parent. A role missing at the branch point stays missing
    /// for a later namesake; a namesake with another OID is `changed`; neither is retried.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn child_run_as_key_needs_the_approved_oid() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE ghost NOLOGIN; CREATE ROLE twin NOLOGIN")
                .await;
            let oid = |name: &'static str| async move {
                fx.value(
                    main,
                    &format!("SELECT oid FROM pg_roles WHERE rolname = '{name}'"),
                )
                .await
                .parse::<i64>()
                .unwrap()
            };
            let f = fx.function("hello").await;
            let g = fx.function("twinned").await;
            assert!(
                fx.metadata
                    .set_run_as(main, f, Some(&run_as("ghost", oid("ghost").await)))
                    .await
                    .unwrap()
            );
            assert!(
                fx.metadata
                    .set_run_as(main, g, Some(&run_as("twin", oid("twin").await)))
                    .await
                    .unwrap()
            );
            // `ghost` is gone at the branch point; `twin` is another role by the same name.
            fx.sql(
                main,
                "DROP ROLE ghost; DROP ROLE twin; CREATE ROLE twin NOLOGIN",
            )
            .await;
            let dev = fx.create("dev").await;
            let st = |b: BranchId, func: Uuid| async move {
                let s = fx
                    .metadata
                    .list_for_branch(b)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|s| s.function_id == func)
                    .unwrap();
                (s.run_as_key_id, s.last_error)
            };
            assert_eq!(
                st(dev, f).await,
                (None, Some(RUN_AS_ROLE_MISSING.to_string()))
            );
            let (key, error) = st(dev, g).await;
            assert_eq!(key, None);
            assert!(
                error
                    .as_deref()
                    .is_some_and(|e| e.starts_with("run_as_role_changed")),
                "{error:?}"
            );
            // Someone with CREATEROLE makes a role under the old name on the child.
            fx.sql(
                dev,
                "BEGIN; SET LOCAL ROLE postgres; CREATE ROLE ghost NOLOGIN; COMMIT;",
            )
            .await;
            fx.worker.sync_pending_run_as().await;
            let (key, err) = st(dev, f).await;
            assert_eq!(
                key, None,
                "a key was minted for a new role named ghost (error now {err:?})"
            );
            assert_eq!(st(dev, g).await.0, None);
            assert!(
                fx.metadata
                    .pending_run_as_branches(None, 1000)
                    .await
                    .unwrap()
                    .iter()
                    .all(|b| *b != dev),
                "terminal: never retried"
            );
        })
        .await;
    }

    /// Round 4 (B1), the third invariant: the drift sweep (every reconcile pass, for every
    /// branch with an installed trigger) never blocks the tenant's writes. A write transaction
    /// open on a triggered table holds a lock on the outbox; the sweep neither waits on it nor
    /// makes other writers queue behind it, and runs no DDL at all while the machinery is in
    /// place (the catalog rows of the schema, the outbox and `enqueue()` keep their `xmin`).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn sweep_never_blocks_tenant_writes() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int)").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            const ROWS: &str = "SELECT (SELECT xmin FROM pg_namespace WHERE nspname = 'chelabase_functions') || ' ' \
                || (SELECT xmin FROM pg_class WHERE oid = 'chelabase_functions.outbox'::regclass) || ' ' \
                || (SELECT xmin FROM pg_proc WHERE oid = 'chelabase_functions.enqueue()'::regprocedure)";
            let rows_before = fx.value(main, ROWS).await;
            // A tenant transaction that wrote to a triggered table and is still open (8 s).
            let long = tokio::spawn(async move {
                fx.on(main, "BEGIN; INSERT INTO public.orders VALUES (1); SELECT pg_sleep(8); COMMIT;").await
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
            // The reconcile sweep's sync of an already-installed branch.
            let sweep = tokio::spawn(async move { fx.worker.sync_fn_triggers(main, false).await });
            tokio::time::sleep(Duration::from_secs(1)).await;
            // Another tenant writer meanwhile: it must not wait behind the sweep.
            let started = Instant::now();
            let other = fx
                .on(main, "SET statement_timeout = '3s'; INSERT INTO public.orders VALUES (2);")
                .await;
            let waited = started.elapsed();
            let locks = fx
                .value(main, "SELECT coalesce(string_agg(l.mode || ':' || l.granted, ','), '') FROM pg_locks l WHERE l.relation = 'chelabase_functions.outbox'::regclass")
                .await;
            let _ = long.await;
            let sweep = sweep.await.unwrap();
            assert!(
                other.is_ok(),
                "a tenant INSERT stalled {waited:?} behind the drift sweep: {other:?}; outbox locks then: {locks}; sweep result {sweep:?}"
            );
            assert_eq!(sweep.map(drop), Ok(()), "the sweep failed on the tenant's open transaction");
            assert_eq!(
                fx.value(main, ROWS).await,
                rows_before,
                "the sweep ran DDL on machinery that was in place (a worker of the stack running the pre-fix code sweeps this branch too, every 30 s)"
            );
            assert_eq!(fx.outbox(main).await, 2);
        })
        .await;
    }

    /// Round 4 (B3): the owner role never backs a data key or a run-as key. The worker's role
    /// check (the one every secret key and run-as role goes through) refuses it by name, so it
    /// is never granted to `authenticator` and the next sync still finds it clean.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn owner_role_never_backs_a_key() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int)").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let check = fx.worker.check_data_role(main, OWNER).await;
            let member = fx
                .value(main, "SELECT pg_has_role('authenticator', 'chelabase_functions_owner', 'MEMBER')::text")
                .await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let rows = fx.triggers(main).await;
            let states: Vec<_> = rows.iter().map(|t| (t.state, t.last_error.clone())).collect();
            assert!(
                matches!(check, worker::data_api::RoleCheck::Refused(_)),
                "the owner role passed the key role check: {check:?}; authenticator now a member: {member}; triggers after the next sync: {states:?}"
            );
            assert_eq!(member, "false");
            assert_eq!(states, vec![(TriggerState::Installed, None)]);
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t]));
        })
        .await;
    }

    /// Round 4 (B2), the third invariant: a role with no rights at all (`authenticated`), or a
    /// table owner, can't turn off the database's function triggers by naming the owner role
    /// where it gives nobody anything: a default privilege for its own future objects, a large
    /// object's grant, a policy. The grants are revoked; the policy is left to its table's
    /// owner (taking the role out of it would change their row security).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn unprivileged_role_cannot_disable_the_triggers() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE TABLE public.mine (id int); ALTER TABLE public.mine OWNER TO postgres").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            for (name, sql) in [
                ("default privileges", "BEGIN; SET LOCAL ROLE authenticated; ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO chelabase_functions_owner; COMMIT;"),
                ("large object", "BEGIN; SET LOCAL ROLE authenticated; SELECT lo_create(4242); GRANT SELECT ON LARGE OBJECT 4242 TO chelabase_functions_owner; COMMIT;"),
                ("policy", "BEGIN; SET LOCAL ROLE postgres; ALTER TABLE public.mine ENABLE ROW LEVEL SECURITY; CREATE POLICY p ON public.mine TO chelabase_functions_owner USING (true); COMMIT;"),
            ] {
                let moved = fx.on(main, sql).await;
                assert!(moved.is_ok(), "setup {name}: {moved:?}");
                fx.worker.sync_fn_triggers(main, false).await.unwrap();
                let rows = fx.triggers(main).await;
                let row = rows.iter().find(|r| r.id == t).unwrap();
                assert_eq!(
                    (row.state, row.last_error.clone(), trigger_names(&fx.db_triggers(main).await)),
                    (TriggerState::Installed, None, names(&[t])),
                    "{name}: disabled the branch's function triggers"
                );
            }
            // What names the role afterwards, beyond what it owns: only the policy.
            assert_eq!(
                fx.value(main, "SELECT coalesce(string_agg(d.deptype::text || ':' || d.classid::regclass::text, ',' ORDER BY 1), '') FROM pg_shdepend d WHERE d.refobjid = 'chelabase_functions_owner'::regrole AND d.refclassid = 'pg_authid'::regclass AND d.deptype <> 'o'").await,
                "r:pg_policy",
                "the grants to the owner role were not revoked"
            );
            // (The compute has default privileges of its own; none names the owner role.)
            assert_eq!(
                fx.value(main, "SELECT count(*) FROM pg_default_acl d CROSS JOIN LATERAL aclexplode(d.defaclacl) a WHERE a.grantee = 'chelabase_functions_owner'::regrole").await,
                "0"
            );
            assert_eq!(
                fx.value(main, "SELECT count(*) FROM pg_largeobject_metadata m CROSS JOIN LATERAL aclexplode(m.lomacl) a WHERE m.oid = 4242 AND a.grantee = 'chelabase_functions_owner'::regrole").await,
                "0"
            );
        })
        .await;
    }

    /// The third invariant, step-back: any session can hold the sync's advisory key
    /// (`pg_advisory_lock` is everyone's). A tenant squatting on it never stalls the install,
    /// and two syncs of one database still never run at once (the second yields and is
    /// retried, never failed).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn squatted_advisory_lock_never_stalls_the_install() {
        with_fx(|fx| async move {
            let main = fx.main;
            // A login role with no rights at all, in a session of its own (the holder check
            // reads the session user, which a `SET ROLE` never changes).
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE squatter LOGIN NOINHERIT").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            let squat = tokio::spawn(async move {
                fx.on_as(main, "squatter", &format!("BEGIN; SELECT pg_advisory_xact_lock({SYNC_LOCK_KEY}); SELECT pg_sleep(8); COMMIT;")).await
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
            let started = Instant::now();
            let synced = fx.worker.sync_fn_triggers(main, false).await;
            let took = started.elapsed();
            let rows = fx.triggers(main).await;
            assert_eq!(
                (synced, rows[0].state, rows[0].last_error.clone()),
                (Ok(true), TriggerState::Installed, None),
                "a session holding the sync's advisory key stalled the install ({took:?})"
            );
            assert!(took < Duration::from_secs(5), "waited on the squatter: {took:?}");
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t]));
            let _ = squat.await;
            // Two syncs at once: both answer Ok, the trigger stays as it is.
            let (a, b) = tokio::join!(
                fx.worker.sync_fn_triggers(main, false),
                fx.worker.sync_fn_triggers(main, false)
            );
            assert_eq!((a.map(drop), b.map(drop)), (Ok(()), Ok(())), "one may yield to the other");
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t]));
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            assert_eq!(fx.outbox(main).await, 1);
        })
        .await;
    }

    // m13b (#186): the functions service's dispatcher, in this process, connected to the
    // fixture's compute as `chelabase_functions_dispatch`.
    //
    // S1: the fixtures' functions stay *disabled* in the metadata, so the stack's own
    // dispatcher holds their events (it neither delivers nor deletes a pending row); the
    // dispatcher here reads the metadata through `Meta`, which shows the functions in `on` as
    // enabled. Only it delivers.

    use functions::events::dispatch::{Dispatcher, EventsMetadata, Invoker, Pass, Session, Woken};
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use worker::fn_triggers::DISPATCH;

    /// The metadata, with the functions in `on` shown as enabled; while `down`, reading a
    /// branch fails (m13b fix S1: a metadata blip mid-batch).
    struct Meta {
        inner: MetadataClient,
        on: Mutex<HashSet<Uuid>>,
        down: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl EventsMetadata for Meta {
        async fn branch(&self, b: BranchId) -> Result<Option<BranchRecord>, String> {
            if self.down.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("metadata down".into());
            }
            self.inner.branch(b).await
        }
        async fn triggers(&self, b: BranchId) -> Result<Vec<TriggerRecord>, String> {
            self.inner.triggers(b).await
        }
        async fn functions(&self, b: BranchId) -> Result<Vec<chelabase_metadata::BranchFunction>, String> {
            let on = self.on.lock().unwrap().clone();
            Ok(EventsMetadata::functions(&self.inner, b)
                .await?
                .into_iter()
                .map(|mut f| {
                    f.enabled = on.contains(&f.function_id);
                    f
                })
                .collect())
        }
        async fn targets(&self, limit: i64, running: &[(BranchId, String)]) -> Result<Vec<(BranchId, String)>, String> {
            self.inner.targets(limit, running).await
        }
        async fn record(&self, b: BranchId, d: &[chelabase_metadata::TriggerDelivery]) -> Result<(), String> {
            self.inner.record(b, d).await
        }
    }

    /// The dispatch login's connection info as the orchestrator's functions route answers it:
    /// the compute's address and TLS name from the stack's data route, the user and the branch's
    /// password from the metadata (the stack's orchestrator may predate the functions route).
    struct Source {
        data: chelabase_pgsession::InternalSource,
        metadata: MetadataClient,
        key: chelabase_secrets::SecretKey,
    }

    impl chelabase_pgsession::ConnSource for Source {
        async fn fetch(&self, branch: Uuid) -> Result<chelabase_pgsession::ConnInfo, chelabase_pgsession::ConnError> {
            let mut info = self.data.fetch(branch).await?;
            let row = self.metadata.branch_connection(BranchId(branch)).await.unwrap().unwrap();
            let sealed = row
                .functions_password_enc
                .ok_or_else(|| chelabase_pgsession::ConnError::NotReady("no dispatch login yet".into()))?;
            info.user = DISPATCH.into();
            info.password = chelabase_secrets::open_credential(
                &self.key,
                &branch.to_string(),
                chelabase_secrets::Purpose::FunctionsDispatch,
                &sealed,
            )
            .unwrap();
            Ok(info)
        }
    }

    /// A change made just before a delivery (m13b: mid-batch).
    type Hook = Box<dyn FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

    /// m13: an invoker that records each event's body and answers the scripted statuses in
    /// turn (then 200); `hook` runs before each delivery (m13b: a change mid-batch).
    struct Scripted {
        statuses: Mutex<Vec<u16>>,
        seen: Mutex<Vec<(Instant, serde_json::Value)>>,
        hook: Mutex<Option<Hook>>,
    }

    impl Scripted {
        fn new(statuses: &[u16]) -> Arc<Scripted> {
            Arc::new(Scripted {
                statuses: Mutex::new(statuses.iter().rev().copied().collect()),
                seen: Mutex::new(Vec::new()),
                hook: Mutex::new(None),
            })
        }
        fn seen(&self) -> Vec<serde_json::Value> {
            self.seen.lock().unwrap().iter().map(|(_, v)| v.clone()).collect()
        }
    }

    #[async_trait::async_trait]
    impl Invoker for Scripted {
        async fn deliver(&self, _: BranchId, _: &str, body: Vec<u8>) -> Result<u16, String> {
            let hook = self.hook.lock().unwrap().as_mut().map(|h| h());
            if let Some(hook) = hook {
                hook.await;
            }
            self.seen.lock().unwrap().push((Instant::now(), serde_json::from_slice(&body).unwrap()));
            Ok(self.statuses.lock().unwrap().pop().unwrap_or(200))
        }
    }

    type Disp = Dispatcher<Meta, Source>;

    /// The dispatcher for `fx` with `invoker`, and its metadata view (functions in `on`).
    async fn dispatcher(fx: &Fx, invoker: Arc<Scripted>, on: &[Uuid]) -> (Disp, Arc<Meta>) {
        let url = std::env::var("CHELABASE_INTERNAL_URL").unwrap_or_else(|_| "http://orchestrator:8083".into());
        let token = std::fs::read_to_string(std::env::var("CHELABASE_STORAGE_INTERNAL_TOKEN_FILE").expect("the storage bearer"))
            .unwrap()
            .trim()
            .to_string();
        let ca = std::env::var("CHELABASE_COMPUTE_CA_FILE").expect("the compute CA");
        let tls = chelabase_query::server_tls::ServerTls::from_ca_file(std::path::Path::new(&ca)).unwrap();
        let meta = Arc::new(Meta { inner: fx.metadata.clone(), on: Mutex::new(on.iter().copied().collect()), down: Default::default() });
        let source = Source {
            data: chelabase_pgsession::InternalSource::new(&url, &token),
            metadata: fx.metadata.clone(),
            key: fx.worker.key.clone(),
        };
        (Dispatcher::new(meta.clone(), source, Some(tls), invoker), meta)
    }

    /// A session on main's database (the connection info is fresh: a sync just set the login).
    async fn session(fx: &Fx, d: &Disp) -> Session {
        d.open(fx.main, &fx.database).await.unwrap().expect("something to dispatch")
    }

    /// Passes until the outbox is quiet: a retried row's backoff (under 20 s) is waited out.
    async fn drive(d: &Disp, s: &mut Session, passes: usize) -> Pass {
        let mut last = Pass::Stop;
        for _ in 0..passes {
            last = d.pass(s).await.unwrap();
            match last {
                Pass::More => {}
                Pass::Idle(idle) if idle < Duration::from_secs(20) => {
                    d.wait(s, idle).await.unwrap();
                }
                _ => break,
            }
        }
        last
    }

    async fn trigger_row(fx: &Fx, id: Uuid) -> TriggerRecord {
        fx.triggers(fx.main).await.into_iter().find(|r| r.id == id).unwrap()
    }

    /// m13 AC: a committed insert is delivered once, a rolled-back one never; 500 three times
    /// then 200 is four deliveries and the row removed; a forged row (another trigger id,
    /// another table, another timeline) is deleted, never delivered.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_delivers_at_least_once_and_drops_forged_rows() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY, note text); CREATE TABLE public.secrets (id int PRIMARY KEY, s text);").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "BEGIN; INSERT INTO public.orders VALUES (9, 'no'); ROLLBACK; INSERT INTO public.orders VALUES (1, 'a');").await;
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            let seen = invoker.seen();
            assert_eq!(seen.len(), 1, "{seen:?}");
            assert_eq!(seen[0]["type"], "INSERT");
            assert_eq!(seen[0]["record"], serde_json::json!({"id": 1, "note": "a"}));
            assert!(seen[0]["txid"].as_u64().is_some());
            assert_eq!(fx.outbox(main).await, 0);

            let failing = Scripted::new(&[500, 500, 500]);
            let (d, _) = dispatcher(fx, failing.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            fx.sql(main, "INSERT INTO public.orders VALUES (2, 'b');").await;
            drive(&d, &mut s, 8).await;
            assert_eq!(failing.seen().len(), 4, "three failures, then the 2xx");
            assert_eq!(fx.outbox(main).await, 0);
            let row = trigger_row(fx, t).await;
            assert_eq!(row.last_delivery_error, None, "the delivery cleared it");
            assert!(row.last_delivered_at.is_some());

            // Forgeries, written straight into the outbox as an owner-level role could.
            let tl = fx.value(main, "SELECT current_setting('neon.timeline_id')").await;
            fx.sql(main, &format!(
                "INSERT INTO chelabase_functions.outbox (trigger_id, payload, timeline_id) VALUES
                   ('{}', '{{\"type\":\"INSERT\",\"schema\":\"public\",\"table\":\"orders\",\"record\":{{\"id\":5}},\"old_record\":null}}', '{tl}'),
                   ('{t}', '{{\"type\":\"INSERT\",\"schema\":\"public\",\"table\":\"secrets\",\"record\":{{\"id\":6}},\"old_record\":null}}', '{tl}'),
                   ('{t}', '{{\"type\":\"INSERT\",\"schema\":\"public\",\"table\":\"orders\",\"record\":{{\"id\":7}},\"old_record\":null}}', 'another-timeline')",
                Uuid::new_v4()
            )).await;
            let quiet = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, quiet.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert!(quiet.seen().is_empty(), "{:?}", quiet.seen());
            assert_eq!(fx.outbox(main).await, 0, "the forged rows were deleted");
        })
        .await;
    }

    /// m13b AC: the dispatch login reads, claims and deletes outbox rows and nothing else: no
    /// member table, no insert into the outbox, no `enqueue()`, no sequence; LOGIN NOINHERIT
    /// with no other attribute and no membership, so `SET ROLE` to anything fails. A disabled
    /// function's events queue until it is enabled again.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatch_role_privilege_matrix() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY); INSERT INTO public.orders VALUES (0);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let attrs = fx
                .value(main, &format!(
                    "SELECT rolcanlogin::text || rolinherit::text || rolsuper::text || rolcreaterole::text || rolcreatedb::text
                       || rolreplication::text || rolbypassrls::text || rolconnlimit::text
                       || (SELECT count(*) FROM pg_auth_members m WHERE m.member = r.oid OR m.roleid = r.oid)::text
                     FROM pg_roles r WHERE rolname = '{DISPATCH}'"
                ))
                .await;
            assert_eq!(attrs, "truefalsefalsefalsefalsefalsefalse400", "LOGIN NOINHERIT, nothing else, no membership");
            for (what, sql) in [
                ("select a member table", "SELECT count(*) FROM public.orders"),
                ("insert a member table", "INSERT INTO public.orders VALUES (5)"),
                ("update a member table", "UPDATE public.orders SET id = 6"),
                ("delete a member table", "DELETE FROM public.orders"),
                ("call enqueue()", "SELECT chelabase_functions.enqueue()"),
                ("insert into the outbox", "INSERT INTO chelabase_functions.outbox (trigger_id, payload) VALUES (gen_random_uuid(), '{}')"),
                ("the outbox's sequence", "SELECT nextval('chelabase_functions.outbox_id_seq')"),
                ("set role to the owner", "SET ROLE chelabase_functions_owner"),
                ("set role to the database owner", "SET ROLE postgres"),
                ("set role to authenticated", "SET ROLE authenticated"),
                ("set_config role", "SELECT set_config('role', 'chelabase_functions_owner', false)"),
                ("create in the schema", "CREATE TABLE chelabase_functions.x (id int)"),
            ] {
                let out = fx.on_as(main, DISPATCH, sql).await;
                assert!(
                    out.as_ref().is_err_and(|e| e.contains("permission denied") || e.contains("must be")),
                    "{what}: {out:?}"
                );
            }
            for sql in [
                "SELECT count(*) FROM chelabase_functions.outbox",
                "UPDATE chelabase_functions.outbox SET attempts = attempts WHERE false",
                "DELETE FROM chelabase_functions.outbox WHERE false",
            ] {
                fx.on_as(main, DISPATCH, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
            }

            // The function is disabled: events queue, and are delivered once it is enabled.
            fx.sql(main, "INSERT INTO public.orders VALUES (1);").await;
            let invoker = Scripted::new(&[]);
            let (d, meta) = dispatcher(fx, invoker.clone(), &[]).await;
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert!(invoker.seen().is_empty());
            assert_eq!(fx.outbox(main).await, 1, "queued while the function is off");
            meta.on.lock().unwrap().insert(f);
            d.pass(&mut s).await.unwrap();
            assert_eq!(invoker.seen().len(), 1, "delivered once it is on again");
        })
        .await;
    }

    /// m13b AC: an insert is delivered within 1 s, woken by LISTEN, not by the sweep.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_wakes_on_listen() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            let Pass::Idle(idle) = d.pass(&mut s).await.unwrap() else { panic!("an empty outbox is idle") };
            assert!(idle > Duration::from_secs(20), "nothing due: the sweep's wait, {idle:?}");
            let insert = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let at = Instant::now();
                fx.sql(main, "INSERT INTO public.orders VALUES (1);").await;
                at
            });
            assert_eq!(d.wait(&mut s, idle).await.unwrap(), Woken::Notified);
            d.pass(&mut s).await.unwrap();
            let committed = insert.await.unwrap();
            let seen = invoker.seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            let took = seen[0].0.duration_since(committed);
            assert!(took < Duration::from_secs(1), "delivered {took:?} after the insert");
        })
        .await;
    }

    /// m13b AC (B1): the bound never deletes a pending row; dead rows over the cap are removed
    /// and counted in the trigger's `dropped_count`.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn outbox_cap_never_drops_a_pending_event() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let n = functions::events::OUTBOX_CAP + 5;
            fx.sql(main, &format!("INSERT INTO public.orders SELECT g FROM generate_series(1, {n}) g;")).await;
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            assert_eq!(d.pass(&mut s).await.unwrap(), Pass::More, "a sweep, then a whole batch");
            let delivered = invoker.seen().len() as i64;
            let row = trigger_row(fx, t).await;
            assert_eq!(
                (delivered + fx.outbox(main).await, row.dead_count, row.dropped_count),
                (n, 0, 0),
                "no pending event was lost"
            );

            // Dead rows past the cap: the oldest go, counted.
            fx.sql(main, "DELETE FROM chelabase_functions.outbox;").await;
            let tl = fx.value(main, "SELECT current_setting('neon.timeline_id')").await;
            let dead = functions::events::OUTBOX_CAP + 3;
            fx.sql(main, &format!(
                "INSERT INTO chelabase_functions.outbox (trigger_id, payload, timeline_id, status, attempts)
                 SELECT '{t}', '{{}}', '{tl}', 'dead', 10 FROM generate_series(1, {dead});
                 INSERT INTO public.orders VALUES (0);"
            )).await;
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert_eq!(
                fx.value(main, "SELECT count(*) FILTER (WHERE status = 'dead')::text || ' ' || count(*) FILTER (WHERE status = 'pending')::text FROM chelabase_functions.outbox").await,
                format!("{} 0", functions::events::OUTBOX_CAP),
                "the cap kept the newest dead rows; the pending one was delivered"
            );
            assert_eq!(trigger_row(fx, t).await.dropped_count, 3);
        })
        .await;
    }

    /// m13b AC (S3): a trigger disabled or deleted while its batch is claimed gets no further
    /// delivery (its rows are deleted, a disabled one's counted); a function turned off
    /// mid-batch keeps its rows queued (released, the attempt taken back).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_stops_on_delete_or_disable_mid_batch() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let invoker = Scripted::new(&[]);
            let (d, meta) = dispatcher(fx, invoker.clone(), &[f]).await;

            // The function goes off after the first delivery: the other two are released.
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2), (3);").await;
            let off = meta.clone();
            *invoker.hook.lock().unwrap() = Some(Box::new(move || {
                off.on.lock().unwrap().clear();
                Box::pin(async {})
            }));
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert_eq!(invoker.seen().len(), 1, "no delivery after the function went off");
            assert_eq!(
                fx.value(main, "SELECT count(*)::text || ' ' || max(attempts)::text FROM chelabase_functions.outbox WHERE status = 'pending'").await,
                "2 0",
                "released, the attempt taken back"
            );

            // The trigger is disabled after the next delivery: the last row is deleted, counted.
            meta.on.lock().unwrap().insert(f);
            let m = fx.metadata.clone();
            *invoker.hook.lock().unwrap() = Some(Box::new(move || {
                let m = m.clone();
                Box::pin(async move {
                    m.update_trigger(main, f, t, false).await.unwrap();
                })
            }));
            d.pass(&mut s).await.unwrap();
            assert_eq!(invoker.seen().len(), 2);
            assert_eq!(fx.outbox(main).await, 0);
            assert_eq!(trigger_row(fx, t).await.dropped_count, 1);

            // A deleted trigger: likewise, from the next row on.
            let u = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (4), (5);").await;
            let m = fx.metadata.clone();
            *invoker.hook.lock().unwrap() = Some(Box::new(move || {
                let m = m.clone();
                Box::pin(async move {
                    let _ = m.delete_trigger(main, f, u).await;
                })
            }));
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert_eq!(invoker.seen().len(), 3, "one more, then none after the delete");
            assert_eq!(fx.outbox(main).await, 0);
        })
        .await;
    }

    /// m13b (B3): the dispatcher stops on a branch that stopped being ready (here, a reset
    /// began): the pass says stop and no session opens.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_stops_when_the_branch_isnt_ready() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1);").await;
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            fx.claim(main).await;
            assert_eq!(d.pass(&mut s).await.unwrap(), Pass::Stop);
            assert!(d.open(main, &fx.database).await.unwrap().is_none());
            assert!(invoker.seen().is_empty());
            assert!(
                fx.metadata.set_branch_status(main, &[BranchStatus::Resetting], BranchStatus::Ready).await.unwrap()
            );
        })
        .await;
    }

    /// m13b (B3, fix N3): the dispatcher stops on a deleted (tombstoned) branch: its pass says
    /// stop, no session opens, and nothing of it is delivered.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_stops_on_branch_delete() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let dev = fx.create("dev").await;
            assert!(fx.triggers(dev).await.iter().all(|t| t.state == TriggerState::Installed), "the child's copy is installed");
            fx.sql(dev, "INSERT INTO public.orders VALUES (1);").await;
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = d.open(dev, &fx.database).await.unwrap().expect("the child dispatches");
            fx.metadata.mark_branch_deleted(dev).await.unwrap();
            assert_eq!(fx.branch(dev).await.status, BranchStatus::Deleted);
            assert_eq!(d.pass(&mut s).await.unwrap(), Pass::Stop);
            assert!(d.open(dev, &fx.database).await.unwrap().is_none());
            assert!(invoker.seen().is_empty());
        })
        .await;
    }

    /// m13b fix (B1): neither NOTIFY spam on an empty outbox nor a due row locked by another
    /// session makes the dispatcher spin: passes stay bounded per second.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_does_not_spin() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            async fn passes_for(d: &Disp, s: &mut Session, span: Duration) -> usize {
                let (start, mut n) = (Instant::now(), 0);
                while start.elapsed() < span {
                    n += 1;
                    if let Pass::Idle(idle) = d.pass(s).await.unwrap() {
                        d.wait(s, idle.min(span.saturating_sub(start.elapsed()))).await.unwrap();
                    }
                }
                n
            }
            let spam = tokio::spawn(async move {
                fx.on(main, "DO $$ BEGIN FOR i IN 1..1500 LOOP PERFORM pg_notify('chelabase_functions', i::text); COMMIT; PERFORM pg_sleep(0.002); END LOOP; END $$;").await
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            let spammed = passes_for(&d, &mut s, Duration::from_secs(2)).await;
            spam.abort();
            assert!(spammed <= 40, "{spammed} passes in 2 s from NOTIFY spam alone");

            let lock = tokio::spawn(async move {
                fx.on(main, "INSERT INTO public.orders VALUES (1); BEGIN; SELECT id FROM chelabase_functions.outbox FOR UPDATE; SELECT pg_sleep(6); COMMIT;").await
            });
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let locked = passes_for(&d, &mut s, Duration::from_secs(2)).await;
            assert!(locked <= 40, "{locked} passes in 2 s around a locked due row");
            let _ = lock.await;
            drive(&d, &mut s, 4).await;
            assert_eq!(invoker.seen().len(), 1, "delivered once the lock went");
        })
        .await;
    }

    /// m13b fix (S1): a metadata error while re-checking a later row settles the rows already
    /// delivered (deleted, never delivered again) and releases the rest.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn mid_batch_metadata_error_settles_delivered_rows() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2), (3);").await;
            let invoker = Scripted::new(&[]);
            let (d, meta) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            let down = meta.clone();
            *invoker.hook.lock().unwrap() = Some(Box::new(move || {
                down.down.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async {})
            }));
            let out = d.pass(&mut s).await;
            assert_eq!(out, Err("metadata down".to_string()));
            assert_eq!(invoker.seen().len(), 1);
            assert_eq!(
                fx.value(main, "SELECT count(*)::text || ' ' || string_agg(attempts::text, ',' ORDER BY id) FROM chelabase_functions.outbox").await,
                "2 0,0",
                "the delivered row settled, the others released"
            );
        })
        .await;
    }

    /// m13b fix (S2): a disabled trigger's queued events count in `dropped_count` when the
    /// worker's sync removes them (no dispatcher running).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn worker_counts_a_disabled_triggers_queued_events() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2);").await;
            fx.metadata.update_trigger(main, f, t, false).await.unwrap();
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(fx.outbox(main).await, 0);
            assert_eq!(trigger_row(fx, t).await.dropped_count, 2);
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(trigger_row(fx, t).await.dropped_count, 2, "counted once");
        })
        .await;
    }

    /// m13b (B3): the tenth failure leaves the event dead, counted in the API's `dead_count`
    /// with its error, and never delivered again.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn tenth_failure_is_dead_and_counted() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1); UPDATE chelabase_functions.outbox SET attempts = 9;").await;
            let invoker = Scripted::new(&[500]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert_eq!(
                fx.value(main, "SELECT status || ':' || attempts FROM chelabase_functions.outbox").await,
                "dead:10"
            );
            let row = trigger_row(fx, t).await;
            assert_eq!((row.dead_count, row.last_delivery_error.as_deref()), (1, Some("the function answered 500")));
            d.pass(&mut s).await.unwrap();
            assert_eq!(invoker.seen().len(), 1, "a dead event is never delivered again");
        })
        .await;
    }

    /// m13 (S2, S3): a deleted trigger, and a deleted function's triggers, are dropped from
    /// the database by the sweep even with no trigger left on the branch; the dispatcher stops.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn deleted_triggers_and_functions_are_dropped() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            let g = fx.function("other").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            let u = fx.trigger(main, g, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t, u]));
            assert!(fx.metadata.delete_trigger(main, f, t).await.unwrap());
            fx.sql(main, "INSERT INTO public.orders VALUES (1);").await;
            let invoker = Scripted::new(&[]);
            let (d, _) = dispatcher(fx, invoker.clone(), &[f]).await;
            // The deleted trigger's row is unknown and deleted; the disabled function's held.
            let mut s = session(fx, &d).await;
            d.pass(&mut s).await.unwrap();
            assert_eq!(fx.outbox(main).await, 1, "only the held row stays");
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[u]));
            assert!(fx.metadata.delete_function(fx.id, g).await.unwrap());
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(fx.db_triggers(main).await, "", "dropped with no trigger left");
            assert!(fx.metadata.trigger_drops(main).await.unwrap().is_empty(), "then forgotten");
            assert!(invoker.seen().is_empty());
            assert_eq!(d.pass(&mut s).await, Ok(Pass::Stop), "no trigger left: the dispatcher stops");
        })
        .await;
    }

    // m17: the read rule. A table trigger is installed, kept and fed only while its
    // function's approved run-as OID exists, has table-level SELECT and is outside the
    // table's row level security (BYPASSRLS, or the owner without FORCE).

    const CANNOT_READ: &str = "run_as_role_cannot_read_table";
    const SUBJECT_TO_RLS: &str = "run_as_role_subject_to_rls";

    /// `(state, last_error)` of trigger `id` on main.
    async fn verdict(fx: &Fx, id: Uuid) -> (TriggerState, Option<String>) {
        let row = trigger_row(fx, id).await;
        (row.state, row.last_error)
    }

    fn failed(why: &str) -> (TriggerState, Option<String>) {
        (TriggerState::Failed, Some(why.to_string()))
    }

    const INSTALLED: (TriggerState, Option<String>) = (TriggerState::Installed, None);

    /// Outbox rows queued for trigger `id` on main.
    async fn queued(fx: &Fx, id: Uuid) -> i64 {
        fx.value(fx.main, &format!("SELECT count(*) FROM chelabase_functions.outbox WHERE trigger_id = '{id}'"))
            .await
            .parse()
            .unwrap()
    }

    /// m17 AC 1, 2, 3, 7: a run-as role with table-level SELECT gets its trigger installed and
    /// delivered; a column grant or no grant fails `run_as_role_cannot_read_table` with no
    /// database trigger and nothing queued; a function without a run-as role fails
    /// `run_as_role_required`.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn install_requires_run_as_select() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.granted (id int, note text); CREATE TABLE public.columns (id int, note text);
                          CREATE TABLE public.ungranted (id int);
                          CREATE ROLE reader NOLOGIN; GRANT SELECT ON public.granted TO reader; GRANT SELECT (id) ON public.columns TO reader;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let none = fx.function_without_run_as("nobody").await;
            let granted = fx.trigger(main, f, "granted", &["insert"]).await;
            let columns = fx.trigger(main, f, "columns", &["insert"]).await;
            let ungranted = fx.trigger(main, f, "ungranted", &["insert"]).await;
            let unset = fx.trigger(main, none, "granted", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, granted).await, INSTALLED);
            assert_eq!(verdict(fx, columns).await, failed(CANNOT_READ));
            assert_eq!(verdict(fx, ungranted).await, failed(CANNOT_READ));
            assert_eq!(verdict(fx, unset).await, failed("run_as_role_required"));
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[granted]));
            fx.sql(main, "INSERT INTO public.granted VALUES (1, 'a'); INSERT INTO public.columns VALUES (1, 'b'); INSERT INTO public.ungranted VALUES (1);").await;
            assert_eq!((queued(fx, granted).await, fx.outbox(main).await), (1, 1), "only the readable table's row");
        })
        .await;
    }

    /// m17 AC 4: a revoked SELECT drops the installed trigger at the next sync, the row fails
    /// `run_as_role_cannot_read_table`, and the events queued before are removed and counted.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn revoked_select_drops_the_installed_trigger() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE reader NOLOGIN; GRANT SELECT ON public.orders TO reader;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, INSTALLED);
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2)").await;
            assert_eq!(queued(fx, t).await, 2);
            fx.sql(main, "REVOKE SELECT ON public.orders FROM reader").await;
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, t).await, failed(CANNOT_READ));
            assert_eq!(fx.db_triggers(main).await, "");
            assert_eq!(queued(fx, t).await, 0);
            assert_eq!(trigger_row(fx, t).await.dropped_count, 2);
        })
        .await;
    }

    /// m17 AC 14: a revoked SELECT stops capture at once, before any sync runs.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn capture_stops_when_select_is_revoked_before_the_next_sync() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE reader NOLOGIN; GRANT SELECT ON public.orders TO reader;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let t = fx.trigger(main, f, "orders", &["insert", "update", "delete"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            assert_eq!(queued(fx, t).await, 1);
            fx.sql(main, "REVOKE SELECT ON public.orders FROM reader").await;
            fx.sql(main, "INSERT INTO public.orders VALUES (2); UPDATE public.orders SET id = 3 WHERE id = 1; DELETE FROM public.orders WHERE id = 2;").await;
            assert_eq!(queued(fx, t).await, 1, "captured after the revoke");
            // Only a column grant back: still not table-level SELECT.
            fx.sql(main, "GRANT SELECT (id) ON public.orders TO reader; INSERT INTO public.orders VALUES (4);").await;
            assert_eq!(queued(fx, t).await, 1, "captured with a column grant");
        })
        .await;
    }

    /// m17 AC 14: becoming subject to the table's RLS stops capture at once, before any sync:
    /// RLS enabled (a reader), RLS forced (the owner), BYPASSRLS removed, ownership changed.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn capture_stops_when_rls_is_enabled_before_the_next_sync() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE reader NOLOGIN; CREATE ROLE owner_role NOLOGIN; CREATE ROLE bypasser NOLOGIN BYPASSRLS; CREATE ROLE mover NOLOGIN;
                          CREATE TABLE public.enabled (id int); GRANT SELECT ON public.enabled TO reader;
                          CREATE TABLE public.forced (id int); ALTER TABLE public.forced OWNER TO owner_role;
                          CREATE TABLE public.bypassed (id int); GRANT SELECT ON public.bypassed TO bypasser; ALTER TABLE public.bypassed ENABLE ROW LEVEL SECURITY;
                          CREATE TABLE public.moved (id int); ALTER TABLE public.moved OWNER TO mover; ALTER TABLE public.moved ENABLE ROW LEVEL SECURITY;").await;
            let mut ts = Vec::new();
            for (name, role, table) in [("a", "reader", "enabled"), ("b", "owner_role", "forced"), ("c", "bypasser", "bypassed"), ("d", "mover", "moved")] {
                let f = fx.function_without_run_as(name).await;
                fx.set_run_as(main, f, role).await;
                ts.push((table, fx.trigger(main, f, table, &["insert"]).await));
            }
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            for (table, t) in &ts {
                assert_eq!(verdict(fx, *t).await, INSTALLED, "{table}");
                fx.sql(main, &format!("INSERT INTO public.{table} VALUES (1)")).await;
                assert_eq!(queued(fx, *t).await, 1, "{table}");
            }
            fx.sql(main, "ALTER TABLE public.enabled ENABLE ROW LEVEL SECURITY;
                          ALTER TABLE public.forced ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;
                          ALTER ROLE bypasser NOBYPASSRLS;
                          ALTER TABLE public.moved OWNER TO postgres; GRANT SELECT ON public.moved TO mover;").await;
            for (table, t) in &ts {
                fx.sql(main, &format!("INSERT INTO public.{table} VALUES (2)")).await;
                assert_eq!(queued(fx, *t).await, 1, "{table}: captured while subject to RLS");
            }
            fx.worker.sync_pending_fn_triggers().await;
            for (table, t) in &ts {
                assert_eq!(verdict(fx, *t).await, failed(SUBJECT_TO_RLS), "{table}");
            }
            assert_eq!(fx.db_triggers(main).await, "");
        })
        .await;
    }

    /// m17 AC 5: a run-as role dropped and re-made under the same name (with SELECT) is not
    /// the approved OID: capture stops, and the next sync drops the trigger and fails it.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn recreated_run_as_role_is_not_trusted() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE reader NOLOGIN; GRANT SELECT ON public.orders TO reader;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, INSTALLED);
            let approved = fx.role_oid(main, "reader").await;
            fx.sql(main, "REVOKE SELECT ON public.orders FROM reader; DROP ROLE reader; CREATE ROLE reader NOLOGIN; GRANT SELECT ON public.orders TO reader;").await;
            assert_ne!(fx.role_oid(main, "reader").await, approved);
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            assert_eq!(queued(fx, t).await, 0, "captured for a namesake");
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, t).await, failed(CANNOT_READ));
            assert_eq!(fx.db_triggers(main).await, "");
            // A fresh trigger of the same function: never installed for the namesake either.
            let u = fx.trigger(main, f, "orders", &["delete"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, u).await, failed(CANNOT_READ));
            assert_eq!(fx.db_triggers(main).await, "");
        })
        .await;
    }

    /// m17 AC 6: a failed trigger stays failed after the grant is fixed, and installs once it
    /// is turned off and on.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn failed_trigger_installs_after_reenable() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE reader NOLOGIN;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, failed(CANNOT_READ));
            fx.sql(main, "GRANT SELECT ON public.orders TO reader").await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, failed(CANNOT_READ), "failed is terminal");
            fx.metadata.update_trigger(main, f, t, false).await.unwrap();
            fx.metadata.update_trigger(main, f, t, true).await.unwrap();
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, t).await, INSTALLED);
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t]));
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            assert_eq!(queued(fx, t).await, 1);
        })
        .await;
    }

    /// m17 AC 8, 9, 12: on a table with RLS enabled, a reader without BYPASSRLS that doesn't
    /// own it fails `run_as_role_subject_to_rls`; a BYPASSRLS reader, the direct owner and an
    /// owner through an inherited membership install and deliver every row; a role without
    /// SELECT reports the grant first. A NOINHERIT membership in the owner, or BYPASSRLS on a
    /// group, puts nobody outside RLS (Postgres inherits neither).
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn rls_table_needs_bypass_or_ownership() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE plain_reader NOLOGIN; CREATE ROLE bypasser NOLOGIN BYPASSRLS; CREATE ROLE owner_role NOLOGIN;
                          CREATE ROLE team NOLOGIN; CREATE ROLE member NOLOGIN; GRANT team TO member WITH INHERIT TRUE;
                          CREATE ROLE outsider NOLOGIN; GRANT team TO outsider WITH INHERIT FALSE;
                          CREATE ROLE bypass_group NOLOGIN BYPASSRLS; CREATE ROLE grouped NOLOGIN; GRANT bypass_group TO grouped WITH INHERIT TRUE;
                          CREATE ROLE nogrant NOLOGIN;
                          CREATE TABLE public.guarded (id int); ALTER TABLE public.guarded ENABLE ROW LEVEL SECURITY;
                          GRANT SELECT ON public.guarded TO plain_reader, bypasser, outsider, grouped;
                          CREATE TABLE public.owned (id int); ALTER TABLE public.owned OWNER TO owner_role; ALTER TABLE public.owned ENABLE ROW LEVEL SECURITY;
                          CREATE TABLE public.team_owned (id int); ALTER TABLE public.team_owned OWNER TO team; ALTER TABLE public.team_owned ENABLE ROW LEVEL SECURITY;
                          GRANT SELECT ON public.team_owned TO outsider;").await;
            let cases = [
                ("plain_reader", "guarded", failed(SUBJECT_TO_RLS)),
                ("bypasser", "guarded", INSTALLED),
                ("owner_role", "owned", INSTALLED),
                ("member", "team_owned", INSTALLED),
                ("outsider", "team_owned", failed(SUBJECT_TO_RLS)),
                ("grouped", "guarded", failed(SUBJECT_TO_RLS)),
                ("nogrant", "guarded", failed(CANNOT_READ)),
            ];
            let mut ts = Vec::new();
            for (role, table, _) in &cases {
                let f = fx.function_without_run_as(&role.replace('_', "-")).await;
                fx.set_run_as(main, f, role).await;
                ts.push(fx.trigger(main, f, table, &["insert"]).await);
            }
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            for ((role, table, expected), t) in cases.iter().zip(&ts) {
                assert_eq!(&verdict(fx, *t).await, expected, "{role} on {table}");
            }
            fx.sql(main, "INSERT INTO public.guarded VALUES (1); INSERT INTO public.owned VALUES (1); INSERT INTO public.team_owned VALUES (1);").await;
            for ((role, table, expected), t) in cases.iter().zip(&ts) {
                let n = if *expected == INSTALLED { 1 } else { 0 };
                assert_eq!(queued(fx, *t).await, n, "{role} on {table}");
            }
        })
        .await;
    }

    /// m17 AC 10: under FORCE ROW LEVEL SECURITY the owner without BYPASSRLS is subject to RLS.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn forced_rls_refuses_the_owner() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE owner_role NOLOGIN; CREATE TABLE public.forced (id int); ALTER TABLE public.forced OWNER TO owner_role;
                          ALTER TABLE public.forced ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "owner_role").await;
            let t = fx.trigger(main, f, "forced", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, failed(SUBJECT_TO_RLS));
            assert_eq!(fx.db_triggers(main).await, "");
        })
        .await;
    }

    /// m17 AC 11: RLS enabled on a reader's table (or forced on an owner's) drops the
    /// installed trigger at the next sync; its queued events are removed and counted.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn enabling_rls_drops_the_installed_trigger() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE reader NOLOGIN; CREATE ROLE owner_role NOLOGIN;
                          CREATE TABLE public.orders (id int); GRANT SELECT ON public.orders TO reader;
                          CREATE TABLE public.owned (id int); ALTER TABLE public.owned OWNER TO owner_role; ALTER TABLE public.owned ENABLE ROW LEVEL SECURITY;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let g = fx.function_without_run_as("owner").await;
            fx.set_run_as(main, g, "owner_role").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            let u = fx.trigger(main, g, "owned", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!((verdict(fx, t).await, verdict(fx, u).await), (INSTALLED, INSTALLED));
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2); INSERT INTO public.owned VALUES (1);").await;
            assert_eq!((queued(fx, t).await, queued(fx, u).await), (2, 1));
            fx.sql(main, "ALTER TABLE public.orders ENABLE ROW LEVEL SECURITY; ALTER TABLE public.owned FORCE ROW LEVEL SECURITY;").await;
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!((verdict(fx, t).await, verdict(fx, u).await), (failed(SUBJECT_TO_RLS), failed(SUBJECT_TO_RLS)));
            assert_eq!(fx.db_triggers(main).await, "");
            assert_eq!((queued(fx, t).await, queued(fx, u).await), (0, 0));
            assert_eq!((trigger_row(fx, t).await.dropped_count, trigger_row(fx, u).await.dropped_count), (2, 1));
        })
        .await;
    }

    /// m17 AC 15: events queued before the upgrade for a trigger that fails its first sync
    /// after it (no run-as role, no SELECT, subject to RLS) are removed and counted, never
    /// left for delivery. The pre-upgrade rows are written straight into the outbox.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn failed_at_first_sync_drops_events_queued_before() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE TABLE public.guarded (id int); ALTER TABLE public.guarded ENABLE ROW LEVEL SECURITY;
                          CREATE ROLE nogrant NOLOGIN; CREATE ROLE plain_reader NOLOGIN; GRANT SELECT ON public.guarded TO plain_reader;").await;
            // An installed trigger, so the outbox exists.
            let good_f = fx.function("good").await;
            let good = fx.trigger(main, good_f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, good).await, INSTALLED);
            let unset_f = fx.function_without_run_as("unset").await;
            let nogrant_f = fx.function_without_run_as("nogrant").await;
            fx.set_run_as(main, nogrant_f, "nogrant").await;
            let rls_f = fx.function_without_run_as("rls").await;
            fx.set_run_as(main, rls_f, "plain_reader").await;
            let unset = fx.trigger(main, unset_f, "orders", &["insert"]).await;
            let nogrant = fx.trigger(main, nogrant_f, "orders", &["insert"]).await;
            let rls = fx.trigger(main, rls_f, "guarded", &["insert"]).await;
            let forged = |id: Uuid, n: i64| format!(
                "INSERT INTO chelabase_functions.outbox (trigger_id, payload, timeline_id)
                 SELECT '{id}', '{{}}'::jsonb, current_setting('neon.timeline_id', true) FROM generate_series(1, {n});"
            );
            fx.sql(main, &format!("{}{}{}", forged(unset, 1), forged(nogrant, 2), forged(rls, 3))).await;
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, unset).await, failed("run_as_role_required"));
            assert_eq!(verdict(fx, nogrant).await, failed(CANNOT_READ));
            assert_eq!(verdict(fx, rls).await, failed(SUBJECT_TO_RLS));
            for (t, n) in [(unset, 1), (nogrant, 2), (rls, 3)] {
                assert_eq!(queued(fx, t).await, 0, "{t}");
                assert_eq!(trigger_row(fx, t).await.dropped_count, n, "{t}");
            }
            assert_eq!(queued(fx, good).await, 1, "the installed trigger's event stays");
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[good]));
        })
        .await;
    }

    /// m17 fix 1 (S1): table-level SELECT without USAGE on the table's schema doesn't read the
    /// table. Such a trigger settles `run_as_role_cannot_read_table` with no database trigger
    /// and nothing queued; revoking the schema's USAGE after the install stops capture at once
    /// and drops the trigger at the next sync, removing and counting its events.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn schema_usage_is_required_to_read_the_table() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE SCHEMA closed; CREATE TABLE closed.orders (id int);
                          CREATE SCHEMA opened; CREATE TABLE opened.orders (id int);
                          CREATE ROLE reader NOLOGIN; GRANT SELECT ON closed.orders, opened.orders TO reader;
                          GRANT USAGE ON SCHEMA opened TO reader;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "reader").await;
            let in_schema = |schema: &'static str| async move {
                fx.metadata
                    .add_trigger(&NewTrigger {
                        branch_id: main,
                        function_id: f,
                        source: TriggerSource::Table,
                        pg_database: &fx.database,
                        schema_name: Some(schema),
                        table_name: Some("orders"),
                        bucket: None,
                        prefix: None,
                        events: &["insert"],
                    })
                    .await
                    .unwrap()
            };
            let closed = in_schema("closed").await;
            let opened = in_schema("opened").await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert!(fx.on(main, "SET ROLE reader; SELECT count(*) FROM closed.orders;").await.is_err(), "the role can't read the table itself");
            assert_eq!(verdict(fx, closed).await, failed(CANNOT_READ));
            assert_eq!(verdict(fx, opened).await, INSTALLED);
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[opened]));
            fx.sql(main, "INSERT INTO closed.orders VALUES (1); INSERT INTO opened.orders VALUES (1);").await;
            assert_eq!((queued(fx, closed).await, queued(fx, opened).await), (0, 1));
            fx.sql(main, "REVOKE USAGE ON SCHEMA opened FROM reader; INSERT INTO opened.orders VALUES (2);").await;
            assert_eq!(queued(fx, opened).await, 1, "captured after the schema's USAGE was revoked");
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, opened).await, failed(CANNOT_READ));
            assert_eq!(fx.db_triggers(main).await, "");
            assert_eq!(queued(fx, opened).await, 0);
            assert_eq!(trigger_row(fx, opened).await.dropped_count, 1);
        })
        .await;
    }

    /// m17 fix 1 (S3): a trigger the database no longer wants (here: its function's run-as
    /// role cleared) settles `failed` only after a pass whose script ran in its database. While
    /// a tenant's lock makes that database's sync yield, the row keeps its state; the next
    /// sweep drops the database trigger, removes and counts its events, and only then settles
    /// `run_as_role_required`.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn unwanted_trigger_settles_only_after_its_database_trigger_is_dropped() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE ra NOLOGIN; GRANT SELECT ON public.orders TO ra;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "ra").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, INSTALLED);
            assert!(fx.metadata.set_run_as(main, f, None).await.unwrap());
            // A tenant transaction that wrote to the triggered table and is still open (8 s).
            let long = tokio::spawn(async move {
                fx.on(main, "BEGIN; INSERT INTO public.orders VALUES (1); SELECT pg_sleep(8); COMMIT;").await
            });
            tokio::time::sleep(Duration::from_secs(1)).await;
            let yielded = fx.worker.sync_fn_triggers(main, false).await;
            let during = verdict(fx, t).await;
            long.await.unwrap().unwrap();
            assert_eq!(yielded, Ok(false), "the sync yields to the tenant's lock");
            // #301: the run-as change pended it (held, never delivered); not settled `failed`
            // while its database trigger was still in place.
            assert_eq!(during, (TriggerState::PendingInstall, None), "settled while its database trigger was still in place");
            assert_eq!(queued(fx, t).await, 1);
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, t).await, failed("run_as_role_required"));
            assert_eq!(fx.db_triggers(main).await, "");
            assert_eq!(queued(fx, t).await, 0);
            assert_eq!(trigger_row(fx, t).await.dropped_count, 1);
        })
        .await;
    }

    /// #301: the run-as OID trigger `id`'s database trigger records (its second argument).
    async fn db_trigger_run_as(fx: &Fx, id: Uuid) -> String {
        fx.value(fx.main, &format!(
            "SELECT convert_from(rtrim(substring(tgargs FROM position('\\x00'::bytea IN tgargs) + 1), '\\x00'::bytea), 'UTF8')
             FROM pg_trigger WHERE tgname = '{}'", trigger_name(id)
        ))
        .await
    }

    /// #301 AC 1: events captured under run-as role A are never delivered once the role is
    /// changed to B. The change pends the trigger at once (the dispatcher holds a pending
    /// trigger's events), and the next sync replaces the database trigger with B's, removing
    /// and counting every event queued before, then captures under B. The same holds when the
    /// old database trigger was gone by the time of the sync (dropped by the table's owner
    /// after it queued), so a row that can't be told apart by its trigger still goes.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn a_run_as_change_stops_delivery_of_rows_captured_before() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE TABLE public.items (id int);
                          CREATE ROLE ra NOLOGIN; CREATE ROLE rb NOLOGIN;
                          GRANT SELECT ON public.orders, public.items TO ra, rb;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "ra").await;
            let kept = fx.trigger(main, f, "orders", &["insert"]).await;
            let gone = fx.trigger(main, f, "items", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!((verdict(fx, kept).await, verdict(fx, gone).await), (INSTALLED, INSTALLED));
            let (ra, rb) = (fx.role_oid(main, "ra").await, fx.role_oid(main, "rb").await);
            assert_eq!(db_trigger_run_as(fx, kept).await, ra.to_string());
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2); INSERT INTO public.items VALUES (1);").await;
            assert_eq!((queued(fx, kept).await, queued(fx, gone).await), (2, 1));
            fx.sql(main, &format!("DROP TRIGGER {} ON public.items", trigger_name(gone))).await;
            fx.set_run_as(main, f, "rb").await;
            // Held from the change on: the dispatcher delivers only installed triggers.
            for t in [kept, gone] {
                assert_eq!(verdict(fx, t).await, (TriggerState::PendingInstall, None));
            }
            fx.sql(main, "INSERT INTO public.orders VALUES (3)").await;
            assert_eq!(queued(fx, kept).await, 3, "still captured under ra until the sync");
            fx.worker.sync_pending_fn_triggers().await;
            for (t, n) in [(kept, 3), (gone, 1)] {
                assert_eq!(verdict(fx, t).await, INSTALLED, "{t}");
                assert_eq!(queued(fx, t).await, 0, "nothing captured under ra stays: {t}");
                assert_eq!(trigger_row(fx, t).await.dropped_count, n, "{t}");
                assert_eq!(db_trigger_run_as(fx, t).await, rb.to_string(), "{t}");
            }
            fx.sql(main, "INSERT INTO public.orders VALUES (4); INSERT INTO public.items VALUES (2);").await;
            assert_eq!((queued(fx, kept).await, queued(fx, gone).await), (1, 1), "captured under rb");
            // A later sync with no change keeps what rb captured.
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!((queued(fx, kept).await, queued(fx, gone).await), (1, 1));
        })
        .await;
    }

    /// #301 AC 1 (cleared): clearing the run-as role pends the trigger at once, and the next
    /// sync drops its database trigger, removes and counts its queued events and settles it
    /// `run_as_role_required`.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn clearing_run_as_settles_its_triggers() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE ra NOLOGIN; GRANT SELECT ON public.orders TO ra;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "ra").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "INSERT INTO public.orders VALUES (1), (2)").await;
            assert_eq!(queued(fx, t).await, 2);
            assert!(fx.metadata.set_run_as(main, f, None).await.unwrap());
            assert_eq!(verdict(fx, t).await, (TriggerState::PendingInstall, None), "held at once");
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, t).await, failed("run_as_role_required"));
            assert_eq!(fx.db_triggers(main).await, "");
            assert_eq!(queued(fx, t).await, 0);
            assert_eq!(trigger_row(fx, t).await.dropped_count, 2);
        })
        .await;
    }

    /// #301 AC 2: a trigger settled `failed` because its run-as role couldn't read the table
    /// is checked again when the run-as role changes: set to a role that can, the next sweep
    /// installs it.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn a_failed_trigger_is_retried_after_a_run_as_change() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int); CREATE ROLE nogrant NOLOGIN; CREATE ROLE reader NOLOGIN;
                          GRANT SELECT ON public.orders TO reader;").await;
            let f = fx.function_without_run_as("hello").await;
            fx.set_run_as(main, f, "nogrant").await;
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(verdict(fx, t).await, failed(CANNOT_READ));
            fx.set_run_as(main, f, "reader").await;
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(verdict(fx, t).await, INSTALLED);
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t]));
            fx.sql(main, "INSERT INTO public.orders VALUES (1)").await;
            assert_eq!(queued(fx, t).await, 1);
        })
        .await;
    }

    /// #301: the database trigger's run-as OID for trigger `id` now (`None`: no database
    /// trigger), then `INSERT INTO public.<table> VALUES (<row>)`: what the event was captured
    /// under.
    async fn capture(fx: &Fx, table: &str, id: Uuid, row: i64) -> Option<i64> {
        let oid = db_trigger_run_as(fx, id).await.parse().ok();
        fx.sql(fx.main, &format!("INSERT INTO public.{table} VALUES ({row})")).await;
        oid
    }

    /// #301: the rows of trigger `id`'s queued events, in queue order.
    async fn queued_rows(fx: &Fx, id: Uuid) -> Vec<i64> {
        fx.value(fx.main, &format!(
            "SELECT coalesce(string_agg(payload->'record'->>'id', ',' ORDER BY id), '')
             FROM chelabase_functions.outbox WHERE trigger_id = '{id}'"
        ))
        .await
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap())
        .collect()
    }

    /// #301 (audit S1, B1): the invariant "an `installed` table trigger's outbox holds only
    /// events captured under its function's current run-as OID", exhaustively over where a
    /// run-as change lands in one sync pass (before it, between its two metadata reads, or
    /// after its scripts and before its settle), what it changes to (the same role, another
    /// role that can read, one that can't, cleared) and the row's state when it lands
    /// (`installed`, or already pending after an earlier change). Each event is tagged with
    /// the run-as OID its database trigger recorded when it was captured. After two more
    /// passes the row is settled for the final role (installed, or failed with the right
    /// reason with nothing queued), and an event captured under that role then is queued.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn run_as_change_at_any_point_of_a_sync_keeps_the_invariant() {
        use worker::fn_triggers::SyncPoint;
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE ROLE ra NOLOGIN; CREATE ROLE rb NOLOGIN; CREATE ROLE rn NOLOGIN;").await;
            let mut oid = std::collections::HashMap::new();
            for role in ["ra", "rb", "rn"] {
                oid.insert(role, fx.role_oid(main, role).await);
            }
            let points = [None, Some(SyncPoint::AfterTriggersRead), Some(SyncPoint::BeforeSettle)];
            let changes = [Some("ra"), Some("rb"), Some("rn"), None];
            let mut failures = Vec::new();
            let mut i = 0;
            for pending in [false, true] {
                for point in points {
                    for change in changes {
                        i += 1;
                        let case = format!("pending {pending}, at {point:?}, to {change:?}");
                        let table = format!("ilv_{i}");
                        fx.sql(main, &format!("CREATE TABLE public.{table} (id int); GRANT SELECT ON public.{table} TO ra, rb;")).await;
                        let f = fx.function_without_run_as(&format!("ilv-{i}")).await;
                        fx.set_run_as(main, f, "ra").await;
                        let t = fx.trigger(main, f, &table, &["insert"]).await;
                        fx.worker.sync_fn_triggers(main, false).await.unwrap();
                        assert_eq!(verdict(fx, t).await, INSTALLED, "{case}");
                        let mut tags = std::collections::HashMap::new();
                        tags.insert(1, capture(fx, &table, t, 1).await);
                        if pending {
                            fx.set_run_as(main, f, "rb").await;
                            tags.insert(2, capture(fx, &table, t, 2).await);
                        }
                        let apply = move || async move {
                            match change {
                                Some(role) => fx.set_run_as(main, f, role).await,
                                None => assert!(fx.metadata.set_run_as(main, f, None).await.unwrap()),
                            }
                        };
                        match point {
                            None => {
                                apply().await;
                                fx.worker.sync_fn_triggers(main, false).await.unwrap();
                            }
                            Some(at) => {
                                let hook = move |p: SyncPoint| -> futures_util::future::BoxFuture<'static, ()> {
                                    Box::pin(async move {
                                        if p == at {
                                            apply().await;
                                        }
                                    })
                                };
                                fx.worker.sync_fn_triggers_at(main, false, &hook).await.unwrap();
                            }
                        }
                        tags.insert(3, capture(fx, &table, t, 3).await);
                        for _ in 0..2 {
                            fx.worker.sync_fn_triggers(main, false).await.unwrap();
                        }
                        tags.insert(4, capture(fx, &table, t, 4).await);
                        let (state, error) = verdict(fx, t).await;
                        let queued = queued_rows(fx, t).await;
                        let current = change.filter(|r| *r != "rn").map(|r| oid[r]);
                        let expected = match change {
                            Some("rn") => failed(CANNOT_READ),
                            None => failed("run_as_role_required"),
                            _ => INSTALLED,
                        };
                        if (state, error.clone()) != expected {
                            failures.push(format!("{case}: settled {state:?} {error:?}, expected {expected:?}"));
                        }
                        let foreign: Vec<i64> = queued.iter().copied().filter(|r| tags[r] != current || current.is_none()).collect();
                        if !foreign.is_empty() {
                            failures.push(format!("{case}: {state:?} with events captured under another role queued: {foreign:?} (tags {tags:?})"));
                        }
                        if state == TriggerState::Installed && !queued.contains(&4) {
                            failures.push(format!("{case}: installed but the event captured after the passes isn't queued: {queued:?}"));
                        }
                    }
                }
            }
            assert!(failures.is_empty(), "{failures:#?}");
        })
        .await;
    }

    /// m17: the read rule ([`run_as_reads`]) checked exhaustively against Postgres itself.
    /// For every combination of a role's grant (none, a column, the table, through an
    /// inherited or a NOINHERIT membership, membership in `pg_read_all_data`), its attributes
    /// (plain, its own BYPASSRLS, a group's BYPASSRLS, SUPERUSER, NOINHERIT), the table's owner
    /// (another role, the role, a group it inherits or doesn't) and the table's RLS (off,
    /// enabled, forced, each without policies so RLS hides every row; or enabled with a
    /// permissive policy passing one of the two rows) and the role's USAGE on the table's
    /// schema (granted, or not, m17 fix 1), the rule's
    /// `can_select` part equals "a `SELECT` of every column as the role is permitted" and the
    /// whole rule equals "that `SELECT` as the role sees every row". The probe reads the whole
    /// row (`count(t.*)`) of a two-column table, so a column grant isn't all columns. The
    /// fragment is evaluated with the caller's own `pg_roles r` in scope, so an alias capture
    /// would show. A dropped role's OID reads nothing
    /// and raises nothing.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn run_as_rule_model_agrees_with_postgres() {
        with_fx(|fx| async move {
            let main = fx.main;
            // #301: plus a member of `pg_read_all_data`, a superuser, a role with the
            // NOINHERIT attribute (its "group" memberships take the attribute's default) and a
            // table with a permissive policy that passes one of its two rows.
            let grants = ["none", "column", "table", "group", "noinherit_group", "read_all_data"];
            let bypasses = ["none", "own", "group", "superuser", "noinherit"];
            let owners = ["other", "self", "group", "noinherit_group"];
            let rls = ["off", "on", "forced", "policy"];
            let usages = ["usage", "no_usage"];
            let mut setup = String::from(
                "CREATE ROLE m_sel NOLOGIN; CREATE ROLE m_own NOLOGIN; CREATE ROLE m_bypass NOLOGIN BYPASSRLS; CREATE ROLE m_other NOLOGIN;\n",
            );
            let mut cases = Vec::new();
            for g in grants {
                for b in bypasses {
                    for o in owners {
                        for r in rls {
                            for u in usages {
                                let i = cases.len();
                                let (role, table) = (format!("m_r{i}"), format!("m_s{i}.m_t{i}"));
                                setup.push_str(&format!("CREATE ROLE {role} NOLOGIN{}; CREATE SCHEMA m_s{i}; CREATE TABLE {table} (id int, note text); INSERT INTO {table} VALUES (1, 'a'), (2, 'b');\n",
                                    match b { "own" => " BYPASSRLS", "superuser" => " SUPERUSER", "noinherit" => " NOINHERIT", _ => "" }));
                                // A NOINHERIT role's "group" memberships are granted without an
                                // INHERIT option, so the role attribute decides.
                                let inherit = if b == "noinherit" { "" } else { " WITH INHERIT TRUE" };
                                if u == "usage" {
                                    setup.push_str(&format!("GRANT USAGE ON SCHEMA m_s{i} TO {role};\n"));
                                }
                                setup.push_str(&match g {
                                    "column" => format!("GRANT SELECT (id) ON {table} TO {role};\n"),
                                    "table" => format!("GRANT SELECT ON {table} TO {role};\n"),
                                    "group" => format!("GRANT SELECT ON {table} TO m_sel; GRANT m_sel TO {role}{inherit};\n"),
                                    "noinherit_group" => format!("GRANT SELECT ON {table} TO m_sel; GRANT m_sel TO {role} WITH INHERIT FALSE;\n"),
                                    "read_all_data" => format!("GRANT pg_read_all_data TO {role}{inherit};\n"),
                                    _ => String::new(),
                                });
                                if b == "group" {
                                    setup.push_str(&format!("GRANT m_bypass TO {role} WITH INHERIT TRUE;\n"));
                                }
                                setup.push_str(&match o {
                                    "self" => format!("ALTER TABLE {table} OWNER TO {role};\n"),
                                    "group" => format!("ALTER TABLE {table} OWNER TO m_own; GRANT m_own TO {role}{inherit};\n"),
                                    "noinherit_group" => format!("ALTER TABLE {table} OWNER TO m_own; GRANT m_own TO {role} WITH INHERIT FALSE;\n"),
                                    _ => format!("ALTER TABLE {table} OWNER TO m_other;\n"),
                                });
                                match r {
                                    "on" => setup.push_str(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;\n")),
                                    "forced" => setup.push_str(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY, FORCE ROW LEVEL SECURITY;\n")),
                                    "policy" => setup.push_str(&format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY; CREATE POLICY m_p ON {table} USING (id = 1);\n")),
                                    _ => {}
                                }
                                cases.push(format!("{g}/{b}/{o}/{r}/{u}"));
                                // In batches, so no one statement list runs into psql's timeout.
                                if cases.len() % 120 == 0 {
                                    fx.sql(main, &std::mem::take(&mut setup)).await;
                                }
                            }
                        }
                    }
                }
            }
            if !setup.is_empty() {
                fx.sql(main, &setup).await;
            }
            let can_select = worker::fn_triggers::run_as_can_select("r.oid", "c");
            let reads = worker::fn_triggers::run_as_reads("r.oid", "c");
            // Postgres's own answer: the SELECT as the role is denied (-1), or how many rows it sees.
            let probe = format!(
                "CREATE TEMP TABLE m_seen (i int, n int);
                 DO $m$ DECLARE n int; BEGIN
                   FOR i IN 0..{last} LOOP
                     BEGIN
                       EXECUTE pg_catalog.format('SET LOCAL ROLE m_r%s', i);
                       EXECUTE pg_catalog.format('SELECT count(t.*) FROM m_s%s.m_t%s t', i, i) INTO n;
                       RESET ROLE;
                     EXCEPTION WHEN insufficient_privilege THEN n := -1;
                     END;
                     INSERT INTO m_seen VALUES (i, n);
                   END LOOP;
                 END $m$;
                 SELECT string_agg(s.i || ':' || s.n || ':' || ({can_select})::text || ':' || ({reads})::text, ',' ORDER BY s.i)
                 FROM m_seen s JOIN pg_catalog.pg_roles r ON r.rolname = 'm_r' || s.i
                   JOIN pg_catalog.pg_class c ON c.oid = ('m_s' || s.i || '.m_t' || s.i)::regclass;",
                last = cases.len() - 1
            );
            let out = fx.value(main, &probe).await;
            let rows: Vec<&str> = out.split(',').collect();
            assert_eq!(rows.len(), cases.len(), "{out}");
            let mut mismatches = Vec::new();
            let mut seen_kinds = std::collections::BTreeSet::new();
            for (row, case) in rows.iter().zip(&cases) {
                let parts: Vec<&str> = row.split(':').collect();
                let (n, sel, all): (i64, bool, bool) = (parts[1].parse().unwrap(), parts[2] == "true", parts[3] == "true");
                seen_kinds.insert(n);
                if sel != (n >= 0) || all != (n == 2) {
                    mismatches.push(format!("{case}: postgres {n}, can_select {sel}, reads {all}"));
                }
            }
            assert!(mismatches.is_empty(), "{mismatches:#?}");
            // The model's assumptions: every outcome happens (denied, every row hidden by RLS,
            // one row passed by the policy, all rows).
            assert_eq!(seen_kinds, [-1, 0, 1, 2].into_iter().collect(), "{out}");
            // A role OID that names no role: false, no error.
            let ghost = fx.value(main, &format!(
                "SELECT ({}) FROM pg_catalog.pg_class c WHERE c.oid = 'm_s0.m_t0'::regclass",
                worker::fn_triggers::run_as_reads("4294967000::pg_catalog.oid", "c")
            )).await;
            assert_eq!(ghost, "f");
        })
        .await;
    }
}
