//! The `chelabase_keys` replication session and its messages (spec §3.1, §3.3; pre-flight §2;
//! R1, R7, W29): what realtime sends on a `replication=database` connection as
//! `chelabase_realtime`, and the parser of the plugin's JSON.
//!
//! - The startup packet carries only `user`, `database`, `replication=database`,
//!   `client_encoding` and `application_name` (the fork refuses anything else, `options`
//!   included); no SQL ever runs on the connection.
//! - `CREATE_REPLICATION_SLOT rt_<random> TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')`
//!   (never `export`: W29), then `START_REPLICATION SLOT rt_<random> LOGICAL <consistent point>
//!   ("max_keys_per_table" '1000', "max_tables" '256', "max_message_bytes" '1048576')`.
//! - A message is `{"v":1,"lsn":"X/Y","xid":<xid8>,"changes":[…]}` or `{…,"all":true}`; `lsn`
//!   is the commit's end LSN, `xid` the full 64-bit id. Unknown fields are ignored; a `v` other
//!   than 1, or a message that can't be read, is [`Body::All`] (everything `changed`).
//!
use serde_json::Value;
use tokio_postgres::config::{ReplicationMode, SslMode};
use tokio_postgres::types::PgLsn;

pub const PLUGIN: &str = "chelabase_keys";
/// The replication login (R1) and the session's `application_name` (never `walproposer`).
pub const ROLE: &str = "chelabase_realtime";
pub const APPLICATION_NAME: &str = "chelabase_realtime";
pub const MAX_KEYS_PER_TABLE: u32 = 1000;
pub const MAX_TABLES: u32 = 256;
pub const MAX_MESSAGE_BYTES: u32 = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Op {
    Insert,
    Update,
    Delete,
    /// `c`: changed without keys (no PK, TRUNCATE, over a limit, …).
    Changed,
}

/// One relation's changes of one op in a transaction (`rel` is the partition's own OID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelChange {
    pub rel: u32,
    pub schema: String,
    pub table: String,
    pub op: Op,
    /// Each key's column texts in PK order (empty for [`Op::Changed`]).
    pub keys: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// `"all": true`, `v != 1` or an unreadable message: every relation of the database changed
    /// (and, W10, counts as a schema change).
    All,
    Changes(Vec<RelChange>),
}

/// One decoded transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Txn {
    /// The commit's end LSN (`None` when the message couldn't be read).
    pub lsn: Option<PgLsn>,
    /// The full transaction id (`xid8`).
    pub xid: Option<u64>,
    pub body: Body,
}

/// Parses one XLogData payload. Anything this build can't read with certainty is
/// [`Body::All`] (fails closed: everything `changed`, never keys).
pub fn parse(data: &[u8]) -> Txn {
    let unreadable = Txn {
        lsn: None,
        xid: None,
        body: Body::All,
    };
    let Ok(Value::Object(msg)) = serde_json::from_slice::<Value>(data) else {
        return unreadable;
    };
    let lsn = match msg.get("lsn") {
        Some(Value::String(text)) => match text.parse::<PgLsn>() {
            Ok(lsn) => Some(lsn),
            Err(_) => return unreadable,
        },
        _ => None,
    };
    let xid = msg.get("xid").and_then(Value::as_u64);
    let all = Txn {
        lsn,
        xid,
        body: Body::All,
    };
    // `v` must be the number 1 (never `"1"`, never 1.0's other spellings).
    if !msg.get("v").is_some_and(|v| v.as_u64() == Some(1)) || lsn.is_none() || xid.is_none() {
        return all;
    }
    if msg.get("all").is_some_and(|a| a == &Value::Bool(true)) {
        return all;
    }
    let Some(Value::Array(changes)) = msg.get("changes") else {
        return all;
    };
    let mut out = Vec::with_capacity(changes.len());
    for change in changes {
        match parse_change(change) {
            Some(c) => out.push(c),
            None => return all,
        }
    }
    Txn {
        lsn,
        xid,
        body: Body::Changes(out),
    }
}

fn parse_change(change: &Value) -> Option<RelChange> {
    let c = change.as_object()?;
    let rel = u32::try_from(c.get("rel")?.as_u64()?).ok()?;
    let schema = c.get("s")?.as_str()?.to_string();
    let table = c.get("t")?.as_str()?.to_string();
    let op = match c.get("op")?.as_str()? {
        "i" => Op::Insert,
        "u" => Op::Update,
        "d" => Op::Delete,
        "c" => Op::Changed,
        _ => return None,
    };
    let keys = if op == Op::Changed {
        Vec::new()
    } else {
        let rows = c.get("k")?.as_array()?;
        let mut keys = Vec::with_capacity(rows.len());
        for row in rows {
            let texts = row
                .as_array()?
                .iter()
                .map(|t| t.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()?;
            if texts.is_empty() {
                return None;
            }
            keys.push(texts);
        }
        keys
    };
    Some(RelChange {
        rel,
        schema,
        table,
        op,
        keys,
    })
}

/// `rt_` + random lowercase letters and digits, at most 63 bytes.
pub fn new_slot_name() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    let tail: String = bytes
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
        .collect();
    format!("rt_{tail}")
}

/// A slot name this module made: `rt_` and lowercase letters and digits only (it's spliced
/// into a replication command, which takes no parameters).
fn checked_slot(slot: &str) -> &str {
    assert!(
        slot.len() <= 63
            && slot.starts_with("rt_")
            && slot[3..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
        "a slot name made by new_slot_name"
    );
    slot
}

/// `CREATE_REPLICATION_SLOT <slot> TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')`.
pub fn create_slot_command(slot: &str) -> String {
    format!(
        "CREATE_REPLICATION_SLOT {} TEMPORARY LOGICAL {PLUGIN} (SNAPSHOT 'nothing')",
        checked_slot(slot)
    )
}

/// `START_REPLICATION SLOT <slot> LOGICAL <from> (<the three options>)`.
pub fn start_replication_command(slot: &str, from: PgLsn) -> String {
    format!(
        "START_REPLICATION SLOT {} LOGICAL {from} (\"max_keys_per_table\" '{MAX_KEYS_PER_TABLE}', \"max_tables\" '{MAX_TABLES}', \"max_message_bytes\" '{MAX_MESSAGE_BYTES}')",
        checked_slot(slot)
    )
}

/// Where the replication session connects (from the branch's connection route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationTarget {
    pub host: String,
    pub port: u16,
    /// The compute certificate's name (TLS verified as it, `host` dialled).
    pub server_name: Option<String>,
    pub database: String,
    /// `chelabase_realtime`'s password (`CHELABASE_REALTIME_PG_PASSWORD_FILE`).
    pub password: String,
}

/// The session's settings: user [`ROLE`], the database, `replication=database`,
/// `application_name` [`APPLICATION_NAME`], no `options`.
///
/// With a `server_name`, `host` (an IP) is dialled and the compute's certificate is verified
/// as the name (the caller's connector), TLS required.
pub fn replication_config(target: &ReplicationTarget) -> tokio_postgres::Config {
    let mut config = tokio_postgres::Config::new();
    config
        .user(ROLE)
        .password(target.password.as_bytes())
        .dbname(&target.database)
        .port(target.port)
        .application_name(APPLICATION_NAME)
        .replication_mode(ReplicationMode::Logical)
        .connect_timeout(CONNECT_TIMEOUT);
    match (&target.server_name, target.host.parse::<std::net::IpAddr>()) {
        (Some(name), Ok(ip)) => {
            config.host(name).hostaddr(ip).ssl_mode(SslMode::Require);
        }
        (Some(name), Err(_)) => {
            config.host(name).ssl_mode(SslMode::Require);
        }
        (None, _) => {
            config.host(&target.host);
        }
    }
    config
}

/// How long connecting (TCP, TLS, authentication) may take.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
