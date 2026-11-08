//! Edge functions m13 (#175): a function's triggers on a branch through the management API,
//! against the fake metadata, with m3's access rules (reads Viewer, writes `BRANCH`, 404 for
//! another database's branch or function).
#![allow(dead_code, unused_imports)]
// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

include!("common/fakes.rs");

mod functions_triggers {
    use super::*;

    /// `dee` a Developer, `vic` a Viewer, `ada` an Admin of the project; `out` a Developer of
    /// another project. `dev` is unprotected; main is protected only where a test says so.
    struct World {
        w: MemberWorld,
        main: BranchId,
    }

    async fn world_in(mode: Mode) -> World {
        let w = member_world(mode).await;
        w.metadata
            .add_granted(w.tenant, "dee", w.project, PlatformRole::Developer, &[]);
        w.metadata
            .add_granted(w.tenant, "vic", w.project, PlatformRole::Viewer, &[]);
        w.metadata
            .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
        let elsewhere = w
            .metadata
            .create_project(w.tenant, "Elsewhere")
            .await
            .unwrap()
            .id;
        w.metadata
            .add_granted(w.tenant, "out", elsewhere, PlatformRole::Developer, &[]);
        let main = w
            .metadata
            .branches
            .lock()
            .unwrap()
            .values()
            .find(|b| b.database_id == w.db && b.parent_branch_id.is_none())
            .map(|b| b.id)
            .unwrap();
        World { w, main }
    }

    async fn world() -> World {
        world_in(Mode::SelfHosted).await
    }

    impl World {
        fn path(&self, branch: BranchId, tail: &str) -> String {
            format!(
                "/v1/databases/{}/branches/{}/functions{tail}",
                self.w.db,
                branch.as_uuid()
            )
        }

        async fn call(
            &self,
            sub: &str,
            method: &str,
            path: &str,
            body: Option<String>,
        ) -> (u16, Value) {
            let mut req = reqwest::Client::new()
                .request(
                    method.parse().unwrap(),
                    format!("{}{path}", self.w.app.base),
                )
                .header(IDENTITY_HEADER, self.w.app.token_for(sub, "org-1"))
                .header("content-type", "application/json");
            if let Some(body) = body {
                req = req.body(body);
            }
            let resp = req.send().await.unwrap();
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap();
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        /// Creates function `name` (no deployment needed for a trigger).
        async fn function(&self, branch: BranchId, name: &str) {
            let (status, body) = self
                .call(
                    "ada",
                    "PUT",
                    &self.path(branch, &format!("/{name}")),
                    Some("{}".into()),
                )
                .await;
            assert!(status == 201 || status == 200, "{status} {body}");
        }

        /// A table trigger of `name` on `branch` (insert on public.orders), made by `ada`.
        async fn trigger(&self, branch: BranchId, name: &str) -> String {
            let (status, body) = self
                .call(
                    "ada",
                    "POST",
                    &self.path(branch, &format!("/{name}/triggers")),
                    Some(table().to_string()),
                )
                .await;
            assert_eq!(status, 201, "{body}");
            body["id"].as_str().unwrap().to_string()
        }
    }

    fn table() -> Value {
        json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "events": ["insert", "update"]})
    }

    fn storage() -> Value {
        json!({"source": "storage", "pg_database": "app", "bucket": "avatars", "prefix": "in/", "events": ["object.created"]})
    }

    /// Every route with its method, and whether it reads.
    fn routes(
        w: &World,
        branch: BranchId,
        id: &str,
    ) -> Vec<(&'static str, String, Option<String>, bool)> {
        vec![
            ("GET", w.path(branch, "/hello/triggers"), None, true),
            (
                "POST",
                w.path(branch, "/hello/triggers"),
                Some(table().to_string()),
                false,
            ),
            (
                "PATCH",
                w.path(branch, &format!("/hello/triggers/{id}")),
                Some(json!({"enabled": false}).to_string()),
                false,
            ),
            (
                "DELETE",
                w.path(branch, &format!("/hello/triggers/{id}")),
                None,
                false,
            ),
        ]
    }

    /// A table trigger starts `pending_install`, a storage one `installed`; both list with
    /// their delivery fields; a PATCH turns one off and on; a DELETE deletes once.
    #[tokio::test]
    async fn functions_triggers_happy_path() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/triggers");
        let (status, made) = w
            .call("dee", "POST", &base, Some(table().to_string()))
            .await;
        assert_eq!(status, 201, "{made}");
        assert_eq!(
            (
                made["source"].as_str(),
                made["state"].as_str(),
                made["enabled"].as_bool()
            ),
            (Some("table"), Some("pending_install"), Some(true))
        );
        assert_eq!(
            (made["schema"].as_str(), made["table"].as_str()),
            (Some("public"), Some("orders"))
        );
        assert_eq!(made["events"], json!(["insert", "update"]));
        assert_eq!(
            (
                made["dead_count"].as_i64(),
                made["dropped_count"].as_i64(),
                made["last_delivery_error"].clone()
            ),
            (Some(0), Some(0), Value::Null)
        );
        let (status, st) = w
            .call("dee", "POST", &base, Some(storage().to_string()))
            .await;
        assert_eq!(status, 201, "{st}");
        assert_eq!(
            (
                st["state"].as_str(),
                st["bucket"].as_str(),
                st["prefix"].as_str()
            ),
            (Some("installed"), Some("avatars"), Some("in/"))
        );
        let (status, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(status, 200);
        assert_eq!(list.as_array().unwrap().len(), 2);
        let one = format!("{base}/{}", made["id"].as_str().unwrap());
        let (status, off) = w
            .call(
                "dee",
                "PATCH",
                &one,
                Some(json!({"enabled": false}).to_string()),
            )
            .await;
        assert_eq!(
            (status, off["enabled"].as_bool()),
            (200, Some(false)),
            "{off}"
        );
        let (status, on) = w
            .call(
                "dee",
                "PATCH",
                &one,
                Some(json!({"enabled": true}).to_string()),
            )
            .await;
        assert_eq!((status, on["enabled"].as_bool()), (200, Some(true)), "{on}");
        let (status, _) = w.call("dee", "DELETE", &one, None).await;
        assert_eq!(status, 204);
        let (status, _) = w.call("dee", "DELETE", &one, None).await;
        assert_eq!(status, 404);
        let (status, _) = w
            .call(
                "dee",
                "PATCH",
                &one,
                Some(json!({"enabled": true}).to_string()),
            )
            .await;
        assert_eq!(status, 404);
    }

    /// 422: table and object events mixed, a table trigger without its schema or table, a
    /// storage one with them, no events, a repeat, an unknown event or field; 400 for bad JSON.
    #[tokio::test]
    async fn functions_triggers_invalid_bodies_422() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/triggers");
        for body in [
            json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "events": ["insert", "object.created"]}),
            json!({"source": "storage", "pg_database": "app", "events": ["object.created", "insert"]}),
            json!({"source": "table", "pg_database": "app", "table": "orders", "events": ["insert"]}),
            json!({"source": "table", "pg_database": "app", "schema": "public", "events": ["insert"]}),
            json!({"source": "storage", "pg_database": "app", "schema": "public", "events": ["object.created"]}),
            json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "bucket": "b", "events": ["insert"]}),
            json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "events": []}),
            json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "events": ["insert", "insert"]}),
            json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "events": ["truncate"]}),
            json!({"source": "storage", "events": ["object.created"]}),
            json!({"source": "table", "pg_database": "", "schema": "public", "table": "orders", "events": ["insert"]}),
            json!({"source": "queue", "pg_database": "app", "events": ["insert"]}),
            json!({"source": "table", "pg_database": "app", "schema": "public", "table": "orders", "events": ["insert"], "extra": 1}),
        ] {
            let (status, err) = w.call("dee", "POST", &base, Some(body.to_string())).await;
            assert_eq!(status, 422, "{body}: {err}");
        }
        let (status, _) = w
            .call("dee", "POST", &base, Some("{\"source\": ".into()))
            .await;
        assert_eq!(status, 400);
        let id = w.trigger(w.w.dev, "hello").await;
        let (status, _) = w
            .call("dee", "PATCH", &format!("{base}/{id}"), Some("{}".into()))
            .await;
        assert_eq!(status, 422, "enabled is required");
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list.as_array().unwrap().len(), 1, "nothing else was added");
    }

    /// m13b (B2): a table trigger on Chelabase's or Postgres's own schemas is 422 (a trigger on
    /// `chelabase_functions.outbox` would recurse into `enqueue()`); (S4) so is a table trigger
    /// on a service, which is never delivered events.
    #[tokio::test]
    async fn functions_triggers_reserved_schemas_and_services_422() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/triggers");
        for (schema, table) in [
            ("chelabase_functions", "outbox"),
            ("chelabase_guard", "x"),
            ("chelabase", "app_users_table"),
            ("chelabase_merge", "x"),
            ("storage", "objects"),
            ("pg_catalog", "pg_authid"),
            ("information_schema", "tables"),
            ("pg_toast", "x"),
            ("pg_temp_3", "x"),
            ("chelabase_new", "x"),
        ] {
            let body = json!({"source": "table", "pg_database": "app", "schema": schema, "table": table, "events": ["insert"]});
            let (status, err) = w.call("dee", "POST", &base, Some(body.to_string())).await;
            assert_eq!(status, 422, "{schema}.{table}: {err}");
            assert!(err["error"]["message"].as_str().unwrap_or("").contains("schema"), "{err}");
        }
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert!(list.as_array().unwrap().is_empty(), "nothing was added");
        // A service is a tenant admin's to make (m9).
        w.w.metadata.add_member(w.w.tenant, "tom", MemberStatus::Active, true);
        let (status, body) = w
            .call("tom", "PUT", &w.path(w.w.dev, "/svc"), Some(json!({"kind": "service"}).to_string()))
            .await;
        assert!(status == 201 || status == 200, "{status} {body}");
        let svc = w.path(w.w.dev, "/svc/triggers");
        let (status, err) = w.call("dee", "POST", &svc, Some(table().to_string())).await;
        assert_eq!(status, 422, "{err}");
        assert!(err["error"]["message"].as_str().unwrap_or("").contains("service"), "{err}");
        let (status, _) = w.call("dee", "POST", &base, Some(table().to_string())).await;
        assert_eq!(status, 201, "a user schema is fine");
    }

    /// A Viewer reads and is 403 on every write; another project's Developer, and a path
    /// naming another database's branch, are 404 everywhere; a Developer on protected main is
    /// 403 on every write.
    #[tokio::test]
    async fn functions_triggers_access() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let id = w.trigger(w.w.dev, "hello").await;
        for (method, path, body, read) in routes(&w, w.w.dev, &id) {
            let (status, text) = w.call("vic", method, &path, body.clone()).await;
            assert_eq!(
                status,
                if read { 200 } else { 403 },
                "vic {method} {path}: {text}"
            );
            let (status, text) = w.call("out", method, &path, body).await;
            assert_eq!(status, 404, "out {method} {path}: {text}");
        }
        let mut other = sample_database(w.w.tenant);
        other.project_id = w.w.project;
        let other_main = other.main.id;
        w.w.metadata.insert_database(other);
        for (method, path, body, _) in routes(&w, other_main, &id) {
            let (status, text) = w.call("dee", method, &path, body).await;
            assert_eq!(status, 404, "{method} {path}: {text}");
        }
        w.function(w.main, "hello").await;
        let main_id = w.trigger(w.main, "hello").await;
        w.w.metadata
            .update_normal_branch(w.main, |b| b.protected = true)
            .unwrap();
        for (method, path, body, read) in routes(&w, w.main, &main_id) {
            let (status, text) = w.call("dee", method, &path, body).await;
            assert_eq!(
                status,
                if read { 200 } else { 403 },
                "dee on protected main {method} {path}: {text}"
            );
        }
    }

    /// A trigger is only reachable under its own function; an unknown function is 404.
    #[tokio::test]
    async fn functions_triggers_of_another_function_404() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        w.function(w.w.dev, "other").await;
        let id = w.trigger(w.w.dev, "hello").await;
        for (method, body) in [
            ("PATCH", Some(json!({"enabled": false}).to_string())),
            ("DELETE", None),
        ] {
            let (status, text) = w
                .call(
                    "dee",
                    method,
                    &w.path(w.w.dev, &format!("/other/triggers/{id}")),
                    body,
                )
                .await;
            assert_eq!(status, 404, "{method}: {text}");
        }
        let (_, list) = w
            .call("dee", "GET", &w.path(w.w.dev, "/other/triggers"), None)
            .await;
        assert_eq!(list, json!([]));
        let (status, _) = w
            .call(
                "dee",
                "POST",
                &w.path(w.w.dev, "/missing/triggers"),
                Some(table().to_string()),
            )
            .await;
        assert_eq!(status, 404);
    }
}
