//! F4 (T8 m3): a `create` that fails after the container exists (joining the control network,
//! uploading the TLS files, starting it) removes the container it made, best effort, and still
//! returns the step's own error. Carried gaps T2 (R4): a `create` whose future is dropped after
//! the container exists (a caller's timeout, a cancelled job) removes it too, and (R5)
//! `list_not_running` names the computes whose container exists but isn't running. A `create`
//! refused for a missing image pulls it and creates again. compute-network-reach m2: every
//! `create` first checks both networks (internal, isolated gateway) and refuses otherwise; the
//! listing of computes with their networks, the legacy networks and their removal. Against a
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
/// A listed container's networks: `(network name, network id)`.
type Attachments = Vec<(&'static str, &'static str)>;
/// Puts the mock in a state where a create must be refused.
type Refusal = Box<dyn Fn(&Mock)>;

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
    /// The compute image isn't there: a container create answers 404 until a pull.
    missing_image: Arc<Mutex<bool>>,
    /// A pull streams an error instead of the image.
    fail_pull: Arc<Mutex<bool>>,
    /// Network inspects by exact name (`GET /networks/<name>`); a name not here is 404.
    networks: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// Inspecting this network answers 500.
    fail_network_inspect: Arc<Mutex<Option<&'static str>>>,
    /// Each listed container's attachments, by endpoint id: `(network name, network id)`.
    /// A container not here is on the two configured networks.
    attachments: Arc<Mutex<HashMap<&'static str, Attachments>>>,
    /// Further containers the listing knows of, as Docker would list them (any labels).
    raw_containers: Arc<Mutex<Vec<serde_json::Value>>>,
    /// The listing ignores `filters`, like an engine that doesn't apply one.
    ignore_filters: Arc<Mutex<bool>>,
}

/// A network inspect as Docker answers it.
fn network_json(name: &str, internal: bool, v4: Option<&str>, v6: Option<&str>) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    if let Some(mode) = v4 {
        options.insert(
            "com.docker.network.bridge.gateway_mode_ipv4".into(),
            mode.into(),
        );
    }
    if let Some(mode) = v6 {
        options.insert(
            "com.docker.network.bridge.gateway_mode_ipv6".into(),
            mode.into(),
        );
    }
    serde_json::json!({
        "Name": name,
        "Id": format!("nid-{name}"),
        "Driver": "bridge",
        "Internal": internal,
        "Options": options,
        "Containers": {},
    })
}

/// The two configured networks, internal and isolated.
fn good_networks() -> HashMap<String, serde_json::Value> {
    ["stack_default", "stack_control"]
        .into_iter()
        .map(|n| (n.to_string(), network_json(n, true, Some("isolated"), Some("isolated"))))
        .collect()
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

/// [`passes`] for a container as Docker lists it (its `Labels` and `State`).
fn passes_json(filters: &HashMap<String, Vec<String>>, container: &serde_json::Value) -> bool {
    let labels = container["Labels"].as_object().cloned().unwrap_or_default();
    let state = container["State"].as_str().unwrap_or("");
    let has_label = |f: &str| match f.split_once('=') {
        Some((k, v)) => labels.get(k).and_then(|l| l.as_str()) == Some(v),
        None => labels.contains_key(f),
    };
    // `network` matches a network the container is attached to by name or id, running or not.
    let on_network = |f: &str| {
        container["NetworkSettings"]["Networks"]
            .as_object()
            .is_some_and(|n| n.iter().any(|(name, e)| name == f || e["NetworkID"] == f))
    };
    filters.get("network").is_none_or(|n| n.iter().all(|f| on_network(f)))
        && filters.get("label").is_none_or(|l| l.iter().all(|f| has_label(f)))
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
    if method == "POST" && path.ends_with("/images/create") {
        // Docker answers a pull 200 and streams its progress; a failure is a line of the stream.
        let line = if *mock.fail_pull.lock().unwrap() {
            r#"{"errorDetail":{"message":"denied"},"error":"denied"}"#
        } else {
            *mock.missing_image.lock().unwrap() = false;
            r#"{"status":"Downloaded newer image"}"#
        };
        return (StatusCode::OK, [("content-type", "application/json")], line).into_response();
    }
    if method == "POST" && path.ends_with("/containers/create") {
        if *mock.missing_image.lock().unwrap() {
            return (
                StatusCode::NOT_FOUND,
                r#"{"message":"No such image: neon/compute-node-v17:test"}"#,
            )
                .into_response();
        }
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
    if method == "GET" && path.contains("/networks/") {
        let name = path.rsplit("/networks/").next().unwrap_or("").to_string();
        if *mock.fail_network_inspect.lock().unwrap() == Some(name.as_str()) {
            return boom();
        }
        return match mock.networks.lock().unwrap().get(&name) {
            Some(network) => json(network.clone()),
            None => (StatusCode::NOT_FOUND, r#"{"message":"network not found"}"#).into_response(),
        };
    }
    if method == "DELETE" && path.contains("/networks/") {
        let id = path.rsplit("/networks/").next().unwrap_or("").to_string();
        let mut networks = mock.networks.lock().unwrap();
        let Some(name) = networks
            .iter()
            .find(|(_, n)| n["Id"] == id.as_str())
            .map(|(name, _)| name.clone())
        else {
            return (StatusCode::NOT_FOUND, r#"{"message":"network not found"}"#).into_response();
        };
        if networks[&name]["Containers"]
            .as_object()
            .is_some_and(|c| !c.is_empty())
        {
            return (
                StatusCode::FORBIDDEN,
                r#"{"message":"error while removing network: has active endpoints"}"#,
            )
                .into_response();
        }
        networks.remove(&name);
        return StatusCode::NO_CONTENT.into_response();
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
        let filters = if *mock.ignore_filters.lock().unwrap() {
            HashMap::new()
        } else {
            filters
        };
        let attachments = mock.attachments.lock().unwrap().clone();
        let mut listed: Vec<serde_json::Value> = mock
            .containers
            .lock()
            .unwrap()
            .iter()
            .filter(|(ep, stack, state)| passes(&filters, ep, stack, state))
            .map(|(ep, stack, state)| {
                let on = attachments
                    .get(ep)
                    .cloned()
                    .unwrap_or_else(|| vec![("stack_default", "nid-stack_default"), ("stack_control", "nid-stack_control")]);
                let networks: serde_json::Map<String, serde_json::Value> = on
                    .iter()
                    .map(|(name, id)| (name.to_string(), serde_json::json!({ "NetworkID": id })))
                    .collect();
                serde_json::json!({
                    "Id": format!("cid-{ep}"),
                    "Names": [format!("/chelabase-{stack}-compute-{ep}")],
                    "Image": "neon/compute-node-v17:test",
                    "Labels": labels(ep, stack),
                    "State": state,
                    "Status": state,
                    "NetworkSettings": { "Networks": networks },
                })
            })
            .collect();
        listed.extend(
            mock.raw_containers
                .lock()
                .unwrap()
                .iter()
                .filter(|c| passes_json(&filters, c))
                .cloned(),
        );
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
        legacy_networks: vec!["stack_compute_old".into(), "stack_control_old".into()],
        egress_dns: None,
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
    *mock.networks.lock().unwrap() = good_networks();
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

fn seen(mock: &Mock, what: &str) -> usize {
    mock.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("POST ") && r.contains(what))
        .count()
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

// A fresh install has the compute image pinned but never pulled (Compose pulls only the images
// of its services): `create` pulls the image it was refused for and creates again.
#[tokio::test]
async fn a_create_pulls_a_missing_image() {
    let (mock, backend) = mock_backend().await;
    *mock.missing_image.lock().unwrap() = true;

    backend.create("ep-new", "tok").await.unwrap();

    let requests = mock.requests.lock().unwrap().clone();
    assert!(
        requests.iter().any(|r| r.contains("/images/create?")
            && r.contains("fromImage=neon%2Fcompute-node-v17%3Atest")),
        "the configured image is pulled: {requests:?}"
    );
    assert_eq!(seen(&mock, "/images/create"), 1, "{requests:?}");
    assert_eq!(
        seen(&mock, "/containers/create"),
        2,
        "refused, then created: {requests:?}"
    );
    assert!(
        requests.iter().any(|r| r.ends_with("/start?")),
        "{requests:?}"
    );
    assert_eq!(deletes(&mock), Vec::<String>::new());
}

// A pull that fails is the create's error, said as a pull; it is tried once, not in a loop.
#[tokio::test]
async fn a_failed_pull_fails_the_create() {
    let (mock, backend) = mock_backend().await;
    *mock.missing_image.lock().unwrap() = true;
    *mock.fail_pull.lock().unwrap() = true;

    let err = backend.create("ep-new", "tok").await.unwrap_err();

    let text = format!("{err:#}");
    assert!(
        text.contains("pulling neon/compute-node-v17:test"),
        "{text}"
    );
    assert!(
        text.contains("denied"),
        "Docker's own reason is kept: {text}"
    );
    assert_eq!(seen(&mock, "/images/create"), 1);
    assert_eq!(seen(&mock, "/containers/create"), 1);
}

// Present images aren't pulled again: a good create makes no pull request.
#[tokio::test]
async fn a_create_with_the_image_present_pulls_nothing() {
    let (mock, backend) = mock_backend().await;

    backend.create("ep-ok", "tok").await.unwrap();

    assert_eq!(seen(&mock, "/images/create"), 0);
}

// --- compute-network-reach m2: the network check on create -------------------------------

fn container_creates(mock: &Mock) -> usize {
    seen(mock, "/containers/create")
}

fn network_inspects(mock: &Mock, name: &str) -> usize {
    mock.requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("GET ") && r.contains(&format!("/networks/{name}?")))
        .count()
}

/// Sets `name`'s inspect.
fn set_network(mock: &Mock, name: &str, network: serde_json::Value) {
    mock.networks.lock().unwrap().insert(name.to_string(), network);
}

#[tokio::test]
async fn create_refuses_a_network_that_is_not_internal() {
    let (mock, backend) = mock_backend().await;
    set_network(
        &mock,
        "stack_control",
        network_json("stack_control", false, Some("isolated"), Some("isolated")),
    );

    let err = backend.create("ep-new", "tok").await.unwrap_err();

    let text = format!("{err:#}");
    assert!(text.contains("network stack_control"), "{text}");
    assert!(text.contains("not internal"), "{text}");
    assert_eq!(container_creates(&mock), 0, "{:?}", mock.requests.lock().unwrap());
}

#[tokio::test]
async fn create_refuses_a_network_without_an_isolated_gateway() {
    let (mock, backend) = mock_backend().await;
    for (v4, v6, lacks) in [
        (Some("isolated"), None, "gateway_mode_ipv6=isolated"),
        (None, Some("isolated"), "gateway_mode_ipv4=isolated"),
        (Some("nat"), Some("isolated"), "gateway_mode_ipv4=isolated"),
    ] {
        set_network(&mock, "stack_default", network_json("stack_default", true, v4, v6));

        let err = backend.create("ep-new", "tok").await.unwrap_err();

        let text = format!("{err:#}");
        assert!(text.contains("network stack_default"), "{text}");
        assert!(text.contains(lacks), "{lacks}: {text}");
    }
    assert_eq!(container_creates(&mock), 0);
}

// Fail closed: a network Docker can't describe (an error, or no such network) refuses the
// create.
#[tokio::test]
async fn create_refuses_when_the_network_cannot_be_inspected() {
    let (mock, backend) = mock_backend().await;
    *mock.fail_network_inspect.lock().unwrap() = Some("stack_control");

    let err = backend.create("ep-new", "tok").await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("network stack_control"), "{text}");
    assert!(text.contains("inspect"), "{text}");

    *mock.fail_network_inspect.lock().unwrap() = None;
    mock.networks.lock().unwrap().remove("stack_default");
    let err = backend.create("ep-new", "tok").await.unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("network stack_default"), "{text}");
    assert!(text.contains("doesn't exist"), "{text}");
    assert_eq!(container_creates(&mock), 0);
}

// Docker resolves a network by id prefix too: an answer for a network of another name is
// refused like a missing one.
#[tokio::test]
async fn create_refuses_a_network_answered_under_another_name() {
    let (mock, backend) = mock_backend().await;
    set_network(
        &mock,
        "stack_default",
        network_json("elsewhere", true, Some("isolated"), Some("isolated")),
    );

    let err = backend.create("ep-new", "tok").await.unwrap_err();

    assert!(format!("{err:#}").contains("network stack_default"), "{err:#}");
    assert_eq!(container_creates(&mock), 0);
}

#[tokio::test]
async fn a_refused_create_leaves_no_container() {
    let (mock, backend) = mock_backend().await;
    let refusals: [Refusal; 3] = [
        Box::new(|m| set_network(m, "stack_control", network_json("stack_control", false, Some("isolated"), Some("isolated")))),
        Box::new(|m| set_network(m, "stack_default", network_json("stack_default", true, Some("isolated"), None))),
        Box::new(|m| *m.fail_network_inspect.lock().unwrap() = Some("stack_default")),
    ];
    for refuse in refusals {
        *mock.networks.lock().unwrap() = good_networks();
        *mock.fail_network_inspect.lock().unwrap() = None;
        mock.requests.lock().unwrap().clear();
        refuse(&mock);

        backend.create("ep-new", "tok").await.unwrap_err();

        let requests = mock.requests.lock().unwrap().clone();
        assert!(
            requests.iter().all(|r| r.starts_with("GET ") && r.contains("/networks/")),
            "only the networks were inspected: {requests:?}"
        );
        assert_eq!(
            settled_deletes(&mock, Duration::from_millis(100)).await,
            Vec::<String>::new()
        );
    }
}

// No cached pass: a network recreated under the same name without `internal` is refused at the
// next create.
#[tokio::test]
async fn the_network_is_inspected_on_every_create() {
    let (mock, backend) = mock_backend().await;

    backend.create("ep-a", "tok").await.unwrap();
    backend.create("ep-b", "tok").await.unwrap();
    assert_eq!(network_inspects(&mock, "stack_default"), 2);
    assert_eq!(network_inspects(&mock, "stack_control"), 2);

    set_network(
        &mock,
        "stack_control",
        network_json("stack_control", false, None, None),
    );
    let err = backend.create("ep-c", "tok").await.unwrap_err();
    assert!(format!("{err:#}").contains("network stack_control"), "{err:#}");
    assert_eq!(container_creates(&mock), 2);
}

// The body the engine receives carries the dropped capabilities and, when configured, the
// resolver.
#[tokio::test]
async fn the_engine_receives_the_resolver_and_no_capability() {
    let mock = Mock::default();
    *mock.networks.lock().unwrap() = good_networks();
    let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let app = Router::new()
        .fallback({
            let bodies = bodies.clone();
            move |state: State<Mock>, request: Request| {
                let bodies = bodies.clone();
                async move {
                    if request.uri().path().ends_with("/containers/create") {
                        let (parts, body) = request.into_parts();
                        let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
                        bodies.lock().unwrap().push(serde_json::from_slice(&bytes).unwrap());
                        return engine(state, Request::from_parts(parts, axum::body::Body::from(bytes))).await;
                    }
                    engine(state, request).await
                }
            }
        })
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let docker =
        bollard::Docker::connect_with_http(&format!("http://{addr}"), 10, bollard::API_DEFAULT_VERSION)
            .unwrap();
    let backend = DockerBackend::with_docker(
        settings(),
        DockerSettings {
            egress_dns: Some("10.214.8.2".parse().unwrap()),
            ..docker_settings()
        },
        docker,
    );

    backend.create("ep-new", "tok").await.unwrap();

    let bodies = bodies.lock().unwrap();
    let host = &bodies[0]["HostConfig"];
    assert_eq!(host["Dns"], serde_json::json!(["10.214.8.2"]), "{host}");
    assert_eq!(host["DnsOptions"], serde_json::json!(["timeout:1", "attempts:1"]), "{host}");
    assert_eq!(host["CapDrop"], serde_json::json!(["ALL"]), "{host}");
    assert!(host.get("ExtraHosts").is_none_or(|h| h.is_null()), "{host}");
}

// --- compute-network-reach m2: computes, their networks and the legacy networks ----------

#[tokio::test]
async fn compute_networks_lists_this_stacks_computes_by_network_id() {
    let (mock, backend) = mock_backend().await;
    *mock.containers.lock().unwrap() = vec![
        ("ep-good", STACK, "running"),
        ("ep-old", STACK, "running"),
        ("ep-new", STACK, "created"),
        ("ep-other", "other", "running"),
    ];
    mock.attachments.lock().unwrap().insert(
        "ep-old",
        vec![("stack_compute_old", "nid-old"), ("stack_control", "nid-stack_control")],
    );

    let mut found = backend.compute_networks().await.unwrap();
    found.computes.sort_by(|a, b| a.endpoint_id.cmp(&b.endpoint_id));

    let expected: Vec<(String, String)> = found
        .expected
        .iter()
        .map(|n| (n.name.clone(), n.id.clone()))
        .collect();
    assert_eq!(
        expected,
        [
            ("stack_default".to_string(), "nid-stack_default".to_string()),
            ("stack_control".to_string(), "nid-stack_control".to_string())
        ]
    );
    let summary: Vec<(String, String, bool, Vec<String>)> = found
        .computes
        .iter()
        .map(|c| {
            let mut ids: Vec<String> = c.networks.iter().map(|n| n.id.clone()).collect();
            ids.sort();
            (c.endpoint_id.clone(), c.container_id.clone(), c.mid_create, ids)
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("ep-good".into(), "cid-ep-good".into(), false, vec!["nid-stack_control".into(), "nid-stack_default".into()]),
            ("ep-new".into(), "cid-ep-new".into(), true, vec!["nid-stack_control".into(), "nid-stack_default".into()]),
            ("ep-old".into(), "cid-ep-old".into(), false, vec!["nid-old".into(), "nid-stack_control".into()]),
        ]
    );
}

// Only containers with the endpoint label and this stack's label are listed, also from an
// engine that ignores the filter: a container without them is never a candidate for removal.
#[tokio::test]
async fn a_container_without_the_stack_label_is_left_alone() {
    let (mock, backend) = mock_backend().await;
    *mock.containers.lock().unwrap() = vec![("ep-other", "other", "running")];
    *mock.raw_containers.lock().unwrap() = vec![
        serde_json::json!({
            "Id": "cid-unlabelled", "Names": ["/someone"], "State": "running",
            "Labels": {}, "NetworkSettings": { "Networks": { "stack_compute_old": { "NetworkID": "nid-old" } } },
        }),
        serde_json::json!({
            "Id": "cid-stack-only", "Names": ["/stack-only"], "State": "running",
            "Labels": { STACK_LABEL: STACK }, "NetworkSettings": { "Networks": { "stack_compute_old": { "NetworkID": "nid-old" } } },
        }),
        serde_json::json!({
            "Id": "cid-endpoint-only", "Names": ["/endpoint-only"], "State": "running",
            "Labels": { ENDPOINT_LABEL: "ep-x" }, "NetworkSettings": { "Networks": { "stack_compute_old": { "NetworkID": "nid-old" } } },
        }),
    ];
    for ignore in [false, true] {
        *mock.ignore_filters.lock().unwrap() = ignore;
        let found = backend.compute_networks().await.unwrap();
        assert_eq!(found.computes, Vec::new(), "ignore_filters={ignore}");
    }
}

// The expected networks are looked up by id each time; one that can't be read fails the call
// (nothing can be judged against it).
#[tokio::test]
async fn compute_networks_fails_without_the_configured_networks() {
    let (mock, backend) = mock_backend().await;
    mock.networks.lock().unwrap().remove("stack_control");
    let err = backend.compute_networks().await.unwrap_err();
    assert!(format!("{err:#}").contains("stack_control"), "{err:#}");
    *mock.networks.lock().unwrap() = good_networks();
    *mock.fail_network_inspect.lock().unwrap() = Some("stack_default");
    assert!(backend.compute_networks().await.is_err());
}

#[tokio::test]
async fn legacy_networks_are_read_by_their_exact_names() {
    use chelabase_neon::backend::LegacyNetwork;
    let (mock, backend) = mock_backend().await;
    let mut occupied = network_json("stack_compute_old", false, None, None);
    occupied["Containers"] = serde_json::json!({ "abc": { "Name": "foreign-container" } });
    set_network(&mock, "stack_compute_old", occupied);

    let found = backend.legacy_networks().await;
    assert_eq!(found.len(), 2, "{found:?}");
    match &found[0] {
        LegacyNetwork::Present { network, members } => {
            assert_eq!(network.name, "stack_compute_old");
            assert_eq!(network.id, "nid-stack_compute_old");
            assert_eq!(members, &["foreign-container".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        found[1],
        LegacyNetwork::Missing {
            name: "stack_control_old".into()
        }
    );

    // An answer for another name (Docker matched an id prefix) is not that network.
    set_network(&mock, "stack_control_old", network_json("someone_elses", false, None, None));
    *mock.fail_network_inspect.lock().unwrap() = Some("stack_compute_old");
    let found = backend.legacy_networks().await;
    assert!(
        matches!(&found[0], LegacyNetwork::Unreadable { name, .. } if name == "stack_compute_old"),
        "{found:?}"
    );
    assert_eq!(
        found[1],
        LegacyNetwork::Missing {
            name: "stack_control_old".into()
        }
    );
}

// Docker's network inspect lists only active endpoints, and Docker deletes a network that only
// stopped containers reference. A stopped container attached to a legacy network keeps it.
#[tokio::test]
async fn a_stopped_foreign_container_keeps_a_legacy_network() {
    use chelabase_neon::backend::LegacyNetwork;
    let (mock, backend) = mock_backend().await;
    set_network(&mock, "stack_compute_old", network_json("stack_compute_old", false, None, None));
    *mock.raw_containers.lock().unwrap() = vec![
        serde_json::json!({
            "Id": "cid-stopped", "Names": ["/stopped-foreign"], "State": "exited",
            "Labels": {}, "NetworkSettings": { "Networks": { "stack_compute_old": { "NetworkID": "nid-stack_compute_old" } } },
        }),
        serde_json::json!({
            "Id": "cid-elsewhere", "Names": ["/elsewhere"], "State": "exited",
            "Labels": {}, "NetworkSettings": { "Networks": { "bridge": { "NetworkID": "nid-bridge" } } },
        }),
    ];

    let found = backend.legacy_networks().await;

    match &found[0] {
        LegacyNetwork::Present { members, .. } => {
            assert_eq!(members, &["stopped-foreign".to_string()]);
        }
        other => panic!("{other:?}"),
    }
    let listing: Vec<String> = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("GET ") && r.contains("/containers/json"))
        .cloned()
        .collect();
    assert!(listing.iter().any(|r| r.contains("all=true") || r.contains("all=1")), "{listing:?}");
}

#[tokio::test]
async fn a_legacy_network_is_removed_by_id_unless_occupied() {
    use chelabase_neon::backend::{NetworkRef, NetworkRemoval};
    let (mock, backend) = mock_backend().await;
    set_network(&mock, "stack_compute_old", network_json("stack_compute_old", false, None, None));
    let mut occupied = network_json("stack_control_old", false, None, None);
    occupied["Containers"] = serde_json::json!({ "abc": { "Name": "foreign-container" } });
    set_network(&mock, "stack_control_old", occupied);
    let r = |name: &str| NetworkRef {
        name: name.into(),
        id: format!("nid-{name}"),
    };

    assert_eq!(backend.remove_network(&r("stack_compute_old")).await.unwrap(), NetworkRemoval::Removed);
    assert_eq!(backend.remove_network(&r("stack_compute_old")).await.unwrap(), NetworkRemoval::Gone);
    assert_eq!(backend.remove_network(&r("stack_control_old")).await.unwrap(), NetworkRemoval::Occupied);
    let removals: Vec<String> = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.starts_with("DELETE ") && r.contains("/networks/"))
        .cloned()
        .collect();
    assert!(removals.iter().all(|r| r.contains("/networks/nid-")), "by id: {removals:?}");
}

#[tokio::test]
async fn a_compute_container_is_removed_by_id() {
    let (mock, backend) = mock_backend().await;

    backend.remove_compute_container("ep-old", "cid-ep-old").await.unwrap();

    let removed = deletes(&mock);
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert!(removed[0].contains("/containers/cid-ep-old?"), "{removed:?}");
    assert!(removed[0].contains("force=true"), "{removed:?}");
    *mock.fail_delete.lock().unwrap() = true;
    assert!(backend.remove_compute_container("ep-old", "cid-ep-old").await.is_err());
}
