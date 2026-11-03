//! The sign-up gate and the fresh-install claim, for the `auth` service only (bearer
//! `CHELABASE_APP_AUTH_INTERNAL_TOKEN`, like `/internal/app/*`). Every body is JSON, so the
//! setup code and the claim nonce never go in a URL.
//!
//! - `POST /internal/signup/check {setup_code?}`: may a visitor create an account in the
//!   instance's tenant ([`InternalState::tenant`], the seeded one)? A tenant with a member
//!   follows its open sign-up switch and ignores any code.
//!   A member-less tenant (a fresh install) takes a sign-up only with the setup code that
//!   `chelabase-init secrets` printed (`CHELABASE_SETUP_CODE_FILE`); without a configured code
//!   it takes none (fail closed). The right code also gets a one-time claim nonce.
//! - `POST /internal/signup/claimed {user_id, nonce}`: after it created the identity user,
//!   `auth` binds the tenant's claim to it by spending the nonce ([`MetadataClient::claim_setup`]),
//!   and that user becomes the tenant admin ([`MetadataClient::admit_member`]). A member token
//!   never makes a row, so this is where it is made. The bearer alone binds nothing: a claim
//!   needs a nonce, which only the code gets.
//! - `POST /internal/signup/joined {user_id}`: the same for an open sign-up, whose row is
//!   `pending` until an admin approves it. It asks the gate's question again, and writes no row
//!   for a user whose email isn't verified.
//! - `POST /internal/members/active {user_id}`: the tenants a person is an active or pending
//!   member of. `auth` chooses a sign-in's tenant from it and re-reads it before minting.
//!
//! [`MetadataClient::claim_setup`]: chelabase_metadata::MetadataClient::claim_setup
//! [`MetadataClient::admit_member`]: chelabase_metadata::MetadataClient::admit_member

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_identity::{Identity, IdentityError, Missing, Pool, User};
use chelabase_metadata::members::{Admission, MemberStatus, SetupClaimOutcome};
use chelabase_neon::ComputeLocator;
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::mode_policy::SignUpRule;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use super::{InternalState, NeonMetadataPort, constant_time_eq};

/// How long a claim nonce stays spendable: one sign-up's account creation.
pub const SETUP_NONCE_TTL: Duration = Duration::from_secs(5 * 60);

/// A setup code's normalised length: `XXXX-XXXX-XXXX-XXXX-XXXX` without its dashes.
const SETUP_CODE_LEN: usize = 20;

/// The setup code, held only as the SHA-256 of its normalised form. `Debug` shows nothing.
#[derive(Clone)]
pub struct SetupCode([u8; 32]);

impl std::fmt::Debug for SetupCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SetupCode(..)")
    }
}

/// Upper case, without dashes and whitespace: how a code is compared, however it was typed.
fn normalise(code: &str) -> String {
    code.chars()
        .filter(|c| *c != '-' && !c.is_whitespace())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn digest(normalised: &str) -> [u8; 32] {
    Sha256::digest(normalised.as_bytes()).into()
}

impl SetupCode {
    /// A stored code: 20 ASCII letters or digits once normalised. Anything else (empty, short,
    /// garbage) is no code, so the claim window stays closed.
    pub fn parse(text: &str) -> Option<Self> {
        let n = normalise(text);
        (n.len() == SETUP_CODE_LEN && n.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then(|| Self(digest(&n)))
    }

    /// Whether `presented` is this code, in any case, with or without dashes. Both sides are
    /// compared as fixed-size digests in constant time, so timing shows neither how much
    /// matched nor the code's length.
    pub fn matches(&self, presented: &str) -> bool {
        constant_time_eq(&digest(&normalise(presented)), &self.0)
    }
}

/// The setup code from `CHELABASE_SETUP_CODE_FILE` (`path`), read once at startup. Unset,
/// unreadable, empty or malformed is `None`: no first-member sign-up and no claim, logged as
/// such. The log names the file, never its contents.
pub fn load_setup_code(path: Option<&str>) -> Option<SetupCode> {
    let Some(path) = path.map(str::trim).filter(|p| !p.is_empty()) else {
        warn!("CHELABASE_SETUP_CODE_FILE is not set: the fresh-install claim window is closed (no first-member sign-up)");
        return None;
    };
    match std::fs::read_to_string(path) {
        Ok(text) => match SetupCode::parse(&text) {
            Some(code) => {
                info!(path, "setup code loaded: a fresh install's first sign-up needs it");
                Some(code)
            }
            None => {
                warn!(path, "the setup code file holds no setup code: the fresh-install claim window is closed");
                None
            }
        },
        Err(e) => {
            warn!(path, error = %e.kind(), "the setup code file can't be read: the fresh-install claim window is closed");
            None
        }
    }
}

/// `Debug` is not derived: the body holds the setup code.
#[derive(Deserialize)]
pub(super) struct CheckBody {
    #[serde(default)]
    setup_code: Option<String>,
}

fn answer(allowed: bool, reason: &str, setup_required: bool) -> Response {
    Json(json!({"allowed": allowed, "reason": reason, "setup_required": setup_required}))
        .into_response()
}

/// `{"allowed", "reason", "setup_required", "claim_nonce"?}`; `reason` is `open`,
/// `first_member` (with `claim_nonce`), `closed`, `setup_code_invalid` or `platform`.
/// `setup_required` is true while a member-less tenant waits for the code. A missing tenant is
/// `closed`.
///
/// "Member-less" is the tenant having no invited, pending, active or suspended member (only
/// `removed` ones, or none): once anyone is in, the code is dead. Two concurrent sign-ups with
/// the code both get a nonce and an account; only one binds the claim (`claimed`), and the
/// other's first request makes it a `pending` request (M7, `FIRST_MEMBER_WINDOW`).
pub(super) async fn check<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Json(b): Json<CheckBody>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    if state.policy.sign_up() == SignUpRule::Refused {
        return answer(false, "platform", false);
    }
    let facts = match state.metadata.signup_facts(state.tenant).await {
        Ok(Some(f)) => f,
        Ok(None) => return answer(false, "closed", false),
        Err(e) => {
            error!("sign-up gate: {e}");
            return ApiError::internal().into_response();
        }
    };
    if facts.has_members {
        return if facts.open { answer(true, "open", false) } else { answer(false, "closed", false) };
    }
    let Some(code) = &state.setup_code else {
        // Fail closed: without a configured code nobody can claim a fresh install.
        return answer(false, "closed", false);
    };
    match b.setup_code.as_deref() {
        Some(presented) if code.matches(presented) => {}
        Some(_) => {
            info!(tenant = %state.tenant, "sign-up gate: wrong setup code");
            return answer(false, "setup_code_invalid", true);
        }
        None => return answer(false, "setup_code_invalid", true),
    }
    let mut nonce = [0u8; 32];
    if getrandom::fill(&mut nonce).is_err() {
        error!("sign-up gate: no randomness for a claim nonce");
        return ApiError::internal().into_response();
    }
    let hash: [u8; 32] = Sha256::digest(nonce).into();
    match state.metadata.insert_setup_nonce(state.tenant, hash, SETUP_NONCE_TTL).await {
        Ok(true) => {}
        Ok(false) => return answer(false, "closed", false),
        Err(e) => {
            error!("sign-up gate: storing the claim nonce: {e}");
            return ApiError::internal().into_response();
        }
    }
    info!(tenant = %state.tenant, "sign-up gate: setup code accepted, claim nonce issued");
    Json(json!({
        "allowed": true,
        "reason": "first_member",
        "setup_required": true,
        "claim_nonce": hex::encode(nonce),
    }))
    .into_response()
}

/// `Debug` is not derived: the body holds the nonce. Missing fields are empty, so they are
/// refused like wrong ones.
#[derive(Deserialize)]
pub(super) struct ClaimedBody {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    nonce: String,
}

fn refused(why: &str) -> Response {
    info!("setup claim refused: {why}");
    ApiError::new(
        StatusCode::FORBIDDEN.as_u16(),
        ErrorCode::SetupCodeInvalid,
        "the setup claim was refused",
    )
    .into_response()
}

fn unavailable(what: &str) -> Response {
    warn!("setup claim: {what}");
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        ErrorCode::Unavailable,
        "the setup claim is unavailable, try again",
    )
    .into_response()
}

/// An identity user id: a UUID, as every id of the service's is.
fn is_user_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

/// The identity user, or `None` for a definite not-found.
async fn identity_user(identity: &Identity, id: &str) -> Result<Option<User>, IdentityError> {
    match identity.get_user(&Pool::Members, id).await {
        Ok(user) => Ok(Some(user)),
        Err(IdentityError::NotFound(Missing::User)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The identity user an open sign-up may write a row for: `None` for a definite not-found and
/// for an unverified email. A `pending` row shows in the admins' request list and takes its
/// address from other rows (`release_email`), so it waits until the address is proven.
async fn joinable_user(identity: &Identity, id: &str) -> Result<Option<User>, IdentityError> {
    Ok(identity_user(identity, id).await?.filter(|user| user.email_verified))
}

/// The nonce's SHA-256, for a nonce of exactly the shape `check` hands out (64 hex digits).
fn nonce_hash(nonce: &str) -> Option<[u8; 32]> {
    let bytes: [u8; 32] = hex::decode(nonce).ok()?.try_into().ok()?;
    Some(Sha256::digest(bytes).into())
}

/// Binds the tenant's claim to `user_id` and writes their member row: 200 `{"bound": true}`,
/// the tenant admin; 200 `{"bound": false}` when the tenant already has an admin or a live
/// claim of another user (the account stays, as a `pending` member); 403 `setup_code_invalid`
/// (nothing written) for a missing, malformed, expired, spent or other tenant's nonce, an unknown
/// user, platform mode, or no configured setup code; 503 when identity can't be asked. `auth`
/// deletes the account it just made on anything but a 200.
///
/// The user must exist in the members pool. A claim of another user is replaced only when that
/// user has no member row and identity no longer has it (a stale claim), or the install would
/// be locked out for good.
pub(super) async fn claimed<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Json(b): Json<ClaimedBody>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    if state.policy.sign_up() == SignUpRule::Refused {
        return refused("platform mode");
    }
    if state.setup_code.is_none() {
        return refused("no setup code is configured");
    }
    let Some(hash) = nonce_hash(&b.nonce) else {
        return refused("no or a malformed nonce");
    };
    if !is_user_id(&b.user_id) {
        return refused("malformed user");
    }
    let Some(identity) = &state.identity else {
        return unavailable("no identity client to check the user");
    };
    let user = match identity_user(identity, &b.user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return refused("no such user"),
        Err(e) => return unavailable(&format!("identity lookup failed: {e}")),
    };
    let current = match state.metadata.setup_claim(state.tenant).await {
        Ok(c) => c,
        Err(e) => {
            error!("setup claim: {e}");
            return ApiError::internal().into_response();
        }
    };
    let replace = match current {
        Some(c) if c.identity_user_id != b.user_id && !c.has_member_row => {
            if !is_user_id(&c.identity_user_id) {
                // A claim from before the identity service (not one of its ids): that user can
                // never sign in again.
                Some(c.identity_user_id)
            } else {
                // Only a definite not-found is "gone"; an unreadable answer fails closed.
                match identity_user(identity, &c.identity_user_id).await {
                    Ok(None) => Some(c.identity_user_id),
                    Ok(Some(_)) => None,
                    Err(e) => return unavailable(&format!("identity lookup failed: {e}")),
                }
            }
        }
        _ => None,
    };
    let outcome = state
        .metadata
        .claim_setup(state.tenant, hash, &b.user_id, replace.as_deref())
        .await;
    let admission = match outcome {
        Ok(Some(SetupClaimOutcome::Bound)) => {
            info!(tenant = %state.tenant, user = %b.user_id, replaced = replace.is_some(), "setup claim bound");
            Admission::Closed
        }
        Ok(Some(SetupClaimOutcome::AlreadyClaimed)) => {
            info!(tenant = %state.tenant, user = %b.user_id, "setup claim: already claimed, the account waits as a newcomer");
            Admission::Pending
        }
        Ok(Some(SetupClaimOutcome::Refused)) => return refused("no live nonce of this tenant"),
        Ok(None) => return refused("the tenant doesn't exist"),
        Err(e) => {
            error!("setup claim: {e}");
            return ApiError::internal().into_response();
        }
    };
    let admit = |admission| {
        state.metadata.admit_sign_up(state.tenant, &b.user_id, &user.email, &user.name, admission)
    };
    let mut admitted = admit(admission).await;
    // An admin appeared between the bind and the row: they wait like any other newcomer.
    if matches!(admitted, Ok(None)) && admission == Admission::Closed {
        admitted = admit(Admission::Pending).await;
    }
    match admitted {
        Ok(status) => Json(json!({"bound": status == Some(MemberStatus::Active)})).into_response(),
        Err(e) => {
            error!("setup claim: writing the member row: {e}");
            ApiError::internal().into_response()
        }
    }
}

#[derive(Deserialize)]
pub(super) struct JoinedBody {
    #[serde(default)]
    user_id: String,
}

/// Writes the `pending` row of a user who signed up while the tenant is open: 200
/// `{"joined": true}`. 200 `{"joined": false}` (nothing written) in platform mode, for a tenant
/// that isn't open now or has no member yet (the setup claim's business), for a missing tenant
/// or user, and for a user whose email isn't verified. 503 when identity can't be asked.
/// `auth` deletes the account unless `joined`. With verification on, `auth` asks only once the
/// email is verified (m11's verify-email).
pub(super) async fn joined<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Json(b): Json<JoinedBody>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let no = || Json(json!({"joined": false})).into_response();
    if state.policy.sign_up() == SignUpRule::Refused || !is_user_id(&b.user_id) {
        return no();
    }
    match state.metadata.signup_facts(state.tenant).await {
        Ok(Some(f)) if f.open && f.has_members => {}
        Ok(_) => return no(),
        Err(e) => {
            error!("sign-up join: {e}");
            return ApiError::internal().into_response();
        }
    }
    let Some(identity) = &state.identity else {
        return unavailable("no identity client to check the user");
    };
    let user = match joinable_user(identity, &b.user_id).await {
        Ok(Some(user)) => user,
        Ok(None) => return no(),
        Err(e) => return unavailable(&format!("identity lookup failed: {e}")),
    };
    match state
        .metadata
        .admit_sign_up(state.tenant, &b.user_id, &user.email, &user.name, Admission::Pending)
        .await
    {
        Ok(status) => Json(json!({"joined": status.is_some()})).into_response(),
        Err(e) => {
            error!("sign-up join: writing the member row: {e}");
            ApiError::internal().into_response()
        }
    }
}

#[derive(Deserialize)]
pub(super) struct ActiveBody {
    #[serde(default)]
    user_id: String,
}

/// `{"active": [tenant…], "pending": [tenant…]}`, each ordered by tenant id. Empty lists for a
/// person with neither: a suspended, removed or invited member is in neither.
pub(super) async fn active<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Json(b): Json<ActiveBody>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let rows = match state.metadata.member_tenants(&b.user_id).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("member tenants: {e}");
            return ApiError::internal().into_response();
        }
    };
    let of = |want: MemberStatus| {
        rows.iter().filter(|(_, s)| *s == want).map(|(t, _)| t.as_uuid()).collect::<Vec<_>>()
    };
    Json(json!({"active": of(MemberStatus::Active), "pending": of(MemberStatus::Pending)})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "ABCD-EFGH-JKMN-PQRS-TUVW";

    #[test]
    fn code_compare_normalises_and_is_constant_time() {
        let code = SetupCode::parse(CODE).unwrap();
        for ok in [
            CODE,
            "abcd-efgh-jkmn-pqrs-tuvw",
            "ABCDEFGHJKMNPQRSTUVW",
            "abcdEFGHjkmnPQRStuvw",
            " ABCD-EFGH-JKMN-PQRS-TUVW\n",
            "AB-CD-EF-GH-JK-MN-PQ-RS-TU-VW",
        ] {
            assert!(code.matches(ok), "{ok:?}");
        }
        for bad in ["", "ABCD-EFGH-JKMN-PQRS-TUVX", "ABCD-EFGH-JKMN-PQRS", "ABCD-EFGH-JKMN-PQRS-TUVWX", "-"] {
            assert!(!code.matches(bad), "{bad:?}");
        }
        // The comparison is between fixed-size digests (constant time, length hidden): the
        // stored form is the digest, never the code.
        assert_eq!(code.0, digest("ABCDEFGHJKMNPQRSTUVW"));
        assert_eq!(format!("{code:?}"), "SetupCode(..)");
        assert!(!format!("{code:?}").contains("ABCD"));
    }

    #[test]
    fn a_stored_code_must_have_the_shape() {
        assert!(SetupCode::parse(CODE).is_some());
        assert!(SetupCode::parse(&format!("{CODE}\n")).is_some());
        for bad in ["", "   \n", "ABCD-EFGH", "ABCD-EFGH-JKMN-PQRS-TUV!", "ÄBCD-EFGH-JKMN-PQRS-TUV"] {
            assert!(SetupCode::parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn no_code_file_closes_window() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_setup_code(None).is_none());
        assert!(load_setup_code(Some("")).is_none());
        let missing = dir.path().join("missing.code");
        assert!(load_setup_code(missing.to_str()).is_none());
        let empty = dir.path().join("empty.code");
        std::fs::write(&empty, "").unwrap();
        assert!(load_setup_code(empty.to_str()).is_none());
        let garbage = dir.path().join("garbage.code");
        std::fs::write(&garbage, "hunter2").unwrap();
        assert!(load_setup_code(garbage.to_str()).is_none());
        // A directory can't be read as a file.
        assert!(load_setup_code(dir.path().to_str()).is_none());
        let good = dir.path().join("setup.code");
        std::fs::write(&good, format!("{CODE}\n")).unwrap();
        assert!(load_setup_code(good.to_str()).unwrap().matches(CODE));
    }

    // #121 audit S1: an address nobody proved gets no `pending` row, so it can't squat an
    // email in the admin's request list.
    #[tokio::test]
    async fn only_a_verified_user_may_join() {
        use chelabase_identity::mock::FakeIdentity;
        let mock = FakeIdentity::start().await;
        let verified = mock.with_user(&Pool::Members, "a@x.com", "password1");
        let unverified = mock.with_unverified_user(&Pool::Members, "b@x.com", "password1");
        let identity = mock.client();
        let joinable = joinable_user(&identity, &verified).await.unwrap();
        assert_eq!(joinable.map(|u| u.id), Some(verified));
        assert!(joinable_user(&identity, &unverified).await.unwrap().is_none(), "unverified");
        let unknown = uuid::Uuid::new_v4().to_string();
        assert!(joinable_user(&identity, &unknown).await.unwrap().is_none(), "unknown");
        mock.set_down(true);
        assert!(joinable_user(&identity, &unknown).await.is_err(), "unreadable is not a no");
    }

    #[test]
    fn nonces_and_user_ids_have_one_shape() {
        assert!(nonce_hash(&"ab".repeat(32)).is_some());
        for bad in ["", "ab", &"ab".repeat(33), &"zz".repeat(32)] {
            assert!(nonce_hash(bad).is_none(), "{bad:?}");
        }
        assert!(is_user_id("0195b3e6-7a10-7c3e-9d50-6f8a3b1c2d4e"));
        for bad in ["", "12a", "../1", "312345678901234567", "0195B3E6-7A10-7C3E-9D50-6F8A3B1C2D4E"] {
            assert!(!is_user_id(bad), "{bad:?}");
        }
    }
}
