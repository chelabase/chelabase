//! Which stack `workbench test`/`run` use: this checkout's own, or another checkout's (the plan's
//! stack, named with `--stack` or recorded in `.workbench-stack` by `just wt-add`).

use super::commands::missing_services;
use super::registry::{Entry, Registry};
use super::slug::{MAIN, slug_for_dir};
use anyhow::{Context as _, bail};
use std::path::{Path, PathBuf};

/// The file `just wt-add` writes in a task worktree: the plan's stack slug, or empty.
pub const STACK_FILE: &str = ".workbench-stack";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The current checkout's stack (today's behaviour: auto-start, rebuild).
    Own,
    /// Another checkout's stack: never started, rebuilt or recreated.
    Other(String),
}

/// `--stack` (trimmed) first, else the recorded file's contents (trimmed; `None` = no file), else
/// `Own`. A slug equal to `own_slug` is `Own`. An empty flag is an error; an empty record (no
/// flag) is an error whose message names `--stack` and `just workbench up`.
pub fn resolve_target(
    flag: Option<&str>,
    recorded: Option<&str>,
    own_slug: &str,
) -> anyhow::Result<Target> {
    let slug = match (flag.map(str::trim), recorded.map(str::trim)) {
        (Some(""), _) => bail!("--stack needs a stack name (see `just workbench status`)"),
        (Some(flag), _) => flag,
        (None, Some("")) => bail!(
            "{STACK_FILE} is empty: `just wt-add` found no stack for the base branch. Pass \
             --stack <slug> to use a stack, or run `just workbench up` in the base branch's \
             worktree and record its slug in {STACK_FILE}; delete {STACK_FILE} to make this \
             worktree use its own stack"
        ),
        (None, Some(recorded)) => recorded,
        (None, None) => return Ok(Target::Own),
    };
    Ok(if slug == own_slug {
        Target::Own
    } else {
        Target::Other(slug.to_string())
    })
}

/// `up` starts this checkout's own stack, so it is refused where another stack is recorded (the
/// record exists to keep a task worktree from starting a second stack). Ok for no file, an empty
/// record or this checkout's own slug.
pub fn check_up_allowed(recorded: Option<&str>, own_slug: &str) -> anyhow::Result<()> {
    match recorded.map(str::trim) {
        None | Some("") => Ok(()),
        Some(slug) if slug == own_slug => Ok(()),
        Some(slug) => bail!(
            "{STACK_FILE} records stack \"{slug}\": this worktree uses it and never starts a stack \
             of its own. Delete {STACK_FILE} to give this worktree its own stack"
        ),
    }
}

/// `<checkout>/.workbench-stack`'s raw contents; `None` when the file doesn't exist.
pub fn read_recorded(checkout: &Path) -> anyhow::Result<Option<String>> {
    let path = checkout.join(STACK_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Another stack is usable when registered, holding its slot (`running_since`) and with every
/// service of `expected` running.
pub fn check_ready(
    slug: &str,
    entry: Option<&Entry>,
    expected: &[&str],
    running: &[String],
) -> anyhow::Result<()> {
    let Some(entry) = entry else {
        bail!("no stack \"{slug}\" is registered: bring it up first (`just workbench up` in its checkout)");
    };
    if entry.running_since.is_none() {
        bail!(
            "stack \"{slug}\" isn't running: run `just workbench up` in {}",
            entry.checkout.display()
        );
    }
    let missing = missing_services(expected, running);
    if !missing.is_empty() {
        bail!(
            "stack \"{slug}\" is only partly up (not running: {}): run `just workbench up` in {}",
            missing.join(", "),
            entry.checkout.display()
        );
    }
    Ok(())
}

/// `<state>/test/archive.tar.zst` for the stack's own checkout, else
/// `<state>/test/<checkout_slug>.tar.zst`.
pub fn archive_path(stack_state: &Path, stack_slug: &str, checkout_slug: &str) -> PathBuf {
    let name = if stack_slug == checkout_slug {
        "archive.tar.zst".to_string()
    } else {
        format!("{checkout_slug}.tar.zst")
    };
    stack_state.join("test").join(name)
}

/// The slug `wt-add` records for a base branch: `main` -> `main`; else `slug_for_dir` of the
/// branch with `/` -> `-`; returned only if that slug is registered (running or not), else "".
pub fn base_stack_slug(base_branch: &str, registry: &Registry) -> anyhow::Result<String> {
    let base_branch = base_branch.strip_prefix("origin/").unwrap_or(base_branch);
    let slug = if base_branch == MAIN {
        MAIN.to_string()
    } else {
        match slug_for_dir(&base_branch.replace('/', "-")) {
            Ok(slug) => slug,
            Err(_) => return Ok(String::new()),
        }
    };
    Ok(if registry.get(&slug).is_some() {
        slug
    } else {
        String::new()
    })
}
