//! The `Chelabase-Signature` on every request proxied to an external service: the storage
//! webhooks' scheme (`t=<unix seconds>,v1=<hex HMAC-SHA256 of "<t>.<signed string>">`, keyed
//! with the service's `whsec_…` secret as UTF-8), over this signed string:
//!
//! ```text
//! <chelabase-request-id>
//! <METHOD> <request-target>
//! <chelabase-branch>
//! <chelabase-role>
//! <chelabase-user-id>
//! <Content-Digest>          (only when the body is signed)
//! ```
//!
//! `request-target` is the exact path and query the service receives, its base URL's path
//! prefix included. A body of at most [`MAX_SIGNED_BODY`] is buffered and signed through its
//! `Content-Digest: sha-256=:<base64>:` (RFC 9530); a longer one streams and the request says
//! `chelabase-body: unsigned`. No part may hold CR or LF.
//!
//! Because the bytes after `"<t>."` are the signed string, the webhooks' verifier checks it
//! too: `verify(secret, header, signed_string, now, 300)`. On its own that doesn't stop a
//! replay inside the window; [`Verifier`] is the documented receiver: it checks the
//! timestamp, the digest against the body, the MAC, and refuses a request id it has seen in
//! the window.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::http::HeaderMap;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const SIGNATURE: &str = "chelabase-signature";
pub const REQUEST_ID: &str = "chelabase-request-id";
pub const BRANCH: &str = "chelabase-branch";
pub const ROLE: &str = "chelabase-role";
/// The app user's subject (else empty). Not `chelabase-user`: that is the token at the gateway.
pub const USER: &str = "chelabase-user-id";
/// `unsigned` when the body streamed and isn't covered by the signature.
pub const BODY: &str = "chelabase-body";
pub const DIGEST: &str = "content-digest";
/// A body up to this size is buffered and signed.
pub const MAX_SIGNED_BODY: usize = 1024 * 1024;
/// A signature older or newer than this (seconds) is refused.
pub const TOLERANCE: i64 = 300;

type HmacSha256 = Hmac<Sha256>;

/// What a request's signature covers.
#[derive(Debug, Clone, Copy)]
pub struct Signed<'a> {
    pub request_id: &'a str,
    pub method: &'a str,
    pub target: &'a str,
    pub branch: &'a str,
    pub role: &'a str,
    pub user: &'a str,
    /// The `Content-Digest` value, when the body is signed.
    pub digest: Option<&'a str>,
}

/// A part of the signed string holds CR or LF: it could forge another line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineBreak;

impl Signed<'_> {
    /// The signed string.
    pub fn string(&self) -> Result<String, LineBreak> {
        let mut parts = vec![
            self.request_id.to_owned(),
            format!("{} {}", self.method, self.target),
            self.branch.to_owned(),
            self.role.to_owned(),
            self.user.to_owned(),
        ];
        parts.extend(self.digest.map(str::to_owned));
        if parts.iter().any(|p| p.contains(['\r', '\n'])) {
            return Err(LineBreak);
        }
        Ok(parts.join("\n"))
    }
}

/// `sha-256=:<base64>:` of `body`.
pub fn content_digest(body: &[u8]) -> String {
    format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(body)))
}

fn mac(secret: &str, t: i64, signed: &str) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes any key size");
    mac.update(format!("{t}.").as_bytes());
    mac.update(signed.as_bytes());
    mac
}

/// The `Chelabase-Signature` value for `signed` at `t` (unix seconds).
pub fn sign(secret: &str, t: i64, signed: &Signed<'_>) -> Result<String, LineBreak> {
    let tag = mac(secret, t, &signed.string()?).finalize().into_bytes();
    Ok(format!("t={t},v1={}", hex::encode(tag)))
}

/// Why [`Verifier`] refused a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejected {
    /// A header is missing, or the signature has no `t=` or `v1=`.
    Malformed,
    /// Outside [`TOLERANCE`].
    Expired,
    /// `Content-Digest` is missing or doesn't match the body.
    Digest,
    /// No `v1` matches.
    Mismatch,
    /// This request id was already accepted within the window.
    Replayed,
}

/// What a service does with each request: [`Verifier::verify`]. It remembers each request id
/// it accepted until its signature's `t + TOLERANCE`: as long as the timestamp check would
/// pass, whatever the skew between the proxy's clock and the receiver's.
pub struct Verifier {
    secret: Zeroizing<String>,
    /// Request id → when it may be forgotten (`t + TOLERANCE`).
    seen: Mutex<HashMap<String, i64>>,
}

impl Verifier {
    pub fn new(secret: &str) -> Self {
        Self {
            secret: Zeroizing::new(secret.to_owned()),
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Checks a request as received: its method, its request-target (path and query, as on
    /// the request line), its headers and its body (`body` is what arrived).
    pub fn verify(
        &self,
        method: &str,
        target: &str,
        headers: &HeaderMap,
        body: &[u8],
        now: i64,
    ) -> Result<(), Rejected> {
        let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let field = |name: &str| get(name).ok_or(Rejected::Malformed);
        let (mut t, mut tags) = (None, Vec::new());
        for part in field(SIGNATURE)?.split(',') {
            match part.trim().split_once('=') {
                Some(("t", v)) => t = v.parse::<i64>().ok(),
                Some(("v1", v)) => tags.extend(hex::decode(v).ok()),
                _ => {}
            }
        }
        let (Some(t), false) = (t, tags.is_empty()) else {
            return Err(Rejected::Malformed);
        };
        if (now - t).abs() > TOLERANCE {
            return Err(Rejected::Expired);
        }
        let digest = match get(BODY) {
            Some("unsigned") => None,
            _ => {
                let d = get(DIGEST).ok_or(Rejected::Digest)?;
                if d != content_digest(body) {
                    return Err(Rejected::Digest);
                }
                Some(d)
            }
        };
        let request_id = field(REQUEST_ID)?;
        let signed = Signed {
            request_id,
            method,
            target,
            branch: field(BRANCH)?,
            role: field(ROLE)?,
            user: field(USER)?,
            digest,
        }
        .string()
        .map_err(|_| Rejected::Malformed)?;
        let expected = mac(&self.secret, t, &signed);
        if !tags
            .iter()
            .any(|tag| expected.clone().verify_slice(tag).is_ok())
        {
            return Err(Rejected::Mismatch);
        }
        let mut seen = self.seen.lock().expect("not poisoned");
        seen.retain(|_, until| now <= *until);
        if seen.contains_key(request_id) {
            return Err(Rejected::Replayed);
        }
        seen.insert(request_id.to_owned(), t + TOLERANCE);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const SECRET: &str = "whsec_0123456789abcdef";

    fn request(body: &[u8], id: &str, t: i64) -> HeaderMap {
        let digest = content_digest(body);
        let signed = Signed {
            request_id: id,
            method: "POST",
            target: "/api/a?x=1",
            branch: "b-1",
            role: "anon",
            user: "",
            digest: Some(&digest),
        };
        let mut h = HeaderMap::new();
        for (k, v) in [
            (SIGNATURE, sign(SECRET, t, &signed).unwrap()),
            (REQUEST_ID, id.to_owned()),
            (BRANCH, "b-1".into()),
            (ROLE, "anon".into()),
            (USER, String::new()),
            (DIGEST, digest.clone()),
        ] {
            h.insert(k, HeaderValue::from_str(&v).unwrap());
        }
        h
    }

    #[test]
    fn a_signed_request_verifies_once_and_tampering_fails() {
        let v = Verifier::new(SECRET);
        let t = 1_700_000_000;
        let h = request(b"payload", "r-1", t);
        assert_eq!(
            v.verify("POST", "/api/a?x=1", &h, b"payload", t + 10),
            Ok(())
        );
        // The same request again: its id was seen.
        assert_eq!(
            v.verify("POST", "/api/a?x=1", &h, b"payload", t + 11),
            Err(Rejected::Replayed)
        );
        // Replayed with another body, a fresh verifier: the digest doesn't match.
        let fresh = Verifier::new(SECRET);
        assert_eq!(
            fresh.verify("POST", "/api/a?x=1", &h, b"other", t),
            Err(Rejected::Digest)
        );
        // ... nor with the digest rewritten to match: the signature covers it.
        let mut forged = h.clone();
        forged.insert(
            DIGEST,
            HeaderValue::from_str(&content_digest(b"other")).unwrap(),
        );
        assert_eq!(
            fresh.verify("POST", "/api/a?x=1", &forged, b"other", t),
            Err(Rejected::Mismatch)
        );
        // ... nor marked unsigned.
        let mut unsigned = h.clone();
        unsigned.remove(DIGEST);
        unsigned.insert(BODY, HeaderValue::from_static("unsigned"));
        assert_eq!(
            fresh.verify("POST", "/api/a?x=1", &unsigned, b"other", t),
            Err(Rejected::Mismatch)
        );
        for (name, value) in [
            (BRANCH, "b-2"),
            (ROLE, "service_role"),
            (USER, "admin"),
            (REQUEST_ID, "r-2"),
        ] {
            let mut tampered = h.clone();
            tampered.insert(name, HeaderValue::from_static(value));
            assert_eq!(
                fresh.verify("POST", "/api/a?x=1", &tampered, b"payload", t),
                Err(Rejected::Mismatch),
                "{name}"
            );
        }
        assert_eq!(
            fresh.verify("GET", "/api/a?x=1", &h, b"payload", t),
            Err(Rejected::Mismatch)
        );
        assert_eq!(
            fresh.verify("POST", "/api/b?x=1", &h, b"payload", t),
            Err(Rejected::Mismatch)
        );
        assert_eq!(
            fresh.verify("POST", "/api/a?x=1", &h, b"payload", t + 301),
            Err(Rejected::Expired)
        );
        assert_eq!(
            Verifier::new("whsec_other").verify("POST", "/api/a?x=1", &h, b"payload", t),
            Err(Rejected::Mismatch)
        );
        // Outside the window a remembered id is forgotten, and the timestamp refuses it anyway.
        let h2 = request(b"", "r-1", t + 400);
        assert_eq!(v.verify("POST", "/api/a?x=1", &h2, b"", t + 400), Ok(()));
    }

    /// Audit B1: an id is kept until its signature's own `t + TOLERANCE`, so a receiver whose
    /// clock is behind the proxy's still refuses a replay while the timestamp passes.
    #[test]
    fn a_replay_is_refused_for_as_long_as_the_timestamp_passes() {
        let v = Verifier::new(SECRET);
        let t = 1_700_000_000;
        let h = request(b"", "r-1", t);
        assert_eq!(v.verify("POST", "/api/a?x=1", &h, b"", t - 100), Ok(()));
        assert_eq!(
            v.verify("POST", "/api/a?x=1", &h, b"", t + 250),
            Err(Rejected::Replayed)
        );
        assert_eq!(
            v.verify("POST", "/api/a?x=1", &h, b"", t + TOLERANCE),
            Err(Rejected::Replayed)
        );
        assert_eq!(
            v.verify("POST", "/api/a?x=1", &h, b"", t + TOLERANCE + 1),
            Err(Rejected::Expired)
        );
    }

    /// The webhooks' verifier accepts the same header over the signed string.
    #[test]
    fn the_scheme_is_the_webhooks_one() {
        let signed = Signed {
            request_id: "r",
            method: "GET",
            target: "/",
            branch: "b",
            role: "anon",
            user: "u",
            digest: None,
        };
        let header = sign(SECRET, 42, &signed).unwrap();
        let text = signed.string().unwrap();
        assert_eq!(text, "r\nGET /\nb\nanon\nu");
        let mut m = HmacSha256::new_from_slice(SECRET.as_bytes()).unwrap();
        m.update(format!("42.{text}").as_bytes());
        assert_eq!(
            header,
            format!("t=42,v1={}", hex::encode(m.finalize().into_bytes()))
        );
    }

    #[test]
    fn line_breaks_are_refused() {
        for bad in ["a\nb", "a\rb", "a\r\n"] {
            let signed = Signed {
                request_id: "r",
                method: "GET",
                target: "/",
                branch: "b",
                role: bad,
                user: "u",
                digest: None,
            };
            assert_eq!(sign(SECRET, 1, &signed), Err(LineBreak), "{bad:?}");
            let target = Signed {
                role: "anon",
                target: bad,
                ..signed
            };
            assert_eq!(target.string(), Err(LineBreak));
        }
    }
}
