// A role session has no constructor.
fn forged(lease: chelabase_query::Lease) {
    let _ = chelabase_pgsession::RoleSession::new(lease);
}

fn main() {}
