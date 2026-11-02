// Nor through a constructor taking text: there is none.
fn main() {
    let request = String::from("DELETE FROM storage.objects");
    let _ = chelabase_pgsession::service::NamedStatement::new("from request", request.leak(), &[]);
}
