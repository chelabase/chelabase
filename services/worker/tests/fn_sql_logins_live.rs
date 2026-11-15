//! Pooled endpoints m13 against real computes: a function's SQL login holds exactly its
//! run-as role (one membership, inherit, no set, no admin; no member), is marked, has no
//! BYPASSRLS whatever the run-as role has, is answered by the proxy row; a squatted role under
//! its name is dropped and made again; a namesake run-as role is never granted.
//!
//! `shared-state`: provisions databases on the workbench stack, deleted afterwards pass or fail.
//!
//! ```text
//! just workbench test -p worker --test fn_sql_logins_live -- --run-ignored only
//! ```

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;

mod fn_sql_logins {
    use std::time::Duration;

    use chelabase_metadata::{
        BranchRecord, DEMO_TENANT_ID, DatabaseStatus, FunctionKind, MetadataClient, NewDeployment,
        RunAs,
    };
    use chelabase_neon::{ComputeBackend, SqlOpts};
    use chelabase_types::data_key::{self, DataKeyKind};
    use chelabase_types::role_name::function_login_name;
    use chelabase_types::{BranchId, DatabaseId, ProjectId};
    use uuid::Uuid;
    use worker::config::RealWorker;

    use crate::common::{cleanup, setup};

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
            let metadata = metadata.with_secrets_key(worker.key.clone());
            let id = DatabaseId::new();
            let name = format!("fs-{}", &id.to_string()[..8]);
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

        async fn ep(&self) -> String {
            self.branch(self.main).await.endpoint_id.expect("endpoint")
        }

        async fn sql(&self, sql: &str) -> String {
            self.worker
                .computes
                .admin_sql(&self.ep().await, &self.database, sql)
                .await
                .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
        }

        /// `sql` in a session of the login `user` (the compute trusts local connections).
        async fn sql_as(&self, user: &str, sql: &str) -> Result<String, String> {
            let opts = SqlOpts {
                user: Some(user.to_string()),
                ..SqlOpts::admin_default()
            };
            self.worker
                .computes
                .admin_sql_with(&self.ep().await, &self.database, sql, &opts)
                .await
                .map_err(|e| format!("{e:#}"))
        }

        async fn value(&self, sql: &str) -> String {
            self.sql(sql)
                .await
                .lines()
                .map(str::trim)
                .rfind(|l| !l.is_empty())
                .unwrap_or("")
                .to_string()
        }

        async fn oid(&self, role: &str) -> i64 {
            self.value(&format!(
                "SELECT oid FROM pg_roles WHERE rolname = '{role}'"
            ))
            .await
            .parse()
            .unwrap()
        }

        /// A function active on main, run as `role` (approved by its current OID).
        async fn function(&self, name: &str, role: &str) -> Uuid {
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
            let oid = self.oid(role).await;
            assert!(
                self.metadata
                    .set_run_as(self.main, f, Some(&run_as(role, oid)))
                    .await
                    .unwrap()
            );
            f
        }

        /// The login's membership rows as `member` (`role:inherit:set:admin`), and as `roleid`.
        async fn memberships(&self, login: &str) -> (String, String) {
            let member = self
                .value(&format!(
                    "SELECT coalesce(string_agg(g.rolname || ':' || a.inherit_option || ':' || a.set_option
                                                || ':' || a.admin_option, ',' ORDER BY g.rolname), '')
                     FROM pg_auth_members a JOIN pg_roles m ON m.oid = a.member
                     JOIN pg_roles g ON g.oid = a.roleid WHERE m.rolname = '{login}'"
                ))
                .await;
            let members = self
                .value(&format!(
                    "SELECT count(*) FROM pg_auth_members a JOIN pg_roles g ON g.oid = a.roleid
                     WHERE g.rolname = '{login}'"
                ))
                .await;
            (member, members)
        }
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

    /// AC 1, 4, 5: with SQL access never set, the reconcile makes the login with exactly the
    /// attributes, the marker and one membership (the run-as role: inherit, no set, no admin),
    /// no member; the proxy row answers its verifier; a second pass changes nothing.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn fn_sql_logins_membership_invariant() {
        with_fx(|fx| async move {
            fx.sql("CREATE ROLE reporting NOLOGIN").await;
            let f = fx.function("hello", "reporting").await;
            let login = function_login_name(f);
            fx.worker.sync_sql_logins(fx.main).await.unwrap();
            let attrs = fx
                .value(&format!(
                    "SELECT rolcanlogin || ',' || rolinherit || ',' || rolsuper || ',' || rolcreatedb || ','
                            || rolcreaterole || ',' || rolreplication || ',' || rolbypassrls || ','
                            || rolconnlimit FROM pg_roles WHERE rolname = '{login}'"
                ))
                .await;
            assert_eq!(attrs, "true,false,false,false,false,false,false,10");
            let settings = fx
                .value(&format!(
                    "SELECT array_to_string(setconfig, ';') FROM pg_db_role_setting s
                     JOIN pg_roles r ON r.oid = s.setrole WHERE r.rolname = '{login}'"
                ))
                .await;
            assert_eq!(settings, "chelabase.managed_role=function");
            assert_eq!(fx.memberships(&login).await, ("reporting:true:false:false".into(), "0".into()));
            let verifier = fx
                .value(&format!("SELECT rolpassword FROM pg_authid WHERE rolname = '{login}'"))
                .await;
            assert!(verifier.starts_with("SCRAM-SHA-256$4096:"), "{verifier}");
            let ep = fx.ep().await;
            assert_eq!(
                fx.metadata.routable_role_secret(&ep, &login).await.unwrap(),
                Some(Some(verifier.clone()))
            );
            assert!(fx.metadata.pending_sql_logins(fx.main).await.unwrap().is_empty());
            let oid = fx.oid(&login).await;
            fx.worker.sync_sql_logins(fx.main).await.unwrap();
            assert_eq!(fx.oid(&login).await, oid, "kept");
            // The login can't become the run-as role (SET FALSE), only inherit it.
            let set_role = fx.sql_as(&login, "SET ROLE reporting").await;
            assert!(set_role.is_err(), "{set_role:?}");
        })
        .await;
    }

    /// AC 6: a role a tenant made under the login's name (unmarked, with a membership of its own
    /// and a member, owning a table) is dropped and made again clean; the table goes to the
    /// database's owner. A marked one with an extra membership goes too.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn fn_sql_logins_squatted_role_is_dropped_and_recreated() {
        with_fx(|fx| async move {
            fx.sql("CREATE ROLE reporting NOLOGIN; CREATE ROLE evil NOLOGIN; CREATE ROLE spy NOLOGIN")
                .await;
            let f = fx.function("hello", "reporting").await;
            let login = function_login_name(f);
            fx.sql(&format!(
                "CREATE ROLE {login} LOGIN PASSWORD 'known'; GRANT evil TO {login}; GRANT {login} TO spy;
                 GRANT reporting TO {login}; CREATE TABLE public.planted (id int); ALTER TABLE public.planted OWNER TO {login}"
            ))
            .await;
            let squatter = fx.oid(&login).await;
            fx.worker.sync_sql_logins(fx.main).await.unwrap();
            assert_ne!(fx.oid(&login).await, squatter, "made again");
            assert_eq!(fx.memberships(&login).await, ("reporting:true:false:false".into(), "0".into()));
            let owner = fx
                .value("SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid = 'public.planted'::regclass")
                .await;
            let dba = fx
                .value("SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = current_database()")
                .await;
            assert_eq!(owner, dba);

            // Marked, but a member of `evil` too (granted after the login was made): gone again.
            fx.sql(&format!("GRANT evil TO {login}")).await;
            sqlx::query("DELETE FROM neon_roles WHERE name = $1")
                .bind(&login)
                .execute(fx.metadata.pool())
                .await
                .unwrap();
            let made = fx.oid(&login).await;
            fx.worker.sync_sql_logins(fx.main).await.unwrap();
            assert_ne!(fx.oid(&login).await, made);
            assert_eq!(fx.memberships(&login).await, ("reporting:true:false:false".into(), "0".into()));
        })
        .await;
    }

    /// AC 8: a run-as role with BYPASSRLS still gives the login none, and an RLS policy for
    /// the run-as role filters the login's rows.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn fn_sql_logins_no_bypassrls_and_rls_filters() {
        with_fx(|fx| async move {
            fx.sql(
                "CREATE ROLE reporting NOLOGIN BYPASSRLS;
                 CREATE TABLE public.notes (id int, visible boolean);
                 INSERT INTO public.notes VALUES (1, true), (2, false);
                 ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY seen ON public.notes FOR SELECT TO reporting USING (visible);
                 GRANT SELECT ON public.notes TO reporting",
            )
            .await;
            let f = fx.function("hello", "reporting").await;
            let login = function_login_name(f);
            fx.worker.sync_sql_logins(fx.main).await.unwrap();
            assert_eq!(
                fx.value(&format!(
                    "SELECT rolbypassrls FROM pg_roles WHERE rolname = '{login}'"
                ))
                .await,
                "f"
            );
            let seen = fx
                .sql_as(&login, "SELECT count(*) FROM public.notes")
                .await
                .unwrap();
            assert_eq!(seen.trim(), "1", "the policy filters the login's rows");
        })
        .await;
    }

    /// A namesake run-as role (dropped and made again: another OID) is never granted: the
    /// reconcile fails and no login is made.
    #[tokio::test]
    #[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
    async fn fn_sql_logins_namesake_run_as_role_is_never_granted() {
        with_fx(|fx| async move {
            fx.sql("CREATE ROLE reporting NOLOGIN").await;
            let f = fx.function("hello", "reporting").await;
            let login = function_login_name(f);
            fx.sql("DROP OWNED BY reporting; REVOKE reporting FROM authenticator; DROP ROLE reporting; CREATE ROLE reporting NOLOGIN")
                .await;
            let err = fx.worker.sync_sql_logins(fx.main).await.unwrap_err();
            assert!(err.contains("different role"), "{err}");
            assert_eq!(
                fx.value(&format!("SELECT count(*) FROM pg_roles WHERE rolname = '{login}'")).await,
                "0"
            );
            let ep = fx.ep().await;
            assert_eq!(fx.metadata.routable_role_secret(&ep, &login).await.unwrap(), Some(None));
        })
        .await;
    }
}
