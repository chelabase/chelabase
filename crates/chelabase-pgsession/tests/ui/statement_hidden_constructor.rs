// The re-audit's `adv_hidden_ctor`: the constructor the macro used to expand to took run-time
// text. There is no such constructor.
use chelabase_pgsession::service::NamedStatement;
use chelabase_pgsession::{InternalSource, ServicePool};

async fn caller_sql(pool: &ServicePool<InternalSource>, request_body: String) {
    let statement = NamedStatement::__literal_only_use_named_statement_macro(
        "from request",
        request_body.leak(),
        &[],
    );
    let _ = pool
        .query(uuid::Uuid::nil(), "main", &statement, &[], "rid")
        .await;
}

fn main() {}
