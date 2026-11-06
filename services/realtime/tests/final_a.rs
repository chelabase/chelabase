//! Members hub step 1, final fix round, area A (T8-1, D7): a Studio ticket is issued only to a
//! member the orchestrator still lets in. Before issuing, the ticket route asks
//! `/internal/realtime/member` (realtime's bearer) with `{sub, tenant_id}` from the gateway's
//! identity: `{allowed: false}` (a suspended or offboarded member, a non-member) is 403 with no
//! ticket stored; an orchestrator that can't answer is 503.

mod common;

use common::*;
use serde_json::json;

fn stored_tickets(h: &Harness) -> usize {
    h.store
        .keys()
        .into_iter()
        .filter(|k| k.starts_with("rt:ticket:"))
        .count()
}

#[tokio::test]
async fn a_ticket_is_issued_only_to_a_member_the_orchestrator_lets_in() {
    let h = Harness::start().await;
    let (status, _, body) = h.post_ticket(h.rt(), Some(&h.identity("ana", TENANT))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        h.orch.member_calls(),
        vec![json!({ "sub": "ana", "tenant_id": TENANT, "sid": "sid-ana" })],
        "the ticket route didn't ask the orchestrator"
    );

    h.orch.on_member(|_| refused("forbidden"));
    let (status, _, body) = h.post_ticket(h.rt(), Some(&h.identity("sus", TENANT))).await;
    assert_eq!(status, 403, "a refused member got a ticket: {body}");
    assert!(body.get("ticket").is_none(), "{body}");
    assert_eq!(stored_tickets(&h), 1, "only ana's ticket is stored");

    h.orch
        .on_member(|_| (500, json!({ "error": { "code": "internal" } })));
    let (status, _, _) = h.post_ticket(h.rt(), Some(&h.identity("ana", TENANT))).await;
    assert_eq!(status, 503, "an orchestrator that can't answer");
    assert_eq!(stored_tickets(&h), 1);
}
