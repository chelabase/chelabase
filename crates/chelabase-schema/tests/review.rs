//! The review's parsing, hash and fix SQL (PB, T2).

use chelabase_schema::review::{fix_sql, parse_review, review_hash, review_sql};
use chelabase_types::protection::*;

const DOC: &str = r#"[
  {"kind":"trigger","schema":"public","name":"tg","identity":"public.t","function":"public.tf()","owner":"dev","reasons":[{"kind":"trigger","via":"public.t.tg"}],"opaque_body":false},
  {"kind":"function","schema":"public","name":"f","identity":"a integer","function":"public.f(a integer)","owner":"dev",
   "reasons":[{"kind":"security_definer","via":null},{"kind":"policy","via":"public.t.p"},{"kind":"policy","via":"public.t.p"}],"opaque_body":true},
  {"kind":"rule","schema":"public","name":"r","identity":"public.t","function":null,"owner":"dev","reasons":[{"kind":"rule","via":"public.t.r"}],"opaque_body":false}
]"#;

fn items() -> Vec<ReviewItem> {
    parse_review("app", DOC).unwrap()
}

#[test]
fn parse_sets_keys_sorts_items_and_dedups_reasons() {
    let items = items();
    let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["app/function/public.f(a integer)", "app/rule/public.r(public.t)", "app/trigger/public.tg(public.t)"]);
    let f = &items[0];
    assert_eq!((f.database.as_str(), f.kind, f.owner.as_str(), f.opaque_body, f.new), ("app", ReviewItemKind::Function, "dev", true, false));
    assert_eq!(f.function.as_deref(), Some("public.f(a integer)"));
    assert_eq!(
        f.reasons,
        [
            ReviewReason { kind: ReviewReasonKind::SecurityDefiner, via: None },
            ReviewReason { kind: ReviewReasonKind::Policy, via: Some("public.t.p".into()) },
        ]
    );
    assert_eq!(items[1].function, None);
}

#[test]
fn parse_refuses_what_it_does_not_know() {
    assert!(parse_review("app", "{}").is_err());
    assert!(parse_review("app", r#"[{"kind":"view","schema":"s","name":"n","identity":"","function":null,"owner":"o","reasons":[],"opaque_body":false}]"#).is_err());
    assert_eq!(parse_review("app", "[]").unwrap(), vec![]);
}

#[test]
fn the_hash_is_order_free_and_covers_keys_owners_and_reasons() {
    let base = items();
    let mut reversed = base.clone();
    reversed.reverse();
    assert_eq!(review_hash(&base), review_hash(&reversed));
    assert_eq!(review_hash(&base).len(), 64);
    let mut marked = base.clone();
    marked[0].new = true;
    marked[0].opaque_body = false;
    assert_eq!(review_hash(&base), review_hash(&marked), "new and opaque_body aren't hashed");
    let mut owner = base.clone();
    owner[0].owner = "other".into();
    assert_ne!(review_hash(&base), review_hash(&owner));
    let mut reasons = base.clone();
    reasons[0].reasons.pop();
    assert_ne!(review_hash(&base), review_hash(&reasons));
    assert_ne!(review_hash(&base), review_hash(&base[1..]));
    // The empty list: SHA-256 of "".
    assert_eq!(review_hash(&[]), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    // One line per item: `key \t owner \t reasons`.
    let one = parse_review("db", r#"[{"kind":"function","schema":"s","name":"n","identity":"","function":"s.n()","owner":"o","reasons":[{"kind":"view","via":"s.v"},{"kind":"security_definer","via":null}],"opaque_body":false}]"#).unwrap();
    use sha2::{Digest, Sha256};
    // PB end-of-step R15: `key \t owner \t reasons \t digest` (no digest in the document: empty).
    let expected = hex::encode(Sha256::digest(b"db/function/s.n()\to\tsecurity_definer,view:s.v\t"));
    assert_eq!(review_hash(&one), expected);
}

#[test]
fn fixes_render_per_kind_with_a_quoted_owner() {
    let items = items();
    let (f, r, t) = (&items[0], &items[1], &items[2]);
    assert_eq!(fix_sql(f, FixAction::Reassign, "postgres").unwrap(), "ALTER ROUTINE public.f(a integer) OWNER TO postgres;");
    assert_eq!(fix_sql(f, FixAction::Drop, "postgres").unwrap(), "DROP ROUTINE public.f(a integer) RESTRICT;");
    assert_eq!(fix_sql(t, FixAction::Reassign, "Own Er").unwrap(), "ALTER FUNCTION public.tf() OWNER TO \"Own Er\";");
    assert_eq!(fix_sql(t, FixAction::Drop, "postgres").unwrap(), "DROP TRIGGER tg ON public.t RESTRICT;");
    assert_eq!(fix_sql(r, FixAction::Reassign, "postgres").unwrap(), "ALTER TABLE public.t OWNER TO postgres;");
    assert_eq!(fix_sql(r, FixAction::Drop, "postgres").unwrap(), "DROP RULE r ON public.t RESTRICT;");
    let mut broken = f.clone();
    broken.function = None;
    assert!(fix_sql(&broken, FixAction::Drop, "postgres").is_err());
}

// --- T2 fix round (review findings 6–8, R4–R6) -------------------------------------------

#[test]
fn keys_quote_their_names_so_they_never_collide() {
    let doc = |schema: &str, name: &str| {
        format!(r#"[{{"kind":"function","schema":"{schema}","name":"{name}","identity":"","function":"x()","owner":"o","reasons":[],"opaque_body":false}}]"#)
    };
    let a = parse_review("app", &doc("a.b", "c")).unwrap();
    let b = parse_review("app", &doc("a", "b.c")).unwrap();
    assert_eq!(a[0].key, r#"app/function/"a.b".c()"#);
    assert_eq!(b[0].key, r#"app/function/a."b.c"()"#);
    assert_ne!(review_hash(&a), review_hash(&b));
    // Plain names stay as they are; the database is quoted too.
    assert_eq!(parse_review("My Db", &doc("public", "f")).unwrap()[0].key, r#""My Db"/function/public.f()"#);
}

#[test]
fn the_hash_quotes_owners_so_a_name_cannot_forge_a_line() {
    let doc = |owner: &str, reasons: &str| {
        format!(r#"[{{"kind":"function","schema":"s","name":"n","identity":"","function":"s.n()","owner":"{owner}","reasons":[{reasons}],"opaque_body":false}}]"#)
    };
    let forged = parse_review("db", &doc("o\\tview:s.v", "")).unwrap();
    let real = parse_review("db", &doc("o", r#"{"kind":"view","via":"s.v"}"#)).unwrap();
    assert_ne!(review_hash(&forged), review_hash(&real));
    use sha2::{Digest, Sha256};
    let quoted = parse_review("db", &doc("Own Er", r#"{"kind":"view","via":"s.v"}"#)).unwrap();
    assert_eq!(review_hash(&quoted), hex::encode(Sha256::digest(b"db/function/s.n()\t\"Own Er\"\tview:s.v\t")));
}

#[test]
fn a_trigger_whose_function_is_admin_owned_reassigns_its_table() {
    let doc = r#"[{"kind":"trigger","schema":"public","name":"atg","identity":"public.t","function":null,"owner":"dev","reasons":[{"kind":"trigger","via":"public.t.atg"}],"opaque_body":false}]"#;
    let item = &parse_review("app", doc).unwrap()[0];
    assert_eq!(fix_sql(item, FixAction::Reassign, "postgres").unwrap(), "ALTER TABLE public.t OWNER TO postgres;");
    assert_eq!(fix_sql(item, FixAction::Drop, "postgres").unwrap(), "DROP TRIGGER atg ON public.t RESTRICT;");
}

#[test]
fn a_range_type_is_a_reason() {
    let doc = r#"[{"kind":"function","schema":"public","name":"f_diff","identity":"double precision, double precision","function":"public.f_diff(double precision, double precision)","owner":"dev","reasons":[{"kind":"type","via":"public.r"}],"opaque_body":true}]"#;
    let item = &parse_review("app", doc).unwrap()[0];
    assert_eq!(item.reasons, [ReviewReason { kind: ReviewReasonKind::Type, via: Some("public.r".into()) }]);
}

// PB end-of-step R15: an item's definition digest is part of the hash, so a body replaced in
// place (same key, owner and reasons) changes it.
#[test]
fn a_changed_definition_changes_the_hash() {
    let doc = |digest: &str| {
        format!(r#"[{{"kind":"function","schema":"s","name":"n","identity":"","function":"s.n()","owner":"o","reasons":[],"opaque_body":true,"digest":"{digest}"}}]"#)
    };
    let a = parse_review("db", &doc("d1")).unwrap();
    let b = parse_review("db", &doc("d2")).unwrap();
    assert_eq!(a[0].digest, "d1");
    assert_ne!(review_hash(&a), review_hash(&b));
    use sha2::{Digest, Sha256};
    assert_eq!(review_hash(&a), hex::encode(Sha256::digest(b"db/function/s.n()\to\t\td1")));
}

#[test]
fn the_scan_is_one_read_only_select_for_the_owner() {
    let sql = review_sql("own'er");
    assert!(sql.starts_with(REVIEW_SCAN_MARK), "{sql}");
    assert!(sql.contains("'own''er'"), "the owner as a literal: {sql}");
    for word in ["INSERT ", "UPDATE ", "DELETE ", "ALTER ", "DROP ", "CREATE ", "GRANT ", "SET "] {
        assert!(!sql.to_uppercase().contains(word), "{word}: {sql}");
    }
    for needle in ["pg_catalog.pg_depend", "prosecdef", "chelabase_guard", "rolsuper"] {
        assert!(sql.contains(needle), "{needle}");
    }
    // R14: only superusers and the owner are trusted; no other role is named in the scan.
    for gone in ["neon_superuser", "cloud_admin"] {
        assert!(!sql.contains(gone), "{gone}");
    }
}

/// #185: object storage's own `storage` schema is skipped like `chelabase_guard`, but only
/// when `chelabase_admin` owns it; a `storage` schema anyone else owns is reviewed.
#[test]
fn the_scan_skips_object_storages_schema_only_when_chelabase_admin_owns_it() {
    let sql = review_sql("owner");
    let skipped = &sql[sql.find("skipped AS (").unwrap()..];
    let skipped = &skipped[..skipped.find("\n),").unwrap()];
    assert!(
        skipped.contains(
            "OR (n.nspname OPERATOR(pg_catalog.=) 'storage'\n         AND n.nspowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r\n           WHERE r.rolname OPERATOR(pg_catalog.=) 'chelabase_admin'))"
        ),
        "{skipped}"
    );
    assert_eq!(skipped.matches("'storage'").count(), 1, "no name-only skip: {skipped}");
}
