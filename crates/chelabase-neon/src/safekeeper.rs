//! Client for a safekeeper's HTTP API: the commit LSN of a timeline.
//!
//! The safekeepers' `commit_lsn` is the highest LSN a quorum has durably accepted, so a
//! branch at it is safe even when the pageserver hasn't ingested that far yet. Plain HTTP,
//! like [`crate::storcon::StorconClient`] (the Compose safekeeper runs without auth).

use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde::Deserialize;

use crate::id::NeonId;
use crate::lsn::Lsn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// No answer, a timeout, a non-404 error status, or an answer we couldn't parse.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct SafekeeperError(#[from] anyhow::Error);

impl From<reqwest::Error> for SafekeeperError {
    fn from(e: reqwest::Error) -> Self {
        Self(e.into())
    }
}

#[derive(Clone)]
pub struct SafekeeperClient {
    http: reqwest::Client,
    base: Url,
}

impl SafekeeperClient {
    pub fn new(base_url: Url) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client without TLS");
        Self {
            http,
            base: base_url,
        }
    }

    /// `GET /v1/tenant/{tenant}/timeline/{timeline}`: the timeline's `commit_lsn`; `None`
    /// when this safekeeper doesn't know the timeline (404).
    pub async fn commit_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, SafekeeperError> {
        #[derive(Deserialize)]
        struct Status {
            commit_lsn: Lsn,
        }
        let url = format!(
            "{}/v1/tenant/{tenant}/timeline/{timeline}",
            self.base.as_str().trim_end_matches('/')
        );
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("safekeeper returned {status}: {body}").into());
        }
        let parsed: Status = resp.json().await?;
        Ok(Some(parsed.commit_lsn))
    }
}
