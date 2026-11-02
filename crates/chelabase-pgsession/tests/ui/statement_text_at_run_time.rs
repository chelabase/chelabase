// A statement's text is an associated const: run-time text can't fill it.
use chelabase_pgsession::service::{NamedStatement, StatementText};

struct FromRequest;

impl StatementText for FromRequest {
    const NAME: &'static str = "from request";
    const SQL: &'static str = String::from("DELETE FROM storage.objects").leak();
    const TYPES: &'static [tokio_postgres::types::Type] = &[];
}

fn main() {
    let _ = NamedStatement::of::<FromRequest>();
}
