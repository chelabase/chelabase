//! The internal listener (`CHELABASE_MCP_INTERNAL_LISTEN`), for `mcp-runtime` only: m8 (#352)
//! puts the two alone on a network. It serves the spec bundle the runtime fetches for `search`
//! (`GET /internal/spec`), and holds the registry of live runs that m11 (#355)'s callback
//! (`POST /internal/exec/{exec_id}/request`) checks. The public listener has no `/internal/`
//! route.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::Router;
use axum::extract::Query;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::Caller;
use crate::sandbox::Kind;
use crate::tools::BUNDLE;

/// The header that carries the served bundle's sha256.
pub const SPEC_HASH_HEADER: &str = "x-spec-hash";

static BUNDLE_SHA256: LazyLock<String> = LazyLock::new(|| {
    Sha256::digest(BUNDLE.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
});

/// The sha256 (lowercase hex) of the bundle's bytes: the `spec_hash` a `search` run sends, which
/// the runtime checks the fetched bytes against. (The bundle's own `info.x-spec-hash` hashes its
/// canonical form, so it cannot be that value.)
pub fn bundle_sha256() -> &'static str {
    &BUNDLE_SHA256
}

/// What a live run acts with: m11's callback finds it by `exec_id`.
#[derive(Debug)]
pub struct Exec {
    pub kind: Kind,
    pub caller: Caller,
    pub destructive: bool,
}

/// The runs in flight, by `exec_id`. Cheap to clone.
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<HashMap<String, Arc<Exec>>>>);

/// A run's entry in the [`Registry`], removed when dropped: whatever way the run ends.
pub struct Registration {
    registry: Registry,
    exec_id: String,
}

impl Registry {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Exec>>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Registers `exec_id` until the returned guard drops.
    pub fn register(&self, exec_id: &str, exec: Exec) -> Registration {
        self.map().insert(exec_id.to_owned(), Arc::new(exec));
        Registration {
            registry: self.clone(),
            exec_id: exec_id.to_owned(),
        }
    }

    /// The live run `exec_id`, if any.
    pub fn get(&self, exec_id: &str) -> Option<Arc<Exec>> {
        self.map().get(exec_id).cloned()
    }

    pub fn contains(&self, exec_id: &str) -> bool {
        self.map().contains_key(exec_id)
    }

    /// Runs in flight.
    pub fn len(&self) -> usize {
        self.map().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.map().remove(&self.exec_id);
    }
}

#[derive(Deserialize)]
struct SpecQuery {
    hash: Option<String>,
}

async fn spec(Query(query): Query<SpecQuery>) -> Response {
    if query.hash.as_deref().is_some_and(|h| h != bundle_sha256()) {
        return (StatusCode::NOT_FOUND, "no spec with that hash").into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::HeaderName::from_static(SPEC_HASH_HEADER),
                bundle_sha256(),
            ),
        ],
        BUNDLE,
    )
        .into_response()
}

/// The internal listener's router.
pub fn router() -> Router {
    Router::new().route("/internal/spec", get(spec))
}
