//! `chelabase neon …`: helpers for the Neon compute path (roadmap step 3).

pub mod compute_jwt;

use anyhow::Result;
use std::io::Read;

/// The password from stdin. One trailing newline (from `echo` or a heredoc) is dropped;
/// everything else, spaces included, is part of the password.
pub fn password_from(mut input: impl Read) -> Result<String> {
    let mut text = String::new();
    input.read_to_string(&mut text)?;
    let password = match text.strip_suffix('\n') {
        Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
        None => &text,
    };
    anyhow::ensure!(!password.is_empty(), "no password on stdin");
    Ok(password.to_string())
}

/// `chelabase neon verifier`: prints the SCRAM verifier for the password on stdin.
pub fn verifier() -> Result<()> {
    let password = password_from(std::io::stdin().lock())?;
    println!("{}", chelabase_neon::scram::verifier(&password));
    Ok(())
}

/// The line `password-updated` prints. Review Focus 5: nobody receiving the message is a
/// failure, not a success.
pub fn receivers_line(receivers: u64) -> Result<String> {
    anyhow::ensure!(
        receivers > 0,
        "no proxy is subscribed to Valkey, so none dropped the old secret (is the stack up? `just workbench status`)"
    );
    Ok(receivers.to_string())
}

/// `chelabase neon password-updated`: tells the proxies to drop a role's cached secret.
pub async fn password_updated(endpoint: &str, role: &str) -> Result<()> {
    let url =
        std::env::var("CHELABASE_VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let receivers = chelabase_neon::proxy_cache::ProxyCache::new(&url)?
        .password_updated(endpoint, role)
        .await?;
    println!("{}", receivers_line(receivers)?);
    Ok(())
}

/// `chelabase neon compute-args`: compute_ctl's arguments, one per line.
pub fn compute_args(endpoint: &str, control_plane_uri: &str) {
    for arg in chelabase_neon::compute::compute_ctl_args(endpoint, control_plane_uri) {
        println!("{arg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_trailing_newline_is_dropped() {
        assert_eq!(password_from("pw\n".as_bytes()).unwrap(), "pw");
        assert_eq!(password_from("pw\r\n".as_bytes()).unwrap(), "pw");
        assert_eq!(password_from("pw".as_bytes()).unwrap(), "pw");
        assert_eq!(password_from("pw\n\n".as_bytes()).unwrap(), "pw\n");
    }

    #[test]
    fn spaces_are_part_of_the_password() {
        assert_eq!(password_from(" p w \n".as_bytes()).unwrap(), " p w ");
    }

    #[test]
    fn nobody_listening_is_an_error_that_says_why() {
        let err = receivers_line(0).unwrap_err().to_string();
        assert!(err.contains("no proxy"), "{err}");
        assert_eq!(receivers_line(1).unwrap(), "1");
        assert_eq!(receivers_line(3).unwrap(), "3");
    }

    #[test]
    fn an_empty_password_is_refused() {
        assert!(password_from("".as_bytes()).is_err());
        assert!(password_from("\n".as_bytes()).is_err());
    }
}
