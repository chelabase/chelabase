//! `chelabase functions schedule …` and `chelabase functions trigger …`: a function's cron
//! schedules (m7) and its table and storage triggers (m13b) on the branch.

use super::Api;
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use reqwest::Method;
use serde_json::{Value, json};

#[derive(Subcommand, Debug)]
pub enum ScheduleCommand {
    /// Run the function on a cron schedule; prints the schedule's id
    Add {
        name: String,
        /// 5 fields (minute granularity), e.g. '*/15 * * * *'
        #[arg(long)]
        cron: String,
        /// IANA time zone the cron is read in (default UTC)
        #[arg(long)]
        timezone: Option<String>,
    },
    /// Remove a schedule
    Rm { name: String, id: String },
    /// The function's schedules
    List { name: String },
}

#[derive(Subcommand, Debug)]
pub enum TriggerCommand {
    /// Call the function on table changes or storage events; prints the trigger's id
    Add(TriggerAdd),
    /// Remove a trigger
    Rm { name: String, id: String },
    /// The function's triggers, with their state and delivery counts
    List { name: String },
}

#[derive(Args, Debug)]
#[command(group = clap::ArgGroup::new("source").required(true).args(["table", "storage"]))]
pub struct TriggerAdd {
    pub name: String,
    /// A table's changes: `schema.table` (`public` when no schema is given)
    #[arg(long, value_name = "SCHEMA.TABLE")]
    pub table: Option<String>,
    /// Storage events of the branch's object storage
    #[arg(long)]
    pub storage: bool,
    /// The branch's Postgres database the table (or the bucket's project) is in
    #[arg(long)]
    pub pg_database: String,
    /// Storage only: one bucket (default every bucket)
    #[arg(long, conflicts_with = "table")]
    pub bucket: Option<String>,
    /// Storage only: keys under this prefix (default every key)
    #[arg(long, conflicts_with = "table")]
    pub prefix: Option<String>,
    /// Comma-separated: insert, update, delete for a table; object.created, object.updated,
    /// object.deleted, object.moved for storage
    #[arg(long, value_delimiter = ',', required = true)]
    pub events: Vec<String>,
}

/// The schedule's create body.
pub fn schedule_body(cron: &str, timezone: Option<&str>) -> Value {
    let mut body = json!({ "cron": cron });
    if let Some(tz) = timezone {
        body["timezone"] = json!(tz);
    }
    body
}

/// The trigger's create body.
pub fn trigger_body(a: &TriggerAdd) -> Value {
    let mut body = json!({ "pg_database": a.pg_database, "events": a.events });
    if let Some(table) = &a.table {
        let (schema, table) = table.split_once('.').unwrap_or(("public", table));
        body["source"] = json!("table");
        body["schema"] = json!(schema);
        body["table"] = json!(table);
    } else {
        body["source"] = json!("storage");
        if let Some(b) = &a.bucket {
            body["bucket"] = json!(b);
        }
        if let Some(p) = &a.prefix {
            body["prefix"] = json!(p);
        }
    }
    body
}

fn s(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// One line per trigger: id, source and target, events, state, enabled, dead, dropped, last
/// delivered, last error.
pub fn trigger_line(t: &Value) -> String {
    let target = if t["source"] == "table" {
        format!("table {}.{}", s(&t["schema"]), s(&t["table"]))
    } else {
        format!("storage {}/{}", s(&t["bucket"]), s(&t["prefix"]))
    };
    let events: Vec<String> = t["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(s)
        .collect();
    format!(
        "{}\t{target}\t{}\t{}\t{}\tdead={}\tdropped={}\tdelivered={}\t{}",
        s(&t["id"]),
        events.join(","),
        s(&t["state"]),
        if t["enabled"] == json!(true) {
            "enabled"
        } else {
            "disabled"
        },
        s(&t["dead_count"]),
        s(&t["dropped_count"]),
        s(&t["last_delivered_at"]),
        t["last_error"].as_str().unwrap_or(""),
    )
}

fn id(v: &Value) -> Result<String> {
    Ok(v["id"].as_str().context("the answer's id")?.to_string())
}

pub async fn schedule(api: &Api, command: ScheduleCommand) -> Result<()> {
    match command {
        ScheduleCommand::Add {
            name,
            cron,
            timezone,
        } => {
            let body = schedule_body(&cron, timezone.as_deref());
            let made = api
                .json(Method::POST, &format!("/{name}/schedules"), body)
                .await?;
            println!("{}", id(&made)?);
        }
        ScheduleCommand::Rm { name, id } => {
            api.send(Method::DELETE, &format!("/{name}/schedules/{id}"), None)
                .await?;
            eprintln!("{name}: schedule {id} removed");
        }
        ScheduleCommand::List { name } => {
            let list = api
                .send(Method::GET, &format!("/{name}/schedules"), None)
                .await?;
            for x in list.as_array().into_iter().flatten() {
                println!(
                    "{}\t{}\t{}\t{}\tnext={}",
                    s(&x["id"]),
                    s(&x["cron"]),
                    s(&x["timezone"]),
                    if x["enabled"] == json!(true) {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    s(&x["next_run_at"]),
                );
            }
        }
    }
    Ok(())
}

pub async fn trigger(api: &Api, command: TriggerCommand) -> Result<()> {
    match command {
        TriggerCommand::Add(a) => {
            let tail = format!("/{}/triggers", a.name);
            let made = api.json(Method::POST, &tail, trigger_body(&a)).await?;
            println!("{}", id(&made)?);
        }
        TriggerCommand::Rm { name, id } => {
            api.send(Method::DELETE, &format!("/{name}/triggers/{id}"), None)
                .await?;
            eprintln!("{name}: trigger {id} removed");
        }
        TriggerCommand::List { name } => {
            let list = api
                .send(Method::GET, &format!("/{name}/triggers"), None)
                .await?;
            for t in list.as_array().into_iter().flatten() {
                println!("{}", trigger_line(t));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::Command;
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("functions").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    fn add(args: &[&str]) -> TriggerAdd {
        match parse(args).unwrap() {
            Command::Trigger {
                command: TriggerCommand::Add(a),
            } => a,
            other => panic!("not trigger add: {other:?}"),
        }
    }

    #[test]
    fn schedule_commands_parse_and_build_their_body() {
        let Command::Schedule {
            command:
                ScheduleCommand::Add {
                    name,
                    cron,
                    timezone,
                },
        } = parse(&[
            "schedule",
            "add",
            "hello",
            "--cron",
            "*/15 * * * *",
            "--timezone",
            "Europe/London",
        ])
        .unwrap()
        else {
            panic!()
        };
        assert_eq!(name, "hello");
        assert_eq!(
            schedule_body(&cron, timezone.as_deref()),
            json!({"cron": "*/15 * * * *", "timezone": "Europe/London"})
        );
        assert_eq!(
            schedule_body("0 * * * *", None),
            json!({"cron": "0 * * * *"})
        );
        assert!(
            parse(&["schedule", "add", "hello"]).is_err(),
            "--cron is required"
        );
        assert!(matches!(
            parse(&["schedule", "rm", "hello", "id1"]).unwrap(),
            Command::Schedule {
                command: ScheduleCommand::Rm { .. }
            }
        ));
        assert!(matches!(
            parse(&["schedule", "list", "hello"]).unwrap(),
            Command::Schedule {
                command: ScheduleCommand::List { .. }
            }
        ));
    }

    #[test]
    fn a_table_trigger_parses_and_builds_its_body() {
        let a = add(&[
            "trigger",
            "add",
            "hello",
            "--table",
            "app.orders",
            "--pg-database",
            "app",
            "--events",
            "insert,update",
        ]);
        assert_eq!(
            trigger_body(&a),
            json!({"source": "table", "pg_database": "app", "schema": "app", "table": "orders",
                   "events": ["insert", "update"]})
        );
        let a = add(&[
            "trigger",
            "add",
            "h",
            "--table",
            "orders",
            "--pg-database",
            "app",
            "--events",
            "delete",
        ]);
        assert_eq!(trigger_body(&a)["schema"], "public");
    }

    #[test]
    fn a_storage_trigger_parses_and_builds_its_body() {
        let a = add(&[
            "trigger",
            "add",
            "hello",
            "--storage",
            "--pg-database",
            "app",
            "--bucket",
            "avatars",
            "--prefix",
            "in/",
            "--events",
            "object.created",
        ]);
        assert_eq!(
            trigger_body(&a),
            json!({"source": "storage", "pg_database": "app", "bucket": "avatars", "prefix": "in/",
                   "events": ["object.created"]})
        );
        let a = add(&[
            "trigger",
            "add",
            "h",
            "--storage",
            "--pg-database",
            "app",
            "--events",
            "object.deleted",
        ]);
        assert_eq!(
            trigger_body(&a),
            json!({"source": "storage", "pg_database": "app", "events": ["object.deleted"]})
        );
    }

    #[test]
    fn a_trigger_needs_exactly_one_source_and_its_own_flags() {
        let base = [
            "trigger",
            "add",
            "h",
            "--pg-database",
            "app",
            "--events",
            "insert",
        ];
        assert!(parse(&base).is_err(), "no source");
        let both = [&base[..], &["--table", "t", "--storage"]].concat();
        assert!(parse(&both).is_err(), "two sources");
        let bucket_on_table = [&base[..], &["--table", "t", "--bucket", "b"]].concat();
        assert!(parse(&bucket_on_table).is_err(), "--bucket is storage only");
        assert!(
            parse(&["trigger", "add", "h", "--table", "t", "--events", "insert"]).is_err(),
            "--pg-database is required"
        );
        assert!(
            parse(&[
                "trigger",
                "add",
                "h",
                "--table",
                "t",
                "--pg-database",
                "app"
            ])
            .is_err(),
            "--events is required"
        );
    }

    #[test]
    fn a_trigger_line_shows_state_counts_and_errors() {
        let t = json!({
            "id": "t1", "source": "table", "schema": "public", "table": "orders",
            "events": ["insert", "update"], "state": "failed", "enabled": true,
            "dead_count": 3, "dropped_count": 7, "last_delivered_at": null,
            "last_error": "relation does not exist",
        });
        assert_eq!(
            trigger_line(&t),
            "t1\ttable public.orders\tinsert,update\tfailed\tenabled\tdead=3\tdropped=7\tdelivered=-\trelation does not exist"
        );
        let t = json!({
            "id": "t2", "source": "storage", "bucket": null, "prefix": "in/", "events": ["object.created"],
            "state": "installed", "enabled": false, "dead_count": 0, "dropped_count": 0,
            "last_delivered_at": "2026-11-08T10:00:00Z", "last_error": null,
        });
        assert_eq!(
            trigger_line(&t),
            "t2\tstorage -/in/\tobject.created\tinstalled\tdisabled\tdead=0\tdropped=0\tdelivered=2026-11-08T10:00:00Z\t"
        );
    }
}
