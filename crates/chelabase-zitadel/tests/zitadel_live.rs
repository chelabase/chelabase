//! The client against the stack's real Zitadel (#57): what "no such user" looks like.
//!
//! Runs in the workbench runner, which has `demo.env` (the issuer and the admin PAT):
//!
//! ```text
//! just workbench test -p chelabase-zitadel --test zitadel_live -- --run-ignored only
//! ```
//!
//! Calls go to `CHELABASE_ZITADEL_INTERNAL_URL` (else the `zitadel` service) with the issuer's
//! host as `Host`, as the apps send them.

// Test code: plain clients against the stack (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use chelabase_zitadel::client::{NewUser, Zitadel};
use std::path::PathBuf;

#[derive(Clone)]
struct Live {
    base: String,
    host: String,
    pat: String,
}

fn live() -> Live {
    let issuer = std::env::var("CHELABASE_ZITADEL_ISSUER")
        .expect("CHELABASE_ZITADEL_ISSUER is unset (source demo.env)");
    let host = issuer
        .split_once("://")
        .map(|(_, rest)| rest.trim_end_matches('/').to_string())
        .expect("the issuer has a host");
    let base = std::env::var("CHELABASE_ZITADEL_INTERNAL_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://zitadel:8080".into());
    let file = PathBuf::from(
        std::env::var("CHELABASE_ZITADEL_ADMIN_PAT_FILE")
            .expect("CHELABASE_ZITADEL_ADMIN_PAT_FILE is unset (source demo.env)"),
    );
    let file = if file.is_relative() {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(file)
    } else {
        file
    };
    let pat = std::fs::read_to_string(file).unwrap().trim().to_string();
    Live {
        base: base.trim_end_matches('/').to_string(),
        host,
        pat,
    }
}

impl Live {
    fn client(&self) -> Zitadel {
        Zitadel::new(&self.base, &self.pat).with_host(Some(&self.host))
    }

    /// The raw answer to `GET /v2/users/{id}`, to show what Zitadel's not-found looks like.
    async fn raw_get_user(&self, id: &str) -> (u16, String) {
        let resp = reqwest::Client::new()
            .get(format!("{}/v2/users/{id}", self.base))
            .header(reqwest::header::HOST, &self.host)
            .bearer_auth(&self.pat)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.text().await.unwrap())
    }
}

/// A deleted user and a never-existing id are both "no such user" (`Ok(None)`, gone), so the
/// not-found match in `get_user` matches the answer the real Zitadel gives.
#[tokio::test]
#[ignore = "needs the workbench stack and demo.env (Zitadel admin)"]
async fn a_deleted_or_unknown_user_is_not_found() {
    let live = live();
    let z = live.client();
    let tag = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let org = format!("live-zitadel-{tag}");
    z.ensure_org(&org, &org).await.unwrap();
    let email = format!("gone-{tag}@example.com");
    let created = z
        .create_user(&NewUser {
            org_id: &org,
            email: &email,
            given_name: "Gone",
            family_name: "User",
            password: Some("Str0ng!pass-live"),
            user_id: None,
            username: Some(&email),
            send_code: false,
            verified: true,
            url_template: "",
        })
        .await;
    let checks = live.clone();
    let checked = async move {
        let live = checks;
        let z = live.client();
        let id = created.unwrap();
        assert_eq!(z.get_user(&id).await.unwrap().unwrap().id, id);
        assert!(!z.user_is_gone(&id).await.unwrap());
        z.delete_user(&id).await.unwrap();
        let (status, body) = live.raw_get_user(&id).await;
        eprintln!("deleted user: {status} {body}");
        assert_eq!(z.get_user(&id).await.unwrap(), None);
        assert!(z.user_is_gone(&id).await.unwrap());
        let never = format!("{tag}");
        let (status, body) = live.raw_get_user(&never).await;
        eprintln!("never-existing user: {status} {body}");
        assert_eq!(z.get_user(&never).await.unwrap(), None);
        assert!(z.user_is_gone(&never).await.unwrap());
    };
    let outcome = tokio::spawn(checked).await;
    z.delete_org(&org).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}
