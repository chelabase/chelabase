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
    if user.contains(':') {
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
