//! The fake's m2 routes: `POST /pools/{pool}/codes`, `POST /pools/{pool}/codes/verify`.
//! (`GET /email/status` is in the parent's router.) Mirrors `services/identity/src/routes/codes.ts`
//! and `email/codes.ts`:
//!
//! - A code is six digits, bound to `(pool, user, kind)`; kinds are strict. TTL 15 minutes, 7
//!   days for `invite`. A new code replaces the old one and resets the attempts, except that a
//!   live non-`invite` code issued less than 60 s ago is kept: `429 code_recently_sent`.
//! - Each `(user, kind)` has a failure budget ([`Budget`]): 10 wrong guesses in a 24-hour window
//!   that starts at the first. It outlives codes (new codes, revocation); a right code clears
//!   it; once spent every guess is `429 code_budget_spent`, checked after the user and before
//!   the code.
//! - Any wrong, expired, used, replaced or locked code, or an unknown user, is `400
//!   invalid_code`. A code allows 5 wrong guesses; after that every guess is refused.
//! - A code is bound to the email it was sent to: a guess against another address never
//!   matches (and counts as a wrong guess). A real change of a user's email (`PUT
//!   users/{id}/email`, or the `email_change` swap) also revokes every outstanding code of that
//!   user ([`revoke_codes`]), so a code never comes back when the address changes back.
//! - An `invite` is refused for an established account (a password and a verified email): 409
//!   `invite_refused` when issued, `invalid_code` when verified, before the guess is counted.
//! - `verify` refuses `new_password`. `reset` and `invite` require it (400 `invalid_request`;
//!   400 `weak_password` before the code is spent), set it, mark the email verified and revoke
//!   every session of the user.
//! - `email_change` needs `new_email` (refused for other kinds), is email-only (400 for
//!   `return`), is refused with 409 `conflict` for an address another user holds or the user's
//!   own, and is mailed to `new_email`. It is bound to the current address and `new_email`:
//!   confirming with another address counts as a wrong guess. After a matching guess, a
//!   `new_email` taken since is 409 `conflict` (the code is spent); otherwise the email swaps,
//!   verified, and a [`ChangeNotice`](super::ChangeNotice) to the old address is recorded when
//!   SMTP is configured.
//! - `delivery: email` is 409 `email_not_configured` without SMTP (checked before the user).
//!   The fake does not model caller tokens, so the callers' kind rules are not here.

use super::{
    ChangeNotice, Inner, Reply, SentCode, SentNotice, check_email, check_password, refuse,
    str_field, user_idx, user_out,
};
use crate::codes::CodeKind;
use chelabase_time::{Timestamp, TimestampExt, days, hours, minutes, seconds};
use std::time::Duration;
use serde_json::{Value, json};

pub const MAX_ATTEMPTS: u32 = 5;
pub const FAILURE_BUDGET: u32 = 10;

fn failure_window() -> Duration {
    hours(24)
}

fn replace_after() -> Duration {
    seconds(60)
}

/// A `(user, kind)`'s wrong guesses in the window that started at `since`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Budget {
    pub failures: u32,
    pub since: Option<Timestamp>,
}

impl Budget {
    fn open(&self, now: Timestamp) -> bool {
        self.since.is_some_and(|s| s > now.checked_sub(failure_window()).expect("in range"))
    }

    fn spent(&self, now: Timestamp) -> bool {
        self.open(now) && self.failures >= FAILURE_BUDGET
    }

    fn fail(&mut self, now: Timestamp) {
        if self.open(now) {
            self.failures += 1;
        } else {
            (self.failures, self.since) = (1, Some(now));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CodeRow {
    /// The user's email when the code was issued.
    pub email: String,
    /// The requested new address (`email_change` only).
    pub next: Option<String>,
    pub code: String,
    pub expires: Timestamp,
    pub attempts: u32,
    pub issued: Timestamp,
}

/// Deletes every outstanding code of `user_id`, of every kind (budgets stay). Called whenever
/// the user's address really changes, as the service's `CodeStore.changeEmail` does.
pub(crate) fn revoke_codes(p: &mut super::PoolState, user_id: &str) {
    p.codes.retain(|(uid, _), _| uid != user_id);
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
        "email_change" => Ok(CodeKind::EmailChange),
        _ => Err(refuse(
            400,
            "invalid_request",
            "kind must be verify, reset, invite or email_change",
        )),
    }
}

/// `new_email`: required (and checked) for `email_change`, refused for every other kind.
fn new_email(body: &Value, kind: CodeKind) -> Result<Option<String>, super::Refusal> {
    match (kind, body.get("new_email")) {
        (CodeKind::EmailChange, _) => Ok(Some(check_email(str_field(body, "new_email")?)?)),
        (_, None) => Ok(None),
        (_, Some(_)) => Err(refuse(
            400,
            "invalid_request",
            "new_email is only for email_change codes",
        )),
    }
}

fn ttl(kind: CodeKind) -> Duration {
    match kind {
        CodeKind::Invite => days(7),
        _ => minutes(15),
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
    let next = new_email(body, kind)?;
    let project = super::project_field(body);
    if next.is_some() && delivery != "email" {
        return Err(refuse(
            400,
            "invalid_request",
            "email_change codes are only delivered by email",
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
    if let Some(next) = &next {
        if *next == email {
            return Err(refuse(409, "conflict", "this is already the user's email"));
        }
        if p.users.iter().any(|o| o.email == *next && o.id != user_id) {
            return Err(refuse(409, "conflict", "another user has this email"));
        }
    }
    let key = (user_id.to_string(), kind);
    if kind != CodeKind::Invite
        && p.codes.get(&key).is_some_and(|row| {
            row.expires > now && row.attempts < MAX_ATTEMPTS && row.issued > now.checked_sub(replace_after()).expect("in range")
        })
    {
        return Err(refuse(
            429,
            "code_recently_sent",
            "a code was sent less than a minute ago; use it or wait",
        ));
    }
    let expires = now.checked_add(ttl(kind)).expect("in range");
    p.codes.insert(
        key,
        CodeRow {
            email: email.clone(),
            next: next.clone(),
            code: code.clone(),
            expires,
            attempts: 0,
            issued: now,
        },
    );
    let mut out = json!({
        "kind": kind.as_str(),
        "delivery": delivery,
        "expires_at": expires.to_rfc3339_millis(),
    });
    if delivery == "return" {
        out["code"] = json!(code);
    } else {
        i.sent.push(SentCode {
            pool: pool.into(),
            user_id: user_id.into(),
            // Where it went: the new address for `email_change`.
            email: next.unwrap_or(email),
            kind,
            code,
            link: body.get("link").and_then(Value::as_str).map(str::to_string),
            project,
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
    let next = new_email(body, kind)?;
    let project = super::project_field(body);
    let new_password = match (kind, body.get("new_password")) {
        (CodeKind::Verify | CodeKind::EmailChange, None) => None,
        (CodeKind::Verify | CodeKind::EmailChange, Some(_)) => {
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
    let (now, configured) = (i.now(), i.email_configured);
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
    let budget = p.budgets.entry(key.clone()).or_default();
    if budget.spent(now) {
        return Err(refuse(
            429,
            "code_budget_spent",
            "too many wrong codes; try again later",
        ));
    }
    let row = p.codes.get_mut(&key).ok_or_else(invalid)?;
    if row.expires <= now {
        p.codes.remove(&key);
        return Err(invalid());
    }
    if row.attempts >= MAX_ATTEMPTS {
        return Err(invalid());
    }
    if row.code != given || row.email != current || row.next != next {
        row.attempts += 1;
        budget.fail(now);
        return Err(invalid());
    }
    *budget = Budget::default();
    if let Some(next) = next {
        p.codes.remove(&key);
        if p.users.iter().any(|o| o.email == next && o.id != user_id) {
            return Err(refuse(409, "conflict", "another user has this email"));
        }
        let u = &mut p.users[at];
        let previous = std::mem::replace(&mut u.email, next);
        (u.verified, u.updated) = (true, now);
        let out = user_out(u);
        revoke_codes(p, user_id);
        if configured {
            i.change_notices.push(ChangeNotice {
                pool: pool.into(),
                user_id: user_id.into(),
                email: previous,
                project,
            });
        }
        return Ok((200, Some(json!({ "user": out }))));
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

#[cfg(test)]
mod tests {
    use crate::mock::FakeIdentity;
    use crate::{CodeKind, IdentityError, Pool};

    fn pool() -> Pool {
        Pool::App(uuid::Uuid::from_u128(0x5a))
    }

    fn is_invalid_code(e: &IdentityError) -> bool {
        matches!(e, IdentityError::CodeInvalid)
    }

    /// The code the fake mailed last, and where it went.
    fn last_sent(f: &FakeIdentity) -> (CodeKind, String, String) {
        let sent = f.codes_sent();
        let s = sent.last().expect("a code was mailed");
        (s.kind, s.email.clone(), s.code.clone())
    }

    /// `expires_at` as the fake writes it: exactly three fraction digits and `Z`; invite codes
    /// last 7 days, the others 15 minutes.
    #[tokio::test]
    async fn expires_at_keeps_millisecond_text() {
        use chelabase_time::{Timestamp, TimestampExt, days, minutes};
        let f = FakeIdentity::start().await;
        let p = Pool::Members;
        let invitee = f.with_invited_user(&p, "new@x.io");
        let member = f.with_user(&p, "old@x.io", "password-1");
        let http = chelabase_http::client_builder().build().unwrap();
        for (user, kind, ttl) in [(invitee, "invite", days(7)), (member, "reset", minutes(15))] {
            let before = Timestamp::now();
            let reply: serde_json::Value = http
                .post(format!("{}/internal/v1/pools/members/codes", f.base))
                .bearer_auth(crate::mock::TOKEN)
                .json(&serde_json::json!({"user_id": user, "kind": kind, "delivery": "return"}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let after = Timestamp::now();
            let text = reply["expires_at"].as_str().expect("expires_at is text");
            let b = text.as_bytes();
            assert_eq!(b.len(), 24, "{text}");
            assert!(b[19] == b'.' && b[23] == b'Z', "{text}");
            assert!(b[20..23].iter().all(u8::is_ascii_digit), "{text}");
            let at = Timestamp::parse_rfc3339(text).unwrap();
            let slack = chelabase_time::seconds(1);
            assert!(at >= before.checked_add(ttl).unwrap().checked_sub(std::time::Duration::from_millis(1)).unwrap(), "{text}");
            assert!(at <= after.checked_add(ttl).unwrap().checked_add(slack).unwrap(), "{text}");
        }
    }

    #[tokio::test]
    async fn mock_email_change_binds_the_new_address() {
        let f = FakeIdentity::start().await;
        let (id, p) = (f.client(), pool());
        let user = f.with_user(&p, "old@x.io", "password-1");

        let issued = id
            .create_email_change_code(&p, &user, "New@X.io", Some("Acme"))
            .await
            .unwrap();
        assert_eq!(issued.kind, CodeKind::EmailChange);
        assert_eq!(issued.code, None, "the code never comes back");
        let (kind, to, code) = last_sent(&f);
        assert_eq!((kind, to.as_str()), (CodeKind::EmailChange, "new@x.io"));
        let u = f.user(&p, &user).unwrap();
        assert_eq!((u.email.as_str(), u.verified), ("old@x.io", true));

        // Another address, or another kind, never matches; each counts against the code.
        let e = id
            .confirm_email_change(&p, &user, "other@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
        let e = id
            .verify_code(&p, &user, CodeKind::Verify, &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
        assert_eq!(f.user(&p, &user).unwrap().email, "old@x.io");

        let out = id
            .confirm_email_change(&p, &user, "new@x.io", &code, None)
            .await
            .unwrap();
        assert_eq!((out.email.as_str(), out.email_verified), ("new@x.io", true));
        let notices = f.change_notices_sent();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            (notices[0].user_id.as_str(), notices[0].email.as_str()),
            (user.as_str(), "old@x.io")
        );
        // Single use.
        let e = id
            .confirm_email_change(&p, &user, "new@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");

        // Five wrong guesses lock the code; a changed current email kills it.
        id.create_email_change_code(&p, &user, "third@x.io", None)
            .await
            .unwrap();
        let (_, _, code) = last_sent(&f);
        let wrong = format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000);
        for _ in 0..5 {
            let e = id
                .confirm_email_change(&p, &user, "third@x.io", &wrong, None)
                .await
                .unwrap_err();
            assert!(is_invalid_code(&e), "{e:?}");
        }
        let e = id
            .confirm_email_change(&p, &user, "third@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
        id.create_email_change_code(&p, &user, "third@x.io", None)
            .await
            .unwrap();
        let (_, _, code) = last_sent(&f);
        id.set_email(&p, &user, "moved@x.io", true).await.unwrap();
        let e = id
            .confirm_email_change(&p, &user, "third@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
        assert_eq!(f.user(&p, &user).unwrap().email, "moved@x.io");

        // A verify code is not an email_change code.
        let v = id
            .create_code(
                &p,
                &crate::CodeRequest::new(&user, CodeKind::Verify, crate::Delivery::Return),
            )
            .await
            .unwrap();
        let e = id
            .confirm_email_change(&p, &user, "fourth@x.io", v.code.as_deref().unwrap(), None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
    }

    #[tokio::test]
    async fn mock_email_change_rechecks_a_taken_address() {
        let f = FakeIdentity::start().await;
        let (id, p) = (f.client(), pool());
        let user = f.with_user(&p, "old@x.io", "password-1");
        let other = f.with_user(&p, "taken@x.io", "password-1");

        // Taken, or the user's own: refused and nothing mailed. Return delivery: refused.
        for addr in ["taken@x.io", "old@x.io", "OLD@x.io"] {
            let e = id
                .create_email_change_code(&p, &user, addr, None)
                .await
                .unwrap_err();
            assert!(matches!(&e, IdentityError::Conflict(_)), "{addr}: {e:?}");
        }
        assert!(f.codes_sent().is_empty());
        let mut request =
            crate::CodeRequest::new(&user, CodeKind::EmailChange, crate::Delivery::Return);
        request.project_name = None;
        let e = id.create_code(&p, &request).await.unwrap_err();
        assert!(matches!(&e, IdentityError::Invalid { .. }), "{e:?}");

        // Free when issued, taken before the confirmation: 409, the email stays, the code is spent.
        id.create_email_change_code(&p, &user, "new@x.io", None)
            .await
            .unwrap();
        let (_, _, code) = last_sent(&f);
        id.set_email(&p, &other, "new@x.io", true).await.unwrap();
        let e = id
            .confirm_email_change(&p, &user, "new@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(matches!(&e, IdentityError::Conflict(_)), "{e:?}");
        assert_eq!(f.user(&p, &user).unwrap().email, "old@x.io");
        id.set_email(&p, &other, "taken@x.io", true).await.unwrap();
        let e = id
            .confirm_email_change(&p, &user, "new@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
        assert!(f.change_notices_sent().is_empty());
    }

    #[tokio::test]
    async fn mock_email_change_code_dies_after_an_email_round_trip() {
        let f = FakeIdentity::start().await;
        let (id, p) = (f.client(), pool());
        let user = f.with_user(&p, "old@x.io", "password-1");

        // Through set_email: A -> C -> A revokes the pending code.
        id.create_email_change_code(&p, &user, "new@x.io", None)
            .await
            .unwrap();
        let (_, _, code) = last_sent(&f);
        id.set_email(&p, &user, "away@x.io", true).await.unwrap();
        id.set_email(&p, &user, "old@x.io", true).await.unwrap();
        let e = id
            .confirm_email_change(&p, &user, "new@x.io", &code, None)
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");
        assert_eq!(f.user(&p, &user).unwrap().email, "old@x.io");

        // Through the swap: A -> B -> A by two confirmations revokes a code bound to A.
        let reset = id
            .create_code(
                &p,
                &crate::CodeRequest::new(&user, CodeKind::Reset, crate::Delivery::Return),
            )
            .await
            .unwrap();
        for addr in ["new@x.io", "old@x.io"] {
            id.create_email_change_code(&p, &user, addr, None)
                .await
                .unwrap();
            let (_, _, code) = last_sent(&f);
            id.confirm_email_change(&p, &user, addr, &code, None)
                .await
                .unwrap();
        }
        assert_eq!(f.user(&p, &user).unwrap().email, "old@x.io");
        let e = id
            .verify_code(
                &p,
                &user,
                CodeKind::Reset,
                reset.code.as_deref().unwrap(),
                Some("password-2-long"),
            )
            .await
            .unwrap_err();
        assert!(is_invalid_code(&e), "{e:?}");

        // Setting the same address again is no change and revokes nothing.
        let v = id
            .create_code(
                &p,
                &crate::CodeRequest::new(&user, CodeKind::Verify, crate::Delivery::Return),
            )
            .await
            .unwrap();
        id.set_email(&p, &user, "OLD@x.io", false).await.unwrap();
        id.verify_code(
            &p,
            &user,
            CodeKind::Verify,
            v.code.as_deref().unwrap(),
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn mock_set_email_notifying_records_the_notice() {
        let f = FakeIdentity::start().await;
        let (id, p) = (f.client(), pool());
        let user = f.with_user(&p, "old@x.io", "password-1");

        id.set_email(&p, &user, "quiet@x.io", true).await.unwrap();
        assert!(
            f.change_notices_sent().is_empty(),
            "set_email mails nothing"
        );

        let out = id
            .set_email_notifying(&p, &user, "loud@x.io", true, None)
            .await
            .unwrap();
        assert_eq!(
            (out.email.as_str(), out.email_verified),
            ("loud@x.io", true)
        );
        let notices = f.change_notices_sent();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].email, "quiet@x.io");

        // The same address is no change: nothing more is mailed. Without SMTP, nothing either.
        id.set_email_notifying(&p, &user, "loud@x.io", true, None)
            .await
            .unwrap();
        f.set_email_configured(false);
        id.set_email_notifying(&p, &user, "silent@x.io", true, None)
            .await
            .unwrap();
        assert_eq!(f.change_notices_sent().len(), 1);
        assert_eq!(f.user(&p, &user).unwrap().email, "silent@x.io");
    }
}
