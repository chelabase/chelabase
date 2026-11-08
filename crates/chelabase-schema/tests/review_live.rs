//! The review scan on a scratch database of the metadata Postgres (PB, T2). Names: `rv_<12 hex>`.

use chelabase_schema::review::{fix_sql, parse_review, review_hash, review_sql};
use chelabase_types::protection::*;
use tokio_postgres::{Client, NoTls, SimpleQueryMessage};

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into())
}

fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
    format!("{head}/{name}{query}")
}

async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls).await.expect("metadata Postgres (is the workbench stack up?)");
    tokio::spawn(connection);
    client
}

async fn one(client: &Client, sql: &str) -> String {
    let rows: Vec<String> = client
        .simple_query(sql)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(row.get(0).unwrap_or_default().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 1, "one row: {rows:?}");
    rows.into_iter().next().unwrap()
}

async fn scan(client: &Client, owner: &str) -> Vec<ReviewItem> {
    parse_review("app", &one(client, &review_sql(owner)).await).unwrap()
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_scan_finds_every_implicit_call_site_and_fixes_apply() {
    let meta = connect(&url()).await;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).unwrap();
    let name = format!("rv_{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let (owner, dev, boss) = (format!("{name}_owner"), format!("{name}_dev"), format!("{name}_boss"));
    meta.batch_execute(&format!("CREATE ROLE {owner} LOGIN; CREATE ROLE {dev}; CREATE ROLE {boss} IN ROLE {owner};")).await.unwrap();
    // CREATE DATABASE can't share a simple query with other statements (implicit transaction).
    meta.batch_execute(&format!("CREATE DATABASE {name} OWNER {owner}")).await.unwrap();
    let db = connect(&url_for(&name)).await;
    db.batch_execute(&format!(
        "GRANT CREATE ON SCHEMA public TO {dev}, {boss};
         SET ROLE {dev};
         CREATE FUNCTION public.f_default() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE FUNCTION public.f_gen(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE FUNCTION public.f_check(int) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
         CREATE FUNCTION public.f_index(int) RETURNS int LANGUAGE sql IMMUTABLE AS 'SELECT $1';
         CREATE FUNCTION public.f_policy() RETURNS boolean LANGUAGE sql STABLE AS 'SELECT true';
         CREATE FUNCTION public.f_view() RETURNS int LANGUAGE sql STABLE AS 'SELECT 1';
         CREATE FUNCTION public.f_inner() RETURNS int LANGUAGE sql STABLE RETURN 1;
         CREATE FUNCTION public.f_outer() RETURNS int LANGUAGE sql STABLE BEGIN ATOMIC SELECT public.f_inner(); END;
         CREATE FUNCTION public.f_definer() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         CREATE FUNCTION public.f_trigger() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION public.f_unused() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE TABLE public.t (id int, c int DEFAULT public.f_default() CONSTRAINT t_c_check CHECK (public.f_check(c)), g int GENERATED ALWAYS AS (public.f_gen(id)) STORED);
         CREATE INDEX t_idx ON public.t (public.f_index(id));
         ALTER TABLE public.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p ON public.t USING (public.f_policy());
         CREATE VIEW public.v AS SELECT public.f_view() AS x, public.f_outer() AS y;
         CREATE TRIGGER tg BEFORE INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.f_trigger();
         CREATE RULE r AS ON UPDATE TO public.t DO ALSO NOTIFY t;
         RESET ROLE;
         SET ROLE {boss};
         CREATE FUNCTION public.f_boss() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         RESET ROLE;"
    ))
    .await
    .unwrap();

    let items = scan(&db, &owner).await;
    let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "app/function/public.f_boss()",
            "app/function/public.f_check(integer)",
            "app/function/public.f_default()",
            "app/function/public.f_definer()",
            "app/function/public.f_gen(integer)",
            "app/function/public.f_index(integer)",
            "app/function/public.f_inner()",
            "app/function/public.f_outer()",
            "app/function/public.f_policy()",
            "app/function/public.f_trigger()",
            "app/function/public.f_view()",
            "app/rule/public.r(public.t)",
            "app/trigger/public.tg(public.t)",
        ],
        // PB end-of-step R14: only superusers and the owner itself are trusted; a member of the
        // owner (f_boss's) is listed like anyone else.
        "not f_unused (unreachable)"
    );
    assert!(items.iter().all(|i| i.owner == *if i.name == "f_boss" { &boss } else { &dev }), "{items:#?}");
    assert!(items.iter().all(|i| i.digest.len() == 32), "an md5 hex digest per item: {items:#?}");
    let reasons = |key: &str| -> Vec<(ReviewReasonKind, Option<String>)> {
        items.iter().find(|i| i.key == key).unwrap().reasons.iter().map(|r| (r.kind, r.via.clone())).collect()
    };
    use ReviewReasonKind::*;
    let s = |v: &str| Some(v.to_string());
    assert_eq!(reasons("app/function/public.f_check(integer)"), [(Check, s("public.t.t_c_check"))]);
    assert_eq!(reasons("app/function/public.f_default()"), [(Default, s("public.t.c"))]);
    assert_eq!(reasons("app/function/public.f_definer()"), [(SecurityDefiner, None)]);
    assert_eq!(reasons("app/function/public.f_gen(integer)"), [(Generated, s("public.t.g"))]);
    assert_eq!(reasons("app/function/public.f_index(integer)"), [(Index, s("public.t_idx"))]);
    assert_eq!(reasons("app/function/public.f_inner()"), [(CalledBy, s("public.f_outer()"))]);
    assert_eq!(reasons("app/function/public.f_outer()"), [(View, s("public.v"))]);
    assert_eq!(reasons("app/function/public.f_policy()"), [(Policy, s("public.t.p"))]);
    assert_eq!(reasons("app/function/public.f_trigger()"), [(Trigger, s("public.t.tg"))]);
    assert_eq!(reasons("app/function/public.f_view()"), [(View, s("public.v"))]);
    assert_eq!(reasons("app/rule/public.r(public.t)"), [(Rule, s("public.t.r"))]);
    assert_eq!(reasons("app/trigger/public.tg(public.t)"), [(Trigger, s("public.t.tg"))]);
    let item = |key: &str| items.iter().find(|i| i.key == key).unwrap().clone();
    assert!(item("app/function/public.f_definer()").opaque_body);
    assert!(!item("app/function/public.f_inner()").opaque_body, "a RETURN body is followed");
    assert_eq!(item("app/trigger/public.tg(public.t)").function.as_deref(), Some("public.f_trigger()"));

    // Fixes, run as the superuser here: reassigning the definer and dropping the trigger take
    // them (and the trigger's function, now unreachable) off the list; the hash moves.
    let before = review_hash(&items);
    db.batch_execute(&fix_sql(&item("app/function/public.f_definer()"), FixAction::Reassign, &owner).unwrap()).await.unwrap();
    db.batch_execute(&fix_sql(&item("app/trigger/public.tg(public.t)"), FixAction::Drop, &owner).unwrap()).await.unwrap();
    let after = scan(&db, &owner).await;
    let keys: Vec<&str> = after.iter().map(|i| i.key.as_str()).collect();
    assert!(!keys.contains(&"app/function/public.f_definer()"));
    assert!(!keys.contains(&"app/trigger/public.tg(public.t)"));
    assert!(!keys.contains(&"app/function/public.f_trigger()"));
    assert_eq!(keys.len(), 10);
    assert_ne!(review_hash(&after), before);
    // A drop that has dependents fails as Postgres says (RESTRICT).
    let err = db.batch_execute(&fix_sql(&item("app/function/public.f_policy()"), FixAction::Drop, &owner).unwrap()).await.unwrap_err();
    assert_eq!(err.as_db_error().map(|e| e.code().code()), Some("2BP01"), "{err}");

    drop(db);
    meta.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    meta.batch_execute(&format!("DROP ROLE {boss}; DROP ROLE {dev}; DROP ROLE {owner};")).await.unwrap();
}

// T2 fix round (review findings 1–6; R2–R5): operators, domain defaults, SECURITY DEFINER
// functions of any owner as starting points, range types, partition clones, and a trigger
// whose function is Admin-owned on a non-Admin table.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_scan_follows_operators_domains_ranges_and_definer_callers() {
    let meta = connect(&url()).await;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).unwrap();
    let name = format!("rv_{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let (owner, dev) = (format!("{name}_owner"), format!("{name}_dev"));
    meta.batch_execute(&format!("CREATE ROLE {owner} LOGIN; CREATE ROLE {dev};")).await.unwrap();
    meta.batch_execute(&format!("CREATE DATABASE {name} OWNER {owner}")).await.unwrap();
    let db = connect(&url_for(&name)).await;
    db.batch_execute(&format!(
        "GRANT CREATE ON SCHEMA public TO {dev};
         SET ROLE {dev};
         CREATE FUNCTION public.f_op(int, int) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT $1 = $2';
         CREATE OPERATOR public.=== (LEFTARG = int, RIGHTARG = int, FUNCTION = public.f_op);
         CREATE FUNCTION public.f_dd() RETURNS int LANGUAGE sql AS 'SELECT 1';
         CREATE DOMAIN public.dd AS int DEFAULT public.f_dd();
         CREATE FUNCTION public.f_called() RETURNS int LANGUAGE sql STABLE RETURN 1;
         CREATE FUNCTION public.f_diff(float8, float8) RETURNS float8 LANGUAGE sql IMMUTABLE AS 'SELECT $1 - $2';
         CREATE TYPE public.r AS RANGE (subtype = float8, subtype_diff = public.f_diff);
         CREATE TABLE public.t (id int);
         ALTER TABLE public.t ENABLE ROW LEVEL SECURITY;
         CREATE POLICY p ON public.t USING (id OPERATOR(public.===) 1);
         CREATE TABLE public.pt (id int) PARTITION BY RANGE (id);
         CREATE TABLE public.pt1 PARTITION OF public.pt FOR VALUES FROM (0) TO (10);
         CREATE FUNCTION public.f_ptrg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TRIGGER ptg BEFORE INSERT ON public.pt FOR EACH ROW EXECUTE FUNCTION public.f_ptrg();
         RESET ROLE;
         SET ROLE {owner};
         CREATE FUNCTION public.f_admin_trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE FUNCTION public.api() RETURNS int LANGUAGE sql SECURITY DEFINER BEGIN ATOMIC SELECT public.f_called(); END;
         RESET ROLE;
         CREATE TRIGGER atg BEFORE INSERT ON public.t FOR EACH ROW EXECUTE FUNCTION public.f_admin_trg();"
    ))
    .await
    .unwrap();

    let items = scan(&db, &owner).await;
    let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    let reasons = |key: &str| -> Vec<(ReviewReasonKind, Option<String>)> {
        items.iter().find(|i| i.key == key).unwrap_or_else(|| panic!("{key} not in {keys:#?}")).reasons.iter().map(|r| (r.kind, r.via.clone())).collect()
    };
    use ReviewReasonKind::*;
    let s = |v: &str| Some(v.to_string());
    // 1: a function behind an operator, reached from a policy.
    assert_eq!(reasons("app/function/public.f_op(integer, integer)"), [(Policy, s("public.t.p"))]);
    // 2: a domain's default.
    assert_eq!(reasons("app/function/public.f_dd()"), [(Default, s("public.dd"))]);
    // 3: called by an Admin-owned SECURITY DEFINER function (which itself isn't listed).
    assert_eq!(reasons("app/function/public.f_called()"), [(CalledBy, s("public.api()"))]);
    assert!(!keys.contains(&"app/function/public.api()"));
    // 4: a range type's subtype_diff.
    assert_eq!(reasons("app/function/public.f_diff(double precision, double precision)"), [(Type, s("public.r"))]);
    // 5: a partitioned table's trigger is listed once, not again for its partition's clone.
    assert_eq!(reasons("app/trigger/public.ptg(public.pt)"), [(Trigger, s("public.pt.ptg"))]);
    assert!(!keys.contains(&"app/trigger/public.ptg(public.pt1)"), "{keys:#?}");
    assert_eq!(reasons("app/function/public.f_ptrg()"), [(Trigger, s("public.pt.ptg"))]);
    // 6: a trigger on a non-Admin table whose function is Admin-owned: listed with no function,
    // the table's owner shown, and reassign gives the table to the owner.
    let atg = items.iter().find(|i| i.key == "app/trigger/public.atg(public.t)").expect("atg listed").clone();
    assert_eq!((atg.function.as_deref(), atg.owner.as_str()), (None, dev.as_str()));
    assert!(!keys.contains(&"app/function/public.f_admin_trg()"));
    db.batch_execute(&fix_sql(&atg, FixAction::Reassign, &owner).unwrap()).await.unwrap();
    let after = scan(&db, &owner).await;
    assert!(after.iter().all(|i| i.key != "app/trigger/public.atg(public.t)"), "{after:#?}");
    // R15: a body replaced in place keeps the key, owner and reasons but changes the hash.
    db.batch_execute(&format!("SET ROLE {dev}; CREATE OR REPLACE FUNCTION public.f_dd() RETURNS int LANGUAGE sql AS 'SELECT 2'; RESET ROLE;")).await.unwrap();
    let replaced = scan(&db, &owner).await;
    let keys_of = |v: &[ReviewItem]| v.iter().map(|i| i.key.clone()).collect::<Vec<_>>();
    assert_eq!(keys_of(&replaced), keys_of(&after));
    assert_ne!(review_hash(&replaced), review_hash(&after));

    drop(db);
    meta.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    meta.batch_execute(&format!("DROP ROLE {dev}; DROP ROLE {owner};")).await.unwrap();
}

// #185 audit B1: `storage` is skipped only when it is object storage's (owned by
// `chelabase_admin`). A `storage` schema the owner made, where another role can create, is
// reviewed like any other: a planted SECURITY DEFINER function and a trigger are listed.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_user_owned_storage_schema_is_reviewed() {
    let meta = connect(&url()).await;
    let mut random = [0u8; 6];
    getrandom::fill(&mut random).unwrap();
    let name = format!("rv_{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let (owner, dev) = (format!("{name}_owner"), format!("{name}_dev"));
    meta.batch_execute(&format!("CREATE ROLE {owner} LOGIN; CREATE ROLE {dev};")).await.unwrap();
    meta.batch_execute(&format!("CREATE DATABASE {name} OWNER {owner}")).await.unwrap();
    let db = connect(&url_for(&name)).await;
    db.batch_execute(&format!(
        "SET ROLE {owner};
         CREATE SCHEMA storage;
         GRANT USAGE, CREATE ON SCHEMA storage TO {dev};
         RESET ROLE;
         SET ROLE {dev};
         CREATE FUNCTION storage.f_planted() RETURNS int LANGUAGE sql SECURITY DEFINER AS 'SELECT 1';
         CREATE FUNCTION storage.f_trg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END';
         CREATE TABLE storage.t (a int);
         CREATE TRIGGER tg BEFORE INSERT ON storage.t FOR EACH ROW EXECUTE FUNCTION storage.f_trg();
         RESET ROLE;"
    ))
    .await
    .unwrap();

    let keys: Vec<String> = scan(&db, &owner).await.into_iter().map(|i| i.key).collect();
    assert_eq!(
        keys,
        ["app/function/storage.f_planted()", "app/function/storage.f_trg()", "app/trigger/storage.tg(storage.t)"],
        "a storage schema that isn't chelabase_admin's is reviewed"
    );

    drop(db);
    meta.batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)")).await.unwrap();
    meta.batch_execute(&format!("DROP ROLE {dev}; DROP ROLE {owner};")).await.unwrap();
}
