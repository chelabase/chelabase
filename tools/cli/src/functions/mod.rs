//! `chelabase functions …`: deploy a project's functions (bundled locally, uploaded through the
//! management API), redeploy on save, and manage functions and their secrets on a branch.

pub mod bundle;
pub mod config;
pub mod watch;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use config::Config;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Where the commands act: the instance, the database and the branch.
#[derive(Args, Debug, Default)]
pub struct Target {
    /// The instance's API URL (default: chelabase.toml's, else the only login)
    #[arg(long, global = true, env = "CHELABASE_URL")]
    pub url: Option<String>,
    /// The database id (default: chelabase.toml's)
    #[arg(long, global = true, env = "CHELABASE_DATABASE")]
    pub database: Option<String>,
    /// The branch id (default: chelabase.toml's, else the database's main branch)
    #[arg(long, global = true, env = "CHELABASE_BRANCH")]
    pub branch: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Bundle and deploy functions of chelabase.toml (all when none is named), made active
    Deploy { names: Vec<String> },
    /// Deploy, then with --watch redeploy each function whose files change
    Dev {
        #[arg(long)]
        watch: bool,
        names: Vec<String>,
    },
    /// The branch's functions, or with a name that function's deployments
    List { name: Option<String> },
    /// Turn a function on on the branch
    Enable { name: String },
    /// Turn a function off on the branch
    Disable { name: String },
    /// Make an earlier deployment the active one
    Rollback { name: String, deployment: String },
    /// A function's secrets on the branch
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum SecretsCommand {
    /// Set a secret; the value comes from stdin or a prompt, never the command line
    Set { function: String, secret: String },
    /// Remove a secret
    Unset { function: String, secret: String },
    /// The secrets' names (values never come back)
    List { function: String },
}

struct Api {
    http: chelabase_http::Client,
    url: String,
    base: String,
}

impl Api {
    async fn send(
        &self,
        method: reqwest::Method,
        tail: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Value> {
        let token = crate::login::bearer(&self.http, &self.url).await?;
        let path = format!("{}{tail}", self.base);
        let mut req = self
            .http
            .request(method.clone(), format!("{}{path}", self.url))
            .bearer_auth(token);
        if let Some(b) = body {
            req = req.header("content-type", "application/json").body(b);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("{method} {path}"))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        match status.as_u16() {
            200..=299 => Ok(value),
            401 => bail!(
                "{} refused the session: run `chelabase login --url {}`",
                self.url,
                self.url
            ),
            _ => bail!(
                "{method} {path}: {status} {}",
                value["error"]["message"]
                    .as_str()
                    .unwrap_or(&value.to_string())
            ),
        }
    }

    async fn json(&self, method: reqwest::Method, tail: &str, body: Value) -> Result<Value> {
        self.send(method, tail, Some(body.to_string().into_bytes()))
            .await
    }

    /// Bundles and deploys `name` (made active), creating it and turning it on the first time;
    /// the deployment's id.
    async fn deploy(
        &self,
        root: &Path,
        name: &str,
        f: &config::Function,
    ) -> Result<(String, BTreeSet<PathBuf>)> {
        let built = bundle::build(root, name, f)?;
        let function = self
            .json(reqwest::Method::PUT, &format!("/{name}"), json!({}))
            .await?;
        // The manifest's bytes exactly as hashed: the server stores them unchanged.
        let mut body = br#"{"activate":true,"sha256":""#.to_vec();
        body.extend_from_slice(built.sha256.as_bytes());
        body.extend_from_slice(br#"","manifest":"#);
        body.extend_from_slice(&built.manifest);
        body.push(b'}');
        let d = self
            .send(
                reqwest::Method::POST,
                &format!("/{name}/deployments"),
                Some(body),
            )
            .await?;
        let id = d["id"].as_str().context("the deployment's id")?.to_string();
        let mut patch = serde_json::Map::new();
        if function["deployment_id"].is_null() {
            patch.insert("enabled".into(), json!(true));
        }
        if let Some(auth) = f.auth
            && function["auth"] != json!(auth)
        {
            patch.insert("auth".into(), json!(auth));
        }
        if !patch.is_empty() {
            self.json(
                reqwest::Method::PATCH,
                &format!("/{name}"),
                Value::Object(patch),
            )
            .await?;
        }
        Ok((id, built.inputs))
    }
}

/// The value of a secret: all of stdin when it isn't a terminal (one trailing newline dropped),
/// else a prompt that doesn't echo.
fn read_secret(input: &mut dyn std::io::BufRead, terminal: bool) -> Result<String> {
    if terminal {
        return crate::login::prompt("Value", true, input);
    }
    let mut value = String::new();
    input
        .read_to_string(&mut value)
        .context("reading the secret from stdin")?;
    let value = value
        .strip_suffix('\n')
        .map(|v| v.strip_suffix('\r').unwrap_or(v))
        .unwrap_or(&value);
    Ok(value.to_string())
}

pub async fn run(target: Target, command: Command) -> Result<()> {
    let found = Config::find(&std::env::current_dir()?);
    let (root, config) = match (&command, found) {
        (_, Ok((root, c))) => (root.canonicalize()?, c),
        // Only these need the project; the others work without one, but not with a broken one.
        (Command::Deploy { .. } | Command::Dev { .. }, Err(e)) => return Err(e),
        (_, Err(e)) if !e.to_string().starts_with("no chelabase.toml") => return Err(e),
        (_, Err(_)) => (PathBuf::new(), Config::default()),
    };
    let url = crate::login::instance(target.url.as_deref(), config.project.url.as_deref())?;
    let database = target
        .database
        .or(config.project.database.clone())
        .context("which database? pass --database or set [project] database in chelabase.toml")?;
    let branch = target
        .branch
        .or(config.project.branch.clone())
        .unwrap_or_else(|| database.clone());
    let api = Api {
        http: chelabase_http::client_builder()
            .build()
            .context("an HTTP client")?,
        base: format!("/v1/databases/{database}/branches/{branch}/functions"),
        url,
    };
    use reqwest::Method;
    match command {
        Command::Deploy { names } => {
            for (name, f) in config.select(&names)? {
                let (id, _) = api.deploy(&root, name, f).await?;
                println!("{name}: deployed {id}");
            }
        }
        Command::Dev { watch, names } => dev(&api, &root, config, &names, watch).await?,
        Command::List { name: None } => {
            for f in api
                .send(Method::GET, "", None)
                .await?
                .as_array()
                .into_iter()
                .flatten()
            {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    f["name"].as_str().unwrap_or(""),
                    if f["enabled"] == json!(true) {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    f["auth"].as_str().unwrap_or("-"),
                    f["deployment_id"].as_str().unwrap_or("-"),
                    f["last_error"].as_str().unwrap_or(""),
                );
            }
        }
        Command::List { name: Some(name) } => {
            let ds = api
                .send(Method::GET, &format!("/{name}/deployments"), None)
                .await?;
            for d in ds.as_array().into_iter().flatten() {
                let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
                println!(
                    "{}\t{}\t{}\t{}",
                    s("id"),
                    s("created_at"),
                    s("compatibility_date"),
                    s("note")
                );
            }
        }
        Command::Enable { name } => {
            api.json(
                Method::PATCH,
                &format!("/{name}"),
                json!({ "enabled": true }),
            )
            .await?;
            eprintln!("{name}: enabled");
        }
        Command::Disable { name } => {
            api.json(
                Method::PATCH,
                &format!("/{name}"),
                json!({ "enabled": false }),
            )
            .await?;
            eprintln!("{name}: disabled");
        }
        Command::Rollback { name, deployment } => {
            api.json(
                Method::PATCH,
                &format!("/{name}"),
                json!({ "active_deployment_id": deployment }),
            )
            .await?;
            eprintln!("{name}: {deployment} is active");
        }
        Command::Secrets { command } => match command {
            SecretsCommand::Set { function, secret } => {
                let terminal = std::io::stdin().is_terminal();
                let value = read_secret(&mut std::io::stdin().lock(), terminal)?;
                let tail = format!("/{function}/secrets/{secret}");
                api.json(Method::PUT, &tail, json!({ "value": value }))
                    .await?;
                eprintln!("{function}: {secret} set");
            }
            SecretsCommand::Unset { function, secret } => {
                api.send(
                    Method::DELETE,
                    &format!("/{function}/secrets/{secret}"),
                    None,
                )
                .await?;
                eprintln!("{function}: {secret} removed");
            }
            SecretsCommand::List { function } => {
                for n in api
                    .send(Method::GET, &format!("/{function}/secrets"), None)
                    .await?
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!("{}", n.as_str().unwrap_or(""));
                }
            }
        },
    }
    Ok(())
}

/// Deploys the selected functions, then with `watch` redeploys each one whose inputs change (all
/// of them when chelabase.toml changes). A failed build prints its error and leaves the
/// previous deployment live.
async fn dev(
    api: &Api,
    root: &Path,
    mut config: Config,
    names: &[String],
    watch: bool,
) -> Result<()> {
    let mut inputs: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
    let (_watcher, mut rx) = if watch {
        watch::watch(root).map(|(w, rx)| (Some(w), Some(rx)))?
    } else {
        (None, None)
    };
    let mut batch: Option<std::collections::HashSet<PathBuf>> = None;
    loop {
        let started = Instant::now();
        let toml_changed = batch
            .as_ref()
            .is_some_and(|b| b.contains(&root.join(config::FILE)));
        if toml_changed {
            match Config::find(root) {
                Ok((_, c)) => config = c,
                Err(e) => eprintln!("{e:#}"),
            }
        }
        for (name, f) in config.select(names)? {
            let changed = match (&batch, inputs.get(name)) {
                (None, _) | (_, None) => true,
                (Some(b), Some(i)) => toml_changed || watch::affects(b, i),
            };
            if !changed {
                continue;
            }
            match api.deploy(root, name, f).await {
                Ok((id, i)) => {
                    let ms = started.elapsed().as_millis();
                    println!("{name}: deployed {id} ({ms} ms after the change)");
                    inputs.insert(name.to_string(), i);
                }
                Err(e) if watch => {
                    eprintln!("{name}: not deployed, the previous deployment stays live:\n{e:#}");
                    inputs
                        .entry(name.to_string())
                        .or_default()
                        .insert(root.join(&f.entry));
                }
                Err(e) => return Err(e),
            }
        }
        let Some(rx) = rx.as_mut() else { return Ok(()) };
        batch = watch::next_batch(rx, watch::DEBOUNCE).await;
        if batch.is_none() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(flatten)]
        target: Target,
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn a_secret_value_is_never_an_argument() {
        let cli = Cli::try_parse_from(["functions", "secrets", "set", "hello", "API_KEY"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Secrets {
                command: SecretsCommand::Set { .. }
            }
        ));
        for argv in [
            &["functions", "secrets", "set", "hello", "API_KEY", "v4lue"][..],
            &[
                "functions",
                "secrets",
                "set",
                "hello",
                "API_KEY",
                "--value",
                "v4lue",
            ],
            &["functions", "secrets", "set", "hello", "API_KEY=v4lue", "x"],
        ] {
            assert!(
                Cli::try_parse_from(argv).is_err(),
                "{argv:?} must be refused"
            );
        }
    }

    #[test]
    fn a_secret_comes_from_all_of_stdin_or_one_prompted_line() {
        let mut piped: &[u8] = b"-----BEGIN-----\nline\n-----END-----\n";
        assert_eq!(
            read_secret(&mut piped, false).unwrap(),
            "-----BEGIN-----\nline\n-----END-----"
        );
        let mut typed: &[u8] = b"typed\nrest\n";
        assert_eq!(read_secret(&mut typed, true).unwrap(), "typed");
    }

    #[test]
    fn target_flags_go_anywhere() {
        let cli = Cli::try_parse_from([
            "functions",
            "deploy",
            "hello",
            "--branch",
            "b",
            "--database",
            "d",
        ])
        .unwrap();
        assert_eq!(
            (cli.target.database.as_deref(), cli.target.branch.as_deref()),
            (Some("d"), Some("b"))
        );
        let Command::Deploy { names } = cli.command else {
            panic!()
        };
        assert_eq!(names, ["hello"]);
    }
}
