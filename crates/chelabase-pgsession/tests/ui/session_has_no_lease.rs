// A role session hands out no client: nothing runs as the login before the preamble.
async fn as_the_login(conn: chelabase_pgsession::Conn, claims: &serde_json::Value) {
    use chelabase_query::QueryClient;
    let _ = conn
        .with_role_session("no_such_role", claims, None, async |s| {
            let _ = s.lease().client().batch_execute("SELECT 1").await;
        })
        .await;
}

fn main() {}
