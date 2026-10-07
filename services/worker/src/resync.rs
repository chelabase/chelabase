//! The resync (spec 2026-10-01-login-roles, Part 3): the branch's catalog is the source of
//! truth for roles made in SQL ("the database wins"), the metadata copy for roles made through
//! the API ("the control plane wins" for existence and password).

use std::collections::BTreeMap;
use std::time::Duration;

use chelabase_metadata::{
    CopyUpsert, DataKeyRecord, KeyRoleFix, ObservedDatabase, ResyncApplied, ResyncWrite,
    RoleCopy, RoleLevel, RoleOrigin, SealedChange,
};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::seal_role_password;
use chelabase_types::BranchId;
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::role_name::{quote_ident, quote_literal};
use chelabase_types::role_password::{PasswordKind, hash_kind};
use tracing::{error, info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{OWNER_ROLE, Worker, credentials};

/// The database [`Worker::resync`] reads the catalog through: the cluster's own `postgres`
/// database, which the owner can't drop (the branch's main database can be, Q1). Roles and
/// `pg_database` are cluster-wide, so any database sees the same catalog.
pub const SYSTEM_DATABASE: &str = "postgres";

/// Every role's OID and hex-encoded name, for the secret keys' identities. The comment is the
/// fakes' needle.
pub const KEY_ROLES_SQL: &str = "SET search_path = pg_catalog, pg_temp;\n\
     SELECT /* chelabase: key roles */ oid::text || ' ' || encode(convert_to(rolname, 'UTF8'), 'hex') \
     FROM pg_roles;";

/// [`KEY_ROLES_SQL`]'s lines: `(oid, name)`.
pub fn parse_key_roles(out: &str) -> anyhow::Result<Vec<(i64, String)>> {
    out.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (oid, name) = line
                .trim()
                .split_once(' ')
                .ok_or_else(|| anyhow::anyhow!("a role line without a name: {line:?}"))?;
            let oid = oid
                .parse::<i64>()
                .map_err(|e| anyhow::anyhow!("a role line with a bad OID {oid:?}: {e}"))?;
            let name = unhex(name).ok_or_else(|| anyhow::anyhow!("a role line with a bad name"))?;
            Ok((oid, name))
        })
        .collect()
}

/// The repairs that make the live secret keys agree with the compute's roles, by identity:
/// a key whose OID is gone is revoked (its role was dropped, maybe recreated under the name);
/// a key whose OID now has another name follows it; a key without an OID yet takes its name's
/// current OID, or is revoked when the name is gone. Public keys are left alone.
pub fn plan_key_roles(keys: &[DataKeyRecord], catalog: &[(i64, String)]) -> Vec<KeyRoleFix> {
    let name_of = |oid: i64| catalog.iter().find(|(o, _)| *o == oid).map(|(_, n)| n);
    let oid_of = |name: &str| catalog.iter().find(|(_, n)| n == name).map(|(o, _)| *o);
    keys.iter()
        .filter(|k| k.kind == DataKeyKind::Secret && k.revoked_at.is_none())
        .filter_map(|k| match k.role_oid {
            Some(oid) => match name_of(oid) {
                None => Some(KeyRoleFix::Revoke { id: k.id, role: k.role.clone(), oid: Some(oid) }),
                Some(name) if *name != k.role => Some(KeyRoleFix::Follow {
                    id: k.id,
                    from: k.role.clone(),
                    to: name.clone(),
                    oid,
                }),
                Some(_) => None,
            },
            None => Some(match oid_of(&k.role) {
                Some(oid) => KeyRoleFix::Backfill { id: k.id, role: k.role.clone(), oid },
                None => KeyRoleFix::Revoke { id: k.id, role: k.role.clone(), oid: None },
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
    waiting: std::sync::Mutex<std::collections::HashSet<BranchId>>,
}

impl PendingResyncs {
    /// True when the caller now owns the branch's pending run; false when one is waiting.
    fn claim(&self, branch: BranchId) -> bool {
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(branch)
    }

    fn release(&self, branch: BranchId) {
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&branch);
    }
}

/// How many times the resync reads and writes before giving up on a moving `roles_version`.
const RESYNC_TRIES: usize = 3;

/// One role of the catalog (owner and system roles excluded).
#[derive(Clone, PartialEq, Eq)]
pub struct ObservedRole {
    pub name: String,
    pub login: bool,
    /// `rolpassword`: a SCRAM verifier, an MD5 hash, or `None`.
    pub password: Option<String>,
    pub owner_level: bool,
    /// The owner holds ADMIN on the role.
    pub owner_admin: bool,
    /// The role has the owner's attributes (`CREATEROLE CREATEDB BYPASSRLS REPLICATION`) and a
    /// direct `neon_superuser` membership: what an API owner-level role is made with.
    pub owner_attributes: bool,
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
            .finish()
    }
}

/// What the catalog read found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observed {
    pub roles: Vec<ObservedRole>,
    pub databases: Vec<ObservedDatabase>,
}

/// The catalog read, as planned (plan Task 1); `$OWNER` is the owner's name.
const RESYNC_SQL: &str = "SET search_path = pg_catalog, pg_temp;
SELECT 'r',
       encode(convert_to(a.rolname, 'UTF8'), 'hex'),
       a.rolcanlogin,
       coalesce(encode(convert_to(a.rolpassword, 'UTF8'), 'hex'), ''),
       (a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolbypassrls OR a.rolreplication
        OR pg_has_role(a.oid, 'neon_superuser', 'MEMBER')),
       EXISTS (SELECT 1 FROM pg_auth_members m
               WHERE m.roleid = a.oid AND m.admin_option
                 AND m.member = (SELECT oid FROM pg_roles WHERE rolname = $OWNER)),
       (a.rolcreaterole AND a.rolcreatedb AND a.rolbypassrls AND a.rolreplication
        AND EXISTS (SELECT 1 FROM pg_auth_members s
                    WHERE s.member = a.oid AND s.roleid = 'neon_superuser'::regrole))
FROM pg_authid a
WHERE a.oid >= 16384
  AND a.rolname <> $OWNER
  AND a.rolname NOT IN ('cloud_admin', 'neon_superuser', 'anon', 'authenticated',
                        'authenticator', 'chelabase_admin', 'chelabase_auth_sync')
  AND a.rolname !~ '^pg_'
UNION ALL
SELECT 'd',
       encode(convert_to(d.datname, 'UTF8'), 'hex'),
       NULL,
       encode(convert_to(pg_get_userbyid(d.datdba), 'UTF8'), 'hex'),
       d.datdba < 16384,
       NULL,
       NULL
FROM pg_database d
WHERE NOT d.datistemplate AND d.datconnlimit <> -2
ORDER BY 1, 2;";

/// The catalog read (plan Task 1, plus Task 12's owner-attributes column), with `$OWNER` as
/// the owner's quoted literal.
pub fn resync_sql(owner: &str) -> String {
    RESYNC_SQL.replace("$OWNER", &quote_literal(owner))
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
        let [kind, name, a, b, c, d, e] = fields[..] else {
            return Err(bad("doesn't have seven fields"));
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
                observed.roles.push(ObservedRole {
                    name,
                    login,
                    password,
                    owner_level,
                    owner_admin,
                    owner_attributes,
                });
            }
            "d" => {
                if !a.is_empty() || !d.is_empty() || !e.is_empty() {
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

    // API rows: the control plane wins for existence and password.
    for row in copies
        .iter()
        .filter(is_copy)
        .filter(|c| c.origin == RoleOrigin::Api)
    {
        let Some(stored) = row.scram_verifier.as_deref() else {
            error!(role = %row.name, "an API-made role has no stored verifier; the resync skips it");
            continue;
        };
        let (login, row_level) = match observed.roles.iter().find(|o| o.name == row.name) {
            None => {
                push_repair(
                    &mut repairs,
                    &row.name,
                    create_role_sql(owner, &row.name, stored, row.api_owner_level),
                );
                (true, level(row.api_owner_level))
            }
            Some(o) => {
                if o.password.as_deref() != Some(stored) {
                    push_repair(&mut repairs, &row.name, set_password_sql(&row.name, stored));
                }
                // N1: an owner-level role has the owner's attributes and neon_superuser; a
                // role that lost any of them (or whose create's GRANT failed) gets them back.
                let restore = row.api_owner_level && !o.owner_attributes;
                if restore {
                    push_repair(&mut repairs, &row.name, owner_attributes_sql(&row.name));
                }
                if !o.owner_admin {
                    let grant = format!(
                        "GRANT {} TO {} WITH ADMIN OPTION, INHERIT FALSE, SET FALSE;",
                        quote_ident(&row.name),
                        quote_ident(owner)
                    );
                    push_repair(&mut repairs, &row.name, grant);
                }
                (o.login, level(o.owner_level || restore))
            }
        };
        write.upserts.push(CopyUpsert {
            name: row.name.clone(),
            origin: RoleOrigin::Api,
            verifier: Some(stored.to_string()),
            password_enc: SealedChange::Keep,
            login,
            level: row_level,
            pending: false,
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
            });
            continue;
        }
        let verifier = match o.password.as_deref().map(|p| (hash_kind(p), p)) {
            Some((PasswordKind::Scram, p)) => Some(p.to_string()),
            // R9: kept, so Studio can flag it; the proxy treats a NULL verifier as unknown.
            Some((PasswordKind::Md5, _)) => None,
            // No password (or none the proxy could ever use): no copy.
            Some((PasswordKind::Other, _)) | None => {
                if row.is_some() {
                    write.removals.push(o.name.clone());
                }
                continue;
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

    /// Makes the branch's live secret keys agree with the compute's roles by OID
    /// ([`plan_key_roles`]): revokes dead keys, follows renames the DDL receiver didn't see and
    /// backfills keys made before OIDs were stored. Only when the branch has live secret keys.
    /// A failure is logged: keys are still held to their OID by rest on every request, so this
    /// is tidying, and the next resync tries again. An empty catalog (never real: `pg_roles`
    /// always has the bootstrap roles) is taken as unreadable, never as "every role is gone".
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
        let catalog = match self.computes.admin_sql(ep, database, KEY_ROLES_SQL).await {
            Ok(out) => match parse_key_roles(&out) {
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
        let fixes = plan_key_roles(&keys, &catalog);
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

/// `SET ROLE <owner>; CREATE ROLE … LOGIN PASSWORD …; RESET ROLE` (the owner gets ADMIN on
/// it through `createrole_self_grant`), then `GRANT neon_superuser` as `cloud_admin` for an
/// owner-level role, which is also made with the owner's attributes (the owner holds them, so
/// it may give them).
pub(crate) fn create_role_sql(
    owner: &str,
    name: &str,
    verifier: &str,
    owner_level: bool,
) -> String {
    let ident = quote_ident(name);
    let attributes = if owner_level {
        format!(" {OWNER_ATTRIBUTES}")
    } else {
        String::new()
    };
    let mut sql = format!(
        "SET ROLE {};\nCREATE ROLE {ident} LOGIN{attributes} PASSWORD {};\nRESET ROLE;",
        quote_ident(owner),
        quote_literal(verifier)
    );
    if owner_level {
        sql.push_str(&format!("\nGRANT neon_superuser TO {ident};"));
    }
    sql
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
            created_at: DateTime::<Utc>::UNIX_EPOCH,
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

    fn key(role: &str, oid: Option<i64>) -> DataKeyRecord {
        DataKeyRecord {
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
            plan_key_roles(&keys, &catalog),
            vec![
                KeyRoleFix::Revoke { id: gone.id, role: "dropped".into(), oid: Some(16385) },
                KeyRoleFix::Follow { id: moved.id, from: "old_name".into(), to: "renamed".into(), oid: 16390 },
                KeyRoleFix::Revoke { id: recreated.id, role: "again".into(), oid: Some(16399) },
                KeyRoleFix::Backfill { id: legacy.id, role: "app".into(), oid: 16384 },
                KeyRoleFix::Revoke { id: legacy_gone.id, role: "ghost".into(), oid: None },
            ]
        );
        assert!(plan_key_roles(&[alive], &catalog).is_empty(), "a key whose OID has its name");
        // Public keys are never touched.
        let mut public = key("anon", None);
        public.kind = DataKeyKind::Public;
        assert!(plan_key_roles(&[public], &[]).is_empty());
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

    // The resync applies the plan after the roles: a key whose OID is gone is revoked.
    #[tokio::test]
    async fn resync_revokes_a_key_whose_role_oid_is_gone() {
        let (w, journal) = worker();
        let (branch, _ep) = ready(&w, &journal).await;
        let alive = key("app", Some(16384));
        let gone = key("app", Some(16385));
        for k in [&alive, &gone] {
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
            });
        }
        w.computes.answer("chelabase: key roles", &format!("16384 {}\n", hex::encode("app")));
        w.resync(branch).await.unwrap();
        let fixes = w.metadata.state().key_fixes.clone();
        assert_eq!(
            fixes,
            vec![(branch, vec![KeyRoleFix::Revoke { id: gone.id, role: "app".into(), oid: Some(16385) }])]
        );
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
            "r|{}|{}|{}|{}|{}|{}\n",
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
            "d|{}||{}|{}||\n",
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
                },
                ObservedRole {
                    name: "nopass".into(),
                    login: false,
                    password: None,
                    owner_level: true,
                    owner_admin: false,
                    owner_attributes: true,
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
            "x|00|t||f|f|f",
            "r|zz|t||f|f|f",
            "r|61|t||f|f",
            "r|61|maybe||f|f|f",
            "r||t||f|f|f",
            "r|61|t|zz|f|f|f",
            "r|61|t||f|f|maybe",
            "d|61|t|62|f||",
            "d|61||62|f|t|",
            "d|61||62|f||t",
            "r|61|t||f|f|f|extra",
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
        let mut removals = write.removals.clone();
        removals.sort();
        assert_eq!(removals, ["gone", "lost_password"]);
        assert!(upsert(&write, "no_password").is_none());
        assert!(upsert(&write, OWNER_ROLE).is_none());
        assert_eq!(write.upserts.len(), 6 + 60);
        assert_eq!(write.databases, observed(vec![]).databases);
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

    #[test]
    fn plan_resync_api_rows() {
        let mut admin = copy("admin", RoleOrigin::Api, Some(SCRAM_A));
        admin.api_owner_level = true;
        admin.level = RoleLevel::OwnerLevel;
        admin.pending = true;
        let mut no_verifier = copy("broken", RoleOrigin::Api, None);
        no_verifier.pending = true;
        let owner_level_copy = |name: &str| {
            let mut c = copy(name, RoleOrigin::Api, Some(SCRAM_A));
            c.api_owner_level = true;
            c.level = RoleLevel::OwnerLevel;
            c
        };
        let copies = [
            copy(OWNER_ROLE, RoleOrigin::Owner, Some(SCRAM_A)),
            admin,
            owner_level_copy("boss_ok"),
            owner_level_copy("boss_stripped"),
            owner_level_copy("boss_partial"),
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
        let restore = |name: &str| {
            format!(
                "ALTER ROLE \"{name}\" CREATEROLE CREATEDB BYPASSRLS REPLICATION;\n\
                 GRANT neon_superuser TO \"{name}\";"
            )
        };
        let roles: Vec<&str> = repairs.iter().map(|g| g.role.as_str()).collect();
        assert_eq!(
            roles,
            [
                "admin",
                "boss_stripped",
                "boss_partial",
                "plain_gone",
                "drifted",
                "cleared",
                "no_admin"
            ]
        );
        assert_eq!(
            flat(&repairs),
            [
                format!(
                    "SET ROLE \"postgres\";\nCREATE ROLE \"admin\" LOGIN CREATEROLE CREATEDB \
                     BYPASSRLS REPLICATION PASSWORD {lit};\n\
                     RESET ROLE;\nGRANT neon_superuser TO \"admin\";"
                ),
                restore("boss_stripped"),
                restore("boss_partial"),
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
        let admin = upsert(&write, "admin").unwrap();
        assert_eq!((admin.login, admin.level), (true, RoleLevel::OwnerLevel));
        assert_eq!(
            upsert(&write, "plain_gone").unwrap().level,
            RoleLevel::Plain
        );
        assert_eq!(upsert(&write, "fine").unwrap().level, RoleLevel::OwnerLevel);
        // Repaired: owner-level again, whatever the catalog said before the repair.
        for name in ["boss_ok", "boss_stripped", "boss_partial"] {
            assert_eq!(
                upsert(&write, name).unwrap().level,
                RoleLevel::OwnerLevel,
                "{name}"
            );
        }
        assert!(upsert(&write, "broken").is_none());
        assert!(upsert(&write, OWNER_ROLE).is_none());
        assert!(write.removals.is_empty(), "{:?}", write.removals);
    }

    // C1: a role's repairs (password, attributes, ADMIN) share one group.
    #[test]
    fn plan_resync_groups_a_roles_repairs() {
        let mut boss = copy("boss", RoleOrigin::Api, Some(SCRAM_A));
        boss.api_owner_level = true;
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
