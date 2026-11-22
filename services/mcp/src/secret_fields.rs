//! Which answer fields hold a credential, and which request fields may take one.
//!
//! Two checked-in tables, keyed by `operationId`, filled from the handlers and their response
//! types (not from field names):
//!
//! - [`CREDENTIAL_FIELDS`]: the locations in an operation's success answer that hold a
//!   credential, each with its [`Kind`]. Epic `mcp-sealed` replaces each with a sealed handle
//!   before the sandbox sees the answer; m11 refuses what it can't seal (fail closed).
//! - [`SECRET_INPUTS`]: the request-body locations that are write-only credential inputs: the
//!   API stores them and never answers them. A handle may be passed there and nowhere else.
//!
//! Locations are JSON pointers in which `*` is every item of an array and `{}` every value of
//! a map ([`lookup`] applies them). Two more tables account for every other field the tests
//! find in `openapi/mcp.json`: [`ALLOWED`] (named like a credential, shown not to be one) and
//! [`UNTYPED`] (answers, or parts of them, the document gives no schema for).
//!
//! **Residual, not sealed:** an answer that returns the member's own data (`consoleQuery`,
//! `runSql`, `insertDefaultRow`'s row, object contents, a service's logs, SQL text in a merge
//! plan) may hold anything the member stored, credentials included. Those are classified
//! [`Carries::MemberData`]; nothing in them is sealed.

use serde_json::Value;

/// A JSON pointer: `/field`, `*` for each array item, `{}` for each map value.
pub type Pointer = &'static str;

/// What a credential is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A data API key (`chk_sec_…`, or `chk_pub_…` when just made).
    DataKey,
    /// A Postgres password.
    Password,
    /// A Postgres URI with the password in it.
    ConnectionString,
    /// A service's signing secret.
    SigningSecret,
    /// A storage webhook's signing secret (`whsec_…`).
    WebhookSecret,
    /// An `/accept-invite` link carrying the invite's code and token.
    InviteLink,
    /// A storage URL carrying a signed token.
    SignedUrl,
}

/// The credential locations of each operation's success answers (every success status).
pub static CREDENTIAL_FIELDS: &[(&str, &[(Pointer, Kind)])] = &[
    // `ConnectionDetails` (orchestrator lib.rs `reveal`): the role's password and the
    // URI that embeds it; both lack it when `password_state` isn't `generated`.
    (
        "getDatabaseConnection",
        &[
            ("/password", Kind::Password),
            ("/uri", Kind::ConnectionString),
        ],
    ),
    (
        "getBranchConnection",
        &[
            ("/password", Kind::Password),
            ("/uri", Kind::ConnectionString),
        ],
    ),
    // `CreatedDataKey` (data_keys.rs): the plaintext key, shown once.
    ("createKey", &[("/key", Kind::DataKey)]),
    ("rotateKey", &[("/key", Kind::DataKey)]),
    // `PersonalLoginSecret` (personal_logins.rs): the generated password, answered once.
    (
        "createPersonalLogin",
        &[
            ("/password", Kind::Password),
            ("/connection_string", Kind::ConnectionString),
        ],
    ),
    (
        "resetPersonalLoginPassword",
        &[
            ("/password", Kind::Password),
            ("/connection_string", Kind::ConnectionString),
        ],
    ),
    // `FunctionView.signing_secret` (functions.rs): set when the function becomes a service
    // (`putFunction`) or with `rotate_signing_secret` (`updateFunction`).
    ("putFunction", &[("/signing_secret", Kind::SigningSecret)]),
    (
        "updateFunction",
        &[("/signing_secret", Kind::SigningSecret)],
    ),
    // `StorageWebhookWithSecret` (storage_webhooks.rs): shown at creation and rotation only.
    ("createStorageWebhook", &[("/secret", Kind::WebhookSecret)]),
    (
        "rotateStorageWebhookSecret",
        &[("/secret", Kind::WebhookSecret)],
    ),
    // `InviteResult` / `ResentInvite` (members.rs `invite_link`): with `delivery: "link"`, the
    // link with the code and token; absent when the invite was mailed.
    ("createInvite", &[("/invite_url", Kind::InviteLink)]),
    ("createProjectInvite", &[("/invite_url", Kind::InviteLink)]),
    ("resendInvite", &[("/invite_url", Kind::InviteLink)]),
    // Storage's answers, passed through unchanged (storage_console.rs → storage signed.rs
    // `sign`, `resolve`): a signed download URL; `urls` holds one per ref, `null` for a ref
    // that can't be read and the plain public URL for a public bucket.
    ("storageSignObject", &[("/signed_url", Kind::SignedUrl)]),
    ("storageResolveObjects", &[("/urls/*", Kind::SignedUrl)]),
];

/// The request-body locations that take a credential the API stores and never answers.
///
/// Not here, because a later read answers them in clear: a storage webhook's `url`, a service
/// spec's `url`. The role routes take no password (the API generates them).
pub static SECRET_INPUTS: &[(&str, &[Pointer])] = &[
    // `PutSecretRequest.value` (functions.rs): `listFunctionSecrets` answers names only.
    // Not truly write-only: the value reaches the function's environment, and the same caller
    // can deploy and run that function's code, which can read it back. Whether to keep it
    // here is decided in epic `mcp-sealed` (#378).
    ("putFunctionSecret", &["/value"]),
    // `CreateAppUserRequest.password` (app_user_admin.rs): the identity service keeps a hash.
    ("createAppUser", &["/password"]),
];

/// The credential locations in `operation_id`'s success answers; empty for any other.
pub fn credential_fields(operation_id: &str) -> &'static [(Pointer, Kind)] {
    CREDENTIAL_FIELDS
        .iter()
        .find(|(id, _)| *id == operation_id)
        .map_or(&[], |(_, fields)| fields)
}

/// The request-body locations of `operation_id` a handle may be passed in.
pub fn secret_inputs(operation_id: &str) -> &'static [Pointer] {
    SECRET_INPUTS
        .iter()
        .find(|(id, _)| *id == operation_id)
        .map_or(&[], |(_, pointers)| pointers)
}

/// Whether `operation_id` can answer with a credential.
pub fn answers_with_credential(operation_id: &str) -> bool {
    !credential_fields(operation_id).is_empty()
}

/// Answer fields named or described like a credential that are not one.
pub struct Allowed {
    /// (operationId, pointer) pairs.
    pub at: &'static [(&'static str, Pointer)],
    pub reason: &'static str,
    /// Where the answer is made, which shows it.
    pub handler: &'static str,
}

/// Every credential-looking answer field that isn't in [`CREDENTIAL_FIELDS`].
pub static ALLOWED: &[Allowed] = &[
    Allowed {
        at: &[
            ("listRoles", "/*/password"),
            ("createRole", "/password"),
            ("updateRole", "/password"),
        ],
        reason: "a `PasswordState` enum (generated, set_in_sql, …): a state, never a password; \
                 the role routes generate passwords and reveal them only through the connection routes",
        handler: "services/orchestrator/src/roles.rs (`Role.password: PasswordState`)",
    },
    Allowed {
        at: &[
            ("getDatabaseConnection", "/password_state"),
            ("getBranchConnection", "/password_state"),
        ],
        reason: "a `PasswordState` enum saying why `password` is null",
        handler: "services/orchestrator/src/lib.rs (`ConnectionDetails`)",
    },
    Allowed {
        at: &[
            ("getDatabaseConnection", "/http_url"),
            ("getBranchConnection", "/http_url"),
        ],
        reason: "`https://<host>:<port>/sql`, built from the proxy host alone: no user or password",
        handler: "services/orchestrator/src/lib.rs (`reveal`)",
    },
    Allowed {
        at: &[
            ("getDatabaseConnection", "/ca_file"),
            ("getBranchConnection", "/ca_file"),
        ],
        reason: "the path of the dev proxy's CA certificate file (public), or null",
        handler: "services/orchestrator/src/lib.rs (`ConnectionDetails.ca_file`)",
    },
    Allowed {
        at: &[("listKeys", "/*/key"), ("updateKey", "/key")],
        reason: "only the public key (`chk_pub_`, meant to ship in clients) carries a value; a \
                 secret key's is never stored, so `key` is absent for it",
        handler: "services/orchestrator/src/data_keys.rs (`summary`, `DataKey.key`)",
    },
    Allowed {
        at: &[
            ("listKeys", "/*/databases/*"),
            ("updateKey", "/databases/*"),
            ("createKey", "/databases/*"),
            ("rotateKey", "/databases/*"),
        ],
        reason: "Postgres database names the key may reach",
        handler: "services/orchestrator/src/data_keys.rs (`DataKey.databases`)",
    },
    Allowed {
        at: &[
            ("listKeys", "/*/managed_by_function"),
            ("updateKey", "/managed_by_function"),
        ],
        reason: "the name of the function whose run-as key this is",
        handler: "services/orchestrator/src/data_keys.rs (`DataKey.managed_by_function`)",
    },
    Allowed {
        at: &[("listFunctions", "/*/signing_secret")],
        reason: "always absent in list answers: the shared view sets it to None and only \
                 `putFunction` (becoming a service) and `updateFunction` (rotate) fill it in",
        handler: "services/orchestrator/src/functions.rs (`views`, `signing_secret: None`)",
    },
    Allowed {
        at: &[
            ("listFunctions", "/*/auth"),
            ("putFunction", "/auth"),
            ("updateFunction", "/auth"),
        ],
        reason: "an enum (key, app_user, public) naming who may call the function",
        handler: "services/orchestrator/src/functions.rs (`Auth`)",
    },
    Allowed {
        at: &[
            ("listFunctions", "/*/run_as_role"),
            ("putFunction", "/run_as_role"),
            ("updateFunction", "/run_as_role"),
        ],
        reason: "a Postgres role name; the run-as key itself is never answered",
        handler: "services/orchestrator/src/functions.rs (`FunctionView.run_as_role`)",
    },
    Allowed {
        at: &[
            ("listStorageWebhooks", "/*/url"),
            ("getStorageWebhook", "/url"),
            ("updateStorageWebhook", "/url"),
            ("createStorageWebhook", "/webhook/url"),
            ("rotateStorageWebhookSecret", "/webhook/url"),
        ],
        reason: "the endpoint's URL; `valid_url` refuses one with credentials in it",
        handler: "services/orchestrator/src/storage_webhooks.rs (`valid_url`)",
    },
    Allowed {
        at: &[
            ("getBranchReview", "/items/*/key"),
            ("acknowledgeBranchReview", "/items/*/key"),
            ("fixBranchReviewItem", "/review/items/*/key"),
        ],
        reason: "a review item's identifier, `<database>/<kind>/<schema>.<name>(<identity>)`",
        handler: "crates/chelabase-types/src/protection.rs (`ReviewItem.key`)",
    },
    Allowed {
        at: &[
            ("getConsoleSchema", "/schemas/{}/tables/{}/primary_key/*"),
            ("getDataApiSchema", "/schemas/{}/tables/{}/primary_key/*"),
        ],
        reason: "column names of a table's primary key",
        handler: "crates/chelabase-types/src/data_api_schema.rs",
    },
    Allowed {
        at: &[("getTable", "/indexes/*/keys/*")],
        reason: "an index's key columns or expressions",
        handler: "services/orchestrator/src/table_api/read.rs (crates/chelabase-types/src/table_api.rs)",
    },
    Allowed {
        at: &[("listAppUsers", "/next_page_token")],
        reason: "a page cursor; it grants nothing (the caller's own access still decides)",
        handler: "services/orchestrator/src/app_user_admin.rs (`AppUserPage.next_page_token`)",
    },
    Allowed {
        at: &[("previewRoleDelete", "/confirm")],
        reason: "a digest of the successor and the exposures (`exposure_token`), recomputed by \
                 the worker; it proves the preview was seen and grants nothing",
        handler: "services/orchestrator/src/roles.rs (services/worker/src/roles.rs; crates/chelabase-nats `exposure_token`)",
    },
    Allowed {
        at: &[
            ("abandonMerge", "/acknowledgements/*"),
            ("updateMerge", "/acknowledgements/*"),
            ("getMerge", "/merge/acknowledgements/*"),
        ],
        reason: "ids of plan items whose data loss or security change was accepted",
        handler: "services/orchestrator/src/merges.rs (`MergeView.acknowledgements`)",
    },
    Allowed {
        at: &[("deleteRole", "/reassigned/definers/*")],
        reason: "`schema.name(argument types)` of SECURITY DEFINER routines that moved",
        handler: "services/orchestrator/src/roles.rs (`delete_role`)",
    },
    Allowed {
        at: &[("listRoleSessions", "/sessions/*/client_addr")],
        reason: "a client's IP address",
        handler: "services/orchestrator/src/roles.rs (`list_role_sessions`)",
    },
    Allowed {
        at: &[
            ("createRole", "/level"),
            ("listRoles", "/*/level"),
            ("updateRole", "/level"),
        ],
        reason: "an enum of the role's privilege level",
        handler: "services/orchestrator/src/roles.rs (`Role.level`)",
    },
    Allowed {
        at: &[
            ("createPersonalLogin", "/login/valid_until"),
            ("resetPersonalLoginPassword", "/login/valid_until"),
            ("listDatabasePersonalLogins", "/logins/*/valid_until"),
            ("listMemberPersonalLogins", "/logins/*/valid_until"),
            ("createRole", "/valid_until"),
            ("listRoles", "/*/valid_until"),
            ("updateRole", "/valid_until"),
        ],
        reason: "a timestamp: when the password stops working",
        handler: "services/orchestrator/src/personal_logins.rs (`PersonalLoginView`), roles.rs (`Role`)",
    },
    Allowed {
        at: &[
            ("approveMember", "/accepted_at"),
            ("createInvite", "/member/accepted_at"),
            ("createProjectInvite", "/member/accepted_at"),
            ("listMembers", "/*/accepted_at"),
            ("reinstateMember", "/accepted_at"),
            ("suspendMember", "/accepted_at"),
            ("updateMember", "/accepted_at"),
            ("approveMember", "/created_at"),
            ("createInvite", "/member/created_at"),
            ("createProjectInvite", "/member/created_at"),
            ("listMembers", "/*/created_at"),
            ("reinstateMember", "/created_at"),
            ("suspendMember", "/created_at"),
            ("updateMember", "/created_at"),
        ],
        reason: "timestamps of a member's invite and acceptance",
        handler: "services/orchestrator/src/members.rs (`Member`)",
    },
    Allowed {
        at: &[
            ("approveMember", "/display_name"),
            ("createInvite", "/member/display_name"),
            ("createProjectInvite", "/member/display_name"),
            ("listMembers", "/*/display_name"),
            ("reinstateMember", "/display_name"),
            ("suspendMember", "/display_name"),
            ("updateMember", "/display_name"),
        ],
        reason: "the member's display name",
        handler: "services/orchestrator/src/members.rs (`Member`)",
    },
    Allowed {
        at: &[
            ("approveMember", "/status"),
            ("createInvite", "/member/status"),
            ("createProjectInvite", "/member/status"),
            ("getMe", "/member/status"),
            ("listMembers", "/*/status"),
            ("reinstateMember", "/status"),
            ("suspendMember", "/status"),
            ("updateMember", "/status"),
        ],
        reason: "an enum of the member's state (invited, pending, active, …)",
        handler: "services/orchestrator/src/members.rs (`Member.status`)",
    },
    Allowed {
        at: &[
            ("createInvite", "/outcome"),
            ("createProjectInvite", "/outcome"),
        ],
        reason: "an enum of what the invite did",
        handler: "services/orchestrator/src/members.rs (`InviteOutcome`)",
    },
    Allowed {
        at: &[("getMe", "/email"), ("getMe", "/name")],
        reason: "the caller's own email and name, from the member row or the token's claims; \
                 never the token",
        handler: "services/orchestrator/src/lib.rs (`get_me`)",
    },
    Allowed {
        at: &[
            ("listAccessEvents", "/events/*/action"),
            ("listProjectAccessEvents", "/events/*/action"),
        ],
        reason: "the event's action name (`invite_created`, `personal_login_reset`, …)",
        handler: "services/orchestrator/src/members.rs (`AccessEvent.action`)",
    },
];

/// What an answer with no schema in the document carries, read from its handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carries {
    /// Its credentials are in [`CREDENTIAL_FIELDS`].
    Listed,
    /// No credential, from this operation.
    Nothing,
    /// The member's own data, which may hold anything (the residual; nothing sealed).
    MemberData,
}

/// An answer, or a part of one, the document gives no schema for.
pub struct Untyped {
    pub operation: &'static str,
    /// The success answer's media type.
    pub media: &'static str,
    /// `""` for the whole answer.
    pub at: Pointer,
    pub carries: Carries,
    /// Where the answer is made, which shows what it carries.
    pub handler: &'static str,
}

const JSON: &str = "application/json";

const fn untyped(operation: &'static str, carries: Carries, handler: &'static str) -> Untyped {
    Untyped {
        operation,
        media: JSON,
        at: "",
        carries,
        handler,
    }
}

const fn part(
    operation: &'static str,
    at: Pointer,
    carries: Carries,
    handler: &'static str,
) -> Untyped {
    Untyped {
        operation,
        media: JSON,
        at,
        carries,
        handler,
    }
}

const STORAGE_BUCKETS: &str = "storage_console.rs proxies services/storage/src/buckets.rs unchanged: \
     `storage.buckets` rows (id, name, public, limits, mime types, write_once, created_at)";
const STORAGE_OBJECTS: &str = "storage_console.rs proxies services/storage/src/objects.rs unchanged: \
     bucket, object names, version ids, sizes, etags, content types";
const STORAGE_UPLOADS: &str = "storage_console.rs proxies services/storage/src/multipart.rs unchanged: \
     the session's id (usable only by its owner), bucket, name, expiry, parts' sizes and etags";
const MERGE: &str = "services/orchestrator/src/merges.rs (`MergeView`): the worker's analysis, options, \
     plan steps, rehearsal and result; plan SQL never carries a password (chelabase-schema plan \
     and export tests); routine bodies in it are the member's own text";

/// Every untyped answer, or part of one, and what its handler shows it carries. Routes the
/// policy table excludes (the binary uploads, `createFunctionDeployment`) are not in the
/// bundle and never answer a sandbox, so they have no entry.
pub static UNTYPED: &[Untyped] = &[
    // Storage's console routes: storage's own answers, passed through unchanged.
    untyped("storageListBuckets", Carries::Nothing, STORAGE_BUCKETS),
    untyped("storageCreateBucket", Carries::Nothing, STORAGE_BUCKETS),
    untyped("storageUpdateBucket", Carries::Nothing, STORAGE_BUCKETS),
    untyped("storageDeleteBucket", Carries::Nothing, STORAGE_BUCKETS),
    untyped("storageListObjects", Carries::Nothing, STORAGE_OBJECTS),
    untyped("storageCopyObject", Carries::Nothing, STORAGE_OBJECTS),
    untyped("storageMoveObject", Carries::Nothing, STORAGE_OBJECTS),
    untyped("storageDeleteObject", Carries::Nothing, STORAGE_OBJECTS),
    untyped("storageDeleteObjects", Carries::Nothing, STORAGE_OBJECTS),
    untyped("storageListParts", Carries::Nothing, STORAGE_UPLOADS),
    untyped("storageAbortUpload", Carries::Nothing, STORAGE_UPLOADS),
    untyped(
        "storageSignObject",
        Carries::Listed,
        "storage_console.rs proxies services/storage/src/signed.rs `sign`: `signed_url`, `expires_at`",
    ),
    untyped(
        "storageResolveObjects",
        Carries::Listed,
        "storage_console.rs proxies services/storage/src/signed.rs `resolve`: `urls`, `expires_at`",
    ),
    Untyped {
        operation: "storageDownloadObject",
        media: "application/octet-stream",
        at: "",
        carries: Carries::MemberData,
        handler: "storage_console.rs proxies services/storage/src/objects.rs `download`: the object's bytes",
    },
    // The member's own data.
    untyped(
        "consoleQuery",
        Carries::MemberData,
        "services/orchestrator/src/console.rs (`console_query`): the data API's rows and results",
    ),
    Untyped {
        operation: "runSql",
        media: "application/x-ndjson",
        at: "",
        carries: Carries::MemberData,
        handler: "services/orchestrator/src/sql_editor/routes.rs (`run_sql`): the run's events and rows",
    },
    part(
        "insertDefaultRow",
        "/row",
        Carries::MemberData,
        "services/orchestrator/src/table_api/rows.rs: the inserted row",
    ),
    Untyped {
        operation: "getServiceLogs",
        media: "text/plain",
        at: "",
        carries: Carries::MemberData,
        handler: "services/orchestrator/src/functions.rs (`service_logs`): the service's own output",
    },
    // Generated from the catalog or the plan; no credential.
    untyped(
        "getDataApiTypes",
        Carries::Nothing,
        "services/orchestrator/src/data_api_types.rs: the `types` map generated from the schema",
    ),
    Untyped {
        operation: "getDataApiTypes",
        media: "text/typescript",
        at: "",
        carries: Carries::Nothing,
        handler: "services/orchestrator/src/data_api_types.rs: the TypeScript module generated from the schema",
    },
    untyped(
        "getSqlSchema",
        Carries::Nothing,
        "services/orchestrator/src/sql_editor/routes.rs (`get_sql_schema`): postgres-language-server's \
         catalog introspection (sql_editor/pgls; roles from `pg_roles`, which hides passwords)",
    ),
    untyped(
        "cancelSqlRun",
        Carries::Nothing,
        "services/orchestrator/src/sql_editor/routes.rs (`cancel_sql_run`): always `{}`",
    ),
    Untyped {
        operation: "exportMergePlan",
        media: "text/plain",
        at: "",
        carries: Carries::Nothing,
        handler: "services/orchestrator/src/merges.rs: the plan as SQL, roles as comments, never passwords \
                  (crates/chelabase-schema/src/export.rs test)",
    },
    part("getMerge", "/merge/analysis", Carries::Nothing, MERGE),
    part("getMerge", "/merge/options", Carries::Nothing, MERGE),
    part("getMerge", "/merge/plan", Carries::Nothing, MERGE),
    part("getMerge", "/merge/rehearsal", Carries::Nothing, MERGE),
    part("getMerge", "/merge/result", Carries::Nothing, MERGE),
    part("getMerge", "/events/*/detail", Carries::Nothing, MERGE),
    part("updateMerge", "/analysis", Carries::Nothing, MERGE),
    part("updateMerge", "/options", Carries::Nothing, MERGE),
    part("updateMerge", "/plan", Carries::Nothing, MERGE),
    part("updateMerge", "/rehearsal", Carries::Nothing, MERGE),
    part("updateMerge", "/result", Carries::Nothing, MERGE),
    part("abandonMerge", "/analysis", Carries::Nothing, MERGE),
    part("abandonMerge", "/options", Carries::Nothing, MERGE),
    part("abandonMerge", "/plan", Carries::Nothing, MERGE),
    part("abandonMerge", "/rehearsal", Carries::Nothing, MERGE),
    part("abandonMerge", "/result", Carries::Nothing, MERGE),
    // Access events: statuses, grants, emails, expiry times, role changes; never a password,
    // code or token.
    part(
        "listAccessEvents",
        "/events/*/before",
        Carries::Nothing,
        "crates/chelabase-metadata/src/members.rs, personal_logins.rs, teams.rs (event writers); \
         services/orchestrator/src/roles.rs `log_role_event` (\"never a password\")",
    ),
    part(
        "listAccessEvents",
        "/events/*/after",
        Carries::Nothing,
        "crates/chelabase-metadata/src/members.rs, personal_logins.rs, teams.rs (event writers); \
         services/orchestrator/src/roles.rs `log_role_event` (\"never a password\")",
    ),
    part(
        "listProjectAccessEvents",
        "/events/*/before",
        Carries::Nothing,
        "crates/chelabase-metadata/src/members.rs, personal_logins.rs, teams.rs (event writers); \
         services/orchestrator/src/roles.rs `log_role_event` (\"never a password\")",
    ),
    part(
        "listProjectAccessEvents",
        "/events/*/after",
        Carries::Nothing,
        "crates/chelabase-metadata/src/members.rs, personal_logins.rs, teams.rs (event writers); \
         services/orchestrator/src/roles.rs `log_role_event` (\"never a password\")",
    ),
    // A service's spec: `{backend, url, health_path}` or `{backend, image, port, resources}`.
    part(
        "listFunctionDeployments",
        "/*/spec",
        Carries::Nothing,
        "services/orchestrator/src/functions.rs (`ServiceSpec`): the URL is refused with credentials",
    ),
];

/// A string found at a credential location.
#[derive(Debug, PartialEq, Eq)]
pub struct Found<'a> {
    /// Its concrete RFC 6901 pointer, for `Value::pointer` and `pointer_mut`.
    pub pointer: String,
    pub value: &'a str,
}

/// The answer's shape doesn't match the pointer: an answer to withhold.
#[derive(Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// Where, as a concrete pointer.
    pub at: String,
    /// What the pointer needed there: `array`, `object`, `string` or `pointer`.
    pub expected: &'static str,
}

/// Every present, non-null string at `pointer` in `answer`.
///
/// `null` and missing fields (or items) yield nothing; anything else that doesn't match
/// (an object where an array is expected, a number where the string is) is a [`Mismatch`].
pub fn lookup<'a>(answer: &'a Value, pointer: &str) -> Result<Vec<Found<'a>>, Mismatch> {
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(Mismatch {
            at: String::new(),
            expected: "pointer",
        });
    };
    let segments: Vec<&str> = rest.split('/').collect();
    let mut out = vec![];
    walk(answer, &segments, String::new(), &mut out)?;
    Ok(out)
}

fn walk<'a>(
    node: &'a Value,
    segments: &[&str],
    at: String,
    out: &mut Vec<Found<'a>>,
) -> Result<(), Mismatch> {
    if node.is_null() {
        return Ok(());
    }
    let Some((segment, rest)) = segments.split_first() else {
        return match node.as_str() {
            Some(value) => {
                out.push(Found { pointer: at, value });
                Ok(())
            }
            None => Err(Mismatch {
                at,
                expected: "string",
            }),
        };
    };
    match *segment {
        "*" => {
            let Some(items) = node.as_array() else {
                return Err(Mismatch {
                    at,
                    expected: "array",
                });
            };
            for (i, item) in items.iter().enumerate() {
                walk(item, rest, format!("{at}/{i}"), out)?;
            }
        }
        "{}" => {
            let Some(map) = node.as_object() else {
                return Err(Mismatch {
                    at,
                    expected: "object",
                });
            };
            for (key, value) in map {
                walk(value, rest, format!("{at}/{}", escape(key)), out)?;
            }
        }
        field => {
            let Some(map) = node.as_object() else {
                return Err(Mismatch {
                    at,
                    expected: "object",
                });
            };
            if let Some(value) = map.get(field) {
                walk(value, rest, format!("{at}/{}", escape(field)), out)?;
            }
        }
    }
    Ok(())
}

fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values<'a>(answer: &'a Value, pointer: &str) -> Vec<&'a str> {
        lookup(answer, pointer)
            .unwrap()
            .into_iter()
            .map(|f| f.value)
            .collect()
    }

    #[test]
    fn pointer_with_star_yields_present_strings() {
        assert!(values(&json!([]), "/*/password").is_empty());
        assert_eq!(values(&json!([{"password": "a"}]), "/*/password"), ["a"]);
        let many = json!([{"password": "a"}, {"password": "b"}, {"password": "c"}]);
        assert_eq!(values(&many, "/*/password"), ["a", "b", "c"]);
        let nested = json!({"review": {"items": [{"key": "k1"}, {"key": "k2"}]}});
        assert_eq!(values(&nested, "/review/items/*/key"), ["k1", "k2"]);
        assert_eq!(
            values(&json!({"urls": ["u1", null, "u3"]}), "/urls/*"),
            ["u1", "u3"]
        );
        assert_eq!(values(&json!({"secret": "s"}), "/secret"), ["s"]);
        // The concrete pointer of each value, as `Value::pointer` takes it.
        let found = lookup(&many, "/*/password").unwrap();
        let pointers: Vec<&str> = found.iter().map(|f| f.pointer.as_str()).collect();
        assert_eq!(pointers, ["/0/password", "/1/password", "/2/password"]);
        for f in &found {
            assert_eq!(
                many.pointer(&f.pointer).and_then(Value::as_str),
                Some(f.value)
            );
        }
        // Any key of a map, its key escaped as RFC 6901 says.
        let map = json!({"m": {"a/b": {"t": "x"}, "c~d": {"t": "y"}}});
        let found = lookup(&map, "/m/{}/t").unwrap();
        let pointers: Vec<&str> = found.iter().map(|f| f.pointer.as_str()).collect();
        assert_eq!(pointers, ["/m/a~1b/t", "/m/c~0d/t"]);
        for f in &found {
            assert_eq!(
                map.pointer(&f.pointer).and_then(Value::as_str),
                Some(f.value)
            );
        }
    }

    #[test]
    fn shape_mismatch_is_reported() {
        // An object where an array is expected.
        let err = lookup(&json!({"password": "a"}), "/*/password").unwrap_err();
        assert_eq!(
            err,
            Mismatch {
                at: String::new(),
                expected: "array"
            }
        );
        let err = lookup(&json!({"urls": {"0": "u"}}), "/urls/*").unwrap_err();
        assert_eq!(
            err,
            Mismatch {
                at: "/urls".into(),
                expected: "array"
            }
        );
        // An array where an object is expected.
        let err = lookup(&json!([{"password": "a"}]), "/password").unwrap_err();
        assert_eq!(
            err,
            Mismatch {
                at: String::new(),
                expected: "object"
            }
        );
        // A scalar on the way.
        assert!(lookup(&json!({"review": "x"}), "/review/items/*/key").is_err());
        assert!(lookup(&json!({"m": []}), "/m/{}/t").is_err());
        // Anything but a string (or null) where the credential sits.
        for leaf in [json!(1), json!(true), json!({"v": "a"}), json!(["a"])] {
            let err = lookup(&json!({"secret": leaf}), "/secret").unwrap_err();
            assert_eq!(
                err,
                Mismatch {
                    at: "/secret".into(),
                    expected: "string"
                }
            );
        }
        // One bad item spoils the answer, even after good ones.
        let mixed = json!([{"password": "a"}, {"password": 7}]);
        let err = lookup(&mixed, "/*/password").unwrap_err();
        assert_eq!(err.at, "/1/password");
        // A pointer that isn't one.
        assert!(lookup(&json!({"a": "b"}), "a").is_err());
    }

    #[test]
    fn null_and_missing_yield_nothing() {
        assert!(values(&json!({"password": null}), "/password").is_empty());
        assert!(values(&json!({}), "/password").is_empty());
        assert!(values(&json!({"urls": null}), "/urls/*").is_empty());
        assert!(values(&json!(null), "/*/password").is_empty());
        let items = json!([{"password": null}, {}, {"password": "a"}, null]);
        assert_eq!(values(&items, "/*/password"), ["a"]);
        assert!(values(&json!({"review": null}), "/review/items/*/key").is_empty());
        assert!(values(&json!({"review": {}}), "/review/items/*/key").is_empty());
    }
}
