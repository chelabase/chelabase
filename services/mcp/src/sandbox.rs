//! Where a tool's code runs. The real runtime arrives in m7 (#351); until then the binary uses
//! [`Unavailable`] and the tests a double.

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
}

/// Why a run gave no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidCode,
    Threw,
    Timeout,
    NotSerializable,
    RuntimeUnavailable,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidCode => "invalid_code",
            ErrorCode::Threw => "threw",
            ErrorCode::Timeout => "timeout",
            ErrorCode::NotSerializable => "not_serializable",
            ErrorCode::RuntimeUnavailable => "runtime_unavailable",
        }
    }
}

/// What a run gave back.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Value(Value),
    Error { code: ErrorCode, message: String },
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn run(&self, run: Run) -> Outcome;
}

/// The sandbox until m7 wires the runtime: every run is `runtime_unavailable`.
pub struct Unavailable;

#[async_trait]
impl Sandbox for Unavailable {
    async fn run(&self, _run: Run) -> Outcome {
        Outcome::Error {
            code: ErrorCode::RuntimeUnavailable,
            message: "the code runtime is not configured".into(),
        }
    }
}
