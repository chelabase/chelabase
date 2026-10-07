//! Measurement runner: interleaved latency and concurrent throughput.

use crate::stats::{self, Summary};
use crate::variants::{Exec, VariantKind};
use crate::workload::{BenchOp, OpKind};

/// Requests per repeat. Full: 200/2000/3; quick: 10/50/1.
#[derive(Debug, Clone, Copy)]
pub struct LatencyPlan {
    pub warmup: usize,
    pub measured: usize,
    pub repeats: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Cell {
    pub variant: VariantKind,
    pub op: OpKind,
    pub summary: Summary,
}

/// Latency per (variant, op). For each repeat and op the variants run round-robin, one
/// request each (a fresh `next(op)` per request; round `i` starts at variant `i % n`),
/// until each has `warmup + measured`
/// requests; only the measured ones are timed (around `exec` only). A cell's summary is the
/// median over the repeats. Any error aborts with the variant, op and error.
pub async fn latency<E: Exec>(
    variants: &[&E],
    ops: &[OpKind],
    plan: &LatencyPlan,
    mut next: impl FnMut(OpKind) -> BenchOp,
) -> anyhow::Result<Vec<Cell>> {
    // runs[op][variant] = one summary per repeat
    let mut runs: Vec<Vec<Vec<Summary>>> = vec![vec![Vec::new(); variants.len()]; ops.len()];
    for _ in 0..plan.repeats {
        for (oi, &op) in ops.iter().enumerate() {
            let mut samples: Vec<Vec<u64>> =
                vec![Vec::with_capacity(plan.measured); variants.len()];
            let n = variants.len();
            for i in 0..plan.warmup + plan.measured {
                // Round i starts at variant i % n, so no variant always runs right after
                // the same one (A's after-release reset, for one).
                for j in 0..n {
                    let vi = (j + i) % n;
                    let v = variants[vi];
                    let bop = next(op);
                    let start = std::time::Instant::now();
                    let res = v.exec(&bop).await;
                    let us = start.elapsed().as_micros() as u64;
                    if let Err(e) = res {
                        anyhow::bail!("variant {:?}, op {:?}, request {}: {e:#}", v.kind(), op, i);
                    }
                    if i >= plan.warmup {
                        samples[vi].push(us);
                    }
                }
            }
            for (vi, s) in samples.iter_mut().enumerate() {
                runs[oi][vi].push(stats::summarize(s));
            }
        }
    }
    let mut cells = Vec::new();
    for (oi, &op) in ops.iter().enumerate() {
        for (vi, v) in variants.iter().enumerate() {
            cells.push(Cell {
                variant: v.kind(),
                op,
                summary: stats::median_of(&runs[oi][vi]),
            });
        }
    }
    Ok(cells)
}

/// One throughput cell's shape. Full: 32 / 2 s / 20 s; quick: 32 / 0.5 s / 2 s.
#[derive(Debug, Clone, Copy)]
pub struct ThroughputPlan {
    pub concurrency: usize,
    /// Untimed: connections open and statements get prepared.
    pub warmup: std::time::Duration,
    /// The measured window.
    pub window: std::time::Duration,
}

/// `xs` rotated left by `round` (round 0: as given), so each round starts elsewhere.
pub fn rotated<T: Copy>(xs: &[T], round: usize) -> Vec<T> {
    let n = xs.len().max(1);
    (0..xs.len()).map(|j| xs[(j + round) % n]).collect()
}

/// Requests per second: `concurrency` spawned tokio tasks loop `exec`; after the untimed
/// warm-up, the requests that complete within the window are counted; count / window.
/// Any error aborts every worker. (Needs a multi-thread runtime to use more than one core;
/// the variant and `next` are shared through `Arc`.)
pub async fn throughput<E: Exec + 'static>(
    variant: std::sync::Arc<E>,
    op: OpKind,
    plan: &ThroughputPlan,
    next: std::sync::Arc<dyn Fn(OpKind) -> BenchOp + Send + Sync>,
) -> anyhow::Result<f64> {
    // tokio's clock, so a test can run the window on paused time (deterministic, whatever
    // the machine's load). It is the monotonic wall clock when time isn't paused.
    use tokio::time::Instant;
    let t0 = Instant::now() + plan.warmup;
    let t1 = t0 + plan.window;
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..plan.concurrency {
        let variant = variant.clone();
        let next = next.clone();
        workers.spawn(async move {
            let mut counted = 0u64;
            while Instant::now() < t1 {
                let bop = next(op);
                variant.exec(&bop).await.map_err(|e| {
                    anyhow::anyhow!("variant {:?}, op {:?}: {e:#}", variant.kind(), op)
                })?;
                let done = Instant::now();
                if done >= t0 && done < t1 {
                    counted += 1;
                }
            }
            anyhow::Ok(counted)
        });
    }
    let mut total = 0u64;
    while let Some(joined) = workers.join_next().await {
        let counted = match joined {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                workers.abort_all();
                return Err(e);
            }
            Err(e) => {
                workers.abort_all();
                return Err(anyhow::anyhow!("a throughput worker died: {e}"));
            }
        };
        total += counted;
    }
    Ok(total as f64 / plan.window.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct Double {
        kind: VariantKind,
        sleep: Duration,
        log: &'static Mutex<Vec<VariantKind>>,
        calls: &'static AtomicUsize,
        fail_on: Option<usize>,
    }

    impl Exec for Double {
        fn kind(&self) -> VariantKind {
            self.kind
        }
        async fn exec(&self, _op: &BenchOp) -> anyhow::Result<String> {
            self.log.lock().unwrap().push(self.kind);
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_on == Some(n) {
                anyhow::bail!("boom");
            }
            if !self.sleep.is_zero() {
                tokio::time::sleep(self.sleep).await;
            }
            Ok("x".into())
        }
    }

    fn bench_op(kind: OpKind) -> BenchOp {
        let parsed = chelabase_query::parse(br#"{"v":1,"op":"select","from":"t"}"#).unwrap();
        BenchOp {
            kind,
            op: parsed.op,
            body: std::sync::Arc::from(&b""[..]),
            plans: std::sync::Arc::new(Vec::new()),
        }
    }

    fn leak<T>(t: T) -> &'static T {
        Box::leak(Box::new(t))
    }

    fn doubles(
        sleep_ms: u64,
        fail_on: Option<usize>,
    ) -> (Double, Double, &'static Mutex<Vec<VariantKind>>) {
        let log = leak(Mutex::new(Vec::new()));
        let calls = leak(AtomicUsize::new(0));
        let mk = |kind| Double {
            kind,
            sleep: Duration::from_millis(sleep_ms),
            log,
            calls,
            fail_on,
        };
        (mk(VariantKind::A), mk(VariantKind::B), log)
    }

    #[tokio::test]
    async fn interleaves_variants_round_robin() {
        let (a, b, log) = doubles(0, None);
        let plan = LatencyPlan {
            warmup: 1,
            measured: 2,
            repeats: 1,
        };
        latency(&[&a, &b], &[OpKind::ByKey, OpKind::Insert], &plan, bench_op)
            .await
            .unwrap();
        use VariantKind::{A, B};
        // Round i starts at variant i % n, so no variant always follows the same one.
        let one_op = [A, B, B, A, A, B];
        let want: Vec<_> = one_op.iter().chain(one_op.iter()).copied().collect();
        assert_eq!(*log.lock().unwrap(), want);
    }

    #[tokio::test]
    async fn warmup_is_not_measured() {
        let (a, b, _) = doubles(0, None);
        let plan = LatencyPlan {
            warmup: 5,
            measured: 7,
            repeats: 1,
        };
        let cells = latency(&[&a, &b], &[OpKind::ByKey], &plan, bench_op)
            .await
            .unwrap();
        assert_eq!(cells.len(), 2);
        assert!(cells.iter().all(|c| c.summary.n == 7));
    }

    #[tokio::test]
    async fn latency_aborts_on_error() {
        let (a, b, _) = doubles(0, Some(5));
        let plan = LatencyPlan {
            warmup: 1,
            measured: 5,
            repeats: 1,
        };
        let err = latency(&[&a, &b], &[OpKind::List100], &plan, bench_op)
            .await
            .unwrap_err()
            .to_string();
        // call 5 is variant A's third request
        assert!(err.contains("A"), "{err}");
        assert!(err.contains("List100"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    fn tplan(warmup_ms: u64, window_ms: u64) -> ThroughputPlan {
        ThroughputPlan {
            concurrency: 4,
            warmup: Duration::from_millis(warmup_ms),
            window: Duration::from_millis(window_ms),
        }
    }

    // Paused time: the 10 ms sleeps take exactly 10 ms, so the count doesn't depend on load.
    #[tokio::test(start_paused = true)]
    async fn throughput_counts_completions() {
        let (a, _, _) = doubles(10, None);
        let rps = throughput(
            std::sync::Arc::new(a),
            OpKind::ByKey,
            &tplan(0, 1000),
            std::sync::Arc::new(bench_op),
        )
        .await
        .unwrap();
        assert!((300.0..=420.0).contains(&rps), "{rps}");
    }

    #[tokio::test(start_paused = true)]
    async fn throughput_warmup_is_not_counted() {
        let (a, _, log) = doubles(10, None);
        let start = tokio::time::Instant::now();
        let rps = throughput(
            std::sync::Arc::new(a),
            OpKind::ByKey,
            &tplan(500, 500),
            std::sync::Arc::new(bench_op),
        )
        .await
        .unwrap();
        let took = start.elapsed();
        // The rate is over the window only (4 workers at ~10 ms: ~400/s), while about
        // twice the window's requests ran in all.
        assert!((300.0..=420.0).contains(&rps), "{rps}");
        let calls = log.lock().unwrap().len() as f64;
        assert!(calls > rps * 0.5 * 1.6, "calls {calls}, rps {rps}");
        assert!(took >= Duration::from_millis(1000), "{took:?}");
    }

    /// The first call fails after 50 ms; every other call takes 20 ms. The error comes back
    /// at once (not at the window's end) and no worker keeps calling afterwards.
    struct FailsFirst {
        calls: AtomicUsize,
    }

    impl Exec for FailsFirst {
        fn kind(&self) -> VariantKind {
            VariantKind::C
        }
        async fn exec(&self, _op: &BenchOp) -> anyhow::Result<String> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                anyhow::bail!("boom");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok("x".into())
        }
    }

    #[tokio::test]
    async fn failing_worker_aborts_the_others() {
        let v = std::sync::Arc::new(FailsFirst {
            calls: AtomicUsize::new(0),
        });
        let start = std::time::Instant::now();
        let err = throughput(
            v.clone(),
            OpKind::Insert,
            &tplan(0, 5000),
            std::sync::Arc::new(bench_op),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("boom") && err.contains("Insert"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{:?}",
            start.elapsed()
        );
        let after = v.calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            v.calls.load(Ordering::SeqCst),
            after,
            "workers kept running"
        );
    }

    #[test]
    fn rotated_starts_each_round_one_later() {
        use VariantKind::{A, B, C, D};
        let all = [A, B, C, D];
        assert_eq!(rotated(&all, 0), [A, B, C, D]);
        assert_eq!(rotated(&all, 1), [B, C, D, A]);
        assert_eq!(rotated(&all, 2), [C, D, A, B]);
        assert_eq!(rotated(&all, 4), [A, B, C, D]);
    }

    /// Every call takes 5 ms and records how many calls are in flight at once.
    struct Tracked {
        in_flight: AtomicUsize,
        max_in_flight: AtomicUsize,
    }

    impl Exec for Tracked {
        fn kind(&self) -> VariantKind {
            VariantKind::A
        }
        async fn exec(&self, _op: &BenchOp) -> anyhow::Result<String> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok("x".into())
        }
    }

    // Paused time needs the current-thread runtime. The workers are still concurrent: they
    // interleave at the sleep, so all four calls are in flight at once. Run one after the
    // other the same four workers would reach a max in flight of 1 and 200 req/s; here
    // each worker completes 199 calls inside the 1 s window (at 5, 10, .. 995 ms): 796/s.
    #[tokio::test(start_paused = true)]
    async fn throughput_uses_parallel_tasks() {
        let v = std::sync::Arc::new(Tracked {
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        });
        let rps = throughput(
            v.clone(),
            OpKind::ByKey,
            &tplan(0, 1000),
            std::sync::Arc::new(bench_op),
        )
        .await
        .unwrap();
        assert_eq!(v.max_in_flight.load(Ordering::SeqCst), 4);
        assert!((790.0..=800.0).contains(&rps), "{rps}");
    }

    #[tokio::test]
    async fn latency_loops_repeats_outer_ops_inner() {
        let (a, _, log) = doubles(0, None);
        let plan = LatencyPlan {
            warmup: 0,
            measured: 1,
            repeats: 2,
        };
        let seen = Mutex::new(Vec::new());
        latency(&[&a], &[OpKind::ByKey, OpKind::List100], &plan, |op| {
            seen.lock().unwrap().push(op);
            bench_op(op)
        })
        .await
        .unwrap();
        assert_eq!(log.lock().unwrap().len(), 4);
        assert_eq!(
            *seen.lock().unwrap(),
            [
                OpKind::ByKey,
                OpKind::List100,
                OpKind::ByKey,
                OpKind::List100
            ]
        );
    }
}
