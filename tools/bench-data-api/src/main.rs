use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bench_data_api::report::{self, Results, Setup};
use bench_data_api::runner::{self, LatencyPlan, ThroughputPlan};
use bench_data_api::variants::{
    AnyVariant, Exec, RTarget, VariantKind, Variants, fixed_claims,
};
use bench_data_api::workload::{ALL_OPS, BenchOp, OpKind, Workload};
use bench_data_api::{netem, rtt, setup, stats};
use clap::Parser;
use rest::conn::{ConnSource, InternalSource};
use sqlx::Connection;

#[derive(Debug, Parser)]
#[command(name = "bench-data-api", about = "Data API pipelining benchmark")]
struct Args {
    #[arg(long)]
    owner_url: String,
    #[arg(long)]
    branch: uuid::Uuid,
    #[arg(long, env = "CHELABASE_INTERNAL_URL")]
    internal_url: String,
    #[arg(long, env = "CHELABASE_REST_INTERNAL_TOKEN")]
    internal_token: String,
    #[arg(long)]
    compute_container: String,
    #[arg(long, value_delimiter = ',', default_value = "0,0.5,2")]
    rtts_ms: Vec<f64>,
    #[arg(long)]
    quick: bool,
    /// The variants to run, comma-separated (C is required: its pool carries the sanity ping).
    /// R is the shipped path (rest's engine); the default is the four Phase 1 variants.
    #[arg(long, value_delimiter = ',', default_value = "A,B,C,D")]
    variants: Vec<VariantKind>,
    /// Latency repeats, in place of the plan's.
    #[arg(long)]
    repeats: Option<usize>,
    /// Latency only: no throughput rounds.
    #[arg(long)]
    no_throughput: bool,
    #[arg(long, default_value = "target/bench-data-api")]
    out: std::path::PathBuf,
    #[arg(long)]
    results_md: Option<std::path::PathBuf>,
    /// Only create the fixture (`setup::seed`) and exit: for the recipe's `--tests-only`.
    #[arg(long)]
    seed_only: bool,
}

/// How much of everything a run does.
#[derive(Debug, Clone, Copy)]
struct RunPlan {
    latency: LatencyPlan,
    sanity_n: usize,
    throughput: ThroughputPlan,
    throughput_rounds: usize,
    pool_size: u32,
}

impl RunPlan {
    fn new(quick: bool) -> RunPlan {
        let ms = Duration::from_millis;
        if quick {
            RunPlan {
                latency: LatencyPlan {
                    warmup: 10,
                    measured: 50,
                    repeats: 1,
                },
                sanity_n: 20,
                throughput: ThroughputPlan {
                    concurrency: 32,
                    warmup: ms(500),
                    window: ms(2_000),
                },
                throughput_rounds: 1,
                pool_size: 32,
            }
        } else {
            RunPlan {
                latency: LatencyPlan {
                    warmup: 200,
                    measured: 2000,
                    repeats: 3,
                },
                sanity_n: 200,
                throughput: ThroughputPlan {
                    concurrency: 32,
                    warmup: ms(2_000),
                    window: ms(20_000),
                },
                throughput_rounds: 3,
                pool_size: 32,
            }
        }
    }

    fn with_repeats(mut self, repeats: Option<usize>) -> RunPlan {
        if let Some(n) = repeats {
            self.latency.repeats = n;
        }
        self
    }

    fn describe(&self, quick: bool) -> String {
        let l = &self.latency;
        let t = &self.throughput;
        let rounds = self.throughput_rounds;
        format!(
            "{} (latency: {} warm-up + {} measured requests per variant and op, {} repeats, \
             variants interleaved with the first rotated per round; sanity: {} pings per RTT; \
             throughput at 0 ms RTT: {rounds} round{} of {} concurrent, {} s warm-up + {} s \
             measured per cell, a fresh pool per cell, variant order rotated per round, the \
             table vacuumed before each round, median reported)",
            if quick { "quick" } else { "full" },
            l.warmup,
            l.measured,
            l.repeats,
            self.sanity_n,
            if rounds == 1 { "" } else { "s" },
            t.concurrency,
            t.warmup.as_secs_f64(),
            t.window.as_secs_f64(),
        )
    }
}

/// The selected variants' `application_name`s.
fn expected_apps(kinds: &[VariantKind]) -> Vec<&'static str> {
    kinds.iter().map(|k| k.application_name()).collect()
}

/// Prints each bench session's TLS state, as the compute sees it (a fairness check: A and B
/// connect with `Prefer`, C and D with `NoTls`).
async fn print_tls(owner: &mut sqlx::PgConnection) -> anyhow::Result<()> {
    let rows: Vec<(String, Option<bool>, i64)> = sqlx::query_as(
        "SELECT a.application_name, s.ssl, count(*) \
         FROM pg_stat_activity a LEFT JOIN pg_stat_ssl s USING (pid) \
         WHERE (a.application_name LIKE 'bench-%' OR a.application_name = 'chelabase-rest') \
         GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .fetch_all(owner)
    .await
    .context("reading pg_stat_ssl")?;
    for (app, ssl, n) in rows {
        eprintln!("tls: {app} ssl={ssl:?} sessions={n}");
    }
    Ok(())
}

async fn run(args: Args) -> anyhow::Result<()> {
    if args.seed_only {
        let mut owner = sqlx::PgConnection::connect(&args.owner_url)
            .await
            .context("connecting as the owner")?;
        let version = setup::seed(&mut owner).await?;
        eprintln!("seeded; schema version {version}");
        return Ok(());
    }
    anyhow::ensure!(
        args.variants.contains(&VariantKind::C),
        "--variants must include C (its pool carries the sanity ping)"
    );
    let plan = RunPlan::new(args.quick).with_repeats(args.repeats);
    let repo_root = std::env::current_dir()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
    let mut setup_info = Setup {
        title: report::TITLE.to_owned(),
        date: report::civil_date(now.as_secs()),
        harness_commit: report::harness_commit(&repo_root)?,
        sanity_p50_ms: Vec::new(),
        neon_ref: report::neon_ref(&repo_root)?,
        compute_image: report::compute_image(&args.compute_container)?,
        compute_image_id: report::compute_image_id(&args.compute_container)?,
        netshoot: netem::NETSHOOT.to_owned(),
        pool_size: plan.pool_size,
        plan: plan.describe(args.quick),
    };

    let source = InternalSource::new(&args.internal_url, &args.internal_token);
    let info = source
        .fetch(args.branch)
        .await
        .map_err(|e| anyhow::anyhow!("fetching the branch's connection info: {e:?}"))?;
    let mut owner = sqlx::PgConnection::connect(&args.owner_url)
        .await
        .context("connecting as the owner")?;
    let version = setup::seed(&mut owner).await?;
    eprintln!("seeded; schema version {version}");
    let dst = setup::client_addr(&info).await?;
    eprintln!("client address as the compute sees it: {dst}");
    let default_schema = info
        .served(&info.database)
        .map_or("public", |d| d.default_schema.as_str())
        .to_owned();
    let workload = Arc::new(Workload::build(&args.owner_url, &default_schema).await?);
    let claims = fixed_claims();
    let (max_rows, timeout_ms) = (info.settings.max_rows, info.settings.statement_timeout_ms);
    let target = RTarget {
        internal_url: args.internal_url.clone(),
        internal_token: args.internal_token.clone(),
        branch: args.branch,
    };
    let variants = Arc::new(
        Variants::connect(
            &args.variants,
            &info,
            &target,
            plan.pool_size,
            &claims,
            version,
            max_rows,
            timeout_ms,
        )
        .await?,
    );

    // One untimed request per variant opens its connection, so the client address check
    // sees all of them from the first RTT on.
    for v in &variants.all {
        v.exec(&workload.next(OpKind::ByKey, &mut rand::rng()))
            .await
            .with_context(|| format!("opening variant {:?}", v.kind()))?;
    }

    let mut latency = Vec::new();
    for &rtt_ms in &args.rtts_ms {
        eprintln!("latency at {rtt_ms} ms RTT ...");
        let refs: Vec<_> = variants.all.iter().collect();
        let ping_variants = variants.clone();
        let ping = move || -> Pin<Box<dyn Future<Output = anyhow::Result<Duration>> + Send>> {
            let v = ping_variants.clone();
            Box::pin(async move { v.ping().await })
        };
        let w = workload.clone();
        let next = move |op: OpKind| w.next(op, &mut rand::rng());
        let apps = expected_apps(&args.variants);
        let owner = &mut owner;
        let measure = async {
            // Every session netem must delay comes from dst (checked after the sanity pings,
            // before measuring).
            let sessions = setup::bench_client_addrs(owner).await?;
            let n = setup::check_client_addrs(&sessions, dst, &apps)?;
            eprintln!("client addresses: all {n} bench sessions come from {dst}");
            runner::latency(&refs, &ALL_OPS, &plan.latency, next).await
        };
        let step = rtt::at_rtt(
            &args.compute_container,
            dst,
            rtt_ms,
            plan.sanity_n,
            ping,
            measure,
        )
        .await
        .with_context(|| format!("latency at {rtt_ms} ms RTT"))?;
        eprintln!(
            "sanity ping p50 at {rtt_ms} ms RTT: {:.3} ms",
            step.ping_p50_ms
        );
        setup_info.sanity_p50_ms.push((rtt_ms, step.ping_p50_ms));
        latency.push((rtt_ms, step.cells));
    }
    print_tls(&mut owner).await?;
    // The latency pools are done: throughput builds its own per cell.
    for v in &variants.all {
        v.close().await;
    }

    // Throughput: rounds with the variant order rotated (ABCD, BCDA, CDAB), the table
    // vacuumed before each; every cell a fresh pool, warmed up untimed, then closed (its
    // connections must go before the next cell's fit the compute's max_connections).
    let w = workload.clone();
    let next: Arc<dyn Fn(OpKind) -> BenchOp + Send + Sync> =
        Arc::new(move |op| w.next(op, &mut rand::rng()));
    let mut runs: HashMap<_, Vec<f64>> = HashMap::new();
    let rounds = if args.no_throughput {
        0
    } else {
        plan.throughput_rounds
    };
    for round in 0..rounds {
        setup::vacuum(&mut owner).await?;
        for kind in runner::rotated(&args.variants, round) {
            for op in ALL_OPS {
                eprintln!("throughput round {} {kind:?} {op:?} ...", round + 1);
                let v = Arc::new(AnyVariant::connect(
                    kind,
                    &info,
                    &target,
                    plan.pool_size,
                    &claims,
                    version,
                    max_rows,
                    timeout_ms,
                )?);
                let rps = runner::throughput(v.clone(), op, &plan.throughput, next.clone()).await;
                v.close().await;
                runs.entry((kind, op)).or_default().push(rps?);
            }
        }
    }
    let mut throughput = Vec::new();
    let mut throughput_runs = Vec::new();
    for &kind in &args.variants {
        for op in ALL_OPS {
            let r = runs.remove(&(kind, op)).unwrap_or_default();
            if !r.is_empty() {
                throughput.push((kind, op, stats::median(&r)));
            }
            throughput_runs.push((kind, op, r));
        }
    }

    let results = Results {
        setup: setup_info,
        latency,
        throughput,
        throughput_runs,
    };
    let json = report::write_outputs(&results, &args.out, args.results_md.as_deref())?;
    if args.variants.contains(&VariantKind::R) {
        println!("{}", report::shipped_markdown(&results));
    } else {
        println!("{}", report::markdown(&results));
    }
    eprintln!("results: {}", json.display());
    if let Some(md) = &args.results_md {
        eprintln!("markdown: {}", md.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run(Args::parse()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bench_data_api::variants::ALL_VARIANTS;

    #[test]
    fn full_plan_is_the_specs() {
        let p = RunPlan::new(false);
        assert_eq!(
            (p.latency.warmup, p.latency.measured, p.latency.repeats),
            (200, 2000, 3)
        );
        assert_eq!(p.sanity_n, 200);
        assert_eq!(p.throughput.concurrency, 32);
        assert_eq!(p.throughput.warmup, Duration::from_secs(2));
        assert_eq!(p.throughput.window, Duration::from_secs(20));
        assert_eq!(p.throughput_rounds, 3);
        assert_eq!(p.pool_size, 32);
    }

    #[test]
    fn quick_plan_is_small() {
        let p = RunPlan::new(true);
        assert_eq!(
            (p.latency.warmup, p.latency.measured, p.latency.repeats),
            (10, 50, 1)
        );
        assert_eq!(p.sanity_n, 20);
        assert_eq!(p.throughput.concurrency, 32);
        assert_eq!(p.throughput.warmup, Duration::from_millis(500));
        assert_eq!(p.throughput.window, Duration::from_secs(2));
        assert_eq!(p.throughput_rounds, 1);
        assert_eq!(p.pool_size, 32);
    }

    #[test]
    fn describe_names_the_mode_and_numbers() {
        let d = RunPlan::new(false).describe(false);
        assert!(d.starts_with("full"), "{d}");
        assert!(d.contains("200 warm-up"), "{d}");
        assert!(d.contains("2000 measured"), "{d}");
        assert!(d.contains("3 repeats"), "{d}");
        assert!(
            d.contains("3 rounds of 32 concurrent, 2 s warm-up + 20 s measured"),
            "{d}"
        );
        let q = RunPlan::new(true).describe(true);
        assert!(q.starts_with("quick"), "{q}");
        assert!(
            q.contains("1 round of 32 concurrent, 0.5 s warm-up + 2 s measured"),
            "{q}"
        );
    }

    #[test]
    fn args_parse_rtts_list() {
        let a = Args::try_parse_from([
            "b",
            "--owner-url",
            "postgresql://x",
            "--branch",
            "00000000-0000-0000-0000-000000000001",
            "--internal-url",
            "http://i",
            "--internal-token",
            "t",
            "--compute-container",
            "c",
            "--rtts-ms",
            "0,0.5,2",
        ])
        .unwrap();
        assert_eq!(a.rtts_ms, vec![0.0, 0.5, 2.0]);
        assert!(!a.quick);
        assert!(!a.seed_only);
    }

    #[test]
    fn args_parse_variants_repeats_and_no_throughput() {
        let base = [
            "b",
            "--owner-url",
            "postgresql://x",
            "--branch",
            "00000000-0000-0000-0000-000000000001",
            "--internal-url",
            "http://i",
            "--internal-token",
            "t",
            "--compute-container",
            "c",
        ];
        let a = Args::try_parse_from(base).unwrap();
        assert_eq!(a.variants, ALL_VARIANTS);
        assert_eq!(a.repeats, None);
        assert!(!a.no_throughput);
        let a = Args::try_parse_from(
            base.into_iter()
                .chain(["--variants", "C,R", "--repeats", "2", "--no-throughput"]),
        )
        .unwrap();
        assert_eq!(a.variants, [VariantKind::C, VariantKind::R]);
        assert_eq!(a.repeats, Some(2));
        assert!(a.no_throughput);
    }

    #[test]
    fn repeats_override_the_plan() {
        let p = RunPlan::new(false).with_repeats(Some(2));
        assert_eq!(p.latency.repeats, 2);
        assert_eq!(p.latency.measured, 2000);
        assert_eq!(RunPlan::new(false).with_repeats(None).latency.repeats, 3);
    }

    #[test]
    fn expected_apps_follow_the_selection() {
        assert_eq!(
            expected_apps(&[VariantKind::C, VariantKind::R]),
            ["bench-c", "chelabase-rest"]
        );
    }

    #[test]
    fn args_parse_seed_only() {
        let a = Args::try_parse_from([
            "b",
            "--owner-url",
            "postgresql://x",
            "--branch",
            "00000000-0000-0000-0000-000000000001",
            "--internal-url",
            "http://i",
            "--internal-token",
            "t",
            "--compute-container",
            "c",
            "--seed-only",
        ])
        .unwrap();
        assert!(a.seed_only);
    }
}
