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
            // The worker migrates at start; this suite's worker is in-process, so it does too
            // (the strict migrator: a new migration is applied and recorded as the binary would).
            metadata.migrate().await.expect("metadata migrations");
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
    /// Audit B1: no column value is evaluated with a user's cast in `enqueue()`'s definer
    /// context. An enum with casts to json and text, a domain over it, a composite holding it
    /// and an array of it are written as their type output (strings); built-in types keep
    /// their JSON types. No hostile function runs, as anyone.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn audit_hostile_casts_never_run_in_enqueue() {
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
    async fn audit_named_grants_and_a_tampered_outbox_are_repaired() {
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
    async fn audit_sweep_after_a_failed_create_sync_drops_foreign_events() {
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
            let parent_wanted = worker::fn_triggers::wanted_in(&fx.triggers(main).await, &fx.database);
            let role: u32 = fx
                .value(dev, "SELECT oid FROM pg_roles WHERE rolname = 'chelabase_functions_owner'")
                .await
                .parse()
                .unwrap();
            fx.sql(dev, &worker::fn_triggers::sync_sql(&parent_wanted, false, Some(role)))
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
    async fn audit_child_run_as_key_retried_until_minted() {
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
        let moves: [(&str, At, bool, &str, &str, Expect); 14] = [
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
            for t in ["casts", "parts", "plain", "dropped", "disabled"] {
                ids.insert(t, shared.trigger(shared.main, f, t, &["insert"]).await);
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
    async fn audit_failed_row_never_installed() {
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
    async fn audit_refusal_after_install_drops_the_triggers() {
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
    async fn audit_a_grant_to_the_owner_role_is_revoked_not_refused() {
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
    async fn audit_member_cannot_escalate() {
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
    async fn audit_run_as_mint_never_overrides_a_concurrent_change() {
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
    async fn audit_child_run_as_key_needs_the_approved_oid() {
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
                    .pending_run_as_branches(1000)
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
    async fn audit_sweep_never_blocks_tenant_writes() {
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
    async fn audit_owner_role_never_backs_a_key() {
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
    async fn audit_unprivileged_role_cannot_disable_the_triggers() {
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
    async fn audit_squatted_advisory_lock_never_stalls_the_install() {
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

    /// m13: an invoker that records each event's body and answers the scripted statuses in
    /// turn (then 200).
    struct Scripted {
        statuses: std::sync::Mutex<Vec<u16>>,
        seen: std::sync::Mutex<Vec<serde_json::Value>>,
    }

    impl Scripted {
        fn new(statuses: &[u16]) -> std::sync::Arc<Scripted> {
            std::sync::Arc::new(Scripted {
                statuses: std::sync::Mutex::new(statuses.iter().rev().copied().collect()),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn seen(&self) -> Vec<serde_json::Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl worker::fn_schedules::Invoker for Scripted {
        async fn invoke(&self, _: BranchId, _: &str, _: serde_json::Value) -> Result<u16, String> {
            Err("not a scheduled run".into())
        }
        async fn deliver_event(&self, _: BranchId, _: &str, body: Vec<u8>) -> Result<u16, String> {
            self.seen.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
            Ok(self.statuses.lock().unwrap().pop().unwrap_or(200))
        }
    }

    /// m13 (#175): passes until the outbox is quiet (each pass waits up to 5 s inside the
    /// database for a due row; a retried row is due within its backoff).
    async fn dispatch(fx: &Fx, invoker: &Scripted, passes: usize) {
        for _ in 0..passes {
            fx.worker.dispatch_once(invoker, fx.main, &fx.database).await.unwrap();
        }
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
            assert!(fx.metadata.set_enabled(main, f, true).await.unwrap());
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            fx.sql(main, "BEGIN; INSERT INTO public.orders VALUES (9, 'no'); ROLLBACK; INSERT INTO public.orders VALUES (1, 'a');").await;
            let invoker = Scripted::new(&[]);
            dispatch(fx, &invoker, 1).await;
            let seen = invoker.seen();
            assert_eq!(seen.len(), 1, "{seen:?}");
            assert_eq!(seen[0]["type"], "INSERT");
            assert_eq!(seen[0]["record"], serde_json::json!({"id": 1, "note": "a"}));
            assert!(seen[0]["txid"].as_u64().is_some());
            assert_eq!(fx.outbox(main).await, 0);

            let failing = Scripted::new(&[500, 500, 500]);
            fx.sql(main, "INSERT INTO public.orders VALUES (2, 'b');").await;
            dispatch(fx, &failing, 6).await;
            assert_eq!(failing.seen().len(), 4, "three failures, then the 2xx");
            assert_eq!(fx.outbox(main).await, 0);
            let row = fx.triggers(main).await.into_iter().find(|r| r.id == t).unwrap();
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
            dispatch(fx, &quiet, 1).await;
            assert!(quiet.seen().is_empty(), "{:?}", quiet.seen());
            assert_eq!(fx.outbox(main).await, 0, "the forged rows were deleted");
        })
        .await;
    }

    /// m13 AC: the dispatcher's database role reads the outbox and no member table, and a
    /// disabled function's events queue until it is enabled again.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn dispatcher_role_reads_only_the_outbox_and_disabled_functions_queue() {
        with_fx(|fx| async move {
            let main = fx.main;
            fx.sql(main, "CREATE TABLE public.orders (id int PRIMARY KEY);").await;
            let f = fx.function("hello").await;
            fx.trigger(main, f, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            let denied = fx.on(main, &format!("BEGIN; SET LOCAL ROLE {OWNER}; SELECT count(*) FROM public.orders; COMMIT;")).await;
            assert!(denied.as_ref().is_err_and(|e| e.contains("permission denied")), "{denied:?}");
            let outbox = fx.on(main, &format!("BEGIN; SET LOCAL ROLE {OWNER}; SELECT count(*) FROM chelabase_functions.outbox; COMMIT;")).await;
            assert!(outbox.is_ok(), "{outbox:?}");

            // The function is disabled (a new deployment's state starts off): events queue.
            fx.sql(main, "INSERT INTO public.orders VALUES (1);").await;
            let invoker = Scripted::new(&[]);
            dispatch(fx, &invoker, 1).await;
            assert!(invoker.seen().is_empty());
            assert_eq!(fx.outbox(main).await, 1, "queued while the function is off");
            assert!(fx.metadata.set_enabled(main, f, true).await.unwrap());
            dispatch(fx, &invoker, 1).await;
            assert_eq!(invoker.seen().len(), 1, "delivered once it is on again");
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
            assert!(fx.metadata.set_enabled(main, f, true).await.unwrap());
            let t = fx.trigger(main, f, "orders", &["insert"]).await;
            let u = fx.trigger(main, g, "orders", &["insert"]).await;
            fx.worker.sync_fn_triggers(main, false).await.unwrap();
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[t, u]));
            assert!(fx.metadata.delete_trigger(main, f, t).await.unwrap());
            fx.sql(main, "INSERT INTO public.orders VALUES (1);").await;
            let invoker = Scripted::new(&[]);
            // The deleted trigger's row is unknown and deleted; the disabled function's held.
            dispatch(fx, &invoker, 1).await;
            assert_eq!(fx.outbox(main).await, 1, "only the held row stays");
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(trigger_names(&fx.db_triggers(main).await), names(&[u]));
            assert!(fx.metadata.delete_function(fx.id, g).await.unwrap());
            fx.worker.sync_pending_fn_triggers().await;
            assert_eq!(fx.db_triggers(main).await, "", "dropped with no trigger left");
            assert!(fx.metadata.trigger_drops(main).await.unwrap().is_empty(), "then forgotten");
            assert!(invoker.seen().is_empty());
            assert_eq!(
                fx.worker.dispatch_once(&*invoker, main, &fx.database).await,
                Ok(worker::fn_events::Pass::Stop),
                "no trigger left: the dispatcher stops"
            );
        })
        .await;
    }
}
