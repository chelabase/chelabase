//! RT1 T1 edge cases beyond the RED set (coder): Valkey refuses any non-`user` line in its ACL
//! file, the NATS users file keeps still-valid hashes across renders (no churn) and rehashes a
//! changed password, and every permission subject is well formed.

use super::service_auth::{render_valkey_acl, write_auth_files};
use crate::dev::stack_secrets;
use std::collections::BTreeMap;

#[test]
fn every_acl_line_is_a_user_line() {
    let pw: BTreeMap<String, String> = stack_secrets::VALKEY_USERS
        .iter()
        .map(|u| (u.to_string(), format!("pw-{u}-0123456789abcdef0123456789")))
        .collect();
    let text = render_valkey_acl(&pw).unwrap();
    // valkey 9.1.2 aborts on a comment line ("should start with user keyword").
    for line in text.lines() {
        assert!(line.starts_with("user "), "{line:?}");
    }
}

#[test]
fn nats_hashes_are_kept_while_valid_and_renewed_when_a_password_changes() {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    stack_secrets::ensure(&keys).unwrap();
    write_auth_files(&keys, &state).unwrap();
    let first = std::fs::read_to_string(state.join("nats/auth.conf")).unwrap();
    write_auth_files(&keys, &state).unwrap();
    assert_eq!(
        std::fs::read_to_string(state.join("nats/auth.conf")).unwrap(),
        first,
        "a re-render keeps the bcrypt salts"
    );
    // A replaced secret (deleted, regenerated) gets a new hash; the others stay.
    std::fs::remove_file(keys.join("nats/worker.password")).unwrap();
    stack_secrets::ensure(&keys).unwrap();
    write_auth_files(&keys, &state).unwrap();
    let second = std::fs::read_to_string(state.join("nats/auth.conf")).unwrap();
    let hash_of = |text: &str, user: &str| -> String {
        let at = text.find(&format!("user: \"{user}\"")).unwrap();
        let rest = &text[at..];
        let p = rest.find("password: \"").unwrap() + "password: \"".len();
        rest[p..p + 60].to_string()
    };
    assert_ne!(hash_of(&first, "worker"), hash_of(&second, "worker"));
    assert_eq!(hash_of(&first, "realtime"), hash_of(&second, "realtime"));
    let pw = std::fs::read_to_string(keys.join("nats/worker.password")).unwrap();
    assert!(bcrypt::verify(pw, &hash_of(&second, "worker")).unwrap());
}

/// The `allow` list of `kind` (`publish` or `subscribe`) in `user`'s block of a rendered
/// auth.conf.
fn allow_list(text: &str, user: &str, kind: &str) -> Vec<String> {
    let at = text
        .find(&format!("user: \"{user}\""))
        .unwrap_or_else(|| panic!("no {user}"));
    let block = &text[at..];
    let block = &block[..block.find("\n    }\n").unwrap()];
    let line = block
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{kind}: {{ allow: [")))
        .unwrap_or_else(|| panic!("{user}: no {kind} list"));
    let list = &line[line.find('[').unwrap() + 1..line.find(']').unwrap()];
    list.split(", ")
        .map(|s| s.trim_matches('"').to_string())
        .collect()
}

// Audit (Critical): with `allow_responses`, a user that may publish to a subject it also
// subscribes to sends itself a message whose reply subject is any subject it likes
// (`chelabase.provision`, `$JS.API.STREAM.DELETE.CHELABASE_CONTROL`, `rt.compute.stopping`) and
// then publishes there: nats-server checks no permission on a reply subject. So no user gets it;
// responders publish to their callers' inboxes only.
#[test]
fn no_user_may_answer_on_an_arbitrary_reply_subject() {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    stack_secrets::ensure(&keys).unwrap();
    write_auth_files(&keys, &state).unwrap();
    let text = std::fs::read_to_string(state.join("nats/auth.conf")).unwrap();
    assert!(!text.contains("allow_responses"), "{text}");
    let inboxes = |user: &str| -> Vec<String> {
        allow_list(&text, user, "publish")
            .into_iter()
            .filter(|s| s.starts_with("_INBOX"))
            .collect()
    };
    // The callers each responder answers: the worker the orchestrator's RPCs, realtime the
    // presence syncs of its peers; the runner (tests) runs both in-process and calls both.
    assert_eq!(inboxes("orchestrator"), Vec::<String>::new());
    assert_eq!(inboxes("worker"), ["_INBOX_orchestrator.>", "_INBOX_runner.>"]);
    assert_eq!(inboxes("realtime"), ["_INBOX_realtime.>", "_INBOX_runner.>"]);
    let runner = inboxes("runner");
    for inbox in ["_INBOX_orchestrator.>", "_INBOX_realtime.>", "_INBOX_runner.>"] {
        assert!(runner.contains(&inbox.to_string()), "runner: {runner:?}");
    }
    // Realtime still can't publish the platform's events or the worker's subjects.
    for p in allow_list(&text, "realtime", "publish") {
        assert!(
            !p.starts_with("chelabase.rt.access")
                && !p.starts_with("chelabase.rt.project")
                && !p.starts_with("chelabase.rt.compute")
                && !p.starts_with("chelabase.rpc")
                && p != ">"
                && p != "chelabase.>"
                && p != "chelabase.*",
            "realtime publishes {p}"
        );
    }
}

#[test]
fn every_nats_subject_is_quoted_and_well_formed() {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    stack_secrets::ensure(&keys).unwrap();
    write_auth_files(&keys, &state).unwrap();
    let text = std::fs::read_to_string(state.join("nats/auth.conf")).unwrap();
    for line in text.lines().filter(|l| l.contains("allow: [")) {
        // Each `[...]` list (allow, then deny) holds `"subject"` items joined by `, `.
        for group in line.split('[').skip(1) {
            let list = &group[..group.find(']').expect("a closed list")];
            for item in list.split(", ") {
                let s = item.trim_matches('"');
                assert!(item.starts_with('"') && item.ends_with('"'), "{item}");
                assert!(
                    !s.is_empty() && !s.contains(' ') && !s.contains(".."),
                    "{s}"
                );
                // `>` only as the last token.
                assert!(!s.contains('>') || s.ends_with(".>") || s == ">", "{s}");
            }
        }
        // Nobody may read the shared inbox or everything.
        assert!(
            !line.contains("\"_INBOX.>\"") && !line.contains("\">\""),
            "{line}"
        );
    }
}

// Audit (upgrade path): a stack started before RT1 T1 keeps its computes on `<project>_default`
// (no storage token, unreachable on the compute network); `up` removes exactly those and the
// worker recreates them.
#[test]
fn computes_off_the_compute_network_are_picked_for_removal() {
    use super::commands::computes_off_network;
    let net = super::env::compute_network("chelabase-feat-x");
    assert_eq!(net, "chelabase-feat-x_compute");
    let ps = "chelabase-feat-x-ep-old\tchelabase-feat-x_default,chelabase-feat-x_control\n\
              /chelabase-feat-x-ep-new\tchelabase-feat-x_compute,chelabase-feat-x_control\n\
              chelabase-feat-x-ep-stopped\t\n\
              chelabase-feat-x-ep-near\tchelabase-feat-x_compute2\n\
              \n";
    assert_eq!(
        computes_off_network(ps, &net),
        [
            "chelabase-feat-x-ep-old",
            "chelabase-feat-x-ep-stopped",
            "chelabase-feat-x-ep-near"
        ]
    );
    assert!(computes_off_network("", &net).is_empty());
}
