// No role, no run: a connection's client can't be read.
fn no_role_no_run(conn: chelabase_pgsession::Conn) {
    let _client = conn.client;
}

fn main() {}
