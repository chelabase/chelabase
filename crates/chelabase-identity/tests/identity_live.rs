//! The same scenarios as tests/fake.rs, through the real client against a running identity
//! service. Skipped (they pass vacuously) unless `CHELABASE_IDENTITY_URL` is set:
//!
//! ```text
//! CHELABASE_IDENTITY_URL=http://127.0.0.1:8080 \
//! CHELABASE_IDENTITY_TOKEN_FILE=tokens/auth-identity.token \
//! CHELABASE_IDENTITY_WORKER_TOKEN_FILE=tokens/worker-identity.token \
//!   cargo nextest run -p chelabase-identity --run-ignored only identity_live
//! ```
//!
//! Pools are made and dropped with the worker's token; everything else uses the auth token.
//! Every scenario drops the pools it made when it ends, passed or failed (each pool is a schema
//! in the stack's metadata database). The code scenarios need m2's routes and fail without them.

mod common;

use chelabase_identity::{Identity, IdentityError, Missing, Pool};
use common::*;

struct Live {
    auth: Identity,
    worker: Identity,
}

fn live() -> Option<Live> {
    let url = std::env::var("CHELABASE_IDENTITY_URL")
        .ok()
        .filter(|v| !v.is_empty())?;
    let file = |var: &str| {
        std::path::PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var} is unset")))
    };
    Some(Live {
        auth: Identity::from_token_file(&url, &file("CHELABASE_IDENTITY_TOKEN_FILE")).unwrap(),
        worker: Identity::from_token_file(&url, &file("CHELABASE_IDENTITY_WORKER_TOKEN_FILE"))
            .unwrap(),
    })
}

/// Runs `body` to the end (a panic included), then drops `pools` with the worker's token
/// (a pool `body` never made, or already dropped, is fine), then fails as `body` did.
async fn dropping_pools<Fut>(worker: &Identity, pools: &[Pool], body: Fut)
where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let outcome = tokio::spawn(body).await;
    for p in pools {
        let _ = worker.drop_pool(p).await;
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// Makes the pools with the worker's token, runs `f`, then drops them, whatever happened.
async fn with_pools<F, Fut>(n: usize, f: F)
where
    F: FnOnce(Identity, Vec<Pool>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(l) = live() else {
        eprintln!("CHELABASE_IDENTITY_URL is unset: skipped");
        return;
    };
    let pools: Vec<Pool> = (0..n).map(|_| app_pool()).collect();
    let (worker, auth, made) = (l.worker.clone(), l.auth.clone(), pools.clone());
    dropping_pools(&l.worker, &pools, async move {
        for p in &made {
            worker.create_pool(p).await.unwrap();
        }
        f(auth, made).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_pool_lifecycle_and_roles() {
    let Some(l) = live() else { return };
    let p = app_pool();
    let (auth, worker) = (l.auth.clone(), l.worker.clone());
    dropping_pools(&l.worker, &[p], async move {
        assert!(
            matches!(auth.create_pool(&p).await, Err(IdentityError::Forbidden(_))),
            "auth may not make pools"
        );
        worker.create_pool(&p).await.unwrap();
        worker.create_pool(&p).await.unwrap();
        assert_eq!(
            auth.get_user(&p, "nobody").await.unwrap_err(),
            IdentityError::NotFound(Missing::User)
        );
        worker.drop_pool(&p).await.unwrap();
        assert_eq!(
            auth.get_user(&p, "nobody").await.unwrap_err(),
            IdentityError::NotFound(Missing::Pool)
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_same_email_two_pools() {
    with_pools(2, |id, p| async move {
        common::same_email_two_pools(&id, &p[0], &p[1]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_ban() {
    with_pools(1, |id, p| async move {
        common::ban_blocks_sign_in_and_kills_sessions(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_revoke_all() {
    with_pools(
        1,
        |id, p| async move { common::revoke_all(&id, &p[0]).await },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_drop_pool_isolated() {
    let Some(l) = live() else { return };
    with_pools(2, |id, p| async move {
        common::drop_pool_isolated(&id, &l.worker, &p[0], &p[1]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_user_lifecycle() {
    with_pools(1, |id, p| async move {
        common::user_lifecycle(&id, &p[0]).await
    })
    .await;
}

/// m2's scenarios. They fail when the service lacks the code routes; they do not skip.
async fn with_codes<F, Fut>(n: usize, f: F)
where
    F: FnOnce(Identity, Vec<Pool>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    if live().is_none() {
        eprintln!("CHELABASE_IDENTITY_URL is unset: skipped");
        return;
    }
    with_pools(n, f).await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_code_single_use() {
    with_codes(1, |id, p| async move {
        common::code_single_use(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_code_lockout() {
    with_codes(
        1,
        |id, p| async move { common::code_lockout(&id, &p[0]).await },
    )
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_code_cross_pool() {
    with_codes(2, |id, p| async move {
        common::code_cross_pool(&id, &p[0], &p[1]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_reset_revokes_sessions() {
    with_codes(1, |id, p| async move {
        common::reset_revokes_sessions(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_code_strictness() {
    with_codes(1, |id, p| async move {
        common::code_strictness(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_email_change_kills_codes() {
    with_codes(1, |id, p| async move {
        common::email_change_kills_codes(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_invite_refused_for_established_account() {
    with_codes(1, |id, p| async move {
        common::invite_refused_for_established_account(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_invite_works_for_preregistered_account() {
    with_codes(1, |id, p| async move {
        common::invite_works_for_preregistered_account(&id, &p[0]).await
    })
    .await;
}

#[tokio::test]
#[ignore = "needs CHELABASE_IDENTITY_URL"]
async fn identity_live_email_status() {
    let Some(l) = live() else { return };
    // Answers whether or not SMTP is configured.
    l.auth.email_status().await.unwrap();
}
