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
