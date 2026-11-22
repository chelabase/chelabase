//! Operation classes (filled in by the policy milestone).

/// How risky an operation is; decides whether the MCP server runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Read,
    Write,
    Destructive,
    Excluded,
}
