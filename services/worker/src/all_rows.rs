//! EOS-H1 (user ruling 2026-10-23: fail closed, via the worker): would a key create, or a
//! role's membership change, let the data API reach a role with the members hub's "all rows"
//! policy? Read as `cloud_admin` on the branch's compute through `admin_sql`: the cluster's
//! memberships once, and the "all rows" roles of every live database (listed from
//! `pg_database`, as the review does). Served on `chelabase.rpc.roles.all_rows_check`. Any
//! failure is `Failed`: the orchestrator refuses (503), never allows.
//!
//! Pinned for the fakes and the implementation:
//! - the databases are listed with [`crate::review::review_databases_sql`];
//! - the memberships read starts with [`ALL_ROWS_MEMBERSHIPS_MARK`] and answers one line per
//!   membership, `<member hex>|<role hex>` (lowercase hex of the names' UTF-8 bytes);
//! - the "all rows" roles read (in each database) starts with [`ALL_ROWS_ROLES_MARK`] and
//!   answers one line per role, `<role hex>`; it finds the hub's policy as
//!   `chelabase_schema::privileges::ALL_ROWS_ROLES_SQL` does (marker `chelabase:all_rows`);
//! - an empty line is skipped; any other line that isn't that shape fails the check.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chelabase_metadata::BranchStatus;
use chelabase_nats::{AllRowsCheckReply, AllRowsCheckRequest, MembershipChange};
use chelabase_neon::ComputeBackend;
use chelabase_schema::privileges::{ALL_ROWS_ROLES_SQL, new_all_rows_chain};
use tracing::warn;

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::SYSTEM_DATABASE;
use crate::review::review_databases_sql;

/// The first line of the memberships read (the fakes' needle).
pub const ALL_ROWS_MEMBERSHIPS_MARK: &str = "-- chelabase: all-rows memberships";

/// The first line of each database's "all rows" roles read (the fakes' needle).
pub const ALL_ROWS_ROLES_MARK: &str = "-- chelabase: all-rows roles";

/// Member → the roles it is a member of (any option).
type Edges = BTreeMap<String, BTreeSet<String>>;

/// Hard cap on one check's answer, below the orchestrator's request timeout (15 s).
pub const ALL_ROWS_CHECK_DEADLINE: Duration = Duration::from_secs(12);

/// The RPC: the request parsed, the check run, and one past `deadline` answers `Failed`.
pub async fn all_rows_check_reply<M, S, C, N, P>(
    worker: Arc<Worker<M, S, C, N, P>>,
    payload: &[u8],
    deadline: Duration,
) -> AllRowsCheckReply
where
    M: WorkerMetadata,
    S: Storage + Send + Sync + 'static,
    C: ComputeBackend + Send + Sync + 'static,
    N: CacheNotifier + Send + Sync + 'static,
    P: SpecPusher + Send + Sync + 'static,
{
    let request = match serde_json::from_slice::<AllRowsCheckRequest>(payload) {
        Ok(request) => request,
        Err(e) => return failed(format!("bad request: {e}")),
    };
    let branch = request.branch_id;
    let run = tokio::spawn(async move { worker.all_rows_check(request).await });
    match tokio::time::timeout(deadline, run).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => failed(format!("the check stopped: {e}")),
        Err(_) => {
            warn!(branch_id = %branch, "all-rows check past its deadline");
            failed("the check timed out")
        }
    }
}

fn failed(message: impl Into<String>) -> AllRowsCheckReply {
    AllRowsCheckReply::Failed {
        message: message.into(),
    }
}

/// Every membership of the cluster, `<member hex>|<role hex>` per line.
pub fn all_rows_memberships_sql() -> String {
    format!(
        "{ALL_ROWS_MEMBERSHIPS_MARK}\n\
         SELECT pg_catalog.encode(pg_catalog.convert_to(a.rolname::pg_catalog.text, 'UTF8'), 'hex') \
         OPERATOR(pg_catalog.||) '|' OPERATOR(pg_catalog.||) \
         pg_catalog.encode(pg_catalog.convert_to(b.rolname::pg_catalog.text, 'UTF8'), 'hex') \
         FROM pg_catalog.pg_auth_members m \
         JOIN pg_catalog.pg_roles a ON a.oid OPERATOR(pg_catalog.=) m.member \
         JOIN pg_catalog.pg_roles b ON b.oid OPERATOR(pg_catalog.=) m.roleid;"
    )
}

/// The roles with the hub's "all rows" policy in the current database, `<role hex>` per line.
pub fn all_rows_roles_sql() -> String {
    format!(
        "{ALL_ROWS_ROLES_MARK}\n\
         SELECT pg_catalog.encode(pg_catalog.convert_to(t.rolname, 'UTF8'), 'hex') \
         FROM ({ALL_ROWS_ROLES_SQL}) AS t(rolname);"
    )
}

/// One hex-encoded UTF-8 name.
fn unhex(field: &str) -> Result<String, String> {
    if field.is_empty() {
        return Err("an empty role name".to_string());
    }
    let bytes = hex::decode(field).map_err(|e| format!("{field:?} isn't hex: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("{field:?} isn't UTF-8: {e}"))
}

/// The memberships read's answer: member → the roles it is a member of.
fn parse_memberships(out: &str) -> Result<Edges, String> {
    let mut edges = Edges::new();
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut parts = line.split('|');
        let (Some(member), Some(role), None) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("unexpected membership line {line:?}"));
        };
        edges.entry(unhex(member)?).or_default().insert(unhex(role)?);
    }
    Ok(edges)
}

/// An "all rows" roles read's answer.
fn parse_roles(out: &str) -> Result<BTreeSet<String>, String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(unhex)
        .collect()
}

/// The memberships after `change`: the role's memberships replaced by `member_of`, its `pg_*`
/// and Chelabase memberships kept (as the worker's role update applies it).
fn after_change(
    edges: &Edges,
    change: &MembershipChange,
) -> Edges {
    let mut after = edges.clone();
    let kept = edges
        .get(&change.role)
        .into_iter()
        .flatten()
        .filter(|g| g.starts_with("pg_") || g.starts_with("chelabase"))
        .cloned();
    let new: BTreeSet<String> = kept.chain(change.member_of.iter().cloned()).collect();
    after.insert(change.role.clone(), new);
    after
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The check on the request's branch: [`AllRowsCheckReply::Exposed`] with one of the
    /// shortest chains (`chelabase_schema::privileges::new_all_rows_chain`; with a change, only
    /// what it newly connects counts, the role's `pg_*` and Chelabase memberships kept), `Clear`,
    /// or `Failed` when anything can't be read (never a partial answer).
    pub async fn all_rows_check(&self, request: AllRowsCheckRequest) -> AllRowsCheckReply {
        let branch = request.branch_id;
        let record = match self.metadata.get_branch(branch).await {
            Ok(Some(b)) => b,
            Ok(None) => return failed("the branch doesn't exist"),
            Err(e) => return failed(format!("reading the branch: {e}")),
        };
        let ep = match (&record.status, &record.endpoint_id) {
            (BranchStatus::Ready, Some(ep)) => ep.clone(),
            _ => return failed("the branch isn't ready"),
        };
        let (edges, all_rows) = match self.all_rows_facts(&ep).await {
            Ok(facts) => facts,
            Err(message) => {
                warn!(branch_id = %branch, "all-rows check failed: {message}");
                return failed(message);
            }
        };
        let starting: BTreeSet<String> = request.starting.into_iter().collect();
        let chain = match &request.change {
            None => new_all_rows_chain(&edges, None, &starting, &all_rows),
            Some(change) => {
                let after = after_change(&edges, change);
                new_all_rows_chain(&after, Some(&edges), &starting, &all_rows)
            }
        };
        match chain {
            Some(chain) => AllRowsCheckReply::Exposed { chain },
            None => AllRowsCheckReply::Clear,
        }
    }

    /// The cluster's memberships and the "all rows" roles of every live database of the
    /// compute; any read that fails or answers a line it can't parse fails the whole read.
    async fn all_rows_facts(&self, ep: &str) -> Result<(Edges, BTreeSet<String>), String> {
        let list = self
            .computes
            .admin_sql(ep, SYSTEM_DATABASE, &review_databases_sql())
            .await
            .map_err(|e| format!("listing the databases: {e:#}"))?;
        // A compute always has a database to connect to: an empty list is a broken read, and
        // reading nothing would answer `Clear`.
        if list.lines().all(|l| l.trim().is_empty()) {
            return Err("listing the databases: no database answered".to_string());
        }
        let out = self
            .computes
            .admin_sql(ep, SYSTEM_DATABASE, &all_rows_memberships_sql())
            .await
            .map_err(|e| format!("reading the memberships: {e:#}"))?;
        let edges = parse_memberships(&out).map_err(|e| format!("reading the memberships: {e}"))?;
        let sql = all_rows_roles_sql();
        let mut all_rows = BTreeSet::new();
        for database in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let out = self
                .computes
                .admin_sql(ep, database, &sql)
                .await
                .map_err(|e| format!("reading the \"all rows\" roles of {database}: {e:#}"))?;
            all_rows.extend(
                parse_roles(&out)
                    .map_err(|e| format!("reading the \"all rows\" roles of {database}: {e}"))?,
            );
        }
        Ok((edges, all_rows))
    }
}

#[cfg(test)]
mod tests;

/// The coder's edge cases beyond the RED set.
#[cfg(test)]
mod edge_tests {
    use super::*;
    use crate::fakes::{ready_database, worker};
    use crate::review::REVIEW_DATABASES_MARK;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    // The reads are schema-qualified catalog reads that hex-encode the names' UTF-8 bytes;
    // the roles read is the shared "all rows" read wrapped.
    #[test]
    fn the_reads_are_qualified_and_hex_encode_utf8() {
        let members = all_rows_memberships_sql();
        assert!(members.starts_with(ALL_ROWS_MEMBERSHIPS_MARK), "{members}");
        assert!(members.contains("pg_catalog.pg_auth_members"), "{members}");
        assert!(members.contains("pg_catalog.convert_to(") && members.contains("'UTF8'"));
        let roles = all_rows_roles_sql();
        assert!(roles.starts_with(ALL_ROWS_ROLES_MARK), "{roles}");
        assert!(roles.contains(ALL_ROWS_ROLES_SQL), "{roles}");
        assert!(roles.contains("pg_catalog.encode(") && roles.contains("'hex'"));
    }

    // Lines: blank ones skipped, either hex case read, anything else (an empty name, invalid
    // UTF-8, a missing or extra field) refused.
    #[test]
    fn lines_are_parsed_strictly() {
        let edges = parse_memberships("\n6B|72\n  \n6b|6d\n").unwrap();
        assert_eq!(edges.get("k"), Some(&set(&["m", "r"])));
        assert_eq!(parse_roles("72\n\n").unwrap(), set(&["r"]));
        for bad in ["6b", "6b|", "|72", "6b|72|", "ff|72", "6b 72", "6g|72"] {
            assert!(parse_memberships(bad).is_err(), "{bad}");
        }
        for bad in ["ff", "7", "zz"] {
            assert!(parse_roles(bad).is_err(), "{bad}");
        }
    }

    // A change keeps the role's `pg_*` and Chelabase memberships: an exposure through one of
    // them was already there, so it isn't this change's; cycles end.
    #[test]
    fn a_change_keeps_pg_and_chelabase_memberships() {
        let mut edges = Edges::new();
        edges.insert("k".into(), set(&["chelabase_x", "old"]));
        edges.insert("chelabase_x".into(), set(&["r", "k"]));
        let change = MembershipChange { role: "k".into(), member_of: vec![] };
        let after = after_change(&edges, &change);
        assert_eq!(after.get("k"), Some(&set(&["chelabase_x"])));
        let starting = set(&["k"]);
        assert_eq!(new_all_rows_chain(&after, Some(&edges), &starting, &set(&["r"])), None);
    }

    // An empty database list is a broken read: `Failed`, never `Clear`.
    #[tokio::test]
    async fn an_empty_database_list_fails_closed() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.answer(REVIEW_DATABASES_MARK, "\n");
        let request = AllRowsCheckRequest { branch_id: main.id, starting: vec!["k".into()], change: None };
        let reply = w.all_rows_check(request).await;
        assert!(matches!(reply, AllRowsCheckReply::Failed { .. }), "{reply:?}");
    }

    // The RPC's deadline: a check past it answers `Failed`.
    #[tokio::test]
    async fn a_check_past_the_deadline_fails() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let w = Arc::new(w);
        let payload = serde_json::to_vec(&AllRowsCheckRequest {
            branch_id: main.id,
            starting: vec!["k".into()],
            change: None,
        })
        .unwrap();
        let reply = all_rows_check_reply(w, &payload, Duration::ZERO).await;
        assert!(matches!(reply, AllRowsCheckReply::Failed { .. }), "{reply:?}");
    }
}
