//! F4 (T8 m3): a `create` that fails after the container exists (joining the control network,
//! uploading the TLS files, starting it) removes the container it made, best effort, and still
//! returns the step's own error. Carried gaps T2 (R4): a `create` whose future is dropped after
//! the container exists (a caller's timeout, a cancelled job) removes it too, and (R5)
//! `list_not_running` names the computes whose container exists but isn't running. Against a
//! mock Docker Engine API on a TCP port, handed to the backend as its client
//! (`DockerBackend::with_docker`), so no daemon is needed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_neon::compute_tls::ComputeCa;
use chelabase_neon::{ComputeBackend, ComputeSettings, DockerBackend, DockerSettings};

/// A container the mock lists: endpoint id, stack, Docker state.
type Listed = (&'static str, &'static str, &'static str);

#[derive(Clone, Default)]
struct Mock {
    /// `METHOD path?query` of every request, in order.
    requests: Arc<Mutex<Vec<String>>>,
    /// The step that answers 500: "connect", "archive" or "start".
    fail_step: Arc<Mutex<Option<&'static str>>>,
    /// The step that never answers: "connect", "archive" or "start".
    hang_step: Arc<Mutex<Option<&'static str>>>,
    /// DELETE answers 500.
    fail_delete: Arc<Mutex<bool>>,
    /// What `GET /containers/json` (and each container's inspect) knows of.
    containers: Arc<Mutex<Vec<Listed>>>,
}

const STACK: &str = "mock";
const ENDPOINT_LABEL: &str = "chelabase.dev/endpoint-id";
const STACK_LABEL: &str = "chelabase.dev/stack";

fn labels(ep: &str, stack: &str) -> serde_json::Value {
    serde_json::json!({ ENDPOINT_LABEL: ep, STACK_LABEL: stack })
}

/// Whether a container with `labels` (endpoint, stack) and `state` passes Docker's `filters`
/// (`label` as `key` or `key=value`, `status`), as the engine applies them.
fn passes(filters: &HashMap<String, Vec<String>>, ep: &str, stack: &str, state: &str) -> bool {
    let has_label = |f: &str| match f.split_once('=') {
        Some((k, v)) => (k == ENDPOINT_LABEL && v == ep) || (k == STACK_LABEL && v == stack),
        None => f == ENDPOINT_LABEL || f == STACK_LABEL,
    };
    filters.get("label").is_none_or(|l| l.iter().all(|f| has_label(f)))
        && filters
            .get("status")
            .is_none_or(|s| s.iter().any(|st| st == state))
}

async fn engine(State(mock): State<Mock>, request: Request) -> Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("").to_string();
    mock.requests
        .lock()
        .unwrap()
        .push(format!("{method} {path}?{query}"));
    let fail = *mock.fail_step.lock().unwrap();
    let hang = *mock.hang_step.lock().unwrap();
    let boom = || (StatusCode::INTERNAL_SERVER_ERROR, r#"{"message":"boom"}"#).into_response();
    let json = |body: serde_json::Value| {
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            body.to_string(),
        )
            .into_response()
    };
    if method == "POST" && path.ends_with("/containers/create") {
        return (
            StatusCode::CREATED,
            [("content-type", "application/json")],
            r#"{"Id":"cid0123456789","Warnings":[]}"#,
        )
            .into_response();
    }
    let step = if method == "POST" && path.contains("/networks/") && path.ends_with("/connect") {
        Some(("connect", StatusCode::OK))
    } else if method == "PUT" && path.ends_with("/archive") {
        Some(("archive", StatusCode::OK))
    } else if method == "POST" && path.ends_with("/start") {
        Some(("start", StatusCode::NO_CONTENT))
    } else {
        None
    };
    if let Some((step, ok)) = step {
        if hang == Some(step) {
            std::future::pending::<()>().await;
        }
        return if fail == Some(step) {
            boom()
        } else {
            ok.into_response()
        };
    }
    if method == "DELETE" && path.contains("/containers/") {
        return if *mock.fail_delete.lock().unwrap() {
            boom()
        } else {
            StatusCode::NO_CONTENT.into_response()
        };
    }
    if method == "GET" && path.ends_with("/containers/json") {
        let params: HashMap<String, String> = Query::try_from_uri(request.uri())
            .map(|Query(q)| q)
            .unwrap_or_default();
        let filters: HashMap<String, Vec<String>> = params
            .get("filters")
            .map(|f| serde_json::from_str(f).expect("filters are JSON"))
            .unwrap_or_default();
        let listed: Vec<serde_json::Value> = mock
            .containers
            .lock()
            .unwrap()
            .iter()
            .filter(|(ep, stack, state)| passes(&filters, ep, stack, state))
            .map(|(ep, stack, state)| {
                serde_json::json!({
                    "Id": format!("cid-{ep}"),
                    "Names": [format!("/chelabase-{stack}-compute-{ep}")],
                    "Image": "neon/compute-node-v17:test",
                    "Labels": labels(ep, stack),
                    "State": state,
                    "Status": state,
                })
            })
            .collect();
        return json(serde_json::Value::Array(listed));
    }
    // A container's inspect, by name or id: for a listing that reads each state this way.
    if method == "GET" && path.contains("/containers/") && path.ends_with("/json") {
        let found = mock
            .containers
            .lock()
            .unwrap()
            .iter()
            .find(|(ep, stack, _)| {
                path.contains(&format!("/chelabase-{stack}-compute-{ep}/"))
                    || path.contains(&format!("/cid-{ep}/"))
            })
            .copied();
        if let Some((ep, stack, state)) = found {
            return json(serde_json::json!({
                "Id": format!("cid-{ep}"),
                "Name": format!("/chelabase-{stack}-compute-{ep}"),
                "State": { "Status": state, "Running": state == "running", "Restarting": state == "restarting" },
                "Config": { "Labels": labels(ep, stack) },
            }));
        }
        return (StatusCode::NOT_FOUND, r#"{"message":"no such container"}"#).into_response();
    }
    (StatusCode::NOT_FOUND, r#"{"message":"unexpected"}"#).into_response()
}

fn docker_settings() -> DockerSettings {
    DockerSettings {
        network: "stack_default".into(),
        // A separate control network, so the join step is a request.
        control_network: "stack_control".into(),
    }
}

fn settings() -> ComputeSettings {
    ComputeSettings {
        image: "neon/compute-node-v17:test".into(),
        control_plane_uri: "http://orchestrator:8083".into(),
        stack: STACK.into(),
        ca: Some(Arc::new(
            ComputeCa::generate(std::time::SystemTime::now()).unwrap(),
        )),
    }
}

fn deletes(mock: &Mock) -> Vec<String> {
    mock.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("DELETE "))
        .cloned()
        .collect()
}

/// The mock engine on a local port and a backend whose client is aimed at it, never at the
/// machine's Docker daemon (`DockerBackend::connect` reads the local socket).
async fn mock_backend() -> (Mock, DockerBackend) {
    let mock = Mock::default();
    let app = Router::new().fallback(engine).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let docker = bollard::Docker::connect_with_http(
        &format!("http://{addr}"),
        10,
        bollard::API_DEFAULT_VERSION,
    )
    .unwrap();
    (mock, DockerBackend::with_docker(settings(), docker_settings(), docker))
}

/// The DELETEs the mock has seen once `wait` has passed with no new one (a removal spawned on
/// drop runs in the background).
async fn settled_deletes(mock: &Mock, wait: Duration) -> Vec<String> {
    tokio::time::sleep(wait).await;
    deletes(mock)
}

#[tokio::test]
async fn a_failed_create_removes_the_container_it_made() {
    let (mock, backend) = mock_backend().await;

    // The control: every step answers, nothing is removed.
    backend.create("ep-ok", "tok").await.unwrap();
    {
        let seen = mock.requests.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|r| r.starts_with("POST ") && r.contains("/containers/create")),
            "the mock saw no create: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|r| r.contains("/networks/stack_control/connect")),
            "{seen:?}"
        );
        assert!(
            seen.iter()
                .any(|r| r.starts_with("PUT ") && r.contains("/archive")),
            "{seen:?}"
        );
        assert!(seen.iter().any(|r| r.ends_with("/start?")), "{seen:?}");
    }
    assert_eq!(
        deletes(&mock),
        Vec::<String>::new(),
        "a good create removes nothing"
    );

    for (step, message) in [
        ("connect", "connecting"),
        ("archive", "uploading the TLS files"),
        ("start", "starting container"),
    ] {
        mock.requests.lock().unwrap().clear();
        *mock.fail_step.lock().unwrap() = Some(step);
        let err = backend.create("ep-bad", "tok").await.unwrap_err();
        assert!(
            format!("{err:#}").contains(message),
            "{step}: the step's own error is returned, got {err:#}"
        );
        let removed = deletes(&mock);
        assert_eq!(
            removed.len(),
            1,
            "{step}: a stopped container is left behind: {:?}",
            mock.requests.lock().unwrap()
        );
        assert!(
            removed[0].contains("cid0123456789")
                || removed[0].contains("mock-ep-bad")
                || removed[0].contains("ep-bad"),
            "{step}: removes the container it made: {removed:?}"
        );
        assert!(
            removed[0].contains("force=true"),
            "{step}: forced (it may have been started): {removed:?}"
        );
    }

    // The cleanup is best effort: a failing remove doesn't replace the step's error.
    mock.requests.lock().unwrap().clear();
    *mock.fail_step.lock().unwrap() = Some("archive");
    *mock.fail_delete.lock().unwrap() = true;
    let err = backend.create("ep-bad", "tok").await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("uploading the TLS files"), "{text}");
    assert!(
        !text.contains("removing"),
        "the cleanup's failure must not mask the cause: {text}"
    );
    assert_eq!(deletes(&mock).len(), 1, "it still tried");
}

// R4: the `create` future dropped (here by a caller's timeout) after the container exists and
// before `Ok`: the container is removed, forced, whichever step it was waiting on.
#[tokio::test]
async fn a_dropped_create_removes_the_container_it_made() {
    let (mock, backend) = mock_backend().await;
    for step in ["connect", "archive", "start"] {
        mock.requests.lock().unwrap().clear();
        *mock.hang_step.lock().unwrap() = Some(step);

        let outcome =
            tokio::time::timeout(Duration::from_millis(300), backend.create("ep-drop", "tok"))
                .await;

        assert!(outcome.is_err(), "{step}: the create should still be waiting");
        let seen = mock.requests.lock().unwrap().clone();
        assert!(
            seen.iter()
                .any(|r| r.starts_with("POST ") && r.contains("/containers/create")),
            "{step}: the container was never created: {seen:?}"
        );
        // The removal is spawned on drop: give it time to reach the engine.
        let mut removed = deletes(&mock);
        for _ in 0..100 {
            if !removed.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            removed = deletes(&mock);
        }
        assert_eq!(
            removed.len(),
            1,
            "{step}: the dropped create's container is left behind: {:?}",
            mock.requests.lock().unwrap()
        );
        assert!(
            removed[0].contains("cid0123456789") || removed[0].contains("ep-drop"),
            "{step}: removes the container it made: {removed:?}"
        );
        assert!(removed[0].contains("force=true"), "{step}: {removed:?}");
    }
}

// R5: `list_not_running` names this stack's computes whose container exists in Docker state
// `exited`, `dead` or `restarting`; never `created` (an in-flight create passes through it),
// `running`, `paused` or `removing`, and never another stack's. `list` is unchanged.
#[tokio::test]
async fn list_not_running_names_exited_dead_and_restarting_computes() {
    let (mock, backend) = mock_backend().await;
    *mock.containers.lock().unwrap() = vec![
        ("ep-running", STACK, "running"),
        ("ep-created", STACK, "created"),
        ("ep-exited", STACK, "exited"),
        ("ep-dead", STACK, "dead"),
        ("ep-restarting", STACK, "restarting"),
        ("ep-paused", STACK, "paused"),
        ("ep-removing", STACK, "removing"),
        ("ep-other-stack", "other", "exited"),
    ];

    let mut not_running = backend.list_not_running().await.unwrap();
    not_running.sort();

    assert_eq!(
        not_running,
        ["ep-dead", "ep-exited", "ep-restarting"],
        "{:?}",
        mock.requests.lock().unwrap()
    );
    let mut all = backend.list().await.unwrap();
    all.sort();
    assert_eq!(
        all,
        [
            "ep-created",
            "ep-dead",
            "ep-exited",
            "ep-paused",
            "ep-removing",
            "ep-restarting",
            "ep-running"
        ]
    );

    // Nothing to list, and a failing engine is an error, not an empty list.
    mock.containers.lock().unwrap().clear();
    assert!(backend.list_not_running().await.unwrap().is_empty());
    *mock.containers.lock().unwrap() = vec![("ep-created", STACK, "created")];
    assert!(backend.list_not_running().await.unwrap().is_empty());
}

#[tokio::test]
async fn list_not_running_fails_when_the_engine_does() {
    // Nothing listens here: the listing fails rather than reporting no stopped compute.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let docker = bollard::Docker::connect_with_http(
        &format!("http://{addr}"),
        2,
        bollard::API_DEFAULT_VERSION,
    )
    .unwrap();
    let backend = DockerBackend::with_docker(settings(), docker_settings(), docker);
    assert!(backend.list_not_running().await.is_err());
}

// R4: the guard does nothing once `create` returned: not after `Ok`, and not a second time after
// the error path's own removal.
#[tokio::test]
async fn a_finished_create_removes_nothing_more() {
    let (mock, backend) = mock_backend().await;

    backend.create("ep-ok", "tok").await.unwrap();
    assert_eq!(
        settled_deletes(&mock, Duration::from_millis(300)).await,
        Vec::<String>::new(),
        "a good create removes nothing, even later"
    );

    for step in ["connect", "archive", "start"] {
        mock.requests.lock().unwrap().clear();
        *mock.fail_step.lock().unwrap() = Some(step);
        backend.create("ep-bad", "tok").await.unwrap_err();
        assert_eq!(
            settled_deletes(&mock, Duration::from_millis(300))
                .await
                .len(),
            1,
            "{step}: removed once by the error path, never again: {:?}",
            mock.requests.lock().unwrap()
        );
    }
}
