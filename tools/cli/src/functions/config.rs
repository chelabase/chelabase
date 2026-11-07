//! `chelabase.toml`: the project's instance, database and branch, and its functions.
//!
//! ```toml
//! [project]
//! url = "https://api.example.com"   # optional: else --url, CHELABASE_URL or the only login
//! database = "<database id>"        # optional: else --database or CHELABASE_DATABASE
//! branch = "<branch id>"            # optional: the database's main branch by default
//!
//! [functions.hello]
//! entry = "functions/hello/index.ts"
//! auth = "key"                      # key | app_user | public (optional)
//! compatibility_date = "2026-10-01" # optional
//! ```

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const FILE: &str = "chelabase.toml";
/// The compatibility date a function gets when it names none.
pub const DEFAULT_COMPATIBILITY_DATE: &str = "2026-10-01";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub functions: BTreeMap<String, Function>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub url: Option<String>,
    pub database: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Function {
    /// Relative to the project's root.
    pub entry: PathBuf,
    pub auth: Option<Auth>,
    pub compatibility_date: Option<String>,
    #[serde(default)]
    pub compatibility_flags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    Key,
    AppUser,
    Public,
}

impl Function {
    pub fn compatibility_date(&self) -> &str {
        self.compatibility_date
            .as_deref()
            .unwrap_or(DEFAULT_COMPATIBILITY_DATE)
    }
}

/// A function name as the management API takes it: a lowercase letter, then up to 62 of a-z,
/// 0-9 and `-`; `public` is reserved.
pub fn check_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok {
        bail!(
            "`{name}` isn't a function name: a lowercase letter, then up to 62 of a-z, 0-9 and -"
        );
    }
    if name == "public" {
        bail!("`public` is reserved (the keyless invoke prefix); pick another name");
    }
    Ok(())
}

impl Config {
    /// Parses and checks a `chelabase.toml`.
    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))?;
        for (name, f) in &config.functions {
            check_name(name).with_context(|| format!("[functions.{name}]"))?;
            if f.entry.as_os_str().is_empty() || f.entry.is_absolute() {
                bail!("[functions.{name}] entry must be a path relative to the project's root");
            }
            if let Some(d) = &f.compatibility_date
                && chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err()
            {
                bail!("[functions.{name}] compatibility_date `{d}` isn't a YYYY-MM-DD date");
            }
        }
        Ok(config)
    }

    /// The nearest `chelabase.toml` from `start` upwards: the project's root and its config.
    pub fn find(start: &Path) -> Result<(PathBuf, Config)> {
        let mut dir = start.to_path_buf();
        loop {
            let file = dir.join(FILE);
            if file.is_file() {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let config =
                    Config::parse(&text).with_context(|| format!("in {}", file.display()))?;
                return Ok((dir, config));
            }
            if !dir.pop() {
                bail!("no {FILE} in {} or above it", start.display());
            }
        }
    }

    /// The named functions (all of them when `names` is empty), each checked to exist.
    pub fn select<'a>(&'a self, names: &[String]) -> Result<Vec<(&'a str, &'a Function)>> {
        if self.functions.is_empty() {
            bail!("{FILE} has no [functions.<name>] sections");
        }
        if names.is_empty() {
            return Ok(self
                .functions
                .iter()
                .map(|(n, f)| (n.as_str(), f))
                .collect());
        }
        names
            .iter()
            .map(|n| {
                self.functions
                    .get_key_value(n)
                    .map(|(n, f)| (n.as_str(), f))
                    .with_context(|| format!("{FILE} has no [functions.{n}]"))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> String {
        format!("{:#}", Config::parse(text).unwrap_err())
    }

    #[test]
    fn a_full_config_parses() {
        let c = Config::parse(
            r#"
[project]
url = "http://api.localhost:8000"
database = "d"

[functions.hello]
entry = "functions/hello/index.ts"
auth = "app_user"
compatibility_date = "2026-09-01"
"#,
        )
        .unwrap();
        assert_eq!(c.project.database.as_deref(), Some("d"));
        let f = &c.functions["hello"];
        assert_eq!(f.entry, PathBuf::from("functions/hello/index.ts"));
        assert_eq!(f.auth, Some(Auth::AppUser));
        assert_eq!(f.compatibility_date(), "2026-09-01");
        let c = Config::parse("[functions.a]\nentry = \"a.js\"").unwrap();
        assert_eq!(
            c.functions["a"].compatibility_date(),
            DEFAULT_COMPATIBILITY_DATE
        );
    }

    #[test]
    fn config_errors_name_the_problem() {
        assert!(err("[functions.hello]\nauth = \"key\"").contains("missing field `entry`"));
        assert!(
            err("[functions.hello]\nentry = \"a.ts\"\nauht = \"key\"")
                .contains("unknown field `auht`")
        );
        assert!(
            err("[functions.hello]\nentry = \"a.ts\"\nauth = \"anyone\"")
                .contains("unknown variant `anyone`")
        );
        assert!(
            err("[functions.Hello]\nentry = \"a.ts\"").contains("`Hello` isn't a function name")
        );
        assert!(err("[functions.public]\nentry = \"a.ts\"").contains("`public` is reserved"));
        assert!(
            err("[functions.a]\nentry = \"/abs/a.ts\"").contains("relative to the project's root")
        );
        assert!(
            err("[functions.a]\nentry = \"a.ts\"\ncompatibility_date = \"10/01/2026\"")
                .contains("isn't a YYYY-MM-DD date")
        );
        assert!(err("[projet]\nurl = \"x\"").contains("unknown field `projet`"));
        assert!(err("[functions.a\nentry = 1").contains("TOML parse error"));
    }

    #[test]
    fn select_names_a_missing_function() {
        let c = Config::parse("[functions.a]\nentry = \"a.js\"").unwrap();
        assert_eq!(c.select(&[]).unwrap().len(), 1);
        let e = c.select(&["b".into()]).unwrap_err().to_string();
        assert!(e.contains("no [functions.b]"), "{e}");
        let e = Config::parse("")
            .unwrap()
            .select(&[])
            .unwrap_err()
            .to_string();
        assert!(e.contains("no [functions.<name>]"), "{e}");
    }

    #[test]
    fn find_walks_up_to_the_project_root() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(FILE), "[functions.a]\nentry = \"a.js\"").unwrap();
        let sub = t.path().join("functions/a");
        std::fs::create_dir_all(&sub).unwrap();
        let (root, c) = Config::find(&sub).unwrap();
        assert_eq!(root, t.path());
        assert!(c.functions.contains_key("a"));
    }
}
