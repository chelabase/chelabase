//! Edge functions m3 (#165): the management API for a branch's functions, against the fake
//! metadata, a fake bundle sink and the fake job queue (which records each
//! `chelabase.fn.changed` publish).
#![allow(dead_code, unused_imports)]
// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

include!("common/fakes.rs");

mod functions {
    use super::*;
    use sha2::{Digest, Sha256};

    const SECRET: &str = "s3cr3t";

    /// A world: `dee` is a Developer holding `writer`, `vic` a Viewer, `ada` an Admin of the
    /// project; `out` is a Developer of another project. `dev` is unprotected; main is
    /// protected only where a test says so.
    struct World {
        w: MemberWorld,
        main: BranchId,
    }

    async fn world_in(mode: Mode) -> World {
        let w = member_world(mode).await;
        let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
        for role in ["writer", "reporter"] {
            w.metadata.add_group_role(&ep, role, &[]);
        }
        w.metadata.add_granted(
            w.tenant,
            "dee",
            w.project,
            PlatformRole::Developer,
            &["writer"],
        );
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
        w.metadata.add_granted(
            w.tenant,
            "out",
            elsewhere,
            PlatformRole::Developer,
            &["writer"],
        );
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
        ) -> (u16, String) {
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
            (resp.status().as_u16(), resp.text().await.unwrap())
        }

        async fn json(
            &self,
            sub: &str,
            method: &str,
            path: &str,
            body: Option<Value>,
        ) -> (u16, Value) {
            let (status, text) = self
                .call(sub, method, path, body.map(|b| b.to_string()))
                .await;
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        }

        fn published(&self) -> Vec<(BranchId, String)> {
            self.w.jobs.fn_changed.lock().unwrap().clone()
        }

        fn protect_main(&self) {
            self.w
                .metadata
                .update_normal_branch(self.main, |b| b.protected = true)
                .unwrap();
        }

        /// Creates `name` and deploys `v` on `branch` as `sub`; the deployment's id.
        async fn deployed(&self, sub: &str, branch: BranchId, name: &str, v: &str) -> String {
            let (status, body) = self
                .json(
                    sub,
                    "PUT",
                    &self.path(branch, &format!("/{name}")),
                    Some(json!({})),
                )
                .await;
            assert!(status == 201 || status == 200, "{status} {body}");
            let (status, body) = self
                .call(
                    sub,
                    "POST",
                    &self.path(branch, &format!("/{name}/deployments")),
                    Some(deploy_body(v)),
                )
                .await;
            assert_eq!(status, 201, "{body}");
            let body: Value = serde_json::from_str(&body).unwrap();
            body["id"].as_str().unwrap().to_string()
        }
    }

    /// A manifest with odd spacing: the orchestrator forwards these bytes, never re-encoded.
    fn manifest(v: &str) -> String {
        format!(
            "{{ \"compatibilityDate\" : \"2026-10-01\", \"mainModule\":\"main.js\",\n  \"modules\": [{{\"name\":\"main.js\",\"type\":\"js\",\"base64\":\"{}\"}}] }}",
            v
        )
    }

    fn sha(bytes: &str) -> String {
        hex::encode(Sha256::digest(bytes.as_bytes()))
    }

    fn deploy_body(v: &str) -> String {
        let m = manifest(v);
        format!(
            "{{\"manifest\": {m}, \"sha256\": \"{}\", \"note\": \"first\"}}",
            sha(&m)
        )
    }

    /// Every route with its method, and whether it reads.
    fn routes(w: &World, branch: BranchId) -> Vec<(&'static str, String, Option<String>, bool)> {
        vec![
            ("GET", w.path(branch, ""), None, true),
            ("PUT", w.path(branch, "/hello"), Some("{}".into()), false),
            (
                "PATCH",
                w.path(branch, "/hello"),
                Some("{\"enabled\": true}".into()),
                false,
            ),
            ("DELETE", w.path(branch, "/hello"), None, false),
            (
                "POST",
                w.path(branch, "/hello/deployments"),
                Some(deploy_body("djE=")),
                false,
            ),
            ("GET", w.path(branch, "/hello/deployments"), None, true),
            ("GET", w.path(branch, "/hello/secrets"), None, true),
            (
                "PUT",
                w.path(branch, "/hello/secrets/API_KEY"),
                Some(json!({"value": SECRET}).to_string()),
                false,
            ),
            (
                "DELETE",
                w.path(branch, "/hello/secrets/API_KEY"),
                None,
                false,
            ),
        ]
    }

    /// AC 1: a Developer creates, deploys and enables `hello` on an unprotected branch; the
    /// list shows it enabled with that deployment, the bundle reached the sink byte for byte,
    /// and each write published `chelabase.fn.changed.<branch>` once.
    #[tokio::test]
    async fn functions_happy_path() {
        let w = world().await;
        let (status, body) = w
            .json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        assert_eq!(status, 201, "{body}");
        assert_eq!(
            (body["name"].as_str(), body["kind"].as_str()),
            (Some("hello"), Some("function"))
        );
        let (status, again) = w
            .json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        assert_eq!(status, 200, "{again}");

        let (status, text) = w
            .call(
                "dee",
                "POST",
                &w.path(w.w.dev, "/hello/deployments"),
                Some(deploy_body("djE=")),
            )
            .await;
        assert_eq!(status, 201, "{text}");
        let deployment: Value = serde_json::from_str(&text).unwrap();
        let puts = w.w.app.bundles.puts.lock().unwrap().clone();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].0, w.w.db);
        assert_eq!(puts[0].1, sha(&manifest("djE=")));
        assert_eq!(
            puts[0].2,
            manifest("djE=").into_bytes(),
            "the manifest's bytes, unchanged"
        );
        assert_eq!(deployment["bundle_sha256"], json!(sha(&manifest("djE="))));
        assert_eq!(deployment["compatibility_date"], json!("2026-10-01"));
        assert_eq!(deployment["created_by"], json!("dee"));

        let (status, body) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"enabled": true})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        let (status, list) = w.json("vic", "GET", &w.path(w.w.dev, ""), None).await;
        assert_eq!(status, 200, "{list}");
        assert_eq!(list[0]["name"], json!("hello"));
        assert_eq!(list[0]["enabled"], json!(true));
        assert_eq!(list[0]["deployment_id"], deployment["id"]);
        assert_eq!(list[0]["auth"], json!("key"));

        let (status, deployments) = w
            .json("vic", "GET", &w.path(w.w.dev, "/hello/deployments"), None)
            .await;
        assert_eq!(status, 200);
        assert_eq!(deployments[0]["id"], deployment["id"]);

        // Created, deployed (and activated), enabled: three writes, three events, all for dev.
        let published = w.published();
        assert_eq!(published, vec![(w.w.dev, "hello".to_string()); 3]);

        // Auth, secrets.
        let (status, body) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"auth": "public"})),
            )
            .await;
        assert_eq!(
            (status, body["auth"].clone()),
            (200, json!("public")),
            "{body}"
        );
        let (status, _) = w
            .json(
                "dee",
                "PUT",
                &w.path(w.w.dev, "/hello/secrets/API_KEY"),
                Some(json!({"value": SECRET})),
            )
            .await;
        assert_eq!(status, 204);
        let (status, names) = w
            .json("vic", "GET", &w.path(w.w.dev, "/hello/secrets"), None)
            .await;
        assert_eq!((status, names), (200, json!(["API_KEY"])));
        let (status, _) = w
            .json(
                "dee",
                "DELETE",
                &w.path(w.w.dev, "/hello/secrets/API_KEY"),
                None,
            )
            .await;
        assert_eq!(status, 204);
        let (status, _) = w
            .json(
                "dee",
                "DELETE",
                &w.path(w.w.dev, "/hello/secrets/API_KEY"),
                None,
            )
            .await;
        assert_eq!(status, 404);
        assert_eq!(w.published().len(), 6);
    }

    /// AC 2: a Viewer is 403 on every write and 200 on every read.
    #[tokio::test]
    async fn functions_viewer_writes_forbidden() {
        let w = world().await;
        w.deployed("dee", w.w.dev, "hello", "djE=").await;
        for (method, path, body, read) in routes(&w, w.w.dev) {
            let (status, text) = w.call("vic", method, &path, body).await;
            if read {
                assert_eq!(status, 200, "{method} {path}: {text}");
            } else {
                assert_eq!(status, 403, "{method} {path}: {text}");
            }
        }
    }

    /// AC 3 and 4: another project's Developer gets 404 everywhere, as for a missing database;
    /// so does a path naming another database's branch.
    #[tokio::test]
    async fn functions_cross_database_and_mismatch_404() {
        let w = world().await;
        w.deployed("dee", w.w.dev, "hello", "djE=").await;
        for (method, path, body, _) in routes(&w, w.w.dev) {
            let (status, text) = w.call("out", method, &path, body).await;
            assert_eq!(status, 404, "{method} {path}: {text}");
        }
        // A database `dee` may use, with the branch of another database in the path.
        let mut other = sample_database(w.w.tenant);
        other.project_id = w.w.project;
        let other_main = other.main.id;
        w.w.metadata.insert_database(other);
        for (method, path, body, _) in routes(&w, other_main) {
            let (status, text) = w.call("dee", method, &path, body).await;
            assert_eq!(status, 404, "{method} {path}: {text}");
        }
        assert!(
            w.published().len() <= 3,
            "only the setup's writes published"
        );
    }

    /// AC 5: a secret's value never comes back in a response or a log line, valid or not.
    #[tokio::test]
    async fn functions_secret_never_echoed() {
        let logs = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || LogWriter(sink.clone()))
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let w = world().await;
        w.deployed("dee", w.w.dev, "hello", "djE=").await;
        let mut answers = Vec::new();
        for body in [
            json!({"value": SECRET}).to_string(),
            format!("{{\"value\": \"{SECRET}\", \"extra\": 1}}"),
            format!("{{\"value\": \"{SECRET}\""),
            format!("{{\"value\": [\"{SECRET}\"]}}"),
        ] {
            answers.push(
                w.call(
                    "dee",
                    "PUT",
                    &w.path(w.w.dev, "/hello/secrets/API_KEY"),
                    Some(body.clone()),
                )
                .await,
            );
            answers.push(
                w.call(
                    "dee",
                    "PUT",
                    &w.path(w.w.dev, "/hello/secrets/9bad"),
                    Some(body),
                )
                .await,
            );
        }
        let big = json!({"value": format!("{SECRET}{}", "x".repeat(70 * 1024))}).to_string();
        answers.push(
            w.call(
                "dee",
                "PUT",
                &w.path(w.w.dev, "/hello/secrets/API_KEY"),
                Some(big),
            )
            .await,
        );
        answers.push(
            w.call(
                "vic",
                "PUT",
                &w.path(w.w.dev, "/hello/secrets/API_KEY"),
                Some(json!({"value": SECRET}).to_string()),
            )
            .await,
        );
        answers.push(
            w.call("dee", "GET", &w.path(w.w.dev, "/hello/secrets"), None)
                .await,
        );
        answers.push(w.call("dee", "GET", &w.path(w.w.dev, ""), None).await);
        assert_eq!(answers[0].0, 204);
        for (status, body) in &answers {
            assert!(!body.contains(SECRET), "{status}: {body}");
        }
        let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        assert!(
            !logs.is_empty(),
            "the subscriber captured the server's logs"
        );
        assert!(!logs.contains(SECRET), "{logs}");
    }

    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// AC 6: rolling back to another function's deployment is 422, nothing changes or is
    /// published; rolling back to its own earlier one works.
    #[tokio::test]
    async fn functions_rollback_cross_function_422() {
        let w = world().await;
        let first = w.deployed("dee", w.w.dev, "hello", "djE=").await;
        let second = w.deployed("dee", w.w.dev, "hello", "djI=").await;
        let other = w.deployed("dee", w.w.dev, "other", "djM=").await;
        let before = w.published().len();
        let (status, body) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"active_deployment_id": other})),
            )
            .await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(w.published().len(), before, "nothing published");
        let (_, list) = w.json("dee", "GET", &w.path(w.w.dev, ""), None).await;
        assert_eq!(list[0]["deployment_id"], json!(second));
        let (status, body) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"active_deployment_id": first})),
            )
            .await;
        assert_eq!(
            (status, body["deployment_id"].clone()),
            (200, json!(first)),
            "{body}"
        );
    }

    /// AC 7: platform mode answers 403 `not_available_in_platform_mode` everywhere.
    #[tokio::test]
    async fn functions_platform_mode_403() {
        let w = world_in(Mode::Platform).await;
        for (method, path, body, _) in routes(&w, w.w.dev) {
            for sub in [DEFAULT_SUB, "ada"] {
                let (status, text) = w.call(sub, method, &path, body.clone()).await;
                assert_eq!(status, 403, "{method} {path}: {text}");
                assert!(text.contains("not_available_in_platform_mode"), "{text}");
            }
        }
    }

    /// `/v1/me` says whether functions are available, so Studio hides the tab in platform mode
    /// without asking the functions API.
    #[tokio::test]
    async fn functions_me_reports_availability() {
        for (mode, available) in [(Mode::SelfHosted, true), (Mode::Platform, false)] {
            let w = world_in(mode).await;
            for sub in [DEFAULT_SUB, "dee"] {
                let (status, text) = w.call(sub, "GET", "/v1/me", None).await;
                assert_eq!(status, 200, "{mode:?} {sub}: {text}");
                let me: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(
                    me["functions_available"],
                    json!(available),
                    "{mode:?} {sub}: {text}"
                );
            }
        }
    }

    /// AC 8: a 13 MiB deploy body is 413 before it's parsed; the sink never sees it.
    #[tokio::test]
    async fn functions_oversize_413() {
        let w = world().await;
        w.json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        let body = format!("{{\"manifest\": \"{}\"", "x".repeat(13 * 1024 * 1024));
        let (status, text) = w
            .call(
                "dee",
                "POST",
                &w.path(w.w.dev, "/hello/deployments"),
                Some(body),
            )
            .await;
        assert_eq!(status, 413, "{text}");
        assert!(w.w.app.bundles.puts.lock().unwrap().is_empty());
    }

    /// Publish after commit, never on failure: the event's hook sees the write done; a refused
    /// write and a refused upload publish nothing and write nothing.
    #[tokio::test]
    async fn functions_publish_after_commit_not_on_failure() {
        let w = world().await;
        w.deployed("dee", w.w.dev, "hello", "djE=").await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (metadata, record, dev) = (w.w.metadata.clone(), seen.clone(), w.w.dev);
        *w.w.jobs.on_fn_changed.lock().unwrap() = Some(Arc::new(move |branch, _| {
            let enabled = metadata
                .functions
                .lock()
                .unwrap()
                .states
                .iter()
                .any(|(b, st)| *b == dev && st.enabled);
            record.lock().unwrap().push((branch, enabled));
        }));
        let (status, _) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"enabled": true})),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(w.w.dev, true)],
            "published once, after the write"
        );

        let before = w.published().len();
        let (status, _) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/nope"),
                Some(json!({"enabled": false})),
            )
            .await;
        assert_eq!(status, 404);
        for code in [422, 404, 413, 500, 503] {
            *w.w.app.bundles.status.lock().unwrap() = Some(code);
            let (status, text) = w
                .call(
                    "dee",
                    "POST",
                    &w.path(w.w.dev, "/hello/deployments"),
                    Some(deploy_body("djk=")),
                )
                .await;
            assert_eq!(status, code, "sink {code}: {text}");
        }
        assert_eq!(w.published().len(), before, "no event for a failed write");
        let (_, deployments) = w
            .json("dee", "GET", &w.path(w.w.dev, "/hello/deployments"), None)
            .await;
        assert_eq!(
            deployments.as_array().unwrap().len(),
            1,
            "no row for a refused bundle"
        );
    }

    /// A hash that doesn't match the manifest is 422 before the sink is called.
    #[tokio::test]
    async fn functions_hash_mismatch_422() {
        let w = world().await;
        w.json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        let body = format!(
            "{{\"manifest\": {}, \"sha256\": \"{}\"}}",
            manifest("djE="),
            "0".repeat(64)
        );
        let (status, text) = w
            .call(
                "dee",
                "POST",
                &w.path(w.w.dev, "/hello/deployments"),
                Some(body),
            )
            .await;
        assert_eq!(status, 422, "{text}");
        assert!(w.w.app.bundles.puts.lock().unwrap().is_empty());
    }

    /// AC 9 and 10: the protected-branch matrix. A Developer is 403 on protected main for a
    /// deploy, PATCH or secret, 2xx on dev; deleting needs Admin anywhere; an Admin's delete of
    /// a function active on protected main is 409 and main keeps its state.
    #[tokio::test]
    async fn functions_protected_branch_matrix() {
        let w = world().await;
        w.deployed("dee", w.main, "hello", "djE=").await;
        w.protect_main();
        for (method, tail, body) in [
            ("POST", "/hello/deployments", Some(deploy_body("djI="))),
            (
                "PATCH",
                "/hello",
                Some(json!({"enabled": true}).to_string()),
            ),
            (
                "PUT",
                "/hello/secrets/API_KEY",
                Some(json!({"value": "v"}).to_string()),
            ),
            ("PUT", "/hello", Some(json!({"auth": "public"}).to_string())),
        ] {
            let (status, text) = w
                .call("dee", method, &w.path(w.main, tail), body.clone())
                .await;
            assert_eq!(status, 403, "dee {method} {tail} on main: {text}");
            let (status, text) = w
                .call("ada", method, &w.path(w.main, tail), body.clone())
                .await;
            assert!(
                (200..300).contains(&status),
                "ada {method} {tail} on main: {status} {text}"
            );
        }
        // dev: a Developer's writes go through.
        w.deployed("dee", w.w.dev, "hello", "djM=").await;
        let (status, _) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"enabled": true})),
            )
            .await;
        assert_eq!(status, 200);
        let (status, text) = w
            .call("dee", "DELETE", &w.path(w.w.dev, "/hello"), None)
            .await;
        assert_eq!(status, 403, "a Developer never deletes: {text}");
        let (status, text) = w
            .call("ada", "DELETE", &w.path(w.w.dev, "/hello"), None)
            .await;
        assert_eq!(status, 409, "{text}");
        assert!(
            text.contains("function_active_on_protected_branch"),
            "{text}"
        );
        let (_, list) = w.json("ada", "GET", &w.path(w.main, ""), None).await;
        assert_eq!(list[0]["name"], json!("hello"), "main's state is untouched");
        // Unprotected again: the Admin's delete goes through and every branch hears of it.
        w.w.metadata
            .update_normal_branch(w.main, |b| b.protected = false)
            .unwrap();
        let before = w.published().len();
        let (status, text) = w
            .call("ada", "DELETE", &w.path(w.w.dev, "/hello"), None)
            .await;
        assert_eq!(status, 204, "{text}");
        let after: Vec<_> = w.published()[before..].iter().map(|(b, _)| *b).collect();
        assert!(
            after.contains(&w.main) && after.contains(&w.w.dev),
            "{after:?}"
        );
    }

    /// A function named `public` would be shadowed by the keyless invoke prefix: 422. Changing
    /// a kind needs Admin.
    #[tokio::test]
    async fn functions_name_and_kind_rules() {
        let w = world().await;
        for name in ["public", "Bad", "9x"] {
            let (status, text) = w
                .call(
                    "dee",
                    "PUT",
                    &w.path(w.w.dev, &format!("/{name}")),
                    Some("{}".into()),
                )
                .await;
            assert_eq!(status, 422, "{name}: {text}");
        }
        w.json("dee", "PUT", &w.path(w.w.dev, "/svc"), Some(json!({})))
            .await;
        let (status, text) = w
            .call(
                "dee",
                "PUT",
                &w.path(w.w.dev, "/svc"),
                Some(json!({"kind": "service"}).to_string()),
            )
            .await;
        assert_eq!(status, 403, "{text}");
        // m9: a project Admin isn't enough to make a service; a tenant admin is.
        let (status, _) = w
            .json("ada", "PUT", &w.path(w.w.dev, "/svc"), Some(json!({"kind": "service"})))
            .await;
        assert_eq!(status, 403);
        w.w.metadata.add_member(w.w.tenant, "tom", MemberStatus::Active, true);
        let (status, body) = w
            .json(
                "tom",
                "PUT",
                &w.path(w.w.dev, "/svc"),
                Some(json!({"kind": "service"})),
            )
            .await;
        assert_eq!(
            (status, body["kind"].clone()),
            (200, json!("service")),
            "{body}"
        );
        // A service has no bundle.
        let (status, text) = w
            .call(
                "tom",
                "POST",
                &w.path(w.w.dev, "/svc/deployments"),
                Some(deploy_body("djE=")),
            )
            .await;
        assert_eq!(status, 422, "{text}");
    }

    /// AC 12 and 13: run-as. A Developer sets a role without BYPASSRLS: a managed key exists for
    /// it, listed as the function's and refused by the keys routes (409). A BYPASSRLS role needs
    /// Admin; a missing one is 422. Deleting the function revokes the key. The reported role
    /// follows the key's (a rename moves it).
    #[tokio::test]
    async fn functions_run_as_role() {
        let w = world().await;
        w.deployed("dee", w.w.dev, "hello", "djE=").await;
        *w.w.jobs.role_check.lock().unwrap() =
            Some(Ok(CheckDataRoleReply::ok_with_oid(4242, "tl-1")));
        let (status, body) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"run_as_role": "writer"})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["run_as_role"], json!("writer"));
        let keys_path = format!(
            "/v1/databases/{}/branches/{}/keys",
            w.w.db,
            w.w.dev.as_uuid()
        );
        let (_, keys) = w.json("dee", "GET", &keys_path, None).await;
        let managed = keys
            .as_array()
            .unwrap()
            .iter()
            .find(|k| k["managed_by_function"] == json!("hello"))
            .cloned()
            .expect("a managed key");
        assert_eq!(managed["role"], json!("writer"));
        assert!(managed.get("key").is_none(), "never revealed");
        let key_id = managed["id"].as_str().unwrap().to_string();
        for (method, tail, body) in [
            ("DELETE", format!("/{key_id}"), None),
            ("POST", format!("/{key_id}/rotate"), None),
            (
                "PATCH",
                format!("/{key_id}"),
                Some(json!({"databases": null})),
            ),
        ] {
            let (status, body) = w
                .json("ada", method, &format!("{keys_path}{tail}"), body)
                .await;
            assert_eq!(status, 409, "{method} {tail}: {body}");
        }

        // The rename follow-up: the reported role is the live key's.
        for (k, _) in w.w.metadata.keys.lock().unwrap().iter_mut() {
            if k.id.to_string() == key_id {
                k.role = "writer2".into();
            }
        }
        let (_, list) = w.json("dee", "GET", &w.path(w.w.dev, ""), None).await;
        assert_eq!(list[0]["run_as_role"], json!("writer2"));

        // BYPASSRLS: Admin only.
        *w.w.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(
            77, "tl-1",
        )
        .bypassing_rls(true)));
        let (status, body) = w
            .json(
                "dee",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"run_as_role": "writer"})),
            )
            .await;
        assert_eq!(status, 403, "{body}");
        let (status, body) = w
            .json(
                "ada",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"run_as_role": "reporter"})),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        // A missing role.
        *w.w.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused(
            "role \"ghost\" does not exist",
        )));
        let (status, body) = w
            .json(
                "ada",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"run_as_role": "ghost"})),
            )
            .await;
        assert_eq!(status, 422, "{body}");
        // Cleared.
        let (status, body) = w
            .json(
                "ada",
                "PATCH",
                &w.path(w.w.dev, "/hello"),
                Some(json!({"run_as_role": null})),
            )
            .await;
        assert_eq!(
            (status, body["run_as_role"].clone()),
            (200, Value::Null),
            "{body}"
        );

        // Deleting the function revokes its key.
        *w.w.jobs.role_check.lock().unwrap() =
            Some(Ok(CheckDataRoleReply::ok_with_oid(4242, "tl-1")));
        w.json(
            "dee",
            "PATCH",
            &w.path(w.w.dev, "/hello"),
            Some(json!({"run_as_role": "writer"})),
        )
        .await;
        let (status, _) = w
            .json("ada", "DELETE", &w.path(w.w.dev, "/hello"), None)
            .await;
        assert_eq!(status, 204);
        let live =
            w.w.metadata
                .keys
                .lock()
                .unwrap()
                .iter()
                .any(|(k, _)| k.managed_by_function.is_some() && k.revoked_at.is_none());
        assert!(!live, "the managed key is revoked");
    }

    /// Audit S1: a deploy that doesn't activate still publishes (CLI and Studio list it).
    #[tokio::test]
    async fn functions_deploy_without_activate_publishes() {
        let w = world().await;
        w.json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        let before = w.published().len();
        let m = manifest("djE=");
        let body = format!(
            "{{\"manifest\": {m}, \"sha256\": \"{}\", \"activate\": false}}",
            sha(&m)
        );
        let (status, text) = w
            .call(
                "dee",
                "POST",
                &w.path(w.w.dev, "/hello/deployments"),
                Some(body),
            )
            .await;
        assert_eq!(status, 201, "{text}");
        assert_eq!(w.published()[before..], [(w.w.dev, "hello".to_string())]);
        let (_, list) = w.json("dee", "GET", &w.path(w.w.dev, ""), None).await;
        assert_eq!(list[0]["deployment_id"], Value::Null, "not activated");
    }

    /// Audit S2: protection is checked again after the upload: a Developer's deploy to a branch
    /// an Admin protected meanwhile is 403, and nothing is recorded or activated.
    #[tokio::test]
    async fn functions_deploy_rechecks_protection_after_upload() {
        let w = world().await;
        w.json("dee", "PUT", &w.path(w.main, "/hello"), Some(json!({})))
            .await;
        let (started, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        *w.w.app.bundles.gate.lock().unwrap() = Some((started.clone(), release.clone()));
        let base = w.w.app.base.clone();
        let token = w.w.app.token_for("dee", "org-1");
        let path = w.path(w.main, "/hello/deployments");
        let deploy = tokio::spawn(async move {
            let resp = reqwest::Client::new()
                .post(format!("{base}{path}"))
                .header(IDENTITY_HEADER, token)
                .header("content-type", "application/json")
                .body(deploy_body("djE="))
                .send()
                .await
                .unwrap();
            (resp.status().as_u16(), resp.text().await.unwrap())
        });
        started.notified().await;
        w.protect_main();
        release.notify_one();
        let (status, text) = deploy.await.unwrap();
        assert_eq!(status, 403, "{text}");
        let (_, deployments) = w
            .json("ada", "GET", &w.path(w.main, "/hello/deployments"), None)
            .await;
        assert_eq!(deployments, json!([]), "no row recorded");
        let (_, list) = w.json("ada", "GET", &w.path(w.main, ""), None).await;
        assert_eq!(list[0]["deployment_id"], Value::Null, "nothing activated");
    }

    /// N1: the manifest is a JSON object.
    #[tokio::test]
    async fn functions_manifest_must_be_an_object() {
        let w = world().await;
        w.json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        for m in ["\"just a string\"", "[1, 2]", "42"] {
            let body = format!(
                "{{\"manifest\": {m}, \"sha256\": \"{}\", \"compatibility_date\": \"2026-10-01\"}}",
                sha(m)
            );
            let (status, text) = w
                .call(
                    "dee",
                    "POST",
                    &w.path(w.w.dev, "/hello/deployments"),
                    Some(body),
                )
                .await;
            assert_eq!(status, 422, "{m}: {text}");
        }
        assert!(w.w.app.bundles.puts.lock().unwrap().is_empty());
    }

    /// N2: an empty or blank run-as role is refused here, before the worker is asked.
    #[tokio::test]
    async fn functions_blank_run_as_role_422() {
        let w = world().await;
        w.deployed("dee", w.w.dev, "hello", "djE=").await;
        for role in ["", "   "] {
            let (status, body) = w
                .json(
                    "dee",
                    "PATCH",
                    &w.path(w.w.dev, "/hello"),
                    Some(json!({"run_as_role": role})),
                )
                .await;
            assert_eq!(status, 422, "{role:?}: {body}");
        }
        assert!(
            w.w.jobs.role_checks.lock().unwrap().is_empty(),
            "the worker wasn't asked"
        );
    }

    /// N8: a body that breaks off (shorter than its Content-Length) is 400, not 413.
    #[tokio::test]
    async fn functions_broken_body_400() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let w = world().await;
        w.json("dee", "PUT", &w.path(w.w.dev, "/hello"), Some(json!({})))
            .await;
        let addr = w.w.app.base.trim_start_matches("http://").to_string();
        let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let request = format!(
            "POST {} HTTP/1.1\r\nhost: {addr}\r\n{IDENTITY_HEADER}: {}\r\ncontent-type: application/json\r\ncontent-length: 1000\r\n\r\n{{\"manifest\":",
            w.path(w.w.dev, "/hello/deployments"),
            w.w.app.token_for("dee", "org-1"),
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer).await;
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
    }

    // --- m9: external services --------------------------------------------------------------

    /// `tom` is a tenant admin.
    async fn service_world() -> World {
        let w = world().await;
        w.w.metadata.add_member(w.w.tenant, "tom", MemberStatus::Active, true);
        w
    }

    fn external(url: &str) -> Value {
        json!({"spec": {"backend": "external", "url": url, "health_path": "/healthz"}})
    }

    /// AC: a tenant admin creates a service (its signing secret shown once), deploys an
    /// external URL (stored normalized, active on the branch, `fn.changed` published) and
    /// rotates the secret (shown once again).
    #[tokio::test]
    async fn functions_service_registered_by_a_tenant_admin() {
        let w = service_world().await;
        let (status, created) = w
            .json("tom", "PUT", &w.path(w.w.dev, "/stub"), Some(json!({"kind": "service"})))
            .await;
        assert_eq!(status, 201, "{created}");
        assert_eq!(created["kind"], "service");
        let first = created["signing_secret"].as_str().unwrap().to_owned();
        assert!(first.starts_with("whsec_") && first.len() == 6 + 64, "{first}");
        let (status, deployment) = w
            .json("tom", "POST", &w.path(w.w.dev, "/stub/deployments"), Some(external("https://HOOKS.example.test/api")))
            .await;
        assert_eq!(status, 201, "{deployment}");
        assert_eq!(
            deployment["spec"],
            json!({"backend": "external", "url": "https://hooks.example.test/api", "health_path": "/healthz"})
        );
        assert!(deployment["bundle_sha256"].is_null());
        assert!(w.w.app.bundles.puts.lock().unwrap().is_empty(), "no bundle");
        assert!(w.published().iter().any(|(b, n)| *b == w.w.dev && n == "stub"));
        let (_, list) = w.json("dee", "GET", &w.path(w.w.dev, ""), None).await;
        let stub = list.as_array().unwrap().iter().find(|f| f["name"] == "stub").unwrap().clone();
        assert_eq!(stub["deployment_id"], deployment["id"]);
        assert!(stub["health"].is_null(), "not checked yet");
        assert!(stub.get("signing_secret").is_none(), "shown only once");
        let (status, rotated) = w
            .json("tom", "PATCH", &w.path(w.w.dev, "/stub"), Some(json!({"rotate_signing_secret": true})))
            .await;
        assert_eq!(status, 200, "{rotated}");
        let second = rotated["signing_secret"].as_str().unwrap();
        assert!(second.starts_with("whsec_") && second != first);
        let stored = w.w.metadata.signing_secrets.lock().unwrap().clone();
        assert_eq!(stored.values().cloned().collect::<Vec<_>>(), vec![second.to_owned()]);
    }

    /// AC: a Developer (or a project Admin) who isn't a tenant admin can't create a service,
    /// set or change its URL, or rotate its secret: 403, nothing written.
    #[tokio::test]
    async fn functions_service_urls_need_a_tenant_admin() {
        let w = service_world().await;
        for sub in ["dee", "ada"] {
            let (status, body) = w
                .json(sub, "PUT", &w.path(w.w.dev, &format!("/svc-{sub}")), Some(json!({"kind": "service"})))
                .await;
            assert_eq!(status, 403, "{sub}: {body}");
        }
        w.json("tom", "PUT", &w.path(w.w.dev, "/stub"), Some(json!({"kind": "service"}))).await;
        w.json("tom", "POST", &w.path(w.w.dev, "/stub/deployments"), Some(external("https://hooks.example.test/a")))
            .await;
        let before = w.w.metadata.signing_secrets.lock().unwrap().clone();
        for sub in ["dee", "ada"] {
            let (status, body) = w
                .json(sub, "POST", &w.path(w.w.dev, "/stub/deployments"), Some(external("https://hooks.example.test/b")))
                .await;
            assert_eq!(status, 403, "{sub}: {body}");
            let (status, body) = w
                .json(sub, "PATCH", &w.path(w.w.dev, "/stub"), Some(json!({"rotate_signing_secret": true})))
                .await;
            assert_eq!(status, 403, "{sub}: {body}");
            // A function made by a Developer can't become a service either.
            w.json("dee", "PUT", &w.path(w.w.dev, "/plain"), Some(json!({}))).await;
            let (status, _) = w
                .json(sub, "PUT", &w.path(w.w.dev, "/plain"), Some(json!({"kind": "service"})))
                .await;
            assert_eq!(status, 403, "{sub}");
        }
        let (_, deployments) = w.json("tom", "GET", &w.path(w.w.dev, "/stub/deployments"), None).await;
        assert_eq!(deployments.as_array().unwrap().len(), 1);
        assert_eq!(*w.w.metadata.signing_secrets.lock().unwrap(), before);
        // Another project's member still hears 404, as for a missing database.
        let (status, _) = w
            .json("out", "POST", &w.path(w.w.dev, "/stub/deployments"), Some(external("https://hooks.example.test/b")))
            .await;
        assert_eq!(status, 404);
    }

    /// AC: unsafe URLs and malformed specs are refused at save time (422).
    #[tokio::test]
    async fn functions_service_unsafe_urls_refused() {
        let w = service_world().await;
        w.json("tom", "PUT", &w.path(w.w.dev, "/stub"), Some(json!({"kind": "service"}))).await;
        for url in [
            "http://127.0.0.1",
            "http://[::1]",
            "http://169.254.169.254",
            "http://metadata-db:5432",
            "http://nats:4222",
            "file:///etc/passwd",
            "http://METADATA-DB./",
            "http://chelabase-metadata-db-1/",
            "https://private.example.test/",
            "https://nowhere.example.test/",
            "https://hooks.example.test/?q=1",
            "https://hooks.example.test/#f",
        ] {
            let (status, body) = w.json("tom", "POST", &w.path(w.w.dev, "/stub/deployments"), Some(external(url))).await;
            assert_eq!(status, 422, "{url}: {body}");
        }
        for spec in [
            json!({"backend": "image", "url": "https://hooks.example.test/", "health_path": "/h"}),
            json!({"backend": "external", "url": "https://hooks.example.test/", "health_path": "h"}),
            json!({"backend": "external", "url": "https://hooks.example.test/", "health_path": "/h#x"}),
            json!({"backend": "external", "url": "https://hooks.example.test/", "health_path": "/a b"}),
        ] {
            let (status, body) = w
                .json("tom", "POST", &w.path(w.w.dev, "/stub/deployments"), Some(json!({"spec": spec})))
                .await;
            assert_eq!(status, 422, "{spec}: {body}");
        }
        // A service takes no bundle, and a function no spec.
        let (status, _) = w.call("tom", "POST", &w.path(w.w.dev, "/stub/deployments"), Some(deploy_body("djE="))).await;
        assert_eq!(status, 422);
        w.json("tom", "PUT", &w.path(w.w.dev, "/plain"), Some(json!({}))).await;
        let (status, _) = w
            .json("tom", "POST", &w.path(w.w.dev, "/plain/deployments"), Some(external("https://hooks.example.test/")))
            .await;
        assert_eq!(status, 422);
        let (_, deployments) = w.json("tom", "GET", &w.path(w.w.dev, "/stub/deployments"), None).await;
        assert!(deployments.as_array().unwrap().is_empty());
    }
}
