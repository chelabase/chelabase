//! The socket protocol (spec §2 as amended by R9, R19, P11, S16, S17): JSON text frames tagged
//! by `"type"`, `ref` always a string, key strings everywhere (S11). Every frame type lives
//! here; T8 adds none.

use chelabase_nats::rt::ProjectEvent;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The one subprotocol (`Sec-WebSocket-Protocol`), required and echoed; a version only, never a
/// credential.
pub const SUBPROTOCOL: &str = "chelabase.realtime.v1";

/// Inbound frames over this size are refused (spec §2, §8.1).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// R19: at most this many keys in one `view` frame.
pub const MAX_VIEW_KEYS: usize = 1000;

/// Close codes (spec §2).
pub mod close {
    /// The OIDC token expired without a renewal, or no valid `auth` frame came first.
    pub const TOKEN_EXPIRED: u16 = 4001;
    /// Access revoked, or a renewal for another person or org.
    pub const REVOKED: u16 = 4003;
    /// No pong in time.
    pub const PONG_TIMEOUT: u16 = 4008;
    /// A socket-level limit (sockets per member).
    pub const LIMIT: u16 = 4029;
    /// An inbound frame over [`super::MAX_FRAME_BYTES`] (RFC 6455 "message too big").
    pub const TOO_BIG: u16 = 1009;
    /// The service is restarting (SIGTERM); reconnect with backoff.
    pub const RESTARTING: u16 = 1012;
}

/// A client → server frame.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientFrame {
    /// The member's OIDC access token: first frame of a Studio socket (R9), then each renewal.
    Auth {
        token: String,
    },
    /// `channel` is `"table"` (with `params`, P11/S17) or `"project:<uuid>"` (no params).
    Join {
        r#ref: String,
        channel: String,
        params: Option<Value>,
    },
    Leave {
        r#ref: String,
        channel: String,
    },
    /// `state` is checked by the channel kind (`presence::TableState::parse` for tables).
    Presence {
        channel: String,
        state: Value,
    },
    /// R19: the keys on this socket's screen (replaces the previous set).
    View {
        channel: String,
        keys: Vec<String>,
    },
}

/// Why an inbound frame was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// Over [`MAX_FRAME_BYTES`].
    TooLarge,
    /// A `type` this protocol doesn't have (RT2's `publish` included).
    UnknownType(String),
    /// Not JSON, no `type`, a missing or mistyped field (a numeric `ref`), or an unknown field.
    Invalid(String),
}

// One shape per frame type; `type` is read first, then the whole frame is checked against its
// type's shape with unknown fields refused.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthIn {
    #[serde(rename = "type")]
    _type: String,
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinIn {
    #[serde(rename = "type")]
    _type: String,
    r#ref: String,
    channel: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaveIn {
    #[serde(rename = "type")]
    _type: String,
    r#ref: String,
    channel: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PresenceIn {
    #[serde(rename = "type")]
    _type: String,
    channel: String,
    state: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewIn {
    #[serde(rename = "type")]
    _type: String,
    channel: String,
    keys: Vec<String>,
}

fn shape<T: DeserializeOwned>(v: Value, ty: &str) -> Result<T, FrameError> {
    // serde's message names fields and types, and may quote a value it didn't expect (`invalid
    // type: string "…"`): the frame's own content, sent back to its own sender in the `error`
    // frame only, never logged.
    serde_json::from_value(v).map_err(|e| FrameError::Invalid(format!("{ty}: {e}")))
}

/// Parses one text frame. Unknown fields are refused.
pub fn parse_client_frame(text: &str) -> Result<ClientFrame, FrameError> {
    if text.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge);
    }
    let value: Value = serde_json::from_str(text)
        .map_err(|_| FrameError::Invalid("the frame isn't JSON".into()))?;
    let Some(ty) = value
        .as_object()
        .and_then(|o| o.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(FrameError::Invalid(
            "a frame is a JSON object with a string type".into(),
        ));
    };
    match ty.as_str() {
        "auth" => {
            let f: AuthIn = shape(value, "auth")?;
            Ok(ClientFrame::Auth { token: f.token })
        }
        "join" => {
            let f: JoinIn = shape(value, "join")?;
            Ok(ClientFrame::Join {
                r#ref: f.r#ref,
                channel: f.channel,
                params: f.params.filter(|p| !p.is_null()),
            })
        }
        "leave" => {
            let f: LeaveIn = shape(value, "leave")?;
            Ok(ClientFrame::Leave {
                r#ref: f.r#ref,
                channel: f.channel,
            })
        }
        "presence" => {
            let f: PresenceIn = shape(value, "presence")?;
            Ok(ClientFrame::Presence {
                channel: f.channel,
                state: f.state,
            })
        }
        "view" => {
            let f: ViewIn = shape(value, "view")?;
            Ok(ClientFrame::View {
                channel: f.channel,
                keys: f.keys,
            })
        }
        _ => Err(FrameError::UnknownType(ty)),
    }
}

/// `error` codes (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Forbidden,
    NotFound,
    Limit,
    Invalid,
    Unavailable,
}

/// `status` reasons (spec §2, S17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusReason {
    NoSlot,
    Waiting,
    NoConnect,
    ReadOnly,
    NotATable,
    Unavailable,
    ComputeStopping,
    Reconnecting,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
    Changed,
}

/// One change in a `rows` frame: `keys` are key strings (S11); absent for a keyless delete and
/// for `changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub op: ChangeOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
}

/// A table member's presence state; `row` is a key string. The default is the blanked state
/// `{row: null, column: null, editing: false}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableState {
    pub row: Option<String>,
    pub column: Option<String>,
    pub editing: bool,
}

/// A Studio member on a `table:` channel (spec §5.1). `id` is per socket and channel; `role` is
/// the act-as role (`null` = the owner). `name`, `email` and `role` serialize as `null` when
/// absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub sub: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub role: Option<String>,
    pub color_seed: u32,
    pub state: TableState,
}

/// `color_seed`: a hash of `sub`, stable across tabs, tables and instances.
pub fn color_seed(sub: &str) -> u32 {
    let digest = Sha256::digest(sub.as_bytes());
    u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]])
}

/// `joined.info`: a table's `{pk, live, reason?}` (`pk: null` without a primary key), a
/// project's `{}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum JoinedInfo {
    Table {
        pk: Option<Vec<String>>,
        live: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<StatusReason>,
    },
    Project {},
}

/// A server → client frame.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Joined {
        r#ref: String,
        channel: String,
        info: JoinedInfo,
    },
    /// `ref` and `channel` are left out when absent.
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        code: ErrorCode,
        message: String,
    },
    /// `seq` counts per channel per socket from 1 (S17).
    Rows {
        channel: String,
        seq: u64,
        changes: Vec<Change>,
    },
    PresenceState {
        channel: String,
        members: Vec<Member>,
    },
    /// An update is a leave plus a join of the same `id`.
    PresenceDiff {
        channel: String,
        joins: Vec<Member>,
        leaves: Vec<Member>,
    },
    /// `reason` is left out when absent.
    Status {
        channel: String,
        live: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<StatusReason>,
    },
    /// The event's fields flattened next to `type` and `channel` (S17).
    Project {
        channel: String,
        #[serde(flatten)]
        event: ProjectEvent,
    },
    Resync {
        channel: String,
        reason: String,
    },
}

impl ServerFrame {
    /// The frame as the text sent on the socket.
    pub fn to_text(&self) -> String {
        serde_json::to_string(self).expect("server frames serialize")
    }
}
