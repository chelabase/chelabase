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
//! Each test also starts a stand-in for the stack's functions container (busybox labelled with
//! the test's compose project and `com.docker.compose.service=functions`), which the runner
//! attaches to each service network. `service_containers_reach_nothing_but_their_own_port` uses
//! the stack's real functions container instead (attached, then detached, by the runner).

use std::collections::HashMap;
use std::sync::Arc;

use bollard::Docker;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CommitContainerOptions, CreateContainerOptions, ListContainersOptions, ListNetworksOptions,
    RemoveContainerOptions, RemoveImageOptions,
};
use futures_util::StreamExt as _;
use serde_json::json;
use service_runner::app::{AppState, router};
use service_runner::backend::ContainerBackend;
use service_runner::docker::DockerBackend;
use service_runner::reconcile::{Action, Desired, DesiredSource, MetadataDesired, reconcile_once};
use service_runner::runner::{EnsureRequest, Key, Runner, container_name};
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
    /// The throwaway stack label, also the stand-in compose project.
    stack: String,
    image: String,
    /// The stand-in functions container.
    functions: String,
}

impl Fixture {
    async fn new() -> Fixture {
        let docker = docker();
        let stack = format!("m10t-{}", hex());
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
        let mut f = Fixture {
            docker,
            stack,
            image,
            functions: String::new(),
        };
        let project = f.stack.clone();
        // On its project's `<project>_default` network, as compose would put it. The name sorts
        // after the runner's `chelabase-…-svc-…` networks, so an attach without a gateway
        // priority would take its default route (re-audit S1).
        let default_net = format!("{project}_default");
        f.docker
            .create_network(bollard::models::NetworkCreateRequest {
                name: default_net.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        f.functions = f
            .container(
                "functions",
                &[
                    ("com.docker.compose.project", &project),
                    ("com.docker.compose.service", "functions"),
                ],
                &default_net,
            )
            .await;
        f
    }

    fn runner(&self) -> Runner {
        Runner::new(
            Arc::new(DockerBackend::connect().unwrap()),
            &self.stack,
            &self.stack,
            Caps::default(),
        )
    }

    /// The container's address on its service network.
    async fn address(&self, id: &str) -> String {
        self.docker
            .inspect_container(id, None)
            .await
            .unwrap()
            .network_settings
            .and_then(|n| n.networks)
            .and_then(|n| n.into_values().next())
            .and_then(|e| e.ip_address)
            .expect("an address")
    }

    fn spec(&self) -> serde_json::Value {
        json!({"backend": "container", "image": self.image, "port": 8080, "health_path": "/",
               "resources": {"memory_mb": 128, "cpus": 0.5}})
    }

    /// A container that sleeps, with `labels` (none: like any compose service), on no network.
    async fn plain(&self, name: &str, labels: &[(&str, &str)]) -> String {
        self.container(name, labels, "none").await
    }

    async fn container(&self, name: &str, labels: &[(&str, &str)], network: &str) -> String {
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
                        network_mode: Some(network.into()),
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
        // The runner's networks of this stack (and its `-b`/`-other` variants), detached first:
        // a real functions container may still be on one after a failed test.
        let nets = self
            .docker
            .list_networks(None::<ListNetworksOptions>)
            .await
            .unwrap();
        for n in nets {
            let name = n.name.unwrap_or_default();
            if !name.contains(&self.stack) {
                continue;
            }
            if let Ok(i) = self.docker.inspect_network(&name, None).await {
                for c in i.containers.unwrap_or_default().into_keys() {
                    self.docker
                        .disconnect_network(
                            &name,
                            bollard::models::NetworkDisconnectRequest {
                                container: c,
                                force: Some(true),
                            },
                        )
                        .await
                        .ok();
                }
            }
            self.docker.remove_network(&name).await.ok();
        }
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
    // Its own network (audit B3), named and labelled like the container.
    let net = container_name(&f.stack, k.service_id, k.branch_id);
    assert_eq!(h.network_mode.as_deref(), Some(net.as_str()));
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
    assert_eq!(networks, std::slice::from_ref(&net));
    let n = f.docker.inspect_network(&net, None).await.unwrap();
    assert_eq!(n.internal, Some(false), "services reach the internet");
    let nl = n.labels.unwrap_or_default();
    assert_eq!(nl[STACK_LABEL], f.stack);
    assert_eq!(nl[SERVICE_LABEL], k.service_id.to_string());
    let mut attached: Vec<String> = n.containers.unwrap_or_default().into_keys().collect();
    attached.sort();
    let mut want = vec![c.id.clone(), f.functions.clone()];
    want.sort();
    assert_eq!(
        attached, want,
        "the container and the functions service, nothing else"
    );
    // Created from the inspected image id (audit S2), with a keyed env digest.
    let image_id = f.docker.inspect_image(&f.image).await.unwrap().id.unwrap();
    assert_eq!(i.image.as_deref(), Some(image_id.as_str()));
    let digest = &labels[service_runner::runner::ENV_DIGEST_LABEL];
    assert_eq!(digest.len(), 64);
    assert!(!labels.values().any(|v| v.contains("hello")));
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
    assert!(
        f.docker.inspect_network(&net, None).await.is_err(),
        "the network goes with it"
    );
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
        &f.stack,
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
    let runner = f.runner().with_active(Arc::new(MetadataDesired {
        pool: client.pool().clone(),
    }));
    // Reconcile never creates (audit B1): only the functions service's ensure does.
    assert!(reconcile_once(&runner, &source).await.unwrap().is_empty());
    assert!(runner.ours().await.unwrap().is_empty());
    runner
        .ensure(&EnsureRequest {
            service_id: svc.id,
            branch_id: branch.as_uuid(),
            deployment_id: deployments[0],
            spec: spec.clone(),
            env: [("API_KEY".to_string(), "s3cret".to_string())].into(),
        })
        .await
        .unwrap();
    assert!(
        reconcile_once(&runner, &source).await.unwrap().is_empty(),
        "converged"
    );
    let ours = runner.ours().await.unwrap();
    assert_eq!(ours.len(), 1);
    assert!(ours[0].running());
    let net = container_name(&f.stack, svc.id, branch.as_uuid());
    // m11 (re-audit B1, B2): a rollover's new deployment is pending in the metadata, so its
    // young container outlives a pass while the service is enabled, beside the active one.
    let next = client
        .create_deployment(&NewDeployment {
            id: Uuid::new_v4(),
            function_id: svc.id,
            bundle_sha256: None,
            bundle_size: None,
            spec: &spec,
            compatibility_date: chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            created_by: "m11 live",
            note: None,
        })
        .await
        .unwrap()
        .id;
    let pending_key = Key {
        service_id: svc.id,
        branch_id: branch.as_uuid(),
        deployment_id: next,
    };
    assert_eq!(source.pending().await.unwrap(), vec![pending_key]);
    assert_eq!(
        service_runner::reconcile::ActiveSource::active(&source, svc.id, branch.as_uuid())
            .await
            .unwrap(),
        Some(deployments[0])
    );
    let rollover = runner
        .ensure(&EnsureRequest {
            service_id: svc.id,
            branch_id: branch.as_uuid(),
            deployment_id: next,
            spec: spec.clone(),
            env: [("API_KEY".to_string(), "s3cret".to_string())].into(),
        })
        .await
        .unwrap();
    assert!(
        reconcile_once(&runner, &source).await.unwrap().is_empty(),
        "the pending container is in its grace"
    );
    assert_eq!(runner.ours().await.unwrap().len(), 2);
    // Disabled: the active container stops and the pending one goes at once.
    client.set_enabled(branch, svc.id, false).await.unwrap();
    reconcile_once(&runner, &source).await.unwrap();
    assert_eq!(f.state(&rollover.id).await, None);
    assert_eq!(f.state(&ours[0].id).await.as_deref(), Some("exited"));
    client
        .delete_function_unprotected(db, svc.id)
        .await
        .unwrap();
    reconcile_once(&runner, &source).await.unwrap();
    assert_eq!(f.state(&ours[0].id).await, None);
    assert!(runner.ours().await.unwrap().is_empty());
    assert!(
        f.docker.inspect_network(&net, None).await.is_err(),
        "its network went too"
    );

    f.finish().await;
    client.pool().close().await;
    sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE {name} WITH (FORCE)")))
        .execute(&meta)
        .await
        .unwrap();
}

/// AC 9 on the stack itself: from a runner-made container, every listener of the stack's real
/// functions container (attached to the container's network by the runner), the metadata
/// database, NATS, SeaweedFS, the orchestrator, the runner's API and a second service's container
/// are all unreachable; the functions container reaches the service. The test's containers carry
/// a throwaway stack label; the runner detaches the functions container when it removes them.
#[tokio::test]
#[ignore = "needs the workbench stack (its real functions container)"]
async fn service_containers_reach_nothing_but_their_own_port() {
    let project =
        std::env::var("CHELABASE_COMPOSE_PROJECT").expect("CHELABASE_COMPOSE_PROJECT (stack.env)");
    let f = Fixture::new().await;
    let backend = Arc::new(DockerBackend::connect().unwrap());
    let functions = backend.functions_containers(&project).await.unwrap();
    assert_eq!(
        functions.len(),
        1,
        "the stack's functions container: {functions:?}"
    );
    let runner = Runner::new(backend, &f.stack, &project, Caps::default());
    let route_before = default_route(&f.docker, &functions[0]).await;
    let attacker = runner.ensure(&request(&key(), f.spec())).await.unwrap();
    let victim = runner.ensure(&request(&key(), f.spec())).await.unwrap();
    let net = attacker.label(BRANCH_LABEL).map(|b| {
        container_name(
            &f.stack,
            attacker.label(SERVICE_LABEL).unwrap().parse().unwrap(),
            b.parse().unwrap(),
        )
    });
    let net = net.unwrap();
    let fn_ip = f
        .docker
        .inspect_network(&net, None)
        .await
        .unwrap()
        .containers
        .unwrap_or_default()
        .remove(&functions[0])
        .and_then(|e| e.ipv4_address)
        .expect("the functions container on the attacker's network")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let victim_ip = f.address(&victim.id).await;
    let attacker_ip = f.address(&attacker.id).await;
    let probe = |target: String| {
        let (d, id) = (f.docker.clone(), attacker.id.clone());
        async move {
            exec(
                &d,
                &id,
                &["wget", "-q", "-T", "3", "-O", "/dev/null", &target],
            )
            .await
            .0
        }
    };
    let mut denied: Vec<String> = [8091, 8080, 8092, 8093]
        .iter()
        .map(|p| format!("http://{fn_ip}:{p}/health"))
        .collect();
    denied.extend(
        [
            "http://functions:8091/health",
            "http://functions-public:8091/health",
            "http://functions-internal:8080/",
            "http://functions-control:8080/internal/invoke/x/y",
            "http://metadata-db:5432/",
            "http://nats:8222/",
            "http://seaweedfs:8333/",
            "http://orchestrator:8081/health",
            "http://service-runner-control:8094/health",
        ]
        .map(String::from),
    );
    denied.push(format!("http://{victim_ip}:8080/sh"));
    let mut reached = Vec::new();
    for target in &denied {
        if probe(target.clone()).await == 0 {
            reached.push(target.clone());
        }
    }
    // The functions container reaches the service (traffic flows functions → container).
    let (fn_code, fn_out) = exec(
        &f.docker,
        &functions[0],
        &[
            "curl",
            "-fsS",
            "-m",
            "3",
            "-o",
            "/dev/null",
            &format!("http://{attacker_ip}:8080/sh"),
        ],
    )
    .await;
    let route_attached = default_route(&f.docker, &functions[0]).await;
    runner.remove(&attacker.id).await.unwrap();
    runner.remove(&victim.id).await.unwrap();
    let route_after = default_route(&f.docker, &functions[0]).await;
    let still: Vec<String> = f
        .docker
        .inspect_container(&functions[0], None)
        .await
        .unwrap()
        .network_settings
        .and_then(|n| n.networks)
        .unwrap_or_default()
        .into_keys()
        .filter(|n| n.contains(&f.stack))
        .collect();
    f.finish().await;
    assert!(
        reached.is_empty(),
        "a service container reached {reached:?}"
    );
    assert_eq!(
        fn_code, 0,
        "the functions container must reach the service: {fn_out}"
    );
    assert!(
        route_before.is_some(),
        "the functions container has a default route"
    );
    assert_eq!(route_attached, route_before, "re-audit S1: attached");
    assert_eq!(route_after, route_before, "re-audit S1: detached");
    assert!(
        still.is_empty(),
        "the functions container is detached afterwards: {still:?}"
    );
}

/// A busybox image committed with `changes` (Dockerfile lines), removed by the test.
async fn commit_image(docker: &Docker, stack: &str, changes: &str) -> String {
    let seed = docker
        .create_container(
            Some(CreateContainerOptions {
                name: Some(format!("{stack}-seed-{}", hex())),
                ..Default::default()
            }),
            ContainerCreateBody {
                image: Some("busybox:latest".into()),
                cmd: Some(vec!["true".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let image = format!("m10t-img:{}", hex());
    let (repo, tag) = image.split_once(':').unwrap();
    docker
        .commit_container(
            CommitContainerOptions {
                container: Some(seed.id.clone()),
                repo: Some(repo.into()),
                tag: Some(tag.into()),
                changes: Some(changes.into()),
                ..Default::default()
            },
            bollard::models::ContainerConfig::default(),
        )
        .await
        .unwrap();
    remove(docker, &seed.id).await;
    image
}

/// Audit B2: an image's own `chelabase.dev/*` and `com.docker.compose.*` labels never reach a
/// runner container (they would make it a compute, or a compose service, to every other socket
/// holder): ensure refuses such an image and creates nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's Docker engine"]
async fn image_labels_never_reach_the_container() {
    let f = Fixture::new().await;
    let image = commit_image(
        &f.docker,
        &f.stack,
        "CMD [\"httpd\", \"-f\", \"-p\", \"8080\", \"-h\", \"/bin\"]\n\
         LABEL chelabase.dev/endpoint-id=ep-victim000001\n\
         LABEL com.docker.compose.project=chelabase-epic-edge-functions\n\
         LABEL com.docker.compose.service=metadata-db",
    )
    .await;
    let mut spec = f.spec();
    spec["image"] = json!(image);
    let result = f.runner().ensure(&request(&key(), spec)).await;
    let computes = f
        .docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "label".to_string(),
                vec![
                    "chelabase.dev/endpoint-id".to_string(),
                    format!("{STACK_LABEL}={}", f.stack),
                ],
            )])),
            ..Default::default()
        }))
        .await
        .unwrap();
    f.docker
        .remove_image(
            &image,
            Some(RemoveImageOptions {
                force: true,
                ..Default::default()
            }),
            None,
        )
        .await
        .ok();
    let listed = computes.len();
    f.finish().await;
    assert_eq!(listed, 0, "a member image's labels reached a container");
    assert!(
        matches!(result, Err(service_runner::runner::RunnerError::Invalid(_))),
        "ensure must refuse the image: {result:?}"
    );
}

/// Audit B3: one service's container can't reach another's port.
#[tokio::test]
#[ignore = "needs the workbench stack's Docker engine"]
async fn service_containers_cannot_reach_each_other() {
    let f = Fixture::new().await;
    let runner = f.runner();
    let attacker = runner.ensure(&request(&key(), f.spec())).await.unwrap();
    let victim = runner.ensure(&request(&key(), f.spec())).await.unwrap();
    let ip = f
        .docker
        .inspect_container(&victim.id, None)
        .await
        .unwrap()
        .network_settings
        .and_then(|n| n.networks)
        .and_then(|n| n.into_values().next())
        .and_then(|e| e.ip_address)
        .expect("the victim's address");
    let (code, _) = exec(
        &f.docker,
        &attacker.id,
        &[
            "wget",
            "-q",
            "-T",
            "3",
            "-O",
            "/dev/null",
            &format!("http://{ip}:8080/sh"),
        ],
    )
    .await;
    // The (stand-in) functions container reaches both.
    let mut from_functions = Vec::new();
    for c in [&attacker, &victim] {
        let ip = f.address(&c.id).await;
        from_functions.push(
            exec(
                &f.docker,
                &f.functions,
                &[
                    "wget",
                    "-q",
                    "-T",
                    "3",
                    "-O",
                    "/dev/null",
                    &format!("http://{ip}:8080/sh"),
                ],
            )
            .await
            .0,
        );
    }
    f.finish().await;
    assert_ne!(code, 0, "the attacker reached the victim's port directly");
    assert_eq!(
        from_functions,
        [0, 0],
        "the functions service reaches each service"
    );
}

/// The container's default route, from `/proc/net/route`: `(interface, gateway hex)`.
async fn default_route(docker: &Docker, id: &str) -> Option<(String, String)> {
    let (_, out) = exec(docker, id, &["cat", "/proc/net/route"]).await;
    out.lines().skip(1).find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.get(1) == Some(&"00000000")).then(|| (f[0].to_string(), f[2].to_string()))
    })
}

/// Re-audit S1: attaching the functions container to a service network (and detaching it)
/// never moves its default route off its own network.
#[tokio::test]
#[ignore = "needs the workbench stack's Docker engine"]
async fn service_networks_never_take_the_functions_default_route() {
    let f = Fixture::new().await;
    let before = default_route(&f.docker, &f.functions).await;
    assert!(before.is_some(), "the stand-in has a default route");
    let runner = f.runner();
    let c = runner.ensure(&request(&key(), f.spec())).await.unwrap();
    let attached = default_route(&f.docker, &f.functions).await;
    runner.remove(&c.id).await.unwrap();
    let after = default_route(&f.docker, &f.functions).await;
    f.finish().await;
    assert_eq!(attached, before, "attached to a service network");
    assert_eq!(after, before, "after the service network went");
}
