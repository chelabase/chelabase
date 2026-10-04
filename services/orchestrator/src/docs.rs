//! Interactive API docs: a self-hosted Scalar API reference (the bundle is embedded in the
//! binary, no CDN, no calls to scalar.com) and the Management API's OpenAPI document it loads.
//! Plain routes, so they are not in the document itself.
//!
//! The bundle is `dist/browser/standalone.js` of `@scalar/api-reference`, byte for byte;
//! `assets/scalar/VERSION` records the version and its sha256.

use std::sync::LazyLock;

use axum::Router;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chelabase_routes::orchestrator as routes;
use chelabase_types::api_error::ApiError;

/// The document, built once.
static DOCUMENT: LazyLock<String> =
    LazyLock::new(|| serde_json::to_string(&crate::openapi()).expect("the document serializes"));

static SCALAR_JS: &[u8] = include_bytes!("../assets/scalar/standalone.js");

/// Our page with the bundle's version in the script URL (the first 12 hex of its sha256, the
/// one `assets/scalar/VERSION` records), so an upgrade busts the day-long script cache.
static INDEX: LazyLock<String> = LazyLock::new(|| {
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(SCALAR_JS));
    PAGE.replace("__SCALAR_V__", &hash[..12])
});

/// Our page. Scalar is configured here: no telemetry, no default fonts, the `bearer`
/// scheme preferred. Scalar 1.72.2 falls back to proxy.scalar.com when `proxyUrl` is
/// undefined (`iS=(e,t)=>e===void 0?xHe(t):e`, web layout), so every scalar.com service
/// URL is set to an empty string and the agent, MCP and developer tools are off.
const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Chelabase Management API</title>
</head>
<body>
<div id="app"></div>
<script src="/v1/docs/scalar.js?v=__SCALAR_V__"></script>
<script>
Scalar.createApiReference("#app", {
  url: "/v1/openapi.json",
  telemetry: false,
  withDefaultFonts: false,
  authentication: { preferredSecurityScheme: "bearer" },
  proxyUrl: "",
  externalUrls: { dashboardUrl: "", registryUrl: "", proxyUrl: "", apiBaseUrl: "" },
  agent: { disabled: true },
  mcp: { disabled: true },
  showDeveloperTools: "never",
});
</script>
</body>
</html>
"##;

pub(crate) fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(routes::OPENAPI_JSON.path, get(openapi_json))
        .route(routes::API_DOCS.path, get(index))
        .route(routes::API_DOCS_SCRIPT.path, get(script))
        .route("/v1/docs/{*rest}", get(unknown))
}

async fn openapi_json() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        DOCUMENT.as_str(),
    )
        .into_response()
}

async fn index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX.as_str()).into_response()
}

async fn script() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        SCALAR_JS,
    )
        .into_response()
}

async fn unknown() -> Response {
    ApiError::not_found("not found").into_response()
}
