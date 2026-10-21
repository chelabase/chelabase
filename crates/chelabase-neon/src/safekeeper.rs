//! Client for a safekeeper's HTTP API: the commit LSN of a timeline.
//!
//! The safekeepers' `commit_lsn` is the highest LSN a quorum has durably accepted, so a
//! branch at it is safe even when the pageserver hasn't ingested that far yet. Plain HTTP,
//! like [`crate::storcon::StorconClient`], with a NeonJWT bearer (the safekeepers check it).

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
        Self {
            http: crate::storcon::http_client_with(REQUEST_TIMEOUT, None),
            base: base_url,
        }
    }

    /// Sends `token` (a NeonJWT with scope `safekeeperdata`) as the bearer on every request:
    /// the safekeepers check NeonJWT on their HTTP API.
    pub fn with_token(mut self, token: String) -> Self {
        self.http = crate::storcon::http_client_with(REQUEST_TIMEOUT, Some(&token));
        self
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
