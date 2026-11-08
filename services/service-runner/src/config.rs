//! The runner's settings, from the environment (blank values count as unset).

use std::time::Duration;

use crate::spec::{Caps, DEFAULT_CPUS, DEFAULT_MEMORY_MB, MIN_CPUS, MIN_MEMORY_MB};

/// The functions service's bearer (every call).
pub const FUNCTIONS_TOKEN: &str = "CHELABASE_FUNCTIONS_SERVICE_RUNNER_TOKEN";
/// The worker's bearer (every call but ensure).
pub const WORKER_TOKEN: &str = "CHELABASE_WORKER_SERVICE_RUNNER_TOKEN";
/// The shortest bearer accepted (chelabase-init makes 32-character ones).
pub const MIN_TOKEN_LEN: usize = 32;
/// The longest stack name (as `chelabase-neon`'s `check_stack`).
const MAX_STACK_LEN: usize = 40;

#[derive(Debug)]
pub struct Config {
    /// `CHELABASE_SERVICE_RUNNER_ADDR` (default `127.0.0.1:8094`): comma-separated `host:port`s,
    /// each resolved at start; compose names the runner's alias on its control network.
    pub addr: String,
    /// `CHELABASE_STACK` (default `main`): the only stack whose containers it touches.
    pub stack: String,
    /// `CHELABASE_COMPOSE_PROJECT` (required): the compose project whose `functions` service
    /// the runner attaches to each service's network.
    pub project: String,
    pub functions_token: String,
    pub worker_token: String,
    pub caps: Caps,
    /// `CHELABASE_SERVICE_RUNNER_RECONCILE_SECS` (default 30).
    pub reconcile_every: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let token = |name: &str| -> anyhow::Result<String> {
            let value = chelabase_types::secret_env::lookup_secret(&get, name)
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| anyhow::anyhow!("{name} (or {name}_FILE) is required"))?;
            anyhow::ensure!(
                value.len() >= MIN_TOKEN_LEN,
                "{name} must be at least {MIN_TOKEN_LEN} characters"
            );
            Ok(value)
        };
        let functions_token = token(FUNCTIONS_TOKEN)?;
        let worker_token = token(WORKER_TOKEN)?;
        anyhow::ensure!(
            functions_token != worker_token,
            "{FUNCTIONS_TOKEN} and {WORKER_TOKEN} must differ: each grants its own calls"
        );
        let stack = non_blank("CHELABASE_STACK").unwrap_or_else(|| "main".to_string());
        let stack_ok = stack.len() <= MAX_STACK_LEN
            && stack.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && stack
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        anyhow::ensure!(
            stack_ok,
            "CHELABASE_STACK {stack:?} is not [a-z0-9][a-z0-9-]*, at most {MAX_STACK_LEN} characters"
        );
        let project = non_blank("CHELABASE_COMPOSE_PROJECT")
            .ok_or_else(|| anyhow::anyhow!("CHELABASE_COMPOSE_PROJECT is required"))?;
        // Compose's own rule for project names.
        let project_ok = project.len() <= 128
            && project.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && project
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b));
        anyhow::ensure!(
            project_ok,
            "CHELABASE_COMPOSE_PROJECT {project:?} is not a compose project name ([a-z0-9][a-z0-9_-]*)"
        );
        let memory_mb: u32 = match non_blank("CHELABASE_SERVICE_RUNNER_MEMORY_CAP_MB") {
            None => DEFAULT_MEMORY_MB,
            Some(v) => v.parse().ok().filter(|m| *m >= MIN_MEMORY_MB).ok_or_else(|| {
                anyhow::anyhow!("CHELABASE_SERVICE_RUNNER_MEMORY_CAP_MB must be an integer of at least {MIN_MEMORY_MB}")
            })?,
        };
        let cpus: f64 = match non_blank("CHELABASE_SERVICE_RUNNER_CPU_CAP") {
            None => DEFAULT_CPUS,
            Some(v) => v
                .parse::<f64>()
                .ok()
                .filter(|c| c.is_finite() && *c >= MIN_CPUS && *c <= 256.0)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "CHELABASE_SERVICE_RUNNER_CPU_CAP must be a number from {MIN_CPUS} to 256"
                    )
                })?,
        };
        let secs: u64 = match non_blank("CHELABASE_SERVICE_RUNNER_RECONCILE_SECS") {
            None => 30,
            Some(v) => v.parse().ok().filter(|s| *s >= 1).ok_or_else(|| {
                anyhow::anyhow!(
                    "CHELABASE_SERVICE_RUNNER_RECONCILE_SECS must be a positive integer"
                )
            })?,
        };
        Ok(Config {
            addr: non_blank("CHELABASE_SERVICE_RUNNER_ADDR")
                .unwrap_or_else(|| "127.0.0.1:8094".into()),
            stack,
            project,
            functions_token,
            worker_token,
            caps: Caps { memory_mb, cpus },
            reconcile_every: Duration::from_secs(secs),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    const OK: [(&str, &str); 3] = [
        (FUNCTIONS_TOKEN, "functions-0123456789abcdef0123456789ab"),
        (WORKER_TOKEN, "worker-0123456789abcdef0123456789abcdef"),
        ("CHELABASE_COMPOSE_PROJECT", "chelabase-feat-x"),
    ];

    #[test]
    fn defaults() {
        let c = Config::from_lookup(env(&OK)).unwrap();
        assert_eq!(c.stack, "main");
        assert_eq!(c.caps, Caps::default());
        assert_eq!(c.reconcile_every, Duration::from_secs(30));
        assert_eq!(c.addr, "127.0.0.1:8094");
    }

    #[test]
    fn refusals() {
        let with = |k: &str, v: &str| {
            let mut e: Vec<(&str, &str)> = OK.iter().copied().filter(|(n, _)| *n != k).collect();
            e.push((k, v));
            Config::from_lookup(env(&e))
        };
        assert!(with(FUNCTIONS_TOKEN, "short").is_err());
        assert!(
            with(WORKER_TOKEN, OK[0].1).is_err(),
            "one value for both bearers"
        );
        for n in ["Chelabase", "-x", "a/b", "a.b", "container:abc", ""] {
            assert!(with("CHELABASE_COMPOSE_PROJECT", n).is_err(), "{n}");
        }
        assert!(with("CHELABASE_STACK", "Main").is_err());
        assert!(with("CHELABASE_SERVICE_RUNNER_MEMORY_CAP_MB", "8").is_err());
        assert!(with("CHELABASE_SERVICE_RUNNER_CPU_CAP", "NaN").is_err());
        assert!(with("CHELABASE_SERVICE_RUNNER_CPU_CAP", "0").is_err());
        let c = with("CHELABASE_SERVICE_RUNNER_MEMORY_CAP_MB", "2048").unwrap();
        assert_eq!(c.caps.memory_mb, 2048);
        let missing: Vec<_> = OK.iter().copied().skip(1).collect();
        assert!(Config::from_lookup(env(&missing)).is_err());
    }
}
