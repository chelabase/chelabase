//! Members hub T10 fix (user ruling 2026-10-23: preview, then confirm). With a named successor
//! S, a role's `SECURITY DEFINER` routines and non-invoker views granted to someone other than
//! S would run with S's rights after the move. The Admin sees them first:
//! `GET …/roles/{role}/delete-preview?successor=<S>` (Admin, the delete's successor rules,
//! 422 `invalid_successor`) answers `{successor, exposures, confirm}`; the delete takes
//! `?confirm=<token>`, and the worker refuses an unconfirmed one with `exposure_unconfirmed`,
//! a 409 whose `error` carries the current `exposures` and `confirm`. `chelabase_orphaned`
//! never needs a token. The `role_deleted` event's `after` gains `exposures_confirmed`.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_nats::{DeletePreview, Exposure, ExposureKind, Reassigned};

const QUARANTINE: &str = "chelabase_orphaned";
const TOKEN: &str = "6e0b5a0c3f0d4f1f9a3e0c2b7d1e4f5a6b7c8d9e0f1a2b3c4d5e6f708192a3b4";

async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap()
}

fn exposures() -> Vec<Exposure> {
    vec![
        Exposure {
            database: "app".into(),
            kind: ExposureKind::Function,
            identity: "public.planted()".into(),
            grantees: vec!["anon".into()],
        },
        Exposure {
            database: "app".into(),
            kind: ExposureKind::View,
            identity: "public.\"Odd v\"".into(),
            grantees: vec!["PUBLIC".into(), "web".into()],
        },
    ]
}

fn exposures_json() -> Value {
    json!([
        { "database": "app", "kind": "function", "identity": "public.planted()", "grantees": ["anon"] },
        { "database": "app", "kind": "view", "identity": "public.\"Odd v\"", "grantees": ["PUBLIC", "web"] },
    ])
}

fn reply(ok: bool, code: Option<&str>, preview: Option<DeletePreview>) -> RoleReply {
    RoleReply {
        ok,
        code: code.map(Into::into),
        error: code.map(|_| "worker said so".to_string()),
        keys: vec![],
        reassigned: None,
        preview,
    }
}

fn previewed(successor: &str, exposures: Vec<Exposure>, confirm: Option<&str>) -> RoleReply {
    reply(
        true,
        None,
        Some(DeletePreview {
            successor: successor.into(),
            exposures,
            confirm: confirm.map(Into::into),
        }),
    )
}

fn unconfirmed() -> RoleReply {
    reply(
        false,
        Some("exposure_unconfirmed"),
        Some(DeletePreview {
            successor: "heir".into(),
            exposures: exposures(),
            confirm: Some(TOKEN.into()),
        }),
    )
}

fn done_keeping(confirmed: Vec<Exposure>) -> RoleReply {
    RoleReply {
        reassigned: Some(Reassigned {
            count: 3,
            definers: vec!["public.planted()".into()],
            confirmed,
        }),
        ..reply(true, None, None)
    }
}

fn set_reply(w: &MemberWorld, reply: RoleReply) {
    *w.jobs.role_reply.lock().unwrap() = Some(Ok(reply));
}

/// As `role_delete_successor.rs`: the owner's row, an API role `app` and roles to hand to.
async fn world() -> (MemberWorld, String) {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata
        .add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata
        .add_role(&ep, "app", RoleOrigin::Api, false, false);
    w.metadata
        .add_role(&ep, "heir", RoleOrigin::Sql, false, false);
    w.metadata
        .add_role(&ep, "boss", RoleOrigin::Api, true, false);
    w.metadata
        .add_role(&ep, "alice_login", RoleOrigin::Personal, false, false);
    let roles = format!("/v1/databases/{}/branches/{}/roles", w.db, w.dev);
    (w, roles)
}

async fn get(w: &MemberWorld, path: &str) -> reqwest::Response {
    w.app
        .send_as(DEFAULT_SUB, "org-1", Method::GET, path, None)
        .await
}

async fn delete(w: &MemberWorld, path: &str) -> reqwest::Response {
    w.app
        .send_as(DEFAULT_SUB, "org-1", Method::DELETE, path, None)
        .await
}

fn previews(w: &MemberWorld) -> Vec<RoleDeleteRequest> {
    w.jobs.role_delete_previews.lock().unwrap().clone()
}

fn deletes(w: &MemberWorld) -> Vec<RoleDeleteRequest> {
    w.jobs.role_deletes.lock().unwrap().clone()
}

fn role_events(w: &MemberWorld) -> Vec<chelabase_metadata::members::AccessEvent> {
    w.metadata
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.action.starts_with("role_"))
        .cloned()
        .collect()
}

// --- the preview -------------------------------------------------------------------------

// The preview answers the worker's list and token as is, for the successor asked about; the
// worker is asked with the decoded names and no `confirm`. A read: no event.
#[tokio::test]
async fn the_preview_lists_the_exposures_and_the_token() {
    let (w, roles) = world().await;
    set_reply(&w, previewed("heir", exposures(), Some(TOKEN)));

    let resp = get(&w, &format!("{roles}/app/delete-preview?successor=heir")).await;

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "successor": "heir", "exposures": exposures_json(), "confirm": TOKEN })
    );
    let sent = previews(&w);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        (
            sent[0].name.as_str(),
            sent[0].successor.as_str(),
            sent[0].confirm.as_deref()
        ),
        ("app", "heir", None)
    );
    assert_eq!(sent[0].branch_id, w.dev);
    assert!(deletes(&w).is_empty(), "a preview never deletes");
    assert!(role_events(&w).is_empty());
}

// Nothing exposed: an empty list and a null token.
#[tokio::test]
async fn an_empty_preview_has_no_token() {
    let (w, roles) = world().await;
    set_reply(&w, previewed("heir", vec![], None));
    let resp = get(&w, &format!("{roles}/app/delete-preview?successor=heir")).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "successor": "heir", "exposures": [], "confirm": null })
    );
}

// Omitted, the successor is the quarantine role, as on the delete; its preview still lists
// what it found, with no token (it never needs one).
#[tokio::test]
async fn the_preview_defaults_to_the_quarantine_role() {
    let (w, roles) = world().await;
    set_reply(&w, previewed(QUARANTINE, exposures(), None));
    let resp = get(&w, &format!("{roles}/app/delete-preview")).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "successor": QUARANTINE, "exposures": exposures_json(), "confirm": null })
    );
    assert_eq!(previews(&w)[0].successor, QUARANTINE);
}

// The delete's successor rules, before the worker is asked: 422 `invalid_successor`.
#[tokio::test]
async fn the_preview_refuses_what_the_delete_refuses() {
    let (w, roles) = world().await;
    for successor in [
        "app",
        "postgres",
        "anon",
        "pg_monitor",
        "CHELABASE_ORPHANED",
        "cb_m_x",
        "boss",
        "alice_login",
        "",
    ] {
        let path = format!("{roles}/app/delete-preview?successor={successor}");
        let err = error_of(get(&w, &path).await, 422).await;
        assert_eq!(code_of(&err), "invalid_successor", "{successor:?}");
    }
    assert!(
        w.jobs.role_calls.lock().unwrap().is_empty(),
        "{:?}",
        w.jobs.role_calls.lock().unwrap()
    );
    // The owner itself, and a malformed query.
    let err = error_of(
        get(
            &w,
            &format!("{roles}/postgres/delete-preview?successor=heir"),
        )
        .await,
        422,
    )
    .await;
    assert_eq!(code_of(&err), "reserved_role_name");
    let err = error_of(
        get(
            &w,
            &format!("{roles}/app/delete-preview?successor=heir&successor=heir"),
        )
        .await,
        422,
    )
    .await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(previews(&w).is_empty());
}

// The worker's refusals map as the delete's do.
#[tokio::test]
async fn the_workers_preview_refusals_map_as_the_deletes() {
    let (w, roles) = world().await;
    let path = format!("{roles}/app/delete-preview?successor=heir");
    for (code, status) in [
        ("invalid_successor", 422),
        ("not_found", 404),
        ("branch_not_ready", 409),
    ] {
        set_reply(&w, reply(false, Some(code), None));
        let err = error_of(get(&w, &path).await, status).await;
        assert_eq!(code_of(&err), code, "{code}");
        assert!(!err.to_string().contains("worker said so"), "{err}");
    }
    *w.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Transport("down".into())));
    assert_eq!(get(&w, &path).await.status(), 503);
    assert!(role_events(&w).is_empty());
}

// Admin, as the delete: a Developer or Viewer of the project is refused, the worker never asked.
#[tokio::test]
async fn the_preview_needs_admin() {
    let (w, roles) = world().await;
    w.metadata.add_granted(
        w.tenant,
        "dev-user",
        w.project,
        PlatformRole::Developer,
        &[],
    );
    w.metadata.add_granted(
        w.tenant,
        "viewer-user",
        w.project,
        PlatformRole::Viewer,
        &[],
    );
    let path = format!("{roles}/app/delete-preview?successor=heir");
    for sub in ["dev-user", "viewer-user"] {
        let resp = w.app.send_as(sub, "org-1", Method::GET, &path, None).await;
        assert_eq!(resp.status(), 403, "{sub}");
    }
    assert!(previews(&w).is_empty());
    // The tenant admin (an Admin of every project) may.
    assert_eq!(get(&w, &path).await.status(), 200);
}

// --- the delete with a token -------------------------------------------------------------

// No token while the worker finds exposures: 409 `exposure_unconfirmed`, with the current
// list and token in `error`; nothing logged.
#[tokio::test]
async fn a_delete_without_the_token_is_409_with_the_current_exposures() {
    let (w, roles) = world().await;
    set_reply(&w, unconfirmed());

    let err = error_of(
        delete(&w, &format!("{roles}/app?successor=heir")).await,
        409,
    )
    .await;

    assert_eq!(code_of(&err), "exposure_unconfirmed", "{err}");
    assert_eq!(err["error"]["exposures"], exposures_json(), "{err}");
    assert_eq!(err["error"]["confirm"], TOKEN, "{err}");
    assert!(!err.to_string().contains("worker said so"), "{err}");
    assert_eq!(deletes(&w)[0].confirm, None);
    assert!(role_events(&w).is_empty());
}

// A stale token: the worker recomputed and refused, the 409 carries the new token.
#[tokio::test]
async fn a_stale_token_reaches_the_worker_and_is_refused() {
    let (w, roles) = world().await;
    set_reply(&w, unconfirmed());
    let stale = "0".repeat(64);
    let err = error_of(
        delete(&w, &format!("{roles}/app?successor=heir&confirm={stale}")).await,
        409,
    )
    .await;
    assert_eq!(code_of(&err), "exposure_unconfirmed");
    assert_eq!(err["error"]["confirm"], TOKEN);
    assert_eq!(deletes(&w)[0].confirm.as_deref(), Some(stale.as_str()));
}

// The preview's token: passed to the worker as is; the delete is done and its event counts
// what was kept on confirmation.
#[tokio::test]
async fn the_token_reaches_the_worker_and_the_event_counts_the_kept_exposures() {
    let (w, roles) = world().await;
    set_reply(&w, done_keeping(exposures()));

    let resp = delete(&w, &format!("{roles}/app?successor=heir&confirm={TOKEN}")).await;

    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["successor"], "heir", "{body}");
    assert_eq!(body["reassigned"]["count"], 3, "{body}");
    let sent = deletes(&w);
    assert_eq!(
        (sent[0].successor.as_str(), sent[0].confirm.as_deref()),
        ("heir", Some(TOKEN))
    );
    let events = role_events(&w);
    assert_eq!(events.len(), 1, "{events:?}");
    let after = events[0].after.as_ref().unwrap();
    assert_eq!(after["exposures_confirmed"], 2, "{after}");
    assert_eq!(after["successor"], "heir", "{after}");
    assert_eq!(after["applied"], true, "{after}");
}

// Nothing kept on confirmation (none found, or the quarantine role): the event says 0. The
// quarantine role is sent without a token, and a done delete needs none.
#[tokio::test]
async fn the_quarantine_role_needs_no_token_and_counts_zero() {
    let (w, roles) = world().await;
    set_reply(&w, done_keeping(vec![]));
    assert_eq!(delete(&w, &format!("{roles}/app")).await.status(), 200);
    assert_eq!(
        delete(&w, &format!("{roles}/app?successor={QUARANTINE}"))
            .await
            .status(),
        200
    );
    assert_eq!(
        delete(&w, &format!("{roles}/app?successor=heir"))
            .await
            .status(),
        200
    );
    let sent: Vec<(String, Option<String>)> = deletes(&w)
        .into_iter()
        .map(|r| (r.successor, r.confirm))
        .collect();
    assert_eq!(
        sent,
        [
            (QUARANTINE.to_string(), None),
            (QUARANTINE.to_string(), None),
            ("heir".to_string(), None)
        ]
    );
    let counts: Vec<Value> = role_events(&w)
        .iter()
        .map(|e| e.after.as_ref().unwrap()["exposures_confirmed"].clone())
        .collect();
    assert_eq!(counts, [json!(0), json!(0), json!(0)]);
}

// A token named twice is a malformed query, refused before the worker.
#[tokio::test]
async fn a_token_named_twice_is_refused() {
    let (w, roles) = world().await;
    let resp = delete(
        &w,
        &format!("{roles}/app?successor=heir&confirm={TOKEN}&confirm={TOKEN}"),
    )
    .await;
    let err = error_of(resp, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(deletes(&w).is_empty());
}
