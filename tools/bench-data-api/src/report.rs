//! Results, the D1 verdict and the markdown report.

use crate::runner::Cell;
use crate::variants::VariantKind;
use crate::workload::{ALL_OPS, OpKind};
use std::fmt::Write as _;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Setup {
    pub title: String,
    /// `YYYY-MM-DD`, UTC.
    pub date: String,
    /// `git rev-parse --short HEAD` of the harness.
    pub harness_commit: String,
    /// (RTT in ms, the sanity pings' p50 in ms).
    pub sanity_p50_ms: Vec<(f64, f64)>,
    pub neon_ref: String,
    pub compute_image: String,
    pub compute_image_id: String,
    pub netshoot: String,
    pub pool_size: u32,
    pub plan: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Results {
    pub setup: Setup,
    /// (RTT in ms, cells)
    pub latency: Vec<(f64, Vec<Cell>)>,
    /// (variant, op, requests/s) at RTT 0
    pub throughput: Vec<(VariantKind, OpKind, f64)>,
    /// Every round's requests/s per (variant, op), in round order; `throughput` is the median.
    pub throughput_runs: Vec<(VariantKind, OpKind, Vec<f64>)>,
}

/// The RTTs D1 judges latency at.
const D1_RTTS_MS: [f64; 2] = [0.5, 2.0];
const VARIANTS: [VariantKind; 4] = [
    VariantKind::A,
    VariantKind::B,
    VariantKind::C,
    VariantKind::D,
];

fn cell(r: &Results, rtt: f64, v: VariantKind, op: OpKind) -> Option<&Cell> {
    r.latency
        .iter()
        .find(|(t, _)| (t - rtt).abs() < 1e-9)?
        .1
        .iter()
        .find(|c| c.variant == v && c.op == op)
}

fn tput(r: &Results, v: VariantKind, op: OpKind) -> Option<f64> {
    r.throughput
        .iter()
        .find(|(vv, o, _)| *vv == v && *o == op)
        .map(|t| t.2)
}

/// D1, for B, C and D each against A: p50 and p99 drop by at least 30% at both 0.5 ms and
/// 2 ms RTT for every op, and 0 ms throughput is at least 95% of A's for every op. A missing
/// RTT, cell or throughput figure is a failing reason.
pub fn verdict(r: &Results) -> Vec<(VariantKind, bool, Vec<String>)> {
    let mut out = Vec::new();
    for &v in &VARIANTS[1..] {
        let mut why = Vec::new();
        for rtt in D1_RTTS_MS {
            if !r.latency.iter().any(|(t, _)| (t - rtt).abs() < 1e-9) {
                why.push(format!("no results at {rtt} ms RTT"));
                continue;
            }
            for op in ALL_OPS {
                let (Some(a), Some(c)) = (cell(r, rtt, VariantKind::A, op), cell(r, rtt, v, op))
                else {
                    why.push(format!("{op:?} at {rtt} ms: missing cell"));
                    continue;
                };
                // drop >= 30%  <=>  v <= 0.7 * a, in integer-friendly form
                for (name, base, got) in [
                    ("p50", a.summary.p50_us, c.summary.p50_us),
                    ("p99", a.summary.p99_us, c.summary.p99_us),
                ] {
                    if got * 100.0 > base * 70.0 {
                        let drop = (1.0 - got / base) * 100.0;
                        why.push(format!(
                            "{op:?} at {rtt} ms: {name} drops {drop:.1}% (< 30%)"
                        ));
                    }
                }
            }
        }
        for op in ALL_OPS {
            match (tput(r, VariantKind::A, op), tput(r, v, op)) {
                (Some(a), Some(t)) => {
                    if t * 100.0 < a * 95.0 {
                        why.push(format!(
                            "{op:?}: throughput {:.1}% of A (< 95%)",
                            t / a * 100.0
                        ));
                    }
                }
                _ => why.push(format!("{op:?}: missing throughput")),
            }
        }
        out.push((v, why.is_empty(), why));
    }
    out
}

pub const TITLE: &str = "Data API pipelining benchmark";

/// The tolerance of the shipped-path check: R's p50 within this fraction of C's.
const R_TOLERANCE: f64 = 0.10;

/// One cell of the shipped-path comparison: (RTT in ms, op, C's p50 us, R's p50 us).
pub type RvsC = (f64, OpKind, f64, f64);

/// The shipped path against C: every (RTT, op) that has both cells, and whether the run
/// measured at least one and each R p50 is within [`R_TOLERANCE`] of C's.
pub fn r_vs_c(r: &Results) -> (bool, Vec<RvsC>) {
    let mut rows = Vec::new();
    for (rtt, _) in &r.latency {
        for op in ALL_OPS {
            if let (Some(c), Some(shipped)) = (
                cell(r, *rtt, VariantKind::C, op),
                cell(r, *rtt, VariantKind::R, op),
            ) {
                rows.push((*rtt, op, c.summary.p50_us, shipped.summary.p50_us));
            }
        }
    }
    let ok = !rows.is_empty()
        && rows
            .iter()
            .all(|&(_, _, c, shipped)| (shipped / c - 1.0).abs() <= R_TOLERANCE + 1e-9);
    (ok, rows)
}

/// The "Shipped path (variant R)" table and verdict, as markdown.
pub fn shipped_markdown(r: &Results) -> String {
    let (ok, rows) = r_vs_c(r);
    let mut s = String::new();
    let _ = writeln!(s, "| op | RTT (ms) | C p50 (ms) | R p50 (ms) | R vs C |");
    let _ = writeln!(s, "|---|---|---|---|---|");
    for (rtt, op, c, shipped) in rows {
        let _ = writeln!(
            s,
            "| {op:?} | {rtt} | {} | {} | {:+.1}% |",
            ms(c),
            ms(shipped),
            (shipped / c - 1.0) * 100.0
        );
    }
    let _ = writeln!(
        s,
        "\nVerdict: {} (R's p50 within ±{:.0}% of C's at every RTT and op)",
        if ok { "PASS" } else { "FAIL" },
        R_TOLERANCE * 100.0
    );
    s
}

/// One read op in the C-vs-D rule: (op, C's p50 us, D's p50 us) at 0.5 ms RTT.
pub type CvsD = (OpKind, f64, f64);

/// The spec's C-vs-D rule: C is recommended over D only if its p50 is at least 20% below
/// D's at 0.5 ms RTT on both read ops. `(holds, [(op, C p50 us, D p50 us)])`, or `None`
/// when 0.5 ms wasn't measured.
pub fn c_vs_d(r: &Results) -> Option<(bool, Vec<CvsD>)> {
    let mut rows = Vec::new();
    for op in [OpKind::ByKey, OpKind::List100] {
        let c = cell(r, 0.5, VariantKind::C, op)?.summary.p50_us;
        let d = cell(r, 0.5, VariantKind::D, op)?.summary.p50_us;
        rows.push((op, c, d));
    }
    // at least 20% lower  <=>  c <= 0.8 * d, in integer-friendly form
    let holds = rows.iter().all(|&(_, c, d)| c * 100.0 <= d * 80.0);
    Some((holds, rows))
}

/// `YYYY-MM-DD` (UTC) for a Unix timestamp (the civil-from-days algorithm).
pub fn civil_date(unix_secs: u64) -> String {
    let z = unix_secs / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn c_vs_d_line(r: &Results) -> String {
    let head = "C beats D's p50 by ≥20% at 0.5 ms on the read ops (ByKey, List100)";
    match c_vs_d(r) {
        None => format!("{head}: not measured"),
        Some((holds, rows)) => {
            let detail: Vec<String> = rows
                .iter()
                .map(|&(op, c, d)| {
                    format!(
                        "{op:?}: C {} ms vs D {} ms, {:.1}% lower",
                        ms(c),
                        ms(d),
                        (1.0 - c / d) * 100.0
                    )
                })
                .collect();
            format!(
                "{head}: {} ({})",
                if holds { "yes" } else { "no" },
                detail.join("; ")
            )
        }
    }
}

fn ms(us: f64) -> String {
    format!("{:.2}", us / 1000.0)
}

pub fn markdown(r: &Results) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {}\n", r.setup.title);
    let _ = writeln!(s, "## Setup\n");
    let _ = writeln!(s, "- Date: {} (UTC)", r.setup.date);
    let _ = writeln!(s, "- Harness commit: `{}`", r.setup.harness_commit);
    let _ = writeln!(s, "- Neon ref: `{}`", r.setup.neon_ref);
    let _ = writeln!(s, "- Compute image: `{}`", r.setup.compute_image);
    let _ = writeln!(s, "- Compute image ID: `{}`", r.setup.compute_image_id);
    let _ = writeln!(s, "- Netem image: `{}`", r.setup.netshoot);
    let _ = writeln!(s, "- Pool size: {}", r.setup.pool_size);
    let pings: Vec<String> = r
        .setup
        .sanity_p50_ms
        .iter()
        .map(|(rtt, p50)| format!("{rtt} ms RTT: {p50:.2} ms"))
        .collect();
    let _ = writeln!(s, "- Sanity ping p50 (SELECT 1): {}", pings.join("; "));
    let _ = writeln!(s, "- Plan: {}\n", r.setup.plan);

    for (rtt, cells) in &r.latency {
        let _ = writeln!(s, "## Latency at {rtt} ms RTT (p50 / p95 / p99, ms)\n");
        let vs: Vec<VariantKind> = VARIANTS
            .iter()
            .copied()
            .filter(|v| cells.iter().any(|c| c.variant == *v))
            .collect();
        let _ = write!(s, "| op |");
        for v in &vs {
            let _ = write!(s, " {v:?} |");
        }
        let _ = write!(s, "\n|---|");
        for _ in &vs {
            let _ = write!(s, "---|");
        }
        let _ = writeln!(s);
        for op in ALL_OPS {
            let _ = write!(s, "| {op:?} |");
            for &v in &vs {
                match cells.iter().find(|c| c.variant == v && c.op == op) {
                    Some(c) => {
                        let _ = write!(
                            s,
                            " {} / {} / {} |",
                            ms(c.summary.p50_us),
                            ms(c.summary.p95_us),
                            ms(c.summary.p99_us)
                        );
                    }
                    None => {
                        let _ = write!(s, " - |");
                    }
                }
            }
            let _ = writeln!(s);
        }
        let _ = writeln!(s);
    }

    let rounds = r
        .throughput_runs
        .iter()
        .map(|(_, _, v)| v.len())
        .max()
        .unwrap_or(0);
    let _ = writeln!(
        s,
        "## Throughput at 0 ms RTT (requests/s, median of {rounds} round{})\n",
        if rounds == 1 { "" } else { "s" }
    );
    let _ = write!(s, "| op |");
    for v in VARIANTS {
        let _ = write!(s, " {v:?} |");
    }
    let _ = write!(s, "\n|---|---|---|---|---|\n");
    for op in ALL_OPS {
        let _ = write!(s, "| {op:?} |");
        for v in VARIANTS {
            match tput(r, v, op) {
                Some(t) => {
                    let _ = write!(s, " {t:.1} |");
                }
                None => {
                    let _ = write!(s, " - |");
                }
            }
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "\n## Verdict (D1, against A)\n");
    for (v, pass, why) in verdict(r) {
        if pass {
            let _ = writeln!(s, "- {v:?}: PASS");
        } else {
            let _ = writeln!(s, "- {v:?}: FAIL");
            for w in why {
                let _ = writeln!(s, "  - {w}");
            }
        }
    }
    let _ = writeln!(s, "\n## C vs D (the spec's rule)\n");
    let _ = writeln!(s, "- {}", c_vs_d_line(r));
    s
}

/// The pinned Neon commit, from `<repo_root>/infra/neon/NEON_REF`.
pub fn neon_ref(repo_root: &std::path::Path) -> anyhow::Result<String> {
    let path = repo_root.join("infra/neon/NEON_REF");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read the Neon ref at {}: {e}", path.display()))?;
    let r = text.trim();
    anyhow::ensure!(!r.is_empty(), "{} is empty", path.display());
    Ok(r.to_string())
}

/// The harness's commit (`git rev-parse --short HEAD` in `repo_root`).
pub fn harness_commit(repo_root: &std::path::Path) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "git rev-parse failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `docker inspect` formats: the image reference the container was created from, and the
/// ID of the image it actually runs (a tag can move; the ID can't).
pub const IMAGE_TAG_FORMAT: &str = "{{.Config.Image}}";
pub const IMAGE_ID_FORMAT: &str = "{{.Image}}";

pub fn inspect_argv(container: &str, format: &str) -> Vec<String> {
    ["docker", "inspect", "--format", format, container]
        .map(str::to_owned)
        .to_vec()
}

fn inspect(container: &str, format: &str) -> anyhow::Result<String> {
    let argv = inspect_argv(container, format);
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()?;
    anyhow::ensure!(
        out.status.success(),
        "docker inspect {container} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The compute container's image reference, via `docker inspect`.
pub fn compute_image(container: &str) -> anyhow::Result<String> {
    inspect(container, IMAGE_TAG_FORMAT)
}

/// The compute container's image ID, via `docker inspect`.
pub fn compute_image_id(container: &str) -> anyhow::Result<String> {
    inspect(container, IMAGE_ID_FORMAT)
}

/// Writes `<out>/results-<unix ts>.json` and, if given, the markdown. Returns the JSON path.
pub fn write_outputs(
    r: &Results,
    out: &std::path::Path,
    results_md: Option<&std::path::Path>,
) -> anyhow::Result<std::path::PathBuf> {
    std::fs::create_dir_all(out)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let path = out.join(format!("results-{ts}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(r)?)?;
    if let Some(md) = results_md {
        std::fs::write(md, markdown(r))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Summary;

    fn sum(p50: f64, p99: f64) -> Summary {
        Summary {
            n: 1,
            p50_us: p50,
            p95_us: p50,
            p99_us: p99,
        }
    }

    /// A baseline of p50 1000 / p99 2000 us everywhere; `b` is the p50/p99 for B, C and D
    /// at the 0.5 and 2 ms RTTs; `t` is their throughput relative to A's 100.
    fn results(rtts: &[f64], b: (f64, f64), t: f64) -> Results {
        let mut latency = Vec::new();
        for &rtt in rtts {
            let mut cells = Vec::new();
            for op in ALL_OPS {
                cells.push(Cell {
                    variant: VariantKind::A,
                    op,
                    summary: sum(1000.0, 2000.0),
                });
                for v in [VariantKind::B, VariantKind::C, VariantKind::D] {
                    cells.push(Cell {
                        variant: v,
                        op,
                        summary: sum(b.0, b.1),
                    });
                }
            }
            latency.push((rtt, cells));
        }
        let mut throughput = Vec::new();
        for op in ALL_OPS {
            throughput.push((VariantKind::A, op, 100.0));
            for v in [VariantKind::B, VariantKind::C, VariantKind::D] {
                throughput.push((v, op, t));
            }
        }
        Results {
            setup: Setup {
                neon_ref: "abc".into(),
                compute_image: "img".into(),
                compute_image_id: "sha256:abc".into(),
                title: TITLE.into(),
                date: "2026-10-04".into(),
                harness_commit: "abc1234".into(),
                sanity_p50_ms: vec![(0.0, 0.3), (0.5, 0.85)],
                netshoot: "ns".into(),
                pool_size: 32,
                plan: "quick".into(),
            },
            latency,
            throughput,
            throughput_runs: vec![(VariantKind::A, OpKind::ByKey, vec![90.0, 100.0, 120.0])],
        }
    }

    #[test]
    fn json_keeps_every_throughput_round() {
        let r = results(&[0.0, 0.5, 2.0], (700.0, 1400.0), 95.0);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json["throughput_runs"][0],
            serde_json::json!(["A", "ByKey", [90.0, 100.0, 120.0]])
        );
        let md = markdown(&r);
        assert!(md.contains("median of 3 rounds"), "{md}");
    }

    #[test]
    fn verdict_passes_at_exactly_30_and_95() {
        let r = results(&[0.0, 0.5, 2.0], (700.0, 1400.0), 95.0);
        for (v, pass, why) in verdict(&r) {
            assert!(pass, "{v:?}: {why:?}");
        }
    }

    #[test]
    fn verdict_fails_on_p99_only() {
        let r = results(&[0.5, 2.0], (700.0, 1400.5), 100.0);
        let v = verdict(&r);
        assert_eq!(v.len(), 3);
        for (_, pass, why) in v {
            assert!(!pass);
            assert!(why.iter().all(|w| w.contains("p99")), "{why:?}");
        }
    }

    #[test]
    fn verdict_fails_on_missing_rtt() {
        let r = results(&[0.5], (100.0, 100.0), 100.0);
        for (_, pass, why) in verdict(&r) {
            assert!(!pass);
            assert!(why.iter().any(|w| w.contains("2 ms RTT")), "{why:?}");
        }
    }

    #[test]
    fn verdict_fails_on_throughput_94() {
        let r = results(&[0.5, 2.0], (100.0, 100.0), 94.0);
        for (_, pass, why) in verdict(&r) {
            assert!(!pass);
            assert!(why.iter().all(|w| w.contains("throughput")), "{why:?}");
        }
    }

    #[test]
    fn markdown_has_no_hardware_words() {
        let r = results(&[0.0, 0.5, 2.0], (700.0, 1400.0), 95.0);
        let md = markdown(&r);
        for w in ["CPU", "cores", "threads", "RAM", "GiB"] {
            assert!(!md.contains(w), "{w}");
        }
        assert!(md.contains("0.70 / 0.70 / 1.40"));
        assert!(md.contains("PASS"));
    }

    fn set_p50(r: &mut Results, rtt: f64, v: VariantKind, op: OpKind, p50: f64) {
        let cells = &mut r.latency.iter_mut().find(|(t, _)| *t == rtt).unwrap().1;
        let c = cells
            .iter_mut()
            .find(|c| c.variant == v && c.op == op)
            .unwrap();
        c.summary.p50_us = p50;
    }

    fn with_c_d(c: f64, d: f64) -> Results {
        let mut r = results(&[0.0, 0.5, 2.0], (700.0, 1400.0), 95.0);
        for op in [OpKind::ByKey, OpKind::List100] {
            set_p50(&mut r, 0.5, VariantKind::C, op, c);
            set_p50(&mut r, 0.5, VariantKind::D, op, d);
        }
        // Writes don't count for the rule.
        set_p50(&mut r, 0.5, VariantKind::C, OpKind::Insert, 5000.0);
        r
    }

    #[test]
    fn c_vs_d_yes_at_exactly_20_percent_on_reads() {
        let (yes, rows) = c_vs_d(&with_c_d(800.0, 1000.0)).unwrap();
        assert!(yes, "{rows:?}");
        assert_eq!(
            rows,
            vec![
                (OpKind::ByKey, 800.0, 1000.0),
                (OpKind::List100, 800.0, 1000.0)
            ]
        );
        let md = markdown(&with_c_d(800.0, 1000.0));
        assert!(
            md.contains(
                "C beats D's p50 by ≥20% at 0.5 ms on the read ops (ByKey, List100): yes \
                 (ByKey: C 0.80 ms vs D 1.00 ms, 20.0% lower; List100: C 0.80 ms vs D 1.00 ms, \
                 20.0% lower)"
            ),
            "{md}"
        );
    }

    #[test]
    fn c_vs_d_no_below_20_percent() {
        let (yes, _) = c_vs_d(&with_c_d(810.0, 1000.0)).unwrap();
        assert!(!yes);
        assert!(markdown(&with_c_d(810.0, 1000.0)).contains("(ByKey, List100): no ("));
    }

    #[test]
    fn c_vs_d_needs_the_half_ms_rtt() {
        let r = results(&[0.0, 2.0], (700.0, 1400.0), 95.0);
        assert!(c_vs_d(&r).is_none());
        assert!(markdown(&r).contains("(ByKey, List100): not measured"));
    }

    #[test]
    fn markdown_has_title_date_commit_and_pings() {
        let md = markdown(&results(&[0.0, 0.5, 2.0], (700.0, 1400.0), 95.0));
        assert!(md.starts_with("# Data API pipelining benchmark\n"), "{md}");
        assert!(md.contains("- Date: 2026-10-04 (UTC)"), "{md}");
        assert!(md.contains("- Harness commit: `abc1234`"), "{md}");
        assert!(
            md.contains("- Sanity ping p50 (SELECT 1): 0 ms RTT: 0.30 ms; 0.5 ms RTT: 0.85 ms"),
            "{md}"
        );
    }

    /// `results` plus an R cell per C cell, R's p50 `factor` times C's.
    fn with_r(factor: f64) -> Results {
        let mut r = results(&[0.5, 2.0], (700.0, 1400.0), 95.0);
        for (_, cells) in r.latency.iter_mut() {
            let rs: Vec<Cell> = cells
                .iter()
                .filter(|c| c.variant == VariantKind::C)
                .map(|c| {
                    let mut c = c.clone();
                    c.variant = VariantKind::R;
                    c.summary.p50_us *= factor;
                    c
                })
                .collect();
            cells.extend(rs);
        }
        r
    }

    #[test]
    fn shipped_table_passes_within_ten_percent() {
        let md = shipped_markdown(&with_r(1.10));
        assert!(md.contains("Verdict: PASS"), "{md}");
        assert!(md.contains("| ByKey | 0.5 | 0.70 | 0.77 | +10.0% |"), "{md}");
        let (ok, rows) = r_vs_c(&with_r(0.92));
        assert!(ok);
        assert_eq!(rows.len(), 8);
    }

    #[test]
    fn shipped_table_fails_beyond_ten_percent_or_without_r() {
        let md = shipped_markdown(&with_r(1.11));
        assert!(md.contains("Verdict: FAIL"), "{md}");
        let (ok, _) = r_vs_c(&with_r(0.89));
        assert!(!ok);
        let none = results(&[0.5, 2.0], (700.0, 1400.0), 95.0);
        let (ok, rows) = r_vs_c(&none);
        assert!(!ok && rows.is_empty());
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(1_000_000_000), "2001-09-09");
        assert_eq!(civil_date(1_791_086_956), "2026-10-04");
    }

    #[test]
    fn markdown_shows_the_compute_image_id() {
        let r = results(&[0.0, 0.5, 2.0], (700.0, 1400.0), 95.0);
        assert!(
            markdown(&r).contains("- Compute image ID: `sha256:abc`"),
            "{}",
            markdown(&r)
        );
    }

    #[test]
    fn image_id_comes_from_docker_inspect_image() {
        assert_eq!(
            inspect_argv("c1", IMAGE_ID_FORMAT),
            ["docker", "inspect", "--format", "{{.Image}}", "c1"]
        );
        assert_eq!(
            inspect_argv("c1", IMAGE_TAG_FORMAT),
            ["docker", "inspect", "--format", "{{.Config.Image}}", "c1"]
        );
    }

    #[test]
    fn neon_ref_missing_errors_clearly() {
        let e = neon_ref(std::path::Path::new("/nonexistent-root")).unwrap_err();
        assert!(e.to_string().contains("NEON_REF"), "{e}");
    }
}
