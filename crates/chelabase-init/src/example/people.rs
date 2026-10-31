//! The demo people and the seed: Ada claims the instance, then invites and admits the others.

use super::session::{Http, InviteLink, Secret, Session, SignIn, Token, call_error};
use super::{Config, DEMO_PASSWORD};
use anyhow::{Result, anyhow, bail};
use reqwest::Method;
use serde_json::{Value, json};
use std::io::Write;
use std::time::Duration;

/// A demo person.
#[derive(Debug, Clone, Copy)]
pub struct Person {
    pub email: &'static str,
    pub given: &'static str,
    pub family: &'static str,
    pub department: &'static str,
}

/// The tenant admin, who claims the instance.
pub const ADA: Person = Person {
    email: "ada@acme.example",
    given: "Ada",
    family: "Lovelace",
    department: "Engineering",
};

/// Everyone Ada invites.
pub const PEOPLE: &[Person] = &[
    Person {
        email: "grace@acme.example",
        given: "Grace",
        family: "Hopper",
        department: "Engineering",
    },
    Person {
        email: "katherine@acme.example",
        given: "Katherine",
        family: "Johnson",
        department: "Data",
    },
    Person {
        email: "hedy@acme.example",
        given: "Hedy",
        family: "Lamarr",
        department: "Product",
    },
    Person {
        email: "alan@acme.example",
        given: "Alan",
        family: "Turing",
        department: "Security",
    },
    Person {
        email: "rosalind@acme.example",
        given: "Rosalind",
        family: "Franklin",
        department: "Finance",
    },
];

const RESET: &str = "reset with `down -v`";

/// Claims the instance as Ada (or signs her in) and makes every demo person an active member.
/// `second` is how long a `Retry-After` second lasts (shorter in tests).
pub async fn seed(
    cfg: &Config,
    http: &impl Http,
    second: Duration,
    out: &mut dyn Write,
) -> Result<()> {
    let s = Session::connect(http, cfg.entry.clone(), cfg.timeout, second).await?;
    let mut writes = 0;
    let instance = s
        .public(
            Method::GET,
            "/auth/v1/instance",
            None,
            "GET /auth/v1/instance",
        )
        .await?;
    // `InstanceInfo` is camelCase; a missing answer is an error, never "claimed".
    let setup_required = instance
        .get("setupRequired")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("GET /auth/v1/instance: no setupRequired"))?;
    let ada = if setup_required {
        // `setupRequired` stays true until Ada's first bearer request makes her member row, so
        // a run that stopped after her sign-up finds her account already there: sign her in
        // (no write) and sign up only when that is refused. Her `/v1/me` below then claims the
        // tenant admin through the bound claim, or the check there fails a lost race.
        match s.sign_in(ADA.email, DEMO_PASSWORD).await? {
            SignIn::Token(token) => token,
            SignIn::Refused => {
                let code = read_setup_code(cfg)?;
                writes += 1;
                let token = s
                    .sign_up(
                        ADA.email,
                        DEMO_PASSWORD,
                        (ADA.given, ADA.family),
                        Some(&code),
                    )
                    .await?;
                writeln!(out, "example: signed up {} with the setup code", ADA.email)?;
                token
            }
        }
    } else {
        // Never a sign-up here: on a claimed instance it could only make a newcomer.
        match s.sign_in(ADA.email, DEMO_PASSWORD).await? {
            SignIn::Token(token) => token,
            SignIn::Refused => bail!("instance already claimed ({RESET})"),
        }
    };
    let me = s
        .api(Method::GET, "/v1/me", &ada, None, "GET /v1/me")
        .await?;
    if !active(&me) || me.pointer("/member/tenant_admin") != Some(&json!(true)) {
        bail!(
            "{} is not this instance's tenant admin: someone else claimed it first ({RESET})",
            ADA.email
        );
    }
    let members = list(&s, &ada).await?;
    for p in PEOPLE {
        let member = members
            .iter()
            .find(|m| m.get("email").and_then(Value::as_str) == Some(p.email));
        let status = member.and_then(|m| m.get("status")).and_then(Value::as_str);
        let token = match status {
            Some("active") => continue,
            None => {
                writes += 1;
                let body = json!({"email": p.email, "delivery": "link"});
                let made = s
                    .api(
                        Method::POST,
                        "/v1/invites",
                        &ada,
                        Some(body),
                        "POST /v1/invites",
                    )
                    .await?;
                if made.get("outcome").and_then(Value::as_str) != Some("invited") {
                    continue;
                }
                writes += 1;
                s.accept_invite(&link(&made)?, DEMO_PASSWORD).await?
            }
            Some("invited") => {
                let id = member
                    .and_then(|m| m.pointer("/invite/id"))
                    .and_then(Value::as_str)
                    .filter(|id| id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                    .ok_or_else(|| {
                        anyhow!("{} is invited without an open invite ({RESET})", p.email)
                    })?;
                admit_invited(&s, &ada, p, id, &mut writes).await?
            }
            Some(other) => bail!("{} is {other} ({RESET})", p.email),
        };
        // Their first request makes the invited member active.
        let me = s
            .api(Method::GET, "/v1/me", &token, None, "GET /v1/me")
            .await?;
        if !active(&me) {
            bail!("{} accepted the invite but is not active", p.email);
        }
        writeln!(out, "example: {} joined", p.email)?;
    }
    let members = list(&s, &ada).await?;
    for p in std::iter::once(&ADA).chain(PEOPLE) {
        let ok = members.iter().any(|m| {
            m.get("email").and_then(Value::as_str) == Some(p.email)
                && m.get("status") == Some(&json!("active"))
        });
        if !ok {
            bail!("{} is not an active member after the seed", p.email);
        }
    }
    writeln!(
        out,
        "example: {writes} writes. LOCAL DEMO ONLY: these accounts all sign in with the password {DEMO_PASSWORD}"
    )?;
    writeln!(out, "example:   {:<26} tenant admin", ADA.email)?;
    for p in PEOPLE {
        writeln!(out, "example:   {:<26} {}", p.email, p.department)?;
    }
    Ok(())
}

/// An invited person: a new link (the old one was lost with the run that made it) and the
/// accept; or, when they already set a password (the run stopped after the accept), a sign-in.
async fn admit_invited<H: Http>(
    s: &Session<'_, H>,
    ada: &Token,
    p: &Person,
    invite: &str,
    writes: &mut usize,
) -> Result<Token> {
    *writes += 1;
    let path = format!("/v1/invites/{invite}/resend");
    let body = json!({"delivery": "link"});
    match s
        .api(
            Method::POST,
            &path,
            ada,
            Some(body),
            "POST /v1/invites/{id}/resend",
        )
        .await
    {
        Ok(resent) => {
            *writes += 1;
            s.accept_invite(&link(&resent)?, DEMO_PASSWORD).await
        }
        Err(e) if call_error(&e).is_some_and(|c| c.code.as_deref() == Some("link_not_allowed")) => {
            match s.sign_in(p.email, DEMO_PASSWORD).await? {
                SignIn::Token(token) => Ok(token),
                SignIn::Refused => bail!(
                    "{} can sign in, but not with the demo password ({RESET})",
                    p.email
                ),
            }
        }
        Err(e) => Err(e),
    }
}

fn link(answer: &Value) -> Result<InviteLink> {
    let url = answer
        .get("invite_url")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("the invite answer has no invite_url"))?;
    InviteLink::parse(url)
}

fn active(me: &Value) -> bool {
    me.pointer("/member/status") == Some(&json!("active"))
}

async fn list<H: Http>(s: &Session<'_, H>, ada: &Token) -> Result<Vec<Value>> {
    match s
        .api(Method::GET, "/v1/members", ada, None, "GET /v1/members")
        .await?
    {
        Value::Array(members) => Ok(members),
        _ => bail!("GET /v1/members: not a list"),
    }
}

/// The one secret file the seed mounts. Its errors name the path, never the contents.
fn read_setup_code(cfg: &Config) -> Result<Secret> {
    use chelabase_types::file_value::{FileValue, Sensitivity};
    let file = FileValue::file(
        "CHELABASE_SETUP_CODE_FILE",
        &cfg.setup_code_file,
        Sensitivity::Secret,
    );
    let code = file.get().map_err(|e| anyhow!("{e}"))?.trim();
    if code.is_empty() {
        bail!(
            "CHELABASE_SETUP_CODE_FILE: {} is empty",
            cfg.setup_code_file.display()
        );
    }
    Ok(Secret::new(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::example::fake::{self, Fake};

    async fn run(f: &Fake) -> Result<String> {
        let mut out = Vec::new();
        let r = seed(&fake::config(), f, Duration::from_millis(1), &mut out).await;
        let text = String::from_utf8(out).unwrap();
        r.map(|()| text)
    }

    fn all_active(f: &Fake) {
        let s = f.0.lock().unwrap();
        let ada = &s.members[ADA.email];
        assert!(ada.admin && ada.status == "active", "{ada:?}");
        for p in PEOPLE {
            assert_eq!(
                s.members.get(p.email).map(|m| m.status),
                Some("active"),
                "{}",
                p.email
            );
            assert!(!s.members[p.email].admin);
        }
        assert_eq!(s.members.len(), PEOPLE.len() + 1);
    }

    #[tokio::test]
    async fn a_fresh_instance_is_claimed_and_seeded_and_a_rerun_writes_nothing() {
        let f = Fake::new();
        let out = run(&f).await.unwrap();
        all_active(&f);
        assert_eq!(f.0.lock().unwrap().claimed_by.as_deref(), Some(ADA.email));
        assert_eq!(f.count("POST /auth/v1/sign-up"), 1);
        assert!(out.contains("LOCAL DEMO ONLY"), "{out}");
        for p in PEOPLE {
            assert!(out.contains(p.email), "{out}");
        }
        let before = f.writes();
        run(&f).await.unwrap();
        assert_eq!(f.writes(), before, "a rerun writes nothing");
        assert_eq!(f.count("POST /auth/v1/sign-up"), 1);
    }

    #[tokio::test]
    async fn a_crash_after_invite_two_is_finished_by_the_next_run() {
        let f = Fake::new();
        f.crash_after("POST /v1/invites", 2);
        run(&f).await.unwrap_err();
        assert_eq!(f.status(PEOPLE[1].email), Some("invited"));
        run(&f).await.unwrap();
        all_active(&f);
        let before = f.writes();
        run(&f).await.unwrap();
        assert_eq!(f.writes(), before);
    }

    #[tokio::test]
    async fn a_crash_after_an_accept_is_finished_by_signing_in() {
        let f = Fake::new();
        f.crash_after("POST /auth/v1/accept-invite", 1);
        run(&f).await.unwrap_err();
        assert_eq!(f.status(PEOPLE[0].email), Some("invited"));
        run(&f).await.unwrap();
        all_active(&f);
    }

    /// The run dies after `key`'s first call is applied; the claim is bound but Ada has no
    /// member row yet, so the instance still answers `setupRequired: true`. The rerun signs her
    /// in (her account exists) instead of signing up again into `409 email_taken`.
    async fn a_crash_before_adas_first_request_recovers(key: &str) {
        let f = Fake::new();
        f.crash_after(key, 1);
        run(&f).await.unwrap_err();
        {
            let s = f.0.lock().unwrap();
            assert_eq!(s.claimed_by.as_deref(), Some(ADA.email));
            assert!(s.members.is_empty(), "{:?}", s.members);
        }
        let rerun = run(&f).await;
        assert!(rerun.is_ok(), "{:#}", rerun.unwrap_err());
        all_active(&f);
        assert_eq!(f.count("POST /auth/v1/sign-up"), 1);
        let before = f.writes();
        run(&f).await.unwrap();
        assert_eq!(f.writes(), before);
    }

    #[tokio::test]
    async fn a_crash_after_adas_sign_up_before_her_first_request_recovers() {
        a_crash_before_adas_first_request_recovers("POST /auth/v1/sign-up").await;
    }

    #[tokio::test]
    async fn a_crash_during_adas_token_exchange_recovers() {
        a_crash_before_adas_first_request_recovers("POST /oauth/v2/token").await;
    }

    #[tokio::test]
    async fn a_claimed_instance_gets_no_sign_up_even_when_sign_up_is_open() {
        let f = Fake::new();
        f.claim_as("someone@else.example");
        f.0.lock().unwrap().open_sign_up = true;
        let e = run(&f).await.unwrap_err();
        assert!(
            e.to_string()
                .contains("instance already claimed (reset with `down -v`)"),
            "{e}"
        );
        assert_eq!(f.count("POST /auth/v1/sign-up"), 0);
        assert_eq!(f.writes(), 0);
    }

    #[tokio::test]
    async fn a_claim_bound_to_someone_else_fails_before_any_invite() {
        let f = Fake::new();
        // The race: the instance looks unclaimed, but someone binds it before Ada's sign-up.
        f.0.lock().unwrap().open_sign_up = true;
        let raced = RaceFake(&f);
        let mut out = Vec::new();
        let e = seed(&fake::config(), &raced, Duration::from_millis(1), &mut out)
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("not this instance's tenant admin"),
            "{e}"
        );
        assert_eq!(f.count("POST /v1/invites"), 0);
    }

    /// Claims the instance for someone else right after `GET /auth/v1/instance` answers.
    struct RaceFake<'a>(&'a Fake);

    impl crate::example::session::Http for RaceFake<'_> {
        async fn send(
            &self,
            r: &crate::example::session::Request,
        ) -> Result<crate::example::session::Response> {
            let a = self.0.send(r).await;
            if r.path == "/auth/v1/instance" && self.0.0.lock().unwrap().claimed_by.is_none() {
                self.0.claim_as("first@else.example");
            }
            a
        }
    }
}
