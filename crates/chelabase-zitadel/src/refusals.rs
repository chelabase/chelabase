//! Zitadel v4.19.1's own refusals, as `tests/zitadel_live.rs` captured them from the real
//! instance: the HTTP status, the gRPC `code` and the error detail's `id`. Match on these, never
//! on a `message`: Zitadel localizes it.
//!
//! The same "user could not be found" has a different id on each endpoint, so each constant
//! names the endpoint it was captured on.

/// One refusal, as Zitadel answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: u32,
    pub id: &'static str,
}

const fn refusal(status: u16, code: u32, id: &'static str) -> Refusal {
    Refusal { status, code, id }
}

// gRPC codes, as Zitadel's error bodies carry them.
pub const INVALID_ARGUMENT: u32 = 3;
pub const NOT_FOUND: u32 = 5;
pub const PERMISSION_DENIED: u32 = 7;
pub const ALREADY_EXISTS: u32 = 6;
pub const FAILED_PRECONDITION: u32 = 9;
pub const UNAUTHENTICATED: u32 = 16;

// --- Not found ----------------------------------------------------------------------------------

/// `GET /v2/users/{id}` for a deleted or never-existing user (#57).
pub const GET_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "QUERY-Dfbg2");
/// `DELETE /v2/users/{id}` for a deleted or never-existing user.
pub const DELETE_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-bd4ir1mblj");
/// `POST /v2/users/{id}/lock` for a deleted or never-existing user.
pub const LOCK_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-450yxuqrh1");
/// `POST /v2/users/{id}/unlock` for a deleted or never-existing user.
pub const UNLOCK_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-x377t913pw");
/// `POST /v2/users/{id}/deactivate` for a deleted or never-existing user.
pub const DEACTIVATE_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-5gp2p62iin");
/// `POST /v2/users/{id}/reactivate` for a deleted or never-existing user.
pub const REACTIVATE_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-9hy5kzbuk6");
/// `POST /v2/sessions` naming a deleted or never-existing user.
pub const SESSION_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "QUERY-Dfbg2");
/// `POST /v2/users/{id}/invite_code/verify` for a deleted or never-existing user: a 400, not a
/// 404.
pub const INVITE_USER_NOT_FOUND: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-F5g2h");
/// `POST /v2/users/new` in a missing organization: a 400.
pub const CREATE_USER_ORG_NOT_FOUND: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-QXPGs");
/// `GET /v2/users/{id}/authentication_methods` for a deleted or never-existing user: the same
/// answer as `GET_USER_NOT_FOUND`.
pub const AUTH_METHODS_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "QUERY-Dfbg2");
/// `POST /v2/users/{id}/email` for a deleted or never-existing user: "Email not found".
pub const SET_EMAIL_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-ieJ2e");
/// `PATCH /v2/users/{id}` (UpdateUser) for a deleted or never-existing user.
pub const UPDATE_USER_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-ugjs0upun6");
/// `POST /v2/users/{id}/invite_code` (`sendCode` or `returnCode`) for a deleted or
/// never-existing user: a 400, not a 404.
pub const CREATE_INVITE_USER_NOT_FOUND: Refusal =
    refusal(400, FAILED_PRECONDITION, "COMMAND-Wgvn4");
/// `GET /v2/oidc/auth_requests/{id}` for an unknown auth request.
pub const AUTH_REQUEST_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "QUERY-Thee9");
/// `POST /v2/oidc/auth_requests/{id}` (finishing the sign-in) for an unknown auth request:
/// another id than the `GET`'s.
pub const FINISH_AUTH_REQUEST_NOT_FOUND: Refusal = refusal(404, NOT_FOUND, "COMMAND-jae5P");

// `DELETE /v2/organizations/{id}` and `DELETE /v2/sessions/{id}` answer 200 for an object that
// was deleted before or never existed, so they have no not-found answer.

// --- State changes (the user is not in the state the change starts from) ------------------------

/// `lock` on a user that is locked already, or inactive: the same answer for both.
pub const LOCK_NOT_ACTIVE: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-lgws8wtsqf");
/// `unlock` on a user that isn't locked (active or inactive).
pub const UNLOCK_NOT_LOCKED: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-olb9vb0oca");
/// `deactivate` on an inactive user. Deactivating a locked user succeeds (it becomes inactive).
pub const DEACTIVATE_ALREADY_INACTIVE: Refusal =
    refusal(400, FAILED_PRECONDITION, "COMMAND-5gunjw0cd7");
/// `reactivate` on a user that isn't inactive (active, locked or initial).
pub const REACTIVATE_NOT_INACTIVE: Refusal =
    refusal(400, FAILED_PRECONDITION, "COMMAND-s5qqcz97hf");
/// `deactivate` on a user in the Initial state, which "can only be deleted".
pub const DEACTIVATE_INITIAL: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-gvx4kct9r2");

// A user in the Initial state (`USER_STATE_INITIAL`) is one made by the v1 management API with
// neither a password nor a verified email; `POST /v2/users/new` never makes one (a passwordless
// invitee is active). For such a user `reactivate` answers `REACTIVATE_NOT_INACTIVE` and
// `unlock` `UNLOCK_NOT_LOCKED`, `deactivate` answers `DEACTIVATE_INITIAL`, and `lock` succeeds;
// unlocking it then makes it active, not initial again.

// --- Codes ---------------------------------------------------------------------------------------

/// `POST /v2/users/{id}/invite_code/verify` with a wrong code, an expired code, or for a user
/// with no pending code: Zitadel answers all three the same.
pub const INVITE_CODE_INVALID: Refusal = refusal(400, INVALID_ARGUMENT, "COMMAND-Wgn4q");
/// `POST /v2/users/{id}/email/verify` with a wrong code.
pub const EMAIL_CODE_INVALID: Refusal = refusal(400, INVALID_ARGUMENT, "COMMAND-eis9R");
/// `POST /v2/users/{id}/password` with a `verificationCode` that isn't the pending one.
pub const PASSWORD_CODE_NOT_FOUND: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-2M9fs");
/// `POST /v2/users/{id}/invite_code` for a user who can sign in already.
pub const INVITE_USER_INITIALIZED: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-EF34g");

// --- Passwords -----------------------------------------------------------------------------------

/// A wrong password, in `POST /v2/sessions` and as `currentPassword`. Its detail is a
/// `CredentialsCheckError`, not an `ErrorDetail`.
pub const PASSWORD_INVALID: Refusal = refusal(400, INVALID_ARGUMENT, "COMMAND-3M0fs");
/// `POST /v2/users/{id}/password`, the password policy's checks (the default policy).
pub const PASSWORD_TOO_SHORT: Refusal = refusal(400, INVALID_ARGUMENT, "DOMAIN-HuJf6");
pub const PASSWORD_NO_UPPER: Refusal = refusal(400, INVALID_ARGUMENT, "DOMAIN-VoaRj");
pub const PASSWORD_NO_LOWER: Refusal = refusal(400, INVALID_ARGUMENT, "DOMAIN-co3Xw");
pub const PASSWORD_NO_NUMBER: Refusal = refusal(400, INVALID_ARGUMENT, "DOMAIN-ZBv4H");
pub const PASSWORD_NO_SYMBOL: Refusal = refusal(400, INVALID_ARGUMENT, "DOMAIN-ZDLwA");
/// `POST /v2/users/new` with a password the policy refuses for its length: the id's prefix
/// differs from `PASSWORD_TOO_SHORT`'s.
pub const CREATE_USER_PASSWORD_TOO_SHORT: Refusal = refusal(400, INVALID_ARGUMENT, "COMMA-HuJf6");
/// `POST /v2/users/new`, the password policy's other checks: the same suffixes as the
/// `PASSWORD_NO_*` ids, with the create path's `COMMA-` prefix.
pub const CREATE_USER_PASSWORD_NO_UPPER: Refusal = refusal(400, INVALID_ARGUMENT, "COMMA-VoaRj");
pub const CREATE_USER_PASSWORD_NO_LOWER: Refusal = refusal(400, INVALID_ARGUMENT, "COMMA-co3Xw");
pub const CREATE_USER_PASSWORD_NO_NUMBER: Refusal = refusal(400, INVALID_ARGUMENT, "COMMA-ZBv4H");
pub const CREATE_USER_PASSWORD_NO_SYMBOL: Refusal = refusal(400, INVALID_ARGUMENT, "COMMA-ZDLwA");

// --- Other ---------------------------------------------------------------------------------------

/// `POST /v2/users/{id}/email` with the address the user has.
pub const EMAIL_NOT_CHANGED: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-Uch5e");
/// `POST /v2/users/new` with a username (or id) that is taken.
pub const USER_ALREADY_EXISTS: Refusal = refusal(409, ALREADY_EXISTS, "V3-DKcYh");
/// `POST /v2/organizations` with an id that is taken.
pub const ORG_ALREADY_EXISTS: Refusal = refusal(409, ALREADY_EXISTS, "ORG-laho2n");
/// `POST /v2/sessions` for a locked user: the user check refuses it when Zitadel's projection
/// already shows the lock, else the password check does (`SESSION_USER_LOCKED`). Either may
/// come.
pub const SESSION_USER_NOT_ACTIVE: Refusal = refusal(400, FAILED_PRECONDITION, "SESSION-Gj4ko");
/// `POST /v2/sessions` for a locked user, refused by the password check.
pub const SESSION_USER_LOCKED: Refusal = refusal(400, FAILED_PRECONDITION, "COMMAND-JLK35");
/// Any call with a token Zitadel doesn't accept.
pub const TOKEN_INVALID: Refusal = refusal(401, UNAUTHENTICATED, "AUTH-7fs1e");
/// A call the token's role has no permission for: Zitadel's authorization check, before the
/// call's own logic, so the same answer on every endpoint. Captured with the login client's
/// token on `DELETE /v2/users/{id}`, `POST /v2/organizations` and
/// `DELETE /v2/organizations/{id}`. A user that doesn't exist is the endpoint's own not-found
/// even then (`DELETE_USER_NOT_FOUND`). The login client may verify invite codes, update users,
/// set a password without a code and list the email providers.
pub const NO_PERMISSION: Refusal = refusal(403, PERMISSION_DENIED, "AUTH-AWfge");

// --- Request validation --------------------------------------------------------------------------

// A request that fails Zitadel's own field validation (a password over 200 characters, in
// `POST /v2/users/new` and `POST /v2/users/{id}/password`) is a 400 with gRPC code 3
// (`INVALID_ARGUMENT`), a `message` naming the field, and no `details` entry, so it has no id:
//
//     {"code":3, "message":"invalid SetPasswordRequest.NewPassword: embedded message failed
//      validation | caused by: invalid Password.Password: value length must be between 1 and
//      200 runes, inclusive"}
//
// The client reads it as `ZError::Rejected { status: 400, code: Some(3), id: None, .. }`
// (`ZError::is_invalid_request`). Nothing settles or counts as gone on it, since that needs an id.

/// The longest password Zitadel's validation accepts, in runes (characters).
pub const PASSWORD_VALIDATION_MAX_RUNES: usize = 200;
/// The longest password Zitadel can store, in bytes: it hashes with bcrypt, which takes 72 bytes
/// at most. A longer one that passes validation (73 bytes to 200 characters) fails with a 500,
/// which the client reads as `Unavailable`: `POST /v2/users/new` answers
/// `{"code":2, "message":"bcrypt: password length exceeds 72 bytes ()"}` (a detail with no
/// id), and `POST /v2/users/{id}/password` `COMMAND-CahN2` "An internal error occurred".
pub const PASSWORD_MAX_BYTES: usize = 72;
