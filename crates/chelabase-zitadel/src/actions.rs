//! Zitadel Actions v2 webhooks: signature check and event shapes.

use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

pub const SIGNATURE_HEADER: &str = "zitadel-signature";
pub const TOLERANCE_SECS: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub enum SignatureError {
    Malformed,
    Expired,
    Mismatch,
}

pub fn verify_signature(
    header: &str,
    body: &[u8],
    signing_key: &str,
    now_unix: i64,
) -> Result<(), SignatureError> {
    let (mut t, mut sigs) = (None, Vec::new());
    for part in header.split(',') {
        let (k, v) = part
            .trim()
            .split_once('=')
            .ok_or(SignatureError::Malformed)?;
        match k {
            "t" => t = Some(v.parse::<i64>().map_err(|_| SignatureError::Malformed)?),
            "v1" => sigs.push(v),
            _ => {}
        }
    }
    let t = t.ok_or(SignatureError::Malformed)?;
    if sigs.is_empty() {
        return Err(SignatureError::Malformed);
    }
    // Zitadel only refuses old timestamps.
    if now_unix - t > TOLERANCE_SECS {
        return Err(SignatureError::Expired);
    }
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(signing_key.as_bytes()).expect("any key length");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    for s in sigs {
        if let Ok(raw) = hex::decode(s)
            && raw.len() == expected.len()
            && bool::from(raw.as_slice().ct_eq(expected.as_slice()))
        {
            return Ok(());
        }
    }
    Err(SignatureError::Mismatch)
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionEvent {
    #[serde(default, rename = "aggregateID")]
    pub aggregate_id: String,
    #[serde(default, rename = "aggregateType")]
    pub aggregate_type: String,
    #[serde(default, rename = "resourceOwner")]
    pub resource_owner: String,
    #[serde(default, rename = "event_type")]
    pub event_type: String,
    #[serde(default, rename = "created_at")]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum UserEventKind {
    Upsert,
    Gone,
    Deactivated,
    PasswordChanged,
}

pub fn user_event_kind(event_type: &str) -> Option<UserEventKind> {
    Some(match event_type {
        "user.human.added"
        | "user.human.selfregistered"
        | "user.human.email.changed"
        | "user.human.email.verified"
        | "user.reactivated" => UserEventKind::Upsert,
        "user.removed" => UserEventKind::Gone,
        "user.deactivated" | "user.locked" => UserEventKind::Deactivated,
        "user.human.password.changed" => UserEventKind::PasswordChanged,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "chelabase-test-key";
    const BODY: &[u8] = br#"{"event_type":"user.removed"}"#;
    // printf '%s' '1700000000.{"event_type":"user.removed"}' | openssl dgst -sha256 -hmac chelabase-test-key
    const SIG: &str = "3536bca49c8c5fcdc309d729f90b7cc8332c2dbb0a59a2072777ff1c0429580a";

    fn header(t: i64, sig: &str) -> String {
        format!("t={t},v1={sig}")
    }

    #[test]
    fn signature_matches_the_fixed_vector() {
        assert_eq!(
            verify_signature(&header(1700000000, SIG), BODY, KEY, 1700000010),
            Ok(())
        );
        assert_eq!(
            verify_signature(&header(1700000000, SIG), b"{}", KEY, 1700000010),
            Err(SignatureError::Mismatch)
        );
        assert_eq!(
            verify_signature(&header(1700000000, SIG), BODY, "other", 1700000010),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn old_timestamp_is_refused() {
        let h = header(1700000000, SIG);
        assert_eq!(verify_signature(&h, BODY, KEY, 1700000300), Ok(()));
        assert_eq!(
            verify_signature(&h, BODY, KEY, 1700000301),
            Err(SignatureError::Expired)
        );
    }

    #[test]
    fn any_v1_may_match() {
        let h = format!("t=1700000000,v1={},v1={SIG}, v0=ignored", "00".repeat(32));
        assert_eq!(verify_signature(&h, BODY, KEY, 1700000000), Ok(()));
    }

    #[test]
    fn malformed_headers() {
        let m = Err(SignatureError::Malformed);
        assert_eq!(verify_signature(&format!("v1={SIG}"), BODY, KEY, 1), m);
        assert_eq!(verify_signature("t=1700000000", BODY, KEY, 1700000000), m);
        assert_eq!(verify_signature("", BODY, KEY, 1), m);
        assert_eq!(verify_signature(&format!("t=x,v1={SIG}"), BODY, KEY, 1), m);
        // bad hex is a mismatch, not a panic
        assert_eq!(
            verify_signature("t=1700000000,v1=zz", BODY, KEY, 1700000000),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn event_parses_zitadel_body() {
        let body = r#"{"aggregateID":"3001","aggregateType":"user","resourceOwner":"o1",
            "instanceID":"i1","version":"v1","sequence":7,"event_type":"user.human.added",
            "created_at":"2026-10-03T10:00:00.5Z","userID":"3001",
            "event_payload":{"userName":"ada"}}"#;
        let e: ActionEvent = serde_json::from_str(body).unwrap();
        assert_eq!(e.aggregate_id, "3001");
        assert_eq!(e.aggregate_type, "user");
        assert_eq!(e.resource_owner, "o1");
        assert_eq!(e.event_type, "user.human.added");
        assert_eq!(e.created_at.unwrap().timestamp(), 1_791_021_600);
        let empty: ActionEvent = serde_json::from_str("{}").unwrap();
        assert!(empty.aggregate_id.is_empty() && empty.created_at.is_none());
    }

    #[test]
    fn event_kinds_map() {
        use UserEventKind::*;
        for t in [
            "user.human.added",
            "user.human.selfregistered",
            "user.human.email.changed",
            "user.human.email.verified",
            "user.reactivated",
        ] {
            assert_eq!(user_event_kind(t), Some(Upsert), "{t}");
        }
        assert_eq!(user_event_kind("user.removed"), Some(Gone));
        assert_eq!(user_event_kind("user.deactivated"), Some(Deactivated));
        assert_eq!(user_event_kind("user.locked"), Some(Deactivated));
        assert_eq!(
            user_event_kind("user.human.password.changed"),
            Some(PasswordChanged)
        );
        assert_eq!(user_event_kind("user.human.phone.changed"), None);
        assert_eq!(user_event_kind(""), None);
    }
}
