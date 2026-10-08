//! E29a: the live sessions of a role on a branch's compute, read from `pg_stat_activity` over
//! the admin path. A read only: a compute that isn't running answers so and is never started.

use std::time::Duration;

use chelabase_metadata::{BranchKind, BranchStatus};
use chelabase_nats::{RoleRequest, RoleSession, RoleSessionsReply};
use chelabase_neon::{ComputeBackend, ComputeStatus, SqlOpts};
use chelabase_types::role_name::check_any_role_name;
use tracing::warn;

use crate::Worker;
use crate::data_api::quote_literal;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// The read gives up after this, inside the request's 10 s deadline (the orchestrator waits
/// 15 s for the reply).
const SESSIONS_TIMEOUT: Duration = Duration::from_secs(8);

/// The client sessions open as `role`, oldest first, as one JSON array.
fn sessions_sql(role: &str) -> String {
    format!(
        "SELECT coalesce(json_agg(json_build_object(\
         'client_addr', host(client_addr), \
         'application_name', application_name, \
         'backend_start', backend_start) ORDER BY backend_start), '[]') \
         FROM pg_stat_activity WHERE usename = {} AND backend_type = 'client backend'",
        quote_literal(role)
    )
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The sessions open as `request.name` on the branch's compute, oldest first. The compute's
    /// state is read first: one that isn't running answers `compute_running: false`, with no
    /// SQL and no start.
    pub async fn role_sessions_reply(&self, request: &RoleRequest) -> RoleSessionsReply {
        let branch = request.branch_id;
        if check_any_role_name(&request.name).is_err() {
            return RoleSessionsReply::failed("invalid role name");
        }
        let record = match self.metadata.get_branch(branch).await {
            Ok(record) => record,
            Err(e) => {
                warn!(branch_id = %branch, "reading the branch for role sessions: {e}");
                return RoleSessionsReply::failed(format!("reading the branch: {e}"));
            }
        };
        let Some(ep) = record
            .filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
            .and_then(|b| b.endpoint_id)
        else {
            return RoleSessionsReply::not_ready();
        };
        let database = match self.metadata.neon_databases(&ep).await {
            Ok(dbs) => match dbs.into_iter().next() {
                Some(db) => db.name,
                None => return RoleSessionsReply::not_ready(),
            },
            Err(e) => {
                warn!(branch_id = %branch, "reading the endpoint's database for role sessions: {e}");
                return RoleSessionsReply::failed(format!("reading the endpoint's database: {e}"));
            }
        };
        match self.computes.status(&ep).await {
            Ok(Some(ComputeStatus::Running)) => {}
            Ok(_) => return RoleSessionsReply::not_running(),
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "reading the compute's state: {e:#}");
                return RoleSessionsReply::failed(format!("reading the compute's state: {e:#}"));
            }
        }
        let opts = SqlOpts {
            timeout: SESSIONS_TIMEOUT,
            ..SqlOpts::admin_default()
        };
        let out = match self
            .computes
            .admin_sql_with(&ep, &database, &sessions_sql(&request.name), &opts)
            .await
        {
            Ok(out) => out,
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "reading role sessions: {e:#}");
                return RoleSessionsReply::failed(format!("reading the sessions: {e:#}"));
            }
        };
        match serde_json::from_str::<Vec<RoleSession>>(out.trim()) {
            Ok(sessions) => RoleSessionsReply::running(sessions),
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "unreadable role sessions: {e}");
                RoleSessionsReply::failed(format!("unreadable sessions: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::BranchStatus;
    use chelabase_nats::RoleSession;
    use chelabase_types::BranchId;

    use super::*;
    use crate::fakes::{entries, ready_database, worker};

    fn ask(branch: BranchId, role: &str) -> RoleRequest {
        RoleRequest {
            branch_id: branch,
            name: role.into(),
        }
    }

    #[tokio::test]
    async fn reads_the_roles_sessions_on_a_running_compute() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        w.computes.sql.lock().unwrap().clear();
        w.computes.answer(
            "pg_stat_activity",
            r#"[{"client_addr":"10.0.0.7","application_name":"api","backend_start":"2026-10-08T10:00:00.5+00:00"},{"client_addr":null,"application_name":"","backend_start":"2026-10-08T11:00:00+00:00"}]"#,
        );

        let reply = w.role_sessions_reply(&ask(main, "it's")).await;

        assert_eq!(
            reply,
            RoleSessionsReply::running(vec![
                RoleSession {
                    client_addr: Some("10.0.0.7".into()),
                    application_name: "api".into(),
                    backend_start: "2026-10-08T10:00:00.5Z".parse().unwrap(),
                },
                RoleSession {
                    client_addr: None,
                    application_name: String::new(),
                    backend_start: "2026-10-08T11:00:00Z".parse().unwrap(),
                },
            ])
        );
        let sql = w.computes.sql.lock().unwrap().clone();
        assert_eq!(sql.len(), 1, "{sql:?}");
        let (on, database, text) = &sql[0];
        assert_eq!((on.as_str(), database.as_str()), (ep.as_str(), "app"));
        assert!(text.contains("FROM pg_stat_activity"), "{text}");
        assert!(
            text.contains("usename = 'it''s'"),
            "the name is a quoted literal: {text}"
        );
        assert!(text.contains("backend_type = 'client backend'"), "{text}");
        assert!(text.contains("ORDER BY backend_start"), "{text}");
        let opts = w.computes.sql_opts.lock().unwrap().last().cloned().unwrap();
        assert_eq!(opts.user, None, "the admin role reads every session");
    }

    #[tokio::test]
    async fn no_sessions_is_an_empty_list() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        w.computes.answer("pg_stat_activity", "[]");
        let reply = w.role_sessions_reply(&ask(BranchId::from(id), "app")).await;
        assert_eq!(reply, RoleSessionsReply::running(Vec::new()));
    }

    #[tokio::test]
    async fn a_stopped_or_missing_compute_is_not_read_and_not_started() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        let before = entries(&journal).len();

        w.computes.set_running(&ep, false);
        let reply = w.role_sessions_reply(&ask(BranchId::from(id), "app")).await;
        assert_eq!(reply, RoleSessionsReply::not_running());

        w.computes.remove(&ep);
        let reply = w.role_sessions_reply(&ask(BranchId::from(id), "app")).await;
        assert_eq!(reply, RoleSessionsReply::not_running());

        let after = entries(&journal)[before..].to_vec();
        assert!(
            after.iter().all(|e| !e.starts_with("compute.")),
            "no SQL and no create: {after:?}"
        );
    }

    #[tokio::test]
    async fn an_unready_or_unknown_branch_is_not_ready() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        let before = entries(&journal).len();
        let reply = w.role_sessions_reply(&ask(BranchId::new(), "app")).await;
        assert_eq!(reply, RoleSessionsReply::not_ready());
        w.metadata.state().branches.get_mut(&main).unwrap().status = BranchStatus::Resetting;
        let reply = w.role_sessions_reply(&ask(main, "app")).await;
        assert_eq!(reply, RoleSessionsReply::not_ready());
        let after = entries(&journal)[before..].to_vec();
        assert!(
            after.iter().all(|e| !e.starts_with("compute.")),
            "{after:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_or_unreadable_read_fails() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("pg_stat_activity".into(), "boom".into()));
        let reply = w.role_sessions_reply(&ask(BranchId::from(id), "app")).await;
        assert_eq!(reply.code.as_deref(), Some("failed"));
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        w.computes.answer("pg_stat_activity", "not json");
        let reply = w.role_sessions_reply(&ask(BranchId::from(id), "app")).await;
        assert_eq!(reply.code.as_deref(), Some("failed"));
    }

    #[tokio::test]
    async fn an_invalid_name_is_never_sent() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let before = entries(&journal).len();
        for name in ["", "a\0b", &"x".repeat(64)] {
            let reply = w.role_sessions_reply(&ask(BranchId::from(id), name)).await;
            assert_eq!(reply.code.as_deref(), Some("failed"), "{name:?}");
        }
        let after = entries(&journal)[before..].to_vec();
        assert!(
            after.iter().all(|e| !e.starts_with("compute.sql")),
            "{after:?}"
        );
    }
}
