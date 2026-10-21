//! RT1 T7: platform events narrowed per database (spec §5.3, R17, R21) and deduplicated per
//! project per second (W34, S28).

use std::time::{Duration, Instant};

use chelabase_nats::rt::ProjectEvent;
use realtime::project::{DEDUP_WINDOW, Delivery, ProjectDedup, ProjectView, event_database, route};
use serde_json::json;
use uuid::Uuid;

fn u(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// Project 10 with databases 20 and 21; project 11 with none.
fn view() -> ProjectView {
    let body = json!({"tenant_id": u(1), "projects": [
        {"project_id": u(10), "database_ids": [u(20), u(21)]},
        {"project_id": u(11), "database_ids": []},
    ]});
    ProjectView::from_json(body.to_string().as_bytes()).unwrap()
}

#[test]
fn the_view_is_the_projects_answer() {
    let v = view();
    assert_eq!(v.tenant_id, u(1));
    assert!(v.has_project(u(10)) && v.has_project(u(11)));
    assert!(!v.has_project(u(12)));
    assert_eq!(v.projects[&u(10)].len(), 2);
    assert!(ProjectView::from_json(b"{}").is_err());
    assert!(ProjectView::from_json(br#"{"tenant_id":"x","projects":[]}"#).is_err());
}

#[test]
fn every_event_names_its_database() {
    let (d, b) = (u(20), u(30));
    for (ev, want) in [
        (
            ProjectEvent::BranchStatus {
                database_id: d,
                branch_id: b,
                status: "ready".into(),
            },
            d,
        ),
        (
            ProjectEvent::BranchCreated {
                database_id: d,
                branch_id: b,
            },
            d,
        ),
        (
            ProjectEvent::BranchDeleted {
                database_id: d,
                branch_id: b,
            },
            d,
        ),
        (ProjectEvent::DatabaseCreated { database_id: d }, d),
        (ProjectEvent::DatabaseDeleted { database_id: d }, d),
        (
            ProjectEvent::PgDatabaseCreated {
                database_id: d,
                branch_id: b,
                name: "x".into(),
            },
            d,
        ),
        (
            ProjectEvent::PgDatabaseDropped {
                database_id: d,
                branch_id: b,
                name: "x".into(),
            },
            d,
        ),
    ] {
        assert_eq!(event_database(&ev), want, "{ev:?}");
    }
}

#[test]
fn events_for_visible_databases_are_delivered() {
    let ev = ProjectEvent::BranchCreated {
        database_id: u(21),
        branch_id: u(30),
    };
    assert_eq!(route(&view(), u(10), &ev, false), Delivery::Deliver);
    let del = ProjectEvent::DatabaseDeleted { database_id: u(20) };
    assert_eq!(route(&view(), u(10), &del, false), Delivery::Deliver);
}

#[test]
fn an_unknown_database_refreshes_the_list_and_is_never_delivered_by_default() {
    // R21: a database the cached list doesn't have (made after the join, or not visible).
    let ev = ProjectEvent::DatabaseCreated { database_id: u(99) };
    assert_eq!(route(&view(), u(10), &ev, false), Delivery::Refresh);
    assert_eq!(
        route(&view(), u(10), &ev, true),
        Delivery::Drop,
        "still missing after the refresh"
    );
    // Another project's database id in this project's event is not this person's to see.
    let other = ProjectEvent::BranchStatus {
        database_id: u(20),
        branch_id: u(30),
        status: "ready".into(),
    };
    assert_eq!(route(&view(), u(11), &other, false), Delivery::Refresh);
    assert_eq!(route(&view(), u(11), &other, true), Delivery::Drop);
}

#[test]
fn a_project_not_in_the_view_drops_everything() {
    let ev = ProjectEvent::DatabaseCreated { database_id: u(20) };
    assert_eq!(route(&view(), u(12), &ev, false), Delivery::Drop);
}

#[test]
fn identical_events_go_out_once_per_second() {
    assert_eq!(DEDUP_WINDOW, Duration::from_secs(1));
    let t0 = Instant::now();
    let ms = Duration::from_millis;
    let mut d = ProjectDedup::new();
    let created = ProjectEvent::PgDatabaseCreated {
        database_id: u(20),
        branch_id: u(30),
        name: "a".into(),
    };
    let dropped = ProjectEvent::PgDatabaseDropped {
        database_id: u(20),
        branch_id: u(30),
        name: "a".into(),
    };
    assert!(d.admit(u(10), &created, t0));
    assert!(
        !d.admit(u(10), &created, t0 + ms(300)),
        "the storm's repeat"
    );
    assert!(
        d.admit(u(10), &dropped, t0 + ms(400)),
        "a distinct event goes out, in arrival order"
    );
    assert!(d.admit(u(11), &created, t0 + ms(400)), "per project");
    assert!(!d.admit(u(10), &dropped, t0 + ms(900)));
    assert!(d.admit(u(10), &created, t0 + ms(1001)), "after the window");
    let other_name = ProjectEvent::PgDatabaseCreated {
        database_id: u(20),
        branch_id: u(30),
        name: "b".into(),
    };
    assert!(
        d.admit(u(10), &other_name, t0 + ms(1002)),
        "other fields: another event"
    );
}
