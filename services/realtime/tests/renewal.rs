//! RT1 T8 RED fixes: a verified `auth` renewal reaches the change source for every joined
//! table channel (`ChangeSource::renew`), so the owner path's `/visible` calls carry the new
//! token instead of the join's, which expires at its `exp`.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

#[tokio::test]
async fn a_renewal_reaches_the_source_for_every_joined_table() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "a", None).await["type"], "joined");
    assert_eq!(
        a.join_table("2", "b", Some("billing")).await["type"],
        "joined"
    );
    let renewed = token("ana", 7200);
    a.send(json!({"type": "auth", "token": renewed.clone()}))
        .await;
    let src = h.source.clone();
    let r = renewed.clone();
    assert!(
        eventually(Duration::from_secs(2), move || {
            src.renewals("a") == vec![r.clone()] && src.renewals("b") == vec![r.clone()]
        })
        .await,
        "a: {:?}, b: {:?}",
        h.source.renewals("a"),
        h.source.renewals("b")
    );
}

#[tokio::test]
async fn a_renewal_that_fails_reaches_no_source() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "a", None).await["type"], "joined");
    // Another person's token: closed 4003, never handed to the source.
    a.send(json!({"type": "auth", "token": token("bob", 7200)}))
        .await;
    assert_eq!(a.closed().await, 4003);
    // The control: the same socket flow with ana's own renewal reaches it.
    let mut b = h.authed("ana").await;
    assert_eq!(b.join_table("1", "c", None).await["type"], "joined");
    let renewed = token("ana", 7200);
    b.send(json!({"type": "auth", "token": renewed.clone()}))
        .await;
    let src = h.source.clone();
    let r = renewed.clone();
    assert!(
        eventually(Duration::from_secs(2), move || src.renewals("c")
            == vec![r.clone()])
        .await,
        "{:?}",
        h.source.renewals("c")
    );
    assert!(
        h.source.renewals("a").is_empty(),
        "{:?}",
        h.source.renewals("a")
    );
}
