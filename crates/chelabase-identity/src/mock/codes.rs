//! The fake's m2 routes (#115): `POST /pools/{pool}/codes`, `POST /pools/{pool}/codes/verify`.
//! (`GET /email/status` is in the parent's router.) Kept apart so they are easy to adjust once
//! m2's real `openapi.json` is known. Assumed behaviour, from #115's acceptance criteria:
//!
//! - A code is six digits, bound to `(pool, user, kind)`, single use. TTL 15 minutes, 7 days for
//!   `invite`. Another kind's code never opens it.
//! - A wrong code is `400 code_invalid` and counts; at 5 wrong, the code is locked and every
//!   further try (the right code too) is `429 code_locked` until a new code is issued.
//! - Issuing a new code replaces the old one; the attempts of a still-open code carry over
//!   (a new code does not give a fresh five), but a locked code's replacement starts clean.
//! - An unknown user on verify is `code_invalid`, not `user_not_found` (enumeration resistance).
//! - `verify` marks the email verified. `reset` and `invite` need `new_password` (400
//!   `invalid_request` / `weak_password` without consuming the code), set it, and revoke every
//!   session of the user; `invite` also marks the email verified.

use super::{Inner, Reply, SentCode, check_password, refuse, str_field, user_idx, user_out};
use crate::codes::CodeKind;
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde_json::{Value, json};

pub const MAX_ATTEMPTS: u32 = 5;

#[derive(Clone, Debug)]
pub(crate) struct CodeRow {
    pub code: String,
    pub expires: DateTime<Utc>,
    pub attempts: u32,
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
        _ => None,
    }
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
    i.next += 1;
    let code = format!("{:06}", (i.next * 7919 + 100_003) % 1_000_000);
    let p = super::pool_mut(i, pool)?;
    let email = p.users[user_idx(p, user_id)?].email.clone();
    if delivery == "email" && !configured {
        return Err(refuse(409, "email_not_configured", "no SMTP is configured"));
    }
    let key = (user_id.to_string(), kind);
    let carried = p
        .codes
        .get(&key)
        .filter(|c| c.expires > now && c.attempts < MAX_ATTEMPTS)
        .map_or(0, |c| c.attempts);
    let expires = now + ttl(kind);
    p.codes.insert(
        key,
        CodeRow {
            code: code.clone(),
            expires,
            attempts: carried,
        },
    );
    let mut out = json!({ "expires_at": expires.to_rfc3339_opts(SecondsFormat::Millis, true) });
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
    let new_password = body.get("new_password").and_then(Value::as_str);
    let now = i.now();
    let p = super::pool_mut(i, pool)?;
    let invalid = || refuse(400, "code_invalid", "the code is not valid");
    let at = user_idx(p, user_id).map_err(|_| invalid())?;
    let key = (user_id.to_string(), kind);
    let row = p.codes.get_mut(&key).ok_or_else(invalid)?;
    if row.expires <= now {
        p.codes.remove(&key);
        return Err(invalid());
    }
    if row.attempts >= MAX_ATTEMPTS {
        return Err(refuse(
            429,
            "code_locked",
            "too many wrong codes: ask for a new one",
        ));
    }
    if row.code != given {
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
