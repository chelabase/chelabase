//! The `chelabase_realtime` login role (RT1 T5: R1, P15, S22).
//!
//! Realtime opens its logical replication connections as this role. The worker creates it as
//! `cloud_admin` (through `admin_sql`, so the DDL receiver never sees it): `LOGIN REPLICATION
//! NOBYPASSRLS NOINHERIT CONNECTION LIMIT 12`, the password from
//! `CHELABASE_REALTIME_PG_PASSWORD_FILE` (sent as a SCRAM verifier, never in plain text), and
//! `logical_decoding_work_mem = '8MB'` as a role setting (W9). The script is idempotent, so it
//! runs at provisioning, at every compute (re)start, and at reconcile (the backfill, and the
//! password rotation). Every database of a branch grants it `CONNECT` (W21): at provisioning,
//! at data API enable, and in the member-roles reconcile before a born-closed database opens.
//! It is never in the proxy's role secrets, a role list, an act-as picker, a merge or a member
//! grant (W30).
//!
//! Neither script fails the step that runs it: realtime is an add-on to a branch, and the next
//! reconcile pass (the role) or member-roles run (the grants) tries again.

use chelabase_neon::ComputeBackend;
use chelabase_neon::backend::SqlOpts;
use tracing::warn;

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// The role's name.
pub const REALTIME_ROLE: &str = "chelabase_realtime";

/// The first line of every [`realtime_connect_sql`] script: the fakes' needle.
pub const REALTIME_CONNECT_MARK: &str = "-- chelabase: realtime connect";

/// The first line of the script that reads the role's stored verifier: the fakes' needle.
pub const REALTIME_VERIFIER_MARK: &str = "-- chelabase: realtime verifier";

/// Where the role script runs: roles are cluster-wide, and the system database always exists
/// (the main one may be dropped).
const ROLE_DATABASE: &str = "postgres";

/// `'…'` with embedded quotes doubled.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `"…"` with embedded double quotes doubled.
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Reads the role's stored password (`rolpassword`): one line, empty without the role or a
/// password. Starts with [`REALTIME_VERIFIER_MARK`]; it never carries the secret.
pub fn realtime_verifier_sql() -> String {
    format!(
        "{REALTIME_VERIFIER_MARK}
SELECT a.rolpassword FROM pg_catalog.pg_authid a WHERE a.rolname OPERATOR(pg_catalog.=) '{REALTIME_ROLE}';
"
    )
}

/// The idempotent script that creates the role or brings it back to its attributes, password
/// (`verifier`, a SCRAM-SHA-256 verifier; `None` leaves the stored one) and setting.
pub fn realtime_role_sql(verifier: Option<&str>) -> String {
    let password = verifier
        .map(|v| format!(" PASSWORD {}", literal(v)))
        .unwrap_or_default();
    format!(
        "SET search_path = pg_catalog, pg_temp; DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{REALTIME_ROLE}') THEN
    CREATE ROLE {REALTIME_ROLE};
  END IF;
END
$$;
ALTER ROLE {REALTIME_ROLE} WITH LOGIN REPLICATION NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS NOINHERIT CONNECTION LIMIT 12{password};
ALTER ROLE {REALTIME_ROLE} SET logical_decoding_work_mem = '8MB';
"
    )
}

/// Grants `CONNECT` on each of `databases` to the role; starts with [`REALTIME_CONNECT_MARK`].
///
/// A sweep re-runs it on every database, so each grant runs only where the database's ACL has
/// no `CONNECT` entry for the role itself (`aclexplode`, not `has_database_privilege`: PUBLIC's
/// default `CONNECT` would answer for the role, and a later `REVOKE … FROM PUBLIC` would then
/// lock realtime out). One `DO` block, under a dollar-quote tag no database name contains.
pub fn realtime_connect_sql(databases: &[&str]) -> String {
    let mut sql = format!("{REALTIME_CONNECT_MARK}\n");
    if databases.is_empty() {
        return sql;
    }
    let mut body = String::new();
    for db in databases {
        body.push_str(&format!(
            "  IF NOT EXISTS (SELECT FROM pg_catalog.pg_database d, \
             pg_catalog.aclexplode(d.datacl) a \
             WHERE d.datname OPERATOR(pg_catalog.=) {} \
             AND a.grantee OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r \
             WHERE r.rolname OPERATOR(pg_catalog.=) '{REALTIME_ROLE}') \
             AND a.privilege_type OPERATOR(pg_catalog.=) 'CONNECT') THEN\n    \
             GRANT CONNECT ON DATABASE {} TO {REALTIME_ROLE};\n  END IF;\n",
            literal(db),
            ident(db)
        ));
    }
    let tag = dollar_tag(&body);
    sql.push_str(&format!("DO {tag}\nBEGIN\n{body}END\n{tag};\n"));
    sql
}

/// A dollar-quote tag that `body` doesn't contain.
fn dollar_tag(body: &str) -> String {
    let mut tag = "$connect$".to_owned();
    let mut n = 0u32;
    while body.contains(&tag) {
        n += 1;
        tag = format!("$connect{n}$");
    }
    tag
}

impl<M, S, C, N, P> Worker<M, S, C, N, P>
where
    M: WorkerMetadata,
    S: Storage,
    C: ComputeBackend,
    N: CacheNotifier,
    P: SpecPusher,
{
    /// Creates the role on `ep`'s compute, or brings it back to its attributes and the current
    /// password. The stored verifier is read first: when it already verifies the password, the
    /// script leaves it (no `ALTER ROLE … PASSWORD` on every pass); otherwise (no role, another
    /// password, an unreadable value, a failed read) it sets a fresh verifier. Nothing without
    /// a password. A failure is logged and returned as `false`.
    pub(crate) async fn apply_realtime_role(&self, ep: &str) -> bool {
        let Some(password) = self.realtime_password.as_deref() else {
            return true;
        };
        let stored = match self
            .computes
            .admin_sql_with(
                ep,
                ROLE_DATABASE,
                &realtime_verifier_sql(),
                &SqlOpts::admin_default(),
            )
            .await
        {
            Ok(out) => Some(out.trim().to_owned()),
            Err(e) => {
                warn!(endpoint_id = %ep, "reading the realtime role's verifier failed; setting it: {e:#}");
                None
            }
        };
        let current =
            stored.is_some_and(|s| !s.is_empty() && chelabase_neon::scram::verifies(&s, password));
        let verifier = (!current).then(|| chelabase_neon::scram::verifier(password));
        // Terse errors (the default): the verifier never shows in a `LINE` excerpt.
        match self
            .computes
            .admin_sql_with(
                ep,
                ROLE_DATABASE,
                &realtime_role_sql(verifier.as_deref()),
                &SqlOpts::admin_default(),
            )
            .await
        {
            Ok(_) => true,
            Err(e) => {
                warn!(endpoint_id = %ep, "making the realtime role failed; reconcile retries: {e:#}");
                false
            }
        }
    }

    /// Grants the role `CONNECT` on `databases` of `ep`'s compute. Nothing without a password
    /// (no role) or without databases. A failure is logged and returned as `false`.
    pub(crate) async fn grant_realtime_connect(&self, ep: &str, databases: &[&str]) -> bool {
        if self.realtime_password.is_none() || databases.is_empty() {
            return true;
        }
        match self
            .computes
            .admin_sql(ep, ROLE_DATABASE, &realtime_connect_sql(databases))
            .await
        {
            Ok(_) => true,
            Err(e) => {
                warn!(endpoint_id = %ep, ?databases, "granting the realtime role CONNECT failed: {e:#}");
                false
            }
        }
    }

    /// [`Self::apply_realtime_role`], then `CONNECT` on `database` (a new or restarted compute).
    pub(crate) async fn ensure_realtime(&self, ep: &str, database: &str) {
        if self.apply_realtime_role(ep).await {
            self.grant_realtime_connect(ep, &[database]).await;
        }
    }
}

/// Edge cases beyond the RED set (RT1 T5 coder).
#[cfg(test)]
mod tests {
    use chelabase_metadata::{DataApiState, DatabaseStatus};
    use chelabase_nats::{DataApiDatabaseRequest, MemberRolesReply};

    use super::*;
    use crate::fakes::{FakeDbRow, FakeWorker, ready_database, worker};

    fn h(s: &str) -> String {
        hex::encode(s.as_bytes())
    }

    fn scripts_with(w: &FakeWorker, needle: &str) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.contains(needle))
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    #[test]
    fn a_connect_script_without_databases_is_only_its_mark() {
        assert_eq!(
            realtime_connect_sql(&[]),
            format!("{REALTIME_CONNECT_MARK}\n")
        );
    }

    /// Coordinator ruling 2 (RT1 final fix round): the guard reads the role's own ACL entry,
    /// never `has_database_privilege` (PUBLIC's default `CONNECT` would answer for the role and
    /// skip the explicit grant that a later `REVOKE … FROM PUBLIC` leaves in place).
    #[test]
    fn each_grant_is_guarded_by_the_roles_own_acl_entry() {
        let sql = realtime_connect_sql(&["app", "it's"]);
        assert!(!sql.contains("has_database_privilege"), "{sql}");
        for (literal, ident) in [("'app'", "\"app\""), ("'it''s'", "\"it's\"")] {
            let guard = format!(
                "IF NOT EXISTS (SELECT FROM pg_catalog.pg_database d, pg_catalog.aclexplode(d.datacl) a \
                 WHERE d.datname OPERATOR(pg_catalog.=) {literal} \
                 AND a.grantee OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r \
                 WHERE r.rolname OPERATOR(pg_catalog.=) 'chelabase_realtime') \
                 AND a.privilege_type OPERATOR(pg_catalog.=) 'CONNECT') THEN\n    \
                 GRANT CONNECT ON DATABASE {ident} TO chelabase_realtime;\n  END IF;"
            );
            assert!(sql.contains(&guard), "{guard}\nin {sql}");
        }
    }

    /// A database name holding the default tag moves the tag on.
    #[test]
    fn the_dollar_tag_avoids_every_database_name() {
        let sql = realtime_connect_sql(&["$connect$", "$connect1$"]);
        assert!(sql.contains("DO $connect2$\n"), "{sql}");
        assert_eq!(sql.matches("$connect2$").count(), 2, "{sql}");
    }

    #[tokio::test]
    async fn no_databases_runs_no_connect_script() {
        let (mut w, _journal) = worker();
        w.realtime_password = Some("pw".into());
        assert!(w.grant_realtime_connect("ep-x", &[]).await);
        assert!(w.computes.sql.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_role_script_never_fails_provisioning_and_grants_nothing() {
        let (mut w, _journal) = worker();
        w.realtime_password = Some("pw".into());
        w.computes.fail_when("CREATE ROLE chelabase_realtime");
        let db = ready_database(&w).await;
        assert_eq!(w.metadata.database(db).status, DatabaseStatus::Ready);
        assert!(
            scripts_with(&w, REALTIME_CONNECT_MARK).is_empty(),
            "no grant to a role that wasn't made"
        );
    }

    #[tokio::test]
    async fn a_failed_connect_grant_never_fails_provisioning_or_an_enable() {
        let (mut w, _journal) = worker();
        w.realtime_password = Some("pw".into());
        w.computes.fail_when(REALTIME_CONNECT_MARK);
        let db = ready_database(&w).await;
        assert_eq!(w.metadata.database(db).status, DatabaseStatus::Ready);

        let main = w.metadata.main(db).id;
        w.metadata
            .state()
            .db_rows
            .entry(main)
            .or_default()
            .push(FakeDbRow::new(
                "analytics",
                DataApiState::Enabling,
                "public",
            ));
        w.enable_data_api(&DataApiDatabaseRequest {
            branch_id: main,
            name: "analytics".into(),
            remove: false,
            grant_existing: false,
        })
        .await;
        let row = w.metadata.state().db_rows[&main]
            .iter()
            .find(|r| r.name == "analytics")
            .map(|r| r.state)
            .unwrap();
        assert_eq!(row, DataApiState::On);
    }

    #[tokio::test]
    async fn a_failed_enable_grants_nothing() {
        let (mut w, _journal) = worker();
        w.realtime_password = Some("pw".into());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        w.metadata
            .state()
            .db_rows
            .entry(main)
            .or_default()
            .push(FakeDbRow::new(
                "analytics",
                DataApiState::Enabling,
                "public",
            ));
        w.computes.sql.lock().unwrap().clear();
        w.computes.fail_in("analytics", "");
        w.enable_data_api(&DataApiDatabaseRequest {
            branch_id: main,
            name: "analytics".into(),
            remove: false,
            grant_existing: false,
        })
        .await;
        assert!(
            scripts_with(&w, "\"analytics\" TO chelabase_realtime").is_empty(),
            "{:?}",
            w.computes.sql.lock().unwrap()
        );
    }

    /// The member catalog: `postgres` open, a closed template and a closed non-template.
    fn catalog() -> String {
        format!(
            "r {} f t f t\nd {} {}\nd {} {}\nc {} f t f\nc {} f f f\ng {}\n",
            h("postgres"),
            h("app"),
            h("postgres"),
            h("postgres"),
            h("postgres"),
            h("tmpl"),
            h("shut"),
            h("template1"),
        )
    }

    #[tokio::test]
    async fn the_member_reconcile_grants_neither_templates_nor_the_system_database() {
        let (mut w, _journal) = worker();
        w.realtime_password = Some("pw".into());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let ep = main.endpoint_id.unwrap();
        w.computes
            .answer_at(&ep, "chelabase: member roles", &catalog());
        w.computes.sql.lock().unwrap().clear();

        w.reconcile_member_roles(main.id, None).await;

        let grants = scripts_with(&w, REALTIME_CONNECT_MARK);
        assert_eq!(
            grants,
            // Closed databases first (the script stops at its first error).
            [realtime_connect_sql(&["shut", "app"])],
            "{grants:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_connect_grant_never_fails_the_member_reconcile() {
        let (mut w, _journal) = worker();
        w.realtime_password = Some("pw".into());
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        let ep = main.endpoint_id.unwrap();
        w.computes
            .answer_at(&ep, "chelabase: member roles", &catalog());
        w.computes.fail_when(REALTIME_CONNECT_MARK);

        let reply = w.reconcile_member_roles(main.id, None).await;

        assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    }

    #[tokio::test]
    async fn a_failed_role_script_never_fails_the_reconcile_resync() {
        let (mut w, journal) = worker();
        let db = ready_database(&w).await;
        let ep = w.metadata.main(db).endpoint_id.unwrap();
        w.computes
            .answer("to_regclass('chelabase.schema_version')", "t\n");
        w.realtime_password = Some("pw".into());
        w.computes.fail_when("CREATE ROLE chelabase_realtime");
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.database(db).status, DatabaseStatus::Ready);
        let entries = crate::fakes::entries(&journal);
        // The role script is the only one run in `postgres` with this first line.
        let tried = entries
            .iter()
            .position(|e| {
                e == &format!(
                    "compute.sql {ep} postgres SET search_path = pg_catalog, pg_temp; DO $$"
                )
            })
            .unwrap_or_else(|| panic!("the role was tried: {entries:?}"));
        assert!(
            entries[tried + 1..]
                .iter()
                .any(|e| e.starts_with(&format!("compute.sql {ep} "))),
            "the resync still ran: {entries:?}"
        );
    }
}
