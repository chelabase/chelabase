//! Edge functions m3: `chelabase.fn.changed.<branch_id>`, published (core NATS, fire and forget)
//! after every functions write commits, so the functions service (m4) drops its cached
//! resolutions for the branch, and the CLI (m5) and Studio (m6) refresh.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::NatsError;

/// `.<branch_id>`: a function's state on the branch changed.
pub const CHANGED: &str = "chelabase.fn.changed";

/// The payload: the branch and the function's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnChanged {
    pub branch_id: Uuid,
    pub function: String,
}

/// `chelabase.fn.changed.<branch_id>`, the id in its canonical hyphenated lowercase form.
pub fn changed_subject(branch_id: Uuid) -> String {
    format!("{CHANGED}.{}", branch_id.hyphenated())
}

/// Publishes `{branch_id, function}` on [`changed_subject`]. Callers publish only after the
/// write committed, and log a failure and carry on: an event never fails the write.
pub async fn publish_changed(
    client: &async_nats::Client,
    branch_id: Uuid,
    function: &str,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(&FnChanged {
        branch_id,
        function: function.to_owned(),
    })?;
    client
        .publish(changed_subject(branch_id), payload.into())
        .await
        .map_err(|e| NatsError::Client(Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_and_payload_are_the_contract() {
        let branch = Uuid::parse_str("6F1C0B6E-9D2A-4A57-8A77-1F3C2A9B4E10").unwrap();
        assert_eq!(
            changed_subject(branch),
            "chelabase.fn.changed.6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10"
        );
        let body = serde_json::to_value(FnChanged {
            branch_id: branch,
            function: "hello".into(),
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({"branch_id": "6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10", "function": "hello"})
        );
    }
}
