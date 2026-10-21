//! RT1 T7: frame parsing and validation, the server frames' JSON shapes and the close codes
//! (spec §2, R19, S16, S17).

use chelabase_nats::rt::ProjectEvent;
use realtime::protocol::{
    Change, ChangeOp, ClientFrame, ErrorCode, FrameError, JoinedInfo, MAX_FRAME_BYTES,
    MAX_VIEW_KEYS, Member, SUBPROTOCOL, ServerFrame, StatusReason, TableState, close, color_seed,
    parse_client_frame,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn frame(v: Value) -> Result<ClientFrame, FrameError> {
    parse_client_frame(&v.to_string())
}

fn json_of(f: &ServerFrame) -> Value {
    serde_json::from_str(&f.to_text()).expect("a server frame is JSON")
}

#[test]
fn auth_join_leave_presence_and_view_parse() {
    assert_eq!(
        frame(json!({"type": "auth", "token": "t-1"})),
        Ok(ClientFrame::Auth {
            token: "t-1".into()
        })
    );
    let params = json!({"database_id": Uuid::nil(), "branch_id": Uuid::nil(), "pg_database": "app",
        "schema": "public", "table": "items", "role": null});
    assert_eq!(
        frame(json!({"type": "join", "ref": "1", "channel": "table", "params": params})),
        Ok(ClientFrame::Join {
            r#ref: "1".into(),
            channel: "table".into(),
            params: Some(params.clone())
        })
    );
    assert_eq!(
        frame(
            json!({"type": "join", "ref": "2", "channel": "project:00000000-0000-4000-8000-000000000002"})
        ),
        Ok(ClientFrame::Join {
            r#ref: "2".into(),
            channel: "project:00000000-0000-4000-8000-000000000002".into(),
            params: None
        })
    );
    assert_eq!(
        frame(json!({"type": "leave", "ref": "3", "channel": "table:[]"})),
        Ok(ClientFrame::Leave {
            r#ref: "3".into(),
            channel: "table:[]".into()
        })
    );
    let state = json!({"row": "[\"1\"]", "column": "name", "editing": true});
    assert_eq!(
        frame(json!({"type": "presence", "channel": "c", "state": state})),
        Ok(ClientFrame::Presence {
            channel: "c".into(),
            state
        })
    );
    assert_eq!(
        frame(json!({"type": "view", "channel": "c", "keys": ["[\"1\"]", "[\"2\"]"]})),
        Ok(ClientFrame::View {
            channel: "c".into(),
            keys: vec!["[\"1\"]".into(), "[\"2\"]".into()]
        })
    );
}

#[test]
fn ref_must_be_a_string() {
    for r in [json!(1), json!(null), json!(["1"])] {
        assert!(
            matches!(
                frame(json!({"type": "join", "ref": r, "channel": "project:x"})),
                Err(FrameError::Invalid(_))
            ),
            "ref {r} is refused"
        );
    }
    assert!(
        matches!(
            frame(json!({"type": "join", "channel": "project:x"})),
            Err(FrameError::Invalid(_))
        ),
        "a join without a ref is refused"
    );
    assert!(matches!(
        frame(json!({"type": "leave", "ref": 7, "channel": "c"})),
        Err(FrameError::Invalid(_))
    ));
}

#[test]
fn unknown_types_are_named_and_rt2_frames_are_unknown() {
    assert_eq!(
        frame(json!({"type": "subscribe"})),
        Err(FrameError::UnknownType("subscribe".into()))
    );
    assert_eq!(
        frame(
            json!({"type": "publish", "ref": "1", "channel": "app:x", "event": "e", "payload": {}})
        ),
        Err(FrameError::UnknownType("publish".into()))
    );
}

#[test]
fn malformed_frames_are_invalid() {
    for text in [
        "",
        "not json",
        "[]",
        "42",
        "{}",
        r#"{"type": 3}"#,
        r#"{"type":"auth"}"#,
        r#"{"type":"auth","token":5}"#,
    ] {
        assert!(
            matches!(parse_client_frame(text), Err(FrameError::Invalid(_))),
            "{text:?} is invalid"
        );
    }
}

#[test]
fn unknown_fields_are_refused() {
    assert!(matches!(
        frame(json!({"type": "auth", "token": "t", "extra": 1})),
        Err(FrameError::Invalid(_))
    ));
    assert!(matches!(
        frame(json!({"type": "view", "channel": "c", "keys": [], "sub": "someone"})),
        Err(FrameError::Invalid(_))
    ));
}

#[test]
fn view_keys_are_strings() {
    assert!(matches!(
        frame(json!({"type": "view", "channel": "c", "keys": [1, 2]})),
        Err(FrameError::Invalid(_))
    ));
    assert!(matches!(
        frame(json!({"type": "view", "channel": "c", "keys": [["1"]]})),
        Err(FrameError::Invalid(_))
    ));
}

/// A text frame of exactly `len` bytes: an auth frame padded in its token.
fn auth_of_len(len: usize) -> String {
    let empty = r#"{"type":"auth","token":""}"#;
    let token = "x".repeat(len - empty.len());
    let text = format!(r#"{{"type":"auth","token":"{token}"}}"#);
    assert_eq!(text.len(), len);
    text
}

#[test]
fn frames_over_64_kb_are_too_large() {
    assert_eq!(MAX_FRAME_BYTES, 65_536);
    assert!(matches!(
        parse_client_frame(&auth_of_len(MAX_FRAME_BYTES)),
        Ok(ClientFrame::Auth { .. })
    ));
    assert_eq!(
        parse_client_frame(&auth_of_len(MAX_FRAME_BYTES + 1)),
        Err(FrameError::TooLarge)
    );
    // Size is checked before JSON: an oversized non-JSON frame is too large, not invalid.
    assert_eq!(
        parse_client_frame(&"x".repeat(MAX_FRAME_BYTES + 1)),
        Err(FrameError::TooLarge)
    );
}

#[test]
fn constants() {
    assert_eq!(SUBPROTOCOL, "chelabase.realtime.v1");
    assert_eq!(MAX_VIEW_KEYS, 1000);
}

#[test]
fn close_codes() {
    assert_eq!(close::TOKEN_EXPIRED, 4001);
    assert_eq!(close::REVOKED, 4003);
    assert_eq!(close::PONG_TIMEOUT, 4008);
    assert_eq!(close::LIMIT, 4029);
    assert_eq!(close::TOO_BIG, 1009);
    assert_eq!(close::RESTARTING, 1012);
}

#[test]
fn joined_frames() {
    let table = ServerFrame::Joined {
        r#ref: "1".into(),
        channel: "table:[]".into(),
        info: JoinedInfo::Table {
            pk: Some(vec!["id".into()]),
            live: true,
            reason: None,
        },
    };
    assert_eq!(
        json_of(&table),
        json!({"type": "joined", "ref": "1", "channel": "table:[]", "info": {"pk": ["id"], "live": true}})
    );
    let paused = ServerFrame::Joined {
        r#ref: "2".into(),
        channel: "table:[]".into(),
        info: JoinedInfo::Table {
            pk: None,
            live: false,
            reason: Some(StatusReason::NoSlot),
        },
    };
    assert_eq!(
        json_of(&paused)["info"],
        json!({"pk": null, "live": false, "reason": "no_slot"}),
        "no primary key is pk: null, never absent"
    );
    let project = ServerFrame::Joined {
        r#ref: "3".into(),
        channel: "project:x".into(),
        info: JoinedInfo::Project {},
    };
    assert_eq!(json_of(&project)["info"], json!({}));
}

#[test]
fn error_frames_leave_out_absent_ref_and_channel() {
    let full = ServerFrame::Error {
        r#ref: Some("1".into()),
        channel: Some("c".into()),
        code: ErrorCode::Forbidden,
        message: "no".into(),
    };
    assert_eq!(
        json_of(&full),
        json!({"type": "error", "ref": "1", "channel": "c", "code": "forbidden", "message": "no"})
    );
    let bare = ServerFrame::Error {
        r#ref: None,
        channel: None,
        code: ErrorCode::Invalid,
        message: "bad frame".into(),
    };
    assert_eq!(
        json_of(&bare),
        json!({"type": "error", "code": "invalid", "message": "bad frame"})
    );
    for (code, text) in [
        (ErrorCode::Forbidden, "forbidden"),
        (ErrorCode::NotFound, "not_found"),
        (ErrorCode::Limit, "limit"),
        (ErrorCode::Invalid, "invalid"),
        (ErrorCode::Unavailable, "unavailable"),
    ] {
        assert_eq!(serde_json::to_value(code).unwrap(), json!(text));
    }
}

#[test]
fn rows_frames_carry_key_strings_and_omit_absent_keys() {
    let rows = ServerFrame::Rows {
        channel: "c".into(),
        seq: 1,
        changes: vec![
            Change {
                op: ChangeOp::Insert,
                keys: Some(vec!["[\"1\"]".into()]),
            },
            Change {
                op: ChangeOp::Update,
                keys: Some(vec!["[\"2\",\"x\"]".into()]),
            },
            Change {
                op: ChangeOp::Delete,
                keys: None,
            },
            Change {
                op: ChangeOp::Changed,
                keys: None,
            },
        ],
    };
    assert_eq!(
        json_of(&rows),
        json!({"type": "rows", "channel": "c", "seq": 1, "changes": [
            {"op": "insert", "keys": ["[\"1\"]"]},
            {"op": "update", "keys": ["[\"2\",\"x\"]"]},
            {"op": "delete"},
            {"op": "changed"},
        ]})
    );
}

fn member() -> Member {
    Member {
        id: "m-1".into(),
        sub: "user-1".into(),
        name: None,
        email: Some("ana@example.com".into()),
        role: None,
        color_seed: 7,
        state: TableState {
            row: Some("[\"1\"]".into()),
            column: None,
            editing: false,
        },
    }
}

#[test]
fn presence_frames_show_every_member_field() {
    let state = ServerFrame::PresenceState {
        channel: "c".into(),
        members: vec![member()],
    };
    assert_eq!(
        json_of(&state),
        json!({"type": "presence_state", "channel": "c", "members": [{
            "id": "m-1", "sub": "user-1", "name": null, "email": "ana@example.com", "role": null,
            "color_seed": 7, "state": {"row": "[\"1\"]", "column": null, "editing": false}
        }]})
    );
    let diff = ServerFrame::PresenceDiff {
        channel: "c".into(),
        joins: vec![],
        leaves: vec![member()],
    };
    let v = json_of(&diff);
    assert_eq!(v["type"], "presence_diff");
    assert_eq!(v["joins"], json!([]));
    assert_eq!(v["leaves"][0]["id"], "m-1");
    assert_eq!(
        TableState::default(),
        TableState {
            row: None,
            column: None,
            editing: false
        }
    );
}

#[test]
fn status_frames_and_every_reason() {
    let live = ServerFrame::Status {
        channel: "c".into(),
        live: true,
        reason: None,
    };
    assert_eq!(
        json_of(&live),
        json!({"type": "status", "channel": "c", "live": true})
    );
    let stopping = ServerFrame::Status {
        channel: "c".into(),
        live: false,
        reason: Some(StatusReason::ComputeStopping),
    };
    assert_eq!(
        json_of(&stopping),
        json!({"type": "status", "channel": "c", "live": false, "reason": "compute_stopping"})
    );
    for (reason, text) in [
        (StatusReason::NoSlot, "no_slot"),
        (StatusReason::Waiting, "waiting"),
        (StatusReason::NoConnect, "no_connect"),
        (StatusReason::ReadOnly, "read_only"),
        (StatusReason::NotATable, "not_a_table"),
        (StatusReason::Unavailable, "unavailable"),
        (StatusReason::ComputeStopping, "compute_stopping"),
        (StatusReason::Reconnecting, "reconnecting"),
        (StatusReason::Unsupported, "unsupported"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(text));
    }
}

#[test]
fn project_frames_flatten_the_event() {
    let (d, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
    let f = ServerFrame::Project {
        channel: "project:p".into(),
        event: ProjectEvent::BranchStatus {
            database_id: d,
            branch_id: b,
            status: "ready".into(),
        },
    };
    assert_eq!(
        json_of(&f),
        json!({"type": "project", "channel": "project:p", "event": "branch.status",
            "database_id": d, "branch_id": b, "status": "ready"})
    );
    let f = ServerFrame::Project {
        channel: "project:p".into(),
        event: ProjectEvent::DatabaseCreated { database_id: d },
    };
    assert_eq!(
        json_of(&f),
        json!({"type": "project", "channel": "project:p", "event": "database.created", "database_id": d})
    );
}

#[test]
fn resync_frames() {
    let f = ServerFrame::Resync {
        channel: "c".into(),
        reason: "gap".into(),
    };
    assert_eq!(
        json_of(&f),
        json!({"type": "resync", "channel": "c", "reason": "gap"})
    );
}

#[test]
fn color_seeds_are_stable_per_sub() {
    assert_eq!(color_seed("user-1"), color_seed("user-1"));
    assert_ne!(color_seed("user-1"), color_seed("user-2"));
    // Spread over the palette: 32 subs don't all land on one of 8 hues.
    let hues: std::collections::BTreeSet<u32> = (0..32)
        .map(|i| color_seed(&format!("sub-{i}")) % 8)
        .collect();
    assert!(hues.len() > 1, "{hues:?}");
}
