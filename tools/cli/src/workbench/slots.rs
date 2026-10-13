//! Slots: at most `cap` stacks run at once; the rest wait in FIFO order on tickets in
//! `<stacks_dir>/queue` (W2 spec section 1).

use super::registry::{Entry, Registry, with_registry};
use anyhow::Context as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const QUEUE_DIR: &str = "queue";
const DEFAULT_CAP: usize = 4;
const DEFAULT_WAIT: Duration = Duration::from_secs(45 * 60);

#[derive(Debug, Clone)]
pub struct SlotOptions {
    /// Stacks allowed to run at once, main included.
    pub cap: usize,
    /// How long `acquire_slot` waits before giving up.
    pub timeout: Duration,
    /// How often the wait re-checks.
    pub poll: Duration,
    /// How often the wait reports a status line.
    pub status_every: Duration,
}

/// A queue ticket's file name: `{nanos:039}-{slug}`, so names sort by time.
pub fn ticket_name(nanos: u128, slug: &str) -> String {
    format!("{nanos:039}-{slug}")
}

/// What this process writes into its ticket: `<pid> <starttime>` (the start time from
/// `/proc/<pid>/stat`), so a later process that reuses the pid doesn't keep the ticket alive. If
/// the start time can't be read, just `<pid>`.
pub fn own_ticket_content() -> String {
    let pid = std::process::id();
    match proc_start_time(pid) {
        Some(start) => format!("{pid} {start}"),
        None => pid.to_string(),
    }
}

/// The queue's tickets as `(ticket name, slug)`, oldest first. A missing queue is empty.
pub fn queued(stacks_dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let queue = stacks_dir.join(QUEUE_DIR);
    if !queue.exists() {
        return Ok(Vec::new());
    }
    Ok(tickets(&queue)?
        .into_iter()
        .filter_map(|name| {
            let slug = parse_ticket(&name)?.1.to_string();
            Some((name, slug))
        })
        .collect())
}

/// Removes every ticket queued for `slug` (under the registry lock); returns how many.
pub fn drop_tickets(stacks_dir: &Path, slug: &str) -> anyhow::Result<usize> {
    with_registry(stacks_dir, |_| {
        let queue = stacks_dir.join(QUEUE_DIR);
        let mut dropped = 0;
        for (name, s) in queued(stacks_dir)? {
            if s == slug {
                remove_ticket(&queue.join(name))?;
                dropped += 1;
            }
        }
        Ok(dropped)
    })
}

/// Our ticket while we wait. Dropping it removes the file, so no path out of `acquire_slot`
/// (taken, error, timeout, panic) leaves it behind.
struct Ticket(Option<PathBuf>);

impl Ticket {
    /// Removes the ticket now, reporting a failure.
    fn remove(&mut self) -> anyhow::Result<()> {
        match self.0.take() {
            Some(path) => remove_ticket(&path),
            None => Ok(()),
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            // Best effort: a ticket left behind would hold the queue only while this process
            // lives, and the next round's prune drops it after that.
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Marks every running entry that `alive` rejects as stopped; returns their slugs.
pub fn reap(registry: &mut Registry, alive: impl Fn(&Entry) -> bool) -> Vec<String> {
    let mut reaped = Vec::new();
    for e in &mut registry.stacks {
        if e.running_since.is_some() && !alive(e) {
            e.running_since = None;
            reaped.push(e.slug.clone());
        }
    }
    reaped
}

/// What one round under the registry lock decided.
enum Round {
    Taken,
    Wait {
        running: Vec<(String, u64)>,
        ahead: Vec<String>,
    },
}

/// Takes a slot for `slug` (which must be registered), waiting in FIFO order while all `cap` slots
/// are held. Each round runs under the registry lock; the sleep between rounds doesn't.
pub fn acquire_slot(
    stacks_dir: &Path,
    slug: &str,
    opts: &SlotOptions,
    alive: &dyn Fn(&Entry) -> bool,
    now: &dyn Fn() -> u64,
    report: &mut dyn FnMut(&str),
) -> anyhow::Result<()> {
    let queue = stacks_dir.join(QUEUE_DIR);
    let start = Instant::now();
    let mut ticket = Ticket(None);
    let mut last_report: Option<Instant> = None;
    loop {
        let round = with_registry(stacks_dir, |r| {
            reap(r, alive);
            let entry = r.get(slug).with_context(|| {
                format!("stack {slug} isn't registered, so it can't take a workbench slot")
            })?;
            if entry.running_since.is_some() {
                ticket.remove()?;
                return Ok(Round::Taken);
            }
            std::fs::create_dir_all(&queue)
                .with_context(|| format!("creating {}", queue.display()))?;
            prune_dead_tickets(&queue)?;
            let ours = match &ticket.0 {
                Some(path) => path.clone(),
                None => {
                    // Strictly after every ticket already queued, even if the clock stepped back.
                    let newest = tickets(&queue)?
                        .iter()
                        .filter_map(|t| parse_ticket(t).map(|(nanos, _)| nanos))
                        .max();
                    let nanos = newest.map_or(unix_nanos(), |n| unix_nanos().max(n + 1));
                    queue.join(ticket_name(nanos, slug))
                }
            };
            ticket.0 = Some(ours.clone());
            if !ours.exists() {
                std::fs::write(&ours, own_ticket_content())
                    .with_context(|| format!("writing {}", ours.display()))?;
            }
            let ours_name = file_name(&ours);
            let ahead: Vec<String> = tickets(&queue)?
                .into_iter()
                .filter(|t| *t < ours_name)
                .collect();
            let running: Vec<(String, u64)> = r
                .running()
                .map(|e| {
                    let since = e.running_since.unwrap_or_default();
                    (e.slug.clone(), now().saturating_sub(since))
                })
                .collect();
            if running.len() < opts.cap && ahead.is_empty() {
                let at = now();
                if let Some(e) = r.get_mut(slug) {
                    e.running_since = Some(at);
                }
                ticket.remove()?;
                return Ok(Round::Taken);
            }
            Ok(Round::Wait { running, ahead })
        })?;

        let (running, ahead) = match round {
            Round::Taken => return Ok(()),
            Round::Wait { running, ahead } => (running, ahead),
        };
        let waited = start.elapsed();
        if waited >= opts.timeout {
            with_registry(stacks_dir, |_| ticket.remove())?;
            anyhow::bail!(
                "no workbench slot came free for {slug} after {}: {} of {} slots held by {}{}; \
                 stop one with `just workbench down --stack <slug>`",
                fmt_waited(waited),
                running.len(),
                opts.cap,
                describe_running(&running),
                describe_ahead(&ahead),
            );
        }
        if last_report.is_none_or(|t| t.elapsed() >= opts.status_every) {
            report(&format!(
                "waiting for a workbench slot ({} of {} held by {}{}); waited {}",
                running.len(),
                opts.cap,
                describe_running(&running),
                describe_ahead(&ahead),
                fmt_waited(waited),
            ));
            last_report = Some(Instant::now());
        }
        std::thread::sleep(opts.poll.min(opts.timeout.saturating_sub(waited)));
    }
}

/// Gives `slug`'s slot back. A missing entry is fine (nothing to release).
pub fn release_slot(stacks_dir: &Path, slug: &str) -> anyhow::Result<()> {
    with_registry(stacks_dir, |r| {
        if let Some(e) = r.get_mut(slug) {
            e.running_since = None;
        }
        Ok(())
    })
}

/// Refreshes a running stack's `running_since` to `now` (under the registry lock), so a long
/// build inside `up` doesn't outlast the reap grace. A stopped or missing stack is left alone:
/// touching never takes a slot.
pub fn touch_slot(stacks_dir: &Path, slug: &str, now: u64) -> anyhow::Result<()> {
    with_registry(stacks_dir, |r| {
        if let Some(e) = r.get_mut(slug).filter(|e| e.running_since.is_some()) {
            e.running_since = Some(now);
        }
        Ok(())
    })
}

/// `CHELABASE_WORKBENCH_SLOTS`: an integer ≥ 1, default 4 (unset or blank).
pub fn cap_from_env(v: Option<&str>) -> anyhow::Result<usize> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_CAP);
    };
    match v.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => anyhow::bail!("CHELABASE_WORKBENCH_SLOTS={v:?} must be an integer of at least 1"),
    }
}

/// `CHELABASE_WORKBENCH_WAIT`: `<secs>` or `<n>m`, default 45 minutes (unset or blank).
pub fn wait_from_env(v: Option<&str>) -> anyhow::Result<Duration> {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(DEFAULT_WAIT);
    };
    let bad = || anyhow::anyhow!("CHELABASE_WORKBENCH_WAIT={v:?} must be seconds or `<n>m`");
    match v.strip_suffix('m') {
        Some(mins) => {
            let mins: u64 = mins.parse().map_err(|_| bad())?;
            mins.checked_mul(60)
                .map(Duration::from_secs)
                .ok_or_else(bad)
        }
        None => v.parse().map(Duration::from_secs).map_err(|_| bad()),
    }
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The queue's ticket names, oldest first.
fn tickets(queue: &Path) -> anyhow::Result<Vec<String>> {
    let mut names = Vec::new();
    for item in std::fs::read_dir(queue).with_context(|| format!("reading {}", queue.display()))? {
        let item = item.with_context(|| format!("reading {}", queue.display()))?;
        names.push(item.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// A ticket name's time and slug (`{nanos:039}-{slug}`).
fn parse_ticket(name: &str) -> Option<(u128, &str)> {
    let (nanos, slug) = name.split_once('-')?;
    Some((nanos.parse().ok()?, slug))
}

/// Field 22 (start time, in clock ticks since boot) of `/proc/<pid>/stat`. The comm field (2) may
/// hold spaces and parentheses, so fields are counted after its last `)`.
fn proc_start_time(pid: u32) -> Option<u64> {
    start_time_from_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

fn start_time_from_stat(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // `rest` starts at field 3 (state), so field 22 is the 20th.
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether a ticket's waiting process lives: `<pid> <starttime>` needs `/proc/<pid>` with that
/// start time; a bare `<pid>` (older tickets) needs only `/proc/<pid>`. Anything else is dead.
fn ticket_is_live(content: &str) -> bool {
    let mut parts = content.split_whitespace();
    let Some(Ok(pid)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    let start = match parts.next().map(str::parse::<u64>) {
        None => None,
        Some(Ok(start)) => Some(start),
        Some(Err(_)) => return false,
    };
    if parts.next().is_some() || !Path::new(&format!("/proc/{pid}")).exists() {
        return false;
    }
    start.is_none_or(|start| proc_start_time(pid) == Some(start))
}

/// Drops every ticket whose waiting process is gone (or whose content can't be read).
fn prune_dead_tickets(queue: &Path) -> anyhow::Result<()> {
    for name in tickets(queue)? {
        let path = queue.join(&name);
        let live = std::fs::read_to_string(&path).is_ok_and(|s| ticket_is_live(&s));
        if !live {
            remove_ticket(&path)?;
        }
    }
    Ok(())
}

fn remove_ticket(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

/// An uptime as `{h}h {m}m` from one hour, else `{m}m`.
fn fmt_uptime(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

fn fmt_waited(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        fmt_uptime(secs)
    }
}

fn describe_running(running: &[(String, u64)]) -> String {
    if running.is_empty() {
        return "no stack".to_string();
    }
    running
        .iter()
        .map(|(slug, up)| format!("{slug} (up {})", fmt_uptime(*up)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The stacks queued ahead, by the slug in each ticket name.
fn describe_ahead(ahead: &[String]) -> String {
    if ahead.is_empty() {
        return String::new();
    }
    let slugs: Vec<&str> = ahead
        .iter()
        .map(|t| t.split_once('-').map_or(t.as_str(), |(_, slug)| slug))
        .collect();
    format!("; queued ahead: {}", slugs.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_time_is_read_after_the_last_paren_of_comm() {
        let tail = "S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 19 20";
        assert_eq!(
            start_time_from_stat(&format!("7 (a) b) (c) {tail}")),
            Some(424_242)
        );
        assert_eq!(start_time_from_stat("7 (short) S 1 2"), None);
        assert_eq!(start_time_from_stat("garbage"), None);
    }

    #[test]
    fn garbled_ticket_content_is_dead() {
        let pid = std::process::id();
        assert!(ticket_is_live(&own_ticket_content()));
        assert!(ticket_is_live(&pid.to_string()), "a legacy pid-only ticket");
        for bad in [
            "",
            "x",
            &format!("{pid} x"),
            &format!("{} 1 2", own_ticket_content()),
        ] {
            assert!(!ticket_is_live(bad), "{bad:?}");
        }
    }
}
