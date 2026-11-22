//! Where a tool's code runs: [`crate::runtime::Runtime`] over HTTP to `mcp-runtime`, or
//! [`Unavailable`] when no runtime is configured.

use async_trait::async_trait;
use serde_json::Value;

use crate::auth::Caller;

/// Which tool the code was sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Search,
    Execute,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Search => "search",
            Kind::Execute => "execute",
        }
    }
}

/// One run: the code, the tool it came to, and the verified caller it acts for.
#[derive(Debug, Clone)]
pub struct Run {
    pub kind: Kind,
    pub code: String,
    /// `execute`'s `destructive` argument (always false for `search`).
    pub destructive: bool,
    pub caller: Caller,
    /// The run's random id (256 bits, hex): it names the run in the runtime's callbacks and
    /// in the audit line.
    pub exec_id: String,
}

/// Why a run gave no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidCode,
    Threw,
    Timeout,
    NotSerializable,
    RuntimeUnavailable,
    /// Refused by a limit before it ran (with `retry_after_ms` in the tool error).
    RateLimited,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidCode => "invalid_code",
            ErrorCode::Threw => "threw",
            ErrorCode::Timeout => "timeout",
            ErrorCode::NotSerializable => "not_serializable",
            ErrorCode::RuntimeUnavailable => "runtime_unavailable",
            ErrorCode::RateLimited => "rate_limited",
        }
    }
}

/// What a run gave back.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Value(Value),
    Error { code: ErrorCode, message: String },
}

/// A finished run: its outcome and the code's `console` lines.
#[derive(Debug, Clone, PartialEq)]
pub struct Ran {
    pub outcome: Outcome,
    pub logs: Vec<String>,
}

impl From<Outcome> for Ran {
    fn from(outcome: Outcome) -> Self {
        Ran {
            outcome,
            logs: vec![],
        }
    }
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn run(&self, run: Run) -> Ran;
}

/// The sandbox when `CHELABASE_MCP_RUNTIME_URL` is unset: every run is `runtime_unavailable`.
pub struct Unavailable;

#[async_trait]
impl Sandbox for Unavailable {
    async fn run(&self, _run: Run) -> Ran {
        Outcome::Error {
            code: ErrorCode::RuntimeUnavailable,
            message: "the code runtime is not configured".into(),
        }
        .into()
    }
}
