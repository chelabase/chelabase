//! Where a proxied request goes: the service's base URL with the request's path rest and query.
//!
//! The rest comes as the client sent it (percent-encoded). Its dot segments are resolved here,
//! within the rest, so the result never leaves the base URL's path prefix: `.` and `..`, in any
//! mix of `.` and `%2e` (a URL parser treats `%2e` as a dot), and a `..` at the top is dropped.
//! A segment holding a backslash or an encoded `/` or `\` (`%2f`, `%5c`, any case) is refused:
//! many upstreams decode before they resolve dot segments, so `%2e%2e%2fadmin` would be
//! `../admin` there. What the URL parser makes of the rest is what is sent and signed
//! ([`Upstream::target`]), and it is checked to still start with the prefix.

use chelabase_http::outbound::Url;

/// A request's upstream URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub url: Url,
}

impl Upstream {
    /// The request-target as sent: path and query.
    pub fn target(&self) -> String {
        match self.url.query() {
            Some(q) => format!("{}?{q}", self.url.path()),
            None => self.url.path().to_owned(),
        }
    }
}

fn dots(segment: &str) -> usize {
    let s = segment.to_ascii_lowercase().replace("%2e", ".");
    match s.as_str() {
        "." => 1,
        ".." => 2,
        _ => 0,
    }
}

/// `base` with `rest` (`""` or starting with `/`) under its path and `query`; `None` when the
/// result would leave the base path, or a segment holds a separator in disguise.
pub fn upstream(base: &Url, rest: &str, query: Option<&str>) -> Option<Upstream> {
    let prefix = base.path().trim_end_matches('/');
    let mut segments: Vec<String> = Vec::new();
    let parts: Vec<&str> = match rest.strip_prefix('/') {
        Some(r) => r.split('/').collect(),
        None if rest.is_empty() => Vec::new(),
        None => return None,
    };
    let disguised = |p: &&str| {
        let p = p.to_ascii_lowercase();
        p.contains('\\') || p.contains("%2f") || p.contains("%5c")
    };
    if parts.iter().any(disguised) {
        return None;
    }
    let last = parts.len().saturating_sub(1);
    for (i, part) in parts.iter().enumerate() {
        match dots(part) {
            1 => {}
            2 => {
                segments.pop();
            }
            _ => segments.push((*part).to_owned()),
        }
        // A trailing dot segment still ends in a slash (`/a/..` is `/`).
        if i == last && dots(part) > 0 {
            segments.push(String::new());
        }
    }
    let path = if segments.is_empty() && !parts.is_empty() {
        format!("{prefix}/")
    } else if segments.is_empty() {
        if prefix.is_empty() {
            "/".to_owned()
        } else {
            prefix.to_owned()
        }
    } else {
        format!("{prefix}/{}", segments.join("/"))
    };
    let mut url = base.clone();
    url.set_path(&path);
    url.set_query(query);
    url.set_fragment(None);
    let sent = url.path();
    let inside = sent == prefix
        || sent
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
        || prefix.is_empty();
    inside.then_some(Upstream { url })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(base: &str, rest: &str, query: Option<&str>) -> Option<String> {
        upstream(&Url::parse(base).unwrap(), rest, query).map(|u| u.target())
    }

    #[test]
    fn the_rest_and_query_go_under_the_base_path() {
        let api = "https://svc.example/api";
        assert_eq!(
            target("http://svc:8080", "/a/b", Some("x=1")).as_deref(),
            Some("/a/b?x=1")
        );
        assert_eq!(target("http://svc:8080/", "", None).as_deref(), Some("/"));
        assert_eq!(target(api, "", None).as_deref(), Some("/api"));
        assert_eq!(target(api, "/", None).as_deref(), Some("/api/"));
        assert_eq!(
            target("https://svc.example/api/", "/a/", None).as_deref(),
            Some("/api/a/")
        );
        assert_eq!(
            target(api, "/a%20b/c%3F", Some("q=%20")).as_deref(),
            Some("/api/a%20b/c%3F?q=%20")
        );
        assert_eq!(target(api, "/a//b", None).as_deref(), Some("/api/a//b"));
    }

    /// Dot segments in every spelling resolve within the rest: the path never leaves `/api`.
    #[test]
    fn dot_segments_never_leave_the_prefix() {
        let api = "https://svc.example/api";
        for (rest, want) in [
            ("/..", "/api/"),
            ("/../secret", "/api/secret"),
            ("/a/../../secret", "/api/secret"),
            ("/%2e%2e/secret", "/api/secret"),
            ("/%2E%2e/%2e./.%2E/secret", "/api/secret"),
            ("/a/%2e/b", "/api/a/b"),
            ("/a/b/..", "/api/a/"),
        ] {
            let got = target(api, rest, Some("x=1")).unwrap();
            assert_eq!(got, format!("{want}?x=1"), "{rest}");
            assert!(got.starts_with("/api/"), "{rest}: {got}");
        }
    }

    /// Audit B2: an encoded `/` or `\` (any case), or a literal `\`, would let an upstream that
    /// decodes before resolving dot segments leave the prefix: such a rest is refused.
    #[test]
    fn encoded_separators_are_refused() {
        let api = "https://svc.example/api";
        for rest in [
            "/%2e%2e%2fadmin",
            "/..%2fadmin",
            "/..%2Fadmin",
            "/..%5cadmin",
            "/..%5Cadmin",
            "/a%2fb",
            "/a\\..\\..\\secret",
            "/x/%2E%2E%2F%2E%2E%2Fadmin",
        ] {
            assert_eq!(target(api, rest, None), None, "{rest}");
        }
    }

    /// The URL parser's own reading is what is sent; a rest that isn't a path is refused.
    #[test]
    fn what_is_sent_is_what_is_signed() {
        let base = Url::parse("https://svc.example/api").unwrap();
        let u = upstream(&base, "/a b/\"c\"", Some("x=a b")).unwrap();
        assert_eq!(
            u.target(),
            format!("{}?{}", u.url.path(), u.url.query().unwrap())
        );
        assert!(u.url.path().starts_with("/api/"));
        assert!(upstream(&base, "a", None).is_none());
        assert_eq!(
            upstream(&base, "/x", None).unwrap().url.host_str(),
            Some("svc.example")
        );
    }
}
