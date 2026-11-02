//! Every route of the identity service, once. The client calls each through its entry here, so
//! `tests/spec_coverage.rs` can hold the list against `openapi.json`.

#[derive(Debug, Clone, Copy)]
pub struct Op {
    /// The constant's name in this module (the spec test counts its call sites).
    pub name: &'static str,
    /// The spec's `operationId` (m2's three are assumed until its spec lands).
    pub operation_id: &'static str,
    pub method: &'static str,
    /// The path template, `{pool}` and `{id}` filled by the client.
    pub path: &'static str,
}

macro_rules! ops {
    ($($name:ident = $id:literal, $method:literal, $path:literal;)*) => {
        $(pub const $name: Op = Op { name: stringify!($name), operation_id: $id, method: $method, path: $path };)*
        pub const OPERATIONS: &[Op] = &[$($name),*];
    };
}

ops! {
    CREATE_POOL = "createPool", "POST", "/internal/v1/pools/{pool}";
    DROP_POOL = "dropPool", "DELETE", "/internal/v1/pools/{pool}";
    CREATE_USER = "createUser", "POST", "/internal/v1/pools/{pool}/users";
    LIST_USERS = "listUsers", "GET", "/internal/v1/pools/{pool}/users";
    GET_USER = "getUser", "GET", "/internal/v1/pools/{pool}/users/{id}";
    DELETE_USER = "deleteUser", "DELETE", "/internal/v1/pools/{pool}/users/{id}";
    SET_EMAIL = "setEmail", "PUT", "/internal/v1/pools/{pool}/users/{id}/email";
    SET_PASSWORD = "setPassword", "PUT", "/internal/v1/pools/{pool}/users/{id}/password";
    MARK_EMAIL_VERIFIED = "markEmailVerified", "POST", "/internal/v1/pools/{pool}/users/{id}/verify-email";
    BAN_USER = "banUser", "POST", "/internal/v1/pools/{pool}/users/{id}/ban";
    UNBAN_USER = "unbanUser", "POST", "/internal/v1/pools/{pool}/users/{id}/unban";
    REVOKE_USER_SESSIONS = "revokeUserSessions", "DELETE", "/internal/v1/pools/{pool}/users/{id}/sessions";
    SIGN_IN = "signIn", "POST", "/internal/v1/pools/{pool}/sessions";
    SESSION_BY_TOKEN = "getSessionByToken", "POST", "/internal/v1/pools/{pool}/sessions/lookup";
    SESSION_BY_ID = "getSessionById", "GET", "/internal/v1/pools/{pool}/sessions/{id}";
    REVOKE_SESSION = "revokeSession", "DELETE", "/internal/v1/pools/{pool}/sessions/{id}";
    // m2 (#115), see codes.rs.
    CREATE_CODE = "createCode", "POST", "/internal/v1/pools/{pool}/codes";
    VERIFY_CODE = "verifyCode", "POST", "/internal/v1/pools/{pool}/codes/verify";
    EMAIL_STATUS = "emailStatus", "GET", "/internal/v1/email/status";
}
