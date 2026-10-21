//! The gateway's log filter.
//!
//! Pingora's debug and trace logs print whole requests (a realtime ticket in the query string,
//! `Authorization` headers), and the gateway's `request_summary` override only covers its error
//! logs. So Pingora's targets (`pingora*`) are capped at `warn` whatever `RUST_LOG` says.

use tracing_subscriber::EnvFilter;

/// What runs when `RUST_LOG` is unset or empty.
const DEFAULT: &str = "gateway=info,chelabase_auth=info";

/// The cap on every Pingora target. A directive's target matches by prefix, and the longest
/// target wins, so `pingora` outranks any global level and every shorter prefix.
const PINGORA_CAP: &str = "pingora=warn";

/// The gateway's filter for `RUST_LOG = rust_log` (`main` passes the variable's value).
///
/// Directives naming a Pingora target are dropped (they would be more specific than the cap),
/// and so are span directives (`[span]=level`, `target[span]=level`): those enable every event
/// inside a matching span, Pingora's included. Unparsable directives are ignored.
pub fn env_filter(rust_log: Option<&str>) -> EnvFilter {
    let rust_log = rust_log.map(str::trim).filter(|s| !s.is_empty());
    let kept: Vec<&str> = match rust_log {
        None => vec![DEFAULT],
        Some(spec) => spec
            .split(',')
            .map(str::trim)
            .filter(|d| !d.is_empty() && !d.contains('[') && !names_pingora(d))
            .collect(),
    };
    let mut directives = kept.join(",");
    if !directives.is_empty() {
        directives.push(',');
    }
    directives.push_str(PINGORA_CAP);
    EnvFilter::builder().parse_lossy(directives)
}

/// Whether directive `d` (`target=level`, `target` or a bare level) names a target that
/// starts with `pingora`.
fn names_pingora(d: &str) -> bool {
    let target = d.split_once('=').map_or(d, |(t, _)| t).trim();
    target.starts_with("pingora")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pingora_directives_and_span_directives_are_dropped() {
        let f = env_filter(Some(
            "debug,pingora_core=trace,[req]=trace,gateway[x]=trace,bogus==",
        ));
        let s = f.to_string();
        assert!(!s.contains("trace"), "{s}");
        assert!(s.contains("pingora=warn"), "{s}");
    }

    #[test]
    fn an_empty_rust_log_is_the_default() {
        assert_eq!(
            env_filter(Some("  ")).to_string(),
            env_filter(None).to_string()
        );
    }
}
