//! F4 (T8 m3): a `create` that fails after the container exists (joining the control network,
//! uploading the TLS files, starting it) removes the container it made, best effort, and still
//! returns the step's own error. Against a mock Docker Engine API on a TCP port, handed to the
//! backend as its client (`DockerBackend::with_docker`), so no daemon is needed.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_neon::compute_tls::ComputeCa;
use chelabase_neon::{ComputeBackend, ComputeSettings, DockerBackend};

#[derive(Clone, Default)]
struct Mock {
    /// `METHOD path?query` of every request, in order.
    requests: Arc<Mutex<Vec<String>>>,
    /// The step that answers 500: "connect", "archive" or "start".
    fail_step: Arc<Mutex<Option<&'static str>>>,
    /// DELETE answers 500.
    fail_delete: Arc<Mutex<bool>>,
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
    let boom = || (StatusCode::INTERNAL_SERVER_ERROR, r#"{"message":"boom"}"#).into_response();
    if method == "POST" && path.ends_with("/containers/create") {
        return (
            StatusCode::CREATED,
            [("content-type", "application/json")],
            r#"{"Id":"cid0123456789","Warnings":[]}"#,
        )
            .into_response();
    }
    if method == "POST" && path.contains("/networks/") && path.ends_with("/connect") {
        return if fail == Some("connect") {
            boom()
        } else {
            StatusCode::OK.into_response()
        };
    }
    if method == "PUT" && path.ends_with("/archive") {
        return if fail == Some("archive") {
            boom()
        } else {
            StatusCode::OK.into_response()
        };
    }
    if method == "POST" && path.ends_with("/start") {
        return if fail == Some("start") {
            boom()
        } else {
            StatusCode::NO_CONTENT.into_response()
        };
    }
    if method == "DELETE" && path.contains("/containers/") {
        return if *mock.fail_delete.lock().unwrap() {
            boom()
        } else {
            StatusCode::NO_CONTENT.into_response()
        };
    }
    (StatusCode::NOT_FOUND, r#"{"message":"unexpected"}"#).into_response()
}

fn settings() -> ComputeSettings {
    ComputeSettings {
        image: "neon/compute-node-v17:test".into(),
        network: "stack_default".into(),
        control_plane_uri: "http://orchestrator:8083".into(),
        stack: "mock".into(),
        // A separate control network, so the join step is a request.
        control_network: "stack_control".into(),
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

#[tokio::test]
async fn a_failed_create_removes_the_container_it_made() {
    let mock = Mock::default();
    let app = Router::new().fallback(engine).with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // A client aimed at the mock, never at the machine's Docker daemon (`DockerBackend::connect`
    // reads the local socket): the backend takes the client it should use.
    let docker = bollard::Docker::connect_with_http(
        &format!("http://{addr}"),
        10,
        bollard::API_DEFAULT_VERSION,
    )
    .unwrap();
    let backend = DockerBackend::with_docker(settings(), docker);

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
