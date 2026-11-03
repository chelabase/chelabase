//! The Acme organisation on top of the demo people: teams and role levels, the project "Acme"
//! with its database, schemas owned by team roles, row-level security, app users, a data API
//! key, a protected `main` with a feature branch, and two saved queries.
//!
//! Ada drives it through the public management API only; the schema and rows are one checked-in
//! script ([`SCHEMA_SQL`]) that the SQL endpoint runs as the database's owner. Every step looks
//! first (by name, or with a read-only query) and writes only what is missing, so a run that
//! stopped anywhere is finished by the next one.
//!
//! The last write is the completion marker, a comment on `main`'s Postgres database
//! ([`MARKER`]). Once [`complete`] finds it, the demo belongs to whoever uses it: the seed is
//! skipped, so a later `up` never puts back what someone changed, renamed or removed.

use super::DEMO_PASSWORD;
use super::people::{ADA, PEOPLE};
use super::session::{Http, Session, SignIn, Token, call_error};
use anyhow::{Result, anyhow, bail};
use reqwest::Method;
use serde_json::{Value, json};
use std::future::Future;
use std::io::Write;
use std::time::Duration;

/// Schema, policies and rows, applied as the database's owner.
pub const SCHEMA_SQL: &str = include_str!("acme.sql");

pub const PROJECT: &str = "Acme";
/// The tenant's display name: Studio's Members page says "People in Acme".
pub const TENANT_NAME: &str = "Acme";
pub const DATABASE: &str = "acme";
/// The schema a data API request names when it names none: `from("orders")` is `sales.orders`.
pub const DEFAULT_SCHEMA: &str = "sales";
pub const FEATURE_BRANCH: &str = "feature-discounts";
pub const KEY_NAME: &str = "acme-server";
/// The comment on `main`'s Postgres database that says the seed finished.
pub const MARKER: &str = "chelabase example seed: complete v1";

/// A team and its grant on the project.
pub struct TeamDef {
    pub name: &'static str,
    pub platform_role: &'static str,
    /// The Postgres roles the team acts as (empty for Admin, which holds every role).
    pub pg_roles: &'static [&'static str],
}

pub const TEAMS: &[TeamDef] = &[
    TeamDef {
        name: "Engineering",
        platform_role: "developer",
        pg_roles: &["engineering"],
    },
    TeamDef {
        name: "Finance",
        platform_role: "developer",
        pg_roles: &["finance"],
    },
    TeamDef {
        name: "Sales",
        platform_role: "developer",
        pg_roles: &["sales"],
    },
    TeamDef {
        name: "Support",
        platform_role: "viewer",
        pg_roles: &["support"],
    },
    TeamDef {
        name: "Leadership",
        platform_role: "admin",
        pg_roles: &[],
    },
];

/// The Postgres roles that own the schemas.
pub const PG_ROLES: &[&str] = &["engineering", "finance", "sales", "support"];

/// The one per-database override: Alan is an Admin through Leadership, narrowed to Viewer on
/// the "acme" database (a read-only auditor).
pub const OVERRIDE: (&str, &str) = ("alan@acme.example", "viewer");

/// The app users: customers who sign in to the data API with the demo password.
pub const CUSTOMERS: &[&str] = &[
    "carol@customer.example",
    "dave@customer.example",
    "erin@customer.example",
];

/// The saved SQL queries (Ada's).
pub const QUERIES: &[(&str, &str)] = &[
    (
        "Revenue by month",
        "select date_trunc('month', issued_on)::date as month, sum(amount) as revenue\nfrom finance.invoices\nwhere paid\ngroup by 1\norder by 1;",
    ),
    (
        "Open tickets by region",
        "select region, count(*) as open_tickets\nfrom support.tickets\nwhere status = 'open'\ngroup by region\norder by open_tickets desc;",
    ),
];

/// True when every table the script makes exists.
const PROBE_SCHEMA: &str = "select (to_regclass('public.customers') is not null \
    and to_regclass('public.employees') is not null \
    and to_regclass('sales.orders') is not null \
    and to_regclass('finance.invoices') is not null \
    and to_regclass('support.tickets') is not null)::text";

/// True when the feature branch already has its column.
const PROBE_FEATURE: &str = "select exists (select 1 from information_schema.columns \
    where table_schema = 'sales' and table_name = 'orders' and column_name = 'discount_pct')::text";

const ADD_DISCOUNT: &str = "alter table sales.orders add column if not exists discount_pct \
    numeric(5, 2) not null default 0 check (discount_pct between 0 and 100);";

/// True when `main`'s database carries [`MARKER`].
const PROBE_MARKER: &str = "select (coalesce(shobj_description(d.oid, 'pg_database'), '') \
    = 'chelabase example seed: complete v1')::text from pg_database d \
    where d.datname = current_database()";

/// Writes [`MARKER`]. `COMMENT ON DATABASE` takes a name, not an expression, hence the `DO`.
const MARK_COMPLETE: &str = "do $$ begin execute format('comment on database %I is %L', \
    current_database(), 'chelabase example seed: complete v1'); end $$;";

struct Ids {
    project: String,
    database: String,
    branch: String,
    /// The Postgres database the data API serves.
    pg: String,
}

struct Run<'a, 'o, H: Http> {
    s: &'a Session<'a, H>,
    ada: &'a Token,
    writes: usize,
    out: &'o mut dyn Write,
}

/// Seeds the Acme organisation as Ada. `people::seed` ran first, so every person is a member.
pub async fn seed<H: Http>(s: &Session<'_, H>, ada: &Token, out: &mut dyn Write) -> Result<usize> {
    let mut r = Run {
        s,
        ada,
        writes: 0,
        out,
    };
    r.tenant().await?;
    let project = r.project().await?;
    let database = r.database(&project).await?;
    let (branch, pg) = r.main(&database).await?;
    let ids = Ids {
        project,
        database,
        branch,
        pg,
    };
    r.roles(&ids).await?;
    r.data_api(&ids).await?;
    r.app_users(&ids).await?;
    r.customers(&ids).await?;
    r.teams(&ids).await?;
    r.schema(&ids).await?;
    r.default_schema(&ids).await?;
    r.feature_branch(&ids).await?;
    r.key(&ids).await?;
    r.queries(&ids).await?;
    r.protect_main(&ids).await?;
    r.mark_complete(&ids).await?;
    writeln!(
        r.out,
        "example: acme: {} writes. {PROJECT}/{DATABASE}: teams {}, customers {} (demo password), branches main (protected) and {FEATURE_BRANCH}",
        r.writes,
        TEAMS.iter().map(|t| t.name).collect::<Vec<_>>().join(", "),
        CUSTOMERS.join(", "),
    )?;
    Ok(r.writes)
}

/// True when an earlier run finished the seed: project Acme's database `acme` has a ready
/// `main` whose Postgres database carries [`MARKER`]. Reads only.
pub async fn complete<H: Http>(s: &Session<'_, H>, ada: &Token) -> Result<bool> {
    let me = s
        .api(Method::GET, "/v1/me", ada, None, "GET /v1/me")
        .await?;
    let database = me
        .pointer("/tenant/projects")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|p| text(p, "name") == PROJECT)
        .flat_map(|p| {
            p.get("databases")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|d| text(d, "name") == DATABASE)
        .map(|d| text(d, "database_id").to_string());
    let Some(database) = database else {
        return Ok(false);
    };
    let database = seg(&database)?.to_string();
    let branches = array(
        s.api(
            Method::GET,
            &format!("/v1/databases/{database}/branches"),
            ada,
            None,
            "GET …/branches",
        )
        .await?,
        "branches",
    )?;
    let main = branches.iter().find(|b| {
        text(b, "kind") == "normal"
            && b.get("parent_branch_id").is_some_and(Value::is_null)
            && text(b, "status") == "ready"
    });
    let Some(main) = main else {
        return Ok(false);
    };
    let branch = seg(text(main, "id"))?.to_string();
    let probe = s
        .sql(
            ada,
            (&database, &branch),
            PROBE_MARKER,
            None,
            false,
            "POST …/sql (marker probe)",
        )
        .await?;
    Ok(probe
        .first()
        .and_then(|r| r.first())
        .and_then(Option::as_deref)
        == Some("true"))
}

fn text<'v>(v: &'v Value, k: &str) -> &'v str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn array(v: Value, what: &str) -> Result<Vec<Value>> {
    match v {
        Value::Array(a) => Ok(a),
        _ => bail!("{what}: not a list"),
    }
}

/// A path segment: ids from the server are UUIDs, but a name in a path is never trusted.
fn seg(id: &str) -> Result<&str> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(id)
    } else {
        bail!("the server sent an id that is not a plain identifier")
    }
}

impl<H: Http> Run<'_, '_, H> {
    async fn get(&self, path: &str, what: &'static str) -> Result<Value> {
        self.s.api(Method::GET, path, self.ada, None, what).await
    }

    /// A write: counted.
    async fn write(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
        what: &'static str,
    ) -> Result<Value> {
        self.writes += 1;
        self.s.api(method, path, self.ada, body, what).await
    }

    async fn say(&mut self, line: &str) -> Result<()> {
        writeln!(self.out, "example: acme: {line}")?;
        Ok(())
    }

    /// Polls `f` until it yields a value, for the session's timeout.
    async fn until<T, F, Fut>(&self, what: &str, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Option<T>>>,
    {
        let (timeout, second) = self.s.patience();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(v) = f().await? {
                return Ok(v);
            }
            if tokio::time::Instant::now() + second > deadline {
                bail!("{what}: not ready after {timeout:?}");
            }
            tokio::time::sleep(second.max(Duration::from_millis(1))).await;
        }
    }

    /// Names the tenant after the organisation (it shows as its slug until named). Read first:
    /// a tenant that already has the name is left alone.
    async fn tenant(&mut self) -> Result<()> {
        let me = self.get("/v1/me", "GET /v1/me").await?;
        if me.pointer("/tenant/display_name").and_then(Value::as_str) == Some(TENANT_NAME) {
            return Ok(());
        }
        self.write(
            Method::PATCH,
            "/v1/tenant",
            Some(json!({"display_name": TENANT_NAME})),
            "PATCH /v1/tenant",
        )
        .await?;
        self.say(&format!("named the tenant {TENANT_NAME}")).await
    }

    async fn project(&mut self) -> Result<String> {
        let me = self.get("/v1/me", "GET /v1/me").await?;
        let projects = me
            .pointer("/tenant/projects")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("GET /v1/me: no tenant projects"))?;
        if let Some(p) = projects.iter().find(|p| text(p, "name") == PROJECT) {
            return Ok(seg(text(p, "id"))?.to_string());
        }
        let made = self
            .write(
                Method::POST,
                "/v1/projects",
                Some(json!({"name": PROJECT})),
                "POST /v1/projects",
            )
            .await?;
        self.say(&format!("created project {PROJECT}")).await?;
        Ok(seg(text(&made, "id"))?.to_string())
    }

    async fn database(&mut self, project: &str) -> Result<String> {
        let me = self.get("/v1/me", "GET /v1/me").await?;
        let found = me
            .pointer("/tenant/projects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| text(p, "id") == project)
            .flat_map(|p| {
                p.get("databases")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .find(|d| text(d, "name") == DATABASE)
            .map(|d| text(d, "database_id").to_string());
        let id = match found {
            Some(id) => id,
            None => {
                let made = self
                    .write(
                        Method::POST,
                        "/v1/databases",
                        Some(json!({"project_id": project, "name": DATABASE})),
                        "POST /v1/databases",
                    )
                    .await?;
                self.say(&format!("created database {DATABASE}")).await?;
                text(&made, "database_id").to_string()
            }
        };
        let id = seg(&id)?.to_string();
        let path = format!("/v1/databases/{id}");
        self.until("the database", || async {
            let d = self.get(&path, "GET /v1/databases/{id}").await?;
            match text(&d, "status") {
                "ready" => Ok(Some(())),
                "failed" | "deleting" | "deleted" => {
                    bail!("the database is {}", text(&d, "status"))
                }
                _ => Ok(None),
            }
        })
        .await?;
        Ok(id)
    }

    /// The main branch's id (waiting until it is ready) and its Postgres database.
    async fn main(&mut self, database: &str) -> Result<(String, String)> {
        let path = format!("/v1/databases/{database}/branches");
        let branch = self
            .until("the main branch", || async {
                let all = array(
                    self.get(&path, "GET /v1/databases/{id}/branches").await?,
                    "branches",
                )?;
                let main = all.iter().find(|b| {
                    text(b, "kind") == "normal"
                        && b.get("parent_branch_id").is_some_and(Value::is_null)
                });
                match main {
                    Some(b) if text(b, "status") == "ready" => Ok(Some(text(b, "id").to_string())),
                    Some(b) if text(b, "status") == "failed" => bail!("the main branch failed"),
                    _ => Ok(None),
                }
            })
            .await?;
        let branch = seg(&branch)?.to_string();
        let dbs = self.pg_databases(database, &branch).await?;
        let main = dbs
            .iter()
            .find(|d| d.get("main") == Some(&json!(true)))
            .ok_or_else(|| anyhow!("the branch lists no main Postgres database"))?;
        Ok((branch, text(main, "name").to_string()))
    }

    async fn pg_databases(&self, database: &str, branch: &str) -> Result<Vec<Value>> {
        let path = format!("/v1/databases/{database}/branches/{branch}/databases");
        array(
            self.get(&path, "GET …/branches/{id}/databases").await?,
            "databases",
        )
    }

    async fn roles(&mut self, ids: &Ids) -> Result<()> {
        let path = format!(
            "/v1/databases/{}/branches/{}/roles",
            ids.database, ids.branch
        );
        let have = array(self.get(&path, "GET …/roles").await?, "roles")?;
        for role in PG_ROLES {
            if have.iter().any(|r| text(r, "name") == *role) {
                continue;
            }
            self.write(
                Method::POST,
                &path,
                Some(json!({"name": role, "login": false})),
                "POST …/roles",
            )
            .await?;
            self.say(&format!("created role {role}")).await?;
        }
        Ok(())
    }

    async fn data_api(&mut self, ids: &Ids) -> Result<()> {
        let on = |dbs: &[Value]| {
            dbs.iter().find(|d| text(d, "name") == ids.pg).map(|d| {
                (
                    text(d, "data_api").to_string(),
                    text(d, "data_api_error").to_string(),
                )
            })
        };
        let dbs = self.pg_databases(&ids.database, &ids.branch).await?;
        if on(&dbs).is_some_and(|(state, _)| state == "on") {
            return Ok(());
        }
        let path = format!(
            "/v1/databases/{}/branches/{}/pg-databases/{}/data-api",
            ids.database,
            ids.branch,
            seg(&ids.pg)?
        );
        self.write(Method::POST, &path, None, "POST …/data-api")
            .await?;
        self.until("the data API", || async {
            let dbs = self.pg_databases(&ids.database, &ids.branch).await?;
            match on(&dbs) {
                Some((state, _)) if state == "on" => Ok(Some(())),
                Some((state, why)) if state == "error" => bail!("the data API failed: {why}"),
                _ => Ok(None),
            }
        })
        .await?;
        self.say("data API on").await
    }

    async fn app_users(&mut self, ids: &Ids) -> Result<()> {
        let path = format!("/v1/projects/{}", ids.project);
        let status = |p: &Value| {
            p.get("app_users")
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let now = status(&self.get(&path, "GET /v1/projects/{id}").await?);
        if matches!(now.as_deref(), None | Some("failed")) {
            let put = format!("/v1/projects/{}/app-users", ids.project);
            let body = json!({"database_id": ids.database, "pg_database": ids.pg});
            self.write(
                Method::PUT,
                &put,
                Some(body),
                "PUT /v1/projects/{id}/app-users",
            )
            .await?;
        }
        self.until("app users", || async {
            let p = self.get(&path, "GET /v1/projects/{id}").await?;
            match status(&p).as_deref() {
                Some("enabled") => Ok(Some(())),
                Some("failed") => bail!("enabling app users failed: {}", text(&p, "error")),
                _ => Ok(None),
            }
        })
        .await?;
        if now.as_deref() != Some("enabled") {
            self.say("app users enabled").await?;
        }
        Ok(())
    }

    async fn customers(&mut self, ids: &Ids) -> Result<()> {
        let path = format!("/v1/projects/{}/users", ids.project);
        let page = self.get(&path, "GET /v1/projects/{id}/users").await?;
        let have: Vec<String> = page
            .get("users")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|u| text(u, "email").to_lowercase())
            .collect();
        for email in CUSTOMERS {
            if have.iter().any(|h| h == email) {
                continue;
            }
            let body = json!({"email": email, "password": DEMO_PASSWORD, "email_verified": true});
            self.write(
                Method::POST,
                &path,
                Some(body),
                "POST /v1/projects/{id}/users",
            )
            .await?;
            self.say(&format!("created app user {email}")).await?;
        }
        Ok(())
    }

    async fn teams(&mut self, ids: &Ids) -> Result<()> {
        let members = array(self.get("/v1/members", "GET /v1/members").await?, "members")?;
        let member_id = |email: &str| {
            members
                .iter()
                .find(|m| text(m, "email") == email)
                .map(|m| text(m, "id").to_string())
                .ok_or_else(|| anyhow!("{email} is not a member"))
        };
        let mut teams = array(self.get("/v1/teams", "GET /v1/teams").await?, "teams")?;
        for def in TEAMS {
            let id = match teams
                .iter()
                .find(|t| text(t, "name").eq_ignore_ascii_case(def.name))
            {
                Some(t) => text(t, "id").to_string(),
                None => {
                    let made = self
                        .write(
                            Method::POST,
                            "/v1/teams",
                            Some(json!({"name": def.name})),
                            "POST /v1/teams",
                        )
                        .await?;
                    self.say(&format!("created team {}", def.name)).await?;
                    teams.push(made.clone());
                    text(&made, "id").to_string()
                }
            };
            let id = seg(&id)?.to_string();
            let team = self
                .get(&format!("/v1/teams/{id}"), "GET /v1/teams/{id}")
                .await?;
            self.team_grant(&id, &team, def, ids).await?;
            let want: Vec<&str> = std::iter::once(&ADA)
                .chain(PEOPLE)
                .filter(|p| p.teams.contains(&def.name))
                .map(|p| p.email)
                .collect();
            let have: Vec<&str> = team
                .get("member_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let mut added = 0;
            for email in want {
                let m = member_id(email)?;
                if have.contains(&m.as_str()) {
                    continue;
                }
                let path = format!("/v1/teams/{id}/members/{}", seg(&m)?);
                self.write(Method::PUT, &path, None, "PUT /v1/teams/{id}/members/{id}")
                    .await?;
                added += 1;
            }
            if added > 0 {
                self.say(&format!("{} gained {added} members", def.name))
                    .await?;
            }
        }
        // The override: someone narrowed on this one database.
        let (email, level) = OVERRIDE;
        let who = member_id(email)?;
        // The grants list shows direct grants only, and Alan's comes from his team, so the
        // check is the effect: his own view of the database.
        let has = match self.s.sign_in(email, DEMO_PASSWORD).await? {
            SignIn::Token(token) => {
                let me = self
                    .s
                    .api(Method::GET, "/v1/me", &token, None, "GET /v1/me")
                    .await?;
                me.get("projects")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|p| text(p, "project_id") == ids.project)
                    .flat_map(|p| {
                        p.get("databases")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .any(|d| {
                        text(d, "database_id") == ids.database && text(d, "platform_role") == level
                    })
            }
            SignIn::Refused | SignIn::Pending => {
                bail!("{email} can't sign in with the demo password")
            }
        };
        if !has {
            let path = format!(
                "/v1/projects/{}/grants/{}/overrides",
                ids.project,
                seg(&who)?
            );
            let body = json!({"database_id": ids.database, "platform_role": level});
            self.write(
                Method::PUT,
                &path,
                Some(body),
                "PUT …/grants/{id}/overrides",
            )
            .await?;
            self.say(&format!("{email} narrowed to {level} on {DATABASE}"))
                .await?;
        }
        Ok(())
    }

    async fn team_grant(&mut self, id: &str, team: &Value, def: &TeamDef, ids: &Ids) -> Result<()> {
        let sorted = |v: Option<&Value>| {
            let mut roles: Vec<String> = v
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            roles.sort();
            roles
        };
        let mut want: Vec<String> = def.pg_roles.iter().map(|r| r.to_string()).collect();
        want.sort();
        let current = team
            .get("projects")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|p| text(p, "project_id") == ids.project);
        // Admin holds every role and drops the names, so only the level is compared.
        let same = current.is_some_and(|p| {
            text(p, "platform_role") == def.platform_role
                && (def.platform_role == "admin" || sorted(p.get("pg_roles")) == want)
        });
        if same {
            return Ok(());
        }
        let path = format!("/v1/teams/{id}/grants/{}", ids.project);
        let mut body = json!({"platform_role": def.platform_role});
        if !def.pg_roles.is_empty() {
            body["pg_roles"] = json!(def.pg_roles);
        }
        self.write(
            Method::PUT,
            &path,
            Some(body),
            "PUT /v1/teams/{id}/grants/{id}",
        )
        .await?;
        self.say(&format!(
            "{} is {} on {PROJECT}",
            def.name, def.platform_role
        ))
        .await
    }

    /// The schemas, tables, policies and rows: one script, run only when a table is missing.
    async fn schema(&mut self, ids: &Ids) -> Result<()> {
        let at = (ids.database.as_str(), ids.branch.as_str());
        let probe = self
            .s
            .sql(
                self.ada,
                at,
                PROBE_SCHEMA,
                None,
                false,
                "POST …/sql (probe)",
            )
            .await?;
        if probe
            .first()
            .and_then(|r| r.first())
            .and_then(Option::as_deref)
            == Some("true")
        {
            return Ok(());
        }
        self.writes += 1;
        self.s
            .sql(
                self.ada,
                at,
                SCHEMA_SQL,
                None,
                true,
                "POST …/sql (acme.sql)",
            )
            .await?;
        self.say("schemas, tables, policies and rows applied").await
    }

    async fn default_schema(&mut self, ids: &Ids) -> Result<()> {
        let dbs = self.pg_databases(&ids.database, &ids.branch).await?;
        let now = dbs
            .iter()
            .find(|d| text(d, "name") == ids.pg)
            .map(|d| text(d, "default_schema").to_string());
        if now.as_deref() == Some(DEFAULT_SCHEMA) {
            return Ok(());
        }
        let path = format!(
            "/v1/databases/{}/branches/{}/pg-databases/{}",
            ids.database,
            ids.branch,
            seg(&ids.pg)?
        );
        let body = json!({"default_schema": DEFAULT_SCHEMA, "grant_existing": true});
        self.write(
            Method::PATCH,
            &path,
            Some(body),
            "PATCH …/pg-databases/{name}",
        )
        .await?;
        self.say(&format!("data API default schema is {DEFAULT_SCHEMA}"))
            .await
    }

    async fn feature_branch(&mut self, ids: &Ids) -> Result<()> {
        let path = format!("/v1/databases/{}/branches", ids.database);
        let list = array(self.get(&path, "GET …/branches").await?, "branches")?;
        if !list.iter().any(|b| text(b, "name") == FEATURE_BRANCH) {
            let body = json!({"name": FEATURE_BRANCH, "parent_branch_id": ids.branch});
            self.write(Method::POST, &path, Some(body), "POST …/branches")
                .await?;
            self.say(&format!("created branch {FEATURE_BRANCH}"))
                .await?;
        }
        let branch = self
            .until("the feature branch", || async {
                let all = array(self.get(&path, "GET …/branches").await?, "branches")?;
                match all.iter().find(|b| text(b, "name") == FEATURE_BRANCH) {
                    Some(b) if text(b, "status") == "ready" => Ok(Some(text(b, "id").to_string())),
                    Some(b) if text(b, "status") == "failed" => bail!("{FEATURE_BRANCH} failed"),
                    _ => Ok(None),
                }
            })
            .await?;
        let branch = seg(&branch)?.to_string();
        let at = (ids.database.as_str(), branch.as_str());
        let probe = self
            .s
            .sql(
                self.ada,
                at,
                PROBE_FEATURE,
                None,
                false,
                "POST …/sql (probe)",
            )
            .await?;
        if probe
            .first()
            .and_then(|r| r.first())
            .and_then(Option::as_deref)
            == Some("true")
        {
            return Ok(());
        }
        self.writes += 1;
        self.s
            .sql(
                self.ada,
                at,
                ADD_DISCOUNT,
                None,
                true,
                "POST …/sql (discount)",
            )
            .await?;
        self.say(&format!("{FEATURE_BRANCH} adds sales.orders.discount_pct"))
            .await
    }

    /// A secret data API key. It is shown once, so it is neither printed nor kept.
    async fn key(&mut self, ids: &Ids) -> Result<()> {
        let path = format!(
            "/v1/databases/{}/branches/{}/keys",
            ids.database, ids.branch
        );
        let keys = array(self.get(&path, "GET …/keys").await?, "keys")?;
        if keys.iter().any(|k| text(k, "name") == KEY_NAME) {
            return Ok(());
        }
        self.write(
            Method::POST,
            &path,
            Some(json!({"name": KEY_NAME})),
            "POST …/keys",
        )
        .await?;
        self.say(&format!(
            "created secret key {KEY_NAME} (shown once, not kept)"
        ))
        .await
    }

    async fn queries(&mut self, ids: &Ids) -> Result<()> {
        let path = format!("/v1/databases/{}/sql-queries", ids.database);
        let have = array(self.get(&path, "GET …/sql-queries").await?, "saved queries")?;
        for (name, sql) in QUERIES {
            if have
                .iter()
                .any(|q| text(q, "name").eq_ignore_ascii_case(name))
            {
                continue;
            }
            let body = json!({"name": name, "sql": sql, "pg_database": ids.pg});
            self.write(Method::POST, &path, Some(body), "POST …/sql-queries")
                .await?;
            self.say(&format!("saved query {name}")).await?;
        }
        Ok(())
    }

    /// Last: a protected branch holds back every statement that isn't a read.
    async fn protect_main(&mut self, ids: &Ids) -> Result<()> {
        let list = array(
            self.get(
                &format!("/v1/databases/{}/branches", ids.database),
                "GET …/branches",
            )
            .await?,
            "branches",
        )?;
        let main = list
            .iter()
            .find(|b| text(b, "id") == ids.branch)
            .ok_or_else(|| anyhow!("the main branch is gone"))?;
        if main.get("protected") == Some(&json!(true)) {
            return Ok(());
        }
        let base = format!("/v1/databases/{}/branches/{}", ids.database, ids.branch);
        let review = self.get(&format!("{base}/review"), "GET …/review").await?;
        let mut body = json!({"protected": true});
        if review
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|i| !i.is_empty())
        {
            body["review_hash"] = json!(text(&review, "hash"));
        }
        self.write(Method::PATCH, &base, Some(body), "PATCH …/branches/{id}")
            .await?;
        self.say("main is protected").await
    }

    /// After every other step: the marker that makes later runs skip the seed. `main` is
    /// protected by now, so the run is confirmed (Ada is an Admin).
    async fn mark_complete(&mut self, ids: &Ids) -> Result<()> {
        self.writes += 1;
        self.s
            .sql(
                self.ada,
                (&ids.database, &ids.branch),
                MARK_COMPLETE,
                None,
                true,
                "POST …/sql (marker)",
            )
            .await?;
        self.say("marked complete: later runs skip the seed").await
    }
}

/// True when the error is the server saying the call was refused (for tests and callers that
/// want to tell a refusal from a transport failure).
pub fn refused(e: &anyhow::Error) -> bool {
    call_error(e).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::example::fake::{self, Fake};

    async fn run(f: &Fake) -> Result<String> {
        let mut out = Vec::new();
        let r = crate::example::run(&fake::config(), f, &mut out).await;
        let text = String::from_utf8(out).unwrap();
        r.map(|()| text)
    }

    #[test]
    fn the_roster_is_about_forty_people_in_one_or_two_known_teams() {
        let all: Vec<_> = std::iter::once(&ADA).chain(PEOPLE).collect();
        assert!((38..=44).contains(&all.len()), "{}", all.len());
        for p in &all {
            assert!(
                (1..=2).contains(&p.teams.len()) || p.email == ADA.email,
                "{}",
                p.email
            );
            for t in p.teams {
                assert!(
                    TEAMS.iter().any(|d| d.name == *t),
                    "{} is in an unknown team {t}",
                    p.email
                );
            }
        }
        for t in TEAMS {
            assert!(
                all.iter().any(|p| p.teams.contains(&t.name)),
                "{} is empty",
                t.name
            );
        }
        let bea = all.iter().find(|p| p.email == "bea@acme.example").unwrap();
        assert_eq!(bea.teams, &["Sales"]);
        let mut emails: Vec<_> = all.iter().map(|p| p.email).collect();
        emails.sort();
        emails.dedup();
        assert_eq!(emails.len(), all.len(), "duplicate email");
        for p in &all {
            assert!(
                SCHEMA_SQL.contains(p.email),
                "{} has no employees row",
                p.email
            );
        }
        assert!(all.iter().any(|p| p.email == OVERRIDE.0));
    }

    #[test]
    fn the_marker_queries_name_the_marker() {
        assert!(PROBE_MARKER.contains(&format!("'{MARKER}'")));
        assert!(MARK_COMPLETE.contains(&format!("'{MARKER}'")));
    }

    /// An owner skips its table's policies unless RLS is forced: every table a team role owns
    /// forces it, so `support`'s region rule filters `support` itself.
    #[test]
    fn every_team_owned_table_forces_rls() {
        let sql = SCHEMA_SQL.to_lowercase();
        let owned: Vec<&str> = sql
            .lines()
            .filter_map(|l| l.strip_prefix("alter table "))
            .filter_map(|l| l.split_once(" owner to "))
            .map(|(table, _)| table)
            .collect();
        assert_eq!(owned.len(), 3, "{owned:?}");
        for table in owned {
            assert!(
                sql.contains(&format!("alter table {table} force row level security;")),
                "{table} is owned by a team role but doesn't force RLS"
            );
        }
    }

    /// Deleting an app user deletes their `public.users` row: everything that hangs off it
    /// goes with it, or the delete fails.
    #[test]
    fn every_reference_to_a_customer_cascades() {
        let sql = SCHEMA_SQL.to_lowercase();
        let refs: Vec<&str> = sql
            .lines()
            .filter(|l| {
                l.contains(" references public.users")
                    || l.contains(" references public.customers")
                    || l.contains(" references sales.orders")
            })
            .collect();
        assert_eq!(refs.len(), 5, "{refs:?}");
        for r in refs {
            assert!(r.contains("on delete cascade"), "{r}");
        }
    }

    /// Staff read customers, and the data API's roles can't write the unprotected directory.
    #[test]
    fn staff_read_customers_and_app_users_cannot_write_employees() {
        let sql = SCHEMA_SQL.to_lowercase();
        assert!(
            sql.contains("on public.customers for select to sales, finance, support"),
            "no staff policy on customers"
        );
        assert!(sql.contains(
            "revoke insert, update, delete, truncate on public.employees from anon, authenticated;"
        ));
        assert!(sql.contains(
            "revoke insert, update, delete, truncate on public.customers from anon, authenticated;"
        ));
    }

    #[test]
    fn the_script_leaves_exactly_one_table_without_rls() {
        let tables = SCHEMA_SQL.matches("create table if not exists").count();
        let rls = SCHEMA_SQL.matches("enable row level security").count();
        assert_eq!(tables, 5);
        assert_eq!(rls, tables - 1);
        assert!(!SCHEMA_SQL.contains("public.employees enable row level security"));
    }

    #[tokio::test]
    async fn a_fresh_instance_gets_the_organisation_and_a_rerun_writes_nothing() {
        let f = Fake::new();
        let out = run(&f).await.unwrap();
        {
            let s = f.0.lock().unwrap();
            let a = &s.acme;
            assert_eq!(a.teams.len(), TEAMS.len());
            let sales = a.teams.iter().find(|t| t.name == "Sales").unwrap();
            assert_eq!(
                sales.grant,
                Some(("developer".into(), vec!["sales".into()]))
            );
            let want = std::iter::once(&ADA)
                .chain(PEOPLE)
                .filter(|p| p.teams.contains(&"Sales"))
                .count();
            assert_eq!(sales.members.len(), want);
            assert_eq!(a.roles.len(), PG_ROLES.len());
            assert_eq!(a.users.len(), CUSTOMERS.len());
            assert!(a.schema_done && a.feature_col && a.protected && a.data_api_on);
            assert_eq!(a.default_schema, DEFAULT_SCHEMA);
            assert_eq!(a.overrides.len(), 1);
            assert_eq!(a.keys, [KEY_NAME]);
            assert_eq!(a.queries.len(), QUERIES.len());
        }
        assert!(out.contains("main is protected"), "{out}");
        let before = f.writes();
        run(&f).await.unwrap();
        assert_eq!(f.writes(), before, "a rerun writes nothing");
        assert_eq!(
            f.count("POST /v1/sql"),
            3,
            "the script, the branch's column, the marker"
        );
    }

    #[tokio::test]
    async fn the_tenant_is_named_after_the_organisation_once() {
        let f = Fake::new();
        run(&f).await.unwrap();
        assert_eq!(
            f.0.lock().unwrap().acme.tenant_name.as_deref(),
            Some(TENANT_NAME)
        );
        assert_eq!(f.count("PATCH /v1/tenant"), 1);
        let before = f.writes();
        run(&f).await.unwrap();
        assert_eq!(f.count("PATCH /v1/tenant"), 1, "a rerun does not rename");
        assert_eq!(f.writes(), before, "a rerun makes zero writes");
        // A name someone gave it after the seed finished is theirs: it stays.
        f.0.lock().unwrap().acme.tenant_name = Some("Other".into());
        run(&f).await.unwrap();
        assert_eq!(f.count("PATCH /v1/tenant"), 1);
        assert_eq!(
            f.0.lock().unwrap().acme.tenant_name.as_deref(),
            Some("Other")
        );
    }

    #[tokio::test]
    async fn a_run_after_completion_writes_nothing_even_when_the_demo_was_changed() {
        let f = Fake::new();
        run(&f).await.unwrap();
        assert!(
            f.0.lock().unwrap().acme.complete,
            "the seed marks itself complete"
        );
        let removed = PEOPLE[3].email;
        let sales_members = {
            let mut s = f.0.lock().unwrap();
            s.acme.protected = false;
            s.acme.tenant_name = Some("Other".into());
            let sales = s.acme.teams.iter_mut().find(|t| t.name == "Sales").unwrap();
            sales.members.pop();
            let left = sales.members.len();
            s.members.remove(removed);
            left
        };
        let before = f.writes();
        let out = run(&f).await.unwrap();
        assert_eq!(
            f.writes(),
            before,
            "a run after completion writes nothing:\n{out}"
        );
        assert!(out.contains("already complete"), "{out}");
        let s = f.0.lock().unwrap();
        assert!(!s.acme.protected, "main stays as the user left it");
        assert_eq!(s.acme.tenant_name.as_deref(), Some("Other"));
        let sales = s.acme.teams.iter().find(|t| t.name == "Sales").unwrap();
        assert_eq!(sales.members.len(), sales_members);
        assert!(!s.members.contains_key(removed), "nobody is invited again");
    }

    #[tokio::test]
    async fn a_run_interrupted_before_the_marker_is_finished_by_the_next() {
        let f = Fake::new();
        // Main is protected, but the answer is lost: the marker is never written.
        f.crash_after("PATCH /v1/branch", 1);
        run(&f).await.unwrap_err();
        {
            let s = f.0.lock().unwrap();
            assert!(s.acme.protected && !s.acme.complete);
        }
        let out = run(&f).await.unwrap();
        assert!(!out.contains("already complete"), "{out}");
        assert!(f.0.lock().unwrap().acme.complete);
        let before = f.writes();
        run(&f).await.unwrap();
        assert_eq!(f.writes(), before);
    }

    #[tokio::test]
    async fn a_run_that_lost_the_markers_answer_is_complete() {
        let f = Fake::new();
        // The third script is the marker: it is applied, then the answer is lost.
        f.crash_after("POST /v1/sql", 3);
        run(&f).await.unwrap_err();
        assert!(f.0.lock().unwrap().acme.complete);
        let before = f.writes();
        let out = run(&f).await.unwrap();
        assert!(out.contains("already complete"), "{out}");
        assert_eq!(f.writes(), before);
    }

    #[tokio::test]
    async fn existing_teams_and_tables_are_skipped() {
        let f = Fake::new();
        run(&f).await.unwrap();
        assert_eq!(f.count("POST /v1/teams"), TEAMS.len());
        // An unfinished seed (no marker) whose Sales team is gone; everything else, tables
        // included, is still there.
        {
            let mut s = f.0.lock().unwrap();
            s.acme.complete = false;
            s.acme.teams.retain(|t| t.name != "Sales");
        }
        run(&f).await.unwrap();
        assert_eq!(f.count("POST /v1/teams"), TEAMS.len() + 1);
        assert_eq!(f.count("PUT /v1/team-grant"), TEAMS.len() + 1);
        assert_eq!(
            f.count("POST /v1/sql"),
            4,
            "the tables exist: no script ran, only the marker again"
        );
        assert_eq!(f.count("POST /v1/roles"), PG_ROLES.len());
        assert_eq!(f.count("POST /v1/users"), CUSTOMERS.len());
    }

    #[tokio::test]
    async fn a_failed_step_stops_the_seed_with_a_clear_error_and_the_next_run_finishes() {
        let f = Fake::new();
        f.0.lock().unwrap().acme.sql_error = Some("42501");
        let e = run(&f).await.unwrap_err();
        assert!(
            e.to_string()
                .contains("SQL error 42501: permission denied for schema sales"),
            "{e:#}"
        );
        {
            let s = f.0.lock().unwrap();
            let a = &s.acme;
            assert!(!a.schema_done && !a.feature && a.keys.is_empty() && a.queries.is_empty());
            assert!(!a.protected);
        }
        run(&f).await.unwrap();
        let s = f.0.lock().unwrap();
        assert!(s.acme.schema_done && s.acme.protected);
    }
}
