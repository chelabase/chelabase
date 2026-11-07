// Leaked to 'static, a request's text still can't go through the macro.
fn main() {
    let request = String::from("DELETE FROM storage.objects");
    let _ = chelabase_pgsession::named_statement!("from request", request.leak(), &[]);
}
