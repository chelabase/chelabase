//! Latency statistics (microseconds).

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Summary {
    pub n: usize,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
}

/// Nearest-rank percentiles. Sorts `samples_us` in place. Panics on empty input.
pub fn summarize(samples_us: &mut [u64]) -> Summary {
    assert!(!samples_us.is_empty(), "summarize: no samples");
    samples_us.sort_unstable();
    let n = samples_us.len();
    let pct = |p: usize| {
        let rank = (p * n).div_ceil(100).max(1);
        samples_us[rank - 1] as f64
    };
    Summary {
        n,
        p50_us: pct(50),
        p95_us: pct(95),
        p99_us: pct(99),
    }
}

/// Field-wise median of several runs; `n` is the sum of the runs' `n`.
pub fn median_of(runs: &[Summary]) -> Summary {
    assert!(!runs.is_empty(), "median_of: no runs");
    let med = |f: fn(&Summary) -> f64| {
        let mut v: Vec<f64> = runs.iter().map(f).collect();
        v.sort_by(|a, b| a.total_cmp(b));
        let m = v.len() / 2;
        if v.len() % 2 == 1 {
            v[m]
        } else {
            (v[m - 1] + v[m]) / 2.0
        }
    };
    Summary {
        n: runs.iter().map(|r| r.n).sum(),
        p50_us: med(|r| r.p50_us),
        p95_us: med(|r| r.p95_us),
        p99_us: med(|r| r.p99_us),
    }
}

/// The median of `xs` (the mean of the middle two for an even count). Panics on empty input.
pub fn median(xs: &[f64]) -> f64 {
    assert!(!xs.is_empty(), "median: no values");
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_values() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[5.0]), 5.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn summarize_nearest_rank() {
        let mut v: Vec<u64> = (1..=100).collect();
        let s = summarize(&mut v);
        assert_eq!(s.n, 100);
        assert_eq!((s.p50_us, s.p95_us, s.p99_us), (50.0, 95.0, 99.0));
        let mut one = [7u64];
        let s = summarize(&mut one);
        assert_eq!((s.n, s.p50_us, s.p95_us, s.p99_us), (1, 7.0, 7.0, 7.0));
    }

    #[test]
    fn median_of_three_runs() {
        let mk = |n, a, b, c| Summary {
            n,
            p50_us: a,
            p95_us: b,
            p99_us: c,
        };
        let m = median_of(&[
            mk(10, 5.0, 30.0, 9.0),
            mk(20, 1.0, 20.0, 99.0),
            mk(30, 3.0, 10.0, 50.0),
        ]);
        assert_eq!(m, mk(60, 3.0, 20.0, 50.0));
    }
}
