//! RT1 T4 (S11): the key strings realtime and Studio compare. A row's key is the JSON array of
//! its primary-key columns' texts, built in Rust with `serde_json::to_string(&Vec<String>)`,
//! never by a Postgres JSON function. `fixtures/text_keys.json` holds `[{texts, key}]`, with
//! each `key` produced by JavaScript's `JSON.stringify(texts)` (compact; `\u00XX` for control
//! characters; non-ASCII kept as is): T8's tests assert the service builds these keys, and T9's
//! assert Studio's `JSON.stringify` builds the same ones.

use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    texts: Vec<String>,
    key: String,
}

fn cases() -> Vec<Case> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text_keys.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn serde_json_builds_every_fixture_key() {
    let cases = cases();
    assert!(cases.len() >= 15, "only {} cases", cases.len());
    for c in &cases {
        assert_eq!(
            serde_json::to_string(&c.texts).unwrap(),
            c.key,
            "texts {:?}",
            c.texts
        );
    }
}

#[test]
fn the_fixture_covers_the_awkward_texts() {
    let cases = cases();
    let any = |f: &dyn Fn(&Case) -> bool| cases.iter().any(f);
    let texts = |c: &Case| c.texts.concat();
    assert!(any(&|c| texts(c).contains('"')), "a double quote");
    assert!(any(&|c| texts(c).contains('\\')), "a backslash");
    assert!(
        any(&|c| texts(c).contains('\n') && texts(c).contains('\t')),
        "newline, tab"
    );
    assert!(any(&|c| texts(c).contains('\u{0}')), "NUL");
    assert!(
        any(&|c| texts(c)
            .chars()
            .any(|ch| ch < ' ' && !"\n\t\r\u{8}\u{c}".contains(ch))),
        "a control char with a \\u escape"
    );
    assert!(
        any(&|c| c.key.contains("\\u00")),
        "a \\u00XX escape in a key"
    );
    assert!(any(&|c| !texts(c).is_ascii()), "non-ASCII");
    assert!(
        any(&|c| texts(c).chars().any(|ch| ch as u32 > 0xFFFF)),
        "an astral character"
    );
    assert!(any(&|c| texts(c).contains('\u{2028}')), "U+2028");
    assert!(any(&|c| c.texts.len() >= 2), "a composite key");
    assert!(
        any(&|c| c.texts.iter().any(String::is_empty)),
        "an empty text"
    );
    // Non-ASCII stays literal in a key; only control characters are escaped.
    for c in &cases {
        for ch in c.key.chars().filter(|ch| !ch.is_ascii()) {
            assert!(c.key.contains(ch));
        }
        assert!(!c.key.contains("\\ud"), "no escaped surrogates: {}", c.key);
    }
    // Compact: no space or newline outside the texts' own.
    assert!(
        cases
            .iter()
            .all(|c| !c.key.starts_with("[ ") && !c.key.contains("\",\"\n"))
    );
}
