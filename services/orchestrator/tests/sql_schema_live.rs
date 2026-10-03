//! Live check of the pgls introspection against the dev metadata Postgres.

use orchestrator::sql_editor::{MAX_SCHEMA_BYTES, schema};
use tokio_postgres::NoTls;

const BASE: &str = "postgres://cheladb:cheladb@127.0.0.1:5433";

async fn connect(db: &str) -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&format!("{BASE}/{db}"), NoTls)
        .await
        .expect("the metadata Postgres (just deps-up)");
    tokio::spawn(conn);
    client
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn introspection_runs_and_fits() {
    let mut buf = [0u8; 4];
    getrandom::fill(&mut buf).unwrap();
    let database = format!("t5_schema_{:08x}", u32::from_le_bytes(buf));

    let admin = connect("cheladb_metadata").await;
    admin
        .batch_execute(&format!("CREATE DATABASE {database}"))
        .await
        .unwrap();

    let result: Result<usize, String> = async {
        let client = connect(&database).await;
        client
            .batch_execute(
                "CREATE TYPE mood AS ENUM ('sad', 'ok');
                 CREATE TABLE notes (id bigint PRIMARY KEY, body text NOT NULL, m mood);
                 ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
                 CREATE POLICY own ON notes FOR SELECT USING (true);
                 CREATE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
                 CREATE TRIGGER notes_touch BEFORE INSERT OR UPDATE ON notes FOR EACH ROW EXECUTE FUNCTION touch();",
            )
            .await
            .map_err(|e| e.to_string())?;
        let json = schema::introspect(&client).await.map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        for key in [
            "schemas", "tables", "functions", "types", "version", "columns", "policies", "extensions",
            "triggers", "roles", "indexes", "sequences",
        ] {
            if v.get(key).is_none() {
                return Err(format!("missing {key}"));
            }
        }
        let has = |key: &str, name: &str| {
            v[key].as_array().unwrap().iter().any(|r| r["name"] == name)
        };
        if !has("tables", "notes") {
            return Err("missing notes in tables".into());
        }
        if !has("types", "mood") {
            return Err("missing mood in types".into());
        }
        if !has("functions", "touch") {
            return Err("missing touch in functions".into());
        }
        if !has("policies", "own") {
            return Err("missing own in policies".into());
        }
        if !has("triggers", "notes_touch") {
            return Err("missing notes_touch in triggers".into());
        }
        if json.len() >= MAX_SCHEMA_BYTES {
            return Err(format!("too large: {}", json.len()));
        }
        Ok(json.len())
    }
    .await;

    admin
        .batch_execute(&format!("DROP DATABASE {database} WITH (FORCE)"))
        .await
        .unwrap();
    let bytes = result.expect("introspection checks");
    println!("schema JSON bytes: {bytes}");
}
