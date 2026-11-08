//! W2 core: slug, registry, slots, stack env and argv (spec section 1).

use super::argv::{Compose, nextest_archive_args, nextest_run_args};
use super::env::{StackEnvInput, control_net, render_stack_env};
use super::registry::{Entry, MAIN_GATEWAY_PORT, MAIN_PROXY_PORT, Registry, with_registry};
use super::slots::{
    SlotOptions, acquire_slot, cap_from_env, reap, release_slot, ticket_name, wait_from_env,
};
use super::slug::{MAIN, hosts, project_name, slug_for_dir};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

// --- slug -----------------------------------------------------------------------------------

#[test]
fn a_worktree_dir_becomes_its_slug() {
    assert_eq!(slug_for_dir("workbench-isolation").unwrap(), "workbench-isolation");
    assert_eq!(slug_for_dir("Feat_X.Y").unwrap(), "feat-x-y");
    assert_eq!(slug_for_dir("--a__b--").unwrap(), "a-b");
    assert_eq!(slug_for_dir("main").unwrap(), "main-wt", "main is the main checkout's");
    assert!(slug_for_dir("___").is_err());
    assert!(slug_for_dir("").is_err());
}

#[test]
fn a_long_dir_is_cut_with_a_hash_of_its_full_name() {
    let name = "members-and-roles-with-a-very-long-branch-name-t3";
    let slug = slug_for_dir(name).unwrap();
    let hash = hex::encode(Sha256::digest(name.as_bytes()));
    assert_eq!(slug, format!("members-and-roles-with-a-very-lon-{}", &hash[..6]));
    assert_eq!(slug.len(), 40);
    let other = slug_for_dir("members-and-roles-with-a-very-long-branch-name-t4").unwrap();
    assert_ne!(slug, other);
    assert_eq!(slug_for_dir(name).unwrap(), slug, "stable");
}

#[test]
fn a_cut_never_leaves_a_double_dash() {
    // the 33rd character is a dash
    let name = "abcdefghij-abcdefghij-abcdefghij-abcdefghij";
    let slug = slug_for_dir(name).unwrap();
    assert!(!slug.contains("--"), "{slug}");
    assert!(slug.starts_with("abcdefghij-abcdefghij-abcdefghij-"));
    assert!(slug.len() <= 40);
}

#[test]
fn slugs_are_dns_labels() {
    for name in ["A b c", "x/y/z", "ÜBER-feature", "1-2-3", "t3"] {
        let slug = slug_for_dir(name).unwrap();
        assert!(
            slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name} -> {slug}"
        );
        assert!(!slug.starts_with('-') && !slug.ends_with('-'), "{name} -> {slug}");
        assert!(slug.len() <= 40);
    }
}

#[test]
fn main_keeps_todays_names_and_ports() {
    assert_eq!(project_name(MAIN), "chelabase");
    let h = hosts(MAIN, 8000);
    assert_eq!(h.studio, "studio.chelabase.localhost:8000");
    assert_eq!(h.api, "api.chelabase.localhost:8000");
    assert_eq!(h.auth, "auth.chelabase.localhost:8000");
    assert_eq!(h.mail, "mail.chelabase.localhost:8000");
    assert_eq!(h.docs, "docs.chelabase.localhost:8000");
    assert_eq!(h.landing, "landing.chelabase.localhost:8000");
}

#[test]
fn a_branch_stack_puts_its_slug_in_every_host() {
    assert_eq!(project_name("feat-x"), "chelabase-feat-x");
    let h = hosts("feat-x", 8137);
    assert_eq!(h.studio, "studio.feat-x.chelabase.localhost:8137");
    assert_eq!(h.api, "api.feat-x.chelabase.localhost:8137");
    assert_eq!(h.auth, "auth.feat-x.chelabase.localhost:8137");
    assert_eq!(
        h.names(),
        [
            "studio.feat-x.chelabase.localhost",
            "api.feat-x.chelabase.localhost",
            "auth.feat-x.chelabase.localhost",
            "mail.feat-x.chelabase.localhost",
            "docs.feat-x.chelabase.localhost",
            "landing.feat-x.chelabase.localhost",
        ]
    );
}

// --- registry -------------------------------------------------------------------------------

fn entry(slug: &str, gw: u16, pg: u16, running: Option<u64>) -> Entry {
    Entry {
        slug: slug.to_string(),
        checkout: PathBuf::from(format!("/w/{slug}")),
        gateway_port: gw,
        proxy_port: pg,
        created_at: 1,
        running_since: running,
        control_slot: None,
    }
}

#[test]
fn main_gets_the_fixed_ports_without_a_probe() {
    assert_eq!((MAIN_GATEWAY_PORT, MAIN_PROXY_PORT), (8000, 4432));
    let mut r = Registry::default();
    let e = r.ensure(MAIN, Path::new("/m"), 10, |_| false).unwrap();
    assert_eq!((e.gateway_port, e.proxy_port), (8000, 4432));
    assert_eq!(e.checkout, PathBuf::from("/m"));
}

#[test]
fn a_branch_gets_the_lowest_free_ports_and_keeps_them() {
    let mut r = Registry::default();
    r.stacks.push(entry("a", 8100, 4500, None));
    let busy_on_host = [8101u16, 4501];
    let e = r
        .ensure("b", Path::new("/w/b"), 10, |p| !busy_on_host.contains(&p))
        .unwrap();
    assert_eq!((e.gateway_port, e.proxy_port), (8102, 4502));
    assert_eq!(e.created_at, 10);
    assert_eq!(e.running_since, None);
    let again = r.ensure("b", Path::new("/w/b"), 99, |_| false).unwrap();
    assert_eq!(again, e, "kept, and no probe for a known stack (it may be running)");
    assert_eq!(r.stacks.len(), 2);
}

#[test]
fn gateway_and_proxy_ports_are_chosen_independently() {
    let mut r = Registry::default();
    let e = r.ensure("b", Path::new("/w/b"), 1, |p| p != 8100).unwrap();
    assert_eq!((e.gateway_port, e.proxy_port), (8101, 4500));
}

#[test]
fn ports_run_out_with_a_clear_error() {
    let mut r = Registry::default();
    let err = r
        .ensure("b", Path::new("/w/b"), 1, |_| false)
        .unwrap_err()
        .to_string();
    assert!(err.contains("8100") && err.contains("8199"), "{err}");
    assert!(r.stacks.is_empty(), "nothing registered on failure");
}

#[test]
fn the_registry_file_round_trips_under_its_lock() {
    let dir = tempfile::tempdir().unwrap();
    with_registry(dir.path(), |r| {
        r.stacks.push(entry("a", 8100, 4500, Some(5)));
        Ok(())
    })
    .unwrap();
    let read = with_registry(dir.path(), |r| Ok(r.clone())).unwrap();
    assert_eq!(read.stacks, vec![entry("a", 8100, 4500, Some(5))]);
    assert!(dir.path().join("registry.json").is_file());
    assert!(dir.path().join("registry.lock").is_file());
}

#[test]
fn a_failed_update_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    with_registry(dir.path(), |r| {
        r.stacks.push(entry("a", 8100, 4500, None));
        Ok(())
    })
    .unwrap();
    let res: anyhow::Result<()> = with_registry(dir.path(), |r| {
        r.stacks.clear();
        anyhow::bail!("no")
    });
    assert!(res.is_err());
    assert_eq!(with_registry(dir.path(), |r| Ok(r.stacks.len())).unwrap(), 1);
}

#[test]
fn a_corrupt_registry_is_an_error_not_a_reset() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("registry.json"), "{not json").unwrap();
    let err = with_registry(dir.path(), |r| Ok(r.stacks.len()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("registry.json"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("registry.json")).unwrap(),
        "{not json"
    );
}

#[test]
fn concurrent_writers_never_lose_an_update() {
    let dir = tempfile::tempdir().unwrap();
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let d = dir.path().to_path_buf();
            std::thread::spawn(move || {
                for j in 0..10 {
                    with_registry(&d, |r| {
                        r.stacks.push(entry(&format!("s{i}-{j}"), 0, 0, None));
                        Ok(())
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(with_registry(dir.path(), |r| Ok(r.stacks.len())).unwrap(), 80);
}

// --- slots ----------------------------------------------------------------------------------

fn opts(cap: usize, timeout_ms: u64) -> SlotOptions {
    SlotOptions {
        cap,
        timeout: Duration::from_millis(timeout_ms),
        poll: Duration::from_millis(20),
        status_every: Duration::from_millis(50),
    }
}

fn seed(dir: &Path, entries: Vec<Entry>) {
    with_registry(dir, |r| {
        r.stacks = entries;
        Ok(())
    })
    .unwrap();
}

fn running_slugs(dir: &Path) -> Vec<String> {
    with_registry(dir, |r| Ok(r.running().map(|e| e.slug.clone()).collect())).unwrap()
}

fn queue_len(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("queue")).map(|d| d.count()).unwrap_or(0)
}

#[test]
fn a_free_slot_is_taken_at_once() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("main", 8000, 4432, Some(1)), entry("b", 8100, 4500, None)]);
    acquire_slot(dir.path(), "b", &opts(4, 1000), &|_| true, &|| 50, &mut |_| {}).unwrap();
    assert_eq!(running_slugs(dir.path()), ["main", "b"]);
    let b = with_registry(dir.path(), |r| Ok(r.get("b").cloned())).unwrap().unwrap();
    assert_eq!(b.running_since, Some(50));
    assert_eq!(queue_len(dir.path()), 0, "no ticket left behind");
}

#[test]
fn an_already_running_stack_takes_no_second_slot() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("a", 8100, 4500, Some(7))]);
    acquire_slot(dir.path(), "a", &opts(1, 100), &|_| true, &|| 50, &mut |_| {}).unwrap();
    let a = with_registry(dir.path(), |r| Ok(r.get("a").cloned())).unwrap().unwrap();
    assert_eq!(a.running_since, Some(7), "unchanged");
}

#[test]
fn a_full_house_times_out_naming_the_running_stacks() {
    let dir = tempfile::tempdir().unwrap();
    seed(
        dir.path(),
        vec![
            entry("main", 8000, 4432, Some(0)),
            entry("alpha", 8100, 4500, Some(0)),
            entry("b", 8101, 4501, None),
        ],
    );
    let mut lines = Vec::new();
    let err = acquire_slot(dir.path(), "b", &opts(2, 200), &|_| true, &|| 3_700, &mut |l| {
        lines.push(l.to_string())
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("main") && err.contains("alpha"), "{err}");
    assert!(err.contains("1h 1m"), "each stack's uptime: {err}");
    assert!(err.contains("just workbench down --stack"), "{err}");
    assert!(!lines.is_empty(), "status lines while waiting");
    assert_eq!(running_slugs(dir.path()), ["main", "alpha"]);
    assert_eq!(queue_len(dir.path()), 0, "the ticket goes on timeout");
}

#[test]
fn a_slot_freed_while_waiting_is_taken() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("a", 8100, 4500, Some(0)), entry("b", 8101, 4501, None)]);
    let d = dir.path().to_path_buf();
    let freer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        release_slot(&d, "a").unwrap();
    });
    acquire_slot(dir.path(), "b", &opts(1, 5000), &|_| true, &|| 9, &mut |_| {}).unwrap();
    freer.join().unwrap();
    assert_eq!(running_slugs(dir.path()), ["b"]);
}

#[test]
fn the_oldest_live_ticket_goes_first() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    std::fs::create_dir_all(dir.path().join("queue")).unwrap();
    // b queued earlier, by a live process (this one)
    std::fs::write(
        dir.path().join("queue").join(ticket_name(1, "b")),
        std::process::id().to_string(),
    )
    .unwrap();
    let err = acquire_slot(dir.path(), "c", &opts(1, 150), &|_| true, &|| 9, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains('b'), "names who is ahead: {err}");
    assert!(running_slugs(dir.path()).is_empty(), "c never jumped the queue");
    assert!(dir.path().join("queue").join(ticket_name(1, "b")).exists());
}

#[test]
fn a_ticket_of_a_dead_process_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    std::fs::create_dir_all(dir.path().join("queue")).unwrap();
    let dead = dir.path().join("queue").join(ticket_name(1, "b"));
    std::fs::write(&dead, "999999999").unwrap();
    acquire_slot(dir.path(), "c", &opts(1, 1000), &|_| true, &|| 9, &mut |_| {}).unwrap();
    assert_eq!(running_slugs(dir.path()), ["c"]);
    assert!(!dead.exists());
}

#[test]
fn concurrent_waiters_never_exceed_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..6).map(|i| format!("s{i}")).collect();
    seed(
        dir.path(),
        names.iter().enumerate().map(|(i, n)| entry(n, 8100 + i as u16, 4500 + i as u16, None)).collect(),
    );
    let threads: Vec<_> = names
        .iter()
        .map(|n| {
            let d = dir.path().to_path_buf();
            let n = n.clone();
            std::thread::spawn(move || {
                acquire_slot(&d, &n, &opts(2, 400), &|_| true, &|| 9, &mut |_| {}).is_ok()
            })
        })
        .collect();
    let taken = threads.into_iter().map(|t| t.join().unwrap()).filter(|ok| *ok).count();
    assert_eq!(taken, 2);
    assert_eq!(running_slugs(dir.path()).len(), 2);
    assert_eq!(queue_len(dir.path()), 0);
}

#[test]
fn dead_stacks_give_their_slot_back() {
    let mut r = Registry::default();
    r.stacks = vec![entry("main", 8000, 4432, Some(1)), entry("gone", 8100, 4500, Some(1))];
    assert_eq!(reap(&mut r, |e| e.slug != "gone"), ["gone"]);
    assert_eq!(r.get("gone").unwrap().running_since, None);
    assert_eq!(r.get("main").unwrap().running_since, Some(1));
    assert!(reap(&mut r, |e| e.slug != "gone").is_empty(), "a stopped stack isn't reaped twice");
}

#[test]
fn acquire_reaps_before_counting() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("gone", 8100, 4500, Some(1)), entry("b", 8101, 4501, None)]);
    acquire_slot(dir.path(), "b", &opts(1, 500), &|e| e.slug != "gone", &|| 9, &mut |_| {})
        .unwrap();
    assert_eq!(running_slugs(dir.path()), ["b"]);
}

#[test]
fn an_unregistered_stack_cannot_take_a_slot() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![]);
    assert!(acquire_slot(dir.path(), "x", &opts(4, 100), &|_| true, &|| 9, &mut |_| {}).is_err());
}

#[test]
fn slot_settings_from_the_env() {
    assert_eq!(cap_from_env(None).unwrap(), 4);
    assert_eq!(cap_from_env(Some("2")).unwrap(), 2);
    assert!(cap_from_env(Some("0")).is_err());
    assert!(cap_from_env(Some("x")).is_err());
    assert_eq!(wait_from_env(None).unwrap(), Duration::from_secs(45 * 60));
    assert_eq!(wait_from_env(Some("300")).unwrap(), Duration::from_secs(300));
    assert_eq!(wait_from_env(Some("10m")).unwrap(), Duration::from_secs(600));
    assert!(wait_from_env(Some("soon")).is_err());
}

#[test]
fn tickets_sort_by_time() {
    assert!(ticket_name(9, "z") < ticket_name(10, "a"));
    assert!(ticket_name(1, "b").ends_with("-b"));
}

// --- stack env ------------------------------------------------------------------------------

const EXAMPLE: &str = "# The workbench defaults\n\
CHELABASE_METADATA_DATABASE_URL=postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata\n\
CHELABASE_MODE=self-hosted\n\
CHELABASE_PUBLIC_SCHEME=http\n\
CHELABASE_STUDIO_HOST=studio.chelabase.localhost:8000\n\
CHELABASE_GATEWAY_LISTEN=127.0.0.1:8000,[::1]:8000\n\
CHELABASE_APP_AUTH_INTERNAL_TOKEN=chelabase-dev-app-auth\n";

fn input<'a>() -> StackEnvInput<'a> {
    StackEnvInput {
        slug: "feat-x",
        project: "chelabase-feat-x",
        checkout: Path::new("/m/.worktrees/feat-x"),
        main: Path::new("/m"),
        state: Path::new("/m/.chelabase/stacks/feat-x"),
        gateway_port: 8137,
        proxy_port: 4537,
        uid: 1000,
        gid: 1001,
        docker_gid: 973,
        image: "chelabase-workbench:abc123def456",
        extra: &[],
        control_slot: 1,
    }
}

fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect()
}

#[test]
fn the_stack_env_keeps_the_example_and_overlays_the_stack() {
    let env = parse(&render_stack_env(EXAMPLE, &input()));
    assert_eq!(env["CHELABASE_MODE"], "self-hosted");
    assert_eq!(env["CHELABASE_APP_AUTH_INTERNAL_TOKEN"], "chelabase-dev-app-auth");
    let expect = [
        ("CHELABASE_STACK", "feat-x"),
        ("CHELABASE_COMPOSE_PROJECT", "chelabase-feat-x"),
        ("CHELABASE_STACK_DIR", "/m/.chelabase/stacks/feat-x"),
        ("CHELABASE_CHECKOUT", "/m/.worktrees/feat-x"),
        ("CHELABASE_MAIN_CHECKOUT", "/m"),
        ("CHELABASE_KEYS_DIR", "/m/.chelabase"),
        ("CHELABASE_UID", "1000"),
        ("CHELABASE_GID", "1001"),
        ("CHELABASE_DOCKER_GID", "973"),
        ("CHELABASE_WORKBENCH_IMAGE", "chelabase-workbench:abc123def456"),
        ("CHELABASE_GATEWAY_PORT", "8137"),
        ("CHELABASE_PROXY_PORT", "4537"),
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_STUDIO_HOST", "studio.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_API_HOST", "api.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_AUTH_HOST", "auth.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_MAIL_HOST", "mail.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_DOCS_HOST", "docs.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_LANDING_HOST", "landing.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_GATEWAY_LISTEN", "0.0.0.0:8137"),
        ("CHELABASE_AUTH_HOST_NAME", "auth.feat-x.chelabase.localhost"),
        ("CHELABASE_STUDIO_HOST_NAME", "studio.feat-x.chelabase.localhost"),
        ("CHELABASE_API_HOST_NAME", "api.feat-x.chelabase.localhost"),
        ("CHELABASE_MAIL_HOST_NAME", "mail.feat-x.chelabase.localhost"),
        ("CHELABASE_DOCS_HOST_NAME", "docs.feat-x.chelabase.localhost"),
        ("CHELABASE_LANDING_HOST_NAME", "landing.feat-x.chelabase.localhost"),
        ("CHELABASE_PUBLIC_PORT", "8137"),
        ("CHELABASE_PUBLIC_SECURE", "false"),
        ("CHELABASE_ENTRY_STUDIO_URL", "http://studio.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_API_URL", "http://api.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_AUTH_URL", "http://auth.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_MAIL_URL", "http://mail.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_DOCS_URL", "http://docs.feat-x.chelabase.localhost:8137"),
        ("CHELABASE_ENTRY_LANDING_URL", "http://landing.feat-x.chelabase.localhost:8137"),
        (
            "CHELABASE_METADATA_DATABASE_URL",
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata",
        ),
        ("CHELABASE_NATS_URL", "nats://nats:4222"),
        ("CHELABASE_VALKEY_URL", "redis://valkey:6379"),
        ("CHELABASE_STORCON_URL", "http://storage-controller:1234"),
        // RT1 T1 (user): three safekeepers, so the storage controller runs without --dev.
        (
            "CHELABASE_SAFEKEEPER_HTTP",
            "1=http://safekeeper1:7676,2=http://safekeeper2:7676,3=http://safekeeper3:7676",
        ),
        ("CHELABASE_INTERNAL_URL", "http://orchestrator:8083"),
        ("CHELABASE_REST_INTERNAL_URL", "http://rest:8086"),
        ("CHELABASE_ORCHESTRATOR_URL", "http://orchestrator:8081"),
        ("CHELABASE_AUTH_URL", "http://auth:8084"),
        ("CHELABASE_REST_URL", "http://rest:8085"),
        ("CHELABASE_STUDIO_UPSTREAM", "http://studio:3000"),
        ("CHELABASE_MAIL_UPSTREAM", "http://mailpit:8025"),
        ("CHELABASE_DOCS_UPSTREAM", "http://docs:3001"),
        ("CHELABASE_LANDING_UPSTREAM", "http://landing:3002"),
        ("CHELABASE_ORCHESTRATOR_ADDR", "0.0.0.0:8081"),
        ("CHELABASE_WORKER_HEALTH_ADDR", "0.0.0.0:8082"),
        ("CHELABASE_INTERNAL_ADDR", "0.0.0.0:8083"),
        ("CHELABASE_AUTH_ADDR", "0.0.0.0:8084"),
        ("CHELABASE_REST_ADDR", "0.0.0.0:8085"),
        ("CHELABASE_REST_INTERNAL_ADDR", "0.0.0.0:8086"),
        // RT1 T1 ruling (e): only the gateway's forwarded headers are trusted.
        ("CHELABASE_AUTH_TRUSTED_PROXY", "gateway"),
        ("CHELABASE_CONTROL_PLANE_URI", "http://orchestrator:8083"),
        // RT1 T1 (P4/W14): computes get the stack's own network.
        ("CHELABASE_COMPUTE_NETWORK", "chelabase-feat-x_compute"),
        ("CHELABASE_BOOTSTRAP_DIR", "/m/.chelabase/stacks/feat-x/bootstrap"),
        ("CHELABASE_SECRETS_DIR", "/m/.chelabase/secrets"),
        ("CHELABASE_AUTH_DIR", "/m/.chelabase/auth"),
        ("CHELABASE_NEON_DIR", "/m/.chelabase/neon"),
        ("CHELABASE_GATEWAY_DIR", "/m/.chelabase/gateway"),
        ("CHELABASE_COMPUTE_JWT_DIR", "/m/.chelabase/neon"),
        ("CHELABASE_PROXY_CA_FILE", "/m/.chelabase/stacks/feat-x/neon/proxy-ca.crt"),
        ("CHELABASE_STUDIO_BIND", "0.0.0.0"),
        ("CHELABASE_DOCS_BIND", "0.0.0.0"),
        ("CHELABASE_LANDING_BIND", "0.0.0.0"),
        ("CHELABASE_CLI", "/m/.worktrees/feat-x/target/debug/chelabase"),
    ];
    for (k, v) in expect {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
}

#[test]
fn every_key_appears_once_and_extra_pairs_win() {
    let extra = [
        ("COMPUTE_NODE_IMAGE".to_string(), "img:1".to_string()),
        ("CHELABASE_MODE".to_string(), "platform".to_string()),
    ];
    let mut i = input();
    i.extra = &extra;
    let text = render_stack_env(EXAMPLE, &i);
    let keys: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| l.split_once('=').unwrap().0)
        .collect();
    let mut unique = keys.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(keys.len(), unique.len(), "no duplicate keys");
    let env = parse(&text);
    assert_eq!(env["COMPUTE_NODE_IMAGE"], "img:1");
    assert_eq!(env["CHELABASE_MODE"], "platform");
    assert!(!text.contains("127.0.0.1:5433"));
    assert!(text.starts_with("# Generated by chelabase workbench for feat-x"));
}

#[test]
fn the_example_order_is_kept() {
    let text = render_stack_env(EXAMPLE, &input());
    let pos = |k: &str| text.find(&format!("\n{k}=")).unwrap_or_else(|| panic!("{k}"));
    assert!(pos("CHELABASE_METADATA_DATABASE_URL") < pos("CHELABASE_MODE"));
    assert!(pos("CHELABASE_MODE") < pos("CHELABASE_APP_AUTH_INTERNAL_TOKEN"));
    assert!(pos("CHELABASE_APP_AUTH_INTERNAL_TOKEN") < pos("CHELABASE_STACK"));
}

#[test]
fn main_renders_todays_hosts_and_network() {
    let mut i = input();
    i.slug = "main";
    i.project = "chelabase";
    i.gateway_port = 8000;
    i.proxy_port = 4432;
    i.checkout = Path::new("/m");
    i.state = Path::new("/m/.chelabase/stacks/main");
    let env = parse(&render_stack_env(EXAMPLE, &i));
    assert_eq!(env["CHELABASE_STUDIO_HOST"], "studio.chelabase.localhost:8000");
    assert_eq!(env["CHELABASE_COMPUTE_NETWORK"], "chelabase_compute");
    assert_eq!(env["CHELABASE_PROXY_PORT"], "4432");
    assert_eq!(env["CHELABASE_CLI"], "/m/target/debug/chelabase");
}

#[test]
fn an_https_example_makes_a_secure_stack() {
    let example = EXAMPLE.replace("CHELABASE_PUBLIC_SCHEME=http", "CHELABASE_PUBLIC_SCHEME=");
    let env = parse(&render_stack_env(&example, &input()));
    assert_eq!(env["CHELABASE_PUBLIC_SCHEME"], "https");
    assert_eq!(env["CHELABASE_PUBLIC_SECURE"], "true");
    assert_eq!(
        env["CHELABASE_ENTRY_API_URL"],
        "https://api.feat-x.chelabase.localhost:8137"
    );
}

#[test]
fn values_are_written_literally() {
    let text = render_stack_env(EXAMPLE, &input());
    assert!(text.lines().all(|l| l.starts_with('#') || !l.contains('"')));
}

// --- argv -----------------------------------------------------------------------------------

#[test]
fn compose_always_names_its_project_file_and_env() {
    let c = Compose {
        project: "chelabase-feat-x",
        files: vec![
            Path::new("/w/infra/compose/docker-compose.yml"),
            Path::new("/w/infra/compose/docker-compose.workbench.yml"),
        ],
        env_file: Path::new("/s/env/stack.env"),
    };
    assert_eq!(
        c.args(&["ps", "-q"]),
        [
            "compose",
            "-p",
            "chelabase-feat-x",
            "-f",
            "/w/infra/compose/docker-compose.yml",
            "-f",
            "/w/infra/compose/docker-compose.workbench.yml",
            "--env-file",
            "/s/env/stack.env",
            "ps",
            "-q"
        ]
    );
}

/// m7: every workbench compose call names the base file and then the workbench overlay, both from
/// the stack's checkout, so no call ever renders the base file alone.
#[test]
fn compose_args_include_overlay() {
    use super::commands::{Stack, runner_command};
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("m");
    let stack = Stack::new("feat", &main.join(".worktrees/feat"), &main);
    std::fs::create_dir_all(stack.env_file.parent().unwrap()).unwrap();
    std::fs::write(&stack.env_file, "CHELABASE_PUBLIC_SCHEME=http\n").unwrap();
    let compose_dir = stack.checkout.join("infra/compose");
    let (base, overlay) = ("docker-compose.yml", "docker-compose.workbench.yml");
    let want = [base, overlay].map(|f| compose_dir.join(f).display().to_string());
    let files = |args: &[String]| -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == "-f")
            .map(|w| w[1].clone())
            .collect()
    };

    // The runner (`test`, `run`): the one builder every other call shares, through compose_cmd.
    let cmd = runner_command(&stack, &stack.checkout, &[], &["true".to_string()]).unwrap();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(files(&args), want, "{args:?}");
    // The files come before the subcommand, so they apply to it.
    let run_at = args.iter().position(|a| a == "run").unwrap();
    let last_f = args.iter().rposition(|a| a == "-f").unwrap();
    assert!(last_f < run_at, "{args:?}");

    // The struct itself: base, then overlay.
    let args = stack.compose().args(&["ps"]);
    assert_eq!(files(&args), want, "{args:?}");
}

#[test]
fn nextest_archives_on_the_host_and_runs_in_the_runner() {
    let archive = Path::new("/s/test/archive.tar.zst");
    assert_eq!(
        nextest_archive_args(&["-p".into(), "rest".into()], archive),
        ["nextest", "archive", "--archive-file", "/s/test/archive.tar.zst", "-p", "rest"]
    );
    assert_eq!(
        nextest_run_args(&["--run-ignored".into(), "only".into()], archive, Path::new("/w")),
        [
            "nextest",
            "run",
            "--archive-file",
            "/s/test/archive.tar.zst",
            "--workspace-remap",
            "/w",
            "--run-ignored",
            "only"
        ]
    );
}

#[test]
fn an_empty_selection_archives_the_workspace() {
    assert_eq!(
        nextest_archive_args(&[], Path::new("/a.tar.zst")),
        ["nextest", "archive", "--archive-file", "/a.tar.zst", "--workspace"]
    );
}

#[test]
fn the_stack_env_carries_the_control_network() {
    let env = parse(&render_stack_env(EXAMPLE, &input()));
    for (k, v) in [
        ("CHELABASE_COMPUTE_CONTROL_NETWORK", "chelabase-feat-x_control"),
        ("CHELABASE_CONTROL_SUBNET", "10.213.4.0/22"),
        ("CHELABASE_CONTROL_COMPUTE_RANGE", "10.213.6.0/23"),
        ("CHELABASE_ORCHESTRATOR_CONTROL_IP", "10.213.4.10"),
        ("CHELABASE_RUNNER_CONTROL_IP", "10.213.4.11"),
        ("CHELABASE_MEMBER_LOGIN_CIDRS", "10.213.4.10/32,10.213.4.11/32"),
        ("CHELABASE_COMPUTE_CA_DIR", "/m/.chelabase/compute-ca"),
        ("CHELABASE_COMPUTE_TLS_DIR", "/m/.chelabase/orchestrator-tls"),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
    let main = control_net(0);
    assert_eq!(
        (main.subnet.as_str(), main.compute_range.as_str(), main.orchestrator_ip.as_str(), main.runner_ip.as_str()),
        ("10.213.0.0/22", "10.213.2.0/23", "10.213.0.10", "10.213.0.11")
    );
    assert_eq!(control_net(63).subnet, "10.213.252.0/22");
}

// U15 (f5b). Invariant: every connection Chelabase opens to a compute uses TLS verified against
// the platform CA. The auth and rest containers get the CA certificate alone (`ca.crt`, mounted
// read-only; the directory also holds `ca.key`, which only the worker and the runner see), so
// the stack env names the file, not the directory: `CHELABASE_COMPUTE_CA_FILE`.
#[test]
fn the_stack_env_names_the_ca_certificate_file_for_auth_and_rest() {
    let env = parse(&render_stack_env(EXAMPLE, &input()));
    assert_eq!(
        env.get("CHELABASE_COMPUTE_CA_FILE").map(String::as_str),
        Some("/m/.chelabase/compute-ca/ca.crt")
    );
}

#[test]
fn every_registered_stack_gets_its_own_control_slot() {
    let mut r = Registry::default();
    let main = r.ensure("main", Path::new("/m"), 1, |_| true).unwrap();
    assert_eq!(main.control_slot, Some(0));
    let a = r.ensure("feat-a", Path::new("/m/.worktrees/feat-a"), 2, |_| true).unwrap();
    let b = r.ensure("feat-b", Path::new("/m/.worktrees/feat-b"), 3, |_| true).unwrap();
    let slots = [main.control_slot, a.control_slot, b.control_slot];
    assert!(slots.iter().all(Option::is_some));
    let mut unique = slots.to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "{slots:?}");
    let old: Registry = serde_json::from_str(r#"{"stacks":[{"slug":"feat-c","checkout":"/m/.worktrees/feat-c","gateway_port":8140,"proxy_port":4540,"created_at":1,"running_since":null}]}"#).unwrap();
    let mut old = old;
    let c = old.ensure("feat-c", Path::new("/m/.worktrees/feat-c"), 4, |_| true).unwrap();
    assert!(c.control_slot.is_some(), "an old registry entry gets a slot");
}

// --- RT1 T7: the realtime service is wired like rest (pre-flight §4) -------------------------

// The gateway's upstream and realtime's listeners inside the stack (spec §1, S1, S18).
#[test]
fn the_stack_env_carries_realtimes_addresses() {
    let env = parse(&render_stack_env(EXAMPLE, &input()));
    for (k, v) in [
        ("CHELABASE_REALTIME_URL", "http://realtime:8087"),
        ("CHELABASE_REALTIME_ADDR", "0.0.0.0:8087"),
        ("CHELABASE_REALTIME_INTERNAL_ADDR", "0.0.0.0:8088"),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
    // S1: realtime's credentials are its own compose `environment:`, never stack.env.
    for k in [
        "CHELABASE_REALTIME_TICKET_KEY_FILE",
        "CHELABASE_REALTIME_INTERNAL_TOKEN_FILE",
        "CHELABASE_REALTIME_PG_PASSWORD_FILE",
        "CHELABASE_VALKEY_TLS",
    ] {
        assert!(!env.contains_key(k), "{k} is not in stack.env");
    }
}

// `up` builds, records and restarts realtime with the other Rust apps; it starts with the late
// apps.
#[test]
fn realtime_is_one_of_the_stacks_rust_apps() {
    use super::commands::{APPS, INFRA, LATE_APPS, RUST_APPS, restart_targets};
    assert!(RUST_APPS.contains(&"realtime"), "{RUST_APPS:?}");
    assert!(APPS.contains(&"realtime"), "{APPS:?}");
    assert!(LATE_APPS.contains(&"realtime"), "{LATE_APPS:?}");
    assert!(!INFRA.contains(&"realtime"));
    assert!(restart_targets(&[]).unwrap().contains(&"realtime".to_string()), "restart with no app named");
    assert_eq!(restart_targets(&["realtime".into()]).unwrap(), ["realtime"]);
}

// Edge functions m10: `up` builds the service runner's binary (its compose profile `services` is
// off by default), but it is no app: never started, recorded or restarted with them.
#[test]
fn service_runner_is_built_but_never_started() {
    use super::commands::{APPS, LATE_APPS, PROFILE_RUST_APPS, RUST_APPS, build_packages};
    assert_eq!(PROFILE_RUST_APPS, ["service-runner"]);
    for list in [&RUST_APPS[..], &APPS[..], LATE_APPS] {
        assert!(!list.contains(&"service-runner"), "{list:?}");
    }
    let up = build_packages(&RUST_APPS, true);
    assert!(up.windows(2).any(|w| w == ["-p", "chelabase-service-runner"]), "{up:?}");
    assert!(up.windows(2).any(|w| w == ["-p", "chelabase-functions"]), "{up:?}");
    let restart = build_packages(&["functions"], false);
    assert_eq!(restart, ["-p", "chelabase-functions"]);
}

// Edge functions m4b: `up` builds, records and restarts the functions service with the other Rust
// apps and starts it with the late apps; the runtime is infra (an image, no binary).
#[test]
fn functions_is_one_of_the_stacks_rust_apps() {
    use super::commands::{APPS, INFRA, LATE_APPS, RUST_APPS, restart_targets};
    assert!(RUST_APPS.contains(&"functions"), "{RUST_APPS:?}");
    assert!(APPS.contains(&"functions"), "{APPS:?}");
    assert!(LATE_APPS.contains(&"functions"), "{LATE_APPS:?}");
    assert!(INFRA.contains(&"functions-runtime"), "{INFRA:?}");
    assert!(!RUST_APPS.contains(&"functions-runtime"));
    assert_eq!(restart_targets(&["functions".into()]).unwrap(), ["functions"]);
}

// auth-replacement m5 (#118): `up` starts identity with the front tier and waits for it to be
// healthy before the bootstrap (which creates its members pool through it); it is an app of the
// stack (restartable, checked by `test --stack`), not a Rust app (no binary).
#[test]
fn identity_starts_before_the_bootstrap() {
    use super::commands::{APPS, FRONT, INFRA, LATE_APPS, RUST_APPS, restart_targets};
    assert!(FRONT.contains(&"identity"), "{FRONT:?}");
    assert!(APPS.contains(&"identity"), "{APPS:?}");
    assert!(!LATE_APPS.contains(&"identity"), "{LATE_APPS:?}");
    assert!(!RUST_APPS.contains(&"identity"), "{RUST_APPS:?}");
    assert!(!INFRA.contains(&"identity"));
    assert_eq!(restart_targets(&["identity".into()]).unwrap(), ["identity"]);
    assert!(!restart_targets(&[]).unwrap().contains(&"identity".to_string()));
}
