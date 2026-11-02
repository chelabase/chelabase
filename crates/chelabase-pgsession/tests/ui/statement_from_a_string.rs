// A named statement is a literal: a request's text can't become one.
fn from_request(sql: String) {
    let _ = chelabase_pgsession::named_statement!("x", &sql, &[]);
}

fn main() {}
