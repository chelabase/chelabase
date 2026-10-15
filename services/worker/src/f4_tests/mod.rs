//! F4 RED tests (fork round 3, final fix round): the worker's side of T9 I2-I4, m1-m6, U11,
//! U14 and T11 m4. One file per area; the shared helpers are here.

mod audit;
mod certs_sweep;
mod config_ca;
mod member_matrix;
mod protection_flag;
mod settings_version;
mod wait_loop;

use chelabase_types::BranchId;

use crate::fakes::{FakeWorker, ready_database, worker};

pub(crate) fn h(s: &str) -> String {
    hex::encode(s.as_bytes())
}

/// A ready database's main branch with its endpoint; the compute's SQL log is cleared.
pub(crate) async fn main_branch(w: &FakeWorker) -> (BranchId, String) {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    let ep = main
        .endpoint_id
        .clone()
        .expect("a ready branch has an endpoint");
    w.computes.sql.lock().unwrap().clear();
    (main.id, ep)
}

pub(crate) fn set_protected(w: &FakeWorker, branch: BranchId, protected: bool) {
    w.metadata
        .state()
        .branches
        .get_mut(&branch)
        .unwrap()
        .protected = protected;
}

/// A fresh worker with a ready database, its main branch and endpoint.
pub(crate) async fn fresh() -> (FakeWorker, crate::fakes::Journal, BranchId, String) {
    let (w, journal) = worker();
    let (branch, ep) = main_branch(&w).await;
    (w, journal, branch, ep)
}

/// Every script the compute ran, `(database, text)`, in order.
pub(crate) fn sql_log(w: &FakeWorker) -> Vec<(String, String)> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, db, s)| (db.clone(), s.clone()))
        .collect()
}
