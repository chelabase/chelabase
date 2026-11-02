//! Signed object URLs' tokens: `v1.{kid}.{payload}.{mac}`, the payload the base64url JSON of
//! [`Claims`] and the MAC its HMAC-SHA256 under the URL key ([`UrlKey`], the orchestrator's
//! `storage_url` derivation of the secrets key, generation `kid`). Verified in constant time
//! before anything in it is read; a token opens only its own purpose, bucket and name, until
//! its expiry.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

/// What a token may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// `GET /storage/v1/object/sign/…`: one version of one object.
    Download,
    /// `PUT /storage/v1/object/upload/sign/…`: one upload, once.
    Upload,
}

/// Everything a token is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claims {
    #[serde(rename = "p")]
    pub purpose: Purpose,
    /// The Chelabase database (its blobs' prefix).
    #[serde(rename = "d")]
    pub database_id: Uuid,
    #[serde(rename = "br")]
    pub branch_id: Uuid,
    /// The Postgres database: its name (to reach it) and OID (it must still be the same one).
    #[serde(rename = "pg")]
    pub database: String,
    #[serde(rename = "oid", default, skip_serializing_if = "Option::is_none")]
    pub datoid: Option<u32>,
    /// The data key that minted it: the URL dies with it.
    #[serde(rename = "k")]
    pub key_id: Uuid,
    #[serde(rename = "b")]
    pub bucket: String,
    #[serde(rename = "n")]
    pub name: String,
    /// A download's version (an overwrite doesn't change what the URL serves) and what it
    /// is served as.
    #[serde(rename = "v", default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<Uuid>,
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(rename = "e", default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(rename = "ct", default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Unique per token (an upload's one-time nonce).
    #[serde(rename = "t")]
    pub token_id: Uuid,
    /// Expiry, seconds since the Unix epoch.
    pub exp: u64,
}

/// The URL-signing key and its generation.
#[derive(Clone)]
pub struct UrlKey {
    pub kid: u32,
    pub key: [u8; 32],
}

impl std::fmt::Debug for UrlKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "UrlKey({}, ..)", self.kid)
    }
}

/// Why a token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    /// Not a token, another key's (or generation's), or changed: 401.
    Invalid,
    /// Past its expiry: 401.
    Expired,
    /// A genuine token for another purpose or object: 403.
    Mismatch,
}

/// `claims` signed with `key`.
pub fn sign(key: &UrlKey, claims: &Claims) -> String {
    let kid = key.kid.to_string();
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims serialize"));
    let mac = URL_SAFE_NO_PAD.encode(mac_of(key, &kid, &payload).finalize().into_bytes());
    format!("v1.{kid}.{payload}.{mac}")
}

/// The claims of `token` when `key` signed it, for `purpose` and `bucket`/`name`, and it is
/// unexpired at `now` (seconds since the epoch). The MAC is checked (in constant time) before
/// the payload is decoded.
pub fn verify(
    key: &UrlKey,
    token: &str,
    purpose: Purpose,
    bucket: &str,
    name: &str,
    now: u64,
) -> Result<Claims, TokenError> {
    let mut parts = token.split('.');
    let (Some("v1"), Some(kid), Some(payload), Some(mac), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(TokenError::Invalid);
    };
    if kid != key.kid.to_string() {
        return Err(TokenError::Invalid);
    }
    let mac = URL_SAFE_NO_PAD
        .decode(mac)
        .map_err(|_| TokenError::Invalid)?;
    mac_of(key, kid, payload)
        .verify_slice(&mac)
        .map_err(|_| TokenError::Invalid)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| TokenError::Invalid)?;
    let claims: Claims = serde_json::from_slice(&bytes).map_err(|_| TokenError::Invalid)?;
    if now >= claims.exp {
        return Err(TokenError::Expired);
    }
    if claims.purpose != purpose || claims.bucket != bucket || claims.name != name {
        return Err(TokenError::Mismatch);
    }
    Ok(claims)
}

fn mac_of(key: &UrlKey, kid: &str, payload: &str) -> Hmac<Sha256> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key.key).expect("any key size");
    mac.update(b"chelabase-storage-url.v1.");
    mac.update(kid.as_bytes());
    mac.update(b".");
    mac.update(payload.as_bytes());
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(kid: u32, byte: u8) -> UrlKey {
        UrlKey {
            kid,
            key: [byte; 32],
        }
    }

    fn claims() -> Claims {
        Claims {
            purpose: Purpose::Download,
            database_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            database: "app".into(),
            datoid: Some(16384),
            key_id: Uuid::new_v4(),
            bucket: "avatars".into(),
            name: "u1/a b.png".into(),
            version_id: Some(Uuid::new_v4()),
            size: Some(10),
            etag: Some("e1".into()),
            content_type: Some("image/png".into()),
            token_id: Uuid::new_v4(),
            exp: 2_000,
        }
    }

    fn check(token: &str, c: &Claims) -> Result<Claims, TokenError> {
        verify(&key(1, 7), token, c.purpose, &c.bucket, &c.name, 1_000)
    }

    /// A token round-trips its claims, for its own purpose and object only.
    #[test]
    fn a_token_round_trips() {
        let c = claims();
        let token = sign(&key(1, 7), &c);
        assert!(token.starts_with("v1.1."), "{token}");
        assert_eq!(check(&token, &c), Ok(c.clone()));
        let upload = Claims {
            purpose: Purpose::Upload,
            version_id: None,
            datoid: None,
            size: None,
            etag: None,
            content_type: None,
            ..c
        };
        let token = sign(&key(1, 7), &upload);
        assert_eq!(check(&token, &upload), Ok(upload));
    }

    /// Changing any claim (re-encoded under the old MAC), the MAC, the kid, or signing with
    /// another key is refused as invalid.
    #[test]
    fn a_tampered_token_is_invalid() {
        let c = claims();
        let token = sign(&key(1, 7), &c);
        let parts: Vec<&str> = token.split('.').collect();
        let mac = parts[3];
        let mut tampered: Vec<Claims> = Vec::new();
        let mut each = |f: &dyn Fn(&mut Claims)| {
            let mut t = c.clone();
            f(&mut t);
            assert_ne!(t, c);
            tampered.push(t);
        };
        each(&|t| t.purpose = Purpose::Upload);
        each(&|t| t.database_id = Uuid::new_v4());
        each(&|t| t.branch_id = Uuid::new_v4());
        each(&|t| t.database = "other".into());
        each(&|t| t.datoid = Some(1));
        each(&|t| t.key_id = Uuid::new_v4());
        each(&|t| t.bucket = "other".into());
        each(&|t| t.name = "u1/b.png".into());
        each(&|t| t.version_id = Some(Uuid::new_v4()));
        each(&|t| t.size = Some(11));
        each(&|t| t.etag = Some("e2".into()));
        each(&|t| t.content_type = Some("text/html".into()));
        each(&|t| t.token_id = Uuid::new_v4());
        each(&|t| t.exp = 9_999);
        for t in &tampered {
            let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(t).unwrap());
            let forged = format!("v1.1.{payload}.{mac}");
            let got = verify(&key(1, 7), &forged, t.purpose, &t.bucket, &t.name, 1_000);
            assert_eq!(got, Err(TokenError::Invalid), "{t:?}");
        }
        let mut flipped = mac.as_bytes().to_vec();
        flipped[0] = if flipped[0] == b'A' { b'B' } else { b'A' };
        let bad_mac = format!(
            "v1.1.{}.{}",
            parts[2],
            std::str::from_utf8(&flipped).unwrap()
        );
        let other_kid = format!("v1.2.{}.{mac}", parts[2]);
        let other_key = sign(&key(1, 8), &c);
        let other_generation = sign(&key(2, 7), &c);
        for t in [
            bad_mac.as_str(),
            other_kid.as_str(),
            other_key.as_str(),
            other_generation.as_str(),
            "",
            "v1",
            "v1.1.x.y",
            "v2.1.a.b",
            &format!("{token}."),
        ] {
            assert_eq!(check(t, &c), Err(TokenError::Invalid), "{t}");
        }
    }

    /// Expired at `exp` (not after), whatever else is right.
    #[test]
    fn an_expired_token_is_refused() {
        let c = claims();
        let token = sign(&key(1, 7), &c);
        let at = |now| verify(&key(1, 7), &token, c.purpose, &c.bucket, &c.name, now);
        assert!(at(1_999).is_ok());
        assert_eq!(at(2_000), Err(TokenError::Expired));
        assert_eq!(at(5_000), Err(TokenError::Expired));
    }

    /// A genuine token for another purpose, bucket or name is a mismatch.
    #[test]
    fn a_token_opens_only_its_purpose_and_object() {
        let c = claims();
        let token = sign(&key(1, 7), &c);
        let k = key(1, 7);
        for (purpose, bucket, name) in [
            (Purpose::Upload, "avatars", "u1/a b.png"),
            (Purpose::Download, "other", "u1/a b.png"),
            (Purpose::Download, "avatars", "u1/a b.PNG"),
            (Purpose::Download, "avatars", "u1/a%20b.png"),
        ] {
            assert_eq!(
                verify(&k, &token, purpose, bucket, name, 1_000),
                Err(TokenError::Mismatch),
                "{purpose:?} {bucket} {name}"
            );
        }
    }
}
