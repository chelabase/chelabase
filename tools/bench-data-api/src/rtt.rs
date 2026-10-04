//! One RTT step: apply netem, sanity-check the injected delay, measure, always clean up.

use crate::netem;
use crate::runner::Cell;
use crate::stats;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::time::Duration;

type CmdFn = dyn Fn(&[String]) -> anyhow::Result<()> + Send + Sync;

/// Runs `argv` with `std::process::Command`; a non-zero exit is an error carrying stderr.
fn run_docker(argv: &[String]) -> anyhow::Result<()> {
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| anyhow::anyhow!("cannot run `{}`: {e}", argv.join(" ")))?;
    anyhow::ensure!(
        out.status.success(),
        "`{}` failed: {}",
        argv.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Removes the netem qdisc (`netem::del_cmd`) on `Drop`, best effort: errors go to stderr,
/// never a panic. Call `remove` for the normal path (it reports the error and disarms, so
/// `Drop` doesn't run the command a second time); `disarm` just forgets the cleanup.
pub struct NetemGuard {
    container: String,
    run: Option<Box<CmdFn>>,
}

impl NetemGuard {
    pub fn new(container: &str) -> Self {
        Self::with_runner(container, Box::new(run_docker))
    }

    fn with_runner(container: &str, run: Box<CmdFn>) -> Self {
        Self {
            container: container.to_string(),
            run: Some(run),
        }
    }

    /// Runs the cleanup now and disarms the guard.
    pub fn remove(&mut self) -> anyhow::Result<()> {
        match self.run.take() {
            Some(run) => run(&netem::del_cmd(&self.container)),
            None => Ok(()),
        }
    }

    pub fn disarm(&mut self) {
        self.run = None;
    }
}

impl Drop for NetemGuard {
    fn drop(&mut self) {
        if let Err(e) = self.remove() {
            eprintln!("netem cleanup failed for {}: {e:#}", self.container);
        }
    }
}

/// The sanity check: the p50 of the pings must be at least the RTT. Returns the p50 in ms.
pub fn sanity_check(pings: &[Duration], rtt_ms: f64) -> anyhow::Result<f64> {
    anyhow::ensure!(!pings.is_empty(), "sanity check: no pings");
    let mut us: Vec<u64> = pings.iter().map(|d| d.as_micros() as u64).collect();
    let p50_ms = stats::summarize(&mut us).p50_us / 1000.0;
    anyhow::ensure!(
        p50_ms >= rtt_ms,
        "netem sanity check failed: SELECT 1 p50 is {p50_ms:.3} ms, below the injected {rtt_ms} ms RTT"
    );
    Ok(p50_ms)
}

/// What one RTT step measured.
#[derive(Debug)]
pub struct RttStep {
    pub cells: Vec<Cell>,
    /// The sanity pings' p50 (at 0 ms: the baseline, with no netem).
    pub ping_p50_ms: f64,
}

/// One RTT step: time `sanity_n` pings and require their p50 >= RTT, then run `run`. At
/// `rtt_ms == 0` no command runs. Otherwise netem is added to `container` for traffic to
/// `dst` first, and removed on every exit path (error, Ctrl-C, drop).
pub async fn at_rtt(
    container: &str,
    dst: IpAddr,
    rtt_ms: f64,
    sanity_n: usize,
    ping: impl FnMut() -> Pin<Box<dyn Future<Output = anyhow::Result<Duration>> + Send>>,
    run: impl Future<Output = anyhow::Result<Vec<Cell>>>,
) -> anyhow::Result<RttStep> {
    at_rtt_with(container, dst, rtt_ms, sanity_n, ping, run, || {
        Box::new(run_docker) as Box<CmdFn>
    })
    .await
}

async fn at_rtt_with(
    container: &str,
    dst: IpAddr,
    rtt_ms: f64,
    sanity_n: usize,
    mut ping: impl FnMut() -> Pin<Box<dyn Future<Output = anyhow::Result<Duration>> + Send>>,
    run: impl Future<Output = anyhow::Result<Vec<Cell>>>,
    runner: impl Fn() -> Box<CmdFn>,
) -> anyhow::Result<RttStep> {
    let work = async {
        let mut pings = Vec::with_capacity(sanity_n);
        for _ in 0..sanity_n {
            pings.push(ping().await?);
        }
        let ping_p50_ms = sanity_check(&pings, rtt_ms)?;
        Ok(RttStep {
            cells: run.await?,
            ping_p50_ms,
        })
    };
    if rtt_ms <= 0.0 {
        return work.await;
    }
    // Armed before the first command, so a half-applied qdisc is still removed.
    let mut guard = NetemGuard::with_runner(container, runner());
    let add = runner();
    for cmd in netem::add_cmds(container, (rtt_ms * 1000.0).round() as u32, dst) {
        add(&cmd)?;
    }
    let result = tokio::select! {
        r = work => r,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("interrupted (Ctrl-C)")),
    };
    // Normal path: remove now so a failure is reported; the guard then has nothing left.
    let removed = guard.remove();
    match (result, removed) {
        (Ok(step), Ok(())) => Ok(step),
        (Ok(_), Err(e)) => Err(e),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(c)) => Err(e.context(format!("and netem cleanup failed: {c:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Log = Arc<Mutex<Vec<Vec<String>>>>;

    fn recorder(log: &Log, fail_on: Option<usize>) -> impl Fn() -> Box<CmdFn> + use<> {
        let log = log.clone();
        move || {
            let log = log.clone();
            Box::new(move |argv: &[String]| {
                let mut l = log.lock().unwrap();
                l.push(argv.to_vec());
                if fail_on == Some(l.len()) {
                    anyhow::bail!("stderr text");
                }
                Ok(())
            })
        }
    }

    fn ping_of(
        ms: u64,
    ) -> impl FnMut() -> Pin<Box<dyn Future<Output = anyhow::Result<Duration>> + Send>> {
        move || Box::pin(async move { Ok(Duration::from_millis(ms)) })
    }

    fn dst() -> IpAddr {
        "172.17.0.1".parse().unwrap()
    }

    #[tokio::test]
    async fn rtt_zero_runs_no_command_but_runs_the_work() {
        let log: Log = Default::default();
        let out = at_rtt_with(
            "c",
            dst(),
            0.0,
            5,
            ping_of(1),
            async { Ok(Vec::new()) },
            recorder(&log, None),
        )
        .await
        .unwrap();
        assert!(out.cells.is_empty());
        // The pings run at 0 ms too: the baseline the other RTTs' p50 compare with.
        assert_eq!(out.ping_p50_ms, 1.0);
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn nonzero_rtt_adds_runs_then_removes_once() {
        let log: Log = Default::default();
        let ran = Arc::new(Mutex::new(false));
        let r2 = ran.clone();
        let out = at_rtt_with(
            "c",
            dst(),
            2.0,
            3,
            ping_of(3),
            async move {
                *r2.lock().unwrap() = true;
                Ok(Vec::new())
            },
            recorder(&log, None),
        )
        .await
        .unwrap();
        assert!(*ran.lock().unwrap());
        assert_eq!(out.ping_p50_ms, 3.0);
        let mut want = netem::add_cmds("c", 2000, dst());
        want.push(netem::del_cmd("c"));
        assert_eq!(*log.lock().unwrap(), want);
    }

    #[tokio::test]
    async fn failed_sanity_check_still_removes_netem() {
        let log: Log = Default::default();
        let err = at_rtt_with(
            "c",
            dst(),
            2.0,
            3,
            ping_of(1),
            async { Ok(Vec::new()) },
            recorder(&log, None),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("sanity"), "{err}");
        assert_eq!(log.lock().unwrap().last().unwrap(), &netem::del_cmd("c"));
    }

    #[tokio::test]
    async fn failed_command_aborts_with_stderr_and_cleans_up() {
        let log: Log = Default::default();
        let err = at_rtt_with(
            "c",
            dst(),
            2.0,
            3,
            ping_of(3),
            async { Ok(Vec::new()) },
            recorder(&log, Some(2)),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("stderr text"), "{err}");
        assert_eq!(log.lock().unwrap().last().unwrap(), &netem::del_cmd("c"));
    }

    #[test]
    fn guard_runs_del_on_drop_and_not_after_remove() {
        let log: Log = Default::default();
        drop(NetemGuard::with_runner("c", recorder(&log, None)()));
        assert_eq!(*log.lock().unwrap(), vec![netem::del_cmd("c")]);

        let log: Log = Default::default();
        let mut g = NetemGuard::with_runner("c", recorder(&log, None)());
        g.remove().unwrap();
        drop(g);
        assert_eq!(log.lock().unwrap().len(), 1);

        let log: Log = Default::default();
        let mut g = NetemGuard::with_runner("c", recorder(&log, None)());
        g.disarm();
        drop(g);
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn guard_drop_never_panics_on_failure() {
        let log: Log = Default::default();
        drop(NetemGuard::with_runner("c", recorder(&log, Some(1))()));
    }

    #[test]
    fn sanity_check_decision() {
        let p = |ms: &[u64]| {
            ms.iter()
                .map(|m| Duration::from_millis(*m))
                .collect::<Vec<_>>()
        };
        assert!(sanity_check(&p(&[2, 2, 3]), 2.0).is_ok());
        assert!(sanity_check(&p(&[1, 1, 3]), 2.0).is_err());
        assert!(sanity_check(&[], 2.0).is_err());
    }
}
