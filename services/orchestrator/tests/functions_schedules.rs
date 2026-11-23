//! Edge functions m7 (#169): a function's schedules on a branch through the management API,
//! against the fake metadata, with m3's access rules (reads Viewer, writes `BRANCH`, 404 for
//! another database's branch or function, 403 in platform mode).
#![allow(dead_code, unused_imports)]
// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

include!("common/fakes.rs");

mod functions_schedules {
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

        /// Creates function `name` (no deployment needed for a schedule).
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

        /// A schedule of `name` on `branch`, made by `ada`; its id.
        async fn schedule(&self, branch: BranchId, name: &str) -> String {
            let (status, body) = self
                .call(
                    "ada",
                    "POST",
                    &self.path(branch, &format!("/{name}/schedules")),
                    Some(json!({"cron": "*/5 * * * *"}).to_string()),
                )
                .await;
            assert_eq!(status, 201, "{body}");
            body["id"].as_str().unwrap().to_string()
        }
    }

    /// Every route with its method, and whether it reads.
    fn routes(
        w: &World,
        branch: BranchId,
        id: &str,
    ) -> Vec<(&'static str, String, Option<String>, bool)> {
        vec![
            ("GET", w.path(branch, "/hello/schedules"), None, true),
            (
                "POST",
                w.path(branch, "/hello/schedules"),
                Some(json!({"cron": "0 * * * *"}).to_string()),
                false,
            ),
            (
                "PATCH",
                w.path(branch, &format!("/hello/schedules/{id}")),
                Some(json!({"enabled": false}).to_string()),
                false,
            ),
            (
                "DELETE",
                w.path(branch, &format!("/hello/schedules/{id}")),
                None,
                false,
            ),
        ]
    }

    /// Add, list, change and delete; the time zone defaults to UTC and the next run is set.
    #[tokio::test]
    async fn functions_schedules_happy_path() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/schedules");
        let (status, made) = w
            .call(
                "dee",
                "POST",
                &base,
                Some(json!({"cron": "0 9 * * MON-FRI", "timezone": "Europe/London"}).to_string()),
            )
            .await;
        assert_eq!(status, 201, "{made}");
        assert_eq!(made["cron"], "0 9 * * MON-FRI");
        assert_eq!(made["timezone"], "Europe/London");
        assert_eq!(made["enabled"], true);
        let next: Timestamp =
            serde_json::from_value(made["next_run_at"].clone()).unwrap();
        assert!(next > Timestamp::now());
        let id = made["id"].as_str().unwrap().to_string();

        let (status, utc) = w
            .call(
                "dee",
                "POST",
                &base,
                Some(json!({"cron": "* * * * *"}).to_string()),
            )
            .await;
        assert_eq!((status, utc["timezone"].as_str()), (201, Some("UTC")));

        let (status, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(status, 200);
        assert_eq!(list.as_array().unwrap().len(), 2);
        assert_eq!(list[0]["id"], made["id"]);

        let one = format!("{base}/{id}");
        let (status, off) = w
            .call(
                "dee",
                "PATCH",
                &one,
                Some(json!({"enabled": false}).to_string()),
            )
            .await;
        assert_eq!(status, 200, "{off}");
        assert_eq!(
            (off["enabled"].as_bool(), off["cron"].as_str()),
            (Some(false), Some("0 9 * * MON-FRI"))
        );
        let (status, moved) = w
            .call(
                "dee",
                "PATCH",
                &one,
                Some(json!({"cron": "30 6 * * *", "timezone": "UTC"}).to_string()),
            )
            .await;
        assert_eq!(status, 200, "{moved}");
        assert_eq!(
            (moved["cron"].as_str(), moved["timezone"].as_str()),
            (Some("30 6 * * *"), Some("UTC"))
        );

        let (status, _) = w.call("dee", "DELETE", &one, None).await;
        assert_eq!(status, 204);
        let (status, body) = w.call("dee", "DELETE", &one, None).await;
        assert_eq!(status, 404, "{body}");
        let (status, body) = w
            .call(
                "dee",
                "PATCH",
                &one,
                Some(json!({"enabled": true}).to_string()),
            )
            .await;
        assert_eq!(status, 404, "{body}");
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
    }

    /// An invalid cron or time zone is 422 naming the field, on create and on change.
    #[tokio::test]
    async fn functions_schedules_invalid_cron_422() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let id = w.schedule(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/schedules");
        for (method, path) in [("POST", base.clone()), ("PATCH", format!("{base}/{id}"))] {
            for (body, field) in [
                (json!({"cron": "61 * * * *"}), "cron"),
                (json!({"cron": "0 0 * * * *"}), "cron"),
                (
                    json!({"cron": "* * * * *", "timezone": "Mars/Olympus"}),
                    "timezone",
                ),
                (json!({"cron": "", "timezone": "UTC"}), "cron"),
            ] {
                let (status, err) = w.call("dee", method, &path, Some(body.to_string())).await;
                assert_eq!(status, 422, "{method} {body}: {err}");
                assert_eq!(err["error"]["field"], field, "{method} {body}: {err}");
                assert!(
                    err["error"]["message"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("{field}: ")),
                    "{err}"
                );
            }
        }
        let (status, _) = w.call("dee", "POST", &base, Some("{}".into())).await;
        assert_eq!(status, 422, "cron is required");
        let (status, _) = w
            .call("dee", "POST", &base, Some("{\"cron\": ".into()))
            .await;
        assert_eq!(status, 400);
        let (status, _) = w
            .call(
                "dee",
                "POST",
                &base,
                Some(json!({"cron": "* * * * *", "extra": 1}).to_string()),
            )
            .await;
        assert_eq!(status, 422, "unknown fields are refused");
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list.as_array().unwrap().len(), 1, "nothing was added");
    }

    /// Review fix: a service is never run on a schedule (only a function's bundle is), so a
    /// schedule for one is 422 with why, and nothing is added.
    #[tokio::test]
    async fn functions_schedules_on_a_service_422() {
        let w = world().await;
        // A service is a tenant admin's to make (m9).
        w.w.metadata
            .add_member(w.w.tenant, "tom", MemberStatus::Active, true);
        let (status, body) = w
            .call(
                "tom",
                "PUT",
                &w.path(w.w.dev, "/svc"),
                Some(json!({"kind": "service"}).to_string()),
            )
            .await;
        assert!(status == 201 || status == 200, "{status} {body}");
        let base = w.path(w.w.dev, "/svc/schedules");
        let (status, err) = w
            .call(
                "dee",
                "POST",
                &base,
                Some(json!({"cron": "* * * * *"}).to_string()),
            )
            .await;
        assert_eq!(status, 422, "{err}");
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("service"),
            "{err}"
        );
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert!(list.as_array().unwrap().is_empty(), "nothing was added");
    }

    /// A Viewer reads and is 403 on every write; another project's Developer, and a path
    /// naming another database's branch, are 404 everywhere.
    #[tokio::test]
    async fn functions_schedules_access() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let id = w.schedule(w.w.dev, "hello").await;
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
    }

    /// Sets `last_error` on every schedule of the fake, as a claim or the startup report would.
    fn set_last_error(w: &World, why: &str) {
        for s in w.w.metadata.functions.lock().unwrap().schedules.iter_mut() {
            s.last_error = Some(why.to_owned());
        }
    }

    /// m15: every schedule carries `last_error`, `null` for a healthy one, the reason once set,
    /// and a successful PATCH with a valid cron clears it.
    #[tokio::test]
    async fn schedules_carry_last_error() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/schedules");
        let (status, made) = w
            .call("dee", "POST", &base, Some(json!({"cron": "0 * * * *"}).to_string()))
            .await;
        assert_eq!(status, 201, "{made}");
        assert_eq!(made.get("last_error"), Some(&Value::Null), "create: {made}");
        let id = made["id"].as_str().unwrap().to_string();
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list[0].get("last_error"), Some(&Value::Null), "{list}");

        set_last_error(&w, "cron: not a 5-field cron (x)");
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list[0]["last_error"], "cron: not a 5-field cron (x)");

        let (status, off) = w
            .call("dee", "PATCH", &format!("{base}/{id}"), Some(json!({"cron": "5 * * * *"}).to_string()))
            .await;
        assert_eq!(status, 200, "{off}");
        assert_eq!(off.get("last_error"), Some(&Value::Null), "update: {off}");
    }

    /// m8: a stored schedule whose cron the parser now rejects: a PATCH that leaves the cron out
    /// is a 422 naming `cron` (never a 500), a valid cron repairs it, and GET lists it throughout.
    #[tokio::test]
    async fn patching_a_schedule_whose_cron_stopped_parsing_is_a_422_and_a_new_cron_repairs_it() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/schedules");
        let id = w.schedule(w.w.dev, "hello").await;
        for s in w.w.metadata.functions.lock().unwrap().schedules.iter_mut() {
            s.cron = "* * * * * *".to_owned();
        }
        let item = format!("{base}/{id}");
        for body in [json!({"enabled": true}), json!({"timezone": "Europe/Paris"})] {
            let (status, err) = w.call("dee", "PATCH", &item, Some(body.to_string())).await;
            assert_eq!(status, 422, "{body}: {err}");
            assert_eq!(err["error"]["field"], "cron", "{body}: {err}");
            let (status, list) = w.call("vic", "GET", &base, None).await;
            assert_eq!(status, 200, "{list}");
            assert_eq!(list[0]["cron"], "* * * * * *", "{list}");
        }
        let (status, fixed) = w
            .call("dee", "PATCH", &item, Some(json!({"cron": "5 * * * *"}).to_string()))
            .await;
        assert_eq!(status, 200, "{fixed}");
        assert_eq!(fixed["cron"], "5 * * * *");
    }

    /// m15: a body naming `last_error` is refused and the stored reason stays.
    #[tokio::test]
    async fn a_request_cannot_set_last_error() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let id = w.schedule(w.w.dev, "hello").await;
        let base = w.path(w.w.dev, "/hello/schedules");
        set_last_error(&w, "timezone: unknown IANA time zone \"X\"");
        for (method, path, body) in [
            ("POST", base.clone(), json!({"cron": "* * * * *", "last_error": "x"})),
            ("PATCH", format!("{base}/{id}"), json!({"enabled": false, "last_error": null})),
        ] {
            let (status, err) = w.call("dee", method, &path, Some(body.to_string())).await;
            assert_eq!(status, 422, "{method}: {err}");
        }
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list.as_array().unwrap().len(), 1, "nothing was added");
        assert_eq!(list[0]["last_error"], "timezone: unknown IANA time zone \"X\"");
        assert_eq!(list[0]["enabled"], true, "the refused PATCH changed nothing");
    }

    /// m15: a Viewer reads `last_error`.
    #[tokio::test]
    async fn a_viewer_sees_last_error() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        w.schedule(w.w.dev, "hello").await;
        set_last_error(&w, "no upcoming time");
        let (status, list) = w
            .call("vic", "GET", &w.path(w.w.dev, "/hello/schedules"), None)
            .await;
        assert_eq!(status, 200);
        assert_eq!(list[0]["last_error"], "no upcoming time");
    }

    /// m15: another database's branch is 404 and no error body carries the reason.
    #[tokio::test]
    async fn another_databases_schedule_is_404_without_last_error() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        let id = w.schedule(w.w.dev, "hello").await;
        set_last_error(&w, "cron: secret-reason");
        let mut other = sample_database(w.w.tenant);
        other.project_id = w.w.project;
        let other_main = other.main.id;
        w.w.metadata.insert_database(other);
        for (method, path, body, _) in routes(&w, other_main, &id) {
            let (status, text) = w.call("dee", method, &path, body).await;
            assert_eq!(status, 404, "{method} {path}: {text}");
            assert!(!text.to_string().contains("secret-reason") && text.get("last_error").is_none(), "{text}");
        }
    }

    /// A schedule is only reachable under its own function, and an unknown function is 404.
    #[tokio::test]
    async fn functions_schedules_of_another_function_404() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        w.function(w.w.dev, "other").await;
        let id = w.schedule(w.w.dev, "hello").await;
        for (method, body) in [
            ("PATCH", Some(json!({"enabled": false}).to_string())),
            ("DELETE", None),
        ] {
            let path = w.path(w.w.dev, &format!("/other/schedules/{id}"));
            let (status, text) = w.call("dee", method, &path, body).await;
            assert_eq!(status, 404, "{method}: {text}");
        }
        let (_, list) = w
            .call("dee", "GET", &w.path(w.w.dev, "/other/schedules"), None)
            .await;
        assert_eq!(list, json!([]));
        let (status, _) = w
            .call("dee", "GET", &w.path(w.w.dev, "/missing/schedules"), None)
            .await;
        assert_eq!(status, 404);
        let (status, _) = w
            .call(
                "dee",
                "POST",
                &w.path(w.w.dev, "/missing/schedules"),
                Some(json!({"cron": "* * * * *"}).to_string()),
            )
            .await;
        assert_eq!(status, 404);
    }

    /// On a protected branch a Developer's writes are 403 and an Admin's go through.
    #[tokio::test]
    async fn functions_schedules_protected_branch() {
        let w = world().await;
        w.function(w.main, "hello").await;
        let id = w.schedule(w.main, "hello").await;
        w.w.metadata
            .update_normal_branch(w.main, |b| b.protected = true)
            .unwrap();
        for (method, path, body, read) in routes(&w, w.main, &id) {
            if read {
                continue;
            }
            let (status, text) = w.call("dee", method, &path, body.clone()).await;
            assert_eq!(status, 403, "dee {method}: {text}");
        }
        for (method, path, body, read) in routes(&w, w.main, &id) {
            if read {
                continue;
            }
            let (status, text) = w.call("ada", method, &path, body).await;
            assert!(
                (200..300).contains(&status),
                "ada {method}: {status} {text}"
            );
        }
    }

    #[tokio::test]
    async fn functions_schedules_platform_mode_403() {
        let w = world_in(Mode::Platform).await;
        let id = uuid::Uuid::new_v4().to_string();
        for (method, path, body, _) in routes(&w, w.w.dev, &id) {
            let (status, text) = w.call("ada", method, &path, body).await;
            assert_eq!(status, 403, "{method} {path}: {text}");
            assert!(
                text.to_string().contains("not_available_in_platform_mode"),
                "{text}"
            );
        }
    }

    /// A function has at most 20 schedules on a branch: the 21st is a 409 `conflict` and
    /// nothing is written; another function on the branch can still add its own.
    #[tokio::test]
    async fn functions_schedules_cap_409() {
        let w = world().await;
        w.function(w.w.dev, "hello").await;
        w.function(w.w.dev, "other").await;
        for _ in 0..20 {
            w.schedule(w.w.dev, "hello").await;
        }
        let base = w.path(w.w.dev, "/hello/schedules");
        let (status, err) = w
            .call("dee", "POST", &base, Some(json!({"cron": "0 * * * *"}).to_string()))
            .await;
        assert_eq!(status, 409, "{err}");
        assert_eq!(err["error"]["code"], "conflict", "{err}");
        let (_, list) = w.call("vic", "GET", &base, None).await;
        assert_eq!(list.as_array().unwrap().len(), 20, "nothing was written");
        w.schedule(w.w.dev, "other").await;
    }
}
