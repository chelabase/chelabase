//! Edge functions m9: external services over HTTP. A fake directory names each service's base
//! URL; a local upstream records what it was sent; the services' client resolves `svc.test`
//! to it through a fake resolver (allowlisted), and `inside.test` to an address in a denied
//! network.

// Test code: plain clients against local servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod services {
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::Request;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use chelabase_auth::internal::{
        AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    };
    use chelabase_http::outbound::{HostLookup, Outbound, Url};
    use chelabase_metadata::{FunctionAuth, ServiceTarget};
    use chelabase_storage::MemoryStore;
    use chelabase_types::data_key::DataKeyKind;
    use functions::app::{AppState, public_router};
    use functions::bundles::Bundles;
    use functions::directory::{Directory, External, Parts};
    use functions::host::Workerd;
    use functions::resolver::{Resolver, TTL};
    use functions::services::health::check_all;
    use functions::services::signature::{Rejected, Verifier};
    use functions::services::{Services, outbound_from_lookup};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;
    use zeroize::Zeroizing;

    const SECRET: &str = "whsec_services_test_secret";

    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        uri: String,
        headers: HeaderMap,
        body: Bytes,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers.get(name).and_then(|v| v.to_str().ok())
        }
    }

    struct Upstream {
        port: u16,
        seen: Arc<Mutex<Vec<Seen>>>,
        healthy: Arc<AtomicBool>,
    }

    /// The service: `/healthz` answers as `healthy` says, `/redirect` a 302 to `inner`,
    /// `/stream` four chunks 300 ms apart, anything else 200 `ok`.
    async fn upstream(inner: u16) -> Upstream {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let healthy = Arc::new(AtomicBool::new(true));
        let (rec, ok) = (seen.clone(), healthy.clone());
        let app = Router::new().fallback(move |req: Request| {
            let (rec, ok) = (rec.clone(), ok.clone());
            async move {
                let (parts, body) = req.into_parts();
                let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let path = parts.uri.path().to_owned();
                if path.ends_with("/healthz") {
                    return if ok.load(Ordering::SeqCst) {
                        StatusCode::OK
                    } else {
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                    .into_response();
                }
                rec.lock().unwrap().push(Seen {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body,
                });
                match path.as_str() {
                    "/redirect" => (
                        StatusCode::FOUND,
                        [("location", format!("http://127.0.0.1:{inner}/"))],
                    )
                        .into_response(),
                    "/stream" => {
                        let chunks = futures_util::stream::unfold(0, |i| async move {
                            if i == 4 {
                                return None;
                            }
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            Some((Ok::<_, std::io::Error>(Bytes::from(format!("{i}"))), i + 1))
                        });
                        Body::from_stream(chunks).into_response()
                    }
                    _ => ([("x-upstream", "1"), ("chelabase-leak", "1")], "ok").into_response(),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Upstream {
            port,
            seen,
            healthy,
        }
    }

    /// `svc.test` and `inside.test` resolve to `127.0.0.1` and `10.0.0.5`.
    struct Dns;
    impl HostLookup for Dns {
        fn lookup<'a>(
            &'a self,
            host: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::io::Result<Vec<IpAddr>>> + Send + 'a>,
        > {
            let out = match host {
                "svc.test" => Ok(vec!["127.0.0.1".parse().unwrap()]),
                "inside.test" => Ok(vec!["10.0.0.5".parse().unwrap()]),
                "lan.test" => Ok(vec!["192.168.1.5".parse().unwrap()]),
                _ => Err(std::io::Error::other("no such host")),
            };
            Box::pin(std::future::ready(out))
        }
    }

    /// name → (base URL, auth, enabled), every one on the harness's branch.
    #[derive(Default)]
    struct Fake {
        services: Mutex<HashMap<String, (String, FunctionAuth, bool)>>,
        health: Mutex<Vec<(Uuid, bool)>>,
    }

    fn deployment(name: &str) -> Uuid {
        use sha2::Digest as _;
        Uuid::from_slice(&sha2::Sha256::digest(name.as_bytes())[..16]).unwrap()
    }

    #[async_trait::async_trait]
    impl Directory for Fake {
        async fn resolve(&self, _branch: Uuid, name: &str) -> anyhow::Result<Option<Parts>> {
            if name == "hello" {
                // A function: no service target.
                return Ok(Some(Parts {
                    database_id: Uuid::nil(),
                    function_id: Uuid::nil(),
                    bundle_sha256: "b".repeat(64),
                    auth: FunctionAuth::Key,
                    enabled: true,
                    secrets: Vec::new(),
                    service_key: None,
                    service: None,
                }));
            }
            let Some((url, auth, enabled)) = self.services.lock().unwrap().get(name).cloned()
            else {
                return Ok(None);
            };
            Ok(Some(Parts {
                database_id: Uuid::nil(),
                function_id: deployment(name),
                bundle_sha256: String::new(),
                auth,
                enabled,
                secrets: Vec::new(),
                service_key: None,
                service: Some(External {
                    deployment_id: deployment(name),
                    url: Url::parse(&url).unwrap(),
                    secret: Some(Zeroizing::new(SECRET.into())),
                }),
            }))
        }
        async fn record_error(&self, _: Uuid, _: Uuid, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn database_live(&self, _: Uuid) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn service_targets(&self) -> anyhow::Result<Vec<ServiceTarget>> {
            Ok(self
                .services
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, (_, _, on))| *on)
                .map(|(name, (url, _, _))| ServiceTarget {
                    deployment_id: deployment(name),
                    spec: json!({"backend": "external", "url": url, "health_path": "/healthz"}),
                })
                .collect())
        }
        async fn record_health(&self, deployment: Uuid, healthy: bool) -> anyhow::Result<()> {
            self.health.lock().unwrap().push((deployment, healthy));
            Ok(())
        }
    }

    struct H {
        public: String,
        addr: String,
        up: Upstream,
        inner_hits: Arc<AtomicUsize>,
        dir: Arc<Fake>,
        services: (
            chelabase_http::outbound::PublicClient,
            Arc<functions::services::health::Health>,
        ),
        signer: InternalSigner,
        branch: Uuid,
        http: chelabase_http::Client,
    }

    async fn harness() -> H {
        let inner = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let inner_port = inner.local_addr().unwrap().port();
        let inner_hits = Arc::new(AtomicUsize::new(0));
        let hits = inner_hits.clone();
        tokio::spawn(async move {
            while let Ok((_s, _)) = inner.accept().await {
                hits.fetch_add(1, Ordering::SeqCst);
            }
        });
        let up = upstream(inner_port).await;
        let dir = Arc::new(Fake::default());
        let base = format!("http://svc.test:{}", up.port);
        for (name, url, auth, enabled) in [
            ("stub", base.clone(), FunctionAuth::Key, true),
            ("pre", format!("{base}/api"), FunctionAuth::Key, true),
            ("users", base.clone(), FunctionAuth::AppUser, true),
            ("off", base.clone(), FunctionAuth::Key, false),
            (
                "lan",
                format!("http://lan.test:{}", up.port),
                FunctionAuth::Key,
                true,
            ),
            (
                "ghost",
                format!("http://ghost.test:{}", up.port),
                FunctionAuth::Key,
                true,
            ),
            (
                "inside",
                format!("http://inside.test:{}", up.port),
                FunctionAuth::Key,
                true,
            ),
        ] {
            dir.services
                .lock()
                .unwrap()
                .insert(name.into(), (url, auth, enabled));
        }
        let outbound = Outbound::new()
            .allow_http(true)
            .allow_hosts(["svc.test", "inside.test"])
            .deny_nets(["10.0.0.0/8".parse().unwrap()])
            .with_lookup(Arc::new(Dns));
        let services = Services::new(&outbound).unwrap();
        let handles = (services.client.clone(), services.health.clone());
        let (private, public) = chelabase_auth::internal::generate_keypair_pem();
        let resolver = Arc::new(Resolver::new(
            dir.clone(),
            "https://api.example".into(),
            TTL,
        ));
        let state = AppState::new(
            resolver,
            Arc::new(Workerd::new("http://127.0.0.1:9", Duration::from_millis(500)).unwrap()),
            InternalVerifier::from_pem(public.as_bytes()).unwrap(),
            Bundles::new(Arc::new(MemoryStore::default())),
            "orchestrator-bearer-0123456789abcdef0123".into(),
            "worker-bearer-0123456789abcdef0123456789".into(),
        )
        .with_services(services);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let app = public_router(Arc::new(state));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        H {
            public: format!("http://{addr}"),
            addr,
            up,
            inner_hits,
            dir,
            services: handles,
            signer: InternalSigner::from_pem(private.as_bytes()).unwrap(),
            branch: Uuid::new_v4(),
            http: chelabase_http::client_builder().build().unwrap(),
        }
    }

    #[derive(Clone, Copy)]
    enum Cred {
        Public,
        User,
    }

    impl H {
        fn identity(&self, cred: Cred) -> String {
            let (role, claims) = match cred {
                Cred::Public => ("anon", json!({})),
                Cred::User => (
                    "authenticated",
                    json!({"sub": "user-1", "role": "authenticated"}),
                ),
            };
            let identity = Identity {
                sub: Uuid::new_v4().to_string(),
                auth: AuthMethod::DataKey,
                request_id: "rid".into(),
                data: Some(DataIdentity {
                    branch_id: self.branch,
                    key_id: Uuid::new_v4(),
                    kind: DataKeyKind::Public,
                    role: role.into(),
                    claims,
                    databases: None,
                    project_id: Some(Uuid::new_v4()),
                    role_oid: None,
                    server_vouched: false,
                }),
                email: None,
                name: None,
            };
            self.signer.sign("functions", &identity).unwrap()
        }

        fn call(&self, method: reqwest::Method, path: &str, cred: Cred) -> reqwest::RequestBuilder {
            self.http
                .request(method, format!("{}{path}", self.public))
                .header(IDENTITY_HEADER, self.identity(cred))
        }

        fn last(&self) -> Seen {
            self.up
                .seen
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("the service was called")
        }

        fn calls(&self) -> usize {
            self.up.seen.lock().unwrap().len()
        }

        /// A request line sent as it is (a client would normalize its dot segments).
        async fn raw_get(&self, path: &str) -> u16 {
            let mut sock = tokio::net::TcpStream::connect(&self.addr).await.unwrap();
            let req = format!(
                "GET {path} HTTP/1.1\r\nhost: x\r\n{IDENTITY_HEADER}: {}\r\nconnection: close\r\n\r\n",
                self.identity(Cred::Public)
            );
            sock.write_all(req.as_bytes()).await.unwrap();
            let mut out = String::new();
            sock.read_to_string(&mut out).await.unwrap();
            out.split(' ').nth(1).unwrap().parse().unwrap()
        }
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// AC 1 and the forged-identity case: the service gets the rest and the query, the
    /// gateway's identity (never the client's `chelabase-*`), a fresh request id and a
    /// signature its verifier accepts once; the answer streams back without our headers.
    #[tokio::test]
    async fn a_call_reaches_the_service_signed_with_the_gateways_identity() {
        let h = harness().await;
        let resp = h
            .call(
                reqwest::Method::POST,
                "/services/v1/users/a/b?x=1",
                Cred::User,
            )
            .header("chelabase-user", "admin")
            .header("chelabase-role", "service_role")
            .header("chelabase-branch", Uuid::new_v4().to_string())
            .header("content-digest", "sha-256=:forged:")
            .header("authorization", "Bearer chk_pub_key")
            .header("x-other", "kept")
            .header("connection", "x-hop")
            .header("x-hop", "dropped")
            .body("payload")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers().get("x-upstream").unwrap(), "1");
        assert!(resp.headers().get("chelabase-leak").is_none());
        assert_eq!(resp.text().await.unwrap(), "ok");
        let seen = h.last();
        assert_eq!(
            (seen.method.as_str(), seen.uri.as_str(), &seen.body[..]),
            ("POST", "/a/b?x=1", &b"payload"[..])
        );
        assert_eq!(seen.header("chelabase-user"), Some("user-1"));
        assert_eq!(seen.header("chelabase-role"), Some("authenticated"));
        assert_eq!(
            seen.header("chelabase-branch"),
            Some(h.branch.to_string().as_str())
        );
        assert!(Uuid::parse_str(seen.header("chelabase-request-id").unwrap()).is_ok());
        assert_eq!(seen.header("x-other"), Some("kept"));
        for gone in ["authorization", IDENTITY_HEADER, "x-hop", "chelabase-body"] {
            assert_eq!(seen.header(gone), None, "{gone}");
        }
        assert_ne!(seen.header("content-digest"), Some("sha-256=:forged:"));
        let v = Verifier::new(SECRET);
        assert_eq!(
            v.verify("POST", &seen.uri, &seen.headers, &seen.body, now()),
            Ok(())
        );
        // AC 9: replayed as it was, or with another body.
        assert_eq!(
            v.verify("POST", &seen.uri, &seen.headers, &seen.body, now()),
            Err(Rejected::Replayed)
        );
        assert_eq!(
            Verifier::new(SECRET).verify("POST", &seen.uri, &seen.headers, b"other", now()),
            Err(Rejected::Digest)
        );

        // A public key: no user, whatever the client claims.
        let resp = h
            .call(reqwest::Method::GET, "/services/v1/stub", Cred::Public)
            .header("chelabase-user", "admin")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let seen = h.last();
        assert_eq!(
            (
                seen.uri.as_str(),
                seen.header("chelabase-user"),
                seen.header("chelabase-role")
            ),
            ("/", Some(""), Some("anon"))
        );
        assert_eq!(
            Verifier::new(SECRET).verify("GET", "/", &seen.headers, b"", now()),
            Ok(())
        );
    }

    /// AC 10: with base path `/api`, dot segments in any spelling stay under it, and the
    /// signed request-target is what the service received.
    #[tokio::test]
    async fn the_path_never_leaves_the_base_prefix_and_is_signed_as_sent() {
        let h = harness().await;
        for (path, want) in [
            (
                "/services/v1/pre/x/%2e%2e/%2e%2e/secret?q=1",
                "/api/secret?q=1",
            ),
            ("/services/v1/pre/../../secret", "/api/secret"),
            ("/services/v1/pre/a/./%2E./.%2e/b", "/api/b"),
            ("/services/v1/pre/a%20b", "/api/a%20b"),
        ] {
            assert_eq!(h.raw_get(path).await, 200, "{path}");
            let seen = h.last();
            assert_eq!(seen.uri, want, "{path}");
            assert_eq!(
                Verifier::new(SECRET).verify("GET", &seen.uri, &seen.headers, b"", now()),
                Ok(()),
                "{path}"
            );
        }
        // Audit B2: an encoded slash or backslash could leave `/api` at an upstream that
        // decodes first: 400, nothing sent.
        let before = h.calls();
        for path in [
            "/services/v1/pre/%2e%2e%2fadmin",
            "/services/v1/pre/..%2Fadmin",
            "/services/v1/pre/..%5cadmin",
            "/services/v1/pre/a%2fb",
        ] {
            assert_eq!(h.raw_get(path).await, 400, "{path}");
        }
        assert_eq!(h.calls(), before);
    }

    /// AC 5: a redirect comes back to the client; nothing follows it.
    #[tokio::test]
    async fn a_redirect_comes_back_unfollowed() {
        let h = harness().await;
        let resp = h
            .call(
                reqwest::Method::GET,
                "/services/v1/stub/redirect",
                Cred::Public,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 302);
        assert!(
            resp.headers()
                .get("location")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("http://127.0.0.1:")
        );
        assert_eq!(h.inner_hits.load(Ordering::SeqCst), 0);
    }

    /// AC 6: three failed checks make calls answer 503 `service_unhealthy`, and the change is
    /// recorded; a passing check brings it back.
    #[tokio::test]
    async fn failed_health_checks_make_calls_answer_503() {
        let h = harness().await;
        let (client, health) = &h.services;
        let status = || async {
            h.call(reqwest::Method::GET, "/services/v1/stub", Cred::Public)
                .send()
                .await
                .unwrap()
        };
        h.up.healthy.store(false, Ordering::SeqCst);
        for _ in 0..2 {
            check_all(h.dir.as_ref(), client, health).await;
        }
        assert_eq!(status().await.status(), 200, "two failures aren't enough");
        check_all(h.dir.as_ref(), client, health).await;
        let resp = status().await;
        assert_eq!(resp.status(), 503);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
            "service_unhealthy"
        );
        assert!(
            h.dir
                .health
                .lock()
                .unwrap()
                .contains(&(deployment("stub"), false))
        );
        h.up.healthy.store(true, Ordering::SeqCst);
        check_all(h.dir.as_ref(), client, health).await;
        assert_eq!(status().await.status(), 200);
        assert!(
            h.dir
                .health
                .lock()
                .unwrap()
                .contains(&(deployment("stub"), true))
        );
        // Audit N1: every decided check is recorded (its time moves), not only a change, so a
        // restarted or re-enabled service's record is fresh after its first decided check.
        let passes = |h: &H| {
            h.dir
                .health
                .lock()
                .unwrap()
                .iter()
                .filter(|r| **r == (deployment("stub"), true))
                .count()
        };
        let before = passes(&h);
        check_all(h.dir.as_ref(), client, health).await;
        assert_eq!(
            passes(&h),
            before + 1,
            "a pass with no change is recorded too"
        );
    }

    /// Disabled is 503 `service_disabled`; a function isn't a service and a service isn't a
    /// function; no key is 401; an app-user service refuses a bare public key.
    #[tokio::test]
    async fn disabled_unknown_and_wrong_kind_calls_are_refused() {
        let h = harness().await;
        let code = |resp: reqwest::Response| async move {
            let status = resp.status().as_u16();
            (
                status,
                resp.json::<serde_json::Value>().await.unwrap()["error"]["code"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
        };
        let get = |path: &'static str, cred| h.call(reqwest::Method::GET, path, cred).send();
        assert_eq!(
            code(get("/services/v1/off", Cred::Public).await.unwrap()).await,
            (503, "service_disabled".into())
        );
        assert_eq!(
            code(get("/services/v1/hello", Cred::Public).await.unwrap()).await,
            (404, "service_not_found".into())
        );
        assert_eq!(
            code(get("/services/v1/nope/x", Cred::Public).await.unwrap()).await,
            (404, "service_not_found".into())
        );
        assert_eq!(
            code(get("/functions/v1/stub", Cred::Public).await.unwrap()).await,
            (404, "function_not_found".into())
        );
        assert_eq!(
            code(get("/services/v1/users", Cred::Public).await.unwrap()).await,
            (401, "unauthorized".into())
        );
        let bare = h
            .http
            .get(format!("{}/services/v1/stub", h.public))
            .send()
            .await
            .unwrap();
        assert_eq!(code(bare).await, (401, "unauthorized".into()));
        assert_eq!(h.calls(), 0);
    }

    /// AC 4 and 8, wired: an allowlisted name resolving into a denied network is refused at
    /// connect time; nothing is sent.
    #[tokio::test]
    async fn an_allowlisted_name_resolving_into_the_stack_is_refused() {
        let h = harness().await;
        let resp = h
            .call(reqwest::Method::GET, "/services/v1/inside/x", Cred::Public)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
            "service_refused"
        );
        assert_eq!(h.calls(), 0);
    }

    /// A body over 1 MiB streams through marked `chelabase-body: unsigned`; the signature
    /// then covers no digest.
    #[tokio::test]
    async fn a_large_body_streams_unsigned() {
        let h = harness().await;
        let big = vec![b'x'; 2 * 1024 * 1024 + 7];
        let resp = h
            .call(reqwest::Method::PUT, "/services/v1/stub/up", Cred::Public)
            .body(big.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let seen = h.last();
        assert_eq!(seen.body.len(), big.len());
        assert_eq!(seen.header("chelabase-body"), Some("unsigned"));
        assert_eq!(seen.header("content-digest"), None);
        assert_eq!(
            Verifier::new(SECRET).verify("PUT", "/up", &seen.headers, &seen.body, now()),
            Ok(())
        );
    }

    /// The answer streams: its chunks arrive as the service sends them.
    #[tokio::test]
    async fn a_streamed_answer_arrives_whole() {
        let h = harness().await;
        let resp = h
            .call(
                reqwest::Method::GET,
                "/services/v1/stub/stream",
                Cred::Public,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "0123");
    }

    /// AC 11's second half and the hard deny's wiring: the webhooks' settings never apply to
    /// services, and the stack's names are refused even when allowlisted.
    #[tokio::test]
    async fn the_services_settings_are_their_own() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        let o = outbound_from_lookup(env(&[
            ("CHELABASE_WEBHOOKS_ALLOW_HOSTS", "hooks.internal"),
            ("CHELABASE_WEBHOOKS_ALLOW_HTTP", "true"),
        ]))
        .unwrap();
        assert!(
            o.check_url("http://hooks.internal/").is_err(),
            "http stays refused"
        );
        assert!(o.check_url("https://10.0.0.1/").is_err());
        let o = outbound_from_lookup(env(&[
            (
                "CHELABASE_SERVICES_ALLOW_HOSTS",
                "metadata-db,host.docker.internal,172.30.0.9",
            ),
            ("CHELABASE_SERVICES_ALLOW_HTTP", "true"),
            ("CHELABASE_INTERNAL_SUBNETS", "172.30.0.0/16"),
        ]))
        .unwrap();
        assert!(o.check_url("http://host.docker.internal:18099/").is_ok());
        for url in [
            "http://127.0.0.1",
            "http://[::1]",
            "http://169.254.169.254",
            "http://metadata-db:5432",
            "http://nats:4222",
            "file:///etc/passwd",
            "http://METADATA-DB./",
            "http://chelabase-metadata-db-1/",
            "http://172.30.0.9/",
        ] {
            assert!(o.check_url(url).is_err(), "{url}");
        }
        assert!(outbound_from_lookup(env(&[("CHELABASE_INTERNAL_SUBNETS", "nope")])).is_err());
    }

    /// Audit S2: the proxy's other refusals: a host resolving to a private address that isn't
    /// allowlisted, and one that doesn't resolve. 502, nothing sent.
    #[tokio::test]
    async fn every_connect_time_refusal_answers_502() {
        let h = harness().await;
        for (name, code) in [("lan", "service_refused"), ("ghost", "service_refused")] {
            let resp = h
                .call(
                    reqwest::Method::GET,
                    &format!("/services/v1/{name}/x"),
                    Cred::Public,
                )
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 502, "{name}");
            assert_eq!(
                resp.json::<serde_json::Value>().await.unwrap()["error"]["code"],
                code,
                "{name}"
            );
        }
        assert_eq!(h.calls(), 0);
    }
}
