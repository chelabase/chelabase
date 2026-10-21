//! RT1 final fix round, batch A: failing tests first.
//!
//! - T1 audit hardening: the test runner's NATS user may create consumers on (and pull from,
//!   and acknowledge on) CHELABASE_CONTROL, the work-queue stream the worker's durable consumers
//!   drain: a test (or anything holding the runner's password) could take the stack's jobs.
//!   No runner live test consumes the control stream (they publish, read stream info and use
//!   their own `CHELABASE_TEST_*` streams), so the runner keeps the worker's rights except those.
//! - T1 live (workbench limit): the runner has one fixed address on the control network (the
//!   member-login rule lists it), so a second `workbench test`/`run` on a stack fails inside
//!   Docker with "Address already in use". It must fail before starting, saying which run holds
//!   the stack's runner and to wait for it.

use super::service_auth::write_auth_files;
use crate::dev::stack_secrets;

fn rendered_nats() -> String {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    stack_secrets::ensure(&keys).unwrap();
    write_auth_files(&keys, &state).unwrap();
    std::fs::read_to_string(state.join("nats/auth.conf")).unwrap()
}

/// `(allow, deny)` of `user`'s publish permissions in a rendered auth.conf.
fn publish_lists(text: &str, user: &str) -> (Vec<String>, Vec<String>) {
    let at = text
        .find(&format!("user: \"{user}\""))
        .unwrap_or_else(|| panic!("no {user}"));
    let block = &text[at..];
    let block = &block[..block.find("\n    }\n").unwrap()];
    let line = block
        .lines()
        .find(|l| l.trim_start().starts_with("publish: {"))
        .unwrap_or_else(|| panic!("{user}: no publish line"));
    let list_after = |marker: &str| -> Vec<String> {
        let Some(start) = line.find(marker) else {
            return Vec::new();
        };
        let rest = &line[start + marker.len()..];
        let list = &rest[..rest.find(']').unwrap()];
        list.split(", ")
            .map(|s| s.trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    (list_after("allow: ["), list_after("deny: ["))
}

/// NATS subject matching: `*` is one token, `>` one or more trailing tokens.
fn matches(pattern: &str, subject: &str) -> bool {
    let (p, s): (Vec<&str>, Vec<&str>) =
        (pattern.split('.').collect(), subject.split('.').collect());
    for (i, tok) in p.iter().enumerate() {
        match *tok {
            ">" => return s.len() > i,
            "*" => {
                if i >= s.len() {
                    return false;
                }
            }
            t => {
                if s.get(i) != Some(&t) {
                    return false;
                }
            }
        }
    }
    p.len() == s.len()
}

#[test]
fn the_subject_matcher_follows_nats() {
    assert!(matches(
        "$JS.API.CONSUMER.>",
        "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL"
    ));
    assert!(!matches("$JS.API.CONSUMER.>", "$JS.API.CONSUMER"));
    assert!(matches("chelabase.*", "chelabase.provision"));
    assert!(!matches("chelabase.*", "chelabase.rt.rows"));
    assert!(matches(
        "$JS.ACK.>",
        "$JS.ACK.CHELABASE_CONTROL.d.1.1.1.1.0"
    ));
}

fn may_publish(text: &str, user: &str, subject: &str) -> bool {
    let (allow, deny) = publish_lists(text, user);
    allow.iter().any(|p| matches(p, subject)) && !deny.iter().any(|p| matches(p, subject))
}

/// What a consumer of the control stream sends: creating one (ephemeral, filtered, durable),
/// pulling a durable's next message, acknowledging a delivery, deleting one.
const CONTROL_CONSUMER_SUBJECTS: [&str; 7] = [
    "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL",
    "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.steal",
    "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.chelabase-worker-provision.chelabase.provision",
    "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_CONTROL.chelabase-worker-provision",
    "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_CONTROL.chelabase-worker-provision",
    "$JS.ACK.CHELABASE_CONTROL.chelabase-worker-provision.1.7.7.1760000000000000000.0",
    "$JS.API.CONSUMER.DELETE.CHELABASE_CONTROL.chelabase-worker-provision",
];

#[test]
fn the_runner_cannot_consume_the_control_stream() {
    let text = rendered_nats();
    for subject in CONTROL_CONSUMER_SUBJECTS {
        assert!(
            !may_publish(&text, "runner", subject),
            "the runner may publish {subject}: it could take the worker's jobs off CHELABASE_CONTROL"
        );
    }
}

// Guards: the worker keeps its consumers; the runner keeps what its live tests use.
#[test]
fn the_worker_still_consumes_and_the_runner_keeps_its_test_rights() {
    let text = rendered_nats();
    for subject in &CONTROL_CONSUMER_SUBJECTS[..6] {
        assert!(
            may_publish(&text, "worker", subject),
            "the worker lost {subject}"
        );
    }
    for subject in [
        // Jobs and stream info (the orchestrator in-process).
        "chelabase.provision",
        "$JS.API.STREAM.INFO.CHELABASE_CONTROL",
        // Its own scratch streams and their consumers.
        "chelabase.test.t1",
        "$JS.API.STREAM.CREATE.CHELABASE_TEST_T1",
        "$JS.API.CONSUMER.CREATE.CHELABASE_TEST_T1",
        "$JS.API.CONSUMER.CREATE.CHELABASE_TEST_T1.c.chelabase.test.t1",
        "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_TEST_T1.c",
        "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_TEST_T1.c",
        "$JS.ACK.CHELABASE_TEST_T1.c.1.1.1.1.0",
        // Realtime in-process: its KV buckets' consumers.
        "$JS.API.CONSUMER.CREATE.KV_CHELABASE_RT_LEASES",
        "$JS.API.CONSUMER.CREATE.KV_CHELABASE_RT_SLOTS.c",
        "$JS.API.CONSUMER.DELETE.KV_CHELABASE_RT_LEASES.c",
    ] {
        assert!(
            may_publish(&text, "runner", subject),
            "the runner lost {subject}"
        );
    }
}

// --- One runner per stack -------------------------------------------------------------------
// Pinned: `commands::runner_ps_args(project: &str) -> Vec<String>` (the `docker` arguments that
// list the project's running runner containers, one name per line) and
// `commands::busy_runner(slug: &str, ps_output: &str) -> anyhow::Result<()>`; `in_runner` runs
// the first and refuses with the second before `compose run`.

#[test]
fn the_busy_check_lists_the_stacks_running_runner_containers() {
    let args = super::commands::runner_ps_args("chelabase-feat-x");
    let has_pair = |flag: &str, value: &str| args.windows(2).any(|w| w[0] == flag && w[1] == value);
    assert_eq!(args.first().map(String::as_str), Some("ps"), "{args:?}");
    assert!(
        has_pair(
            "--filter",
            "label=com.docker.compose.project=chelabase-feat-x"
        ),
        "{args:?}"
    );
    assert!(
        has_pair("--filter", "label=com.docker.compose.service=runner"),
        "{args:?}"
    );
    assert!(has_pair("--format", "{{.Names}}"), "{args:?}");
    // Running only (no `-a`): a finished run's container is gone (`--rm`) or not in the way.
    assert!(!args.iter().any(|a| a == "-a" || a == "--all"), "{args:?}");
}

#[test]
fn a_second_run_on_a_busy_stack_fails_clearly_before_docker_does() {
    // Nothing running: go.
    assert!(super::commands::busy_runner("feat-x", "").is_ok());
    assert!(super::commands::busy_runner("feat-x", "\n  \n").is_ok());
    // One running: refused, naming the stack and the container, saying why and what to do.
    let err = super::commands::busy_runner("feat-x", "chelabase-feat-x-runner-run-3f2a1b\n")
        .expect_err("a second run must be refused while the stack's runner is busy");
    let message = format!("{err:#}");
    for want in [
        "feat-x",
        "chelabase-feat-x-runner-run-3f2a1b",
        "one",
        "wait",
    ] {
        assert!(message.contains(want), "missing {want:?}: {message}");
    }
    assert!(
        !message.contains("Address already in use"),
        "the message explains the limit itself: {message}"
    );
}
