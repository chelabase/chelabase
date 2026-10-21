//! Owner viewers' checks (spec §4, R10, R19, S12, S14, T6's contract): realtime never holds
//! owner credentials, so an owner group's probe and check go to the orchestrator's
//! `POST /internal/realtime/visible` at `CHELABASE_ORCHESTRATOR_URL` with realtime's bearer,
//! timeout [`crate::orchestrator::VISIBLE_TIMEOUT`]:
//!
//! - request `{token, database_id, branch_id, pg_database, rel_oid, xid: "<xid8 decimal>" | null,
//!   keys: [[text, …], …] | null}` (`keys: null` = probe only); one attempt per call (realtime
//!   polls, P7);
//! - answer 200 `{probe: {ok, bypass}, visible, keys: [key string]}`;
//! - with `convert_keys: [[text, …], …]` beside a check (a window's every key), the answer adds
//!   `converted: [key string]`, printed in the same owner session and transaction;
//! - convert mode (owner deletes): `convert: true` with `keys` (required; `xid` ignored) →
//!   200 `{keys: [key string]}`, each key's texts cast through its column's type and printed in
//!   the owner's console session, in the request's order, rows needn't exist, no probe;
//! - keys are filtered with `keys_supported`/`key_text_allowed` **before** the call; a 400 (and a
//!   401, a 5xx, a body that isn't the route's, a timeout) is `unavailable`.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::check::{CheckAnswer, CheckError, CheckTarget};
use crate::orchestrator::VISIBLE_TIMEOUT;

pub const VISIBLE_PATH: &str = "/internal/realtime/visible";

/// The request body.
pub fn visible_body(
    token: &str,
    target: &CheckTarget,
    rel_oid: u32,
    xid: Option<u64>,
    keys: Option<&[Vec<String>]>,
) -> Value {
    json!({
        "token": token,
        "database_id": target.database_id,
        "branch_id": target.branch_id,
        "pg_database": target.pg_database,
        "rel_oid": rel_oid,
        "xid": xid.map(|x| x.to_string()),
        "keys": keys,
    })
}

#[derive(Deserialize)]
struct ProbeBody {
    ok: bool,
    bypass: bool,
}

#[derive(Deserialize)]
struct VisibleAnswer {
    probe: ProbeBody,
    visible: bool,
    keys: Vec<String>,
    /// Only when the request had `convert_keys` (and the conversion worked).
    #[serde(default)]
    converted: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct ConvertAnswer {
    keys: Vec<String>,
}

fn unavailable(status: u16) -> CheckError {
    if status == 400 || status == 401 {
        tracing::error!(
            status,
            "the orchestrator refused realtime's /visible request: check CHELABASE_REALTIME_INTERNAL_TOKEN and the P6 filter"
        );
    }
    CheckError::Unavailable(format!("/visible answered {status}"))
}

/// The HTTP answer as a check answer.
pub fn parse_visible(status: u16, body: &[u8]) -> Result<CheckAnswer, CheckError> {
    if status != 200 {
        return Err(unavailable(status));
    }
    let a: VisibleAnswer = serde_json::from_slice(body)
        .map_err(|_| CheckError::Unavailable("/visible: not the route's answer".into()))?;
    let shown = a.probe.ok && a.visible;
    Ok(CheckAnswer {
        probe_ok: a.probe.ok,
        bypass: a.probe.bypass,
        visible: a.visible,
        keys: if shown { a.keys } else { Vec::new() },
        converted: match a.converted {
            Some(converted) if shown => converted.into_iter().map(Some).collect(),
            _ => Vec::new(),
        },
    })
}

/// A check's request body with the keys to convert in the same owner session
/// (`convert_keys`, left out when there are none).
pub fn check_body(
    token: &str,
    target: &CheckTarget,
    rel_oid: u32,
    xid: Option<u64>,
    keys: Option<&[Vec<String>]>,
    convert: &[Vec<String>],
) -> Value {
    let mut body = visible_body(token, target, rel_oid, xid, keys);
    if !convert.is_empty() {
        body["convert_keys"] = json!(convert);
    }
    body
}

/// The convert mode's request body (owner deletes, coordinator ruling): `{token, database_id,
/// branch_id, pg_database, rel_oid, xid: null, keys, convert: true}`.
pub fn convert_body(
    token: &str,
    target: &CheckTarget,
    rel_oid: u32,
    keys: &[Vec<String>],
) -> Value {
    let mut body = visible_body(token, target, rel_oid, None, Some(keys));
    body["convert"] = json!(true);
    body
}

/// The convert mode's answer: 200 `{keys: [key string]}`, one per requested key in order (a
/// count that differs is `unavailable`); anything else as [`parse_visible`].
pub fn parse_convert(
    status: u16,
    body: &[u8],
    requested: usize,
) -> Result<Vec<String>, CheckError> {
    if status != 200 {
        return Err(unavailable(status));
    }
    let a: ConvertAnswer = serde_json::from_slice(body)
        .map_err(|_| CheckError::Unavailable("/visible convert: not the route's answer".into()))?;
    if a.keys.len() != requested {
        return Err(CheckError::Unavailable(format!(
            "/visible convert: {} keys for {requested}",
            a.keys.len()
        )));
    }
    Ok(a.keys)
}

/// The orchestrator's `/internal/realtime/visible`.
#[derive(Clone)]
pub struct OwnerClient {
    http: reqwest::Client,
    url: String,
    bearer: String,
    timeout: Duration,
}

impl std::fmt::Debug for OwnerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerClient")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl OwnerClient {
    /// `base_url` = `CHELABASE_ORCHESTRATOR_URL`, `bearer` = realtime's internal token.
    pub fn new(base_url: &str, bearer: &str) -> Self {
        Self {
            // Never follow a redirect: the bearer goes to the configured host only.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            url: format!("{}{VISIBLE_PATH}", base_url.trim_end_matches('/')),
            bearer: bearer.to_string(),
            timeout: VISIBLE_TIMEOUT,
        }
    }

    /// One POST, within the timeout: (status, body).
    async fn post(&self, body: Value) -> Result<(u16, Vec<u8>), CheckError> {
        let call = async {
            let resp = self
                .http
                .post(&self.url)
                .bearer_auth(&self.bearer)
                .json(&body)
                .send()
                .await
                .map_err(|e| CheckError::Unavailable(format!("/visible: {e}")))?;
            let status = resp.status().as_u16();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| CheckError::Unavailable(format!("/visible: {e}")))?;
            Ok((status, bytes.to_vec()))
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| CheckError::Timeout)?
    }

    pub async fn visible(
        &self,
        token: &str,
        target: &CheckTarget,
        rel_oid: u32,
        xid: Option<u64>,
        keys: Option<&[Vec<String>]>,
    ) -> Result<CheckAnswer, CheckError> {
        let (status, body) = self
            .post(visible_body(token, target, rel_oid, xid, keys))
            .await?;
        parse_visible(status, &body)
    }

    /// [`Self::visible`] with `convert` printed in the same owner session and transaction
    /// (`converted` in the answer, one per key in order; empty when it didn't work).
    pub async fn check(
        &self,
        token: &str,
        target: &CheckTarget,
        rel_oid: u32,
        xid: Option<u64>,
        keys: Option<&[Vec<String>]>,
        convert: &[Vec<String>],
    ) -> Result<CheckAnswer, CheckError> {
        let (status, body) = self
            .post(check_body(token, target, rel_oid, xid, keys, convert))
            .await?;
        parse_visible(status, &body)
    }
    /// The convert mode: `keys` in the owner session's key form, in order (rows need not exist).
    pub async fn convert(
        &self,
        token: &str,
        target: &CheckTarget,
        rel_oid: u32,
        keys: &[Vec<String>],
    ) -> Result<Vec<String>, CheckError> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let (status, body) = self
            .post(convert_body(token, target, rel_oid, keys))
            .await?;
        parse_convert(status, &body, keys.len())
    }
}
