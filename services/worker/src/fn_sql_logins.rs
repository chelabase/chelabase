//! Pooled endpoints m13: a function's own SQL login on a branch.
//!
//! **Invariant.** For each (branch, function) whose run-as role is live on the branch (an
//! approved OID and a live run-as key, so the role was checked there) and whose `sql_access` is
//! on, the worker keeps the login [`function_login_name`] (`cb_fn_<12 hex of the function id>`)
//! with exactly the run-as role's rights:
//!
//! - `LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS`,
//!   `CONNECTION LIMIT` [`CONNECTION_LIMIT`], no expiry, the verifier of the branch's own
//!   password (sealed `Purpose::FunctionsSql`, AAD branch + function), and exactly one setting,
//!   the marker [`PERSONAL_LOGIN_MARKER`]` = `[`MARKER_VALUE`];
//! - `pg_auth_members` holds exactly one row with the login as `member` (the run-as role, by
//!   its approved OID: `inherit_option` true, `set_option` and `admin_option` false) and none
//!   with it as `roleid`.
//!
//! [`login_sql`] makes or repairs the login and checks all of that by OID in the same
//! transaction, so a run-as role renamed, dropped or recreated (a namesake with another OID)
//! meanwhile rolls the whole change back. A role under the login's name that the worker
//! can't adopt ([`inspect_sql`]: unmarked, any member, any membership but the approved one,
//! a per-database setting) is dropped first ([`drop_sqls`]) and made again. Owner-level roles
//! can never be run-as (`data_api::check_and_grant`, rerun here before each change), and
//! BYPASSRLS isn't inherited, so the login never gets either. Only after the compute holds the
//! login does the worker record its proxy row (`MetadataClient::record_function_sql_login`),
//! which the proxy answers only while the state row still matches.
//!
//! The name is reserved (`role_name::has_reserved_prefix`), so no API role, grant, key or
//! run-as role ever uses it. Removal, and the branch-copy and PITR sweeps, are m13b's.

use chelabase_metadata::{FunctionSqlLogin, MetadataClient};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::{open_function_sql_password, seal_function_sql_password};
use chelabase_types::BranchId;
use chelabase_types::role_name::{
    PERSONAL_LOGIN_MARKER, function_login_name, quote_ident, quote_literal,
};
use tracing::{info, warn};

use crate::Worker;
use crate::data_api::RoleCheck;
use crate::fn_triggers::Sweep;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// A function login's connection limit (per function, per branch).
pub const CONNECTION_LIMIT: i32 = 10;

/// The marker's value on a function login (a personal login's is `personal`).
pub const MARKER_VALUE: &str = "function";

/// The settings every script starts with: no tenant search path, event trigger or default
/// can change what it does, and it waits at most 2 s for a lock.
const PRELUDE: &str = "SET standard_conforming_strings = on;\nSET search_path = pg_catalog, pg_temp;\n\
SET lock_timeout = '2s';\nSET statement_timeout = '30s';\n";

/// What [`inspect_sql`] found under the login's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// No role by that name.
    Absent,
    /// A role the worker may adopt (or already made): marked, no member, no per-database
    /// setting, and no membership but the approved one.
    Adoptable,
    /// Anything else: dropped and made again.
    Foreign,
}

/// The SQL predicate (over `pg_roles l`) that the login holds exactly the approved run-as
/// role (`oid`) and nothing else, and has exactly the attributes and setting it is given.
fn exact_predicate(oid: i64) -> String {
    let marker = quote_literal(&format!("{PERSONAL_LOGIN_MARKER}={MARKER_VALUE}"));
    format!(
        "l.rolcanlogin AND NOT l.rolsuper AND NOT l.rolinherit AND NOT l.rolcreatedb
   AND NOT l.rolcreaterole AND NOT l.rolreplication AND NOT l.rolbypassrls
   AND l.rolconnlimit = {CONNECTION_LIMIT}
   AND (l.rolvaliduntil IS NULL OR l.rolvaliduntil = 'infinity')
   AND (SELECT count(*) FROM pg_catalog.pg_auth_members a WHERE a.member = l.oid) = 1
   AND EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members a
                WHERE a.member = l.oid AND a.roleid = {oid}::pg_catalog.oid
                  AND a.inherit_option AND NOT a.set_option AND NOT a.admin_option)
   AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members a WHERE a.roleid = l.oid)
   AND (SELECT s.setconfig FROM pg_catalog.pg_db_role_setting s
         WHERE s.setrole = l.oid AND s.setdatabase = 0) = ARRAY[{marker}]::pg_catalog.text[]
   AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_db_role_setting s
                    WHERE s.setrole = l.oid AND s.setdatabase <> 0)"
    )
}

/// One line, `fn_sql_state absent|adoptable|foreign`: what holds the login's name. A role is
/// adoptable when it carries the marker (exactly), has no member, no per-database setting
/// and no membership but the approved run-as role (`oid`) with inherit, no set and no admin.
pub fn inspect_sql(login: &str, oid: i64) -> String {
    let name = quote_literal(login);
    let marker = quote_literal(&format!("{PERSONAL_LOGIN_MARKER}={MARKER_VALUE}"));
    format!(
        "{PRELUDE}SELECT 'fn_sql_state ' || CASE
  WHEN NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles l WHERE l.rolname = {name}) THEN 'absent'
  WHEN EXISTS (SELECT 1 FROM pg_catalog.pg_roles l WHERE l.rolname = {name}
     AND EXISTS (SELECT 1 FROM pg_catalog.pg_db_role_setting s
                  WHERE s.setrole = l.oid AND s.setdatabase = 0
                    AND {marker} = ANY (s.setconfig))
     AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_db_role_setting s
                      WHERE s.setrole = l.oid AND s.setdatabase <> 0)
     AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members a WHERE a.roleid = l.oid)
     AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members a
                      WHERE a.member = l.oid
                        AND NOT (a.roleid = {oid}::pg_catalog.oid AND a.inherit_option
                                 AND NOT a.set_option AND NOT a.admin_option)))
  THEN 'adoptable' ELSE 'foreign' END;\n"
    )
}

/// [`inspect_sql`]'s answer; `None` for anything else.
pub fn parse_found(out: &str) -> Option<Found> {
    let state = out
        .lines()
        .find_map(|l| l.trim().strip_prefix("fn_sql_state "))?;
    match state.trim() {
        "absent" => Some(Found::Absent),
        "adoptable" => Some(Found::Adoptable),
        "foreign" => Some(Found::Foreign),
        _ => None,
    }
}

/// The drop of a role the worker can't adopt, in order: `(database, script)`. First, in
/// `system_db`, the role can't log in any more and its sessions end; then, in each of
/// `databases`, what it owns goes to the database's owner and every privilege and policy
/// naming it goes (`DROP OWNED`); last, in `system_db`, the role. Each step does nothing once
/// the role is gone.
pub fn drop_sqls(login: &str, system_db: &str, databases: &[String]) -> Vec<(String, String)> {
    let name = quote_literal(login);
    let exists = format!("EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {name})");
    let body = |stmts: &str| {
        format!(
            "{PRELUDE}DO {};\n",
            quote_literal(&format!("BEGIN\n  IF {exists} THEN\n{stmts}  END IF;\nEND"))
        )
    };
    let mut out = vec![(
        system_db.to_string(),
        body(&format!(
            "    EXECUTE {};\n    PERFORM pg_catalog.pg_terminate_backend(a.pid, 2000)\n      FROM pg_catalog.pg_stat_activity a WHERE a.usename = {name};\n",
            quote_literal(&format!("ALTER ROLE {} NOLOGIN", quote_ident(login)))
        )),
    )];
    for db in databases {
        out.push((
            db.clone(),
            body(&format!(
                "    EXECUTE pg_catalog.format('REASSIGN OWNED BY %I TO %I', {name},\n      (SELECT pg_catalog.pg_get_userbyid(d.datdba) FROM pg_catalog.pg_database d\n        WHERE d.datname = pg_catalog.current_database()));\n    EXECUTE {};\n",
                quote_literal(&format!("DROP OWNED BY {}", quote_ident(login)))
            )),
        ));
    }
    out.push((
        system_db.to_string(),
        body(&format!(
            "    EXECUTE {};\n",
            quote_literal(&format!("DROP ROLE {}", quote_ident(login)))
        )),
    ));
    out
}

/// The connectable databases' names, one per line (hex), for [`drop_sqls`].
pub const DATABASES_SQL: &str = "SET search_path = pg_catalog, pg_temp;\n\
SELECT 'fn_sql_db ' || encode(convert_to(datname, 'UTF8'), 'hex') FROM pg_catalog.pg_database \
WHERE datallowconn AND NOT datistemplate ORDER BY datname;\n";

/// [`DATABASES_SQL`]'s names.
pub fn parse_databases(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("fn_sql_db "))
        .filter_map(|h| String::from_utf8(hex::decode(h.trim()).ok()?).ok())
        .collect()
}

/// One transaction that makes (when absent) or repairs the login `login` for the run-as role
/// `role` of approved OID `oid`, with `verifier`: the role named `role` must be that very OID
/// (else nothing changes); then the attributes, the password, the marker as its only setting,
/// the one grant (inherit, no set, no admin); then the invariant ([`exact_predicate`]) is
/// checked by OID, and anything else raises and rolls everything back. Answers
/// `fn_sql_login <oid>` after the commit.
pub fn login_sql(login: &str, role: &str, oid: i64, verifier: &str) -> String {
    let (l, r) = (quote_ident(login), quote_ident(role));
    let (ln, rn) = (quote_literal(login), quote_literal(role));
    let body = format!(
        "DECLARE run_as pg_catalog.name;
BEGIN
  SELECT rolname INTO run_as FROM pg_catalog.pg_roles WHERE oid = {oid}::pg_catalog.oid;
  IF run_as IS DISTINCT FROM {rn} THEN
    RAISE EXCEPTION 'chelabase: the run-as role % is not the approved one', {rn};
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = {ln}) THEN
    CREATE ROLE {l} NOLOGIN NOINHERIT;
  END IF;
  ALTER ROLE {l} WITH LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION
    NOBYPASSRLS CONNECTION LIMIT {CONNECTION_LIMIT} VALID UNTIL 'infinity' PASSWORD {verifier};
  ALTER ROLE {l} RESET ALL;
  ALTER ROLE {l} SET {PERSONAL_LOGIN_MARKER} = {marker};
  GRANT {r} TO {l} WITH INHERIT TRUE, SET FALSE, ADMIN FALSE;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles l WHERE l.rolname = {ln} AND {exact}) THEN
    RAISE EXCEPTION 'chelabase: the function login % does not hold exactly its run-as role', {ln};
  END IF;
END",
        verifier = quote_literal(verifier),
        marker = quote_literal(MARKER_VALUE),
        exact = exact_predicate(oid),
    );
    format!(
        "{PRELUDE}BEGIN;\nSET LOCAL event_triggers = off;\nDO {};\nCOMMIT;\n\
         SELECT 'fn_sql_login ' || oid FROM pg_catalog.pg_roles WHERE rolname = {ln};\n",
        quote_literal(&body)
    )
}

/// [`login_sql`]'s answer: the login's OID.
pub fn parse_login(out: &str) -> Option<i64> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("fn_sql_login "))
        .and_then(|oid| oid.trim().parse().ok())
}

/// The login's verifier: SCRAM of `password` with its stored `salt` (Postgres's default
/// iteration count).
pub fn login_verifier(password: &str, salt: &[u8]) -> String {
    chelabase_neon::scram::verifier_with_salt(password, salt, 4096)
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The reconcile sweep for function SQL logins still to make or confirm (best effort):
    /// each pass takes the least recently attempted branches.
    pub async fn sync_pending_sql_logins(&self) {
        let Some(meta) = self.metadata.functions() else {
            return;
        };
        let candidates: Vec<(BranchId, bool)> = match meta
            .pending_sql_login_branches(crate::fn_triggers::SWEEP_CANDIDATES)
            .await
        {
            Ok(branches) => branches.into_iter().map(|b| (b, true)).collect(),
            Err(e) => return warn!("listing branches with function SQL logins to make: {e}"),
        };
        for branch in self.sweep_batch(Sweep::SqlLogins, &candidates) {
            self.attempted(Sweep::SqlLogins, branch);
            if let Err(e) = self.sync_sql_logins(branch).await {
                warn!(branch_id = %branch, "function SQL logins not synced: {e}");
            }
        }
    }

    /// Makes or confirms every function SQL login the ready `branch` still owes
    /// ([`FunctionSqlLogin`]). One login's failure doesn't stop the others; the call fails
    /// when any did.
    pub async fn sync_sql_logins(&self, branch: BranchId) -> Result<(), String> {
        let Some(meta) = self.metadata.functions() else {
            return Ok(());
        };
        let Some((_, ep)) = self.ready_endpoint(branch).await? else {
            return Ok(());
        };
        // The role check runs where `check_data_role` runs it: the branch's main database.
        let connection = self
            .metadata
            .branch_connection(branch)
            .await
            .map_err(|e| format!("reading the branch: {e}"))?;
        let Some(db) = connection.and_then(|c| c.database_name) else {
            return Ok(());
        };
        let logins = meta
            .pending_sql_logins(branch)
            .await
            .map_err(|e| format!("listing the function SQL logins: {e}"))?;
        let mut failures = Vec::new();
        for login in &logins {
            if let Err(e) = self.sync_sql_login(meta, branch, &ep, &db, login).await {
                warn!(branch_id = %branch, function = %login.name, "function SQL login: {e}");
                failures.push(format!("{}: {e}", login.name));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    async fn sync_sql_login(
        &self,
        meta: &MetadataClient,
        branch: BranchId,
        ep: &str,
        db: &str,
        login: &FunctionSqlLogin,
    ) -> Result<(), String> {
        let name = function_login_name(login.function_id);
        let (sealed, password, salt) = self.sql_password(meta, branch, login).await?;
        // The run-as role must still pass the key's checks, as that very OID.
        match self
            .check_and_grant(
                branch,
                ep,
                db,
                &login.run_as_role,
                Some(login.run_as_role_oid),
            )
            .await
        {
            RoleCheck::Ok(..) => {}
            RoleCheck::Refused(why) => return Err(format!("the run-as role is refused: {why}")),
            RoleCheck::NotReady => {
                return Err("the branch's compute couldn't check the run-as role".into());
            }
            RoleCheck::Failed(why) => return Err(why),
        }
        let found = self
            .computes
            .admin_sql(ep, db, &inspect_sql(&name, login.run_as_role_oid))
            .await
            .map_err(|e| format!("reading the role {name}: {e:#}"))?;
        match parse_found(&found) {
            Some(Found::Absent | Found::Adoptable) => {}
            Some(Found::Foreign) => {
                warn!(branch_id = %branch, role = %name, "a role the worker can't adopt holds a function login's name; dropping it");
                self.drop_foreign_login(ep, db, &name).await?;
            }
            None => {
                return Err(format!(
                    "reading the role {name}: unexpected answer {found:?}"
                ));
            }
        }
        let verifier = login_verifier(&password, &salt);
        let out = self
            .computes
            .admin_sql(
                ep,
                db,
                &login_sql(&name, &login.run_as_role, login.run_as_role_oid, &verifier),
            )
            .await
            .map_err(|e| format!("making the role {name}: {e:#}"))?;
        if parse_login(&out).is_none() {
            return Err(format!("making the role {name}: no login after the commit"));
        }
        let recorded = meta
            .record_function_sql_login(
                branch,
                login.function_id,
                ep,
                &name,
                &verifier,
                &sealed,
                login.run_as_role_oid,
            )
            .await
            .map_err(|e| format!("recording the login {name}: {e}"))?;
        if !recorded {
            // SQL access, the password or the run-as role changed meanwhile: the next pass
            // sees the new state.
            info!(branch_id = %branch, role = %name, "the function changed while its login was made; not recorded");
            return Ok(());
        }
        if let Err(e) = self.cache.password_updated(ep, &name).await {
            warn!(branch_id = %branch, role = %name, "telling the proxies about the login: {e:#}");
        }
        info!(branch_id = %branch, function = %login.name, role = %name, "function SQL login ready");
        Ok(())
    }

    /// The login's `(sealed, password, salt)`: the stored one, or a new one stored first (a
    /// racing run's wins: it is read back).
    async fn sql_password(
        &self,
        meta: &MetadataClient,
        branch: BranchId,
        login: &FunctionSqlLogin,
    ) -> Result<(String, String, Vec<u8>), String> {
        let open = |sealed: &str| {
            open_function_sql_password(&self.key, branch.as_uuid(), login.function_id, sealed)
                .map_err(|e| format!("opening the login's password: {e}"))
        };
        if let Some((sealed, salt)) = &login.password {
            return Ok((sealed.clone(), open(sealed)?, salt.clone()));
        }
        let password = crate::credentials::generate_password();
        let salt: [u8; 16] = rand::random();
        let sealed =
            seal_function_sql_password(&self.key, branch.as_uuid(), login.function_id, &password);
        meta.store_function_sql_password(branch, login.function_id, &sealed, &salt)
            .await
            .map_err(|e| format!("storing the login's password: {e}"))?;
        let stored = meta
            .pending_sql_logins(branch)
            .await
            .map_err(|e| format!("reading the login's password: {e}"))?
            .into_iter()
            .find(|l| {
                l.function_id == login.function_id && l.run_as_role_oid == login.run_as_role_oid
            })
            .and_then(|l| l.password)
            .ok_or("the login's password wasn't stored (did SQL access change?)")?;
        Ok((stored.0.clone(), open(&stored.0)?, stored.1))
    }

    /// [`drop_sqls`] on the compute: the first error stops the drop.
    async fn drop_foreign_login(&self, ep: &str, db: &str, name: &str) -> Result<(), String> {
        let databases = self
            .computes
            .admin_sql(ep, db, DATABASES_SQL)
            .await
            .map(|out| parse_databases(&out))
            .map_err(|e| format!("listing the databases: {e:#}"))?;
        for (target, sql) in drop_sqls(name, db, &databases) {
            self.computes
                .admin_sql(ep, &target, &sql)
                .await
                .map_err(|e| format!("dropping the role {name} (in {target}): {e:#}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: &str = "cb_fn_0123456789ab";

    /// `parts` appear in `sql` in this order.
    fn in_order(sql: &str, parts: &[&str]) {
        let mut at = 0;
        for part in parts {
            let found = sql[at..]
                .find(part)
                .unwrap_or_else(|| panic!("{part} after {at} in {sql}"));
            at += found + part.len();
        }
    }

    /// The login is made with exactly the attributes, setting and grant the issue names, by
    /// OID, and the invariant is checked before the commit.
    #[test]
    fn fn_sql_logins_login_script_is_exact() {
        let sql = login_sql(L, "reporting", 4242, "SCRAM-SHA-256$4096:abc");
        assert!(sql.starts_with(PRELUDE), "{sql}");
        in_order(
            &sql,
            &[
                "BEGIN;",
                "SET LOCAL event_triggers = off;",
                "SELECT rolname INTO run_as FROM pg_catalog.pg_roles WHERE oid = 4242::pg_catalog.oid;",
                "IF run_as IS DISTINCT FROM ''reporting'' THEN",
                "RAISE EXCEPTION",
                "CREATE ROLE \"cb_fn_0123456789ab\" NOLOGIN NOINHERIT;",
                "ALTER ROLE \"cb_fn_0123456789ab\" WITH LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION\n    NOBYPASSRLS CONNECTION LIMIT 10 VALID UNTIL ''infinity'' PASSWORD ''SCRAM-SHA-256$4096:abc'';",
                "ALTER ROLE \"cb_fn_0123456789ab\" RESET ALL;",
                "ALTER ROLE \"cb_fn_0123456789ab\" SET chelabase.managed_role = ''function'';",
                "GRANT \"reporting\" TO \"cb_fn_0123456789ab\" WITH INHERIT TRUE, SET FALSE, ADMIN FALSE;",
                "a.roleid = 4242::pg_catalog.oid",
                "a.inherit_option AND NOT a.set_option AND NOT a.admin_option",
                "NOT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members a WHERE a.roleid = l.oid)",
                "COMMIT;",
                "SELECT 'fn_sql_login ' || oid",
            ],
        );
        // Exactly one membership, the marker as the only setting, no per-database setting.
        assert!(sql.contains("WHERE a.member = l.oid) = 1"), "{sql}");
        assert!(
            sql.contains("= ARRAY[''chelabase.managed_role=function'']::pg_catalog.text[]"),
            "{sql}"
        );
        assert!(sql.contains("s.setdatabase <> 0"), "{sql}");
        for bad in [
            "SET TRUE",
            "ADMIN TRUE",
            "BYPASSRLS ",
            " SUPERUSER",
            "CREATEROLE ",
            "INHERIT TRUE, SET TRUE",
        ] {
            let bad_hits = sql.matches(bad).count();
            let allowed = sql.matches(&format!("NO{}", bad.trim_start())).count();
            assert!(bad_hits <= allowed, "{bad} in {sql}");
        }
    }

    /// A run-as role whose name tries to end the literal or the identifier stays one literal and
    /// one identifier: the DO body is a single quoted string.
    #[test]
    fn fn_sql_logins_names_are_quoted() {
        let sql = login_sql(L, "x'\"; DROP ROLE y; --", 7, "v");
        let body = sql.split_once("DO '").unwrap().1;
        let end = body
            .find("';\nCOMMIT;")
            .expect("the body ends before COMMIT");
        // Inside the body every quote is doubled.
        let inner = &body[..end];
        assert!(!inner.replace("''", "").contains('\''), "{inner}");
        assert!(
            inner.contains("GRANT \"x''\"\"; DROP ROLE y; --\" TO"),
            "{inner}"
        );
    }

    #[test]
    fn fn_sql_logins_inspect_tells_adoptable_from_foreign() {
        let sql = inspect_sql(L, 4242);
        assert!(sql.starts_with(PRELUDE), "{sql}");
        for needle in [
            "'absent'",
            "'adoptable'",
            "'foreign'",
            "'chelabase.managed_role=function' = ANY (s.setconfig)",
            "a.roleid = l.oid",
            "NOT (a.roleid = 4242::pg_catalog.oid AND a.inherit_option",
            "s.setdatabase <> 0",
        ] {
            assert!(sql.contains(needle), "{needle} missing: {sql}");
        }
        assert_eq!(parse_found("x\nfn_sql_state absent\n"), Some(Found::Absent));
        assert_eq!(
            parse_found("fn_sql_state adoptable"),
            Some(Found::Adoptable)
        );
        assert_eq!(parse_found("fn_sql_state foreign\n"), Some(Found::Foreign));
        assert_eq!(parse_found("fn_sql_state maybe"), None);
        assert_eq!(parse_found(""), None);
    }

    /// The drop stops new logins and ends sessions first, then clears every database, then
    /// drops the role.
    #[test]
    fn fn_sql_logins_drop_order() {
        let steps = drop_sqls(L, "app", &["app".into(), "other".into()]);
        let targets: Vec<&str> = steps.iter().map(|(d, _)| d.as_str()).collect();
        assert_eq!(targets, ["app", "app", "other", "app"]);
        in_order(
            &steps[0].1,
            &[
                "ALTER ROLE \"cb_fn_0123456789ab\" NOLOGIN",
                "pg_terminate_backend",
            ],
        );
        for (_, sql) in &steps[1..3] {
            in_order(
                sql,
                &[
                    "REASSIGN OWNED BY %I TO %I",
                    "DROP OWNED BY \"cb_fn_0123456789ab\"",
                ],
            );
        }
        assert!(steps[3].1.contains("DROP ROLE \"cb_fn_0123456789ab\""));
        for (_, sql) in &steps {
            assert!(sql.starts_with(PRELUDE));
            assert!(sql.contains("IF EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = ''cb_fn_0123456789ab'')"), "{sql}");
        }
    }

    #[test]
    fn fn_sql_logins_answers_parse() {
        assert_eq!(parse_login("fn_sql_login 16384\n"), Some(16384));
        assert_eq!(parse_login(""), None);
        let names = parse_databases(&format!(
            "fn_sql_db {}\nfn_sql_db {}\n",
            hex::encode("app"),
            hex::encode("My Db")
        ));
        assert_eq!(names, ["app", "My Db"]);
    }

    #[test]
    fn fn_sql_logins_verifier_uses_the_stored_salt() {
        let salt = [3u8; 16];
        let v = login_verifier("pw", &salt);
        assert_eq!(v, login_verifier("pw", &salt));
        assert!(v.starts_with("SCRAM-SHA-256$4096:"), "{v}");
        assert_ne!(v, login_verifier("pw", &[4u8; 16]));
    }
}
