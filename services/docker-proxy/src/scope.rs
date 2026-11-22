//! The stack scope rule.
//!
//! An object (container, network, volume; an exec through its container) is in scope iff at
//! least one of `chelabase.dev/stack == slug` and `com.docker.compose.project == project`
//! holds **and neither label has a foreign value**. Containers inherit their image's labels,
//! so a lone matching label is not enough: an image carrying another stack's compose label
//! would otherwise put its containers in scope for both stacks.
//!
//! Label keys are case-sensitive in the engine, but a key that differs from a scope key only
//! in ASCII case or surrounding whitespace is treated as a forgery attempt: it puts the object
//! out of scope and refuses a create.

use std::collections::BTreeMap;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

use crate::Deny;
use crate::ops::parse_query;

/// The stack label every proxied create carries.
pub const STACK_LABEL: &str = "chelabase.dev/stack";
/// Compose's project label.
pub const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";

/// An object's labels.
pub type Labels = BTreeMap<String, String>;

/// The stack a proxy serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    /// The workbench slug (`feat-x`, `main`).
    pub slug: String,
    /// Its Compose project (`chelabase-feat-x`; `chelabase` for main).
    pub project: String,
}

impl Stack {
    /// Checks both names: `[a-z0-9][a-z0-9-]*`, at most 64 characters (Compose's project
    /// charset without `_`, the workbench's slug charset).
    pub fn new(slug: &str, project: &str) -> Result<Self, Deny> {
        let ok = |s: &str| {
            s.len() <= 64
                && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        };
        if !ok(slug) || !ok(project) {
            return Err(Deny::new(format!(
                "stack {slug:?} / project {project:?} is not valid"
            )));
        }
        Ok(Self {
            slug: slug.to_string(),
            project: project.to_string(),
        })
    }
}

/// A key as compared for look-alikes: trimmed and ASCII lower-cased.
fn fold(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

/// Whether `labels` hold a key that folds to a scope key without being it.
fn has_lookalike(labels: &Labels) -> bool {
    labels.keys().any(|k| {
        let f = fold(k);
        (f == STACK_LABEL || f == COMPOSE_PROJECT_LABEL)
            && k != STACK_LABEL
            && k != COMPOSE_PROJECT_LABEL
    })
}

/// The scope labels' values, or `None` when either is foreign (or a look-alike is present).
fn scope_values<'a>(
    labels: &'a Labels,
    stack: &Stack,
) -> Option<(Option<&'a str>, Option<&'a str>)> {
    if has_lookalike(labels) {
        return None;
    }
    let s = labels.get(STACK_LABEL).map(String::as_str);
    let p = labels.get(COMPOSE_PROJECT_LABEL).map(String::as_str);
    if s.is_some_and(|v| v != stack.slug) || p.is_some_and(|v| v != stack.project) {
        return None;
    }
    Some((s, p))
}

/// Whether an object with `labels` belongs to `stack`.
pub fn in_scope(labels: &Labels, stack: &Stack) -> bool {
    matches!(scope_values(labels, stack), Some((s, p)) if s.is_some() || p.is_some())
}

/// Forces the stack label onto a create's labels. A create that names another stack or
/// project (or a look-alike key) is refused.
pub fn force_labels(labels: &mut Labels, stack: &Stack) -> Result<(), Deny> {
    if scope_values(labels, stack).is_none() {
        return Err(Deny::new(format!(
            "the create's labels name another stack than {} (or a look-alike scope key)",
            stack.slug
        )));
    }
    labels.insert(STACK_LABEL.to_string(), stack.slug.clone());
    Ok(())
}

/// Refuses a create whose image carries any `chelabase.dev/*` or `com.docker.compose.*` label:
/// the container would inherit it.
pub fn reserved_image_labels(labels: &Labels) -> Result<(), Deny> {
    match labels.keys().find(|k| {
        let f = fold(k);
        f.starts_with("chelabase.dev/") || f.starts_with("com.docker.compose.")
    }) {
        Some(k) => Err(Deny::new(format!(
            "the image carries the reserved label {k:?}"
        ))),
        None => Ok(()),
    }
}

/// Rewrites a list query so the engine returns only the stack's objects: a
/// `label=chelabase.dev/stack=<slug>` filter is merged into `filters`, unless the filters
/// already pin `com.docker.compose.project=<project>` (a compose list, whose objects may
/// predate the stack label). A `filters` value that doesn't parse, or that names another
/// project or stack, is refused. Other parameters pass through in order.
pub fn scope_list_query(query: Option<&str>, stack: &Stack) -> Result<String, Deny> {
    let mut filters = BTreeMap::new();
    let mut out = Vec::new();
    for (k, v) in parse_query(query)? {
        if k == "filters" {
            filters = parse_filters(&v)?;
        } else {
            out.push(format!("{}={}", encode(&k), encode(&v)));
        }
    }
    let labels: &mut Vec<String> = filters.entry("label".to_string()).or_default();
    let own_stack = format!("{STACK_LABEL}={}", stack.slug);
    let own_project = format!("{COMPOSE_PROJECT_LABEL}={}", stack.project);
    for l in labels.iter() {
        let foreign = |key: &str, own: &str| {
            l.strip_prefix(key)
                .is_some_and(|rest| rest.starts_with('=') && l != own)
        };
        if foreign(STACK_LABEL, &own_stack) || foreign(COMPOSE_PROJECT_LABEL, &own_project) {
            return Err(Deny::new(format!("list filter {l:?} names another stack")));
        }
    }
    // Docker ANDs label filters, so a pinned project already confines the list.
    if !labels.contains(&own_project) && !labels.contains(&own_stack) {
        labels.push(own_stack);
    }
    let json = serde_json::to_string(&filters).map_err(|e| Deny::new(e.to_string()))?;
    out.push(format!("filters={}", encode(&json)));
    Ok(out.join("&"))
}

/// A `filters` value: `{"key": ["v", …]}`, or the legacy `{"key": {"v": true}}`. Empty means
/// none.
fn parse_filters(raw: &str) -> Result<BTreeMap<String, Vec<String>>, Deny> {
    let deny = || Deny::new(format!("list filters {raw:?} don't parse"));
    let mut out = BTreeMap::new();
    if raw.is_empty() {
        return Ok(out);
    }
    let Value::Object(map) = serde_json::from_str::<Value>(raw).map_err(|_| deny())? else {
        return Err(deny());
    };
    for (key, values) in map {
        let values: Vec<String> = match values {
            Value::Array(items) => items
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    _ => Err(deny()),
                })
                .collect::<Result<_, _>>()?,
            Value::Object(items) => items
                .into_iter()
                .map(|(k, v)| if v.is_boolean() { Ok(k) } else { Err(deny()) })
                .collect::<Result<_, _>>()?,
            _ => return Err(deny()),
        };
        out.insert(key, values);
    }
    Ok(out)
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn encode(s: &str) -> String {
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(s, SET).to_string()
}

/// Drops out-of-scope items from a list response: a JSON array of objects with `Labels`
/// (containers, networks), or a volume list (`{"Volumes": [...]}`). Any other shape is
/// refused.
pub fn post_filter_list(body: Value, stack: &Stack) -> Result<Value, Deny> {
    let keep = |items: Vec<Value>| -> Vec<Value> {
        items
            .into_iter()
            .filter(|item| item_in_scope(item, stack))
            .collect()
    };
    match body {
        Value::Array(items) => Ok(Value::Array(keep(items))),
        Value::Object(mut map) => match map.remove("Volumes") {
            Some(Value::Array(items)) => {
                map.insert("Volumes".into(), Value::Array(keep(items)));
                Ok(Value::Object(map))
            }
            Some(Value::Null) => {
                map.insert("Volumes".into(), Value::Array(Vec::new()));
                Ok(Value::Object(map))
            }
            _ => Err(Deny::new("unexpected list response")),
        },
        _ => Err(Deny::new("unexpected list response")),
    }
}

/// A list item is in scope when its `Labels` are a string map that is.
fn item_in_scope(item: &Value, stack: &Stack) -> bool {
    let Some(Value::Object(raw)) = item.get("Labels") else {
        return false;
    };
    let mut labels = Labels::new();
    for (k, v) in raw {
        let Value::String(v) = v else {
            return false;
        };
        labels.insert(k.clone(), v.clone());
    }
    in_scope(&labels, stack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stack() -> Stack {
        Stack::new("feat-x", "chelabase-feat-x").unwrap()
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn stack_names_are_checked() {
        Stack::new("main", "chelabase").unwrap();
        for (s, p) in [
            ("", "chelabase-x"),
            ("x", ""),
            ("-x", "chelabase-x"),
            ("X", "chelabase-x"),
            ("x_y", "c"),
            ("x", "a,b"),
            ("x", "a=b"),
        ] {
            assert!(Stack::new(s, p).is_err(), "{s:?} {p:?}");
        }
    }

    #[test]
    fn foreign_label() {
        let s = stack();
        assert!(!in_scope(&labels(&[(STACK_LABEL, "other")]), &s));
        assert!(!in_scope(&Labels::new(), &s));
        assert!(!in_scope(
            &labels(&[("com.docker.compose.service", "x")]),
            &s
        ));
        assert!(!in_scope(&labels(&[(STACK_LABEL, "")]), &s));
        assert!(!in_scope(&labels(&[(STACK_LABEL, "feat-x ")]), &s));
        assert!(!in_scope(&labels(&[(STACK_LABEL, "FEAT-X")]), &s));
        assert!(!in_scope(&labels(&[(STACK_LABEL, "feat")]), &s));
        // Look-alike keys never count, and spoil a matching one.
        assert!(!in_scope(&labels(&[("Chelabase.dev/stack", "feat-x")]), &s));
        assert!(!in_scope(
            &labels(&[(STACK_LABEL, "feat-x"), ("chelabase.dev/Stack", "other")]),
            &s
        ));
        assert!(!in_scope(
            &labels(&[(STACK_LABEL, "feat-x"), (" chelabase.dev/stack", "other")]),
            &s
        ));
        assert!(!in_scope(
            &labels(&[(STACK_LABEL, "feat-x"), ("com.docker.compose.Project", "x")]),
            &s
        ));
        assert!(in_scope(&labels(&[(STACK_LABEL, "feat-x")]), &s));
    }

    #[test]
    fn compose_project() {
        let s = stack();
        assert!(in_scope(
            &labels(&[(COMPOSE_PROJECT_LABEL, "chelabase-feat-x")]),
            &s
        ));
        assert!(in_scope(
            &labels(&[
                (COMPOSE_PROJECT_LABEL, "chelabase-feat-x"),
                (STACK_LABEL, "feat-x")
            ]),
            &s
        ));
        assert!(!in_scope(
            &labels(&[(COMPOSE_PROJECT_LABEL, "chelabase")]),
            &s
        ));
        assert!(!in_scope(
            &labels(&[(COMPOSE_PROJECT_LABEL, "chelabase-feat-x-2")]),
            &s
        ));
        // The main stack's project is `chelabase`.
        let main = Stack::new("main", "chelabase").unwrap();
        assert!(in_scope(
            &labels(&[(COMPOSE_PROJECT_LABEL, "chelabase")]),
            &main
        ));
        assert!(!in_scope(
            &labels(&[(COMPOSE_PROJECT_LABEL, "chelabase-feat-x")]),
            &main
        ));
    }

    #[test]
    fn mixed_foreign_label_out_of_scope() {
        let a = Stack::new("a", "chelabase-a").unwrap();
        let b = Stack::new("b", "chelabase-b").unwrap();
        let mixed = labels(&[(STACK_LABEL, "a"), (COMPOSE_PROJECT_LABEL, "chelabase-b")]);
        assert!(!in_scope(&mixed, &a));
        assert!(!in_scope(&mixed, &b));
        let reverse = labels(&[(STACK_LABEL, "b"), (COMPOSE_PROJECT_LABEL, "chelabase-a")]);
        assert!(!in_scope(&reverse, &a));
        assert!(!in_scope(&reverse, &b));
    }

    #[test]
    fn forced_label_conflict() {
        let s = stack();
        for bad in [
            labels(&[(STACK_LABEL, "other")]),
            labels(&[(COMPOSE_PROJECT_LABEL, "chelabase-other")]),
            labels(&[
                (STACK_LABEL, "feat-x"),
                (COMPOSE_PROJECT_LABEL, "chelabase-other"),
            ]),
            labels(&[(STACK_LABEL, "")]),
            labels(&[("CHELABASE.DEV/STACK", "other")]),
            labels(&[("com.docker.compose.project ", "chelabase-feat-x")]),
        ] {
            let mut l = bad.clone();
            assert!(force_labels(&mut l, &s).is_err(), "{bad:?}");
        }
        for good in [
            Labels::new(),
            labels(&[("app", "x")]),
            labels(&[(STACK_LABEL, "feat-x")]),
            labels(&[
                (COMPOSE_PROJECT_LABEL, "chelabase-feat-x"),
                ("com.docker.compose.service", "x"),
            ]),
        ] {
            let mut l = good.clone();
            force_labels(&mut l, &s).unwrap();
            assert_eq!(l.get(STACK_LABEL).map(String::as_str), Some("feat-x"));
            assert!(in_scope(&l, &s), "{l:?}");
            for (k, v) in &good {
                assert_eq!(l.get(k), Some(v), "other labels kept");
            }
        }
    }

    #[test]
    fn reserved_image_labels_refused() {
        for bad in [
            labels(&[(STACK_LABEL, "feat-x")]),
            labels(&[("chelabase.dev/endpoint-id", "x")]),
            labels(&[("chelabase.dev/service-id", "x")]),
            labels(&[(COMPOSE_PROJECT_LABEL, "chelabase-feat-x")]),
            labels(&[("com.docker.compose.service", "x")]),
            labels(&[("Com.Docker.Compose.project", "x")]),
            labels(&[("CHELABASE.DEV/x", "y")]),
            labels(&[(" chelabase.dev/x", "y")]),
            labels(&[("maintainer", "x"), ("chelabase.dev/", "")]),
        ] {
            assert!(reserved_image_labels(&bad).is_err(), "{bad:?}");
        }
        reserved_image_labels(&Labels::new()).unwrap();
        reserved_image_labels(&labels(&[
            ("org.opencontainers.image.source", "x"),
            ("chelabase", "x"),
            ("com.docker.composer", "x"),
        ]))
        .unwrap();
    }

    fn filters_of(query: &str) -> Value {
        let pairs = parse_query(Some(query)).unwrap();
        let f = pairs
            .iter()
            .find(|(k, _)| k == "filters")
            .expect("filters")
            .1
            .clone();
        serde_json::from_str(&f).unwrap()
    }

    #[test]
    fn list_filter_injection() {
        let s = stack();
        let want = format!("{STACK_LABEL}=feat-x");
        // No filters.
        for q in [None, Some(""), Some("all=true"), Some("filters=")] {
            let out = scope_list_query(q, &s).unwrap();
            assert_eq!(filters_of(&out), json!({"label": [want]}), "{q:?}");
        }
        assert!(
            scope_list_query(Some("all=true"), &s)
                .unwrap()
                .starts_with("all=true&")
        );
        // Existing filters, array and legacy map forms.
        let q = format!(
            "all=1&filters={}",
            urlenc(r#"{"label":["chelabase.dev/endpoint-id"],"status":["exited"]}"#)
        );
        let f = filters_of(&scope_list_query(Some(&q), &s).unwrap());
        assert_eq!(f["status"], json!(["exited"]));
        let l: Vec<&str> = f["label"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            l.contains(&"chelabase.dev/endpoint-id") && l.contains(&want.as_str()),
            "{l:?}"
        );
        let q = format!(
            "filters={}",
            urlenc(r#"{"label":{"x=y":true},"name":{"n":true}}"#)
        );
        let f = filters_of(&scope_list_query(Some(&q), &s).unwrap());
        assert_eq!(f["name"], json!(["n"]));
        assert_eq!(f["label"], json!(["x=y", want]));
        // A compose list keeps its own project filter and gets no stack filter.
        let q = format!(
            "filters={}",
            urlenc(
                r#"{"label":["com.docker.compose.project=chelabase-feat-x","com.docker.compose.service=functions"]}"#
            )
        );
        let f = filters_of(&scope_list_query(Some(&q), &s).unwrap());
        assert_eq!(
            f["label"],
            json!([
                "com.docker.compose.project=chelabase-feat-x",
                "com.docker.compose.service=functions"
            ])
        );
        // Malformed or foreign filters, or a repeated parameter.
        for bad in [
            "filters=%7B",
            "filters=not-json",
            "filters=%5B%5D",
            "filters=%22x%22",
            "filters=%7B%22label%22%3A%22x%22%7D",
            "filters=%7B%22label%22%3A%5B1%5D%7D",
            "filters=%7B%22label%22%3A%7B%22x%22%3A1%7D%7D",
            "filters=%zz",
            "filters=%7B%7D&filters=%7B%7D",
        ] {
            assert!(scope_list_query(Some(bad), &s).is_err(), "{bad}");
        }
        for foreign in [
            r#"{"label":["chelabase.dev/stack=other"]}"#,
            r#"{"label":["com.docker.compose.project=chelabase-other"]}"#,
        ] {
            let q = format!("filters={}", urlenc(foreign));
            assert!(scope_list_query(Some(&q), &s).is_err(), "{foreign}");
        }
    }

    fn urlenc(s: &str) -> String {
        percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
    }

    #[test]
    fn list_responses_are_post_filtered() {
        let s = stack();
        let list = json!([
            {"Id": "a", "Labels": {STACK_LABEL: "feat-x"}},
            {"Id": "b", "Labels": {STACK_LABEL: "other"}},
            {"Id": "c", "Labels": null},
            {"Id": "d"},
            {"Id": "e", "Labels": {COMPOSE_PROJECT_LABEL: "chelabase-feat-x"}},
            {"Id": "f", "Labels": {STACK_LABEL: "feat-x", COMPOSE_PROJECT_LABEL: "chelabase-b"}},
            {"Id": "g", "Labels": {STACK_LABEL: 1}},
            "not an object",
        ]);
        let out = post_filter_list(list, &s).unwrap();
        let ids: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["Id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "e"]);
        let volumes = json!({"Volumes": [
            {"Name": "chelabase-feat-x_pg", "Labels": {COMPOSE_PROJECT_LABEL: "chelabase-feat-x"}},
            {"Name": "chelabase-b_pg", "Labels": {COMPOSE_PROJECT_LABEL: "chelabase-b"}},
        ], "Warnings": null});
        let out = post_filter_list(volumes, &s).unwrap();
        assert_eq!(out["Volumes"].as_array().unwrap().len(), 1);
        assert_eq!(out["Volumes"][0]["Name"], "chelabase-feat-x_pg");
        assert!(
            post_filter_list(json!({"Volumes": null}), &s).unwrap()["Volumes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        for bad in [
            json!({"x": 1}),
            json!("x"),
            json!(null),
            json!({"Volumes": "x"}),
        ] {
            assert!(post_filter_list(bad.clone(), &s).is_err(), "{bad}");
        }
    }
}
