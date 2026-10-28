//! Chosen values for `chelabase-init secrets`, read from the compose `.env` (`--seeds <path>`).
//!
//! An operator writes `CHELABASE_SEED_<NAME>=<value>` lines in `infra/compose/.env` before the
//! first `docker compose up`; `ensure_secrets` writes each one as its [`SEED_KEYS`] file, through
//! the same write path as a generated value, when that file doesn't exist yet. Seeds never reach a
//! container's environment, and apply on first creation only: a file that already holds another
//! value fails the run (rotation is chelabase/chelabase#6).
//!
//! # The file
//!
//! A dotenv subset, as compose reads it: `KEY=VALUE` lines, an optional `export ` prefix, blank
//! lines, `#` comment lines, CRLF line ends, values unquoted (an inline comment starts at a `#`
//! after whitespace), in single quotes or in double quotes. There is no interpolation. Only keys
//! starting with [`PREFIX`] are kept; every other line is skipped (a quoted value spanning
//! lines is skipped whole). A `CHELABASE_SEED_*` key that isn't in [`SEED_KEYS`], a duplicate,
//! or a value that breaks its rule ([`Rule`]) fails the whole file, naming the key and line.
//!
//! # No leaks
//!
//! A seed's value is held in a [`SeedValue`], whose `Debug` hides it and which has no `Display`.
//! Every message here names keys, lines and paths, never a value (nor a value's length).

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::Path;

/// The prefix of every seed key.
pub const PREFIX: &str = "CHELABASE_SEED_";

/// What a seed's value must look like (on top of [`CHARSET`] and [`MAX_LEN`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A password or token: at least [`MIN_SECRET_LEN`] characters.
    Secret,
    /// The object store's access key, an identifier: at least [`MIN_ACCESS_KEY_LEN`].
    AccessKey,
    /// Zitadel's masterkey: exactly [`MASTERKEY_LEN`] characters.
    Masterkey,
}

/// One seedable file of [`crate::secrets::LAYOUT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedKey {
    pub key: &'static str,
    pub path: &'static str,
    pub rule: Rule,
}

const fn s(key: &'static str, path: &'static str, rule: Rule) -> SeedKey {
    SeedKey { key, path, rule }
}

/// Every seedable file: the layout's plain generated values (passwords, the S3 credentials, the
/// masterkey, the internal bearer tokens). Key pairs, certificates, signing keys, raw keys,
/// derived files, `zitadel/steps.yaml` and the setup code are not seedable.
pub const SEED_KEYS: &[SeedKey] = &[
    s(
        "CHELABASE_SEED_METADATA_DB_PASSWORD",
        "db/metadata.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_ZITADEL_DB_PASSWORD",
        "db/zitadel-user.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_ZITADEL_DB_ADMIN_PASSWORD",
        "db/zitadel-admin.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_STORCON_DB_PASSWORD",
        "db/storcon.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_S3_ACCESS_KEY",
        "s3/access-key",
        Rule::AccessKey,
    ),
    s(
        "CHELABASE_SEED_S3_SECRET_KEY",
        "s3/secret-key",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_ZITADEL_MASTERKEY",
        "zitadel/masterkey",
        Rule::Masterkey,
    ),
    s(
        "CHELABASE_SEED_NATS_ORCHESTRATOR_PASSWORD",
        "nats/orchestrator.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_NATS_WORKER_PASSWORD",
        "nats/worker.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_NATS_REALTIME_PASSWORD",
        "nats/realtime.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_NATS_RUNNER_PASSWORD",
        "nats/runner.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_VALKEY_PROXY_PASSWORD",
        "valkey/proxy.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_VALKEY_CONTROL_PASSWORD",
        "valkey/control.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_VALKEY_REALTIME_PASSWORD",
        "valkey/realtime.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_VALKEY_RUNNER_PASSWORD",
        "valkey/runner.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_VALKEY_HEALTH_PASSWORD",
        "valkey/health.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_REALTIME_PG_PASSWORD",
        "realtime/pg.password",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_REALTIME_INTERNAL_TOKEN",
        "realtime/internal.token",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_REST_INTERNAL_TOKEN",
        "tokens/rest-internal.token",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_DATA_KEYS_TOKEN",
        "tokens/data-keys.token",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_APP_AUTH_TOKEN",
        "tokens/app-auth.token",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_APP_SESSIONS_TOKEN",
        "tokens/app-sessions.token",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_PROXY_API_TOKEN",
        "tokens/proxy-api.token",
        Rule::Secret,
    ),
    s(
        "CHELABASE_SEED_STORCON_HOOK_TOKEN",
        "tokens/storcon-hook.token",
        Rule::Secret,
    ),
];

/// A value's longest length, in characters.
pub const MAX_LEN: usize = 1024;
/// A password's or token's shortest length.
pub const MIN_SECRET_LEN: usize = 16;
/// The access key's shortest length (MinIO's minimum).
pub const MIN_ACCESS_KEY_LEN: usize = 3;
/// Zitadel's masterkey length.
pub const MASTERKEY_LEN: usize = 32;
/// The characters a value may hold: printable ASCII but space, quotes, backslash, backtick and
/// `$`. The consumers read these files into URLs (percent-encoded), shell `$(cat …)`, YAML and
/// HTTP headers, and compose would interpolate a `$` in `.env` itself.
pub const CHARSET: &str = "printable ASCII (! to ~) except \" ' \\ ` and $";
/// The seeds file's largest size.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

fn allowed(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '"' | '\'' | '\\' | '`' | '$')
}

/// A seed's value. `Debug` hides it; there is no `Display`, and no `PartialEq` (compare with
/// [`same_value`], which is constant-time).
#[derive(Clone)]
pub struct SeedValue(String);

impl SeedValue {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for SeedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SeedValue(..)")
    }
}

/// The seeds a file gave, by layout path. `Debug` lists the keys only.
#[derive(Clone, Default)]
pub struct Seeds {
    by_path: BTreeMap<&'static str, (&'static SeedKey, SeedValue)>,
}

impl std::fmt::Debug for Seeds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seeds")
            .field("keys", &self.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A loaded seeds file, and a warning to print when the file is open to its group or others.
#[derive(Debug)]
pub struct Loaded {
    pub seeds: Seeds,
    pub warning: Option<String>,
}

impl Seeds {
    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    /// The seeded keys, in layout-path order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.by_path.values().map(|(k, _)| k.key)
    }

    /// The seed for a layout path: its key and value.
    pub(crate) fn get(&self, path: &str) -> Option<(&'static SeedKey, &SeedValue)> {
        self.by_path.get(path).map(|(k, v)| (*k, v))
    }

    /// Parses a seeds file's text (see the module doc). Every problem is reported at once, each
    /// naming its line and key.
    pub fn parse(text: &str) -> Result<Seeds> {
        let mut problems: Vec<String> = Vec::new();
        let mut seen: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut seeds = Seeds::default();
        for a in assignments(text) {
            let a = match a {
                Ok(a) => a,
                Err(problem) => {
                    problems.push(problem);
                    continue;
                }
            };
            if !a.key.starts_with(PREFIX) {
                continue;
            }
            let (line, key) = (a.line, a.key);
            let Some(seed) = SEED_KEYS.iter().find(|k| k.key == key) else {
                problems.push(format!(
                    "line {line}: {} is not a seed key; the accepted keys are {}",
                    shown_key(key),
                    SEED_KEYS
                        .iter()
                        .map(|k| k.key)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                continue;
            };
            if let Some(first) = seen.insert(seed.key, line) {
                problems.push(format!(
                    "line {line}: {key} is set more than once (first on line {first}); keep one"
                ));
                continue;
            }
            let value = match a.value {
                Value::None => Err("has no value (no `=`)".to_string()),
                Value::Multiline => {
                    Err("spans more than one line: a value can't hold a newline".into())
                }
                Value::TrailingText => Err("has text after its closing quote".into()),
                Value::Text(v) => check_value(seed.rule, &v).map(|()| v),
            };
            match value {
                Ok(v) => {
                    seeds.by_path.insert(seed.path, (seed, SeedValue(v)));
                }
                Err(what) => problems.push(format!("line {line}: {key} {what}")),
            }
        }
        if !problems.is_empty() {
            bail!(
                "no seed was used and nothing was changed; fix these first:\n  {}",
                problems.join("\n  ")
            );
        }
        Ok(seeds)
    }

    /// Reads and parses the seeds file at `path`: a symlink or anything but a regular file is
    /// refused; a file its group or others can open runs, with a warning.
    pub fn load(path: &Path) -> Result<Loaded> {
        use std::io::Read;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let shown = path.display();
        // O_NOFOLLOW: a symlink is refused, not followed; O_NONBLOCK: a FIFO can't hang the open.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                bail!("refusing the seeds file {shown}: it is a symlink")
            }
            Err(e) => return Err(e).with_context(|| format!("opening the seeds file {shown}")),
        };
        // Checked on the open descriptor: what is checked is what is read.
        let meta = file
            .metadata()
            .with_context(|| format!("reading the seeds file {shown}"))?;
        if !meta.is_file() {
            bail!("refusing the seeds file {shown}: it is not a regular file");
        }
        let mode = meta.permissions().mode() & 0o7777;
        let warning = (mode & 0o077 != 0).then(|| {
            format!(
                "the seeds file {shown} is open to its group or others (mode {mode:04o}); it \
                 holds secrets: chmod 600 {shown}"
            )
        });
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading the seeds file {shown}"))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("refusing the seeds file {shown}: it is larger than {MAX_FILE_BYTES} bytes");
        }
        // Invalid UTF-8 becomes U+FFFD, which no seed value may hold, so a seed line with it
        // fails its rule; other lines are skipped as ever.
        let seeds = Seeds::parse(&String::from_utf8_lossy(&bytes))
            .with_context(|| format!("in the seeds file {shown}"))?;
        Ok(Loaded { seeds, warning })
    }
}

/// A seed value's rule check. The message never holds the value, nor its length.
fn check_value(rule: Rule, v: &str) -> std::result::Result<(), String> {
    if v.trim().is_empty() {
        return Err("is blank".into());
    }
    if v.chars().any(|c| !allowed(c)) {
        return Err(format!(
            "holds a character that isn't allowed: only {CHARSET} (no spaces or newlines)"
        ));
    }
    let n = v.chars().count();
    if n > MAX_LEN {
        return Err(format!("is longer than {MAX_LEN} characters"));
    }
    match rule {
        Rule::Secret if n < MIN_SECRET_LEN => Err(format!(
            "is shorter than {MIN_SECRET_LEN} characters (a password or token)"
        )),
        Rule::AccessKey if n < MIN_ACCESS_KEY_LEN => {
            Err(format!("is shorter than {MIN_ACCESS_KEY_LEN} characters"))
        }
        Rule::Masterkey if n != MASTERKEY_LEN => Err(format!(
            "must be exactly {MASTERKEY_LEN} characters (Zitadel's masterkey)"
        )),
        _ => Ok(()),
    }
}

/// A key for a message: itself when it is a plain name (`[A-Za-z0-9_]`, at most 64 characters),
/// else a placeholder, since a malformed key (`KEY value`, `KEY:value`, a missing `=`) may hold
/// the value an operator meant to write.
fn shown_key(key: &str) -> String {
    if !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        key.to_string()
    } else {
        format!(
            "a malformed key (not shown, as it may hold a value; it should look like {PREFIX}NAME)"
        )
    }
}

/// One `KEY=VALUE` of the file.
struct Assignment<'a> {
    /// 1-based, the line the key is on.
    line: usize,
    key: &'a str,
    value: Value,
}

enum Value {
    /// A line with a key and no `=`.
    None,
    /// A quoted value that went on past its line.
    Multiline,
    /// Something other than a comment after a closing quote.
    TrailingText,
    Text(String),
}

/// The file's assignments, in order. A quote that never closes is an error naming the line and
/// key (compose refuses it too, and every later line would be inside it).
fn assignments(text: &str) -> Vec<std::result::Result<Assignment<'_>, String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = i + 1;
        let t = lines[i].trim_start();
        i += 1;
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let t = t.strip_prefix("export ").map(str::trim_start).unwrap_or(t);
        let Some((key, rest)) = t.split_once('=') else {
            out.push(Ok(Assignment {
                line,
                key: t.trim_end(),
                value: Value::None,
            }));
            continue;
        };
        let key = key.trim();
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => {
                // The body, and the lines it runs on to when the closing quote isn't on this one.
                let mut body = rest[1..].to_string();
                let mut multiline = false;
                loop {
                    if let Some(end) = closing_quote(&body, q) {
                        let after = body[end + 1..].trim();
                        break if multiline {
                            Value::Multiline
                        } else if !after.is_empty() && !after.starts_with('#') {
                            Value::TrailingText
                        } else {
                            body.truncate(end);
                            Value::Text(body)
                        };
                    }
                    if i >= lines.len() {
                        out.push(Err(format!(
                            "line {line}: {}'s value has no closing quote",
                            shown_key(key)
                        )));
                        return out;
                    }
                    body.push('\n');
                    body.push_str(lines[i]);
                    i += 1;
                    multiline = true;
                }
            }
            _ => {
                // Unquoted: an inline comment starts at a `#` after whitespace.
                let end = rest
                    .char_indices()
                    .find(|&(at, c)| c == '#' && rest[..at].ends_with([' ', '\t']))
                    .map_or(rest.len(), |(at, _)| at);
                Value::Text(rest[..end].trim().to_string())
            }
        };
        out.push(Ok(Assignment { line, key, value }));
    }
    out
}

/// The byte offset of the quote closing `body` (a double-quoted body skips `\x` escapes).
fn closing_quote(body: &str, q: char) -> Option<usize> {
    let mut escaped = false;
    for (at, c) in body.char_indices() {
        if escaped {
            escaped = false;
        } else if q == '"' && c == '\\' {
            escaped = true;
        } else if c == q {
            return Some(at);
        }
    }
    None
}

/// Whether an existing file's bytes (a trailing line break ignored) are the seed's value,
/// compared in constant time (both hashed, then every byte of the digests compared).
pub(crate) fn same_value(existing: &[u8], seed: &SeedValue) -> bool {
    use sha2::{Digest, Sha256};
    let existing = existing
        .strip_suffix(b"\n")
        .map(|e| e.strip_suffix(b"\r").unwrap_or(e))
        .unwrap_or(existing);
    let (a, b) = (Sha256::digest(existing), Sha256::digest(seed.as_bytes()));
    let diff = a.iter().zip(b.iter()).fold(0u8, |d, (x, y)| d | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<'a>(seeds: &'a Seeds, path: &str) -> &'a str {
        std::str::from_utf8(seeds.get(path).unwrap().1.as_bytes()).unwrap()
    }

    #[test]
    fn parses_dotenv_subset() {
        let text = "\u{feff}# The compose .env\r\n\
            CHELABASE_KEYS_DIR=/srv/keys\r\n\
            \r\n\
            CHELABASE_SEED_METADATA_DB_PASSWORD=plain=with=equals-0123\r\n\
            CHELABASE_SEED_NATS_WORKER_PASSWORD='single#quoted=0123456'\n\
            export CHELABASE_SEED_VALKEY_PROXY_PASSWORD=\"double-quoted-0123\"  # a comment\n\
              CHELABASE_SEED_S3_ACCESS_KEY = spaced-key   # inline comment\n\
            CHELABASE_SEED_S3_SECRET_KEY=hash#inside-not-a-comment\n\
            # CHELABASE_SEED_ZITADEL_MASTERKEY=commented-out-so-ignored\n\
            OTHER_MULTILINE=\"first line\n\
            CHELABASE_SEED_DATA_KEYS_TOKEN=inside-a-quoted-value\n\
            last line\"\n\
            OTHER='$NOT_INTERPOLATED'\n\
            NOVALUE\n\
            chelabase_seed_lowercase=ignored\n\
            CHELABASE_SEED_ZITADEL_MASTERKEY=ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
        let seeds = Seeds::parse(text).unwrap();
        assert_eq!(
            seeds.keys().collect::<Vec<_>>(),
            [
                "CHELABASE_SEED_METADATA_DB_PASSWORD",
                "CHELABASE_SEED_NATS_WORKER_PASSWORD",
                "CHELABASE_SEED_S3_ACCESS_KEY",
                "CHELABASE_SEED_S3_SECRET_KEY",
                "CHELABASE_SEED_VALKEY_PROXY_PASSWORD",
                "CHELABASE_SEED_ZITADEL_MASTERKEY",
            ]
        );
        assert_eq!(
            value(&seeds, "db/metadata.password"),
            "plain=with=equals-0123"
        );
        assert_eq!(
            value(&seeds, "nats/worker.password"),
            "single#quoted=0123456"
        );
        assert_eq!(value(&seeds, "valkey/proxy.password"), "double-quoted-0123");
        assert_eq!(value(&seeds, "s3/access-key"), "spaced-key");
        assert_eq!(value(&seeds, "s3/secret-key"), "hash#inside-not-a-comment");
        assert_eq!(
            value(&seeds, "zitadel/masterkey"),
            "ABCDEFGHIJKLMNOPQRSTUVWXYZ012345"
        );
        assert!(seeds.get("tokens/data-keys.token").is_none());
        // Nothing but blank lines, comments and other keys: no seeds.
        assert!(Seeds::parse("A=1\n# x\n\nB='2'\n").unwrap().is_empty());
        assert!(Seeds::parse("").unwrap().is_empty());
    }

    #[test]
    fn rejects_what_compose_and_the_rules_would_trip_on() {
        let long = "a".repeat(MAX_LEN + 1);
        let cases: Vec<(String, &str, &str)> = vec![
            (
                "CHELABASE_SEED_NOPE=0123456789abcdefgh".into(),
                "CHELABASE_SEED_NOPE",
                "CHELABASE_SEED_METADATA_DB_PASSWORD",
            ),
            (
                "CHELABASE_SEED_METADATA_DB_PASSWORD=".into(),
                "CHELABASE_SEED_METADATA_DB_PASSWORD",
                "blank",
            ),
            (
                "CHELABASE_SEED_METADATA_DB_PASSWORD='   '".into(),
                "CHELABASE_SEED_METADATA_DB_PASSWORD",
                "blank",
            ),
            (
                "CHELABASE_SEED_METADATA_DB_PASSWORD".into(),
                "CHELABASE_SEED_METADATA_DB_PASSWORD",
                "no value",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=short-0123".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "shorter than 16",
            ),
            (
                "CHELABASE_SEED_S3_ACCESS_KEY=ab".into(),
                "CHELABASE_SEED_S3_ACCESS_KEY",
                "shorter than 3",
            ),
            (
                format!("CHELABASE_SEED_ZITADEL_MASTERKEY={}", "M".repeat(31)),
                "CHELABASE_SEED_ZITADEL_MASTERKEY",
                "exactly 32",
            ),
            (
                format!("CHELABASE_SEED_ZITADEL_MASTERKEY={}", "M".repeat(33)),
                "CHELABASE_SEED_ZITADEL_MASTERKEY",
                "exactly 32",
            ),
            (
                format!("CHELABASE_SEED_APP_AUTH_TOKEN={long}"),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "longer than 1024",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=\"first-line-0123\nsecond-line\"".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "newline",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=\"escaped\\nnewline-0123\"".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "isn't allowed",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=has space-0123456".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "isn't allowed",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=dollar$INTERPOLATED-0123".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "isn't allowed",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=unicode-é-0123456789".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "isn't allowed",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=tab\tinside-0123456".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "isn't allowed",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN='unterminated-0123456".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "closing quote",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN='quoted-0123456789'trailing".into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "after its closing quote",
            ),
            (
                "CHELABASE_SEED_APP_AUTH_TOKEN=first-value-0123456\n\
                 CHELABASE_SEED_APP_AUTH_TOKEN=second-value-0123456"
                    .into(),
                "CHELABASE_SEED_APP_AUTH_TOKEN",
                "more than once",
            ),
            (
                "OTHER=\"never closed\nCHELABASE_SEED_APP_AUTH_TOKEN=0123456789abcdef".into(),
                "OTHER",
                "closing quote",
            ),
        ];
        for (text, key, what) in cases {
            let err = match Seeds::parse(&text) {
                Ok(s) => panic!("{text:?}: accepted {s:?}"),
                Err(e) => format!("{e:#}"),
            };
            assert!(err.contains(key), "{text:?}: {err}");
            assert!(err.contains(what), "{text:?}: {err}");
            assert!(err.contains("line "), "{text:?}: {err}");
        }
    }

    #[test]
    fn same_value_ignores_one_trailing_line_break_only() {
        let seed = Seeds::parse("CHELABASE_SEED_APP_AUTH_TOKEN=0123456789abcdef")
            .unwrap()
            .get("tokens/app-auth.token")
            .unwrap()
            .1
            .clone();
        assert!(same_value(b"0123456789abcdef", &seed));
        assert!(same_value(b"0123456789abcdef\n", &seed));
        assert!(same_value(b"0123456789abcdef\r\n", &seed));
        assert!(!same_value(b"0123456789abcdeF", &seed));
        assert!(!same_value(b"0123456789abcdef0", &seed));
        assert!(!same_value(b" 0123456789abcdef", &seed));
        assert!(!same_value(b"", &seed));
    }

    #[test]
    fn debug_hides_values() {
        let seeds = Seeds::parse("CHELABASE_SEED_APP_AUTH_TOKEN=SentinelDebug0123456").unwrap();
        let shown = format!(
            "{seeds:?} {:?} {:#?}",
            seeds.get("tokens/app-auth.token").unwrap(),
            seeds
        );
        assert!(!shown.contains("SentinelDebug"), "{shown}");
        assert!(shown.contains("CHELABASE_SEED_APP_AUTH_TOKEN"), "{shown}");
    }

    // Every seedable layout file has exactly one key, and every key names a seedable file: the
    // table can't drift from `LAYOUT`.
    #[test]
    fn seed_keys_match_the_layout() {
        use crate::secrets::{Gen, LAYOUT};
        let plain: Vec<&str> = LAYOUT
            .iter()
            .filter(|e| {
                matches!(
                    e.make,
                    Gen::Text32 | Gen::Password { .. } | Gen::Alnum { .. }
                )
            })
            .map(|e| e.path)
            .collect();
        let mut table: Vec<&str> = SEED_KEYS.iter().map(|k| k.path).collect();
        let mut sorted = plain.clone();
        sorted.sort();
        table.sort();
        assert_eq!(table, sorted);
        let mut keys: Vec<&str> = SEED_KEYS.iter().map(|k| k.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), SEED_KEYS.len(), "a key used twice");
        for k in SEED_KEYS {
            assert!(k.key.starts_with(PREFIX), "{}", k.key);
            assert!(
                k.key[PREFIX.len()..]
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                k.key
            );
        }
        let masterkey = SEED_KEYS
            .iter()
            .find(|k| k.path == "zitadel/masterkey")
            .unwrap();
        assert_eq!(masterkey.rule, Rule::Masterkey);
    }
}
