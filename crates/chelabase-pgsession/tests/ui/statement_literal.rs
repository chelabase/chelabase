// A named statement can't be built around the check.
fn main() {
    let _ = chelabase_pgsession::service::NamedStatement {
        name: "escalate",
        sql: "SET ROLE postgres",
        types: &[],
    };
}
