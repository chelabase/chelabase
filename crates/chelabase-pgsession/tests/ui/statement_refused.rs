// What the text check refuses doesn't build: a role change, and the audits' bypasses.
use chelabase_pgsession::named_statement;
use chelabase_pgsession::service::NamedStatement;

const SET_ROLE: NamedStatement = named_statement!("a", "SET ROLE postgres", &[]);
const COMMENTS: NamedStatement = named_statement!("b", "/**/SET/**/ROLE postgres", &[]);
const LINE_COMMENT: NamedStatement =
    named_statement!("c", "-- managed write\nSET search_path TO evil", &[]);
const BLOCK_COMMENT: NamedStatement =
    named_statement!("d", "/* x */ SET search_path TO evil", &[]);
const UNICODE: NamedStatement =
    named_statement!("e", "SELECT U&\"set\\005fconfig\"('role', 'postgres', false)", &[]);
const DYNAMIC: NamedStatement = named_statement!(
    "f",
    "DO $$BEGIN EXECUTE format('%s ROLE postgres', 'SE' || 'T'); END$$",
    &[]
);
const ADVISORY: NamedStatement = named_statement!("g", "SELECT pg_advisory_lock(1)", &[]);

fn main() {
    let _ = (SET_ROLE, COMMENTS, LINE_COMMENT, BLOCK_COMMENT, UNICODE, DYNAMIC, ADVISORY);
}
