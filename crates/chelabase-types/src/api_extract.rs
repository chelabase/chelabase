//! Extractors whose rejection is an [`ApiError`] (JSON, stable codes).

use crate::api_error::{ApiError, ErrorCode};
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{FromRequest, FromRequestParts, Path, Request};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

/// `axum::Json` with JSON rejections: 415/400/422 are `invalid_request`, 413 is `too_large`.
pub struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(Self(v)),
            Err(rej) => Err(json_rejection(&rej)),
        }
    }
}

fn json_rejection(rej: &JsonRejection) -> ApiError {
    let status = rej.status();
    let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
        ErrorCode::TooLarge
    } else {
        ErrorCode::InvalidRequest
    };
    ApiError::new(status.as_u16(), code, rej.body_text())
}

/// `axum::extract::Path` with a JSON 400 `invalid_request` rejection.
pub struct ApiPath<T>(pub T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match Path::<T>::from_request_parts(parts, state).await {
            Ok(Path(v)) => Ok(Self(v)),
            Err(rej) => Err(path_rejection(&rej)),
        }
    }
}

fn path_rejection(rej: &PathRejection) -> ApiError {
    ApiError::invalid_request(400, format!("invalid path parameter: {}", rej.body_text()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::DefaultBodyLimit;
    use axum::http::{Request, header};
    use axum::routing::post;
    use serde::Deserialize;
    use tower::ServiceExt;

    #[derive(Deserialize)]
    struct Payload {
        #[allow(dead_code)]
        n: u32,
    }

    async fn run(req: Request<Body>) -> (u16, serde_json::Value) {
        let app = Router::new()
            .route("/j", post(|ApiJson(_p): ApiJson<Payload>| async { "ok" }))
            .route(
                "/p/{id}",
                axum::routing::get(|ApiPath(_id): ApiPath<u32>| async { "ok" }),
            )
            .layer(DefaultBodyLimit::max(16));
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status().as_u16();
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/json");
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn post_json(ct: Option<&str>, body: &'static str) -> Request<Body> {
        let mut b = Request::post("/j");
        if let Some(ct) = ct {
            b = b.header(header::CONTENT_TYPE, ct);
        }
        b.body(Body::from(body)).unwrap()
    }

    fn assert_shape(v: &serde_json::Value, code: &str) {
        assert_eq!(v["error"]["code"], code);
        assert!(
            v["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty())
        );
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["error"].as_object().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn api_json_rejections_are_json() {
        let (s, v) = run(post_json(Some("text/plain"), r#"{"n":1}"#)).await;
        assert_eq!(s, 415);
        assert_shape(&v, "invalid_request");
        let (s, v) = run(post_json(None, r#"{"n":1}"#)).await;
        assert_eq!(s, 415);
        assert_shape(&v, "invalid_request");
        let (s, v) = run(post_json(Some("application/json"), "{nope")).await;
        assert_eq!(s, 400);
        assert_shape(&v, "invalid_request");
        let (s, v) = run(post_json(Some("application/json"), r#"{"n":"x"}"#)).await;
        assert_eq!(s, 422);
        assert_shape(&v, "invalid_request");
        let (s, v) = run(post_json(
            Some("application/json"),
            r#"{"n":1,"pad":"0123456789012345678901234567890"}"#,
        ))
        .await;
        assert_eq!(s, 413);
        assert_shape(&v, "too_large");
        // A valid body passes through (the handler's plain "ok" is not JSON, so call directly).
        let app = Router::new().route(
            "/j",
            post(|ApiJson(p): ApiJson<Payload>| async move { p.n.to_string() }),
        );
        let r = app
            .oneshot(post_json(Some("application/json"), r#"{"n":5}"#))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn api_path_rejection_is_json() {
        let (s, v) = run(Request::get("/p/abc").body(Body::empty()).unwrap()).await;
        assert_eq!(s, 400);
        assert_shape(&v, "invalid_request");
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("invalid path parameter: ")
        );
    }
}
