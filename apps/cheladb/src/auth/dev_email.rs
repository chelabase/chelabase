//! Dev-only switch to skip email verification for self-signups.
//!
//! Zitadel 2.71 has no setting for this: its login flow asks for a code
//! whenever the new user's email isn't verified. A "Pre Creation" Action
//! (Actions v1) can mark it verified instead. Never enable this outside local
//! development: anyone could register with an email they don't own.

use serde_json::{json, Value};

pub const ENV_VAR: &str = "CHELADB_DEV_SKIP_EMAIL_VERIFICATION";
/// Zitadel requires the action name to equal the script's function name.
pub const ACTION_NAME: &str = "cheladbDevSkipEmailVerification";
/// `Action.Flow.Type.InternalAuthentication`
pub const INTERNAL_AUTH_FLOW: &str = "3";
/// `Action.TriggerType.PreCreation`
pub const PRE_CREATION_TRIGGER: &str = "2";

pub fn skip_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("true" | "1" | "yes")
    )
}

const SCRIPT: &str = "\
function cheladbDevSkipEmailVerification(ctx, api) {
  // Local dev only (CHELADB_DEV_SKIP_EMAIL_VERIFICATION): treat the email of
  // a self-registered user as verified, so no code is needed.
  api.setEmailVerified(true);
}
";

pub fn action_payload() -> Value {
    json!({
        "name": ACTION_NAME,
        "script": SCRIPT,
        "timeout": "10s",
        "allowedToFail": false
    })
}

/// The action named `name`, as `(id, is_active)`.
pub fn find_action(search: &Value, name: &str) -> Option<(String, bool)> {
    let action = search
        .get("result")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name))?;
    let id = action.get("id")?.as_str()?.to_string();
    let active = action.get("state").and_then(Value::as_str) == Some("ACTION_STATE_ACTIVE");
    Some((id, active))
}

/// Action ids to set on `trigger` of `flow` (a `GET /management/v1/flows/{type}`
/// response): whatever is already there, plus `action_id`.
pub fn trigger_actions_with(flow: &Value, trigger: &str, action_id: &str) -> Vec<String> {
    let mut ids: Vec<String> = flow
        .pointer("/flow/triggerActions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.pointer("/triggerType/id").and_then(Value::as_str) == Some(trigger))
        .flat_map(|t| t.get("actions").and_then(Value::as_array).cloned().unwrap_or_default())
        .filter_map(|a| a.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if !ids.iter().any(|id| id == action_id) {
        ids.push(action_id.to_string());
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_is_off_unless_explicitly_enabled() {
        for on in ["true", "TRUE", "1", "yes", " true "] {
            assert!(skip_enabled(Some(on)), "{on:?} should enable");
        }
        for off in ["false", "0", "", "no", "maybe"] {
            assert!(!skip_enabled(Some(off)), "{off:?} should not enable");
        }
        assert!(!skip_enabled(None));
    }

    #[test]
    fn action_marks_the_email_verified_and_is_named_after_its_function() {
        let p = action_payload();
        assert_eq!(p["name"], ACTION_NAME);
        let script = p["script"].as_str().unwrap();
        assert!(script.contains(&format!("function {ACTION_NAME}(ctx, api)")), "{script}");
        assert!(script.contains("api.setEmailVerified(true)"), "{script}");
        assert_eq!(p["allowedToFail"], false);
    }

    #[test]
    fn find_action_reports_id_and_state() {
        let search = json!({ "result": [
            { "id": "a1", "name": "other", "state": "ACTION_STATE_ACTIVE" },
            { "id": "a2", "name": ACTION_NAME, "state": "ACTION_STATE_INACTIVE" }
        ]});
        assert_eq!(find_action(&search, ACTION_NAME), Some(("a2".into(), false)));
        assert_eq!(find_action(&json!({}), ACTION_NAME), None);
    }

    #[test]
    fn trigger_keeps_existing_actions_and_adds_ours_once() {
        let flow = json!({ "flow": { "triggerActions": [
            { "triggerType": { "id": "1" }, "actions": [ { "id": "post-auth" } ] },
            { "triggerType": { "id": "2" }, "actions": [ { "id": "theirs" }, { "id": "ours" } ] }
        ]}});
        assert_eq!(trigger_actions_with(&flow, "2", "ours"), vec!["theirs", "ours"]);
        assert_eq!(trigger_actions_with(&flow, "2", "new"), vec!["theirs", "ours", "new"]);
        assert_eq!(trigger_actions_with(&json!({ "flow": {} }), "2", "ours"), vec!["ours"]);
    }
}
