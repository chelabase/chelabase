//! Join targets and channel identity (spec §2, W24, P11, R4, S17).
//!
//! - A table join is `{"channel":"table","params":{database_id, branch_id, pg_database, schema,
//!   table, role}}`; its channel id is `table:` + the compact JSON array
//!   `[database_id,branch_id,pg_database,schema,table]` (no role: one id per table per socket).
//! - A project join is `{"channel":"project:<uuid>"}`; its id is `project:` + the canonical
//!   (hyphenated lowercase) UUID.
//! - `app:<name>` is RT2's: parsed, so a Studio ticket gets `forbidden` for it (R9).
//! - Object storage m17: a storage join is `{"channel":"storage","params":{database_id,
//!   branch_id, pg_database, bucket_id, role}}`: a watch of `storage.objects` scoped to one
//!   bucket ([`TableChannel::bucket`]), checked and authorized as a table join of that role. Its
//!   channel id is `storage:` + the compact JSON array `[database_id,branch_id,pg_database,
//!   bucket_id]`. A `table` join of the `storage` schema is refused: only a storage channel
//!   carries those keys, and only for its bucket.
//! - R4: `channel_hash = hex(sha256(scope ‖ channel id))`, scope `branch_id ‖ db_oid` for tables
//!   and `project_id` for projects; every presence and fan-out message carries its scope.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Postgres truncates identifiers past 63 bytes (NAMEDATALEN - 1): a longer name would watch a
/// table under one id and check it under another, so it is refused.
pub const MAX_NAME_BYTES: usize = 63;

/// The schema a `table` join may not name (object metadata: storage channels only).
pub const STORAGE_SCHEMA: &str = "storage";
/// The table a storage channel watches, in [`STORAGE_SCHEMA`].
pub const OBJECTS_TABLE: &str = "objects";

/// A table, without the role it is watched as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableChannel {
    pub database_id: Uuid,
    pub branch_id: Uuid,
    pub pg_database: String,
    pub schema: String,
    pub table: String,
    /// Object storage m17: a storage channel's bucket (`storage.objects` only): every check of
    /// the watch is scoped to it, and its deletes are never a bypass group's. `None` for a
    /// table channel.
    pub bucket: Option<Uuid>,
}

impl TableChannel {
    /// `table:` + compact JSON `[database_id,branch_id,pg_database,schema,table]`; a storage
    /// channel's `storage:` + compact JSON `[database_id,branch_id,pg_database,bucket_id]`.
    pub fn id(&self) -> String {
        if let Some(bucket) = self.bucket {
            let parts = [
                self.database_id.hyphenated().to_string(),
                self.branch_id.hyphenated().to_string(),
                self.pg_database.clone(),
                bucket.hyphenated().to_string(),
            ];
            return format!(
                "storage:{}",
                serde_json::to_string(&parts).expect("strings serialize")
            );
        }
        let parts = [
            self.database_id.hyphenated().to_string(),
            self.branch_id.hyphenated().to_string(),
            self.pg_database.clone(),
            self.schema.clone(),
            self.table.clone(),
        ];
        format!(
            "table:{}",
            serde_json::to_string(&parts).expect("strings serialize")
        )
    }
}

/// What a `join` frame asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinTarget {
    /// `role: None` is the owner (console/query's meaning).
    Table {
        channel: TableChannel,
        role: Option<String>,
    },
    Project(Uuid),
    /// `app:<name>` (RT2): the whole channel string.
    App(String),
}

impl JoinTarget {
    /// The channel id later frames use (`joined.channel`).
    pub fn id(&self) -> String {
        match self {
            JoinTarget::Table { channel, .. } => channel.id(),
            JoinTarget::Project(p) => project_channel_id(*p),
            JoinTarget::App(name) => name.clone(),
        }
    }
}

/// `project:` + the canonical UUID.
pub fn project_channel_id(project_id: Uuid) -> String {
    format!("project:{}", project_id.hyphenated())
}

/// A UUID in its hyphenated form only (any case): the other forms `Uuid::parse_str` takes
/// (simple, braced, URN) are refused, so one id has one spelling on the wire.
fn parse_uuid(text: &str, what: &str) -> Result<Uuid, String> {
    if text.len() != 36 {
        return Err(format!("{what} must be a hyphenated UUID"));
    }
    Uuid::parse_str(text).map_err(|_| format!("{what} must be a hyphenated UUID"))
}

/// A Postgres name: non-empty, at most [`MAX_NAME_BYTES`], no NUL.
fn parse_name(value: Option<&Value>, what: &str) -> Result<String, String> {
    let Some(Value::String(s)) = value else {
        return Err(format!("{what} must be a string"));
    };
    if s.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if s.len() > MAX_NAME_BYTES {
        return Err(format!("{what} is longer than {MAX_NAME_BYTES} bytes"));
    }
    if s.contains('\0') {
        return Err(format!("{what} contains a NUL"));
    }
    Ok(s.clone())
}

const TABLE_FIELDS: [&str; 6] = [
    "database_id",
    "branch_id",
    "pg_database",
    "schema",
    "table",
    "role",
];

fn parse_table(params: Option<&Value>) -> Result<JoinTarget, String> {
    let Some(Value::Object(p)) = params else {
        return Err(
            "a table join takes params {database_id, branch_id, pg_database, schema, table, role}"
                .into(),
        );
    };
    if let Some(unknown) = p.keys().find(|k| !TABLE_FIELDS.contains(&k.as_str())) {
        return Err(format!("unknown join param {unknown:?}"));
    }
    let uuid_of = |field: &str| match p.get(field) {
        Some(Value::String(s)) => parse_uuid(s, field),
        _ => Err(format!("{field} must be a hyphenated UUID")),
    };
    let database_id = uuid_of("database_id")?;
    let branch_id = uuid_of("branch_id")?;
    let pg_database = parse_name(p.get("pg_database"), "pg_database")?;
    let schema = parse_name(p.get("schema"), "schema")?;
    if schema == STORAGE_SCHEMA {
        return Err("the storage schema is watched through a storage channel".into());
    }
    let table = parse_name(p.get("table"), "table")?;
    let role = parse_role(p.get("role"))?;
    Ok(JoinTarget::Table {
        channel: TableChannel {
            database_id,
            branch_id,
            pg_database,
            schema,
            table,
            bucket: None,
        },
        role,
    })
}

/// `role` is required: `null` (the owner) must be said, never implied by a missing field.
fn parse_role(value: Option<&Value>) -> Result<Option<String>, String> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(v @ Value::String(_)) => Ok(Some(parse_name(Some(v), "role")?)),
        Some(_) => Err("role must be a string or null".into()),
        None => Err("role is required (null for the owner)".into()),
    }
}

const STORAGE_FIELDS: [&str; 5] = ["database_id", "branch_id", "pg_database", "bucket_id", "role"];

/// A storage join (object storage m17): `storage.objects` of one bucket, as `role`.
fn parse_storage(params: Option<&Value>) -> Result<JoinTarget, String> {
    let Some(Value::Object(p)) = params else {
        return Err(
            "a storage join takes params {database_id, branch_id, pg_database, bucket_id, role}"
                .into(),
        );
    };
    if let Some(unknown) = p.keys().find(|k| !STORAGE_FIELDS.contains(&k.as_str())) {
        return Err(format!("unknown join param {unknown:?}"));
    }
    let uuid_of = |field: &str| match p.get(field) {
        Some(Value::String(s)) => parse_uuid(s, field),
        _ => Err(format!("{field} must be a hyphenated UUID")),
    };
    let database_id = uuid_of("database_id")?;
    let branch_id = uuid_of("branch_id")?;
    let pg_database = parse_name(p.get("pg_database"), "pg_database")?;
    let bucket = uuid_of("bucket_id")?;
    let role = parse_role(p.get("role"))?;
    Ok(JoinTarget::Table {
        channel: TableChannel {
            database_id,
            branch_id,
            pg_database,
            schema: STORAGE_SCHEMA.into(),
            table: OBJECTS_TABLE.into(),
            bucket: Some(bucket),
        },
        role,
    })
}

/// Parses a join's `channel` and `params`. `Err` carries the `error invalid` message: an unknown
/// channel kind, a `table` join without params or with an unknown or missing field, an empty
/// name or role (`role: null` is the owner; `""` is refused), a malformed UUID.
pub fn parse_join(channel: &str, params: Option<&Value>) -> Result<JoinTarget, String> {
    let params = params.filter(|p| !p.is_null());
    if channel == "table" {
        return parse_table(params);
    }
    if channel == "storage" {
        return parse_storage(params);
    }
    if let Some(id) = channel.strip_prefix("project:") {
        if params.is_some() {
            return Err("a project join takes no params".into());
        }
        return parse_uuid(id, "the project id").map(JoinTarget::Project);
    }
    if let Some(name) = channel.strip_prefix("app:") {
        if params.is_some() {
            return Err("an app join takes no params".into());
        }
        if name.is_empty() {
            return Err("an app channel needs a name".into());
        }
        return Ok(JoinTarget::App(channel.to_string()));
    }
    Err("unknown channel: table, storage, project:<uuid> or app:<name>".into())
}

/// A channel's R4 scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Scope {
    Table { branch_id: Uuid, db_oid: u32 },
    Project { project_id: Uuid },
}

/// R4: hex(sha256(scope ‖ channel id)), 64 lowercase hex digits.
pub fn channel_hash(scope: &Scope, channel_id: &str) -> String {
    let scope_text = match scope {
        Scope::Table { branch_id, db_oid } => format!("table:{}:{db_oid}", branch_id.hyphenated()),
        Scope::Project { project_id } => format!("project:{}", project_id.hyphenated()),
    };
    let mut h = Sha256::new();
    h.update(scope_text.as_bytes());
    // The scope text has no newline, so scope and channel can't run into each other.
    h.update(b"\n");
    h.update(channel_id.as_bytes());
    hex::encode(h.finalize())
}
