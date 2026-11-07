//! The schema cache's pure parts: reserved schemas, type spelling, which functions are
//! callable, visibility and the `DataApiSchema` JSON.

use std::collections::{HashMap, HashSet};

use chelabase_query::query::Name;
use chelabase_query::schema::{
    INTROSPECT_SQL, Privileges, RESERVED_SCHEMAS, Raw, Schema, Visibility, is_reserved,
};
use chelabase_types::data_api_schema::TableDesc;
use chelabase_query::sql::{Catalog, FnLookupError};
use serde_json::json;

const PUBLIC: u32 = 2200;
const BILLING: u32 = 30000;
const ORDERS: u32 = 40000;
const SECRETS: u32 = 40001;
const INVOICES: u32 = 40002;

#[allow(clippy::too_many_arguments)]
fn ty(
    oid: u32,
    schema: &str,
    name: &str,
    typtype: &str,
    elem: u32,
    base: u32,
    category: &str,
    len: i16,
    formatted: &str,
) -> (u32, String, String, String, u32, u32, String, i16, String) {
    (
        oid,
        schema.into(),
        name.into(),
        typtype.into(),
        elem,
        base,
        category.into(),
        len,
        formatted.into(),
    )
}

#[allow(clippy::type_complexity)]
fn col(
    name: &str,
    typid: u32,
    formatted: &str,
) -> (String, u32, String, bool, bool, bool, Option<String>, bool) {
    col_with(name, typid, formatted, None, false)
}

/// A nullable column with its comment and whether a foreign key makes it `storage.objects(id)`.
#[allow(clippy::type_complexity)]
fn col_with(
    name: &str,
    typid: u32,
    formatted: &str,
    comment: Option<&str>,
    storage_object: bool,
) -> (String, u32, String, bool, bool, bool, Option<String>, bool) {
    (
        name.into(),
        typid,
        formatted.into(),
        true,
        false,
        false,
        comment.map(Into::into),
        storage_object,
    )
}

#[allow(clippy::type_complexity)]
fn function(
    oid: u32,
    name: &str,
    args: &[(&str, u32)],
    defaults: i16,
    rettype: u32,
) -> (
    u32,
    String,
    String,
    String,
    bool,
    u32,
    Vec<u32>,
    Option<Vec<u32>>,
    Option<Vec<String>>,
    Option<Vec<String>>,
    i16,
    bool,
    bool,
) {
    (
        oid,
        "public".into(),
        name.into(),
        "f".into(),
        false,
        rettype,
        args.iter().map(|(_, t)| *t).collect(),
        None,
        None,
        Some(args.iter().map(|(n, _)| (*n).to_owned()).collect()),
        defaults,
        false,
        false,
    )
}

fn raw() -> Raw {
    let types = vec![
        ty(16, "pg_catalog", "bool", "b", 0, 0, "B", 1, "boolean"),
        ty(23, "pg_catalog", "int4", "b", 0, 0, "N", 4, "integer"),
        ty(
            1007,
            "pg_catalog",
            "_int4",
            "b",
            23,
            0,
            "A",
            -1,
            "integer[]",
        ),
        ty(25, "pg_catalog", "text", "b", 0, 0, "S", -1, "text"),
        ty(1700, "pg_catalog", "numeric", "b", 0, 0, "N", -1, "numeric"),
        ty(2950, "pg_catalog", "uuid", "b", 0, 0, "U", 16, "uuid"),
        ty(2278, "pg_catalog", "void", "p", 0, 0, "P", 4, "void"),
        ty(2279, "pg_catalog", "trigger", "p", 0, 0, "P", 4, "trigger"),
        ty(
            2283,
            "pg_catalog",
            "anyelement",
            "p",
            0,
            0,
            "P",
            4,
            "anyelement",
        ),
        ty(90001, "public", "mood", "e", 0, 0, "E", 4, "mood"),
        ty(90002, "public", "_mood", "b", 90001, 0, "A", -1, "mood[]"),
        ty(90003, "public", "email", "d", 0, 25, "S", -1, "email"),
        ty(
            90004,
            "public",
            "Weird \"T\"",
            "d",
            0,
            1700,
            "N",
            -1,
            "\"Weird \"\"T\"\"\"",
        ),
    ];
    let mut ext = function(70010, "uuid_generate_v4", &[], 0, 2950);
    ext.12 = true;
    let mut variadic = function(70011, "concat_all", &[("parts", 1007)], 0, 25);
    variadic.11 = true;
    let mut procedure = function(70012, "do_it", &[], 0, 2278);
    procedure.3 = "p".into();
    Raw {
        namespaces: vec![
            (PUBLIC, "public".into()),
            (BILLING, "billing".into()),
            (99, "chelabase".into()),
        ],
        types,
        enums: vec![(90001, "sad".into()), (90001, "happy".into())],
        relations: vec![
            (
                ORDERS,
                "public".into(),
                "Order".into(),
                "r".into(),
                false,
                false,
                vec![
                    (
                        "id".into(),
                        23,
                        "integer".into(),
                        false,
                        true,
                        true,
                        None,
                        false,
                    ),
                    col("select", 25, "text"),
                    col("total", 1700, "numeric(10,2)"),
                    col("mood", 90001, "mood"),
                    col("moods", 90002, "mood[]"),
                    col("email", 90003, "email"),
                    col("ids", 1007, "integer[]"),
                    col("weird", 90004, "\"Weird \"\"T\"\"\""),
                ],
                vec![],
            ),
            (
                SECRETS,
                "public".into(),
                "secrets".into(),
                "r".into(),
                true,
                false,
                vec![col("v", 25, "text")],
                vec![],
            ),
            (
                INVOICES,
                "billing".into(),
                "invoices".into(),
                "v".into(),
                false,
                false,
                vec![col("amount", 1700, "numeric")],
                vec![],
            ),
            (
                40003,
                "chelabase".into(),
                "schema_version".into(),
                "r".into(),
                false,
                false,
                vec![col("version", 2950, "uuid")],
                vec![],
            ),
        ],
        functions: vec![
            function(70001, "add_points", &[("user_id", 2950), ("n", 23)], 1, 23),
            function(
                70002,
                "add_points",
                &[("user_id", 2950), ("tag", 25)],
                0,
                23,
            ),
            function(70003, "touch", &[], 0, 2278),
            function(70004, "unnamed", &[("", 23)], 0, 23),
            function(70005, "on_insert", &[], 0, 2279),
            function(70006, "poly", &[("x", 2283)], 0, 2283),
            function(70007, "tag", &[("ids", 1007), ("m", 90001)], 0, 90002),
            ext,
            variadic,
            procedure,
        ],
        app_users: None,
    }
}

fn schema() -> Schema {
    Schema::build("v1".into(), raw())
}

fn everything() -> Visibility {
    let all = Privileges {
        select: true,
        insert: true,
        update: true,
        delete: true,
    };
    Visibility {
        schemas: HashSet::from([PUBLIC, BILLING]),
        tables: HashMap::from([(ORDERS, all), (SECRETS, all), (INVOICES, all)]),
        functions: (70001..=70012).collect(),
    }
}

fn name(s: &str) -> Name {
    let (schema, name) = s.split_once('.').unwrap();
    Name {
        schema: Some(schema.into()),
        name: name.into(),
    }
}

#[test]
fn reserved_schemas_are_one_constant_and_hidden() {
    for s in RESERVED_SCHEMAS {
        assert!(is_reserved(s), "{s}");
    }
    for s in [
        "chelabase",
        "pg_catalog",
        "information_schema",
        "pg_toast",
        "neon",
        "neon_migration",
    ] {
        assert!(RESERVED_SCHEMAS.contains(&s), "{s}");
    }
    assert!(is_reserved("pg_temp_3"));
    assert!(is_reserved("pg_toast_temp_12"));
    for s in ["public", "billing", "pg_cron_x", "Chelabase", "neon2"] {
        assert!(!is_reserved(s), "{s}");
    }
    // Even granted, a reserved schema's tables never reach the catalog or the JSON.
    let s = schema();
    assert!(s.tables.iter().all(|t| t.schema != "chelabase"));
    let mut v = everything();
    v.schemas.insert(99);
    let json = s.to_json("public", &[("anon", &v)], false);
    assert!(json["schemas"].get("chelabase").is_none(), "{json}");
}

#[test]
fn types_are_spelt_for_casts_and_described_for_clients() {
    let s = schema();
    let order = s.tables.iter().find(|t| t.name == "Order").unwrap();
    let col = |n: &str| order.columns.iter().find(|c| c.info.name == n).unwrap();
    // Postgres's own types keep format_type's spelling (with the column's typmod); others
    // are quoted and qualified, so a cast never depends on the search path.
    assert_eq!(col("total").info.type_sql, "numeric(10,2)");
    assert_eq!(col("ids").info.type_sql, "integer[]");
    assert_eq!(col("mood").info.type_sql, "\"public\".\"mood\"");
    assert_eq!(col("moods").info.type_sql, "\"public\".\"mood\"[]");
    assert_eq!(col("email").info.type_sql, "\"public\".\"email\"");
    assert_eq!(col("weird").info.type_sql, "\"public\".\"Weird \"\"T\"\"\"");

    assert!(col("ids").info.array && col("moods").info.array);
    assert!(!col("mood").info.array);
    // like/ilike: text and domains over text.
    assert!(col("select").info.text_like && col("email").info.text_like);
    assert!(!col("total").info.text_like && !col("mood").info.text_like);
    assert!(col("id").info.generated);
    assert!(!col("id").nullable && col("id").has_default);

    // Domains resolve to their base type; arrays to their element; enums name themselves.
    assert_eq!(col("email").ty.name, "text");
    assert_eq!(col("weird").ty.name, "numeric");
    assert_eq!(col("ids").ty.name, "int4");
    assert_eq!(col("moods").ty.enum_ref.as_deref(), Some("public.mood"));
    assert_eq!(col("mood").ty.name, "mood");
}

#[test]
fn only_named_plain_functions_are_callable() {
    let s = schema();
    let mut names: Vec<&str> = s.functions.iter().map(|f| f.info.name.as_str()).collect();
    names.sort_unstable();
    // Hidden: an extension's function, an unnamed argument, a variadic, a procedure, a
    // trigger function and a polymorphic one.
    assert_eq!(names, ["add_points", "add_points", "tag", "touch"]);
    let touch = s.functions.iter().find(|f| f.info.name == "touch").unwrap();
    assert!(touch.info.returns_void && !touch.info.returns_set);
    let tag = s.functions.iter().find(|f| f.info.name == "tag").unwrap();
    assert!(!tag.info.returns_void);
    assert_eq!(
        tag.info.args,
        vec![
            ("ids".into(), "integer[]".into()),
            ("m".into(), "\"public\".\"mood\"".into())
        ]
    );
}

#[test]
fn function_lookup_by_argument_names() {
    let view = schema().view(everything());
    let f = |n: &str, args: &[&str]| view.function(&name(n), args).map(|f| f.args.len());
    // add_points(user_id, n default 1) and add_points(user_id, tag).
    assert_eq!(f("public.add_points", &["n", "user_id"]), Ok(2));
    assert_eq!(f("public.add_points", &["tag", "user_id"]), Ok(2));
    assert_eq!(
        f("public.add_points", &["user_id"]),
        Ok(2),
        "only the first overload can leave out its defaulted argument"
    );
    assert_eq!(f("public.add_points", &[]), Err(FnLookupError::NotFound));
    assert_eq!(
        f("public.add_points", &["x", "user_id"]),
        Err(FnLookupError::NotFound)
    );
    assert_eq!(f("public.touch", &[]), Ok(0));
    assert_eq!(
        f("public.uuid_generate_v4", &[]),
        Err(FnLookupError::NotFound)
    );
    assert_eq!(f("billing.touch", &[]), Err(FnLookupError::NotFound));
}

#[test]
fn ambiguous_overloads_are_reported() {
    let mut r = raw();
    // add_points(user_id, n default 1) and add_points(user_id, k default 2): `user_id`
    // alone fits both.
    r.functions.push(function(
        70020,
        "add_points",
        &[("user_id", 2950), ("k", 23)],
        1,
        23,
    ));
    let mut v = everything();
    v.functions.insert(70020);
    let view = Schema::build("v".into(), r).view(v);
    assert_eq!(
        view.function(&name("public.add_points"), &["user_id"])
            .err(),
        Some(FnLookupError::Ambiguous)
    );
}

#[test]
fn unreachable_tables_and_functions_are_not_in_the_catalog() {
    let s = schema();
    let mut v = everything();
    // No privilege at all on `secrets`, and no EXECUTE on `touch`.
    v.tables.insert(SECRETS, Privileges::default());
    v.functions.remove(&70003);
    // Only a column-level SELECT on billing.invoices still reaches it.
    v.tables.insert(
        INVOICES,
        Privileges {
            select: true,
            ..Default::default()
        },
    );
    let view = s.view(v.clone());
    assert!(view.table(&name("public.Order")).is_some());
    assert!(
        view.table(&name("public.order")).is_none(),
        "no case folding"
    );
    assert!(view.table(&name("public.secrets")).is_none());
    assert!(view.table(&name("billing.invoices")).is_some());
    assert!(view.function(&name("public.touch"), &[]).is_err());

    // A table whose schema the role has no USAGE on isn't listed by the visibility query
    // at all.
    v.tables.remove(&INVOICES);
    v.schemas.remove(&BILLING);
    let view = s.view(v);
    assert!(view.table(&name("billing.invoices")).is_none());
}

#[test]
fn visibility_reads_its_json() {
    let v = Visibility::from_json(
        r#"{"schemas":[2200],"tables":[[40000,true,false,false,false],[40001,false,false,false,false]],"functions":[70001]}"#,
    )
    .unwrap();
    assert_eq!(v.schemas, HashSet::from([PUBLIC]));
    assert_eq!(v.table(ORDERS).unwrap().names(), ["select"]);
    assert_eq!(v.table(SECRETS), None);
    assert!(v.functions.contains(&70001));
}

#[test]
fn schema_json_for_a_role_and_for_studio() {
    let s = schema();
    let anon = Visibility {
        schemas: HashSet::from([PUBLIC]),
        tables: HashMap::from([(
            ORDERS,
            Privileges {
                select: true,
                ..Default::default()
            },
        )]),
        functions: HashSet::new(),
    };
    let json = s.to_json("public", &[("anon", &anon)], false);
    assert_eq!(json["version"], "v1");
    assert_eq!(
        json["schemas"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["public"]
    );
    let public = &json["schemas"]["public"];
    assert_eq!(
        public["tables"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["Order"]
    );
    let order = &public["tables"]["Order"];
    assert_eq!(order["kind"], "table");
    assert_eq!(order["rls"], false);
    assert_eq!(order["privileges"], serde_json::Value::Null);
    assert_eq!(
        order["columns"]["id"],
        json!({"type":"int4","array":false,"nullable":false,"has_default":true,"generated":true})
    );
    assert_eq!(
        order["columns"]["moods"],
        json!({"type":"mood","array":true,"nullable":true,"has_default":false,"generated":false,"enum":"public.mood"})
    );
    assert_eq!(public["functions"], json!({}));
    assert_eq!(public["enums"], json!({"mood": ["sad", "happy"]}));

    // Studio's view: everything anon or authenticated reaches, with what each holds.
    let authenticated = everything();
    let json = s.to_json("public", &[("anon", &anon), ("authenticated", &authenticated)], true);
    let public = &json["schemas"]["public"];
    assert_eq!(
        public["tables"]["Order"]["privileges"],
        json!({"anon": ["select"], "authenticated": ["select", "insert", "update", "delete"]})
    );
    assert_eq!(public["tables"]["secrets"]["rls"], true);
    assert_eq!(
        json["schemas"]["billing"]["tables"]["invoices"]["kind"],
        "view"
    );
    assert_eq!(
        public["functions"]["touch"],
        json!([{"args": [], "returns": {"type": "void", "array": false, "set": false}}])
    );
    assert_eq!(
        public["functions"]["add_points"][0]["args"],
        json!([
            {"name": "user_id", "type": "uuid", "array": false, "has_default": false},
            {"name": "n", "type": "int4", "array": false, "has_default": true}
        ])
    );
    assert!(public["functions"].get("uuid_generate_v4").is_none());
}

#[test]
fn json_names_the_default_schema() {
    // The JSON is rendered per request, never cached, so a changed setting shows at once.
    let s = schema();
    let v = everything();
    for name in ["public", "billing"] {
        let json = s.to_json(name, &[("anon", &v)], false);
        assert_eq!(json["default_schema"], name, "{json}");
    }
}

/// `kind` follows relkind (a partitioned table is a table), and only a view whose options
/// set `security_invoker` says so.
#[test]
fn kinds_and_security_invoker() {
    let mut r = raw();
    for (oid, name, relkind, invoker) in [
        (40004, "mv", "m", false),
        (40005, "ft", "f", false),
        (40006, "parted", "p", false),
        (40007, "safe", "v", true),
    ] {
        r.relations.push((
            oid,
            "public".into(),
            name.into(),
            relkind.into(),
            false,
            invoker,
            vec![col("x", 25, "text")],
            vec![],
        ));
    }
    let s = Schema::build("v1".into(), r);
    let mut v = everything();
    let all = v.tables[&ORDERS];
    for oid in 40004..=40007 {
        v.tables.insert(oid, all);
    }
    let json = s.to_json("public", &[("anon", &v)], false);
    let public = &json["schemas"]["public"]["tables"];
    let billing = &json["schemas"]["billing"]["tables"];
    for (t, kind, invoker) in [
        (&public["Order"], "table", false),
        (&public["mv"], "materialized_view", false),
        (&public["ft"], "foreign_table", false),
        (&public["parted"], "table", false),
        (&public["safe"], "view", true),
        (&billing["invoices"], "view", false),
    ] {
        assert_eq!(t["kind"], kind, "{t}");
        assert_eq!(t["security_invoker"], invoker, "{t}");
    }
}

/// Each table's primary-key columns, in the key's order (not the columns' order); a view
/// or a table without a key has none, and the JSON leaves the field out then.
#[test]
fn introspection_reports_primary_keys() {
    assert!(
        INTROSPECT_SQL.contains("contype = 'p'") && INTROSPECT_SQL.contains("WITH ORDINALITY"),
        "{INTROSPECT_SQL}"
    );
    let mut r = raw();
    r.relations[0].7 = vec!["id".into()];
    r.relations.push((
        40008,
        "public".into(),
        "pairs".into(),
        "r".into(),
        false,
        false,
        vec![col("a", 25, "text"), col("b", 25, "text")],
        vec!["b".into(), "a".into()],
    ));
    let s = Schema::build("v1".into(), r);
    let key = |name: &str| {
        s.tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .primary_key
            .clone()
    };
    assert_eq!(key("Order"), ["id"]);
    assert_eq!(key("pairs"), ["b", "a"]);
    assert!(key("secrets").is_empty());
    assert!(key("invoices").is_empty());

    let mut v = everything();
    v.tables.insert(40008, v.tables[&ORDERS]);
    let json = s.to_json("public", &[("anon", &v)], false);
    let public = &json["schemas"]["public"]["tables"];
    assert_eq!(public["Order"]["primary_key"], json!(["id"]));
    assert_eq!(public["pairs"]["primary_key"], json!(["b", "a"]));
    assert!(public["secrets"].get("primary_key").is_none(), "{}", public["secrets"]);
    let invoices = &json["schemas"]["billing"]["tables"]["invoices"];
    assert!(invoices.get("primary_key").is_none(), "{invoices}");

    // A document written before the field existed still reads, with no key.
    let old: TableDesc = serde_json::from_value(json!({
        "kind": "table", "rls": false, "security_invoker": false, "privileges": null, "columns": {}
    }))
    .unwrap();
    assert!(old.primary_key.is_empty());
}

#[test]
fn schema_reports_app_users_table() {
    let without = schema().to_json("public", &[], false);
    assert!(without.get("app_users").is_none(), "{without}");

    let mut r = raw();
    r.app_users = Some(("public".into(), "users".into()));
    let with = Schema::build("v1".into(), r).to_json("public", &[], false);
    assert_eq!(with["app_users"], json!({"schema": "public", "table": "users"}));

    // The marker rides in the introspection JSON as an optional key.
    let parsed: Raw = serde_json::from_str(r#"{"namespaces":[],"types":[],"enums":[],"relations":[],"functions":[],"app_users":["public","users"]}"#).unwrap();
    assert_eq!(parsed.app_users, Some(("public".into(), "users".into())));
    let parsed: Raw = serde_json::from_str(
        r#"{"namespaces":[],"types":[],"enums":[],"relations":[],"functions":[]}"#,
    )
    .unwrap();
    assert_eq!(parsed.app_users, None);
}

/// #102: the three storage_ref forms are described on their columns (`storage_ref`), and
/// nothing else of a comment or a foreign key is: a `storage.object_ref` (by its schema, not
/// any type named `object_ref`) is `ref`; a column a single-column foreign key makes
/// `storage.objects(id)` is `id`; a text column whose comment says `@storage_ref bucket=…` is
/// `path` with that bucket. Every other column's JSON is as before (no new field).
#[test]
fn storage_ref_columns_are_described() {
    const PROFILES: u32 = 40010;
    let mut raw = raw();
    raw.types.push(ty(
        91001,
        "storage",
        "object_ref",
        "c",
        0,
        0,
        "C",
        -1,
        "storage.object_ref",
    ));
    raw.types.push(ty(
        91002,
        "storage",
        "_object_ref",
        "b",
        91001,
        0,
        "A",
        -1,
        "storage.object_ref[]",
    ));
    raw.types.push(ty(
        91003,
        "public",
        "object_ref",
        "c",
        0,
        0,
        "C",
        -1,
        "object_ref",
    ));
    raw.types.push(ty(
        1009,
        "pg_catalog",
        "_text",
        "b",
        25,
        0,
        "A",
        -1,
        "text[]",
    ));
    raw.relations.push((
        PROFILES,
        "public".into(),
        "profiles".into(),
        "r".into(),
        false,
        false,
        vec![
            col("avatar", 91001, "storage.object_ref"),
            col("gallery", 91002, "storage.object_ref[]"),
            col("lookalike", 91003, "object_ref"),
            col_with(
                "avatar_path",
                25,
                "text",
                Some("The avatar.\n@storage_ref  bucket=avatars (resized)"),
                false,
            ),
            col_with(
                "paths",
                1009,
                "text[]",
                Some("@storage_ref bucket=docs"),
                false,
            ),
            col_with(
                "email",
                90003,
                "email",
                Some("@storage_ref bucket=mail"),
                false,
            ),
            col_with("no_bucket", 25, "text", Some("@storage_ref"), false),
            col_with(
                "empty_bucket",
                25,
                "text",
                Some("@storage_ref bucket="),
                false,
            ),
            col_with(
                "other_word",
                25,
                "text",
                Some("@storage_ref bucket_id=x"),
                false,
            ),
            col_with(
                "not_text",
                23,
                "integer",
                Some("@storage_ref bucket=x"),
                false,
            ),
            col_with("photo_id", 2950, "uuid", Some("internal: photo"), true),
            col_with(
                "secret_note",
                25,
                "text",
                Some("internal note, do not share"),
                false,
            ),
        ],
        vec![],
    ));
    let s = Schema::build("v1".into(), raw);
    let reader = Visibility {
        schemas: HashSet::from([PUBLIC]),
        tables: HashMap::from([(
            PROFILES,
            Privileges {
                select: true,
                ..Default::default()
            },
        )]),
        functions: HashSet::new(),
    };
    let json = s.to_json("public", &[("anon", &reader)], false);
    let cols = &json["schemas"]["public"]["tables"]["profiles"]["columns"];
    assert_eq!(
        cols["avatar"],
        json!({"type":"object_ref","array":false,"nullable":true,"has_default":false,"generated":false,"storage_ref":{"kind":"ref"}})
    );
    assert_eq!(cols["gallery"]["storage_ref"], json!({"kind":"ref"}));
    assert_eq!(cols["gallery"]["array"], true);
    assert_eq!(
        cols["avatar_path"]["storage_ref"],
        json!({"kind":"path","bucket":"avatars"})
    );
    assert_eq!(
        cols["paths"]["storage_ref"],
        json!({"kind":"path","bucket":"docs"})
    );
    // A domain over text is text.
    assert_eq!(
        cols["email"]["storage_ref"],
        json!({"kind":"path","bucket":"mail"})
    );
    assert_eq!(cols["photo_id"]["storage_ref"], json!({"kind":"id"}));
    for plain in [
        "lookalike",
        "no_bucket",
        "empty_bucket",
        "other_word",
        "not_text",
        "secret_note",
    ] {
        assert!(
            cols[plain].get("storage_ref").is_none(),
            "{plain}: {}",
            cols[plain]
        );
    }
    assert_eq!(
        cols["secret_note"],
        json!({"type":"text","array":false,"nullable":true,"has_default":false,"generated":false})
    );
    // No comment text reaches the document.
    let text = json.to_string();
    for leaked in ["internal", "The avatar", "resized"] {
        assert!(!text.contains(leaked), "{leaked}: {text}");
    }
    // A role that can't reach the table sees none of it.
    let other = Visibility {
        schemas: HashSet::from([PUBLIC]),
        tables: HashMap::from([(
            ORDERS,
            Privileges {
                select: true,
                ..Default::default()
            },
        )]),
        functions: HashSet::new(),
    };
    let json = s.to_json("public", &[("anon", &other)], false);
    assert!(
        json["schemas"]["public"]["tables"]
            .get("profiles")
            .is_none(),
        "{json}"
    );
    assert!(!json.to_string().contains("avatars"), "{json}");
}

/// #102: the introspection reads each column's comment and whether a single-column foreign
/// key points it at `storage.objects(id)` (found by name, so a database without storage
/// still introspects). It runs as the request's role, so the table is found through the
/// catalog alone: a qualified name (`to_regclass('storage.objects')`, a `::regclass` cast)
/// checks USAGE on `storage` and fails for a role without it (every custom role: a 500).
#[test]
fn introspection_reads_storage_ref_hints() {
    assert!(
        INTROSPECT_SQL.contains("pg_catalog.col_description(c.oid, a.attnum)"),
        "{INTROSPECT_SQL}"
    );
    assert!(
        INTROSPECT_SQL.contains("s.nspname = 'storage' AND o.relname = 'objects'"),
        "{INTROSPECT_SQL}"
    );
    for qualified in ["to_regclass", "to_regtype", "'storage."] {
        assert!(!INTROSPECT_SQL.contains(qualified), "{qualified}: {INTROSPECT_SQL}");
    }
    assert!(
        INTROSPECT_SQL.contains("f.contype = 'f'"),
        "{INTROSPECT_SQL}"
    );
}
