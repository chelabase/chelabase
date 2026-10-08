//! One JSON error shape with stable codes for the management, login and gateway layers.
//!
//! Wire body: `{"error": {"code": "<snake_case>", "message": "<text>", ...extras}}`.

use serde::{Deserialize, Serialize};

/// Stable, machine-readable error code. Serialises as snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    Forbidden,
    TenantNotBound,
    NotFound,
    MethodNotAllowed,
    InvalidRequest,
    TooLarge,
    RateLimited,
    Unavailable,
    Timeout,
    Internal,
    DatabaseNotReady,
    BranchNotReady,
    CredentialsUnavailable,
    BackupBranch,
    HasChildren,
    BranchCapReached,
    MainBranch,
    NameTaken,
    AlreadyDeleting,
    PublicKeyExists,
    PublicKeyNotRevocable,
    RoleRefused,
    NotApplied,
    WorkerUnavailable,
    InvalidCredentials,
    EmailTaken,
    InvalidCode,
    WeakPassword,
    SignInExpired,
    InvalidRoleName,
    ReservedRoleName,
    RoleExists,
    RoleCapReached,
    RoleInUse,
    RolePrivilegeMismatch,
    BranchProtected,
    Conflict,
    DataApiDisabled,
    OwnerPasswordUnknown,
    ProjectDeleting,
    AppUsersDisabled,
    AppUsersLocationFixed,
    AppUsersHasUsers,
    AppUsersTableExists,
    DataApiNotEnabled,
    UnknownColumn,
    ConstraintViolation,
    EmailNotVerified,
    UserInactive,
    RefreshTokenInvalid,
    RefreshTokenReused,
    UserTokenInvalid,
    UserTokenExpired,
    UserSessionRevoked,
    UserTokenWithSecretKey,
    RunIdInUse,
    SqlConfirmRequired,
    SqlRunLimit,
    SchemaTooLarge,
    SavedQueryNameTaken,
    SavedQueryConflict,
    SavedQueryCap,
    MergeConflictsOpen,
    MergeDropsUnacknowledged,
    MergeNotRehearsed,
    MergeStale,
    MergeBusy,
    MergeOpenExists,
    MergeBaseUnavailable,
    MergeVersionConflict,
    MergeConfirmRequired,
    BranchMergeBusy,
    /// A role attribute outside the allowed set (U2).
    RoleAttributeNotAllowed,
    /// A membership in a role the API may not grant: Chelabase's own roles, the owner, `pg_…`,
    /// a role that doesn't exist or one the owner can't administer.
    RoleMembershipNotAllowed,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::TenantNotBound => "tenant_not_bound",
            Self::NotFound => "not_found",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::InvalidRequest => "invalid_request",
            Self::TooLarge => "too_large",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
            Self::DatabaseNotReady => "database_not_ready",
            Self::BranchNotReady => "branch_not_ready",
            Self::CredentialsUnavailable => "credentials_unavailable",
            Self::BackupBranch => "backup_branch",
            Self::HasChildren => "has_children",
            Self::BranchCapReached => "branch_cap_reached",
            Self::MainBranch => "main_branch",
            Self::NameTaken => "name_taken",
            Self::AlreadyDeleting => "already_deleting",
            Self::PublicKeyExists => "public_key_exists",
            Self::PublicKeyNotRevocable => "public_key_not_revocable",
            Self::RoleRefused => "role_refused",
            Self::NotApplied => "not_applied",
            Self::WorkerUnavailable => "worker_unavailable",
            Self::InvalidCredentials => "invalid_credentials",
            Self::EmailTaken => "email_taken",
            Self::InvalidCode => "invalid_code",
            Self::WeakPassword => "weak_password",
            Self::SignInExpired => "sign_in_expired",
            Self::InvalidRoleName => "invalid_role_name",
            Self::ReservedRoleName => "reserved_role_name",
            Self::RoleExists => "role_exists",
            Self::RoleCapReached => "role_cap_reached",
            Self::RoleInUse => "role_in_use",
            Self::RolePrivilegeMismatch => "role_privilege_mismatch",
            Self::BranchProtected => "branch_protected",
            Self::Conflict => "conflict",
            Self::DataApiDisabled => "data_api_disabled",
            Self::OwnerPasswordUnknown => "owner_password_unknown",
            Self::ProjectDeleting => "project_deleting",
            Self::AppUsersDisabled => "app_users_disabled",
            Self::AppUsersLocationFixed => "app_users_location_fixed",
            Self::AppUsersHasUsers => "app_users_has_users",
            Self::AppUsersTableExists => "app_users_table_exists",
            Self::DataApiNotEnabled => "data_api_not_enabled",
            Self::UnknownColumn => "unknown_column",
            Self::ConstraintViolation => "constraint_violation",
            Self::EmailNotVerified => "email_not_verified",
            Self::UserInactive => "user_inactive",
            Self::RefreshTokenInvalid => "refresh_token_invalid",
            Self::RefreshTokenReused => "refresh_token_reused",
            Self::UserTokenInvalid => "user_token_invalid",
            Self::UserTokenExpired => "user_token_expired",
            Self::UserSessionRevoked => "user_session_revoked",
            Self::UserTokenWithSecretKey => "user_token_with_secret_key",
            Self::RunIdInUse => "run_id_in_use",
            Self::SqlConfirmRequired => "sql_confirm_required",
            Self::SqlRunLimit => "sql_run_limit",
            Self::SchemaTooLarge => "schema_too_large",
            Self::SavedQueryNameTaken => "saved_query_name_taken",
            Self::SavedQueryConflict => "saved_query_conflict",
            Self::SavedQueryCap => "saved_query_cap",
            Self::MergeConflictsOpen => "merge_conflicts_open",
            Self::MergeDropsUnacknowledged => "merge_drops_unacknowledged",
            Self::MergeNotRehearsed => "merge_not_rehearsed",
            Self::MergeStale => "merge_stale",
            Self::MergeBusy => "merge_busy",
            Self::MergeOpenExists => "merge_open_exists",
            Self::MergeBaseUnavailable => "merge_base_unavailable",
            Self::MergeVersionConflict => "merge_version_conflict",
            Self::MergeConfirmRequired => "merge_confirm_required",
            Self::BranchMergeBusy => "branch_merge_busy",
            Self::RoleAttributeNotAllowed => "role_attribute_not_allowed",
            Self::RoleMembershipNotAllowed => "role_membership_not_allowed",
        }
    }
}

/// The documented shape of the `error` object. Some codes add extra keys (for example
/// `keys`), which are documented per code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

/// The schema error responses reference: `{"error": ErrorBody}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// An HTTP error: status, stable code, message, optional extra body keys and `Retry-After`.
#[cfg(feature = "axum")]
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub code: ErrorCode,
    pub message: String,
    pub extra: Vec<(String, serde_json::Value)>,
    pub retry_after: Option<u32>,
}

#[cfg(feature = "axum")]
impl ApiError {
    pub fn new(status: u16, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            extra: Vec::new(),
            retry_after: None,
        }
    }

    /// Adds a key inside the `error` object (after `code` and `message`).
    pub fn with_extra(mut self, key: &str, value: serde_json::Value) -> Self {
        self.extra.push((key.to_owned(), value));
        self
    }

    /// Sets the `Retry-After` header (seconds); never part of the body.
    pub fn retry_after(mut self, secs: u32) -> Self {
        self.retry_after = Some(secs);
        self
    }

    /// `{"error": {"code", "message", ...extra}}`.
    pub fn body(&self) -> serde_json::Value {
        let mut error = serde_json::Map::new();
        error.insert("code".into(), self.code.as_str().into());
        error.insert("message".into(), self.message.clone().into());
        for (k, v) in &self.extra {
            error.insert(k.clone(), v.clone());
        }
        serde_json::json!({ "error": error })
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(404, ErrorCode::NotFound, msg)
    }

    pub fn internal() -> Self {
        Self::new(500, ErrorCode::Internal, "internal error")
    }

    pub fn invalid_request(status: u16, msg: impl Into<String>) -> Self {
        Self::new(status, ErrorCode::InvalidRequest, msg)
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(401, ErrorCode::Unauthorized, msg)
    }
}

#[cfg(feature = "axum")]
impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::{HeaderValue, StatusCode, header};
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = (status, axum::Json(self.body())).into_response();
        if let Some(secs) = self.retry_after {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        resp
    }
}

#[cfg(all(test, feature = "axum"))]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    const ALL: [(ErrorCode, &str); 75] = [
        (ErrorCode::Unauthorized, "unauthorized"),
        (ErrorCode::Forbidden, "forbidden"),
        (ErrorCode::TenantNotBound, "tenant_not_bound"),
        (ErrorCode::NotFound, "not_found"),
        (ErrorCode::MethodNotAllowed, "method_not_allowed"),
        (ErrorCode::InvalidRequest, "invalid_request"),
        (ErrorCode::TooLarge, "too_large"),
        (ErrorCode::RateLimited, "rate_limited"),
        (ErrorCode::Unavailable, "unavailable"),
        (ErrorCode::Timeout, "timeout"),
        (ErrorCode::Internal, "internal"),
        (ErrorCode::DatabaseNotReady, "database_not_ready"),
        (ErrorCode::BranchNotReady, "branch_not_ready"),
        (ErrorCode::CredentialsUnavailable, "credentials_unavailable"),
        (ErrorCode::BackupBranch, "backup_branch"),
        (ErrorCode::HasChildren, "has_children"),
        (ErrorCode::BranchCapReached, "branch_cap_reached"),
        (ErrorCode::MainBranch, "main_branch"),
        (ErrorCode::NameTaken, "name_taken"),
        (ErrorCode::AlreadyDeleting, "already_deleting"),
        (ErrorCode::PublicKeyExists, "public_key_exists"),
        (ErrorCode::PublicKeyNotRevocable, "public_key_not_revocable"),
        (ErrorCode::RoleRefused, "role_refused"),
        (ErrorCode::NotApplied, "not_applied"),
        (ErrorCode::WorkerUnavailable, "worker_unavailable"),
        (ErrorCode::InvalidCredentials, "invalid_credentials"),
        (ErrorCode::EmailTaken, "email_taken"),
        (ErrorCode::InvalidCode, "invalid_code"),
        (ErrorCode::WeakPassword, "weak_password"),
        (ErrorCode::SignInExpired, "sign_in_expired"),
        (ErrorCode::InvalidRoleName, "invalid_role_name"),
        (ErrorCode::ReservedRoleName, "reserved_role_name"),
        (ErrorCode::RoleExists, "role_exists"),
        (ErrorCode::RoleCapReached, "role_cap_reached"),
        (ErrorCode::RoleInUse, "role_in_use"),
        (ErrorCode::RolePrivilegeMismatch, "role_privilege_mismatch"),
        (ErrorCode::BranchProtected, "branch_protected"),
        (ErrorCode::Conflict, "conflict"),
        (ErrorCode::DataApiDisabled, "data_api_disabled"),
        (ErrorCode::OwnerPasswordUnknown, "owner_password_unknown"),
        (ErrorCode::ProjectDeleting, "project_deleting"),
        (ErrorCode::AppUsersDisabled, "app_users_disabled"),
        (ErrorCode::AppUsersLocationFixed, "app_users_location_fixed"),
        (ErrorCode::AppUsersHasUsers, "app_users_has_users"),
        (ErrorCode::AppUsersTableExists, "app_users_table_exists"),
        (ErrorCode::DataApiNotEnabled, "data_api_not_enabled"),
        (ErrorCode::UnknownColumn, "unknown_column"),
        (ErrorCode::ConstraintViolation, "constraint_violation"),
        (ErrorCode::EmailNotVerified, "email_not_verified"),
        (ErrorCode::UserInactive, "user_inactive"),
        (ErrorCode::RefreshTokenInvalid, "refresh_token_invalid"),
        (ErrorCode::RefreshTokenReused, "refresh_token_reused"),
        (ErrorCode::UserTokenInvalid, "user_token_invalid"),
        (ErrorCode::UserTokenExpired, "user_token_expired"),
        (ErrorCode::UserSessionRevoked, "user_session_revoked"),
        (ErrorCode::UserTokenWithSecretKey, "user_token_with_secret_key"),
        (ErrorCode::RunIdInUse, "run_id_in_use"),
        (ErrorCode::SqlConfirmRequired, "sql_confirm_required"),
        (ErrorCode::SqlRunLimit, "sql_run_limit"),
        (ErrorCode::SchemaTooLarge, "schema_too_large"),
        (ErrorCode::SavedQueryNameTaken, "saved_query_name_taken"),
        (ErrorCode::SavedQueryConflict, "saved_query_conflict"),
        (ErrorCode::SavedQueryCap, "saved_query_cap"),
        (ErrorCode::MergeConflictsOpen, "merge_conflicts_open"),
        (ErrorCode::MergeDropsUnacknowledged, "merge_drops_unacknowledged"),
        (ErrorCode::MergeNotRehearsed, "merge_not_rehearsed"),
        (ErrorCode::MergeStale, "merge_stale"),
        (ErrorCode::MergeBusy, "merge_busy"),
        (ErrorCode::MergeOpenExists, "merge_open_exists"),
        (ErrorCode::MergeBaseUnavailable, "merge_base_unavailable"),
        (ErrorCode::MergeVersionConflict, "merge_version_conflict"),
        (ErrorCode::MergeConfirmRequired, "merge_confirm_required"),
        (ErrorCode::BranchMergeBusy, "branch_merge_busy"),
        (ErrorCode::RoleAttributeNotAllowed, "role_attribute_not_allowed"),
        (ErrorCode::RoleMembershipNotAllowed, "role_membership_not_allowed"),
    ];

    #[test]
    fn every_code_serialises_snake_case() {
        for (code, name) in ALL {
            assert_eq!(serde_json::to_value(code).unwrap(), name);
            assert_eq!(code.as_str(), name);
            let back: ErrorCode = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(back, code);
        }
    }

    #[test]
    fn branch_protected_is_a_409() {
        let e = ApiError::new(409, ErrorCode::BranchProtected, "protected");
        assert_eq!((e.status, e.code.as_str()), (409, "branch_protected"));
    }

    #[test]
    fn body_shape_and_extras() {
        let e = ApiError::new(409, ErrorCode::NameTaken, "taken")
            .with_extra("keys", serde_json::json!(["a", "b"]));
        assert_eq!(
            e.body(),
            serde_json::json!({"error": {"code": "name_taken", "message": "taken", "keys": ["a", "b"]}})
        );
        let n = ApiError::not_found("nope");
        assert_eq!((n.status, n.code), (404, ErrorCode::NotFound));
        let i = ApiError::internal();
        assert_eq!((i.status, i.message.as_str()), (500, "internal error"));
        assert_eq!(ApiError::unauthorized("x").status, 401);
        assert_eq!(
            ApiError::invalid_request(422, "x").code,
            ErrorCode::InvalidRequest
        );
    }

    #[tokio::test]
    async fn retry_after_header() {
        let r = ApiError::new(429, ErrorCode::RateLimited, "slow down")
            .retry_after(7)
            .into_response();
        assert_eq!(r.status(), 429);
        assert_eq!(r.headers()["retry-after"], "7");
        assert_eq!(r.headers()["content-type"], "application/json");
        let r = ApiError::not_found("x").into_response();
        assert!(r.headers().get("retry-after").is_none());
        let bytes = axum::body::to_bytes(r.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"]["code"], "not_found");
    }
}
