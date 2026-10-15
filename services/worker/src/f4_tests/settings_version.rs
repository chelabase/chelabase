//! T11 m4: computes created before the CA existed have no server certificate and aren't on the
//! control network. They carry compute-settings version 2; round 3's settings (TLS, the control
//! network, the protection flag) are version 3, so reconcile recreates them once, as it does for
//! any older version (R11).

use crate::COMPUTE_SETTINGS_VERSION;
use crate::fakes::{entries, position, ready_database, worker};

#[tokio::test]
async fn a_compute_made_before_round_3_is_recreated_once() {
    const _: () = assert!(
        COMPUTE_SETTINGS_VERSION >= 3,
        "round 3 raises the version past 2"
    );
    let (w, journal) = worker();
    let id = ready_database(&w).await;
    let ep = w.metadata.main(id).endpoint_id.unwrap();
    // A new compute records the current version.
    assert_eq!(
        w.metadata.state().endpoints[&ep].compute_settings_version,
        COMPUTE_SETTINGS_VERSION
    );
    w.computes
        .answer("to_regclass('chelabase.schema_version')", "t\n");
    // As round 2 left it.
    w.metadata
        .state()
        .endpoints
        .get_mut(&ep)
        .unwrap()
        .compute_settings_version = 2;
    journal.lock().unwrap().clear();

    w.reconcile_once().await.unwrap();

    let delete = position(&journal, &format!("compute.delete {ep}"));
    let create = position(&journal, &format!("compute.create {ep}"));
    assert!(
        delete.is_some() && create.is_some() && delete < create,
        "recreated: {:?}",
        entries(&journal)
    );
    assert_eq!(
        w.metadata.state().endpoints[&ep].compute_settings_version,
        COMPUTE_SETTINGS_VERSION
    );

    // Once: the next pass leaves it alone.
    journal.lock().unwrap().clear();
    w.reconcile_once().await.unwrap();
    assert!(
        position(&journal, "compute.delete").is_none(),
        "{:?}",
        entries(&journal)
    );
    assert!(position(&journal, "compute.create").is_none());
}
