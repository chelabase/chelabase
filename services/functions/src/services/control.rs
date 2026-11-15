//! m11: the orchestrator's container-service calls, on the internal listener's control side with
//! the orchestrator's bearer.
//!
//! - `POST /internal/services/images/resolve` `{image}` → `{image}`: the registry digest
//!   reference a deploy stores (422 `invalid_image`).
//! - `POST /internal/services/rollout` `{branch_id, function_id, deployment_id}`: ensures the
//!   deployment's container with the branch's secrets and waits for it to pass its health check
//!   (up to 60 s); 200 once it does, 409 `unhealthy` when it never did (the container is gone and
//!   the active deployment still serves), 422 `invalid_spec` when the runner refused it.
//! - `GET /internal/services/logs/{branch_id}/{function_id}/{deployment_id}`: its container's
//!   last 200 lines (404 `no_container` before the keeper or a rollover placed one).
//!
//! Without the runner (the `services` profile off) every call is 503 `runner_unavailable`.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::container::{ROLLOUT_TIMEOUT, RolloutError, rollout};
use super::runner::{Ensure, RunnerError};
use crate::app::{AppState, Peer, error, require};

type Shared = Arc<AppState>;

fn runner_error(e: RunnerError) -> Response<Body> {
    match e {
        RunnerError::Refused(why) => error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_spec", &why),
        RunnerError::NotFound => error(
            StatusCode::NOT_FOUND,
            "no_container",
            "the container is gone",
        ),
        RunnerError::Unavailable(why) => {
            tracing::warn!("service runner: {why}");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "runner_unavailable",
                "the service runner is unavailable (is the services profile on?)",
            )
        }
    }
}

fn unavailable() -> Response<Body> {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "runner_unavailable",
        "container services are unavailable",
    )
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Box<Response<Body>>> {
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
struct Image {
    image: String,
}

pub(crate) async fn resolve_image(
    State(s): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if let Err(r) = require(&s, &headers, Peer::Orchestrator) {
        return *r;
    }
    let Image { image } = match parse(&body) {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let Some(services) = s.services.as_ref() else {
        return unavailable();
    };
    match services.runner.resolve_image(&image).await {
        Ok(image) => axum::Json(json!({ "image": image })).into_response(),
        Err(RunnerError::Refused(why)) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_image", &why)
        }
        Err(e) => runner_error(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rollout {
    branch_id: Uuid,
    function_id: Uuid,
    deployment_id: Uuid,
}

pub(crate) async fn rollout_deployment(
    State(s): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    if let Err(r) = require(&s, &headers, Peer::Orchestrator) {
        return *r;
    }
    let req: Rollout = match parse(&body) {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let Some(services) = s.services.as_ref() else {
        return unavailable();
    };
    let directory = s.resolver.directory();
    let spec = match directory
        .deployment_spec(req.function_id, req.deployment_id)
        .await
    {
        Ok(Some(spec)) if spec.get("backend").and_then(|b| b.as_str()) == Some("container") => spec,
        Ok(Some(_)) => {
            return error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_spec",
                "not a container deployment",
            );
        }
        Ok(None) => return error(StatusCode::NOT_FOUND, "not_found", "no such deployment"),
        Err(e) => {
            tracing::warn!("read a deployment: {e}");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "the metadata is unavailable",
            );
        }
    };
    let env = match directory
        .container_env(req.branch_id, req.function_id)
        .await
    {
        Ok(env) => env,
        Err(e) => {
            tracing::warn!("a container service's env: {e}");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "the service's secrets are unavailable",
            );
        }
    };
    let ensure = Ensure {
        service_id: req.function_id,
        branch_id: req.branch_id,
        deployment_id: req.deployment_id,
        spec: &spec,
        env: &env,
    };
    match rollout(
        services.runner.as_ref(),
        services.probe.as_ref(),
        &ensure,
        ROLLOUT_TIMEOUT,
    )
    .await
    {
        Ok(placed) => {
            let id = placed.container_id.clone();
            services
                .containers
                .pin(req.branch_id, req.function_id, req.deployment_id, placed);
            axum::Json(json!({ "container_id": id })).into_response()
        }
        Err(RolloutError::Runner(e)) => runner_error(e),
        Err(RolloutError::Invalid(why)) => {
            error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_spec", &why)
        }
        Err(RolloutError::Unhealthy(why)) => error(StatusCode::CONFLICT, "unhealthy", &why),
    }
}

pub(crate) async fn logs(
    State(s): State<Shared>,
    headers: HeaderMap,
    // Strings, parsed after the bearer check: a caller without it gets 401, never a parse error.
    Path((branch, function, deployment)): Path<(String, String, String)>,
) -> Response<Body> {
    if let Err(r) = require(&s, &headers, Peer::Orchestrator) {
        return *r;
    }
    let Some(services) = s.services.as_ref() else {
        return unavailable();
    };
    let ids = (
        Uuid::parse_str(&branch),
        Uuid::parse_str(&function),
        Uuid::parse_str(&deployment),
    );
    let placed = match ids {
        (Ok(b), Ok(f), Ok(d)) => services.containers.get(b, f, d),
        _ => None,
    };
    let Some(placed) = placed else {
        return error(
            StatusCode::NOT_FOUND,
            "no_container",
            "the service has no running container on this branch",
        );
    };
    match services.runner.logs(&placed.container_id).await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
        Err(e) => runner_error(e),
    }
}
