//! The resync (spec 2026-10-01-login-roles, Part 3; step 6g E32): SQL controls every role.
//! The branch's catalog is the source of truth for LOGIN, owner level, the U2 attributes
//! (INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS) and memberships of every role, made in
//! SQL or through the API: the resync records them into the role's row. Two things stay the
//! control plane's for a role made through the API: its existence (a role the compute lost is
//! made again) and its password (set back to the stored verifier; a password changed in SQL
//! reaches the row through forwarding first). A change the API wrote that the compute never
//! confirmed (`attributes_pending`) is driven onto the compute instead of being recorded over.

use std::collections::BTreeMap;
use std::time::Duration;

use chelabase_metadata::{
    CopyUpsert, DataKeyRecord, KeyRoleFix, ObservedDatabase, ResyncApplied, ResyncWrite,
    RoleAttributes, RoleCopy, RoleLevel, RoleOrigin, SealedChange,
};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::seal_role_password;
use chelabase_types::BranchId;
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::role_name::{
    has_reserved_prefix, is_membership_reserved, is_reserved_for, quote_ident, quote_literal,
};
use chrono::{DateTime, SecondsFormat, Utc};
use chelabase_types::role_password::{PasswordKind, hash_kind};
use tracing::{error, info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{OWNER_ROLE, Worker, credentials};

/// The database [`Worker::resync`] reads the catalog through: the cluster's own `postgres`
/// database, which the owner can't drop (the branch's main database can be, Q1). Roles and
/// `pg_database` are cluster-wide, so any database sees the same catalog.
pub const SYSTEM_DATABASE: &str = "postgres";

/// Every role's OID, hex-encoded name and BYPASSRLS (`t`/`f`, E33), for the secret keys'
/// identities and marks. The comment is the fakes' needle.
pub const KEY_ROLES_SQL: &str = "SET search_path = pg_catalog, pg_temp;\n\
     SELECT /* chelabase: key roles */ oid::text || ' ' || encode(convert_to(rolname, 'UTF8'), 'hex') \
     || CASE WHEN rolbypassrls THEN ' t' ELSE ' f' END FROM pg_roles;";

/// [`KEY_ROLES_SQL`]'s lines: `(oid, name, bypass_rls)`; a line without the flag (an older
/// read) is `false`.
pub fn parse_key_role_flags(out: &str) -> anyhow::Result<Vec<(i64, String, bool)>> {
    out.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.trim().split(' ');
            let oid = fields.next().unwrap_or_default();
            let name = fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("a role line without a name: {line:?}"))?;
            let bypass = match fields.next() {
                None | Some("f") => false,
                Some("t") => true,
                Some(other) => anyhow::bail!("a role line with a bad BYPASSRLS flag {other:?}"),
            };
            if fields.next().is_some() {
                anyhow::bail!("a role line with extra fields");
            }
            let oid = oid
                .parse::<i64>()
                .map_err(|e| anyhow::anyhow!("a role line with a bad OID {oid:?}: {e}"))?;
            let name = unhex(name).ok_or_else(|| anyhow::anyhow!("a role line with a bad name"))?;
            Ok((oid, name, bypass))
        })
        .collect()
}

/// [`KEY_ROLES_SQL`]'s lines: `(oid, name)`.
pub fn parse_key_roles(out: &str) -> anyhow::Result<Vec<(i64, String)>> {
    Ok(parse_key_role_flags(out)?
        .into_iter()
        .map(|(oid, name, _)| (oid, name))
        .collect())
}

/// E33: the BYPASSRLS marks of the live secret keys that stay bound to a role after `fixes`
/// (their own OID, or the one a backfill gives them), from `catalog`'s `(oid, name,
/// bypass_rls)`. Only marks that change; a revoked key gets none.
pub fn plan_key_bypass(
    keys: &[DataKeyRecord],
    catalog: &[(i64, String, bool)],
    fixes: &[KeyRoleFix],
) -> Vec<KeyRoleFix> {
    keys.iter()
        .filter(|k| k.kind == DataKeyKind::Secret && k.revoked_at.is_none())
        .filter_map(|k| {
            let mut oid = k.role_oid;
            for fix in fixes {
                match fix {
                    KeyRoleFix::Revoke { id, .. } if *id == k.id => return None,
                    KeyRoleFix::Backfill { id, oid: o, .. } if *id == k.id => oid = Some(*o),
                    _ => {}
                }
            }
            let oid = oid?;
            let (_, _, bypass) = catalog.iter().find(|(o, _, _)| *o == oid)?;
            (*bypass != k.role_bypass_rls).then_some(KeyRoleFix::Bypass {
                id: k.id,
                oid,
                bypass_rls: *bypass,
            })
        })
        .collect()
}

/// The repairs that make the live secret keys agree with the compute's roles (`catalog`, read
/// on the branch's current `timeline`), by identity: a key is bound to its role's OID *on the
/// timeline the OID was read on* (E26).
///
/// - On that timeline: a key whose OID is gone is revoked (its role was dropped, maybe
///   recreated under the name); a key whose OID now has another name follows it.
/// - On another timeline (a reset, restore or a merge's reset since; or none recorded): the
///   OID counter diverged at the branch point, so the OID alone proves nothing. A key whose
///   OID still carries its name is re-stamped with `timeline` and kept; any other is revoked,
///   never followed.
/// - A key without an OID yet takes its name's current OID on `timeline`, or is revoked when
///   the name is gone.
///
/// Public keys are left alone.
pub fn plan_key_roles(
    keys: &[DataKeyRecord],
    catalog: &[(i64, String)],
    timeline: &str,
) -> Vec<KeyRoleFix> {
    let name_of = |oid: i64| catalog.iter().find(|(o, _)| *o == oid).map(|(_, n)| n);
    let oid_of = |name: &str| catalog.iter().find(|(_, n)| n == name).map(|(o, _)| *o);
    let revoke = |k: &DataKeyRecord| KeyRoleFix::Revoke {
        id: k.id,
        role: k.role.clone(),
        oid: k.role_oid,
    };
    keys.iter()
        .filter(|k| k.kind == DataKeyKind::Secret && k.revoked_at.is_none())
        .filter_map(|k| match k.role_oid {
            Some(oid) if k.role_timeline.as_deref() == Some(timeline) => match name_of(oid) {
                None => Some(revoke(k)),
                Some(name) if *name != k.role => Some(KeyRoleFix::Follow {
                    id: k.id,
                    from: k.role.clone(),
                    to: name.clone(),
                    oid,
                    timeline: timeline.to_string(),
                }),
                Some(_) => None,
            },
            Some(oid) => Some(match name_of(oid) {
                Some(name) if *name == k.role => KeyRoleFix::Restamp {
                    id: k.id,
                    role: k.role.clone(),
                    oid,
                    from: k.role_timeline.clone(),
                    timeline: timeline.to_string(),
                },
                _ => revoke(k),
            }),
            None => Some(match oid_of(&k.role) {
                Some(oid) => KeyRoleFix::Backfill {
                    id: k.id,
                    role: k.role.clone(),
                    oid,
                    timeline: timeline.to_string(),
                },
                None => revoke(k),
            }),
        })
        .collect()
}

/// How long after the first `password_updated` the second one goes out (R22).
pub const REPUBLISH_DELAY: Duration = Duration::from_secs(1);

/// How long a touched-roles message waits before the branch is resynced: the compute's DDL
/// commit has to be visible to the catalog read, and a burst of statements shares one run.
pub const TOUCHED_RESYNC_DELAY: Duration = Duration::from_secs(3);

/// The branches with a touched-roles resync waiting in this worker process (one pending run
/// per branch per process; with several workers each queue message goes to one of them).
#[derive(Default)]
pub struct PendingResyncs {
    /// The waiting branches; the flag is whether a request from the DDL receiver is among the
    /// ones the waiting run covers (it gets the RT3-11 retry).
    waiting: std::sync::Mutex<std::collections::HashMap<BranchId, bool>>,
}

impl PendingResyncs {
    /// True when the caller now owns the branch's pending run; false when one is waiting.
    pub(crate) fn claim(&self, branch: BranchId) -> bool {
        self.claim_marked(branch, false)
    }

    /// [`Self::claim`], noting `from_ddl` on the branch's run whether the caller owns it or
    /// merges into a waiting one.
    pub(crate) fn claim_marked(&self, branch: BranchId, from_ddl: bool) -> bool {
        let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
        match waiting.entry(branch) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                *entry.get_mut() |= from_ddl;
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(from_ddl);
                true
            }
        }
    }

    pub(crate) fn release(&self, branch: BranchId) {
        self.release_marked(branch);
    }

    /// Ends the branch's wait; whether a DDL request was among those the run covers.
    pub(crate) fn release_marked(&self, branch: BranchId) -> bool {
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&branch)
            .unwrap_or(false)
    }
}

/// Where the worker asks for a branch resync on `chelabase.resync.branch` (F2 fix round I-1: a
/// role write whose confirm lost to a concurrent write leaves `pending` for the resync to
/// clear). The process connects it to NATS once; until then (and in tests) requests are only
/// recorded. A failed publish is logged: the periodic resync is the backstop.
#[derive(Default)]
pub struct ResyncRequests {
    client: std::sync::OnceLock<async_nats::Client>,
    sent: std::sync::Mutex<Vec<BranchId>>,
}

impl ResyncRequests {
    /// Publishes later requests with `client`.
    pub fn connect(&self, client: async_nats::Client) {
        let _ = self.client.set(client);
    }

    /// Asks for a resync of `branch`, off the caller's path.
    pub fn request(&self, branch: BranchId) {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(branch);
        let Some(client) = self.client.get().cloned() else {
            return;
        };
        tokio::spawn(async move {
            let body = match serde_json::to_vec(&chelabase_nats::ResyncBranchRequest {
                branch_id: branch,
            }) {
                Ok(body) => body,
                Err(e) => {
                    warn!(branch_id = %branch, "resync request not encoded: {e}");
                    return;
                }
            };
            if let Err(e) = client
                .publish(chelabase_nats::SUBJECT_RESYNC_BRANCH, body.into())
                .await
            {
                warn!(branch_id = %branch, "resync request not published: {e}");
            }
        });
    }

    /// The branches asked for so far.
    pub fn sent(&self) -> Vec<BranchId> {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// How many branches [`Worker::sync_key_marks_at_start`] syncs at once.
pub const KEY_MARK_SYNC_CONCURRENCY: usize = 4;

/// How many times the resync reads and writes before giving up on a moving `roles_version`.
const RESYNC_TRIES: usize = 3;

/// One role of the catalog (owner and system roles excluded).
#[derive(Clone, PartialEq, Eq)]
pub struct ObservedRole {
    pub name: String,
    pub login: bool,
    /// `rolpassword`: a SCRAM verifier, an MD5 hash, or `None`.
    pub password: Option<String>,
    /// Owner-level (E33): superuser, CREATEROLE, CREATEDB, REPLICATION, or a member of
    /// `neon_superuser` at any depth. BYPASSRLS alone isn't ([`Self::bypass_rls`]).
    pub owner_level: bool,
    /// The owner holds ADMIN on the role.
    pub owner_admin: bool,
    /// The role has the owner's attributes (`CREATEROLE CREATEDB BYPASSRLS REPLICATION`) and a
    /// direct `neon_superuser` membership: what an API owner-level role is made with.
    pub owner_attributes: bool,
    pub inherit: bool,
    /// `-1`: no limit.
    pub connection_limit: i32,
    /// `None`: no expiry (or `infinity`).
    pub valid_until: Option<DateTime<Utc>>,
    pub bypass_rls: bool,
    /// The roles it's a direct member of (`neon_superuser` excluded), sorted, once each.
    pub member_of: Vec<String>,
    /// Those of [`Self::member_of`] the owner granted: the ones an owner's `REVOKE` removes.
    pub owner_granted: Vec<String>,
    /// Those of [`Self::member_of`] no grantor gave the SET option (I2), sorted.
    pub member_no_set: Vec<String>,
    /// Those of [`Self::member_of`] some grantor gave the ADMIN option (I2), sorted.
    pub member_admin: Vec<String>,
}

impl ObservedRole {
    /// A role with Postgres' defaults: LOGIN, no password, INHERIT, no limit, no expiry, no
    /// memberships, the owner holding ADMIN on it.
    pub fn plain(name: &str) -> Self {
        Self {
            name: name.to_string(),
            login: true,
            password: None,
            owner_level: false,
            owner_admin: true,
            owner_attributes: false,
            inherit: true,
            connection_limit: -1,
            valid_until: None,
            bypass_rls: false,
            member_of: Vec::new(),
            owner_granted: Vec::new(),
            member_no_set: Vec::new(),
            member_admin: Vec::new(),
        }
    }

    /// Its attributes and memberships as the metadata records them.
    pub fn attributes(&self) -> RoleAttributes {
        RoleAttributes {
            inherit: self.inherit,
            connection_limit: self.connection_limit,
            valid_until: self.valid_until,
            bypass_rls: self.bypass_rls,
            member_of: self.member_of.clone(),
            member_no_set: self.member_no_set.clone(),
            member_admin: self.member_admin.clone(),
        }
    }
}

impl std::fmt::Debug for ObservedRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservedRole")
            .field("name", &self.name)
            .field("login", &self.login)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("owner_level", &self.owner_level)
            .field("owner_admin", &self.owner_admin)
            .field("owner_attributes", &self.owner_attributes)
            .field("inherit", &self.inherit)
            .field("connection_limit", &self.connection_limit)
            .field("valid_until", &self.valid_until)
            .field("bypass_rls", &self.bypass_rls)
            .field("member_of", &self.member_of)
            .field("owner_granted", &self.owner_granted)
            .field("member_no_set", &self.member_no_set)
            .field("member_admin", &self.member_admin)
            .finish()
    }
}

/// What the catalog read found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observed {
    pub roles: Vec<ObservedRole>,
    pub databases: Vec<ObservedDatabase>,
}

/// One `r` line per role of `pg_authid a` (the caller adds the `WHERE`); `$OWNER` is the
/// owner's name. Past the owner-attributes column: INHERIT, the connection limit, VALID UNTIL
/// in microseconds since the epoch (empty when there's none or it's infinite), BYPASSRLS, and
/// the direct memberships (`neon_superuser` left out) as `hex(name):o` (granted by the owner)
/// or `hex(name):x`, then `n` when the grant lacks the SET option and `a` when it has the ADMIN
/// option (I2), comma-separated.
const ROLE_SELECT: &str = "SELECT 'r',
       encode(convert_to(a.rolname, 'UTF8'), 'hex'),
       a.rolcanlogin,
       coalesce(encode(convert_to(a.rolpassword, 'UTF8'), 'hex'), ''),
       (a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication
        OR pg_has_role(a.oid, 'neon_superuser', 'MEMBER')),
       EXISTS (SELECT 1 FROM pg_auth_members m
               WHERE m.roleid = a.oid AND m.admin_option
                 AND m.member = (SELECT oid FROM pg_roles WHERE rolname = $OWNER)),
       (a.rolcreaterole AND a.rolcreatedb AND a.rolbypassrls AND a.rolreplication
        AND EXISTS (SELECT 1 FROM pg_auth_members s
                    WHERE s.member = a.oid AND s.roleid = 'neon_superuser'::regrole)),
       a.rolinherit,
       a.rolconnlimit,
       coalesce(CASE WHEN isfinite(a.rolvaliduntil)
                     THEN (extract(epoch FROM a.rolvaliduntil) * 1000000)::bigint::text END, ''),
       a.rolbypassrls,
       coalesce((SELECT string_agg(encode(convert_to(g.rolname, 'UTF8'), 'hex')
                                   || CASE WHEN m.grantor = (SELECT oid FROM pg_roles
                                                             WHERE rolname = $OWNER)
                                           THEN ':o' ELSE ':x' END
                                   || CASE WHEN m.set_option THEN '' ELSE 'n' END
                                   || CASE WHEN m.admin_option THEN 'a' ELSE '' END,
                                   ',' ORDER BY g.rolname)
                 FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid
                 WHERE m.member = a.oid AND g.rolname <> 'neon_superuser'), '')
FROM pg_authid a";

/// The catalog read, as planned (plan Task 1); `$OWNER` is the owner's name.
const RESYNC_SQL: &str = "SET search_path = pg_catalog, pg_temp;
$ROLE_SELECT
WHERE a.oid >= 16384
  AND a.rolname <> $OWNER
  AND a.rolname NOT IN ('cloud_admin', 'neon_superuser', 'anon', 'authenticated',
                        'authenticator', 'chelabase_admin', 'chelabase_auth_sync')
  AND a.rolname !~ '^pg_'
  AND a.rolname !~* '^(cb_m_|chelabase_)'
UNION ALL
SELECT 'd',
       encode(convert_to(d.datname, 'UTF8'), 'hex'),
       NULL,
       encode(convert_to(pg_get_userbyid(d.datdba), 'UTF8'), 'hex'),
       d.datdba < 16384,
       NULL, NULL, NULL, NULL, NULL, NULL, NULL
FROM pg_database d
WHERE NOT d.datistemplate AND d.datconnlimit <> -2
ORDER BY 1, 2;";

/// The catalog read (plan Task 1, plus Task 12's owner-attributes column and step 6g's
/// attributes and memberships), with `$OWNER` as the owner's quoted literal.
pub fn resync_sql(owner: &str) -> String {
    RESYNC_SQL
        .replace("$ROLE_SELECT", ROLE_SELECT)
        .replace("$OWNER", &quote_literal(owner))
}

/// The catalog read of one role (its `r` line, or nothing when it doesn't exist).
pub fn role_read_sql(owner: &str, name: &str) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp;\n{}\nWHERE a.rolname = {};",
        ROLE_SELECT.replace("$OWNER", &quote_literal(owner)),
        quote_literal(name)
    )
}

/// What [`parse_memberships`] read.
struct Memberships {
    member_of: Vec<String>,
    owner_granted: Vec<String>,
    no_set: Vec<String>,
    admin: Vec<String>,
}

/// [`ROLE_SELECT`]'s memberships column, sorted, once each. A role granted by several grantors
/// has the SET (or ADMIN) option when any of them gave it, as Postgres decides.
fn parse_memberships(field: &str) -> Option<Memberships> {
    let mut member_of = std::collections::BTreeSet::new();
    let mut owner_granted = std::collections::BTreeSet::new();
    let mut set = std::collections::BTreeSet::new();
    let mut admin = std::collections::BTreeSet::new();
    for entry in field.split(',').filter(|e| !e.is_empty()) {
        let (name, by) = entry.split_once(':')?;
        let name = unhex(name).filter(|n| !n.is_empty())?;
        let mut flags = by.chars();
        match flags.next()? {
            'o' => {
                owner_granted.insert(name.clone());
            }
            'x' => {}
            _ => return None,
        }
        let rest: String = flags.collect();
        let (no_set, with_admin) = match rest.as_str() {
            "" => (false, false),
            "n" => (true, false),
            "a" => (false, true),
            "na" => (true, true),
            _ => return None,
        };
        if !no_set {
            set.insert(name.clone());
        }
        if with_admin {
            admin.insert(name.clone());
        }
        member_of.insert(name);
    }
    let no_set = member_of.difference(&set).cloned().collect();
    Some(Memberships {
        member_of: member_of.iter().cloned().collect(),
        owner_granted: owner_granted.into_iter().collect(),
        no_set,
        admin: admin.into_iter().collect(),
    })
}

/// A VALID UNTIL in microseconds since the epoch; empty is none.
fn parse_valid_until(field: &str) -> Option<Option<DateTime<Utc>>> {
    if field.is_empty() {
        return Some(None);
    }
    DateTime::from_timestamp_micros(field.parse().ok()?).map(Some)
}

/// `o` as [`ROLE_SELECT`] prints it (tests feed it to the fake backend).
#[cfg(test)]
pub(crate) fn role_line(o: &ObservedRole) -> String {
    let tf = |b: bool| if b { "t" } else { "f" };
    let members: Vec<String> = o
        .member_of
        .iter()
        .map(|g| {
            let by = if o.owner_granted.contains(g) { "o" } else { "x" };
            let no_set = if o.member_no_set.contains(g) { "n" } else { "" };
            let admin = if o.member_admin.contains(g) { "a" } else { "" };
            format!("{}:{by}{no_set}{admin}", hex::encode(g))
        })
        .collect();
    format!(
        "r|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
        hex::encode(&o.name),
        tf(o.login),
        o.password.as_deref().map(hex::encode).unwrap_or_default(),
        tf(o.owner_level),
        tf(o.owner_admin),
        tf(o.owner_attributes),
        tf(o.inherit),
        o.connection_limit,
        o.valid_until
            .map(|t| t.timestamp_micros().to_string())
            .unwrap_or_default(),
        tf(o.bypass_rls),
        members.join(",")
    )
}

/// A hex-encoded UTF-8 string.
fn unhex(field: &str) -> Option<String> {
    String::from_utf8(hex::decode(field).ok()?).ok()
}

fn flag(field: &str) -> Option<bool> {
    match field {
        "t" => Some(true),
        "f" => Some(false),
        _ => None,
    }
}

/// [`resync_sql`]'s output: `r|…` and `d|…` lines, names hex-encoded. Any malformed line is an
/// error (the message never echoes the line: it may hold a password hash).
pub fn parse_observed(out: &str) -> anyhow::Result<Observed> {
    let mut observed = Observed::default();
    for (i, line) in out.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let bad = |why: &str| anyhow::anyhow!("line {} of the catalog read {why}", i + 1);
        let fields: Vec<&str> = line.split('|').collect();
        let [kind, name, a, b, c, d, e, f, g, h, i, j] = fields[..] else {
            return Err(bad("doesn't have twelve fields"));
        };
        let name = unhex(name)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| bad("has a malformed name"))?;
        match kind {
            "r" => {
                let password = if b.is_empty() {
                    None
                } else {
                    Some(unhex(b).ok_or_else(|| bad("has a malformed password"))?)
                };
                let (Some(login), Some(owner_level), Some(owner_admin), Some(owner_attributes)) =
                    (flag(a), flag(c), flag(d), flag(e))
                else {
                    return Err(bad("has a malformed flag"));
                };
                let (Some(inherit), Some(bypass_rls)) = (flag(f), flag(i)) else {
                    return Err(bad("has a malformed flag"));
                };
                let connection_limit = g
                    .parse::<i32>()
                    .ok()
                    .filter(|n| *n >= -1)
                    .ok_or_else(|| bad("has a malformed connection limit"))?;
                let valid_until =
                    parse_valid_until(h).ok_or_else(|| bad("has a malformed expiry"))?;
                let Memberships { member_of, owner_granted, no_set, admin } =
                    parse_memberships(j).ok_or_else(|| bad("has malformed memberships"))?;
                observed.roles.push(ObservedRole {
                    name,
                    login,
                    password,
                    owner_level,
                    owner_admin,
                    owner_attributes,
                    inherit,
                    connection_limit,
                    valid_until,
                    bypass_rls,
                    member_of,
                    owner_granted,
                    member_no_set: no_set,
                    member_admin: admin,
                });
            }
            "d" => {
                if !a.is_empty() || !d.is_empty() || !e.is_empty() || [f, g, h, i, j].iter().any(|x| !x.is_empty()) {
                    return Err(bad("has unexpected fields for a database"));
                }
                let owner = unhex(b)
                    .filter(|o| !o.is_empty())
                    .ok_or_else(|| bad("has a malformed owner"))?;
                let system = flag(c).ok_or_else(|| bad("has a malformed flag"))?;
                observed.databases.push(ObservedDatabase {
                    name,
                    owner,
                    system,
                });
            }
            _ => return Err(bad("has an unknown kind")),
        }
    }
    Ok(observed)
}

/// An `sql` row an API reset gave a new password that wasn't applied yet (T4): still pending,
/// with a verifier and its sealed password. The reset's reply promised a retry, so the resync
/// sets that password back for as long as the role exists.
fn retried_reset(row: &RoleCopy) -> bool {
    row.origin == RoleOrigin::Sql
        && row.pending
        && row.scram_verifier.is_some()
        && row.password_enc.is_some()
}

/// One role's SQL statements, run as one script of their own (C1): a failure stops only
/// that role's group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairGroup {
    pub role: String,
    pub statements: Vec<String>,
}

/// Appends `statement` to `role`'s group (the last one when it's `role`'s, else a new one).
fn push_repair(groups: &mut Vec<RepairGroup>, role: &str, statement: String) {
    match groups.last_mut() {
        Some(g) if g.role == role => g.statements.push(statement),
        _ => groups.push(RepairGroup {
            role: role.to_string(),
            statements: vec![statement],
        }),
    }
}

/// The SQL repairs for `api` rows and for [`retried_reset`] rows (run as `cloud_admin`), one
/// [`RepairGroup`] per role, and the metadata write. The owner's row is never touched.
pub fn plan_resync(
    owner: &str,
    copies: &[RoleCopy],
    observed: &Observed,
) -> (Vec<RepairGroup>, ResyncWrite) {
    let level = |owner_level: bool| {
        if owner_level {
            RoleLevel::OwnerLevel
        } else {
            RoleLevel::Plain
        }
    };
    let is_copy = |c: &&RoleCopy| c.origin != RoleOrigin::Owner && c.name != owner;
    let row_of = |name: &str| copies.iter().filter(is_copy).find(|c| c.name == name);
    let seen = |name: &str| observed.roles.iter().any(|o| o.name == name);
    let mut repairs = Vec::new();
    let mut write = ResyncWrite {
        upserts: Vec::new(),
        removals: Vec::new(),
        databases: observed.databases.clone(),
    };

    // API rows (E32): SQL controls LOGIN, owner level, attributes and memberships, so the
    // catalog's are recorded; the control plane keeps the role's existence and password. A
    // change the API wrote and the compute never confirmed is driven instead.
    for row in copies
        .iter()
        .filter(is_copy)
        .filter(|c| c.origin == RoleOrigin::Api)
        .filter(|c| !has_reserved_prefix(&c.name))
    {
        let stored = row.scram_verifier.as_deref();
        let o = observed.roles.iter().find(|o| o.name == row.name);
        let driven = row.attributes_pending || o.is_none();
        // A LOGIN row without a verifier can't be made again or have its password set back.
        // (LOGIN turned on in SQL without a password is recorded like any SQL change.)
        let password_repair = o.is_some_and(|o| o.password.is_some());
        if row.login && stored.is_none() && (driven || password_repair) {
            error!(role = %row.name, "an API-made LOGIN role has no stored verifier; the resync skips it");
            continue;
        }
        // A wanted group dropped in SQL is gone: the database wins for its existence.
        let kept: Vec<String> = row
            .attributes
            .member_of
            .iter()
            .filter(|g| seen(g.as_str()))
            .cloned()
            .collect();
        // I2: the driven change grants plainly (SET, no ADMIN); a membership the catalog
        // already has keeps the options it has.
        let options = |observed: Option<&Vec<String>>| -> Vec<String> {
            kept.iter()
                .filter(|g| observed.is_some_and(|set| set.contains(g)))
                .cloned()
                .collect()
        };
        let wanted_attributes = RoleAttributes {
            member_no_set: options(o.map(|o| &o.member_no_set)),
            member_admin: options(o.map(|o| &o.member_admin)),
            member_of: kept.clone(),
            ..row.attributes.clone()
        };
        let wanted = WantedRole {
            name: &row.name,
            owner_level: row.api_owner_level,
            login: row.login,
            attributes: &wanted_attributes,
        };
        let upsert = match o {
            None => {
                push_repair(
                    &mut repairs,
                    &row.name,
                    create_role_sql(owner, &wanted, stored),
                );
                (row.login, level(row.api_owner_level), None, wanted_attributes.clone())
            }
            Some(o) => {
                if o.password.as_deref() != stored {
                    push_repair(&mut repairs, &row.name, password_sql(&row.name, stored));
                }
                let recorded = if row.attributes_pending {
                    for sql in wanted_statements(owner, &wanted, o) {
                        push_repair(&mut repairs, &row.name, sql);
                    }
                    (row.login, level(row.api_owner_level), None, wanted_attributes.clone())
                } else {
                    (
                        o.login,
                        level(o.owner_level),
                        Some(o.owner_attributes),
                        o.attributes(),
                    )
                };
                // The owner administers every role made through the API: Chelabase's own
                // invariant, not one of the role's attributes.
                if !o.owner_admin {
                    let grant = format!(
                        "GRANT {} TO {} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE;",
                        quote_ident(&row.name),
                        quote_ident(owner)
                    );
                    push_repair(&mut repairs, &row.name, grant);
                }
                recorded
            }
        };
        let (login, row_level, api_owner_level, attributes) = upsert;
        write.upserts.push(CopyUpsert {
            name: row.name.clone(),
            origin: RoleOrigin::Api,
            verifier: stored.map(str::to_string),
            password_enc: SealedChange::Keep,
            login,
            level: row_level,
            pending: false,
            attributes,
            api_owner_level,
            no_password: false,
        });
    }

    // Everything else: the database wins.
    for o in observed.roles.iter().filter(|o| o.name != owner) {
        let row = row_of(&o.name);
        if row.is_some_and(|r| r.origin == RoleOrigin::Api) {
            continue;
        }
        // An API reset not applied yet: its password is set back, like an `api` row's.
        if let Some(r) = row.filter(|r| retried_reset(r)) {
            let stored = r.scram_verifier.as_deref().unwrap_or_default();
            if o.password.as_deref() != Some(stored) {
                push_repair(&mut repairs, &o.name, set_password_sql(&o.name, stored));
            }
            write.upserts.push(CopyUpsert {
                name: o.name.clone(),
                origin: RoleOrigin::Sql,
                verifier: Some(stored.to_string()),
                password_enc: SealedChange::Keep,
                login: o.login,
                level: level(o.owner_level),
                pending: false,
                attributes: o.attributes(),
                api_owner_level: None,
                no_password: false,
            });
            continue;
        }
        let (verifier, no_password) = match o.password.as_deref().map(|p| (hash_kind(p), p)) {
            Some((PasswordKind::Scram, p)) => (Some(p.to_string()), false),
            // R9: kept, so Studio can flag it; the proxy treats a NULL verifier as unknown.
            Some((PasswordKind::Md5, _)) => (None, false),
            // No password (or none the proxy could ever use): a group role, or one whose
            // password was removed. It exists, so it's kept (members can be granted it), with no
            // verifier; Chelabase's reserved names (the data API's roles) are not listed.
            Some((PasswordKind::Other, _)) | None => {
                if is_reserved_for(&o.name, owner) {
                    if row.is_some() {
                        write.removals.push(o.name.clone());
                    }
                    continue;
                }
                (None, true)
            }
        };
        let password_enc = match row {
            Some(r) if r.scram_verifier == verifier => SealedChange::Keep,
            _ => SealedChange::Clear,
        };
        write.upserts.push(CopyUpsert {
            name: o.name.clone(),
            origin: RoleOrigin::Sql,
            verifier,
            password_enc,
            login: o.login,
            level: level(o.owner_level),
            pending: false,
            attributes: o.attributes(),
            api_owner_level: None,
            no_password,
        });
    }
    for row in copies.iter().filter(is_copy) {
        if row.origin == RoleOrigin::Sql && !seen(&row.name) {
            write.removals.push(row.name.clone());
        }
    }
    (repairs, write)
}

/// A protected parent's child (I3): every role of the child's catalog with a password (SCRAM
/// or MD5, LOGIN or not) gets a fresh one, whatever the parent's copies held when they were
/// read. Kept across the resync's tries, so a retry neither sets a second password nor
/// loses the sealed one.
pub(crate) struct FreshPasswords {
    /// Name → (verifier, the sealed password to store; `None` when the row already holds it).
    assigned: BTreeMap<String, (String, Option<String>)>,
}

impl FreshPasswords {
    /// `recorded`: `(name, verifier)` of the copies already recorded with a fresh password
    /// and its sealed form.
    pub(crate) fn new(recorded: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            assigned: recorded
                .into_iter()
                .map(|(name, verifier)| (name, (verifier, None)))
                .collect(),
        }
    }

    /// For each role of `observed` (never `owner`) with a password that isn't its fresh one:
    /// its recorded fresh verifier, or a new one from `generate` (`(verifier, sealed)`). The
    /// role's observed password becomes that verifier (the caller sets it before planning),
    /// and the statements that set them are returned, one group per role.
    pub(crate) fn assign(
        &mut self,
        owner: &str,
        observed: &mut Observed,
        mut generate: impl FnMut(&str) -> (String, String),
    ) -> Vec<RepairGroup> {
        let mut script = Vec::new();
        for o in observed.roles.iter_mut().filter(|o| o.name != owner) {
            let Some(current) = o.password.as_deref() else {
                continue;
            };
            let (verifier, _) = self.assigned.entry(o.name.clone()).or_insert_with(|| {
                let (verifier, sealed) = generate(&o.name);
                (verifier, Some(sealed))
            });
            if current != verifier.as_str() {
                push_repair(&mut script, &o.name, set_password_sql(&o.name, verifier));
                o.password = Some(verifier.clone());
            }
        }
        script
    }

    /// An upsert carrying a role's new fresh verifier stores its sealed password.
    pub(crate) fn seal(&self, write: &mut ResyncWrite) {
        for up in &mut write.upserts {
            if let Some((verifier, Some(sealed))) = self.assigned.get(&up.name)
                && up.verifier.as_deref() == Some(verifier.as_str())
            {
                up.password_enc = SealedChange::Set(sealed.clone());
            }
        }
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The resync, for a caller holding the endpoint's lock: reads the copies and their
    /// version, the catalog, runs the repairs and writes; a moving version starts it over (at
    /// most [`RESYNC_TRIES`] times). Every changed or repaired name gets `password_updated`
    /// twice (R22). Errors start with `syncing login roles: `.
    pub(crate) async fn resync_locked(
        &self,
        branch: BranchId,
        ep: &str,
        database: &str,
    ) -> Result<(), String> {
        self.resync_locked_with(branch, ep, database, None).await
    }

    /// [`Self::resync_locked`]; with `fresh` (a protected parent's child), the catalog's
    /// roles get their fresh passwords first, right after the read, and the write stores them
    /// sealed for `branch`.
    pub(crate) async fn resync_locked_with(
        &self,
        branch: BranchId,
        ep: &str,
        database: &str,
        mut fresh: Option<&mut FreshPasswords>,
    ) -> Result<(), String> {
        let fail =
            |what: &str, e: &dyn std::fmt::Display| format!("syncing login roles: {what}: {e}");
        for attempt in 1..=RESYNC_TRIES {
            let (copies, version) = self
                .metadata
                .role_copies(ep)
                .await
                .map_err(|e| fail("reading the copies", &e))?;
            let out = self
                .computes
                .admin_sql(ep, database, &resync_sql(OWNER_ROLE))
                .await
                .map_err(|e| fail("reading the catalog", &format_args!("{e:#}")))?;
            let mut observed = parse_observed(&out).map_err(|e| fail("reading the catalog", &e))?;
            // `(role, what failed)` for each group that failed this try.
            let mut failed: Vec<(String, String)> = Vec::new();
            if let Some(fresh) = fresh.as_deref_mut() {
                let aad = branch.to_string();
                let script = fresh.assign(OWNER_ROLE, &mut observed, |name| {
                    let password = credentials::generate_password();
                    (
                        chelabase_neon::scram::verifier(&password),
                        seal_role_password(&self.key, &aad, name, &password),
                    )
                });
                self.run_groups(
                    ep,
                    database,
                    &script,
                    "setting a fresh password",
                    &mut failed,
                )
                .await;
            }
            let (repairs, mut write) = plan_resync(OWNER_ROLE, &copies, &observed);
            if let Some(fresh) = fresh.as_deref() {
                fresh.seal(&mut write);
            }
            self.run_groups(ep, database, &repairs, "repairing", &mut failed)
                .await;
            // A failed role's row is left as it was: its catalog state isn't what the write
            // would record.
            write
                .upserts
                .retain(|up| !failed.iter().any(|(role, _)| *role == up.name));
            let changed = match self
                .metadata
                .apply_resync(ep, branch, version, &write)
                .await
                .map_err(|e| fail("writing the copies", &e))?
            {
                ResyncApplied::Applied { changed } => changed,
                ResyncApplied::VersionMoved => {
                    info!(branch_id = %branch, endpoint_id = %ep, attempt, "roles changed during the resync; reading again");
                    continue;
                }
            };
            // T8 I3: roles made, dropped or changed in SQL (attributes, memberships) can change
            // what member roles may hold (R16) or which names grants find.
            if !changed.is_empty() {
                self.member_role_requests.request(branch);
            }
            // A role re-created or set back in SQL may be cached with its old password too.
            let mut names = changed;
            for row in copies
                .iter()
                .filter(|c| c.origin == RoleOrigin::Api || retried_reset(c))
            {
                let set_back = row.scram_verifier.is_some()
                    && !observed
                        .roles
                        .iter()
                        .any(|o| o.name == row.name && o.password == row.scram_verifier);
                let failed_role = failed.iter().any(|(role, _)| *role == row.name);
                if set_back && !failed_role && !names.contains(&row.name) {
                    names.push(row.name.clone());
                }
            }
            self.publish_twice(ep, &names).await;
            if !names.is_empty() {
                info!(branch_id = %branch, endpoint_id = %ep, roles = %names.join(","), "login roles resynced");
            }
            self.sync_key_roles(branch, ep, database).await;
            if !failed.is_empty() {
                let failures: Vec<String> = failed.into_iter().map(|(_, what)| what).collect();
                return Err(format!("syncing login roles: {}", failures.join("; ")));
            }
            return Ok(());
        }
        Err(format!(
            "syncing login roles: roles kept changing; gave up after {RESYNC_TRIES} tries"
        ))
    }

    /// Makes the branch's live secret keys agree with the compute's roles by OID and timeline
    /// ([`plan_key_roles`]): revokes dead keys, follows renames the DDL receiver didn't see,
    /// re-stamps or revokes keys from before a timeline switch and backfills keys made before
    /// OIDs were stored. Only when the branch has live secret keys. A failure is logged: keys
    /// are still held to their OID by rest on every request, so this is tidying, and the next
    /// resync tries again. An empty catalog (never real: `pg_roles` always has the bootstrap
    /// roles) is taken as unreadable, never as "every role is gone".
    ///
    /// The branch's timeline is read before and after the catalog: a switch landing between
    /// them (the catalog may be either timeline's) skips the repairs for this run. A switch
    /// stops the compute before it moves the timeline and starts the next one after, so a
    /// catalog read between two equal readings is that timeline's.
    async fn sync_key_roles(&self, branch: BranchId, ep: &str, database: &str) {
        let keys = match self.metadata.list_data_keys(branch).await {
            Ok(keys) => keys,
            Err(e) => {
                warn!(branch_id = %branch, "secret keys versus roles: listing the keys: {e}");
                return;
            }
        };
        if !keys.iter().any(|k| k.kind == DataKeyKind::Secret) {
            return;
        }
        let Some(timeline) = self.current_timeline(branch).await else {
            return;
        };
        let flagged = match self.computes.admin_sql(ep, database, KEY_ROLES_SQL).await {
            Ok(out) => match parse_key_role_flags(&out) {
                Ok(catalog) if !catalog.is_empty() => catalog,
                Ok(_) => {
                    warn!(branch_id = %branch, "secret keys versus roles: the role list came back empty");
                    return;
                }
                Err(e) => {
                    warn!(branch_id = %branch, "secret keys versus roles: {e:#}");
                    return;
                }
            },
            Err(e) => {
                warn!(branch_id = %branch, "secret keys versus roles: reading the roles: {e:#}");
                return;
            }
        };
        if self.current_timeline(branch).await.as_deref() != Some(timeline.as_str()) {
            warn!(branch_id = %branch, "secret keys versus roles: the branch's timeline moved during the read; the next resync repairs them");
            return;
        }
        let catalog: Vec<(i64, String)> = flagged
            .iter()
            .map(|(oid, name, _)| (*oid, name.clone()))
            .collect();
        let mut fixes = plan_key_roles(&keys, &catalog, &timeline);
        let marks = plan_key_bypass(&keys, &flagged, &fixes);
        fixes.extend(marks);
        if fixes.is_empty() {
            return;
        }
        match self.metadata.fix_key_roles(branch, &fixes).await {
            Ok(changed) => {
                info!(branch_id = %branch, changed, ?fixes, "secret keys made to agree with the roles");
            }
            Err(e) => warn!(branch_id = %branch, "secret keys versus roles: writing: {e}"),
        }
    }

    /// E33a, at worker start: [`Self::sync_key_roles`] once for every ready `normal` branch
    /// with a compute, [`KEY_MARK_SYNC_CONCURRENCY`] at a time, so the keys of roles that gained
    /// BYPASSRLS in SQL before this release are marked at once (branches without live secret
    /// keys read nothing). Failures are logged and left to the normal resync.
    pub async fn sync_key_marks_at_start(&self) {
        use futures_util::StreamExt;
        let branches = match self.metadata.list_live_branches_for_reconcile().await {
            Ok(branches) => branches,
            Err(e) => {
                warn!("syncing the secret keys' marks at start: listing the branches: {e}");
                return;
            }
        };
        let targets: Vec<(BranchId, String)> = branches
            .into_iter()
            .filter(|b| {
                b.status == chelabase_metadata::BranchStatus::Ready
                    && b.kind == chelabase_metadata::BranchKind::Normal
            })
            .filter_map(|b| Some((b.id, b.endpoint_id?)))
            .collect();
        futures_util::stream::iter(targets)
            .for_each_concurrent(KEY_MARK_SYNC_CONCURRENCY, |(branch, ep)| async move {
                self.sync_key_roles(branch, &ep, SYSTEM_DATABASE).await;
            })
            .await;
    }

    /// The branch's timeline, for [`Self::sync_key_roles`]; `None` (logged) when it can't be
    /// read or the branch has none.
    async fn current_timeline(&self, branch: BranchId) -> Option<String> {
        match self.metadata.get_branch(branch).await {
            Ok(Some(b)) => {
                if b.timeline_id.is_none() {
                    warn!(branch_id = %branch, "secret keys versus roles: the branch has no timeline");
                }
                b.timeline_id
            }
            Ok(None) => None,
            Err(e) => {
                warn!(branch_id = %branch, "secret keys versus roles: reading the branch: {e}");
                None
            }
        }
    }

    /// Runs each group as its own script (C1: psql stops at a script's first error, so one
    /// role's failure would stop the others'). A failure is logged and recorded in `failed`
    /// as `(role, "<what> <role>: <error>")`.
    async fn run_groups(
        &self,
        ep: &str,
        database: &str,
        groups: &[RepairGroup],
        what: &str,
        failed: &mut Vec<(String, String)>,
    ) {
        for group in groups {
            let script = format!(
                "SET search_path = pg_catalog, pg_temp;\n{}",
                group.statements.join("\n")
            );
            if let Err(e) = self.computes.admin_sql(ep, database, &script).await {
                error!(endpoint_id = %ep, role = %group.role, "{what} failed: {e:#}");
                failed.push((
                    group.role.clone(),
                    format!("{what} {}: {e:#}", quote_ident(&group.role)),
                ));
            }
        }
    }

    /// [`Self::resync_locked`] under the endpoint's lock, through [`SYSTEM_DATABASE`].
    pub async fn resync(&self, branch: BranchId) -> Result<(), String> {
        let ep = self
            .metadata
            .get_branch(branch)
            .await
            .map_err(|e| format!("syncing login roles: reading the branch: {e}"))?
            .and_then(|b| b.endpoint_id)
            .ok_or_else(|| "syncing login roles: the branch has no compute".to_string())?;
        let _lock = self
            .metadata
            .lock_endpoint(&ep)
            .await
            .map_err(|e| format!("syncing login roles: locking the endpoint: {e}"))?;
        self.resync_locked(branch, &ep, SYSTEM_DATABASE).await
    }

    /// The resync a touched role asks for (`chelabase.resync.branch`): waits `delay` (so the
    /// DDL's commit is visible and a burst of statements is one run), then [`Self::resync`].
    /// A call while another for the same branch is still waiting returns at once: the waiting
    /// one covers it. The claim is released before the run, so a touch that arrives during
    /// the run schedules the next one.
    pub async fn resync_touched(
        &self,
        pending: &PendingResyncs,
        branch: BranchId,
        delay: Duration,
    ) -> Result<(), String> {
        if !pending.claim(branch) {
            return Ok(());
        }
        tokio::time::sleep(delay).await;
        pending.release(branch);
        self.resync(branch).await
    }

    /// `password_updated` for each name now, and again [`REPUBLISH_DELAY`] later. A failure
    /// only logs: a stale proxy cache refuses logins, it never lets a wrong password in.
    pub(crate) async fn publish_twice(&self, ep: &str, names: &[String]) {
        for name in names {
            match self.cache.password_updated(ep, name).await {
                Ok(0) => {
                    warn!(endpoint_id = %ep, role = name, "no proxy received password_updated");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(endpoint_id = %ep, role = name, "publishing password_updated: {e:#}");
                }
            }
        }
        if !names.is_empty() {
            self.cache
                .password_updated_later(ep, names.to_vec(), REPUBLISH_DELAY);
        }
    }
}

/// The owner's attributes, which an owner-level role is made with (N1, "match the owner").
const OWNER_ATTRIBUTES: &str = "CREATEROLE CREATEDB BYPASSRLS REPLICATION";

/// A role as Chelabase wants it (U1, U2): what [`create_role_sql`] makes and the repairs and
/// updates restore.
#[derive(Debug, Clone, Copy)]
pub struct WantedRole<'a> {
    pub name: &'a str,
    pub owner_level: bool,
    pub login: bool,
    pub attributes: &'a RoleAttributes,
}

/// A VALID UNTIL as a literal, to the microsecond Postgres keeps.
fn valid_until_literal(at: Option<DateTime<Utc>>) -> String {
    match at {
        Some(at) => quote_literal(&at.to_rfc3339_opts(SecondsFormat::Micros, true)),
        None => "'infinity'".to_string(),
    }
}

/// The microseconds a VALID UNTIL compares by.
fn micros(at: Option<DateTime<Utc>>) -> Option<i64> {
    at.map(|t| t.timestamp_micros())
}

/// `SET ROLE <owner>; CREATE ROLE … [NO]LOGIN [attributes] [PASSWORD …]; GRANT <group> TO …;
/// RESET ROLE` (the owner gets ADMIN on it through `createrole_self_grant`, and grants the
/// memberships it administers), then `GRANT neon_superuser` as `cloud_admin` for an
/// owner-level role, which is also made with the owner's attributes (the owner holds them, so
/// it may give them). Only attributes that differ from Postgres' defaults are spelled out.
pub fn create_role_sql(owner: &str, role: &WantedRole<'_>, verifier: Option<&str>) -> String {
    let ident = quote_ident(role.name);
    let a = role.attributes;
    let mut options = vec![if role.login { "LOGIN" } else { "NOLOGIN" }.to_string()];
    if role.owner_level {
        options.push(OWNER_ATTRIBUTES.to_string());
    }
    if !a.inherit {
        options.push("NOINHERIT".into());
    }
    if a.connection_limit != -1 {
        options.push(format!("CONNECTION LIMIT {}", a.connection_limit));
    }
    if a.valid_until.is_some() {
        options.push(format!("VALID UNTIL {}", valid_until_literal(a.valid_until)));
    }
    if a.bypass_rls && !role.owner_level {
        options.push("BYPASSRLS".into());
    }
    if let Some(verifier) = verifier {
        options.push(format!("PASSWORD {}", quote_literal(verifier)));
    }
    let mut sql = format!(
        "SET ROLE {};\nCREATE ROLE {ident} {};",
        quote_ident(owner),
        options.join(" ")
    );
    for group in &a.member_of {
        sql.push_str(&format!("\nGRANT {} TO {ident};", quote_ident(group)));
    }
    sql.push_str("\nRESET ROLE;");
    if role.owner_level {
        sql.push_str(&format!("\nGRANT neon_superuser TO {ident};"));
    }
    sql
}

/// `ALTER ROLE … PASSWORD …`, or `PASSWORD NULL` without a verifier, as `cloud_admin`.
pub fn password_sql(name: &str, verifier: Option<&str>) -> String {
    match verifier {
        Some(verifier) => set_password_sql(name, verifier),
        None => format!("ALTER ROLE {} PASSWORD NULL;", quote_ident(name)),
    }
}

/// One `ALTER ROLE` (as `cloud_admin`) for every attribute of `role` the catalog's `o`
/// doesn't have: LOGIN, INHERIT, CONNECTION LIMIT, VALID UNTIL and, for a role that isn't
/// owner-level (whose BYPASSRLS comes with the owner's attributes), BYPASSRLS. `None` when
/// they all match.
pub fn attributes_sql(role: &WantedRole<'_>, o: &ObservedRole) -> Option<String> {
    let a = role.attributes;
    let mut options = Vec::new();
    if role.login != o.login {
        options.push(if role.login { "LOGIN" } else { "NOLOGIN" }.to_string());
    }
    if a.inherit != o.inherit {
        options.push(if a.inherit { "INHERIT" } else { "NOINHERIT" }.to_string());
    }
    if a.connection_limit != o.connection_limit {
        options.push(format!("CONNECTION LIMIT {}", a.connection_limit));
    }
    if micros(a.valid_until) != micros(o.valid_until) {
        options.push(format!("VALID UNTIL {}", valid_until_literal(a.valid_until)));
    }
    if !role.owner_level && a.bypass_rls != o.bypass_rls {
        options.push(if a.bypass_rls { "BYPASSRLS" } else { "NOBYPASSRLS" }.to_string());
    }
    (!options.is_empty()).then(|| {
        format!("ALTER ROLE {} {};", quote_ident(role.name), options.join(" "))
    })
}

/// What takes the catalog's `o` to `wanted`, its password aside: one `ALTER ROLE` for the
/// attributes (LOGIN, INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS), owner level turned on
/// (the owner's attributes and `neon_superuser`) or off (both taken away, BYPASSRLS kept only
/// when wanted on its own), then the memberships as the owner. Empty when they all match. The
/// roles path's PATCH runs it after its password change; the resync drives an unconfirmed API
/// change with it (E32).
pub(crate) fn wanted_statements(
    owner: &str,
    wanted: &WantedRole<'_>,
    o: &ObservedRole,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut o = o.clone();
    let strip = !wanted.owner_level && o.owner_attributes;
    if strip {
        // The strip sets BYPASSRLS as wanted.
        o.bypass_rls = wanted.attributes.bypass_rls;
    }
    if let Some(sql) = attributes_sql(wanted, &o) {
        out.push(sql);
    }
    if strip {
        out.push(strip_owner_attributes_sql(wanted.name, wanted.attributes.bypass_rls));
    } else if wanted.owner_level && !o.owner_attributes {
        out.push(owner_attributes_sql(wanted.name));
    }
    if let Some(sql) = memberships_sql(owner, wanted, &o) {
        out.push(sql);
    }
    out
}

/// As the owner: `GRANT` each wanted membership the catalog's `o` lacks, and `REVOKE` each
/// one the owner granted that isn't wanted (never one of Chelabase's own roles, which the API
/// can't express). `None` when nothing differs.
pub fn memberships_sql(owner: &str, role: &WantedRole<'_>, o: &ObservedRole) -> Option<String> {
    let ident = quote_ident(role.name);
    let wanted = &role.attributes.member_of;
    let mut statements: Vec<String> = wanted
        .iter()
        .filter(|g| !o.member_of.contains(g))
        .map(|g| format!("GRANT {} TO {ident};", quote_ident(g)))
        .collect();
    statements.extend(
        o.owner_granted
            .iter()
            .filter(|g| !wanted.contains(g) && !is_membership_reserved(g))
            .map(|g| format!("REVOKE {} FROM {ident};", quote_ident(g))),
    );
    (!statements.is_empty()).then(|| {
        format!(
            "SET ROLE {};\n{}\nRESET ROLE;",
            quote_ident(owner),
            statements.join("\n")
        )
    })
}

/// Takes the owner's attributes and `neon_superuser` away from a role that stops being
/// owner-level, as `cloud_admin`; BYPASSRLS stays only when it's wanted on its own.
pub fn strip_owner_attributes_sql(name: &str, bypass_rls: bool) -> String {
    let ident = quote_ident(name);
    let bypass = if bypass_rls { "BYPASSRLS" } else { "NOBYPASSRLS" };
    format!(
        "ALTER ROLE {ident} NOCREATEROLE NOCREATEDB NOREPLICATION {bypass};\n\
         REVOKE neon_superuser FROM {ident};"
    )
}

/// Gives an owner-level role back the owner's attributes and `neon_superuser`, as
/// `cloud_admin`. Both statements are idempotent (a repeated GRANT is only a notice).
pub(crate) fn owner_attributes_sql(name: &str) -> String {
    let ident = quote_ident(name);
    format!("ALTER ROLE {ident} {OWNER_ATTRIBUTES};\nGRANT neon_superuser TO {ident};")
}

/// `ALTER ROLE … PASSWORD …` as `cloud_admin`.
pub(crate) fn set_password_sql(name: &str, verifier: &str) -> String {
    format!(
        "ALTER ROLE {} PASSWORD {};",
        quote_ident(name),
        quote_literal(verifier)
    )
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::fakes::{FakeLogin, FakeWorker, Journal, entries, position, ready_database, worker};

    // F3 (from F1): a merge's patch leaves out memberships the roles path can't make (`pg_*`,
    // Chelabase's own), so `member_of` isn't the exact set: the owner's grants of those are
    // never revoked by a patch that doesn't list them.
    #[test]
    fn a_membership_list_never_revokes_reserved_groups() {
        let mut o = ObservedRole::plain("app");
        o.member_of = vec!["chelabase_x".into(), "pg_read_all_data".into(), "readers".into(), "writers".into()];
        o.owner_granted = o.member_of.clone();
        let attributes = RoleAttributes {
            member_of: vec!["readers".into()],
            ..o.attributes()
        };
        let wanted = WantedRole {
            name: "app",
            owner_level: false,
            login: true,
            attributes: &attributes,
        };
        let sql = memberships_sql("postgres", &wanted, &o).unwrap();
        assert!(sql.contains("REVOKE \"writers\" FROM \"app\";"), "{sql}");
        assert!(!sql.contains("pg_read_all_data") && !sql.contains("chelabase_x"), "{sql}");
    }

    const SCRAM_A: &str = "SCRAM-SHA-256$4096:c2FsdA==$a:b";
    const SCRAM_B: &str = "SCRAM-SHA-256$4096:c2FsdA==$c:d";
    const MD5: &str = "md5000102030405060708090a0b0c0d0e0f";

    fn copy(name: &str, origin: RoleOrigin, verifier: Option<&str>) -> RoleCopy {
        RoleCopy {
            name: name.into(),
            origin,
            scram_verifier: verifier.map(str::to_string),
            password_enc: (origin == RoleOrigin::Api).then(|| "sealed".to_string()),
            login: true,
            level: RoleLevel::Plain,
            api_owner_level: false,
            pending: false,
            attributes_pending: false,
            created_at: DateTime::<Utc>::UNIX_EPOCH,
            attributes: RoleAttributes::default(),
        }
    }

    fn role(name: &str, password: Option<&str>) -> ObservedRole {
        ObservedRole {
            name: name.into(),
            login: true,
            password: password.map(str::to_string),
            owner_level: false,
            owner_admin: true,
            owner_attributes: false,
            ..ObservedRole::plain(name)
        }
    }

    fn observed(roles: Vec<ObservedRole>) -> Observed {
        Observed {
            roles,
            databases: vec![ObservedDatabase {
                name: "app".into(),
                owner: OWNER_ROLE.into(),
                system: false,
            }],
        }
    }

    /// The groups' statements in order.
    fn flat(groups: &[RepairGroup]) -> Vec<String> {
        groups.iter().flat_map(|g| g.statements.clone()).collect()
    }

    fn upsert(write: &ResyncWrite, name: &str) -> Option<CopyUpsert> {
        write.upserts.iter().find(|u| u.name == name).cloned()
    }

    /// The branch's current timeline in the key tests.
    const NOW: &str = "11111111111111111111111111111111";
    /// The timeline before a switch.
    const BEFORE: &str = "22222222222222222222222222222222";

    /// A live secret key bound to `oid` read on [`NOW`] (no timeline without an OID).
    fn key(role: &str, oid: Option<i64>) -> DataKeyRecord {
        key_on(role, oid, oid.map(|_| NOW))
    }

    fn key_on(role: &str, oid: Option<i64>, timeline: Option<&str>) -> DataKeyRecord {
        DataKeyRecord {
            role_timeline: timeline.map(str::to_string),
            id: uuid::Uuid::new_v4(),
            branch_id: BranchId::new(),
            kind: DataKeyKind::Secret,
            role: role.into(),
            name: format!("{role}-key"),
            prefix: "chk_sec_xxxx".into(),
            sealed: None,
            created_at: DateTime::<Utc>::UNIX_EPOCH,
            last_used_at: None,
            revoked_at: None,
            databases: None,
            role_oid: oid,
            role_bypass_rls: false,
        }
    }

    // Keys versus roles, by OID: a key whose OID is gone is revoked; one whose OID now has
    // another name follows it (a rename the receiver didn't see); a key without an OID gets
    // the OID of its name, or is revoked when the name is gone; a key whose name now belongs
    // to another OID (dropped and created again) is revoked. Exhaustive over the cases.
    #[test]
    fn plan_key_roles_follows_identity() {
        let catalog = vec![(16384, "app".to_string()), (16390, "renamed".to_string()), (16400, "again".to_string())];
        let alive = key("app", Some(16384));
        let gone = key("dropped", Some(16385));
        let moved = key("old_name", Some(16390));
        let recreated = key("again", Some(16399));
        let legacy = key("app", None);
        let legacy_gone = key("ghost", None);
        let keys = vec![alive.clone(), gone.clone(), moved.clone(), recreated.clone(), legacy.clone(), legacy_gone.clone()];
        assert_eq!(
            plan_key_roles(&keys, &catalog, NOW),
            vec![
                KeyRoleFix::Revoke { id: gone.id, role: "dropped".into(), oid: Some(16385) },
                KeyRoleFix::Follow { id: moved.id, from: "old_name".into(), to: "renamed".into(), oid: 16390, timeline: NOW.into() },
                KeyRoleFix::Revoke { id: recreated.id, role: "again".into(), oid: Some(16399) },
                KeyRoleFix::Backfill { id: legacy.id, role: "app".into(), oid: 16384, timeline: NOW.into() },
                KeyRoleFix::Revoke { id: legacy_gone.id, role: "ghost".into(), oid: None },
            ]
        );
        assert!(plan_key_roles(&[alive], &catalog, NOW).is_empty(), "a key whose OID has its name");
        // Public keys are never touched.
        let mut public = key("anon", None);
        public.kind = DataKeyKind::Public;
        assert!(plan_key_roles(&[public], &[], NOW).is_empty());
    }

    // E26: after a timeline switch (reset, restore, a merge's reset) the catalog's OID counter
    // diverged at the branch point, so an OID read on another timeline proves nothing alone.
    // A key whose OID still carries its name is re-stamped and kept; any other is revoked,
    // never followed. A key with an OID but no timeline (made before timelines were stored)
    // is treated the same way.
    #[test]
    fn a_key_from_another_timeline_is_kept_only_under_its_own_name() {
        let catalog = vec![(16384, "app".to_string()), (16390, "x".to_string())];
        // Same name, same OID after the switch: kept, stamped with the current timeline.
        let same = key_on("app", Some(16384), Some(BEFORE));
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&same), &catalog, NOW),
            vec![KeyRoleFix::Restamp {
                id: same.id,
                role: "app".into(),
                oid: 16384,
                from: Some(BEFORE.into()),
                timeline: NOW.into(),
            }]
        );
        // The concrete case: the source made `app` (OID 16390) with a key; the parent has `x`
        // at 16390. After the reset the key is revoked, never moved to `x`.
        let reused = key_on("app", Some(16390), Some(BEFORE));
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&reused), &catalog, NOW),
            vec![KeyRoleFix::Revoke { id: reused.id, role: "app".into(), oid: Some(16390) }]
        );
        // The OID is gone on the new timeline: revoked.
        let gone = key_on("app", Some(16500), Some(BEFORE));
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&gone), &catalog, NOW),
            vec![KeyRoleFix::Revoke { id: gone.id, role: "app".into(), oid: Some(16500) }]
        );
        // No timeline recorded: the same rules, the timeline stamped when kept.
        let unstamped = key_on("app", Some(16384), None);
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&unstamped), &catalog, NOW),
            vec![KeyRoleFix::Restamp {
                id: unstamped.id,
                role: "app".into(),
                oid: 16384,
                from: None,
                timeline: NOW.into(),
            }]
        );
        let unstamped_reused = key_on("app", Some(16390), None);
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&unstamped_reused), &catalog, NOW),
            vec![KeyRoleFix::Revoke { id: unstamped_reused.id, role: "app".into(), oid: Some(16390) }]
        );
    }

    // On its own timeline a key still follows a rename, and a backfill stamps the timeline.
    #[test]
    fn same_timeline_follows_and_backfill_stamps_the_timeline() {
        let catalog = vec![(16384, "app".to_string()), (16390, "renamed".to_string())];
        let moved = key_on("old_name", Some(16390), Some(NOW));
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&moved), &catalog, NOW),
            vec![KeyRoleFix::Follow {
                id: moved.id,
                from: "old_name".into(),
                to: "renamed".into(),
                oid: 16390,
                timeline: NOW.into(),
            }]
        );
        let legacy = key_on("app", None, None);
        assert_eq!(
            plan_key_roles(std::slice::from_ref(&legacy), &catalog, NOW),
            vec![KeyRoleFix::Backfill { id: legacy.id, role: "app".into(), oid: 16384, timeline: NOW.into() }]
        );
    }

    /// The key after `fixes` (`None`: revoked), as the store applies them.
    fn apply_fixes(k: &DataKeyRecord, fixes: &[KeyRoleFix]) -> Option<DataKeyRecord> {
        let mut k = k.clone();
        for fix in fixes {
            match fix {
                KeyRoleFix::Revoke { id, role, oid } if *id == k.id && *role == k.role && *oid == k.role_oid => {
                    return None;
                }
                KeyRoleFix::Follow { id, from, to, oid, timeline }
                    if *id == k.id && *from == k.role && Some(*oid) == k.role_oid && Some(timeline) == k.role_timeline.as_ref() =>
                {
                    k.role = to.clone();
                }
                KeyRoleFix::Backfill { id, role, oid, timeline } if *id == k.id && *role == k.role && k.role_oid.is_none() => {
                    k.role_oid = Some(*oid);
                    k.role_timeline = Some(timeline.clone());
                }
                KeyRoleFix::Restamp { id, role, oid, from, timeline }
                    if *id == k.id && *role == k.role && Some(*oid) == k.role_oid && *from == k.role_timeline =>
                {
                    k.role_timeline = Some(timeline.clone());
                }
                other => panic!("a fix that doesn't match the key it names: {other:?} for {k:?}"),
            }
        }
        Some(k)
    }

    /// The role rest runs a live key as on `catalog`: its OID's role, when that role has the
    /// key's name (`None`: refused).
    fn served(k: &DataKeyRecord, catalog: &[(i64, String)]) -> Option<(i64, String)> {
        let oid = k.role_oid?;
        catalog.iter().find(|(o, n)| *o == oid && *n == k.role).cloned()
    }

    // The invariant (E26, D24): a key is only ever served as the role it was bound to: the
    // same OID on the same timeline, or the same OID and the same name after a switch (a key
    // without an OID, made before OIDs were stored, by its name). And a key whose binding
    // still holds is never revoked. Exhaustive over every catalog of up to three roles among
    // three OIDs and three names, every key (each name; no OID, each catalog OID or one
    // outside it; no timeline, the current one or an earlier one), on the current timeline;
    // the plan applied once is stable.
    #[test]
    fn plan_key_roles_serves_a_key_only_as_its_bound_role_exhaustively() {
        let oids = [16384_i64, 16385, 16386];
        let names = ["a", "b", "c"];
        // Every partial injective map OID → name.
        let mut catalogs: Vec<Vec<(i64, String)>> = vec![Vec::new()];
        for oid in oids {
            let mut next = Vec::new();
            for c in &catalogs {
                next.push(c.clone());
                for name in names {
                    if !c.iter().any(|(_, n)| n == name) {
                        let mut with = c.clone();
                        with.push((oid, name.to_string()));
                        next.push(with);
                    }
                }
            }
            catalogs = next;
        }
        assert_eq!(catalogs.len(), 34);
        let mut checked = 0;
        for catalog in &catalogs {
            for role in names {
                for oid in [None, Some(16384), Some(16385), Some(16386), Some(16999)] {
                    for timeline in [None, Some(NOW), Some(BEFORE)] {
                        if oid.is_none() && timeline.is_some() {
                            continue; // the column's CHECK: no timeline without an OID
                        }
                        let k = key_on(role, oid, timeline);
                        let fixes = plan_key_roles(std::slice::from_ref(&k), catalog, NOW);
                        let after = apply_fixes(&k, &fixes);
                        let case = format!("{k:?} on {catalog:?}: {fixes:?}");
                        let name_of = |o: i64| catalog.iter().find(|(c, _)| *c == o).map(|(_, n)| n.clone());
                        // Whether the binding holds, and the role it binds to now.
                        let bound: Option<(i64, String)> = match (oid, timeline) {
                            (Some(o), Some(t)) if t == NOW => name_of(o).map(|n| (o, n)),
                            (Some(o), _) => (name_of(o).as_deref() == Some(role)).then(|| (o, role.to_string())),
                            (None, _) => catalog.iter().find(|(_, n)| n == role).cloned(),
                        };
                        // In the window before the sync, rest's preamble serves the key as its
                        // bound role or not at all, never as another one.
                        let before = served(&k, catalog);
                        assert!(
                            before.is_none() || before == bound,
                            "served as another role before the sync: {case}"
                        );
                        match (&after, &bound) {
                            (Some(after), Some(bound)) => {
                                assert_eq!(served(after, catalog).as_ref(), Some(bound), "served as its bound role: {case}");
                                assert_eq!(after.role_timeline.as_deref(), Some(NOW), "stamped: {case}");
                                assert!(
                                    plan_key_roles(std::slice::from_ref(after), catalog, NOW).is_empty(),
                                    "stable: {case}"
                                );
                            }
                            (None, None) => {}
                            (Some(after), None) => {
                                panic!("a key whose binding is broken stays live as {:?}: {case}", served(after, catalog))
                            }
                            (None, Some(_)) => panic!("a key whose binding holds is revoked: {case}"),
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 34 * 3 * (1 + 4 * 3));
    }

    // E33: a live key's BYPASSRLS mark follows the role it stays bound to (after the plan's
    // own repairs), by OID; a revoked key, a public key, or one whose mark is right gets none.
    #[test]
    fn key_bypass_marks_follow_the_bound_role() {
        let catalog = vec![
            (16384, "app".to_string(), true),
            (16390, "renamed".to_string(), false),
        ];
        let names: Vec<(i64, String)> = catalog.iter().map(|(o, n, _)| (*o, n.clone())).collect();
        let gained = key("app", Some(16384));
        let mut lost = key("old_name", Some(16390));
        lost.role_bypass_rls = true;
        let gone = key("gone", Some(16385));
        let legacy = key("app", None);
        let mut right = key("app", Some(16384));
        right.role_bypass_rls = true;
        let mut public = key("anon", None);
        public.kind = DataKeyKind::Public;
        let keys = vec![gained.clone(), lost.clone(), gone, legacy.clone(), right, public];
        let fixes = plan_key_roles(&keys, &names, NOW);
        assert_eq!(
            plan_key_bypass(&keys, &catalog, &fixes),
            vec![
                KeyRoleFix::Bypass { id: gained.id, oid: 16384, bypass_rls: true },
                KeyRoleFix::Bypass { id: lost.id, oid: 16390, bypass_rls: false },
                KeyRoleFix::Bypass { id: legacy.id, oid: 16384, bypass_rls: true },
            ]
        );
        let line = format!("16384 {} t\n16390 {} f\n16391 {}\n", hex::encode("a"), hex::encode("b"), hex::encode("c"));
        assert_eq!(
            parse_key_role_flags(&line).unwrap(),
            vec![(16384, "a".to_string(), true), (16390, "b".to_string(), false), (16391, "c".to_string(), false)]
        );
        assert!(parse_key_role_flags(&format!("1 {} maybe\n", hex::encode("a"))).is_err());
        assert!(KEY_ROLES_SQL.contains("rolbypassrls"), "{KEY_ROLES_SQL}");
    }

    #[test]
    fn key_roles_catalog_parses_oid_and_hex_name() {
        let out = format!("16384 {}\n16390 {}\n", hex::encode("app"), hex::encode("a b\n\"c"));
        assert_eq!(
            parse_key_roles(&out).unwrap(),
            vec![(16384, "app".to_string()), (16390, "a b\n\"c".to_string())]
        );
        assert!(parse_key_roles("x 61\n").is_err());
        assert!(parse_key_roles("16384 zz\n").is_err());
        assert!(KEY_ROLES_SQL.contains("FROM pg_roles"), "{KEY_ROLES_SQL}");
        assert!(KEY_ROLES_SQL.contains("chelabase: key roles"), "the fakes' needle");
    }

    fn store_key(w: &FakeWorker, branch: BranchId, k: &DataKeyRecord) {
        w.metadata.state().data_keys.push(chelabase_metadata::NewDataKey {
            id: k.id,
            branch_id: branch,
            kind: DataKeyKind::Secret,
            role: k.role.clone(),
            name: k.name.clone(),
            prefix: k.prefix.clone(),
            sha256: [0; 32],
            sealed: None,
            databases: None,
            role_oid: k.role_oid,
            role_timeline: k.role_timeline.clone(),
            role_bypass_rls: k.role_bypass_rls,
        });
    }

    // The resync applies the plan after the roles, on the branch's current timeline: a key
    // whose OID is gone is revoked; one read on another timeline whose OID now has another
    // name is revoked too (E26), never followed; one still under its name is re-stamped.
    #[tokio::test]
    async fn resync_revokes_a_key_whose_role_oid_is_gone() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;
        let now = w.metadata.branch(branch).timeline_id.unwrap();
        let alive = key_on("app", Some(16384), Some(&now));
        let gone = key_on("app", Some(16385), Some(&now));
        let reused = key_on("old", Some(16390), Some(BEFORE));
        let kept = key_on("app", Some(16384), Some(BEFORE));
        for k in [&alive, &gone, &reused, &kept] {
            store_key(&w, branch, k);
        }
        w.computes.answer(
            "chelabase: key roles",
            &format!("16384 {}\n16390 {}\n", hex::encode("app"), hex::encode("x")),
        );
        w.resync(branch).await.unwrap();
        let fixes = w.metadata.state().key_fixes.clone();
        assert_eq!(
            fixes,
            vec![(
                branch,
                vec![
                    KeyRoleFix::Revoke { id: gone.id, role: "app".into(), oid: Some(16385) },
                    KeyRoleFix::Revoke { id: reused.id, role: "old".into(), oid: Some(16390) },
                    KeyRoleFix::Restamp {
                        id: kept.id,
                        role: "app".into(),
                        oid: 16384,
                        from: Some(BEFORE.into()),
                        timeline: now.clone(),
                    },
                ]
            )]
        );
    }

    // E33a: at worker start every ready branch with live secret keys gets its key marks
    // synced once, so a key whose role gained BYPASSRLS in SQL before the upgrade is marked
    // at once. A branch without secret keys reads nothing.
    #[tokio::test]
    async fn key_marks_are_synced_for_every_branch_at_start() {
        let (w, journal) = worker();
        let (main, _ep) = ready(&w, &journal).await;
        let db = w.metadata.branch(main).database_id;
        let dev = crate::fakes::ready_branch(&w, db, "dev", main).await;
        let bare = crate::fakes::ready_branch(&w, db, "bare", main).await;
        let mut keys = Vec::new();
        for branch in [main, dev] {
            let now = w.metadata.branch(branch).timeline_id.unwrap();
            let k = key_on("svc", Some(16384), Some(&now));
            store_key(&w, branch, &k);
            keys.push((branch, k));
        }
        w.computes.answer("chelabase: key roles", &format!("16384 {} t\n", hex::encode("svc")));
        journal.lock().unwrap().clear();

        w.sync_key_marks_at_start().await;

        let mut fixes = w.metadata.state().key_fixes.clone();
        fixes.sort_by_key(|(b, _)| b.to_string());
        let mut want: Vec<(BranchId, Vec<KeyRoleFix>)> = keys
            .iter()
            .map(|(b, k)| (*b, vec![KeyRoleFix::Bypass { id: k.id, oid: 16384, bypass_rls: true }]))
            .collect();
        want.sort_by_key(|(b, _)| b.to_string());
        assert_eq!(fixes, want);
        let bare_ep = w.metadata.branch(bare).endpoint_id.unwrap();
        assert!(
            !entries(&journal).iter().any(|e| e.starts_with(&format!("compute.sql {bare_ep}"))),
            "{:?}",
            entries(&journal)
        );
    }

    // A branch whose timeline moves while the roles are read (a switch landed meanwhile)
    // gets no key repairs this run: the catalog may be the other timeline's. The timeline is
    // read before and after the catalog (`resync` reads the branch once first, for its
    // endpoint); without the move the same run follows the rename.
    #[tokio::test]
    async fn key_repairs_wait_when_the_timeline_moves_during_the_read() {
        for moves in [false, true] {
            let (w, journal) = worker();
            let (branch, _ep) = ready(&w, &journal).await;
            let now = w.metadata.branch(branch).timeline_id.unwrap();
            let k = key_on("old", Some(16390), Some(&now));
            store_key(&w, branch, &k);
            w.computes.answer("chelabase: key roles", &format!("16390 {}\n", hex::encode("x")));
            if moves {
                w.metadata.state().move_timeline_at_get_branch(branch, 3, "33333333333333333333333333333333");
            }
            w.resync(branch).await.unwrap();
            let fixes = w.metadata.state().key_fixes.clone();
            if moves {
                assert!(fixes.is_empty(), "{fixes:?}");
            } else {
                assert_eq!(
                    fixes,
                    vec![(
                        branch,
                        vec![KeyRoleFix::Follow { id: k.id, from: "old".into(), to: "x".into(), oid: 16390, timeline: now }]
                    )]
                );
            }
        }
    }

    // I3: every catalog role with a password (MD5 too) gets its fresh verifier: the recorded
    // one, or a generated one that the write stores sealed; the owner and roles without a
    // password are left alone, and a retry that sees the fresh hashes sets nothing again.
    #[test]
    fn fresh_passwords_cover_the_whole_catalog_once() {
        let mut fresh = FreshPasswords::new([("rec".to_string(), SCRAM_B.to_string())]);
        let mut seen = observed(vec![
            role(OWNER_ROLE, Some(SCRAM_A)),
            role("rec", Some(SCRAM_A)),
            role("late", Some(SCRAM_A)),
            role("old", Some(MD5)),
            role("none", None),
        ]);
        let mut generated = Vec::new();
        let script = fresh.assign(OWNER_ROLE, &mut seen, |name| {
            generated.push(name.to_string());
            (
                format!("SCRAM-SHA-256$4096:c2FsdA==${name}:x"),
                format!("sealed-{name}"),
            )
        });
        assert_eq!(generated, ["late", "old"]);
        let roles: Vec<&str> = script.iter().map(|g| g.role.as_str()).collect();
        assert_eq!(roles, ["rec", "late", "old"]);
        let script = flat(&script);
        let late = "SCRAM-SHA-256$4096:c2FsdA==$late:x";
        let old = "SCRAM-SHA-256$4096:c2FsdA==$old:x";
        assert_eq!(
            script,
            [
                set_password_sql("rec", SCRAM_B),
                set_password_sql("late", late),
                set_password_sql("old", old),
            ]
        );
        let password = |name: &str| {
            seen.roles
                .iter()
                .find(|o| o.name == name)
                .unwrap()
                .password
                .clone()
        };
        assert_eq!(password(OWNER_ROLE).as_deref(), Some(SCRAM_A));
        assert_eq!(password("rec").as_deref(), Some(SCRAM_B));
        assert_eq!(password("late").as_deref(), Some(late));
        assert_eq!(password("none"), None);

        let copies = [
            copy("rec", RoleOrigin::Sql, Some(SCRAM_B)),
            copy("old", RoleOrigin::Sql, None),
        ];
        let (_, mut write) = plan_resync(OWNER_ROLE, &copies, &seen);
        fresh.seal(&mut write);
        let sealed = |name: &str| upsert(&write, name).unwrap().password_enc;
        // The recorded copy's row holds its sealed password already.
        assert_eq!(sealed("rec"), SealedChange::Keep);
        assert_eq!(sealed("late"), SealedChange::Set("sealed-late".into()));
        assert_eq!(sealed("old"), SealedChange::Set("sealed-old".into()));
        assert_eq!(
            upsert(&write, "old").unwrap().verifier.as_deref(),
            Some(old)
        );

        // A retry reads the fresh hashes: nothing is set or generated again.
        let mut again = seen.clone();
        let script = fresh.assign(OWNER_ROLE, &mut again, |_| unreachable!());
        assert!(script.is_empty(), "{script:?}");
        let (_, mut write) = plan_resync(OWNER_ROLE, &copies, &again);
        fresh.seal(&mut write);
        assert_eq!(sealed("late"), SealedChange::Set("sealed-late".into()));
        assert_eq!(
            upsert(&write, "late").unwrap().password_enc,
            SealedChange::Set("sealed-late".into())
        );
    }

    /// One `r` line as psql prints it.
    fn r_line(
        name: &str,
        login: bool,
        password: Option<&str>,
        level: bool,
        admin: bool,
        attributes: bool,
    ) -> String {
        let tf = |b: bool| if b { "t" } else { "f" };
        format!(
            "r|{}|{}|{}|{}|{}|{}|t|-1||f|\n",
            hex::encode(name),
            tf(login),
            password.map(hex::encode).unwrap_or_default(),
            tf(level),
            tf(admin),
            tf(attributes)
        )
    }

    fn d_line(name: &str, owner: &str, system: bool) -> String {
        format!(
            "d|{}||{}|{}|||||||\n",
            hex::encode(name),
            hex::encode(owner),
            if system { "t" } else { "f" }
        )
    }

    #[test]
    fn resync_sql_is_the_planned_query_with_the_owner_quoted() {
        let sql = resync_sql("o'wner");
        assert!(sql.starts_with("SET search_path = pg_catalog, pg_temp;\nSELECT 'r',"));
        assert!(
            sql.contains("AND m.member = (SELECT oid FROM pg_roles WHERE rolname = 'o''wner'))")
        );
        assert!(sql.contains("AND a.rolname <> 'o''wner'\n"));
        assert!(!sql.contains("$OWNER"));
        assert!(sql.ends_with("ORDER BY 1, 2;"));
        // The owner's attributes and a direct neon_superuser membership, for N1's repair.
        assert!(sql.contains(
            "(a.rolcreaterole AND a.rolcreatedb AND a.rolbypassrls AND a.rolreplication\n        \
             AND EXISTS (SELECT 1 FROM pg_auth_members s\n                    \
             WHERE s.member = a.oid AND s.roleid = 'neon_superuser'::regrole))"
        ));
    }

    /// E33: the owner-level column is the narrow definition (what a secret key's role may not
    /// have); BYPASSRLS is its own column.
    #[test]
    fn the_owner_level_column_leaves_bypassrls_out() {
        let sql = resync_sql("postgres");
        assert!(
            sql.contains(
                "(a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication\n        \
                 OR pg_has_role(a.oid, 'neon_superuser', 'MEMBER')),"
            ),
            "{sql}"
        );
    }

    /// I1: the app users sync login is Chelabase's, like `authenticator`: the catalog read
    /// leaves it out, so it never becomes a `neon_roles` row.
    #[test]
    fn resync_ignores_the_sync_role() {
        let sql = resync_sql("postgres");
        let start = sql.find("AND a.rolname NOT IN (").unwrap();
        let list = &sql[start..start + sql[start..].find(')').unwrap()];
        for role in ["authenticator", "chelabase_admin", crate::app_users_sql::USERS_SYNC_ROLE] {
            assert!(list.contains(&format!("'{role}'")), "{role} in {list}");
        }
    }

    #[test]
    fn resync_sql_skips_reserved_prefixes() {
        let sql = resync_sql("postgres");
        assert!(sql.contains("AND a.rolname !~* '^(cb_m_|chelabase_)'"), "{sql}");
    }

    /// A row named with a hidden prefix is never re-created: the catalog read never shows
    /// such a role, so a repair would fail "already exists" on every resync.
    #[test]
    fn resync_never_plans_create_for_hidden_prefix_rows() {
        for name in ["cb_m_x", "CB_M_Y", "chelabase_x", "Chelabase_Y"] {
            let copies = [
                copy(OWNER_ROLE, RoleOrigin::Owner, Some(SCRAM_A)),
                copy(name, RoleOrigin::Api, Some(SCRAM_A)),
            ];
            let (repairs, _write) = plan_resync(OWNER_ROLE, &copies, &observed(vec![]));
            assert!(flat(&repairs).is_empty(), "{name}: {repairs:?}");
        }
    }

    #[test]
    fn parse_observed_decodes_hex_and_refuses_garbage() {
        let odd = "we|ird\nname";
        let out = format!(
            "{}{}{}{}",
            r_line(odd, true, Some(SCRAM_A), false, true, false),
            r_line("nopass", false, None, true, false, true),
            d_line("app", "postgres", false),
            d_line("postgres", "cloud_admin", true),
        );

        let o = parse_observed(&out).unwrap();

        assert_eq!(
            o.roles,
            [
                ObservedRole {
                    name: odd.into(),
                    login: true,
                    password: Some(SCRAM_A.into()),
                    owner_level: false,
                    owner_admin: true,
                    owner_attributes: false,
                    ..ObservedRole::plain(odd)
                },
                ObservedRole {
                    name: "nopass".into(),
                    login: false,
                    password: None,
                    owner_level: true,
                    owner_admin: false,
                    owner_attributes: true,
                    ..ObservedRole::plain("nopass")
                },
            ]
        );
        assert_eq!(
            o.databases,
            [
                ObservedDatabase {
                    name: "app".into(),
                    owner: "postgres".into(),
                    system: false
                },
                ObservedDatabase {
                    name: "postgres".into(),
                    owner: "cloud_admin".into(),
                    system: true
                },
            ]
        );
        assert_eq!(parse_observed("").unwrap(), Observed::default());

        for bad in [
            "x|00|t||f|f|f|t|-1||f|",
            "r|zz|t||f|f|f|t|-1||f|",
            "r|61|t||f|f|t|-1||f|",
            "r|61|maybe||f|f|f|t|-1||f|",
            "r||t||f|f|f|t|-1||f|",
            "r|61|t|zz|f|f|f|t|-1||f|",
            "r|61|t||f|f|maybe|t|-1||f|",
            "r|61|t||f|f|f|maybe|-1||f|",
            "r|61|t||f|f|f|t|lots||f|",
            "r|61|t||f|f|f|t|-2||f|",
            "r|61|t||f|f|f|t|-1|soon|f|",
            "r|61|t||f|f|f|t|-1||maybe|",
            "r|61|t||f|f|f|t|-1||f|zz:o",
            "r|61|t||f|f|f|t|-1||f|62:q",
            "r|61|t||f|f|f|t|-1||f|62",
            "d|61|t|62|f|||||||",
            "d|61||62|f|t||||||",
            "d|61||62|f||t|||||",
            "d|61||62|f|||t||||",
            "d|61||62|f|||||||62:o",
            "r|61|t||f|f|f",
            "r|61|t||f|f|f|t|-1||f||extra",
        ] {
            let err = parse_observed(bad).unwrap_err().to_string();
            assert!(
                !err.contains(bad),
                "{bad}: the error echoes the line: {err}"
            );
        }
    }

    #[test]
    fn plan_resync_sql_rows() {
        let copies = [
            copy(OWNER_ROLE, RoleOrigin::Owner, Some(SCRAM_A)),
            copy("same", RoleOrigin::Sql, Some(SCRAM_A)),
            copy("changed", RoleOrigin::Sql, Some(SCRAM_A)),
            copy("gone", RoleOrigin::Sql, Some(SCRAM_A)),
            copy("now_md5", RoleOrigin::Sql, Some(SCRAM_A)),
            copy("lost_password", RoleOrigin::Sql, Some(SCRAM_A)),
            copy("revealable", RoleOrigin::Sql, Some(SCRAM_B)),
        ];
        let mut demoted = role("same", Some(SCRAM_A));
        demoted.login = false;
        demoted.owner_level = true;
        let mut roles = vec![
            demoted,
            role("changed", Some(SCRAM_B)),
            role("now_md5", Some(MD5)),
            role("lost_password", None),
            role("revealable", Some(SCRAM_B)),
            role("added", Some(SCRAM_A)),
            role("added_md5", Some(MD5)),
            role("no_password", None),
        ];
        // Over any cap: nothing is dropped for it.
        for i in 0..60 {
            roles.push(role(&format!("bulk{i}"), Some(SCRAM_A)));
        }
        let mut copies = copies.to_vec();
        copies[6].password_enc = Some("sealed".into());

        let (repairs, write) = plan_resync(OWNER_ROLE, &copies, &observed(roles));

        assert!(repairs.is_empty(), "{repairs:?}");
        let up =
            |name: &str| upsert(&write, name).unwrap_or_else(|| panic!("no upsert for {name}"));
        // (name, verifier, sealed change, login, level)
        let cases = [
            (
                "same",
                Some(SCRAM_A),
                SealedChange::Keep,
                false,
                RoleLevel::OwnerLevel,
            ),
            (
                "changed",
                Some(SCRAM_B),
                SealedChange::Clear,
                true,
                RoleLevel::Plain,
            ),
            ("now_md5", None, SealedChange::Clear, true, RoleLevel::Plain),
            (
                "revealable",
                Some(SCRAM_B),
                SealedChange::Keep,
                true,
                RoleLevel::Plain,
            ),
            (
                "added",
                Some(SCRAM_A),
                SealedChange::Clear,
                true,
                RoleLevel::Plain,
            ),
            (
                "added_md5",
                None,
                SealedChange::Clear,
                true,
                RoleLevel::Plain,
            ),
        ];
        for (name, verifier, sealed, login, level) in cases {
            let u = up(name);
            assert_eq!(u.origin, RoleOrigin::Sql, "{name}");
            assert_eq!(u.verifier.as_deref(), verifier, "{name}");
            assert_eq!(u.password_enc, sealed, "{name}");
            assert_eq!(
                (u.login, u.level, u.pending),
                (login, level, false),
                "{name}"
            );
        }
        assert_eq!(up("bulk59").verifier.as_deref(), Some(SCRAM_A));
        // A role without a password (a group role, or one whose password was removed) is kept
        // as a row with no verifier: it exists, and members can be granted it.
        for name in ["no_password", "lost_password"] {
            let u = up(name);
            assert_eq!(u.verifier, None, "{name}");
            assert!(u.no_password, "{name}");
            assert_eq!(u.password_enc, SealedChange::Clear, "{name}");
        }
        for name in ["same", "changed", "now_md5", "added", "added_md5"] {
            assert!(!up(name).no_password, "{name}");
        }
        let mut removals = write.removals.clone();
        removals.sort();
        assert_eq!(removals, ["gone"]);
        assert!(upsert(&write, OWNER_ROLE).is_none());
        assert_eq!(write.upserts.len(), 8 + 60);
        assert_eq!(write.databases, observed(vec![]).databases);
    }

    /// Group roles made in SQL (no password) reach the metadata, so members can be granted
    /// them; reserved names (the data API's `anon`, `authenticated`, …) never do.
    #[test]
    fn plan_resync_keeps_group_roles_but_not_reserved_ones() {
        let mut group = role("billing", None);
        group.login = false;
        let mut roles = vec![group, role("app_login", None)];
        for reserved in ["anon", "authenticated", "authenticator", "neon_superuser", "pg_x"] {
            let mut r = role(reserved, None);
            r.login = false;
            roles.push(r);
        }
        let (_, write) = plan_resync(OWNER_ROLE, &[], &observed(roles));
        let billing = upsert(&write, "billing").expect("billing is kept");
        assert_eq!((billing.verifier.as_deref(), billing.no_password, billing.login), (None, true, false));
        assert!(upsert(&write, "app_login").is_some_and(|u| u.no_password && u.login));
        for reserved in ["anon", "authenticated", "authenticator", "neon_superuser", "pg_x"] {
            assert!(upsert(&write, reserved).is_none(), "{reserved}");
        }
    }

    // T4: an API reset of an `sql` row that couldn't be applied leaves it pending with the new
    // verifier and its sealed password. The reply promised a retry, so the resync sets the
    // stored verifier back, like an `api` row's; a dropped role still goes (the database wins
    // for existence), and a confirmed row still follows the catalog.
    #[test]
    fn plan_resync_retries_a_pending_sql_reset() {
        let mut reset = copy("reset", RoleOrigin::Sql, Some(SCRAM_B));
        reset.password_enc = Some("sealed".into());
        reset.pending = true;
        let mut dropped = reset.clone();
        dropped.name = "dropped".into();
        let mut confirmed = reset.clone();
        confirmed.name = "confirmed".into();
        confirmed.pending = false;
        let copies = [
            copy(OWNER_ROLE, RoleOrigin::Owner, Some(SCRAM_A)),
            reset,
            dropped,
            confirmed,
        ];
        let seen = observed(vec![
            role("reset", Some(SCRAM_A)),
            role("confirmed", Some(SCRAM_A)),
        ]);

        let (repairs, write) = plan_resync(OWNER_ROLE, &copies, &seen);

        assert_eq!(
            repairs,
            [RepairGroup {
                role: "reset".into(),
                statements: vec![set_password_sql("reset", SCRAM_B)],
            }]
        );
        let reset = upsert(&write, "reset").unwrap();
        assert_eq!(
            (
                reset.origin,
                reset.verifier.as_deref(),
                reset.password_enc,
                reset.pending
            ),
            (RoleOrigin::Sql, Some(SCRAM_B), SealedChange::Keep, false)
        );
        assert_eq!(write.removals, ["dropped"]);
        let confirmed = upsert(&write, "confirmed").unwrap();
        assert_eq!(
            (confirmed.verifier.as_deref(), confirmed.password_enc),
            (Some(SCRAM_A), SealedChange::Clear)
        );
    }

    // E32: SQL controls an API role's LOGIN, owner level, attributes and memberships: an
    // owner-level role stripped in SQL stays stripped and is recorded so; only a change the
    // API wrote and the compute never confirmed (`attributes_pending`) is driven. Existence
    // and the password are still the control plane's.
    #[test]
    fn plan_resync_api_rows() {
        let mut admin = copy("admin", RoleOrigin::Api, Some(SCRAM_A));
        admin.api_owner_level = true;
        admin.level = RoleLevel::OwnerLevel;
        admin.pending = true;
        admin.attributes_pending = true;
        let mut no_verifier = copy("broken", RoleOrigin::Api, None);
        no_verifier.pending = true;
        let owner_level_copy = |name: &str| {
            let mut c = copy(name, RoleOrigin::Api, Some(SCRAM_A));
            c.api_owner_level = true;
            c.level = RoleLevel::OwnerLevel;
            c
        };
        let mut boss_unapplied = owner_level_copy("boss_unapplied");
        boss_unapplied.pending = true;
        boss_unapplied.attributes_pending = true;
        let copies = [
            copy(OWNER_ROLE, RoleOrigin::Owner, Some(SCRAM_A)),
            admin,
            owner_level_copy("boss_ok"),
            owner_level_copy("boss_stripped"),
            owner_level_copy("boss_partial"),
            boss_unapplied,
            copy("plain_gone", RoleOrigin::Api, Some(SCRAM_A)),
            copy("drifted", RoleOrigin::Api, Some(SCRAM_A)),
            copy("cleared", RoleOrigin::Api, Some(SCRAM_A)),
            copy("no_admin", RoleOrigin::Api, Some(SCRAM_A)),
            copy("fine", RoleOrigin::Api, Some(SCRAM_A)),
            no_verifier,
        ];
        let mut no_admin = role("no_admin", Some(SCRAM_A));
        no_admin.owner_admin = false;
        let mut fine = role("fine", Some(SCRAM_A));
        fine.owner_level = true;
        let mut boss_ok = role("boss_ok", Some(SCRAM_A));
        boss_ok.owner_level = true;
        boss_ok.owner_attributes = true;
        // Every attribute and the membership taken away in SQL.
        let boss_stripped = role("boss_stripped", Some(SCRAM_A));
        // A create whose GRANT failed (Task 4's minor): some attributes, no membership.
        let mut boss_partial = role("boss_partial", Some(SCRAM_A));
        boss_partial.owner_level = true;
        let roles = vec![
            boss_ok,
            boss_stripped,
            boss_partial,
            role("boss_unapplied", Some(SCRAM_A)),
            role("drifted", Some(SCRAM_B)),
            role("cleared", None),
            no_admin,
            fine,
            role("broken", Some(SCRAM_B)),
            // The owner is excluded by the query; were it there, it's never touched.
            role(OWNER_ROLE, Some(SCRAM_B)),
        ];

        let (repairs, write) = plan_resync(OWNER_ROLE, &copies, &observed(roles));

        let lit = quote_literal(SCRAM_A);
        let roles: Vec<&str> = repairs.iter().map(|g| g.role.as_str()).collect();
        assert_eq!(
            roles,
            ["admin", "boss_unapplied", "plain_gone", "drifted", "cleared", "no_admin"]
        );
        assert_eq!(
            flat(&repairs),
            [
                format!(
                    "SET ROLE \"postgres\";\nCREATE ROLE \"admin\" LOGIN CREATEROLE CREATEDB \
                     BYPASSRLS REPLICATION PASSWORD {lit};\n\
                     RESET ROLE;\nGRANT neon_superuser TO \"admin\";"
                ),
                "ALTER ROLE \"boss_unapplied\" CREATEROLE CREATEDB BYPASSRLS REPLICATION;\n\
                 GRANT neon_superuser TO \"boss_unapplied\";"
                    .to_string(),
                format!(
                    "SET ROLE \"postgres\";\nCREATE ROLE \"plain_gone\" LOGIN PASSWORD {lit};\n\
                     RESET ROLE;"
                ),
                format!("ALTER ROLE \"drifted\" PASSWORD {lit};"),
                format!("ALTER ROLE \"cleared\" PASSWORD {lit};"),
                "GRANT \"no_admin\" TO \"postgres\" WITH ADMIN OPTION, INHERIT FALSE, SET FALSE;"
                    .to_string(),
            ]
        );
        for name in [
            "admin",
            "boss_ok",
            "boss_stripped",
            "boss_partial",
            "boss_unapplied",
            "plain_gone",
            "drifted",
            "cleared",
            "no_admin",
            "fine",
        ] {
            let u = upsert(&write, name).unwrap_or_else(|| panic!("no upsert for {name}"));
            assert_eq!(u.verifier.as_deref(), Some(SCRAM_A), "{name}");
            assert_eq!(u.password_enc, SealedChange::Keep, "{name}");
            assert!(!u.pending, "{name}");
        }
        // (level, api_owner_level) as written: what was driven, or what the catalog shows.
        let shape = |name: &str| {
            let u = upsert(&write, name).unwrap();
            (u.level, u.api_owner_level)
        };
        assert_eq!(shape("admin"), (RoleLevel::OwnerLevel, None));
        assert_eq!(shape("boss_unapplied"), (RoleLevel::OwnerLevel, None));
        assert_eq!(shape("plain_gone"), (RoleLevel::Plain, None));
        assert_eq!(shape("boss_ok"), (RoleLevel::OwnerLevel, Some(true)));
        assert_eq!(shape("boss_stripped"), (RoleLevel::Plain, Some(false)), "stays stripped");
        assert_eq!(shape("boss_partial"), (RoleLevel::OwnerLevel, Some(false)));
        assert_eq!(shape("fine"), (RoleLevel::OwnerLevel, Some(false)));
        assert!(upsert(&write, "admin").unwrap().login);
        assert!(upsert(&write, "broken").is_none());
        assert!(upsert(&write, OWNER_ROLE).is_none());
        assert!(write.removals.is_empty(), "{:?}", write.removals);
    }

    // E32, the upgrade: rows from before 6g carry the migration's defaults (no memberships,
    // no limit, INHERIT); the first resync records what SQL made of the role instead of
    // revoking a group, re-adding INHERIT or stripping BYPASSRLS. Nothing is run.
    #[test]
    fn the_first_resync_after_an_upgrade_records_sql_changes_on_api_roles() {
        let app = copy("app", RoleOrigin::Api, Some(SCRAM_A));
        let mut nologin = copy("readers", RoleOrigin::Api, None);
        nologin.login = false;
        nologin.password_enc = None;
        let mut seen = role("app", Some(SCRAM_A));
        seen.member_of = vec!["readers".into(), "theirs".into()];
        seen.owner_granted = vec!["readers".into()];
        seen.inherit = false;
        seen.connection_limit = 4;
        seen.valid_until = Some(DateTime::from_timestamp(1_900_000_000, 0).unwrap());
        seen.bypass_rls = true;
        let mut readers = role("readers", None);
        readers.login = false;
        // LOGIN turned on in SQL (without a password): it sticks too.
        let mut turned_on = readers.clone();
        turned_on.login = true;
        for (readers_seen, login) in [(readers, false), (turned_on, true)] {
            let roles = vec![seen.clone(), readers_seen, role("theirs", None)];
            let (repairs, write) =
                plan_resync(OWNER_ROLE, &[app.clone(), nologin.clone()], &observed(roles));
            assert!(repairs.is_empty(), "{repairs:?}");
            let u = upsert(&write, "app").unwrap();
            assert_eq!(u.attributes, seen.attributes());
            assert_eq!((u.login, u.level, u.api_owner_level), (true, RoleLevel::Plain, Some(false)));
            let r = upsert(&write, "readers").unwrap();
            assert_eq!((r.login, r.verifier.clone()), (login, None));
        }
    }

    // E32 invariant: for every API row and catalog state, the resync drives the stored wanted
    // state only while the row's change is unconfirmed; otherwise it runs nothing but the
    // control plane's own repairs (existence, password, the owner's ADMIN), and records the
    // catalog. Exhaustive over pending × each attribute differing × owner level × login.
    #[test]
    fn plan_resync_drives_attributes_only_while_a_change_is_pending_exhaustively() {
        let mut checked = 0;
        let cases = (0..16_u8).map(|i| (i & 8 != 0, i & 4 != 0, i & 2 != 0, i & 1 != 0));
        for (pending, attributes_pending, missing, password_differs) in cases {
            for wanted_owner in [false, true] {
                for seen_owner in [false, true] {
                    for wanted_login in [false, true] {
                        for seen_login in [false, true] {
                            for differs in 0..6 {
                                let mut row = copy("r", RoleOrigin::Api, None);
                                row.login = wanted_login;
                                row.scram_verifier = wanted_login.then(|| SCRAM_A.to_string());
                                row.api_owner_level = wanted_owner;
                                // A password reset in flight (`pending`) is independent of an
                                // unconfirmed attribute change.
                                row.pending = pending;
                                row.attributes_pending = attributes_pending;
                                let seen_password = match (wanted_login, password_differs) {
                                    (_, true) => Some(SCRAM_B),
                                    (true, false) => Some(SCRAM_A),
                                    (false, false) => None,
                                };
                                let mut o = role("r", seen_password);
                                o.login = seen_login;
                                o.owner_attributes = seen_owner;
                                o.owner_level = seen_owner;
                                o.bypass_rls = seen_owner;
                                match differs {
                                    1 => o.inherit = false,
                                    2 => o.connection_limit = 9,
                                    3 => o.valid_until = Some(DateTime::from_timestamp(1, 0).unwrap()),
                                    4 => o.bypass_rls = !o.bypass_rls,
                                    5 => {
                                        o.member_of = vec!["g".into()];
                                        o.owner_granted = vec!["g".into()];
                                    }
                                    _ => {}
                                }
                                let mut roles = vec![role("g", None)];
                                if !missing {
                                    roles.push(o.clone());
                                }
                                let (repairs, write) =
                                    plan_resync(OWNER_ROLE, std::slice::from_ref(&row), &observed(roles));
                                let case = format!(
                                    "pending={pending} attributes_pending={attributes_pending} \
                                     missing={missing} {row:?} vs {o:?}: {repairs:?}"
                                );
                                let u = upsert(&write, "r").expect("written");
                                let statements = flat(&repairs);
                                if missing {
                                    // Existence is the control plane's: made again as stored.
                                    assert_eq!(statements.len(), 1, "{case}");
                                    assert!(statements[0].contains("CREATE ROLE \"r\""), "{case}");
                                    assert_eq!(u.attributes, row.attributes, "{case}");
                                    assert_eq!(u.login, wanted_login, "{case}");
                                    checked += 1;
                                    continue;
                                }
                                // The password is the control plane's: set back when it differs,
                                // and only then; `pending` alone drives nothing else.
                                let password = password_sql("r", row.scram_verifier.as_deref());
                                assert_eq!(
                                    statements.iter().filter(|s| **s == password).count(),
                                    usize::from(password_differs),
                                    "{case}"
                                );
                                let others: Vec<&String> =
                                    statements.iter().filter(|s| **s != password).collect();
                                if attributes_pending {
                                    assert_eq!(u.attributes, row.attributes, "{case}");
                                    assert_eq!(u.login, wanted_login, "{case}");
                                    assert_eq!(u.api_owner_level, None, "{case}");
                                    // Driving it again against the catalog it made changes nothing.
                                    let mut done = o.clone();
                                    done.login = wanted_login;
                                    done.inherit = row.attributes.inherit;
                                    done.connection_limit = row.attributes.connection_limit;
                                    done.valid_until = row.attributes.valid_until;
                                    done.owner_attributes = wanted_owner;
                                    done.owner_level = wanted_owner;
                                    done.bypass_rls = wanted_owner || row.attributes.bypass_rls;
                                    done.member_of = Vec::new();
                                    done.owner_granted = Vec::new();
                                    done.password = row.scram_verifier.clone();
                                    let (again, _) = plan_resync(
                                        OWNER_ROLE,
                                        std::slice::from_ref(&row),
                                        &observed(vec![done, role("g", None)]),
                                    );
                                    assert!(again.is_empty(), "converges: {case} then {again:?}");
                                } else {
                                    assert!(others.is_empty(), "nothing driven: {case}");
                                    assert_eq!(u.attributes, o.attributes(), "{case}");
                                    assert_eq!(u.login, seen_login, "{case}");
                                    assert_eq!(u.api_owner_level, Some(seen_owner), "{case}");
                                }
                                assert!(!u.pending, "{case}");
                                checked += 1;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 16 * 2 * 2 * 2 * 2 * 6);
    }

    // U1, E32: an API change the compute never confirmed is driven by the resync: a NOLOGIN
    // row's role loses the password and LOGIN it still has, gets the wanted attributes, its
    // wanted memberships granted and the owner's other grants revoked. A wanted group that
    // was dropped in SQL is dropped from what's wanted; a lost role is made again as it was.
    #[test]
    fn resync_drives_a_pending_api_change() {
        let wanted = RoleAttributes {
            inherit: false,
            connection_limit: 3,
            valid_until: Some(DateTime::from_timestamp(1_900_000_000, 0).unwrap()),
            bypass_rls: true,
            member_of: vec!["g".into(), "gone".into()],
            member_no_set: vec![],
            member_admin: vec![],
        };
        let api = |name: &str| {
            let mut c = copy(name, RoleOrigin::Api, None);
            c.login = false;
            c.password_enc = None;
            c.attributes = wanted.clone();
            c.pending = true;
            c.attributes_pending = true;
            c
        };
        let copies = [api("r"), api("lost")];
        let mut seen = role("r", Some(SCRAM_B));
        seen.member_of = vec!["extra".into(), "theirs".into()];
        seen.owner_granted = vec!["extra".into()];
        let roles = vec![
            seen,
            role("g", None),
            role("extra", None),
            role("theirs", None),
        ];

        let (repairs, write) = plan_resync(OWNER_ROLE, &copies, &observed(roles));

        let until = "'2030-03-17T17:46:40.000000Z'";
        let groups: Vec<&str> = repairs.iter().map(|g| g.role.as_str()).collect();
        assert_eq!(groups, ["r", "lost"]);
        assert_eq!(
            repairs[0].statements,
            [
                "ALTER ROLE \"r\" PASSWORD NULL;".to_string(),
                format!(
                    "ALTER ROLE \"r\" NOLOGIN NOINHERIT CONNECTION LIMIT 3 VALID UNTIL {until} \
                     BYPASSRLS;"
                ),
                "SET ROLE \"postgres\";\nGRANT \"g\" TO \"r\";\nREVOKE \"extra\" FROM \"r\";\n\
                 RESET ROLE;"
                    .to_string(),
            ]
        );
        assert_eq!(
            repairs[1].statements,
            [format!(
                "SET ROLE \"postgres\";\nCREATE ROLE \"lost\" NOLOGIN NOINHERIT CONNECTION LIMIT 3 \
                 VALID UNTIL {until} BYPASSRLS;\nGRANT \"g\" TO \"lost\";\nRESET ROLE;"
            )]
        );
        let kept = RoleAttributes {
            member_of: vec!["g".into()],
            ..wanted.clone()
        };
        for name in ["r", "lost"] {
            let u = upsert(&write, name).unwrap();
            assert_eq!((u.login, u.verifier, u.pending), (false, None, false), "{name}");
            assert_eq!(u.attributes, kept, "{name}");
        }

        // Once the catalog matches, nothing is repaired.
        let mut fixed = role("r", None);
        fixed.login = false;
        fixed.inherit = false;
        fixed.connection_limit = 3;
        fixed.valid_until = wanted.valid_until;
        fixed.bypass_rls = true;
        fixed.member_of = vec!["g".into()];
        fixed.owner_granted = vec!["g".into()];
        let copies = [api("r")];
        let (repairs, _) = plan_resync(
            OWNER_ROLE,
            &copies,
            &observed(vec![fixed, role("g", None)]),
        );
        assert!(repairs.is_empty(), "{repairs:?}");
    }

    // A role made in SQL: the catalog's attributes and memberships are recorded.
    #[test]
    fn plan_resync_records_sql_role_attributes() {
        let mut seen = role("reporter", Some(SCRAM_A));
        seen.inherit = false;
        seen.connection_limit = 7;
        seen.bypass_rls = true;
        seen.member_of = vec!["g".into(), "h".into()];
        seen.member_no_set = vec!["h".into()];
        seen.member_admin = vec!["g".into()];
        let (repairs, write) = plan_resync(OWNER_ROLE, &[], &observed(vec![seen]));
        assert!(repairs.is_empty());
        let u = upsert(&write, "reporter").unwrap();
        assert_eq!(
            u.attributes,
            RoleAttributes {
                inherit: false,
                connection_limit: 7,
                valid_until: None,
                bypass_rls: true,
                member_of: vec!["g".into(), "h".into()],
                member_no_set: vec!["h".into()],
                member_admin: vec!["g".into()],
            }
        );
    }

    // The new catalog columns round-trip through the read's parser.
    #[test]
    fn parse_observed_reads_attributes_and_memberships() {
        let mut full = role("r", None);
        full.login = false;
        full.inherit = false;
        full.connection_limit = 3;
        full.valid_until = Some(DateTime::from_timestamp_micros(1_900_000_000_123_456).unwrap());
        full.bypass_rls = true;
        full.member_of = vec!["a,b".into(), "g".into()];
        full.owner_granted = vec!["g".into()];
        // I2: the SET and ADMIN options of a membership travel with it.
        full.member_no_set = vec!["a,b".into()];
        full.member_admin = vec!["a,b".into(), "g".into()];
        let o = parse_observed(&role_line(&full)).unwrap();
        assert_eq!(o.roles, [full]);
        let sql = role_read_sql("postgres", "a'b");
        assert!(sql.contains("WHERE a.rolname = 'a''b'"), "{sql}");
        assert!(!sql.contains("UNION ALL"), "{sql}");
    }

    // C1: a role's repairs (password, attributes, ADMIN) share one group.
    #[test]
    fn plan_resync_groups_a_roles_repairs() {
        let mut boss = copy("boss", RoleOrigin::Api, Some(SCRAM_A));
        boss.api_owner_level = true;
        boss.pending = true;
        boss.attributes_pending = true;
        let copies = [boss, copy("plain", RoleOrigin::Api, Some(SCRAM_A))];
        let mut seen_boss = role("boss", Some(SCRAM_B));
        seen_boss.owner_admin = false;
        let roles = vec![seen_boss, role("plain", Some(SCRAM_B))];

        let (repairs, _) = plan_resync(OWNER_ROLE, &copies, &observed(roles));

        let shape: Vec<(&str, usize)> = repairs
            .iter()
            .map(|g| (g.role.as_str(), g.statements.len()))
            .collect();
        assert_eq!(shape, [("boss", 3), ("plain", 1)]);
    }

    /// Main of a ready database: `(branch, endpoint)`; the journal starts empty.
    async fn ready(w: &FakeWorker, journal: &Journal) -> (BranchId, String) {
        let id = ready_database(w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.unwrap();
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        (main.id, ep)
    }

    fn count(journal: &Journal, entry: &str) -> usize {
        entries(journal).iter().filter(|e| *e == entry).count()
    }

    // T8 I3: a resync that finds roles changed in SQL (made, dropped, renamed, their
    // memberships or attributes) asks for a member-role reconcile; one that changes nothing, or
    // only the databases (R17), doesn't.
    #[tokio::test]
    async fn a_role_change_asks_for_a_member_reconcile() {
        let (w, journal) = worker();
        let (branch, ep) = ready(&w, &journal).await;
        let main_db = w.metadata.state().db_rows[&branch]
            .iter()
            .find(|r| r.main)
            .unwrap()
            .name
            .clone();
        let db = d_line(&main_db, OWNER_ROLE, false);
        w.computes.answer("FROM pg_authid", &db);
        w.resync(branch).await.unwrap();
        let base = w.member_role_requests.sent().len();
        w.resync(branch).await.unwrap();
        assert_eq!(w.member_role_requests.sent().len(), base, "nothing changed");
        w.computes.answer_once_at(
            &ep,
            "FROM pg_authid",
            &format!("{db}{}", d_line("extra", OWNER_ROLE, false)),
        );
        w.resync(branch).await.unwrap();
        assert_eq!(w.member_role_requests.sent().len(), base, "a database alone (R17)");
        w.computes.answer_once_at(
            &ep,
            "FROM pg_authid",
            &format!("{}{db}", r_line("reporter", true, Some(SCRAM_B), false, true, false)),
        );
        w.resync(branch).await.unwrap();
        assert_eq!(w.member_role_requests.sent().len(), base + 1, "a role made in SQL");
        w.resync(branch).await.unwrap();
        assert_eq!(w.member_role_requests.sent().len(), base + 2, "and dropped");
        assert!(w.member_role_requests.sent().iter().all(|b| *b == branch));
    }

    #[tokio::test]
    async fn resync_retries_when_the_version_moves() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;

        w.metadata.state().resync_version_moves = 2;
        w.resync(branch).await.unwrap();
        assert_eq!(count(&journal, "metadata.apply_resync"), 3);

        journal.lock().unwrap().clear();
        w.metadata.state().resync_version_moves = 4;
        let err = w.resync(branch).await.unwrap_err();
        assert!(err.starts_with("syncing login roles: "), "{err}");
        assert_eq!(count(&journal, "metadata.apply_resync"), 3);

        // Every try re-reads the copies and the catalog.
        assert_eq!(
            entries(&journal)
                .iter()
                .filter(|e| e.starts_with("metadata.role_copies"))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn resync_failures_carry_the_prefix() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;
        w.computes.answer("FROM pg_authid", "garbage\n");
        let err = w.resync(branch).await.unwrap_err();
        assert!(err.starts_with("syncing login roles: "), "{err}");
        assert!(position(&journal, "metadata.apply_resync").is_none());

        w.computes.answers.lock().unwrap().clear();
        *w.computes.sql_failures_left.lock().unwrap() = 1;
        let err = w.resync(branch).await.unwrap_err();
        assert!(err.starts_with("syncing login roles: "), "{err}");

        assert!(
            w.resync(BranchId::new())
                .await
                .unwrap_err()
                .starts_with("syncing login roles: ")
        );
    }

    /// An API-made row of `ep` with `verifier`, still pending.
    fn api_row(w: &FakeWorker, ep: &str, name: &str, verifier: &str) {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(ep).unwrap();
        endpoint.set_verifier(name, Some(verifier));
        endpoint.logins.insert(
            name.into(),
            FakeLogin {
                origin: RoleOrigin::Api,
                password_enc: Some("sealed".into()),
                pending: true,
                login: true,
                level: RoleLevel::Plain,
                api_owner_level: false,
                attributes: RoleAttributes::default(),
                attributes_pending: false,
                attributes_change_id: None,
            },
        );
    }

    // C1: each role's repairs run as their own script; one that fails is logged and its
    // role's write skipped, the others still run and are written, and the resync fails
    // afterwards naming the failed role.
    #[tokio::test(start_paused = true)]
    async fn a_failing_repair_does_not_stop_the_others() {
        let (w, journal) = worker();
        let (branch, ep) = ready(&w, &journal).await;
        api_row(&w, &ep, "aaa", SCRAM_A);
        api_row(&w, &ep, "bbb", SCRAM_B);
        // The catalog lost both: each is re-created, and `aaa`'s create fails.
        w.computes
            .answer("FROM pg_authid", &d_line("app", "postgres", false));
        w.computes.fail_when("CREATE ROLE \"aaa\"");

        let err = w.resync(branch).await.unwrap_err();

        assert!(err.starts_with("syncing login roles: "), "{err}");
        assert!(err.contains("aaa"), "{err}");
        assert!(!err.contains("bbb"), "{err}");
        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            sql.iter()
                .any(|(_, _, s)| s.contains("CREATE ROLE \"bbb\"")),
            "{sql:?}"
        );
        assert!(position(&journal, "metadata.apply_resync").is_some());
        let state = w.metadata.state();
        let endpoint = &state.endpoints[&ep];
        assert!(!endpoint.logins["bbb"].pending, "bbb's row is written");
        assert!(endpoint.logins["aaa"].pending, "aaa's write is skipped");
        assert_eq!(endpoint.branch_databases, observed(vec![]).databases);
    }

    // A role whose repair failed isn't published: its catalog password is not the stored one,
    // and the proxy must not be told it changed. The role that was repaired is.
    #[tokio::test(start_paused = true)]
    async fn a_role_whose_repair_failed_is_not_published() {
        let (w, journal) = worker();
        let (branch, ep) = ready(&w, &journal).await;
        api_row(&w, &ep, "aaa", SCRAM_A);
        api_row(&w, &ep, "bbb", SCRAM_B);
        // Both were set back in SQL: the catalog holds the other role's verifier.
        w.computes.answer(
            "FROM pg_authid",
            &format!(
                "{}{}{}",
                r_line("aaa", true, Some(SCRAM_B), false, true, false),
                r_line("bbb", true, Some(SCRAM_A), false, true, false),
                d_line("app", "postgres", false),
            ),
        );
        w.computes.fail_when("ALTER ROLE \"aaa\" PASSWORD");

        let err = w.resync(branch).await.unwrap_err();

        assert!(err.contains("aaa") && !err.contains("bbb"), "{err}");
        assert_eq!(count(&journal, &format!("cache.password_updated {ep} aaa")), 0);
        assert_eq!(count(&journal, &format!("cache.password_updated {ep} bbb")), 1);
    }

    // C1, the fresh passwords: one role's failed script doesn't stop the others'; the failed
    // role isn't written (so it keeps no fresh password it doesn't have), the resync fails
    // naming it.
    #[tokio::test(start_paused = true)]
    async fn failed_fresh_password_group_is_isolated() {
        let (w, journal) = worker();
        let (branch, ep) = ready(&w, &journal).await;
        w.computes.answer(
            "FROM pg_authid",
            &format!(
                "{}{}",
                r_line("aaa", true, Some(SCRAM_A), false, true, false),
                r_line("bbb", true, Some(SCRAM_B), false, true, false),
            ),
        );
        w.computes.fail_when("ALTER ROLE \"aaa\" PASSWORD");
        let mut fresh = FreshPasswords::new([]);

        let err = w
            .resync_locked_with(branch, &ep, SYSTEM_DATABASE, Some(&mut fresh))
            .await
            .unwrap_err();

        assert!(err.starts_with("syncing login roles: "), "{err}");
        assert!(err.contains("aaa"), "{err}");
        assert!(!err.contains("bbb"), "{err}");
        let state = w.metadata.state();
        let endpoint = &state.endpoints[&ep];
        let bbb = endpoint.verifier("bbb").expect("bbb is written");
        assert_ne!(bbb, SCRAM_B, "bbb has its fresh verifier");
        assert!(endpoint.logins["bbb"].password_enc.is_some());
        assert!(!endpoint.logins.contains_key("aaa"), "aaa isn't written");
    }

    #[tokio::test(start_paused = true)]
    async fn resync_publishes_changed_names() {
        let (w, journal) = worker();
        let (branch, ep) = ready(&w, &journal).await;
        // `app` made through the API; the catalog lost it. `reporter` made in SQL.
        {
            let mut state = w.metadata.state();
            let endpoint = state.endpoints.get_mut(&ep).unwrap();
            endpoint.set_verifier("app", Some(SCRAM_A));
            endpoint.logins.insert(
                "app".into(),
                FakeLogin {
                    origin: RoleOrigin::Api,
                    password_enc: Some("sealed".into()),
                    pending: true,
                    login: true,
                    level: RoleLevel::Plain,
                    api_owner_level: false,
                    attributes: RoleAttributes::default(),
                    attributes_pending: false,
                    attributes_change_id: None,
                },
            );
            endpoint.set_verifier("quiet", Some(SCRAM_B));
            endpoint.logins.insert(
                "quiet".into(),
                FakeLogin {
                    origin: RoleOrigin::Sql,
                    password_enc: None,
                    pending: false,
                    login: true,
                    level: RoleLevel::Plain,
                    api_owner_level: false,
                    attributes: RoleAttributes::default(),
                    attributes_pending: false,
                    attributes_change_id: None,
                },
            );
        }
        w.computes.answer(
            "FROM pg_authid",
            &format!(
                "{}{}{}",
                r_line("quiet", true, Some(SCRAM_B), false, true, false),
                r_line("reporter", true, Some(SCRAM_B), false, true, false),
                d_line("app", "postgres", false)
            ),
        );

        w.resync(branch).await.unwrap();

        let sql: Vec<(String, String)> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .map(|(_, db, s)| (db.clone(), s.clone()))
            .collect();
        assert_eq!(sql.len(), 2, "{sql:?}");
        assert_eq!(sql[0].0, SYSTEM_DATABASE);
        assert!(sql[0].1.contains("FROM pg_authid"));
        assert!(
            sql[1].1.starts_with("SET search_path = pg_catalog, pg_temp;\nSET ROLE \"postgres\";\nCREATE ROLE \"app\""),
            "{sql:?}"
        );
        let order = [
            position(&journal, "metadata.lock_endpoint").unwrap(),
            position(&journal, "metadata.role_copies").unwrap(),
            position(&journal, "metadata.apply_resync").unwrap(),
            position(&journal, &format!("cache.password_updated {ep} app")).unwrap(),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        {
            let state = w.metadata.state();
            let endpoint = &state.endpoints[&ep];
            assert!(!endpoint.logins["app"].pending);
            assert_eq!(endpoint.logins["reporter"].origin, RoleOrigin::Sql);
            assert_eq!(endpoint.verifier("reporter").as_deref(), Some(SCRAM_B));
            assert_eq!(endpoint.branch_databases, observed(vec![]).databases);
        }
        for name in ["app", "reporter"] {
            assert_eq!(
                count(&journal, &format!("cache.password_updated {ep} {name}")),
                1
            );
        }
        assert_eq!(
            count(&journal, &format!("cache.password_updated {ep} quiet")),
            0
        );

        tokio::time::sleep(REPUBLISH_DELAY * 2).await;
        for name in ["app", "reporter"] {
            assert_eq!(
                count(&journal, &format!("cache.password_updated {ep} {name}")),
                2
            );
        }

        // Nothing changed the second time: nothing is published or repaired.
        journal.lock().unwrap().clear();
        w.computes.answers.lock().unwrap().clear();
        w.computes.answer(
            "FROM pg_authid",
            &format!(
                "{}{}{}{}",
                r_line("app", true, Some(SCRAM_A), false, true, false),
                r_line("quiet", true, Some(SCRAM_B), false, true, false),
                r_line("reporter", true, Some(SCRAM_B), false, true, false),
                d_line("app", "postgres", false),
            ),
        );
        w.computes.sql.lock().unwrap().clear();
        w.resync(branch).await.unwrap();
        assert_eq!(w.computes.sql.lock().unwrap().len(), 1);
        assert!(
            position(&journal, "cache.password_updated").is_none(),
            "{:?}",
            entries(&journal)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn touched_resyncs_coalesce_into_one_run_after_the_delay() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;
        let pending = PendingResyncs::default();
        let burst = async {
            tokio::join!(
                w.resync_touched(&pending, branch, TOUCHED_RESYNC_DELAY),
                w.resync_touched(&pending, branch, TOUCHED_RESYNC_DELAY),
                w.resync_touched(&pending, branch, TOUCHED_RESYNC_DELAY),
            )
        };
        let observer = async {
            tokio::time::sleep(TOUCHED_RESYNC_DELAY - Duration::from_millis(100)).await;
            assert_eq!(count(&journal, "metadata.apply_resync"), 0, "still waiting");
        };
        let (results, ()) = tokio::join!(burst, observer);
        assert!(results.0.is_ok() && results.1.is_ok() && results.2.is_ok());
        assert_eq!(
            count(&journal, "metadata.apply_resync"),
            1,
            "{:?}",
            entries(&journal)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_touch_after_the_run_schedules_another() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;
        let pending = PendingResyncs::default();
        w.resync_touched(&pending, branch, TOUCHED_RESYNC_DELAY)
            .await
            .unwrap();
        w.resync_touched(&pending, branch, TOUCHED_RESYNC_DELAY)
            .await
            .unwrap();
        assert_eq!(count(&journal, "metadata.apply_resync"), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn touched_resyncs_of_different_branches_do_not_coalesce() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;
        let other = BranchId::new();
        let pending = PendingResyncs::default();
        let (a, b) = tokio::join!(
            w.resync_touched(&pending, branch, TOUCHED_RESYNC_DELAY),
            w.resync_touched(&pending, other, TOUCHED_RESYNC_DELAY),
        );
        assert!(a.is_ok());
        // The unknown branch fails on its own and is not swallowed by the first one's claim.
        assert!(b.unwrap_err().starts_with("syncing login roles: "));
        assert_eq!(count(&journal, "metadata.apply_resync"), 1);
    }
}
