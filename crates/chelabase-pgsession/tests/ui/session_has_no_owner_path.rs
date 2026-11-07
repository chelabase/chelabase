// No owner path: `RunAs::Owner` needs a client the session doesn't hand out.
async fn as_the_owner(conn: chelabase_pgsession::Conn, claims: &serde_json::Value) {
    let _ = conn
        .with_role_session("authenticated", claims, None, async |s| {
            let settings = s.settings(&s.info().databases[0]);
            let _ = chelabase_query::engine::console_session(
                s.lease(),
                "main",
                chelabase_query::RunAs::Owner { user: "authenticator" },
                &settings,
                "rid",
                async |_, _| Ok(()),
            )
            .await;
        })
        .await;
}

fn main() {}
