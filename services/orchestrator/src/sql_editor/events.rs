//! The run's stream events (NDJSON) and the response byte budget.

use serde::Serialize;

/// One line of a run's NDJSON stream.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum RunEvent {
    Notice {
        severity: String,
        code: String,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        position: Option<u32>,
    },
    Columns {
        statement: usize,
        columns: Vec<ColumnInfo>,
    },
    Rows {
        statement: usize,
        rows: Vec<Vec<Option<String>>>,
    },
    Complete {
        statement: usize,
        tag: String,
        rows: u64,
        truncated: bool,
    },
    Error {
        statement: usize,
        code: String,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        position: Option<u32>,
    },
    Heartbeat,
    Done {
        outcome: Outcome,
        duration_ms: u64,
    },
}

/// A result column: its name and its type's name (`""` when unknown).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Error,
    Cancelled,
    Timeout,
    TruncatedResponse,
}

impl RunEvent {
    /// The event as one NDJSON line (JSON plus `\n`).
    pub fn to_line(&self) -> bytes::Bytes {
        let mut line = serde_json::to_vec(self).expect("run events always serialize");
        line.push(b'\n');
        line.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(event: RunEvent) -> String {
        String::from_utf8(event.to_line().to_vec()).unwrap()
    }

    #[test]
    fn events_serialize_to_the_spec_shapes() {
        assert_eq!(
            line(RunEvent::Notice {
                severity: "NOTICE".into(),
                code: "00000".into(),
                message: "hi".into(),
                detail: None,
                hint: None,
                position: None,
            }),
            "{\"t\":\"notice\",\"severity\":\"NOTICE\",\"code\":\"00000\",\"message\":\"hi\",\"detail\":null,\"hint\":null,\"position\":null}\n"
        );
        assert_eq!(
            line(RunEvent::Columns {
                statement: 0,
                columns: vec![ColumnInfo {
                    name: "id".into(),
                    type_name: "int8".into()
                }],
            }),
            "{\"t\":\"columns\",\"statement\":0,\"columns\":[{\"name\":\"id\",\"type\":\"int8\"}]}\n"
        );
        assert_eq!(
            line(RunEvent::Rows {
                statement: 0,
                rows: vec![vec![Some("1".into()), Some("a".into()), None]],
            }),
            "{\"t\":\"rows\",\"statement\":0,\"rows\":[[\"1\",\"a\",null]]}\n"
        );
        assert_eq!(
            line(RunEvent::Complete {
                statement: 0,
                tag: "SELECT 3".into(),
                rows: 3,
                truncated: false
            }),
            "{\"t\":\"complete\",\"statement\":0,\"tag\":\"SELECT 3\",\"rows\":3,\"truncated\":false}\n"
        );
        assert_eq!(
            line(RunEvent::Error {
                statement: 2,
                code: "42P01".into(),
                message: "nope".into(),
                detail: None,
                hint: None,
                position: Some(17),
            }),
            "{\"t\":\"error\",\"statement\":2,\"code\":\"42P01\",\"message\":\"nope\",\"detail\":null,\"hint\":null,\"position\":17}\n"
        );
        assert_eq!(line(RunEvent::Heartbeat), "{\"t\":\"heartbeat\"}\n");
        assert_eq!(
            line(RunEvent::Done {
                outcome: Outcome::Ok,
                duration_ms: 12
            }),
            "{\"t\":\"done\",\"outcome\":\"ok\",\"duration_ms\":12}\n"
        );
        for (outcome, name) in [
            (Outcome::Error, "error"),
            (Outcome::Cancelled, "cancelled"),
            (Outcome::Timeout, "timeout"),
            (Outcome::TruncatedResponse, "truncated_response"),
        ] {
            assert_eq!(
                line(RunEvent::Done {
                    outcome,
                    duration_ms: 0
                }),
                format!("{{\"t\":\"done\",\"outcome\":\"{name}\",\"duration_ms\":0}}\n")
            );
        }
    }
}
