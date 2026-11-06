//! Carried gaps T4: the NATS rights of the tenant-attached events (P9, R10), and R9 (`up`
//! restarts NATS when its rendered `auth.conf` changed: the single-file bind mount keeps the old
//! inode, so a running NATS never sees a rewritten file).

use super::service_auth::write_auth_files;
use crate::dev::stack_secrets;
use std::collections::BTreeSet;
use std::path::Path;

const TENANT: &str = "0123456789abcdef0123456789abcdef";

fn attached() -> String {
    format!("chelabase.neon.attached.{TENANT}")
}

/// A consumer create as async-nats sends it: `…CREATE.<stream>.<name>.<filter>`.
fn filtered_create() -> String {
    format!(
        "$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS.attached-{TENANT}-0a1b2c3d.{}",
        attached()
    )
}

/// The rendered auth.conf of a fresh stack.
fn rendered() -> String {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    stack_secrets::ensure(&keys).unwrap();
    write_auth_files(&keys, &state).unwrap();
    std::fs::read_to_string(state.join("nats/auth.conf")).unwrap()
}

/// `user`'s block in a rendered auth.conf.
fn block<'a>(text: &'a str, user: &str) -> &'a str {
    let at = text
        .find(&format!("user: \"{user}\""))
        .unwrap_or_else(|| panic!("no {user}"));
    let block = &text[at..];
    &block[..block.find("\n    }\n").unwrap()]
}

/// The quoted items of the `[...]` list that follows `key` on `kind`'s line (`publish` or
/// `subscribe`; `key` `allow` or `deny`). Empty when the line has no such list.
fn list(text: &str, user: &str, kind: &str, key: &str) -> Vec<String> {
    let line = block(text, user)
        .lines()
        .find(|l| l.trim_start().starts_with(&format!("{kind}: {{")))
        .unwrap_or_else(|| panic!("{user}: no {kind} line"));
    let Some(at) = line.find(&format!("{key}: [")) else {
        return Vec::new();
    };
    let rest = &line[at + key.len() + 3..];
    let inner = &rest[..rest.find(']').unwrap()];
    inner
        .split(", ")
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_matches('"').to_string())
        .collect()
}

/// Whether a NATS permission subject matches a concrete subject (`*` one token, `>` the rest).
fn matches(pattern: &str, subject: &str) -> bool {
    let (p, s): (Vec<&str>, Vec<&str>) =
        (pattern.split('.').collect(), subject.split('.').collect());
    for (i, token) in p.iter().enumerate() {
        if *token == ">" {
            return s.len() > i;
        }
        match s.get(i) {
            Some(t) if *token == "*" || token == t => {}
            _ => return false,
        }
    }
    p.len() == s.len()
}

#[test]
fn the_matcher_follows_nats_wildcards() {
    assert!(matches("chelabase.*", "chelabase.provision"));
    assert!(!matches("chelabase.*", &attached()));
    assert!(matches("chelabase.neon.attached.*", &attached()));
    assert!(!matches(
        "chelabase.neon.attached.*",
        "chelabase.neon.attached"
    ));
    assert!(matches("$JS.API.CONSUMER.>", &filtered_create()));
    assert!(!matches(
        "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.>",
        &filtered_create()
    ));
}

/// Whether `user` may publish `subject`: an allow entry matches and no deny entry does.
fn may_publish(text: &str, user: &str, subject: &str) -> bool {
    list(text, user, "publish", "allow")
        .iter()
        .any(|p| matches(p, subject))
        && !list(text, user, "publish", "deny")
            .iter()
            .any(|p| matches(p, subject))
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn as_set(items: Vec<String>) -> BTreeSet<String> {
    items.into_iter().collect()
}

// P9: the orchestrator publishes the events and creates or updates their stream; nothing else
// changes in its rights.
#[test]
fn the_orchestrator_gets_exactly_the_events_publish_and_stream_rights() {
    let text = rendered();
    assert_eq!(
        as_set(list(&text, "orchestrator", "publish", "allow")),
        set(&[
            "chelabase.*",
            "chelabase.rpc.>",
            "chelabase.resync.branch",
            "chelabase.member_roles.reconcile",
            "chelabase.rt.project.>",
            "chelabase.rt.access.>",
            // Object storage m9: its cache events.
            "chelabase.storage.>",
            "$JS.API.STREAM.INFO.CHELABASE_CONTROL",
            "$JS.API.STREAM.CREATE.CHELABASE_CONTROL",
            "$JS.API.STREAM.UPDATE.CHELABASE_CONTROL",
            "chelabase.neon.attached.*",
            "$JS.API.STREAM.INFO.CHELABASE_NEON_EVENTS",
            "$JS.API.STREAM.CREATE.CHELABASE_NEON_EVENTS",
            "$JS.API.STREAM.UPDATE.CHELABASE_NEON_EVENTS",
        ])
    );
    assert!(list(&text, "orchestrator", "publish", "deny").is_empty());
    assert_eq!(
        list(&text, "orchestrator", "subscribe", "allow"),
        ["_INBOX_orchestrator.>"]
    );
    assert!(!may_publish(&text, "orchestrator", &filtered_create()));
}

// P9: the worker reads the events through consumers filtered to one tenant (create, pull,
// delete); it never publishes an attach event and never touches the stream itself (R10).
#[test]
fn the_worker_gets_exactly_the_filtered_consumer_rights() {
    let text = rendered();
    assert_eq!(
        as_set(list(&text, "worker", "publish", "allow")),
        set(&[
            "chelabase.*",
            "chelabase.resync.branch",
            "chelabase.member_roles.reconcile",
            "chelabase.rt.project.>",
            "chelabase.rt.compute.stopping.>",
            "$JS.API.STREAM.INFO.CHELABASE_CONTROL",
            "$JS.API.STREAM.CREATE.CHELABASE_CONTROL",
            "$JS.API.STREAM.UPDATE.CHELABASE_CONTROL",
            "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL",
            "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.>",
            "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_CONTROL.>",
            "$JS.API.CONSUMER.INFO.CHELABASE_CONTROL.>",
            "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_CONTROL.>",
            "$JS.ACK.CHELABASE_CONTROL.>",
            "_INBOX_orchestrator.>",
            "_INBOX_runner.>",
            "$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS.*.chelabase.neon.attached.*",
            "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_NEON_EVENTS.*",
            "$JS.API.CONSUMER.DELETE.CHELABASE_NEON_EVENTS.*",
        ])
    );
    assert_eq!(
        as_set(list(&text, "worker", "subscribe", "allow")),
        set(&[
            "chelabase.rpc.>",
            "chelabase.resync.branch",
            "chelabase.member_roles.reconcile",
            "_INBOX_worker.>",
        ])
    );
    // What it may and may not send, whatever entry would allow it.
    assert!(may_publish(&text, "worker", &filtered_create()));
    assert!(may_publish(
        &text,
        "worker",
        &format!("$JS.API.CONSUMER.MSG.NEXT.CHELABASE_NEON_EVENTS.attached-{TENANT}-0a1b2c3d")
    ));
    assert!(may_publish(
        &text,
        "worker",
        &format!("$JS.API.CONSUMER.DELETE.CHELABASE_NEON_EVENTS.attached-{TENANT}-0a1b2c3d")
    ));
    for refused in [
        attached(),
        "$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS.attached-x".to_string(),
        "$JS.API.CONSUMER.CREATE.CHELABASE_NEON_EVENTS.attached-x.chelabase.neon.other.x"
            .to_string(),
        "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_NEON_EVENTS.attached-x".to_string(),
        "$JS.API.STREAM.INFO.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.STREAM.CREATE.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.STREAM.UPDATE.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.STREAM.DELETE.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.STREAM.PURGE.CHELABASE_NEON_EVENTS".to_string(),
    ] {
        assert!(
            !may_publish(&text, "worker", &refused),
            "the worker may publish {refused}"
        );
    }
}

#[test]
fn realtime_gets_nothing_on_the_events() {
    let text = rendered();
    for kind in ["publish", "subscribe"] {
        for item in list(&text, "realtime", kind, "allow") {
            assert!(
                !item.contains("NEON_EVENTS") && !item.contains("chelabase.neon"),
                "realtime {kind}: {item}"
            );
        }
    }
    for refused in [attached(), filtered_create()] {
        assert!(
            !may_publish(&text, "realtime", &refused),
            "realtime may publish {refused}"
        );
    }
}

// P9: the runner runs both services in-process, so it has their union; it still can't delete
// or purge the events stream.
#[test]
fn the_runner_has_the_union_but_cannot_delete_or_purge_the_events_stream() {
    let text = rendered();
    let deny = list(&text, "runner", "publish", "deny");
    assert!(
        deny.contains(&"$JS.API.STREAM.DELETE.CHELABASE_NEON_EVENTS".to_string()),
        "{deny:?}"
    );
    // Nothing that was denied before is allowed now.
    for kept in [
        "$JS.API.STREAM.DELETE.CHELABASE_CONTROL",
        "$JS.API.STREAM.DELETE.KV_CHELABASE_RT_LEASES",
        "$JS.API.STREAM.DELETE.KV_CHELABASE_RT_SLOTS",
        "$JS.API.STREAM.PURGE.>",
        "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL",
        "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.>",
        "$JS.API.CONSUMER.DURABLE.CREATE.CHELABASE_CONTROL.>",
        "$JS.API.CONSUMER.MSG.NEXT.CHELABASE_CONTROL.>",
        "$JS.API.CONSUMER.DELETE.CHELABASE_CONTROL.>",
        "$JS.ACK.CHELABASE_CONTROL.>",
    ] {
        assert!(
            deny.contains(&kept.to_string()),
            "{kept} is no longer denied: {deny:?}"
        );
    }
    for refused in [
        "$JS.API.STREAM.DELETE.CHELABASE_NEON_EVENTS",
        "$JS.API.STREAM.PURGE.CHELABASE_NEON_EVENTS",
    ] {
        assert!(
            !may_publish(&text, "runner", refused),
            "the runner may publish {refused}"
        );
    }
    for allowed in [
        attached(),
        filtered_create(),
        format!("$JS.API.CONSUMER.MSG.NEXT.CHELABASE_NEON_EVENTS.attached-{TENANT}-0a1b2c3d"),
        format!("$JS.API.CONSUMER.DELETE.CHELABASE_NEON_EVENTS.attached-{TENANT}-0a1b2c3d"),
        "$JS.API.STREAM.INFO.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.STREAM.CREATE.CHELABASE_NEON_EVENTS".to_string(),
        "$JS.API.STREAM.UPDATE.CHELABASE_NEON_EVENTS".to_string(),
    ] {
        assert!(
            may_publish(&text, "runner", &allowed),
            "the runner may not publish {allowed}"
        );
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// R9: `write_auth_files` says whether `nats/auth.conf`'s contents changed (created, rewritten);
// a re-render with the same passwords, a mode fix or a Valkey-only change is not a change.
#[test]
fn write_auth_files_reports_whether_the_nats_file_changed() {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    let auth = state.join("nats/auth.conf");
    stack_secrets::ensure(&keys).unwrap();

    assert!(
        write_auth_files(&keys, &state).unwrap().nats,
        "the first render creates it"
    );
    assert!(
        !write_auth_files(&keys, &state).unwrap().nats,
        "an unchanged re-render"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A loose mode is refused, never silently tightened (selfhost-install m1).
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(write_auth_files(&keys, &state).is_err(), "a loose auth.conf is refused");
        assert_eq!(mode(&auth), 0o644);
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!write_auth_files(&keys, &state).unwrap().nats);
    }

    // Valkey's file changes; NATS's doesn't.
    std::fs::remove_file(keys.join("valkey/realtime.password")).unwrap();
    stack_secrets::ensure(&keys).unwrap();
    assert!(
        !write_auth_files(&keys, &state).unwrap().nats,
        "only users.acl changed"
    );

    // A new NATS password: a new hash in auth.conf.
    std::fs::remove_file(keys.join("nats/worker.password")).unwrap();
    stack_secrets::ensure(&keys).unwrap();
    assert!(
        write_auth_files(&keys, &state).unwrap().nats,
        "a NATS password changed"
    );
    assert!(!write_auth_files(&keys, &state).unwrap().nats);

    // A file edited by hand (or an older render with other permissions) is rewritten.
    std::fs::write(&auth, "authorization { users: [] }\n").unwrap();
    assert!(
        write_auth_files(&keys, &state).unwrap().nats,
        "a stale file was rewritten"
    );
    assert!(!write_auth_files(&keys, &state).unwrap().nats);
}

// R9: on a running stack a changed auth.conf restarts NATS (the running server keeps reading the
// old inode); an unchanged one doesn't, and a stack that wasn't running starts NATS fresh anyway.
#[test]
fn up_restarts_nats_only_for_a_changed_auth_file_on_a_running_stack() {
    use super::commands::nats_needs_restart;
    assert!(nats_needs_restart(true, true));
    assert!(!nats_needs_restart(true, false));
    assert!(!nats_needs_restart(false, true));
    assert!(!nats_needs_restart(false, false));
}

// hardening-b: `write_auth_files` reports each file separately; a Valkey-only change is
// valkey true / nats false, and the other way round.
#[test]
fn write_auth_files_reports_the_valkey_file_separately() {
    let t = tempfile::tempdir().unwrap();
    let (keys, state) = (t.path().join("keys"), t.path().join("state"));
    stack_secrets::ensure(&keys).unwrap();

    let first = write_auth_files(&keys, &state).unwrap();
    assert!(first.nats && first.valkey, "the first render creates both");
    let again = write_auth_files(&keys, &state).unwrap();
    assert!(!again.nats && !again.valkey, "an unchanged re-render");

    std::fs::remove_file(keys.join("valkey/realtime.password")).unwrap();
    stack_secrets::ensure(&keys).unwrap();
    let c = write_auth_files(&keys, &state).unwrap();
    assert!(c.valkey && !c.nats, "only users.acl changed");
    let c = write_auth_files(&keys, &state).unwrap();
    assert!(!c.valkey && !c.nats);

    std::fs::remove_file(keys.join("nats/worker.password")).unwrap();
    stack_secrets::ensure(&keys).unwrap();
    let c = write_auth_files(&keys, &state).unwrap();
    assert!(c.nats && !c.valkey, "only auth.conf changed");

    // A hand-edited ACL file is rewritten.
    std::fs::write(state.join("valkey/users.acl"), "user default off\n").unwrap();
    let c = write_auth_files(&keys, &state).unwrap();
    assert!(c.valkey && !c.nats, "a stale users.acl was rewritten");
}

// hardening-b: on a running stack a changed users.acl restarts Valkey (single-file bind mount
// keeps the old inode); an unchanged one doesn't, and a stack that wasn't running starts fresh.
#[test]
fn up_restarts_valkey_only_for_a_changed_acl_on_a_running_stack() {
    use super::commands::valkey_needs_restart;
    assert!(valkey_needs_restart(true, true));
    assert!(!valkey_needs_restart(true, false));
    assert!(!valkey_needs_restart(false, true));
    assert!(!valkey_needs_restart(false, false));
}

// Object storage m9: storage's NATS user only listens, to exactly its cache events; it can
// publish nothing (no allow list, deny everything), so it can neither send jobs nor answer.
#[test]
fn storage_only_listens_to_its_cache_events() {
    let text = rendered();
    assert!(list(&text, "storage", "publish", "allow").is_empty());
    assert_eq!(as_set(list(&text, "storage", "publish", "deny")), set(&[">"]));
    for subject in ["chelabase.storage.events", "chelabase.provision", "chelabase.rpc.check_data_role", "_INBOX_storage.x"] {
        assert!(!may_publish(&text, "storage", subject), "{subject}");
    }
    assert_eq!(
        as_set(list(&text, "storage", "subscribe", "allow")),
        set(&["chelabase.rt.project.>", "chelabase.rt.access.>", "chelabase.storage.>", "_INBOX_storage.>"])
    );
    // Only the orchestrator (and the runner, as it) publishes the storage events.
    for user in ["worker", "realtime"] {
        assert!(!may_publish(&text, user, "chelabase.storage.events"), "{user}");
    }
    assert!(may_publish(&text, "orchestrator", "chelabase.storage.events"));
}
