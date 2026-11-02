use chelabase_identity::rules::{normalize_email, strong_password};

#[test]
fn strong_password_needs_every_class() {
    assert!(strong_password("Abcdef1!"));
    for weak in [
        "Abcde1!", "abcdef1!", "ABCDEF1!", "Abcdefg!", "Abcdefg1", "",
    ] {
        assert!(!strong_password(weak), "{weak:?}");
    }
}

#[test]
fn normalize_email_trims_and_lowercases() {
    assert_eq!(normalize_email("  Ada@Example.COM "), "ada@example.com");
}
