//! The protection review (R8/R10): code owned by non-Admin roles that can run in someone
//! else's session, scanned as `cloud_admin` in every live database of a branch (listed from
//! `pg_database`, never from the metadata), and its fixes, re-checked against the worker's own
//! scan before they run. Served on `chelabase.rpc.review`.

use std::sync::Arc;
use std::time::Duration;

use chelabase_metadata::{BranchKind, BranchStatus};
use chelabase_nats::{ReviewFixOrder, ReviewReply, ReviewRequest};
use chelabase_neon::ComputeBackend;
use chelabase_schema::review::{fix_sql, parse_review, review_hash, review_sql};
use chelabase_types::BranchId;
use chelabase_types::protection::ReviewItem;
use tracing::{info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::resync::SYSTEM_DATABASE;
use crate::{OWNER_ROLE, Worker};

/// The first line of the live database list's SQL (the fakes' needle).
pub const REVIEW_DATABASES_MARK: &str = "-- chelabase: review databases";

/// A fix's `lock_timeout` (P7).
const FIX_LOCK_TIMEOUT: &str = "5s";

/// Hard cap on one review's answer, below the orchestrator's request timeout (15 s). A fix
/// past it goes on to its end under the endpoint's lock.
pub const REVIEW_DEADLINE: Duration = Duration::from_secs(12);

/// Every database a session can connect to, one name per line (psql `-A -t`).
pub fn review_databases_sql() -> String {
    format!(
        "{REVIEW_DATABASES_MARK}\nSELECT datname FROM pg_catalog.pg_database \
         WHERE NOT datistemplate AND datallowconn ORDER BY datname;"
    )
}

/// The fix in its own transaction with the fix's `lock_timeout`; errors verbose, so the
/// SQLSTATE comes back with the message.
pub fn fix_script(sql: &str) -> String {
    format!(
        "\\set VERBOSITY verbose\nBEGIN;\nSET LOCAL lock_timeout = '{FIX_LOCK_TIMEOUT}';\n{sql}\nCOMMIT;\n"
    )
}

/// The SQLSTATE of a verbose psql error (`ERROR:  2BP01: …`); `XX000` when there is none.
pub fn sqlstate_of(error: &str) -> String {
    error
        .match_indices("ERROR:")
        .find_map(|(at, tag)| {
            let rest = error[at + tag.len()..].trim_start();
            let code = rest.get(..5)?;
            (code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
                && rest[5..].starts_with(':'))
            .then(|| code.to_string())
        })
        .unwrap_or_else(|| "XX000".to_string())
}

fn failed(message: impl Into<String>) -> ReviewReply {
    ReviewReply::Failed {
        message: message.into(),
    }
}

/// The review RPC: the run is spawned, and one past `deadline` answers `Failed` while it goes
/// on (a fix ends under the endpoint's lock).
pub async fn review_reply<M, S, C, N, P>(
    worker: Arc<Worker<M, S, C, N, P>>,
    payload: &[u8],
    deadline: Duration,
) -> ReviewReply
where
    M: WorkerMetadata,
    S: Storage + Send + Sync + 'static,
    C: ComputeBackend + Send + Sync + 'static,
    N: CacheNotifier + Send + Sync + 'static,
    P: SpecPusher + Send + Sync + 'static,
{
    let request = match serde_json::from_slice::<ReviewRequest>(payload) {
        Ok(request) => request,
        Err(e) => return failed(format!("bad request: {e}")),
    };
    let branch = request.branch_id;
    let run = tokio::spawn(async move { worker.review(request).await });
    match tokio::time::timeout(deadline, run).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => failed(format!("the review stopped: {e}")),
        Err(_) => {
            warn!(branch_id = %branch, "review past its deadline; it goes on");
            failed("timed out; the review goes on, and a retry is safe")
        }
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The branch's review, or one fix of it. Any failure of the scan is `Failed`: never a
    /// partial list.
    pub async fn review(&self, request: ReviewRequest) -> ReviewReply {
        let branch = BranchId::from(request.branch_id);
        let record = match self.metadata.get_branch(branch).await {
            Ok(Some(b)) => b,
            Ok(None) => return failed("the branch doesn't exist"),
            Err(e) => return failed(format!("reading the branch: {e}")),
        };
        let ep = match (&record.status, &record.kind, &record.endpoint_id) {
            (BranchStatus::Ready, BranchKind::Normal, Some(ep)) => ep.clone(),
            _ => return failed("the branch isn't ready"),
        };
        let Some(order) = request.fix else {
            return match self.scan(&ep).await {
                Ok(items) => ReviewReply::Scanned { items },
                Err(message) => failed(message),
            };
        };
        // A fix runs under the endpoint's lock, as the reconciler's changes do.
        let _lock = match self.metadata.lock_endpoint(&ep).await {
            Ok(lock) => lock,
            Err(e) => return failed(format!("locking the endpoint: {e}")),
        };
        self.fix(branch, &ep, order).await
    }

    async fn fix(&self, branch: BranchId, ep: &str, order: ReviewFixOrder) -> ReviewReply {
        let items = match self.scan(ep).await {
            Ok(items) => items,
            Err(message) => return failed(message),
        };
        if review_hash(&items) != order.hash {
            return ReviewReply::Changed { items };
        }
        let Some(item) = items.iter().find(|i| i.key == order.key) else {
            return ReviewReply::NotFound { items };
        };
        let sql = match fix_sql(item, order.action, OWNER_ROLE) {
            Ok(sql) => sql,
            Err(e) => return failed(format!("rendering the fix of {}: {e}", item.key)),
        };
        let database = item.database.clone();
        if let Err(e) = self
            .computes
            .admin_sql(ep, &database, &fix_script(&sql))
            .await
        {
            let message = format!("{e:#}");
            warn!(branch_id = %branch, key = %order.key, "review fix failed: {message}");
            return ReviewReply::FixFailed {
                sqlstate: sqlstate_of(&message),
                message,
                items,
            };
        }
        info!(branch_id = %branch, key = %order.key, action = ?order.action, "review fix ran");
        match self.scan(ep).await {
            Ok(items) => ReviewReply::Fixed { sql, items },
            Err(message) => failed(format!(
                "the fix ran ({sql}); the scan after it failed: {message}"
            )),
        }
    }

    /// Every live database of the compute, scanned; sorted by key.
    async fn scan(&self, ep: &str) -> Result<Vec<ReviewItem>, String> {
        let list = self
            .computes
            .admin_sql(ep, SYSTEM_DATABASE, &review_databases_sql())
            .await
            .map_err(|e| format!("listing the databases: {e:#}"))?;
        let sql = review_sql(OWNER_ROLE);
        let mut items = Vec::new();
        for database in list.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let out = self
                .computes
                .admin_sql(ep, database, &sql)
                .await
                .map_err(|e| format!("scanning {database}: {e:#}"))?;
            let found = parse_review(database, out.trim())
                .map_err(|e| format!("reading the scan of {database}: {e}"))?;
            items.extend(found);
        }
        items.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(items)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod sqlstate_tests {
    use super::*;

    #[test]
    fn the_sqlstate_comes_from_a_verbose_error() {
        let err = "psql in compute ep-1 exited with 3: psql:<stdin>:4: ERROR:  2BP01: cannot drop function public.f() because other objects depend on it\nDETAIL:  …";
        assert_eq!(sqlstate_of(err), "2BP01");
        assert_eq!(
            sqlstate_of("ERROR:  55P03: canceling statement due to lock timeout"),
            "55P03"
        );
        for none in [
            "ERROR:  cannot drop",
            "psql: connection refused",
            "ERROR:  ab12x: no",
            "",
        ] {
            assert_eq!(sqlstate_of(none), "XX000", "{none}");
        }
    }

    #[test]
    fn the_fix_script_is_one_transaction_with_the_lock_timeout() {
        let script = fix_script("DROP ROUTINE public.f() RESTRICT;");
        assert!(
            script.starts_with("\\set VERBOSITY verbose\nBEGIN;\nSET LOCAL lock_timeout = '5s';\n"),
            "{script}"
        );
        assert!(
            script.ends_with("DROP ROUTINE public.f() RESTRICT;\nCOMMIT;\n"),
            "{script}"
        );
    }
}
