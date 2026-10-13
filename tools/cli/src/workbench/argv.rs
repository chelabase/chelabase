//! Command lines the workbench runs: `docker compose` for one stack, and nextest's archive (on the
//! host) and run (in the stack's runner).

use std::path::Path;

/// One stack's `docker compose`: every call names its project, file and env file.
pub struct Compose<'a> {
    pub project: &'a str,
    pub file: &'a Path,
    pub env_file: &'a Path,
}

impl Compose<'_> {
    /// `docker` arguments: `compose -p <project> -f <file> --env-file <env> <rest…>`.
    pub fn args(&self, rest: &[&str]) -> Vec<String> {
        let mut args = vec![
            "compose".to_string(),
            "-p".to_string(),
            self.project.to_string(),
            "-f".to_string(),
            self.file.display().to_string(),
            "--env-file".to_string(),
            self.env_file.display().to_string(),
        ];
        args.extend(rest.iter().map(|s| s.to_string()));
        args
    }
}

/// `cargo` arguments that build the test archive: the selection, or the whole workspace.
pub fn nextest_archive_args(selection: &[String], archive: &Path) -> Vec<String> {
    let mut args = vec![
        "nextest".to_string(),
        "archive".to_string(),
        "--archive-file".to_string(),
        archive.display().to_string(),
    ];
    if selection.is_empty() {
        args.push("--workspace".to_string());
    } else {
        args.extend(selection.iter().cloned());
    }
    args
}

/// `cargo` arguments that run the archive in the runner, with the checkout as the workspace.
pub fn nextest_run_args(run: &[String], archive: &Path, checkout: &Path) -> Vec<String> {
    let mut args = vec![
        "nextest".to_string(),
        "run".to_string(),
        "--archive-file".to_string(),
        archive.display().to_string(),
        "--workspace-remap".to_string(),
        checkout.display().to_string(),
    ];
    args.extend(run.iter().cloned());
    args
}
