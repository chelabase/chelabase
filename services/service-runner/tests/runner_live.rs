//! Edge functions m10, live: the runner against the stack's Docker engine. Each test labels its
//! containers with its own throwaway stack (`m10t-<hex>`), so the workbench's own containers
//! and other stacks are never touched; images, containers and networks it makes are removed at
//! the end. The test image is busybox (already on the host) committed with an httpd `CMD` and a
//! `VOLUME`, no `USER`.
//!
//! ```text
//! just workbench test -p chelabase-service-runner --test runner_live -- --run-ignored only
//! ```
//!
//! `service_containers_reach_only_the_public_listener` needs the stack recreated with this
//! milestone's compose files (the `services` network and the functions service on it).

use std::collections::HashMap;
use std::sync::Arc;

use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig, NetworkCreateRequest};
use bollard::query_parameters::{
    CommitContainerOptions, CreateContainerOptions, ListContainersOptions, RemoveContainerOptions,
    RemoveImageOptions,
};
use futures_util::StreamExt as _;
use serde_json::json;
use service_runner::app::{AppState, router};
use service_runner::backend::ContainerBackend;
use service_runner::docker::DockerBackend;
use service_runner::reconcile::{Action, Desired, DesiredSource, MetadataDesired, reconcile_once};
use service_runner::runner::{EnsureRequest, Key, Runner};
use service_runner::spec::Caps;
use service_runner::{BRANCH_LABEL, DEPLOYMENT_LABEL, SERVICE_LABEL, STACK_LABEL};
use uuid::Uuid;

fn hex() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn docker() -> Docker {
    Docker::connect_with_socket_defaults().expect("the Docker socket (run in the workbench runner)")
}

/// A throwaway stack: its label, its own (non-internal) network, and the test image.
struct Fixture {
    docker: Docker,
    stack: String,
    network: String,
    image: String,
}

impl Fixture {
    async fn new() -> Fixture {
        let docker = docker();
        let stack = format!("m10t-{}", hex());
        let network = format!("{stack}_services");
        docker
            .create_network(NetworkCreateRequest {
                name: network.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        // busybox + httpd on 8080 + VOLUME /data, no USER.
        let seed = docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(format!("{stack}-seed")),
                    ..Default::default()
                }),
                ContainerCreateBody {
                    image: Some("busybox:latest".into()),
                    cmd: Some(vec!["true".into()]),
                    ..Default::default()
                },
            )
            .await
            .expect("busybox:latest on the host");
        let image = format!("m10t-echo:{}", hex());
        let (repo, tag) = image.split_once(':').unwrap();
        docker
            .commit_container(
                CommitContainerOptions {
                    container: Some(seed.id.clone()),
                    repo: Some(repo.into()),
                    tag: Some(tag.into()),
                    changes: Some(
                        "CMD [\"httpd\", \"-f\", \"-p\", \"8080\", \"-h\", \"/bin\"]\nVOLUME /data"
                            .into(),
                    ),
                    ..Default::default()
                },
                bollard::models::ContainerConfig::default(),
            )
            .await
            .unwrap();
        remove(&docker, &seed.id).await;
        Fixture {
            docker,
            stack,
            network,
            image,
        }
    }

    fn runner(&self) -> Runner {
        Runner::new(
            Arc::new(DockerBackend::connect().unwrap()),
            &self.stack,
            &self.network,
            Caps::default(),
        )
    }

    fn spec(&self) -> serde_json::Value {
        json!({"backend": "container", "image": self.image, "port": 8080, "health_path": "/",
               "resources": {"memory_mb": 128, "cpus": 0.5}})
    }

    /// A container that sleeps, with `labels` (none: like any compose service).
    async fn plain(&self, name: &str, labels: &[(&str, &str)]) -> String {
        let c = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(format!("{}-{name}", self.stack)),
                    ..Default::default()
                }),
                ContainerCreateBody {
                    image: Some("busybox:latest".into()),
                    cmd: Some(vec!["sleep".into(), "600".into()]),
                    labels: Some(
                        labels
                            .iter()
                            .map(|(k, v)| (k.to_string(), v.to_string()))
                            .collect(),
                    ),
                    host_config: Some(HostConfig {
                        network_mode: Some("none".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        self.docker.start_container(&c.id, None).await.unwrap();
        c.id
    }

    async fn state(&self, id: &str) -> Option<String> {
        DockerBackend::connect()
            .unwrap()
            .inspect(id)
            .await
            .unwrap()
            .map(|c| c.state)
    }

    /// Every container carrying `stack`'s label, by name prefix too (the plain ones).
    async fn finish(self) {
        let all = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                ..Default::default()
            }))
            .await
            .unwrap();
        for c in all {
            let named = c
                .names
                .unwrap_or_default()
                .iter()
                .any(|n| n.contains(&self.stack));
            let labelled = c
                .labels
                .unwrap_or_default()
                .get(STACK_LABEL)
                .is_some_and(|s| s.starts_with(&self.stack));
            if named || labelled {
                remove(&self.docker, c.id.as_deref().unwrap()).await;
            }
        }
        self.docker.remove_network(&self.network).await.ok();
        self.docker
            .remove_image(
                &self.image,
                Some(RemoveImageOptions {
                    force: true,
                    ..Default::default()
                }),
                None,
            )
            .await
            .ok();
    }
}

async fn remove(docker: &Docker, id: &str) {
    docker
        .remove_container(
            id,
            Some(RemoveContainerOptions {
                force: true,
                v: true,
                ..Default::default()
            }),
        )
        .await
        .ok();
}

/// Runs `cmd` in the container; `(exit code, output)`.
async fn exec(docker: &Docker, id: &str, cmd: &[&str]) -> (i64, String) {
    let e = docker
        .create_exec(
            id,
            CreateExecOptions {
                cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut out = String::new();
    if let StartExecResults::Attached { mut output, .. } =
        docker.start_exec(&e.id, None).await.unwrap()
    {
        while let Some(Ok(chunk)) = output.next().await {
            out.push_str(&chunk.to_string());
        }
    }
    let code = docker
        .inspect_exec(&e.id)
        .await
        .unwrap()
        .exit_code
        .unwrap_or(-1);
    (code, out)
}

fn key() -> Key {
    Key {
        service_id: Uuid::new_v4(),
        branch_id: Uuid::new_v4(),
        deployment_id: Uuid::new_v4(),
    }
}

fn request(key: &Key, spec: serde_json::Value) -> EnsureRequest {
    EnsureRequest {
        service_id: key.service_id,
        branch_id: key.branch_id,
        deployment_id: key.deployment_id,
        spec,
        env: [("GREETING".to_string(), "hello".to_string())].into(),
    }
}

/// Ensure, then `docker inspect` shows every hardening setting; inside, the process is not
/// root, the root is read-only and `/tmp` and the image volume are tmpfs. Stop and remove.
#[tokio::test]
#[ignore = "needs the workbench stack's Docker engine"]
async fn ensure_stop_remove_and_hardening() {
    let f = Fixture::new().await;
    let runner = f.runner();
    let k = key();
    let c = runner.ensure(&request(&k, f.spec())).await.unwrap();
    assert!(c.running(), "{c:?}");
    let i = f.docker.inspect_container(&c.id, None).await.unwrap();
    let h = i.host_config.clone().unwrap();
    assert_eq!(h.privileged, Some(false));
    assert_eq!(h.readonly_rootfs, Some(true));
    assert_eq!(h.cap_drop, Some(vec!["ALL".to_string()]));
    assert!(h.cap_add.unwrap_or_default().is_empty());
    assert_eq!(
        h.security_opt,
        Some(vec!["no-new-privileges:true".to_string()])
    );
    assert_eq!(h.memory, Some(128 * 1024 * 1024));
    assert_eq!(h.memory_swap, Some(128 * 1024 * 1024));
    assert_eq!(h.nano_cpus, Some(500_000_000));
    assert_eq!(h.pids_limit, Some(256));
    assert_eq!(h.network_mode.as_deref(), Some(f.network.as_str()));
    assert!(h.binds.unwrap_or_default().is_empty());
    assert!(h.devices.unwrap_or_default().is_empty());
    assert_eq!(h.privileged, Some(false));
    assert!(
        h.pid_mode.unwrap_or_default().is_empty() && h.userns_mode.unwrap_or_default().is_empty()
    );
    assert_eq!(h.ipc_mode.as_deref(), Some("private"));
    assert!(h.port_bindings.unwrap_or_default().is_empty());
    let config = i.config.clone().unwrap();
    assert_eq!(config.user.as_deref(), Some("65532:65532"));
    let env = config.env.unwrap_or_default();
    assert!(env.contains(&"GREETING=hello".to_string()) && env.contains(&"PORT=8080".to_string()));
    let labels = config.labels.unwrap_or_default();
    assert_eq!(labels[STACK_LABEL], f.stack);
    assert_eq!(labels[SERVICE_LABEL], k.service_id.to_string());
    // No host mount of any kind: /tmp and the image's VOLUME are tmpfs, no anonymous volume.
    let mounts = i.mounts.clone().unwrap_or_default();
    assert!(
        mounts.iter().all(|m| m.typ.as_deref() == Some("tmpfs")),
        "{mounts:?}"
    );
    let networks: Vec<String> = i
        .network_settings
        .and_then(|n| n.networks)
        .unwrap_or_default()
        .into_keys()
        .collect();
    assert_eq!(networks, std::slice::from_ref(&f.network));
    let (code, out) = exec(&f.docker, &c.id, &["id", "-u"]).await;
    assert_eq!((code, out.trim()), (0, "65532"));
    let (code, _) = exec(&f.docker, &c.id, &["touch", "/root-write"]).await;
    assert_ne!(code, 0, "the root filesystem is read-only");
    for dir in ["/tmp", "/data"] {
        let (code, out) = exec(&f.docker, &c.id, &["touch", &format!("{dir}/x")]).await;
        assert_eq!(code, 0, "{dir} is writable: {out}");
    }
    let (_, mounts) = exec(&f.docker, &c.id, &["cat", "/proc/mounts"]).await;
    for dir in ["/tmp", "/data"] {
        assert!(
            mounts
                .lines()
                .any(|l| l.starts_with(&format!("tmpfs {dir} "))),
            "{dir}: {mounts}"
        );
    }
    assert!(!mounts.contains("docker.sock"), "{mounts}");
    let status = runner.stop(&c.id).await.unwrap();
    assert_eq!(status.state, "exited");
    assert_eq!(status.port, Some(8080));
    runner.start(&c.id).await.unwrap();
    assert!(runner.logs(&c.id).await.is_ok());
    runner.remove(&c.id).await.unwrap();
    assert_eq!(f.state(&c.id).await, None);
    f.finish().await;
}

/// Over HTTP with the real engine: stop, remove, start, status and logs on an unlabelled
/// container (and on the stack's `metadata-db`, when it's there), a compute-like one (stack
/// label only) and another stack's service are all 404, and each keeps running.
#[tokio::test]
#[ignore = "needs the workbench stack's Docker engine"]
async fn unlabelled_containers_are_refused() {
    let f = Fixture::new().await;
    let svc = Uuid::new_v4().to_string();
    let other_stack = format!("{}-other", f.stack);
    let mut targets = vec![
        f.plain("plain", &[]).await,
        f.plain(
            "compute",
            &[
                (STACK_LABEL, &f.stack),
                ("chelabase.dev/endpoint-id", "ep-x"),
            ],
        )
        .await,
        f.plain(
            "other",
            &[(STACK_LABEL, &other_stack), (SERVICE_LABEL, &svc)],
        )
        .await,
    ];
    // The workbench's own metadata-db (any project), by id and by name.
    let dbs = f
        .docker
        .list_containers(Some(ListContainersOptions {
            filters: Some(HashMap::from([(
                "label".to_string(),
                vec!["com.docker.compose.service=metadata-db".to_string()],
            )])),
            ..Default::default()
        }))
        .await
        .unwrap();
    for db in &dbs {
        targets.push(db.id.clone().unwrap());
        if let Some(name) = db.names.as_ref().and_then(|n| n.first()) {
            targets.push(name.trim_start_matches('/').to_string());
        }
    }
    let functions = "functions-0123456789abcdef0123456789abcdef";
    let state = Arc::new(AppState {
        runner: Arc::new(f.runner()),
        functions_token: functions.into(),
        worker_token: "worker-0123456789abcdef0123456789abcdef00".into(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await });
    let http = chelabase_http::client_builder().build().unwrap();
    for t in &targets {
        for op in ["stop", "remove", "start"] {
            let res = http
                .post(format!("http://{addr}/internal/containers/{op}"))
                .bearer_auth(functions)
                .json(&json!({"container": t}))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 404, "{op} {t}");
        }
        for what in ["status", "logs"] {
            let res = http
                .get(format!("http://{addr}/internal/containers/{t}/{what}"))
                .bearer_auth(functions)
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 404, "{what} {t}");
        }
        assert_eq!(
            f.state(t).await.as_deref(),
            Some("running"),
            "{t} untouched"
        );
    }
    f.finish().await;
}

struct Rows(Vec<Desired>);

#[async_trait::async_trait]
impl DesiredSource for Rows {
    async fn desired(&self) -> anyhow::Result<Vec<Desired>> {
        Ok(self.0.clone())
    }
}

/// Two stacks on one engine: stack A's pass with no desired rows removes A's service
/// containers and leaves B's (and an unlabelled one) running.
#[tokio::test]
#[ignore = "needs the workbench stack's Docker engine"]
async fn two_stacks_reconcile_apart() {
    let f = Fixture::new().await;
    let b_stack = format!("{}-b", f.stack);
    let labels = |stack: &str, k: &Key| {
        vec![
            (STACK_LABEL, stack.to_string()),
            (SERVICE_LABEL, k.service_id.to_string()),
            (BRANCH_LABEL, k.branch_id.to_string()),
            (DEPLOYMENT_LABEL, k.deployment_id.to_string()),
        ]
    };
    let (ka, kb) = (key(), key());
    let la = labels(&f.stack, &ka);
    let lb = labels(&b_stack, &kb);
    let a = f
        .plain(
            "a",
            &la.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>(),
        )
        .await;
    let b = f
        .plain(
            "b",
            &lb.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>(),
        )
        .await;
    let plain = f.plain("unlabelled", &[]).await;
    let runner = f.runner();
    let done = reconcile_once(&runner, &Rows(vec![])).await.unwrap();
    assert_eq!(done, [Action::Remove(a.clone())]);
    assert_eq!(f.state(&a).await, None);
    assert_eq!(f.state(&b).await.as_deref(), Some("running"));
    assert_eq!(f.state(&plain).await.as_deref(), Some("running"));
    // B's own pass keeps B's container when it is desired.
    let rb = Runner::new(
        Arc::new(DockerBackend::connect().unwrap()),
        &b_stack,
        &f.network,
        Caps::default(),
    );
    let spec = f.spec();
    let done = reconcile_once(
        &rb,
        &Rows(vec![Desired {
            key: kb,
            enabled: true,
            spec,
        }]),
    )
    .await
    .unwrap();
    assert!(done.is_empty(), "{done:?}");
    assert_eq!(f.state(&b).await.as_deref(), Some("running"));
    f.finish().await;
}

/// Desired state from the metadata (a scratch database): an enabled container service gets a
/// running container, disabling it stops it, deleting the function removes it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres and Docker engine"]
async fn reconcile_follows_the_metadata() {
    use chelabase_metadata::{
        DEMO_PROJECT_ID, DEMO_TENANT_ID, FunctionKind, MetadataClient, MetadataConfig,
        NewDeployment,
    };
    use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
    use sqlx::AssertSqlSafe;

    let url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    });
    let meta = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let name = format!(
        "cm_{:08x}{}",
        chrono::Utc::now().timestamp() as u32,
        &hex()[..4]
    );
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&meta)
        .await
        .unwrap();
    let (head, tail) = url.rsplit_once('/').unwrap();
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    let client = MetadataClient::connect(&MetadataConfig {
        database_url: format!("{head}/{name}{query}"),
    })
    .await
    .unwrap()
    .with_secrets_key(chelabase_secrets::SecretKey::generate());
    client.migrate().await.unwrap();
    let db = DatabaseId::new();
    client
        .insert_database_pending(
            db,
            TenantId(DEMO_TENANT_ID),
            ProjectId(DEMO_PROJECT_ID),
            &format!("d-{db}"),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE databases SET status = 'ready' WHERE id = $1")
        .bind(db.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();
    let branch = BranchId::from(db);
    sqlx::query("UPDATE branches SET status = 'ready' WHERE id = $1")
        .bind(branch.as_uuid())
        .execute(client.pool())
        .await
        .unwrap();

    let f = Fixture::new().await;
    let svc = client
        .create_function(db, "echo", FunctionKind::Service)
        .await
        .unwrap();
    // A function (bundle) and a URL service on the same branch are never the runner's.
    let url_svc = client
        .create_function(db, "remote", FunctionKind::Service)
        .await
        .unwrap();
    let spec = f.spec();
    let url_spec = json!({"backend": "url", "url": "https://example.com", "port": 443});
    let mut deployments = Vec::new();
    for (fid, s) in [(svc.id, &spec), (url_svc.id, &url_spec)] {
        let d = client
            .create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: fid,
                bundle_sha256: None,
                bundle_size: None,
                spec: s,
                compatibility_date: chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
                created_by: "m10 live",
                note: None,
            })
            .await
            .unwrap();
        client
            .set_active_deployment(branch, fid, d.id)
            .await
            .unwrap();
        client.set_enabled(branch, fid, true).await.unwrap();
        deployments.push(d.id);
    }
    let source = MetadataDesired {
        pool: client.pool().clone(),
    };
    let rows = source.desired().await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].key,
        Key {
            service_id: svc.id,
            branch_id: branch.as_uuid(),
            deployment_id: deployments[0]
        }
    );
    let runner = f.runner();
    reconcile_once(&runner, &source).await.unwrap();
    let ours = runner.ours().await.unwrap();
    assert_eq!(ours.len(), 1);
    assert!(ours[0].running());
    client.set_enabled(branch, svc.id, false).await.unwrap();
    reconcile_once(&runner, &source).await.unwrap();
    assert_eq!(f.state(&ours[0].id).await.as_deref(), Some("exited"));
    client
        .delete_function_unprotected(db, svc.id)
        .await
        .unwrap();
    reconcile_once(&runner, &source).await.unwrap();
    assert_eq!(f.state(&ours[0].id).await, None);
    assert!(runner.ours().await.unwrap().is_empty());

    f.finish().await;
    client.pool().close().await;
    sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .execute(&meta)
        .await
        .unwrap();
}

/// From a runner-made container on the stack's `services` network: the functions service's
/// public listener answers; its internal listener, the metadata database, NATS, SeaweedFS and
/// the orchestrator don't. Needs the stack recreated with this milestone's compose files.
#[tokio::test]
#[ignore = "needs a workbench stack running this milestone's compose files"]
async fn service_containers_reach_only_the_public_listener() {
    let project =
        std::env::var("CHELABASE_COMPOSE_PROJECT").expect("CHELABASE_COMPOSE_PROJECT (stack.env)");
    let network = format!("{project}_services");
    let d = docker();
    d.inspect_network(&network, None).await.unwrap_or_else(|e| {
        panic!("{network}: recreate the stack with this milestone's compose files ({e})")
    });
    let f = Fixture::new().await;
    let runner = Runner::new(
        Arc::new(DockerBackend::connect().unwrap()),
        &f.stack,
        &network,
        Caps::default(),
    );
    let c = runner.ensure(&request(&key(), f.spec())).await.unwrap();
    let probe = |target: &'static str| {
        let (d, id) = (d.clone(), c.id.clone());
        async move {
            exec(
                &d,
                &id,
                &["wget", "-q", "-T", "3", "-O", "/dev/null", target],
            )
            .await
            .0
        }
    };
    assert_eq!(
        probe("http://functions:8091/health").await,
        0,
        "the public listener"
    );
    for denied in [
        "http://functions:8080/internal/bundles/x/y",
        "http://functions-internal:8080/",
        "http://functions-control:8080/internal/invoke/x/y",
        "http://metadata-db:5432/",
        "http://nats:8222/",
        "http://seaweedfs:8333/",
        "http://orchestrator:8081/health",
        "http://service-runner-control:8094/health",
    ] {
        assert_ne!(probe(denied).await, 0, "{denied} must be unreachable");
    }
    runner.remove(&c.id).await.unwrap();
    // Removes the test's own network and image; the stack's network stays.
    f.finish().await;
}
