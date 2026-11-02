//! W2 end-of-step review fix round: failing tests first.

use super::env::{StackEnvInput, render_stack_env};
use std::collections::HashMap;
use std::path::Path;

fn render(slug: &str) -> HashMap<String, String> {
    let input = StackEnvInput {
        slug,
        project: "chelabase-feat-x",
        checkout: Path::new("/m/.worktrees/feat-x"),
        main: Path::new("/m"),
        state: Path::new("/m/.chelabase/stacks/feat-x"),
        gateway_port: 8137,
        proxy_port: 4537,
        uid: 1000,
        gid: 1000,
        docker_gid: 973,
        image: "chelabase-workbench:abc",
        extra: &[],
        control_slot: 1,
    };
    render_stack_env("CHELABASE_PUBLIC_SCHEME=http\n", &input)
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect()
}

// C1: the gateway starts before the first bootstrap (bootstrap reaches Zitadel through it), so
// every machine-level key path an app needs at startup is in stack.env, not only in demo.env.
#[test]
fn the_machine_keys_are_in_the_stack_env_before_any_bootstrap() {
    let env = render("feat-x");
    for (k, v) in [
        ("CHELABASE_INTERNAL_SIGNING_KEY", "/m/.chelabase/gateway/internal-signing.pem"),
        ("CHELABASE_INTERNAL_VERIFY_KEY", "/m/.chelabase/gateway/internal-verify.pem"),
        ("CHELABASE_SECRETS_KEY", "/m/.chelabase/secrets/secrets.key"),
        ("CHELABASE_AUTH_COOKIE_KEY", "/m/.chelabase/auth/cookie.key"),
        ("CHELABASE_APP_JWT_PRIVATE_KEY", "/m/.chelabase/auth/app-jwt.key"),
        ("CHELABASE_APP_JWT_PUBLIC_KEY", "/m/.chelabase/auth/app-jwt.pub"),
        ("CHELABASE_MEMBER_JWT_PRIVATE_KEY", "/m/.chelabase/auth/member-jwt.key"),
        ("CHELABASE_MEMBER_JWT_PUBLIC_KEY", "/m/.chelabase/auth/member-jwt.pub"),
        (
            "CHELABASE_ZITADEL_JWKS_URL",
            "http://auth.feat-x.chelabase.localhost:8137/oauth/v2/keys",
        ),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
}
