//! Protected branches: the review of code owned by non-Admin roles that can run in someone
//! else's session, its fixes and acknowledgement, and the login-role guard's exemptions.
//!
//! The serde contract: enums are externally tagged with snake_case names, request structs
//! refuse unknown fields.

use serde::{Deserialize, Serialize};

/// The first line of the review scan's SQL (the fakes' needle).
pub const REVIEW_SCAN_MARK: &str = "-- chelabase: protection review";

/// What a review item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReviewItemKind {
    Function,
    Rule,
    Trigger,
}

/// Why an item can run in someone else's session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ReviewReasonKind {
    SecurityDefiner,
    Trigger,
    Rule,
    Policy,
    Default,
    Generated,
    Check,
    Index,
    View,
    CalledBy,
    /// A range type's support function (`subtype_diff`, `canonical`).
    Type,
}

impl ReviewReasonKind {
    /// The snake_case name (as serialized).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SecurityDefiner => "security_definer",
            Self::Trigger => "trigger",
            Self::Rule => "rule",
            Self::Policy => "policy",
            Self::Default => "default",
            Self::Generated => "generated",
            Self::Check => "check",
            Self::Index => "index",
            Self::View => "view",
            Self::CalledBy => "called_by",
            Self::Type => "type",
        }
    }
}

impl ReviewItemKind {
    /// The snake_case name (as serialized).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Rule => "rule",
            Self::Trigger => "trigger",
        }
    }
}

/// One reason, with the referencing object's name (`None` for `security_definer`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReviewReason {
    pub kind: ReviewReasonKind,
    pub via: Option<String>,
}

/// One listed item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReviewItem {
    /// `<database>/<kind>/<schema>.<name>(<identity>)`.
    pub key: String,
    pub database: String,
    pub kind: ReviewItemKind,
    pub schema: String,
    pub name: String,
    /// A function's identity arguments; a trigger's or rule's relation.
    pub identity: String,
    /// The function a fix targets, `%I.%I(<identity args>)`: the function itself, or a trigger's function; None for a rule.
    pub function: Option<String>,
    pub owner: String,
    pub reasons: Vec<ReviewReason>,
    /// A function whose body can't be followed (not SQL-standard).
    pub opaque_body: bool,
    /// The md5 hex of the item's definition (`pg_get_functiondef`, `pg_get_triggerdef` or
    /// `pg_get_ruledef`; `prosrc` for an aggregate), so a body replaced in place changes the
    /// review's hash.
    #[serde(default)]
    pub digest: String,
    /// Not in the list last acknowledged (set by the orchestrator).
    #[serde(default)]
    pub new: bool,
}

/// A branch's review: the items, their hash and the hash last acknowledged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Review {
    pub items: Vec<ReviewItem>,
    pub hash: String,
    pub acknowledged_hash: Option<String>,
}

/// What a fix does to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum FixAction {
    Reassign,
    Drop,
}

/// `POST …/review/fix`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct ReviewFixRequest {
    pub key: String,
    pub action: FixAction,
    pub hash: String,
}

/// The SQL a fix ran and the review after it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ReviewFixResponse {
    pub sql: String,
    pub review: Review,
}

/// `POST …/review/acknowledge`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeReviewRequest {
    pub hash: String,
}

/// A branch's guard exemptions, by role name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct GuardExemptions {
    pub exemptions: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_serde() {
        for kind in [
            ReviewReasonKind::SecurityDefiner,
            ReviewReasonKind::Trigger,
            ReviewReasonKind::Rule,
            ReviewReasonKind::Policy,
            ReviewReasonKind::Default,
            ReviewReasonKind::Generated,
            ReviewReasonKind::Check,
            ReviewReasonKind::Index,
            ReviewReasonKind::View,
            ReviewReasonKind::CalledBy,
            ReviewReasonKind::Type,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        for kind in [
            ReviewItemKind::Function,
            ReviewItemKind::Rule,
            ReviewItemKind::Trigger,
        ] {
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
    }

    #[test]
    fn requests_refuse_unknown_fields() {
        assert!(serde_json::from_str::<AcknowledgeReviewRequest>(r#"{"hash":"h","x":1}"#).is_err());
        assert!(
            serde_json::from_str::<ReviewFixRequest>(
                r#"{"key":"k","action":"drop","hash":"h","x":1}"#
            )
            .is_err()
        );
        let fix: ReviewFixRequest =
            serde_json::from_str(r#"{"key":"k","action":"reassign","hash":"h"}"#).unwrap();
        assert_eq!(fix.action, FixAction::Reassign);
    }
}
