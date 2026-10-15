//! F4 (T9 I2): the DDL receiver's reconcile request is the one run the worker's RT3-11 retry
//! applies to, so it says so (`from_ddl`); the whole branch, no single member.

use super::*;

#[test]
fn the_receivers_reconcile_request_is_marked_from_ddl() {
    let branch = BranchId::from(Uuid::from_u128(7));
    let request = ddl_reconcile_request(branch);
    assert_eq!(request.branch_id, branch.as_uuid());
    assert_eq!(request.member_id, None, "the whole branch");
    assert!(request.from_ddl);
    // On the wire under the name the worker reads.
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["from_ddl"], serde_json::json!(true));
}
