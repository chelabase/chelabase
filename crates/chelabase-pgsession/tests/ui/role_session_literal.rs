// A role session can't be built outside the crate.
fn forged(
    lease: chelabase_query::Lease,
    info: std::sync::Arc<chelabase_pgsession::ConnInfo>,
    claims: serde_json::Value,
) -> chelabase_pgsession::RoleSession {
    chelabase_pgsession::RoleSession {
        lease,
        info,
        role: "postgres".into(),
        claims,
        oid: None,
    }
}

fn main() {}
