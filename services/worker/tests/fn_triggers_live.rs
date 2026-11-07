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

    use chelabase_metadata::{
        BranchRecord, BranchStatus, CreateBranchOutcome, DEMO_TENANT_ID, DatabaseStatus,
        FunctionKind, MetadataClient, NewBranch, NewDeployment, NewTrigger, RunAs, TriggerRecord,
        TriggerSource, TriggerState,
    };
    use chelabase_nats::{BranchJob, PointInTime};
    use chelabase_neon::ComputeBackend;
    use chelabase_schema::catalog::{CATALOG_SQL, parse_catalog};
    use chelabase_schema::plan::admin_digest_sql;
    use chelabase_types::data_key::{self, DataKeyKind};
    use chelabase_types::{BranchId, DatabaseId, ProjectId};
    use uuid::Uuid;
    use worker::config::RealWorker;
    use worker::fn_triggers::{RUN_AS_ROLE_MISSING, trigger_name};

    use crate::common::{cleanup, setup};

    const QUOTED: &str = r#"Ord"er s"#;

    struct Fx {
        worker: RealWorker,
        metadata: MetadataClient,
        project: ProjectId,
        id: DatabaseId,
        main: BranchId,
        database: String,
    }

    impl Fx {
        async fn provision() -> Fx {
            let (worker, metadata, _config) = setup().await;
            // `set_run_as` seals the key, as the orchestrator's client does.
            let metadata = metadata.with_secrets_key(worker.key.clone());
            let id = DatabaseId::new();
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
            Fx {
                database: record.database_name.clone().expect("database name"),
                main: BranchId::from(id),
                worker,
                metadata,
                project,
                id,
            }
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
                .admin_sql(&ep, &self.database, sql)
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

        /// A function with a deployment, active on main.
        async fn function(&self, name: &str) -> Uuid {
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
                    compatibility_date: chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
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
                 CREATE ROLE reporter NOLOGIN; CREATE ROLE ghost NOLOGIN;",
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
            let noted = chrono::Utc::now();
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
}
