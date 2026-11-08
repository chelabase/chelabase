//! The internal API. Each caller presents its own bearer, compared in constant time: the
//! functions service (`tokens/functions-service-runner.token`) may make every call; the worker
//! (`tokens/worker-service-runner.token`) every call but ensure, the one that carries a branch's
//! secrets. No or an unknown bearer is 401, the worker's bearer on ensure 403.
//!
//! - `POST /internal/containers/ensure` `{service_id, branch_id, deployment_id, spec, env}`
//! - `POST /internal/containers/{start|stop|remove}` `{container}` (an id or a name)
//! - `GET /internal/containers/{container}/status`, `GET /internal/containers/{container}/logs`
//! - `POST /internal/images/resolve` `{image}` → `{image}`, its registry digest reference (m11;
//!   the functions service's bearer only: it pulls)
//! - `GET /health` (no bearer)

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::runner::{EnsureRequest, Runner, RunnerError, Status};

pub struct AppState {
    pub runner: Arc<Runner>,
    pub functions_token: String,
    pub worker_token: String,
}

type Shared = Arc<AppState>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Caller {
    Functions,
    Worker,
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/internal/containers/ensure", post(ensure))
        .route("/internal/containers/start", post(start))
        .route("/internal/containers/stop", post(stop))
        .route("/internal/containers/remove", post(remove))
        .route("/internal/containers/{container}/status", get(status))
        .route("/internal/containers/{container}/logs", get(logs))
        .route("/internal/images/resolve", post(resolve_image))
        .method_not_allowed_fallback(|| async {
            error(StatusCode::NOT_FOUND, "not_found", "no route")
        })
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "no route") })
        .with_state(state)
}

fn error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    (
        status,
        axum::Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn refusal(e: RunnerError) -> Response<Body> {
    match e {
        RunnerError::NotFound => error(StatusCode::NOT_FOUND, "not_found", "no such container"),
        RunnerError::Invalid(e) => error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_spec", &e.0),
        RunnerError::Backend(e) => {
            tracing::warn!("container engine: {e:#}");
            error(
                StatusCode::BAD_GATEWAY,
                "engine_unavailable",
                "the container engine refused or failed the call",
            )
        }
    }
}

/// The caller the bearer names, if it's one of `allowed`.
fn require(
    s: &AppState,
    headers: &HeaderMap,
    allowed: &[Caller],
) -> Result<(), Box<Response<Body>>> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .as_bytes();
    let functions = bool::from(bearer.ct_eq(s.functions_token.as_bytes()));
    let worker = bool::from(bearer.ct_eq(s.worker_token.as_bytes()));
    let caller = match (functions, worker) {
        (true, _) => Caller::Functions,
        (_, true) => Caller::Worker,
        _ => {
            return Err(Box::new(error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a valid bearer is required",
            )));
        }
    };
    if allowed.contains(&caller) {
        Ok(())
    } else {
        Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this bearer can't make this call",
        )))
    }
}

const ANY: &[Caller] = &[Caller::Functions, Caller::Worker];

fn json_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Box<Response<Body>>> {
    serde_json::from_slice(body).map_err(|e| {
        Box::new(error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            &format!("invalid request body: {e}"),
        ))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    container: String,
}

fn status_response(s: Status) -> Response<Body> {
    axum::Json(s).into_response()
}

async fn ensure(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if let Err(r) = require(&s, &headers, &[Caller::Functions]) {
        return *r;
    }
    let req: EnsureRequest = match json_body(&body) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    match s.runner.ensure(&req).await {
        Ok(c) => status_response(s.runner.status_of(&c)),
        Err(e) => refusal(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    image: String,
}

async fn resolve_image(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    if let Err(r) = require(&s, &headers, &[Caller::Functions]) {
        return *r;
    }
    let Image { image } = match json_body(&body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    match s.runner.resolve_image(&image).await {
        Ok(image) => axum::Json(json!({ "image": image })).into_response(),
        Err(e) => refusal(e),
    }
}

async fn lifecycle(s: Shared, headers: HeaderMap, body: Bytes, op: &str) -> Response<Body> {
    if let Err(r) = require(&s, &headers, ANY) {
        return *r;
    }
    let Named { container } = match json_body(&body) {
        Ok(n) => n,
        Err(r) => return *r,
    };
    let result = match op {
        "start" => s.runner.start(&container).await.map(Some),
        "stop" => s.runner.stop(&container).await.map(Some),
        _ => s.runner.remove(&container).await.map(|()| None),
    };
    match result {
        Ok(Some(st)) => status_response(st),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refusal(e),
    }
}

async fn start(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    lifecycle(s, headers, body, "start").await
}

async fn stop(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    lifecycle(s, headers, body, "stop").await
}

async fn remove(State(s): State<Shared>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    lifecycle(s, headers, body, "remove").await
}

async fn status(
    State(s): State<Shared>,
    headers: HeaderMap,
    Path(container): Path<String>,
) -> Response<Body> {
    if let Err(r) = require(&s, &headers, ANY) {
        return *r;
    }
    match s.runner.status(&container).await {
        Ok(st) => status_response(st),
        Err(e) => refusal(e),
    }
}

async fn logs(
    State(s): State<Shared>,
    headers: HeaderMap,
    Path(container): Path<String>,
) -> Response<Body> {
    if let Err(r) = require(&s, &headers, ANY) {
        return *r;
    }
    match s.runner.logs(&container).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(e) => refusal(e),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::runner::fake::Fake;
    use crate::spec::Caps;
    use crate::{SERVICE_LABEL, STACK_LABEL};

    const FN: &str = "functions-bearer-0123456789abcdef0123456789";
    const WK: &str = "worker-bearer-0123456789abcdef0123456789abc";

    fn app(fake: &Arc<Fake>) -> Router {
        router(Arc::new(AppState {
            runner: Arc::new(Runner::new(fake.clone(), "a", "net", Caps::default())),
            functions_token: FN.into(),
            worker_token: WK.into(),
        }))
    }

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        use tower::ServiceExt as _;
        let mut req = Request::builder().method(method).uri(path);
        if let Some(b) = bearer {
            req = req.header(header::AUTHORIZATION, b);
        }
        let req = req
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn ensure_body() -> Value {
        json!({
            "service_id": Uuid::new_v4(), "branch_id": Uuid::new_v4(), "deployment_id": Uuid::new_v4(),
            "spec": {"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080},
            "env": {"API_KEY": "k"}
        })
    }

    /// The bearer matrix: missing, malformed or unknown → 401 on every route; the worker's on
    /// ensure → 403; each caller's own calls pass.
    #[tokio::test]
    async fn bearer_matrix() {
        let fake = Arc::new(Fake::default());
        let app = app(&fake);
        let svc = Uuid::new_v4().to_string();
        let id = fake.add(
            "mine",
            &[(STACK_LABEL, "a"), (SERVICE_LABEL, &svc)],
            "running",
        );
        let routes: Vec<(&str, String, Option<Value>)> = vec![
            (
                "POST",
                "/internal/containers/ensure".into(),
                Some(ensure_body()),
            ),
            (
                "POST",
                "/internal/containers/start".into(),
                Some(json!({"container": id})),
            ),
            (
                "POST",
                "/internal/containers/stop".into(),
                Some(json!({"container": id})),
            ),
            ("GET", format!("/internal/containers/{id}/status"), None),
            ("GET", format!("/internal/containers/{id}/logs"), None),
            (
                "POST",
                "/internal/containers/remove".into(),
                Some(json!({"container": id})),
            ),
        ];
        let wrong = [
            None,
            Some(FN.to_string()),
            Some(format!("Basic {FN}")),
            Some("Bearer ".to_string()),
            Some(format!("Bearer {}", &FN[..FN.len() - 1])),
            Some(format!("Bearer {FN}x")),
            Some(format!("bearer {FN}")),
        ];
        for (m, p, b) in &routes {
            for w in &wrong {
                let (st, body) = call(&app, m, p, w.as_deref(), b.clone()).await;
                assert_eq!(st, StatusCode::UNAUTHORIZED, "{m} {p} {w:?}");
                assert_eq!(body["error"]["code"], "unauthorized");
            }
        }
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        let (st, _) = call(
            &app,
            "POST",
            "/internal/containers/ensure",
            Some(&format!("Bearer {WK}")),
            Some(ensure_body()),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert!(fake.calls().is_empty());
        for (m, p, b) in routes.iter().skip(1) {
            let (st, _) = call(&app, m, p, Some(&format!("Bearer {WK}")), b.clone()).await;
            assert!(st.is_success(), "worker: {m} {p}: {st}");
        }
        let (st, body) = call(
            &app,
            "POST",
            "/internal/containers/ensure",
            Some(&format!("Bearer {FN}")),
            Some(ensure_body()),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "running");
        assert_eq!(body["port"], 8080);
        assert!(body["address"].is_string(), "{body}");
        // m11: resolving an image pulls, so it's the functions service's call alone.
        let resolve = json!({"image": format!("ghcr.io/acme/echo@sha256:{}", "a".repeat(64))});
        for w in &wrong {
            let (st, _) = call(
                &app,
                "POST",
                "/internal/images/resolve",
                w.as_deref(),
                Some(resolve.clone()),
            )
            .await;
            assert_eq!(st, StatusCode::UNAUTHORIZED, "{w:?}");
        }
        let (st, _) = call(
            &app,
            "POST",
            "/internal/images/resolve",
            Some(&format!("Bearer {WK}")),
            Some(resolve.clone()),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, body) = call(
            &app,
            "POST",
            "/internal/images/resolve",
            Some(&format!("Bearer {FN}")),
            Some(resolve.clone()),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        assert_eq!(body["image"], resolve["image"]);
        let (st, body) = call(
            &app,
            "POST",
            "/internal/images/resolve",
            Some(&format!("Bearer {FN}")),
            Some(json!({"image": "localhost/x:1"})),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"]["code"], "invalid_spec");
        let (st, _) = call(&app, "GET", "/health", None, None).await;
        assert_eq!(st, StatusCode::OK);
    }

    /// Refusals over HTTP: a bad spec or an unknown request key is 422 and creates nothing; an
    /// unlabelled container is 404 and untouched.
    #[tokio::test]
    async fn refusals() {
        let fake = Arc::new(Fake::default());
        let app = app(&fake);
        let fnb = format!("Bearer {FN}");
        for (k, v) in [
            ("privileged", json!(true)),
            ("binds", json!(["/:/h"])),
            ("network_mode", json!("host")),
        ] {
            let mut b = ensure_body();
            b["spec"][k] = v.clone();
            let (st, body) = call(
                &app,
                "POST",
                "/internal/containers/ensure",
                Some(&fnb),
                Some(b),
            )
            .await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{k}");
            assert_eq!(body["error"]["code"], "invalid_spec");
            let mut b = ensure_body();
            b[k] = v;
            let (st, _) = call(
                &app,
                "POST",
                "/internal/containers/ensure",
                Some(&fnb),
                Some(b),
            )
            .await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "top-level {k}");
        }
        let mut b = ensure_body();
        b["spec"]["resources"] = json!({"memory_mb": 1 << 20});
        let (st, _) = call(
            &app,
            "POST",
            "/internal/containers/ensure",
            Some(&fnb),
            Some(b),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(fake.calls().is_empty());
        let db = fake.add(
            "metadata-db",
            &[("com.docker.compose.service", "metadata-db")],
            "running",
        );
        for op in ["stop", "remove", "start"] {
            for name in [db.as_str(), "metadata-db"] {
                let (st, _) = call(
                    &app,
                    "POST",
                    &format!("/internal/containers/{op}"),
                    Some(&fnb),
                    Some(json!({"container": name})),
                )
                .await;
                assert_eq!(st, StatusCode::NOT_FOUND, "{op} {name}");
            }
        }
        let (st, _) = call(
            &app,
            "GET",
            &format!("/internal/containers/{db}/logs"),
            Some(&fnb),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert!(fake.calls().is_empty());
        assert_eq!(fake.get(&db).unwrap().state, "running");
    }
}
