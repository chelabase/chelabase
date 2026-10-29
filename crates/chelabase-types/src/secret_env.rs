//! Secrets from the environment: `NAME` itself, or the file `NAME_FILE` names (RT1 T1, S1/S27:
//! the stack mounts each service's own secret files and sets `NAME_FILE`; nothing secret goes
//! in the shared `stack.env`).

/// `NAME`, or the contents of the file at `NAME_FILE` without its trailing line break. Blank
/// values count as unset. Both set, an unreadable file or an empty one is an error naming the
/// variables and the path, never a secret.
pub fn lookup_secret(
    lookup: impl Fn(&str) -> Option<String>,
    name: &str,
) -> Result<Option<String>, String> {
    let file_var = format!("{name}_FILE");
    let get = |n: &str| lookup(n).filter(|v| !v.trim().is_empty());
    match (get(name), get(&file_var)) {
        (Some(_), Some(_)) => Err(format!("set {name} or {file_var}, not both")),
        (Some(value), None) => Ok(Some(value.trim().to_string())),
        (None, Some(path)) => {
            let path = path.trim();
            let text =
                std::fs::read_to_string(path).map_err(|e| format!("{file_var}={path}: {e}"))?;
            let value = text.trim_end_matches(['\n', '\r']);
            if value.trim().is_empty() {
                return Err(format!("{file_var}={path} is empty"));
            }
            Ok(Some(value.to_string()))
        }
        (None, None) => Ok(None),
    }
}

/// `url_var`'s URL, with its password from the file `file_var` names when that is set: put in
/// the URL's user info, percent-encoded (`postgres://user@host/db` becomes
/// `postgres://user:pw@host/db`), so the password is never in the environment. The file is
/// read with [`FileValue`](crate::file_value::FileValue)'s rules as a
/// [`SharedSecret`](crate::file_value::Sensitivity::SharedSecret) (a regular file, no symlink,
/// not readable by others, not empty). `None` when `url_var` is unset or blank.
///
/// Refused, naming the variables and never the password: a URL that carries a password as
/// well as the file, a URL with no user for it, a missing or refused file.
pub fn url_with_password_file(
    lookup: impl Fn(&str) -> Option<String>,
    url_var: &str,
    file_var: &str,
) -> Result<Option<String>, String> {
    use crate::file_value::{FileValue, Sensitivity, Unavailable};
    let get = |n: &str| lookup(n).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let Some(url) = get(url_var) else {
        return Ok(None);
    };
    let Some(path) = get(file_var) else {
        return Ok(Some(url));
    };
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url_var} is not a URL"))?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        return Err(format!("{url_var} names no user for the password in {file_var}"));
    };
    let user = &rest[..at];
    // A password in the user info, or in a `password` query parameter (which sqlx and libpq
    // take over the user info): either would silently win over, or sit beside, the file.
    if user.contains(':') || query_pairs(rest).any(|(key, _)| is_password_key(key)) {
        return Err(format!(
            "{url_var} carries a password and {file_var} is set: give the password in one of them"
        ));
    }
    let password = match FileValue::file(file_var, &path, Sensitivity::SharedSecret).get() {
        Ok(p) => p.to_string(),
        Err(Unavailable::NotYet { .. }) => {
            return Err(format!("{file_var}={path} is missing or empty"));
        }
        Err(e) => return Err(e.to_string()),
    };
    Ok(Some(format!("{scheme}://{user}:{}{}", percent_encode(&password), &rest[at..])))
}

/// The `key=value` pairs of a URL's query (`rest` is the URL after `scheme://`), raw.
fn query_pairs(rest: &str) -> impl Iterator<Item = (&str, &str)> {
    let query = rest.split_once('?').map(|(_, q)| q).unwrap_or("");
    let query = query.split('#').next().unwrap_or("");
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| p.split_once('=').unwrap_or((p, "")))
}

/// Whether a raw query key is `password` (percent-decoded, any case).
fn is_password_key(raw: &str) -> bool {
    percent_decode(raw).eq_ignore_ascii_case("password")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b'+', ..) => {
                out.push(b' ');
                i += 1;
            }
            (b, ..) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `url` for a log or `Debug`: the password in its user info and the value of any `password`
/// query parameter become `<redacted>`; everything else is kept.
pub fn redact_url_passwords(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<redacted>".into();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rfind('@') {
        Some(at) if authority[..at].contains(':') => {
            let user = authority[..at].split(':').next().unwrap_or_default();
            format!("{user}:<redacted>{}", &authority[at..])
        }
        _ => authority.to_string(),
    };
    let (before_fragment, fragment) = match tail.find('#') {
        Some(i) => tail.split_at(i),
        None => (tail, ""),
    };
    let tail = match before_fragment.split_once('?') {
        Some((path, query)) => {
            let query: Vec<String> = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((key, _)) if is_password_key(key) => format!("{key}=<redacted>"),
                    _ => pair.to_string(),
                })
                .collect();
            format!("{path}?{}", query.join("&"))
        }
        None => before_fragment.to_string(),
    };
    format!("{scheme}://{authority}{tail}{fragment}")
}

/// Every byte but the unreserved ones (`A-Z a-z 0-9 - . _ ~`) as `%XX`, as the bootstrap's
/// `urlencoding::encode` does.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// [`lookup_secret`] on the process environment.
pub fn env_secret(name: &str) -> Result<Option<String>, String> {
    lookup_secret(|n| std::env::var(n).ok(), name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn file(contents: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chelabase-secret-env-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, contents).unwrap();
        path
    }

    // m5 re-audit S2: a `password` query parameter (sqlx and libpq take it, and it wins over
    // the user info) is a password in the URL too: with the file set it is refused, naming
    // both variables and never the password. Any spelling, any position, percent-encoded keys.
    #[test]
    fn a_query_password_and_the_file_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let path = file("from-file\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let path = path.to_str().unwrap().to_string();
        for url in [
            "postgres://chelabase@db:5432/m?password=in-url-pw",
            "postgres://chelabase@db:5432/m?sslmode=disable&password=in-url-pw",
            "postgres://chelabase@db:5432/m?%70assword=in-url-pw",
            "postgres://chelabase@db/m?application_name=x&password=in-url-pw#frag",
        ] {
            let got = url_with_password_file(vars(&[("U", url), ("F", &path)]), "U", "F");
            let err = match got {
                Err(e) => e,
                Ok(v) => panic!("{url}: accepted a URL with a query password and the file: {v:?}"),
            };
            assert!(err.contains("U ") && err.contains("F"), "{url}: {err}");
            assert!(!err.contains("in-url-pw") && !err.contains("from-file"), "{url}: {err}");
        }
        // Another key that merely contains the word is fine.
        let ok = url_with_password_file(
            vars(&[("U", "postgres://chelabase@db/m?passwordless=1"), ("F", &path)]),
            "U",
            "F",
        )
        .unwrap();
        assert_eq!(ok.as_deref(), Some("postgres://chelabase:from-file@db/m?passwordless=1"));
    }

    #[test]
    fn passwords_are_percent_encoded_like_the_bootstrap() {
        assert_eq!(percent_encode("p@ss/w:rd%Zz ~-._é"), "p%40ss%2Fw%3Ard%25Zz%20~-._%C3%A9");
    }

    #[test]
    fn the_value_or_the_file() {
        assert_eq!(lookup_secret(vars(&[]), "T").unwrap(), None);
        assert_eq!(
            lookup_secret(vars(&[("T", " v ")]), "T")
                .unwrap()
                .as_deref(),
            Some("v")
        );
        let f = file("from-file\n");
        let got = lookup_secret(vars(&[("T_FILE", f.to_str().unwrap())]), "T").unwrap();
        assert_eq!(got.as_deref(), Some("from-file"));
        // Blank counts as unset, on either form.
        let got = lookup_secret(vars(&[("T", "  "), ("T_FILE", f.to_str().unwrap())]), "T");
        assert_eq!(got.unwrap().as_deref(), Some("from-file"));
    }

    #[test]
    fn both_forms_an_empty_or_a_missing_file_are_refused_without_the_secret() {
        let f = file("file-secret");
        let err = lookup_secret(
            vars(&[("T", "env-secret"), ("T_FILE", f.to_str().unwrap())]),
            "T",
        )
        .unwrap_err();
        assert!(err.contains("T_FILE") && !err.contains("secret-"), "{err}");
        let empty = file("\n");
        assert!(lookup_secret(vars(&[("T_FILE", empty.to_str().unwrap())]), "T").is_err());
        let err = lookup_secret(vars(&[("T_FILE", "/nonexistent/chelabase/t")]), "T").unwrap_err();
        assert!(err.contains("T_FILE=/nonexistent/chelabase/t"), "{err}");
    }
}
