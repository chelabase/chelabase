//! RT1 T7: tickets (spec §1, R9, P5, W15): sealing with AAD = the ticket hash, single use
//! through `GETDEL`, the 30 s TTL, and a value copied under another ticket's key refused.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use chelabase_auth::internal::{AuthMethod, InternalClaims};
use realtime::store::{MemoryStore, TicketStore};
use realtime::ticket::{
    TICKET_TTL, TicketError, TicketIdentity, TicketKey, TicketKind, Tickets, new_ticket, store_key,
    ticket_hash,
};
use sha2::Digest;

fn ana() -> TicketIdentity {
    TicketIdentity {
        sub: "user-ana-1234".into(),
        org_id: "org-77".into(),
        auth: AuthMethod::ManagementJwt,
        email: Some("ana@example.com".into()),
        name: Some("Ana".into()),
        data: None,
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn a_ticket_is_32_random_bytes_in_base64url() {
    let t = new_ticket();
    assert_eq!(t.len(), 43, "{t}");
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&t)
        .expect("base64url without padding");
    assert_eq!(bytes.len(), 32);
    assert_ne!(t, new_ticket());
}

#[test]
fn the_hash_and_the_key() {
    let t = "abc";
    assert_eq!(ticket_hash(t), hex::encode(sha2::Sha256::digest(b"abc")));
    assert_eq!(
        store_key(&ticket_hash(t)),
        format!("rt:ticket:{}", ticket_hash(t))
    );
    assert_eq!(TICKET_TTL, Duration::from_secs(30));
}

#[test]
fn the_key_is_exactly_32_bytes() {
    assert!(TicketKey::from_bytes(&[7u8; 32]).is_ok());
    assert_eq!(
        TicketKey::from_bytes(&[7u8; 31]).err(),
        Some(TicketError::BadKey)
    );
    assert_eq!(
        TicketKey::from_bytes(&[7u8; 33]).err(),
        Some(TicketError::BadKey)
    );
    assert_eq!(TicketKey::from_bytes(&[]).err(), Some(TicketError::BadKey));
}

#[test]
fn the_key_file_is_raw_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("ticket.key");
    // Raw: a newline byte inside or at the end is key material, not a line break.
    let mut raw = [3u8; 32];
    raw[31] = b'\n';
    std::fs::write(&good, raw).unwrap();
    let key = TicketKey::from_file(&good).expect("32 raw bytes");
    let same = TicketKey::from_bytes(&raw).unwrap();
    let h = ticket_hash("t");
    assert_eq!(
        same.unseal(&h, &key.seal(&h, &ana())),
        Ok(ana()),
        "the file's bytes are the key"
    );
    let short = dir.path().join("short.key");
    std::fs::write(&short, [3u8; 16]).unwrap();
    assert_eq!(
        TicketKey::from_file(&short).err(),
        Some(TicketError::BadKey)
    );
    assert!(TicketKey::from_file(&dir.path().join("missing")).is_err());
}

#[test]
fn seal_and_unseal_round_trip_and_hide_the_identity() {
    let key = TicketKey::generate();
    let h = ticket_hash(&new_ticket());
    let sealed = key.seal(&h, &ana());
    assert_eq!(key.unseal(&h, &sealed), Ok(ana()));
    assert!(
        !contains(&sealed, b"user-ana-1234"),
        "the sub isn't stored in clear"
    );
    assert!(
        !contains(&sealed, b"ana@example.com"),
        "the email isn't stored in clear"
    );
    assert_ne!(sealed, key.seal(&h, &ana()), "a fresh nonce per seal");
}

#[test]
fn a_sealed_value_opens_only_under_its_own_hash_and_key() {
    let key = TicketKey::generate();
    let (h1, h2) = (ticket_hash(&new_ticket()), ticket_hash(&new_ticket()));
    let sealed = key.seal(&h1, &ana());
    assert_eq!(
        key.unseal(&h2, &sealed),
        Err(TicketError::Forged),
        "AAD = the ticket hash"
    );
    assert_eq!(
        TicketKey::generate().unseal(&h1, &sealed),
        Err(TicketError::Forged),
        "another key"
    );
    for i in [0, sealed.len() / 2, sealed.len() - 1] {
        let mut bad = sealed.clone();
        bad[i] ^= 1;
        assert_eq!(
            key.unseal(&h1, &bad),
            Err(TicketError::Forged),
            "byte {i} flipped"
        );
    }
    assert_eq!(
        key.unseal(&h1, &sealed[..4]),
        Err(TicketError::Forged),
        "truncated"
    );
    assert_eq!(key.unseal(&h1, b""), Err(TicketError::Forged), "empty");
}

fn tickets() -> (Tickets, Arc<MemoryStore>) {
    let store = MemoryStore::new();
    (Tickets::new(TicketKey::generate(), store.clone()), store)
}

#[tokio::test]
async fn issue_stores_the_sealed_identity_for_30_seconds() {
    let (t, store) = tickets();
    let ticket = t.issue(&ana()).await.unwrap();
    let key = store_key(&ticket_hash(&ticket));
    assert_eq!(
        store.keys(),
        vec![key.clone()],
        "one key, named by the hash"
    );
    let ttl = store.ttl_of(&key).expect("a TTL");
    assert!(
        ttl <= TICKET_TTL && ttl > TICKET_TTL - Duration::from_secs(2),
        "{ttl:?}"
    );
    let raw = store.get_raw(&key).unwrap();
    assert!(!contains(&raw, b"user-ana-1234"));
    assert!(
        !contains(&raw, ticket.as_bytes()),
        "the ticket itself isn't stored"
    );
}

#[tokio::test]
async fn a_ticket_redeems_once() {
    let (t, store) = tickets();
    let ticket = t.issue(&ana()).await.unwrap();
    assert_eq!(t.redeem(&ticket).await, Ok(ana()));
    assert_eq!(
        t.redeem(&ticket).await,
        Err(TicketError::Unknown),
        "a replay finds nothing (GETDEL)"
    );
    assert!(store.keys().is_empty());
}

#[tokio::test]
async fn a_copied_value_under_another_ticket_is_refused() {
    let (t, store) = tickets();
    let victim = t.issue(&ana()).await.unwrap();
    let mut mallory = ana();
    mallory.sub = "user-mallory".into();
    let own = t.issue(&mallory).await.unwrap();
    // W15: someone with Valkey write access copies the victim's value under their own ticket.
    let value = store.get_raw(&store_key(&ticket_hash(&victim))).unwrap();
    store.set_raw(&store_key(&ticket_hash(&own)), value);
    assert_eq!(t.redeem(&own).await, Err(TicketError::Forged));
    assert_eq!(
        t.redeem(&victim).await,
        Ok(ana()),
        "the victim's own ticket still works"
    );
}

#[tokio::test]
async fn a_value_planted_under_an_unissued_ticket_is_refused() {
    let (t, store) = tickets();
    let victim = t.issue(&ana()).await.unwrap();
    let value = store.get_raw(&store_key(&ticket_hash(&victim))).unwrap();
    let planted = new_ticket();
    store.set_raw(&store_key(&ticket_hash(&planted)), value);
    assert_eq!(t.redeem(&planted).await, Err(TicketError::Forged));
}

#[tokio::test(start_paused = true)]
async fn a_ticket_expires_after_30_seconds() {
    let (t, _store) = tickets();
    let early = t.issue(&ana()).await.unwrap();
    let late = t.issue(&ana()).await.unwrap();
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(t.redeem(&early).await, Ok(ana()), "29 s: still valid");
    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(
        t.redeem(&late).await,
        Err(TicketError::Unknown),
        "31 s: expired"
    );
}

#[tokio::test]
async fn strings_that_are_not_tickets_are_unknown_without_valkey() {
    let (t, store) = tickets();
    store.fail(true);
    for bad in [
        "",
        "x",
        "not a ticket",
        &"A".repeat(44),
        &"+".repeat(43),
        "../../rt:ticket:x",
    ] {
        assert_eq!(t.redeem(bad).await, Err(TicketError::Unknown), "{bad:?}");
    }
}

#[tokio::test]
async fn valkey_down_is_a_store_error() {
    let (t, store) = tickets();
    let ticket = t.issue(&ana()).await.unwrap();
    store.fail(true);
    assert!(matches!(t.issue(&ana()).await, Err(TicketError::Store(_))));
    assert!(matches!(
        t.redeem(&ticket).await,
        Err(TicketError::Store(_))
    ));
    store.fail(false);
    assert_eq!(
        t.redeem(&ticket).await,
        Ok(ana()),
        "a failed redeem didn't consume it"
    );
}

#[test]
fn the_identity_comes_from_the_gateway_claims() {
    let claims = InternalClaims {
        iss: "chelabase-gateway".into(),
        aud: "realtime".into(),
        iat: 1,
        exp: 61,
        sub: "user-ana-1234".into(),
        org_id: "org-77".into(),
        auth: AuthMethod::ManagementJwt,
        rid: "rid-1".into(),
        data: None,
        email: Some("ana@example.com".into()),
        name: Some("Ana".into()),
    };
    assert_eq!(TicketIdentity::from_claims(&claims), ana());
    assert_eq!(ana().kind(), TicketKind::Studio);
    let mut app = ana();
    app.auth = AuthMethod::DataKey;
    assert_eq!(app.kind(), TicketKind::App);
}

#[tokio::test]
async fn the_memory_store_takes_once() {
    // The fake's own GETDEL, which every socket test relies on.
    let store = MemoryStore::new();
    store
        .put("rt:x", b"v".to_vec(), Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(store.take("rt:x").await.unwrap(), Some(b"v".to_vec()));
    assert_eq!(store.take("rt:x").await.unwrap(), None);
}
