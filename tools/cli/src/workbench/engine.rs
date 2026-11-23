//! The workbench's seam to Docker and Compose, and the rule every Compose call keeps.
//!
//! # Invariant W-ENV
//!
//! **No Compose call runs with an incomplete `stack.env`.** An env is complete when every
//! variable the current base file and workbench overlay require (`${VAR:?…}` or `${VAR?…}`) has
//! a non-empty value in it ([`missing_required`]). It is not checked to be rendered from the
//! current files: an env with every required variable passes whatever its other values. Compose
//! refuses to load the project otherwise, so a call made anyway fails with a raw interpolation
//! error ("required variable … is missing a value") that names no way out.
//!
//! An env turns incomplete when the checkout moves to a release whose compose files require new
//! variables (the first command after an upgrade), when `up` stopped before it rendered the env
//! (after it left the legacy networks, say), and before the first `up`. So:
//!
//! - every Compose call goes through `Stack::compose_run`/`compose_output`/`compose_cmd`, which
//!   check the env first and refuse with the one command that recovers it (`just workbench up`);
//! - whether the stack runs, and which services run, is asked of Docker by the project label,
//!   never through Compose;
//! - a command that may meet a stale env either goes through `up`, which renders the env before
//!   its first Compose call (`test`, `run`), works through Docker labels only (`down`, `purge`),
//!   or refuses with that one command (`restart`, `logs`, `exec`, `psql`, `--stack`).
//!
//! `upgrade_model_tests` holds every entry point against every state an upgrade can leave a stack
//! in, through a fake [`Engine`].

use super::commands::Stack;

/// What one `docker` call came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Everything the stack lifecycle asks of the host that the tests can't run: `docker` (its state
/// is what the lifecycle reads and changes), `docker compose` with the stack's files and env, and
/// the builds and key files `up` makes, which touch no Docker state the lifecycle reads back.
pub trait Engine {
    /// `docker <args…>`, its output captured.
    fn docker(&self, args: &[String]) -> anyhow::Result<DockerOut>;
    /// `docker compose -p <project> -f <base> -f <overlay> --env-file <env> <args…>` with the
    /// stack's env (see `compose_command`): stdout when `capture`, else inherited (and empty).
    /// Callers go through `Stack::compose_run`/`compose_output`, which keep W-ENV.
    fn compose(&self, stack: &Stack, args: &[String], capture: bool) -> anyhow::Result<String>;
    /// The workbench image's tag, built when missing.
    fn workbench_image(&self, stack: &Stack) -> anyhow::Result<String>;
    /// The Neon images `stack.env` names.
    fn neon_images(&self, stack: &Stack) -> anyhow::Result<Vec<(String, String)>>;
    /// The functions runtime image's tag, built when missing.
    fn functions_runtime_image(&self, stack: &Stack) -> anyhow::Result<String>;
    /// The MCP runtime image's tag, built when missing.
    fn mcp_runtime_image(&self, stack: &Stack) -> anyhow::Result<String>;
    /// `cargo build` of `apps` in the stack's checkout (and the CLI when `cli`).
    fn cargo_build(&self, stack: &Stack, apps: &[&str], cli: bool) -> anyhow::Result<()>;
    /// The stack's keys and auth files; which auth files changed.
    fn secrets(
        &self,
        stack: &Stack,
    ) -> anyhow::Result<chelabase_init::kinds::service_auth::AuthFilesChanged>;
    /// The demo bootstrap, inside the stack.
    fn bootstrap(&self, stack: &Stack) -> anyhow::Result<()>;
    /// The group that owns the Docker socket.
    fn docker_gid(&self) -> anyhow::Result<u32>;
}

/// `docker <args…>`'s stdout; a failure is an error carrying Docker's message.
pub fn docker_output(eng: &dyn Engine, args: &[String]) -> anyhow::Result<String> {
    let out = eng.docker(args)?;
    if !out.ok {
        anyhow::bail!("docker {} failed: {}", args.join(" "), out.stderr.trim());
    }
    Ok(out.stdout)
}

/// The variables the compose texts require (`${VAR:?…}`, `${VAR?…}`; `$$` is a literal `$`), in
/// first-seen order.
pub fn required_vars(texts: &[&str]) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for text in texts {
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'$' {
                i += 1;
                continue;
            }
            let run = b[i..].iter().take_while(|c| **c == b'$').count();
            i += run;
            if run % 2 == 0 || b.get(i) != Some(&b'{') {
                continue;
            }
            let start = i + 1;
            let len = b[start..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
                .count();
            let rest = &b[start + len..];
            if len > 0 && (rest.starts_with(b":?") || rest.starts_with(b"?")) {
                let name = &text[start..start + len];
                if !found.iter().any(|f| f == name) {
                    found.push(name.to_string());
                }
            }
            i = start + len;
        }
    }
    found
}

/// The variables of `required` that `env_text` (a `KEY=VALUE` file) lacks or leaves empty.
pub fn missing_required(env_text: &str, required: &[String]) -> Vec<String> {
    let set: std::collections::HashSet<&str> = env_text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(_, v)| !v.trim().is_empty())
        .map(|(k, _)| k.trim())
        .collect();
    required
        .iter()
        .filter(|r| !set.contains(r.as_str()))
        .cloned()
        .collect()
}

/// Whether `env_text` is complete for the compose texts (W-ENV): every variable they require has a value.
#[cfg(test)]
pub fn stack_env_current(env_text: &str, compose_texts: &[&str]) -> bool {
    missing_required(env_text, &required_vars(compose_texts)).is_empty()
}
