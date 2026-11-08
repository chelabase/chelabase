//! `chelabase services …`: container services on a branch. A service is a function of kind
//! `service`, so these go through the same management routes as `chelabase functions`.

use crate::functions::config::Config;
use crate::functions::{Api, Target, connect};
use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Deploy a container image (creating and enabling the service the first time); prints the
    /// deployment's id. A running service switches once the new container is healthy: the
    /// command waits for that and exits non-zero when it never is
    Deploy(Deploy),
    /// The branch's services
    List,
    /// Turn a service on on the branch
    Enable { name: String },
    /// Turn a service off on the branch (its container stops)
    Disable { name: String },
    /// The service container's last 200 log lines
    Logs { name: String },
    /// Roll back to an earlier deployment (once its container is healthy)
    Rollback { name: String, deployment: String },
}

#[derive(Args, Debug)]
pub struct Deploy {
    pub name: String,
    /// `name:tag` or `name@sha256:<digest>` from a public registry (a tag is pinned to its digest)
    #[arg(long)]
    pub image: String,
    /// The port the image listens on (it gets it as PORT)
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: u16,
    /// The health check's path (default /)
    #[arg(long)]
    pub health: Option<String>,
    /// Memory limit: MiB, or with an m or g suffix (e.g. 256m, 1g)
    #[arg(long, value_parser = parse_memory)]
    pub memory: Option<u32>,
    /// CPU limit, e.g. 0.5
    #[arg(long)]
    pub cpus: Option<f64>,
}

/// A memory limit in MiB: `256`, `256m` or `1g` (any case).
pub fn parse_memory(s: &str) -> Result<u32, String> {
    let lower = s.trim().to_ascii_lowercase();
    let (digits, factor) = match lower.strip_suffix('g') {
        Some(d) => (d, 1024),
        None => (lower.strip_suffix('m').unwrap_or(&lower), 1),
    };
    digits
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(factor))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("`{s}` isn't a memory size: MiB, or with an m or g suffix"))
}

/// The deployment's body: a container spec.
pub fn deploy_body(d: &Deploy) -> Value {
    let mut spec = json!({ "backend": "container", "image": d.image, "port": d.port });
    if let Some(h) = &d.health {
        spec["health_path"] = json!(h);
    }
    if d.memory.is_some() || d.cpus.is_some() {
        let mut r = json!({});
        if let Some(m) = d.memory {
            r["memory_mb"] = json!(m);
        }
        if let Some(c) = d.cpus {
            r["cpus"] = json!(c);
        }
        spec["resources"] = r;
    }
    json!({ "spec": spec })
}

/// The project's `chelabase.toml` when there is one (services need none).
fn project() -> Result<Config> {
    match Config::find(&std::env::current_dir()?) {
        Ok((_, c)) => Ok(c),
        Err(e) if e.to_string().starts_with("no chelabase.toml") => Ok(Config::default()),
        Err(e) => Err(e),
    }
}

async fn deploy(api: &Api, d: &Deploy) -> Result<()> {
    let name = &d.name;
    let service = api
        .json(
            Method::PUT,
            &format!("/{name}"),
            json!({ "kind": "service" }),
        )
        .await?;
    if service["kind"] != "service" {
        bail!("{name} is a function, not a service");
    }
    if let Some(secret) = service["signing_secret"].as_str() {
        println!("{name}: signing secret {secret} (shown only now)");
    }
    let made = api
        .json(
            Method::POST,
            &format!("/{name}/deployments"),
            deploy_body(d),
        )
        .await?;
    let id = made["id"].as_str().context("the deployment's id")?;
    println!("{name}: deployed {id}");
    // A running service rolls over in the background; wait for its outcome.
    if made["rollout"] == "pending" {
        api.await_rollout(name, id).await?;
        eprintln!("{name}: {id} is live");
    }
    if service["deployment_id"].is_null() {
        api.set_enabled(name, true).await?;
    }
    Ok(())
}

pub async fn run(target: Target, command: Command) -> Result<()> {
    let api = connect(target, &project()?)?;
    match command {
        Command::Deploy(d) => deploy(&api, &d).await?,
        Command::List => {
            let list = api.send(Method::GET, "", None).await?;
            for f in list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| f["kind"] == "service")
            {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    f["name"].as_str().unwrap_or(""),
                    if f["enabled"] == json!(true) {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    f["health"].as_str().unwrap_or("-"),
                    f["deployment_id"].as_str().unwrap_or("-"),
                    f["last_error"].as_str().unwrap_or(""),
                );
            }
        }
        Command::Enable { name } => api.set_enabled(&name, true).await?,
        Command::Disable { name } => api.set_enabled(&name, false).await?,
        Command::Logs { name } => print!(
            "{}",
            api.text(Method::GET, &format!("/{name}/logs"), None)
                .await?
        ),
        Command::Rollback { name, deployment } => api.rollback(&name, &deployment).await?,
    }
    Ok(())
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

    fn deploy_args(args: &[&str]) -> Result<Deploy, clap::Error> {
        let argv = ["services", "deploy"]
            .into_iter()
            .chain(args.iter().copied());
        Cli::try_parse_from(argv).map(|c| match c.command {
            Command::Deploy(d) => d,
            other => panic!("not deploy: {other:?}"),
        })
    }

    #[test]
    fn deploy_parses_and_builds_a_container_spec() {
        let d = deploy_args(&[
            "echo",
            "--image",
            "mccutchen/go-httpbin:2.26.0",
            "--port",
            "8080",
            "--health",
            "/status/200",
            "--memory",
            "256m",
            "--cpus",
            "0.5",
            "--branch",
            "b",
        ])
        .unwrap();
        assert_eq!(
            deploy_body(&d),
            json!({"spec": {"backend": "container", "image": "mccutchen/go-httpbin:2.26.0", "port": 8080,
                            "health_path": "/status/200", "resources": {"memory_mb": 256, "cpus": 0.5}}})
        );
        let d = deploy_args(&["echo", "--image", "img:1", "--port", "80"]).unwrap();
        assert_eq!(
            deploy_body(&d),
            json!({"spec": {"backend": "container", "image": "img:1", "port": 80}})
        );
    }

    #[test]
    fn deploy_needs_an_image_and_a_valid_port() {
        assert!(
            deploy_args(&["echo", "--port", "8080"]).is_err(),
            "--image is required"
        );
        assert!(
            deploy_args(&["echo", "--image", "i"]).is_err(),
            "--port is required"
        );
        assert!(deploy_args(&["echo", "--image", "i", "--port", "0"]).is_err());
        assert!(deploy_args(&["echo", "--image", "i", "--port", "70000"]).is_err());
        assert!(
            deploy_args(&["echo", "--image", "i", "--port", "80", "--memory", "lots"]).is_err()
        );
    }

    #[test]
    fn memory_sizes_are_mib() {
        assert_eq!(parse_memory("256"), Ok(256));
        assert_eq!(parse_memory("256M"), Ok(256));
        assert_eq!(parse_memory("1g"), Ok(1024));
        assert!(parse_memory("0").is_err());
        assert!(parse_memory("1.5g").is_err());
        assert!(parse_memory("9999999g").is_err(), "overflow");
    }

    #[test]
    fn the_other_commands_parse() {
        let cmd = |args: &[&str]| {
            Cli::try_parse_from(["services"].into_iter().chain(args.iter().copied()))
                .map(|c| c.command)
        };
        assert!(matches!(cmd(&["list"]).unwrap(), Command::List));
        assert!(matches!(
            cmd(&["logs", "echo"]).unwrap(),
            Command::Logs { .. }
        ));
        assert!(matches!(
            cmd(&["enable", "echo"]).unwrap(),
            Command::Enable { .. }
        ));
        assert!(matches!(
            cmd(&["disable", "echo"]).unwrap(),
            Command::Disable { .. }
        ));
        let Command::Rollback { name, deployment } = cmd(&["rollback", "echo", "d1"]).unwrap()
        else {
            panic!()
        };
        assert_eq!((name.as_str(), deployment.as_str()), ("echo", "d1"));
        assert!(cmd(&["rollback", "echo"]).is_err());
    }

    /// A rollover is pending until the service shows the deployment active with no rollover, and
    /// fails with the reason (non-zero exit) when it failed or a newer one replaced it.
    #[test]
    fn rollover_outcomes() {
        use crate::functions::rollout_outcome;
        let pending = json!({"deployment_id": "d1", "rollout": {"deployment_id": "d2", "state": "pending", "error": null}});
        assert_eq!(rollout_outcome(&pending, "d2"), None);
        assert_eq!(rollout_outcome(&json!({"deployment_id": "d2", "rollout": null}), "d2"), Some(Ok(())));
        let failed = json!({"deployment_id": "d1", "rollout": {"deployment_id": "d2", "state": "failed",
                                                               "error": "never passed GET /status/500"}});
        let Some(Err(why)) = rollout_outcome(&failed, "d2") else { panic!() };
        assert!(why.contains("d2") && why.contains("never passed GET /status/500") && why.contains("previous deployment stays live"), "{why}");
        assert!(matches!(rollout_outcome(&pending, "d3"), Some(Err(_))), "replaced");
        assert!(matches!(rollout_outcome(&json!({"deployment_id": "d1", "rollout": null}), "d2"), Some(Err(_))));
    }

    #[test]
    fn an_unhealthy_rollover_names_the_new_deployment_and_the_reason() {
        let body = json!({"error": {"code": "conflict", "reason": "unhealthy", "deployment_id": "d2",
                                     "message": "the new container never passed /status/500 in 60 s"}});
        let msg = crate::functions::error_message("POST", "/x/deployments", 409, &body);
        assert!(msg.contains("d2"), "{msg}");
        assert!(msg.contains("never passed /status/500 in 60 s"), "{msg}");
        assert!(msg.contains("previous deployment stays live"), "{msg}");
        // Another 409 reads as a plain error.
        let other = json!({"error": {"code": "conflict", "reason": "not_deployed", "message": "no deployment"}});
        assert_eq!(
            crate::functions::error_message("PATCH", "/x", 409, &other),
            "PATCH /x: 409 no deployment"
        );
        assert_eq!(
            crate::functions::error_message("GET", "/x", 502, &json!("bad gateway")),
            "GET /x: 502 \"bad gateway\""
        );
    }
}
