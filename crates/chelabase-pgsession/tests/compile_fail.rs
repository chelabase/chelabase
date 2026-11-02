//! What the crate's API refuses to compile, each with its expected error (`tests/ui/*.stderr`):
//! no statement without a role, no owner path, no run-time text as a named statement, nothing
//! run on the managed pool but named statements. `TRYBUILD=overwrite` rewrites the expected
//! errors after a deliberate change.

#[test]
fn ui() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}
