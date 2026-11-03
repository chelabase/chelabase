//! The fake's m2 routes: `POST /pools/{pool}/codes`, `POST /pools/{pool}/codes/verify`.
//! (`GET /email/status` is in the parent's router.) Mirrors `services/identity/src/routes/codes.ts`
//! and `email/codes.ts`:
//!
//! - A code is six digits, bound to `(pool, user, kind)`; kinds are strict. TTL 15 minutes, 7
//!   days for `invite`. A new code replaces the old one and resets the attempts.
//! - Any wrong, expired, used, replaced or locked code, or an unknown user, is `400
//!   invalid_code`. A code allows 5 wrong guesses; after that every guess is refused.
//! - A code is bound to the email it was sent to: a guess against another address never
//!   matches (and counts as a wrong guess), so changing a user's email kills their codes.
//! - An `invite` is refused for an established account (a password and a verified email): 409
//!   `invite_refused` when issued, `invalid_code` when verified, before the guess is counted.
//! - `verify` refuses `new_password`. `reset` and `invite` require it (400 `invalid_request`;
//!   400 `weak_password` before the code is spent), set it, mark the email verified and revoke
//!   every session of the user.
//! - `delivery: email` is 409 `email_not_configured` without SMTP (checked before the user).
//!   The fake does not model caller tokens, so the orchestrator's invite-only rule is not here.

use super::{Inner, Reply, SentCode, SentNotice, check_password, refuse, str_field, user_idx, user_out};
use crate::codes::CodeKind;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};

pub const MAX_ATTEMPTS: u32 = 5;

#[derive(Clone, Debug)]
pub(crate) struct CodeRow {
    /// The user's email when the code was issued.
    pub email: String,
    pub code: String,
    pub expires: DateTime<Utc>,
    pub attempts: u32,
}

/// A password and a verified email: an invite never overwrites that, a reset does.
fn established(u: &super::FUser) -> bool {
    u.verified && u.password.is_some()
}

fn kind_of(body: &Value) -> Result<CodeKind, super::Refusal> {
    match str_field(body, "kind")? {
        "verify" => Ok(CodeKind::Verify),
        "reset" => Ok(CodeKind::Reset),
        "invite" => Ok(CodeKind::Invite),
        _ => Err(refuse(
            400,
            "invalid_request",
            "kind must be verify, reset or invite",
        )),
    }
}

fn ttl(kind: CodeKind) -> Duration {
    match kind {
        CodeKind::Invite => Duration::days(7),
        _ => Duration::minutes(15),
    }
}

/// `Some` when the route is one of the code routes.
pub(super) fn route(
    i: &mut Inner,
    pool: &str,
    method: &str,
    tail: &[&str],
    body: &Value,
) -> Option<Reply> {
    match (method, tail) {
        ("POST", ["codes"]) => Some(create(i, pool, body)),
        ("POST", ["codes", "verify"]) => Some(verify(i, pool, body)),
        ("POST", ["users", id, "invite-notice"]) => Some(notice(i, pool, id, body)),
        _ => None,
    }
}

/// As the service's `invite.notice`: members pool only, a link without a code, SMTP, the user.
fn notice(i: &mut Inner, pool: &str, id: &str, body: &Value) -> Reply {
    if pool != "members" {
        return Err(refuse(400, "invalid_pool", "invite notices are for the members pool"));
    }
    let link = str_field(body, "link")?.to_string();
    if link.split(['?', '&']).any(|part| part.starts_with("code=")) {
        return Err(refuse(400, "invalid_request", "link must not carry a code"));
    }
    if !i.email_configured {
        return Err(refuse(409, "email_not_configured", "no SMTP is configured"));
    }
    let p = super::pool_mut(i, pool)?;
    let email = p.users[user_idx(p, id)?].email.clone();
    i.notices.push(SentNotice { user_id: id.to_string(), email, link });
    Ok((202, Some(json!({ "sent": true }))))
}

fn create(i: &mut Inner, pool: &str, body: &Value) -> Reply {
    let user_id = str_field(body, "user_id")?;
    let kind = kind_of(body)?;
    let delivery = str_field(body, "delivery")?;
    if !matches!(delivery, "email" | "return") {
        return Err(refuse(
            400,
            "invalid_request",
            "delivery must be email or return",
        ));
    }
    let (now, configured) = (i.now(), i.email_configured);
    if delivery == "email" && !configured {
        return Err(refuse(409, "email_not_configured", "no SMTP is configured"));
    }
    i.next += 1;
    let code = format!("{:06}", (i.next * 7919 + 100_003) % 1_000_000);
    let p = super::pool_mut(i, pool)?;
    let u = &p.users[user_idx(p, user_id)?];
    if kind == CodeKind::Invite && established(u) {
        return Err(refuse(
            409,
            "invite_refused",
            "the user already has a password and a verified email; use a reset code",
        ));
    }
    let email = u.email.clone();
    let expires = now + ttl(kind);
    p.codes.insert(
        (user_id.to_string(), kind),
        CodeRow {
            email: email.clone(),
            code: code.clone(),
            expires,
            attempts: 0,
        },
    );
    let mut out = json!({
        "kind": kind.as_str(),
        "delivery": delivery,
        "expires_at": expires.to_rfc3339_opts(SecondsFormat::Millis, true),
    });
    if delivery == "return" {
        out["code"] = json!(code);
    } else {
        i.sent.push(SentCode {
            pool: pool.into(),
            user_id: user_id.into(),
            email,
            kind,
            code,
        });
    }
    Ok((201, Some(out)))
}

fn verify(i: &mut Inner, pool: &str, body: &Value) -> Reply {
    let user_id = str_field(body, "user_id")?;
    let kind = kind_of(body)?;
    let given = str_field(body, "code")?;
    if given.len() != 6 || !given.bytes().all(|b| b.is_ascii_digit()) {
        return Err(refuse(400, "invalid_request", "code must be six digits"));
    }
    let new_password = match (kind, body.get("new_password")) {
        (CodeKind::Verify, None) => None,
        (CodeKind::Verify, Some(_)) => {
            return Err(refuse(
                400,
                "invalid_request",
                "new_password is only for reset and invite codes",
            ));
        }
        (_, Some(Value::String(pw))) if !pw.is_empty() => Some(pw.as_str()),
        _ => return Err(refuse(400, "invalid_request", "new_password is required")),
    };
    if let Some(pw) = new_password {
        check_password(pw)?;
    }
    let now = i.now();
    let p = super::pool_mut(i, pool)?;
    let invalid = || {
        refuse(
            400,
            "invalid_code",
            "the code is wrong, expired, used up or was replaced",
        )
    };
    let at = user_idx(p, user_id).map_err(|_| invalid())?;
    if kind == CodeKind::Invite && established(&p.users[at]) {
        return Err(invalid());
    }
    let current = p.users[at].email.clone();
    let key = (user_id.to_string(), kind);
    let row = p.codes.get_mut(&key).ok_or_else(invalid)?;
    if row.expires <= now {
        p.codes.remove(&key);
        return Err(invalid());
    }
    if row.attempts >= MAX_ATTEMPTS {
        return Err(invalid());
    }
    if row.code != given || row.email != current {
        row.attempts += 1;
        return Err(invalid());
    }
    if kind != CodeKind::Verify {
        let pw = new_password
            .ok_or_else(|| refuse(400, "invalid_request", "new_password is required"))?;
        check_password(pw)?;
    }
    p.codes.remove(&key);
    let u = &mut p.users[at];
    u.verified = true;
    u.updated = now;
    if kind != CodeKind::Verify {
        u.password = new_password.map(str::to_string);
        p.sessions.retain(|s| s.user_id != user_id);
    }
    let out = user_out(&p.users[at]);
    Ok((200, Some(json!({ "user": out }))))
}
