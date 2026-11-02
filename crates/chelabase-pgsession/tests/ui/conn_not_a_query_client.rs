// A connection is no `QueryClient`: chelabase_query can't run on it without a session.
fn no_role_no_run(conn: &chelabase_pgsession::Conn) {
    fn runs_on<C: chelabase_query::QueryClient>(_: &C) {}
    runs_on(conn);
}

fn main() {}
