//! C2 RED tests (fork round 4): the worker's storage-token refresh, the capability read before
//! each spec push, the realtime settings and the generalised certificate sweep. One file per
//! area; the shared helpers are here.
//!
//! Names these tests pin (the coder implements them; see the C2 report):
//! - `crate::ports::SpecPusher::{report, push_storage_token}`;
//! - `FakePusher::{set_capability, fail_report, hang_report, hang_storage_token,
//!   fail_storage_token_for, storage_tokens}` and the journal entries `pusher.report <ep>` and
//!   `pusher.storage_token <ep>`;
//! - `crate::token_refresh::{RefreshPass, storage_token_refresh_loop}` and
//!   `Worker::{refresh_storage_tokens_once, storage_token_failure_count}`;
//! - `Worker` fields `realtime_logins`, `storage_token_refresh_every`, `storage_token_failures`;
//! - `CertSweep::realtime_tls_dir`;
//! - `WorkerConfig::{storage_token_refresh_every, realtime_login_cidrs, realtime_tls_dir}` and
//!   `WorkerConfig::realtime_logins()`.

mod config;
mod spec_push;
mod sweep;
mod token_refresh;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chelabase_metadata::DatabaseStatus;

use crate::fakes::FakeWorker;

/// The claims of a JWT (the signature isn't checked here).
pub(crate) fn claims(token: &str) -> serde_json::Value {
    let payload = token.split('.').nth(1).expect("a JWT has three parts");
    let bytes = URL_SAFE_NO_PAD.decode(payload).expect("base64url payload");
    serde_json::from_slice(&bytes).expect("JSON claims")
}

/// A ready database named `name` (provisioned through the fakes): its main endpoint's id.
pub(crate) async fn ready_named(w: &FakeWorker, name: &str) -> String {
    let id = w.metadata.insert_database(name, DatabaseStatus::Pending);
    w.handle_provision(id, name).await.unwrap();
    assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
    w.metadata
        .main(id)
        .endpoint_id
        .expect("a ready branch has an endpoint")
}

/// The tenant id the endpoint's row records.
pub(crate) fn tenant_of(w: &FakeWorker, endpoint: &str) -> String {
    w.metadata.state().endpoints[endpoint]
        .endpoint
        .tenant_id
        .clone()
}

/// The `storage_auth_token` of the last spec the fake compute took.
pub(crate) fn last_pushed_token(w: &FakeWorker) -> String {
    let pushed = w.pusher.pushed.lock().unwrap();
    pushed.last().expect("a spec was pushed").1["spec"]["storage_auth_token"]
        .as_str()
        .expect("the spec carries a storage token")
        .to_string()
}
