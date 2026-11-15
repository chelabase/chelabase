//! Object names, blob keys and the small pieces of the object routes that need no database:
//! the upload's byte counter and streaming, `Content-Disposition`, and the list cursor.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use chelabase_storage::{ByteRange, ByteStream, ObjectMeta, ObjectStore, PutOptions, StorageError};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::error::Failure;

/// The longest object name, in bytes of UTF-8 (the object event's bound, so every accepted
/// write's event fits the stream).
pub const MAX_NAME_BYTES: usize = chelabase_nats::storage_objects::MAX_NAME_BYTES;

/// 1 to [`MAX_NAME_BYTES`] bytes, no control character, no leading `/`, no empty segment and
/// none made only of dots (`.`, `..`, and their full-width `．`): clients normalise those away,
/// so such an object would be out of reach.
pub fn valid_object_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.chars().any(char::is_control)
        && name
            .split('/')
            .all(|s| !s.is_empty() && !s.chars().all(is_dot))
}

/// `.` and every code point NFKC folds to dots: `．` U+FF0E, `․` U+2024, `‥` U+2025, `…`
/// U+2026, `﹒` U+FE52.
fn is_dot(c: char) -> bool {
    matches!(
        c,
        '.' | '\u{ff0e}' | '\u{2024}' | '\u{2025}' | '\u{2026}' | '\u{fe52}'
    )
}

pub fn check_object_name(name: &str) -> Result<(), Failure> {
    if valid_object_name(name) {
        Ok(())
    } else {
        Err(Failure::invalid_name())
    }
}

/// A version's blob: `{database_id}/{datoid}/{version_id}` (`prefix` from
/// [`object_prefix`], the OID read in the request's caller transaction). A row carrying another database's version names a key
/// under that database's prefix, never reached from here.
pub fn version_key(prefix: &str, version: Uuid) -> String {
    format!("{prefix}{version}")
}

/// A Postgres database's blob prefix: `{database_id}/{datoid}/`. The OID is stable across a
/// rename and the same on every branch (a branch is a physical copy), and one database's
/// prefix never starts with another's.
pub fn object_prefix(database_id: Uuid, datoid: u32) -> String {
    format!("{database_id}/{datoid}/")
}

/// The range to read: none for a zero-byte object (200 with the empty body, as the store
/// refuses any range of it).
pub fn range_for(size: u64, range: Option<ByteRange>) -> Option<ByteRange> {
    range.filter(|_| size > 0)
}

/// `inline` or, with `?download=` (its value the file name; empty: the object's last segment),
/// `attachment`. Quotes and backslashes are dropped from the file name; non-ASCII goes in
/// `filename*`.
pub fn content_disposition(name: &str, download: Option<&str>) -> String {
    let Some(given) = download else {
        return "inline".into();
    };
    let file = if given.is_empty() {
        name.rsplit('/').next().unwrap_or(name)
    } else {
        given
    };
    let ascii: String = file
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '"' && *c != '\\')
        .collect();
    let encoded: String = file
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}

/// Whether a stored content type may be served `inline`: images (never SVG, which runs script),
/// video, audio, PDF and plain text. Anything else (HTML, XML, unknown) is an attachment.
pub fn inline_safe(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let Some((kind, sub)) = essence.split_once('/') else {
        return false;
    };
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-+.".contains(&b))
    };
    if !token(sub) {
        return false;
    }
    match kind {
        "image" => !sub.contains("svg") && !sub.contains("xml"),
        "video" | "audio" => !sub.contains("xml"),
        "application" => sub == "pdf",
        "text" => sub == "plain",
        _ => false,
    }
}

/// The longest `Content-Type` an upload may declare, in bytes (the object event's bound, so
/// every accepted write's event fits the stream).
pub const MAX_CONTENT_TYPE_BYTES: usize = chelabase_nats::storage_objects::MAX_CONTENT_TYPE_BYTES;

/// A stored or declared Content-Type under a strict grammar: `type/subtype` then any
/// `; name=value` parameters, every part an RFC 9110 token (no quoted strings, no `,`, no
/// whitespace inside a part). Its lower-case essence and its parameters (names lower case),
/// or `None`. A browser reads a comma-separated list's last type, so a `,` is never valid.
pub fn parse_content_type(content_type: &str) -> Option<(String, Vec<(String, String)>)> {
    // RFC 9110's `tchar`: no separator (`,` `;` `"` `/` `=` space …), no control character.
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    };
    // Optional whitespace around `;` is spaces only: a tab is two bytes in JSON, and the byte
    // cap must equal the JSON length.
    fn ows(s: &str) -> &str {
        s.trim_matches(' ')
    }
    let mut parts = content_type.split(';');
    let (kind, sub) = ows(parts.next()?).split_once('/')?;
    if !token(kind) || !token(sub) {
        return None;
    }
    let essence = format!("{kind}/{sub}").to_ascii_lowercase();
    let params = parts
        .map(|p| {
            let (name, value) = ows(p).split_once('=')?;
            (token(name) && token(value)).then(|| (name.to_ascii_lowercase(), value.to_owned()))
        })
        .collect::<Option<Vec<_>>>()?;
    Some((essence, params))
}

/// What a download says its Content-Type is: the essence [`inline_safe`] judged, plus a
/// validated `charset` for `text/plain` only; never the stored text. Anything that doesn't
/// parse is `application/octet-stream`.
pub fn served_content_type(stored: &str) -> String {
    let Some((essence, params)) = parse_content_type(stored) else {
        return "application/octet-stream".into();
    };
    // The first `charset`, as a browser reads it.
    match params.iter().find(|(name, _)| name == "charset") {
        Some((_, charset)) if essence == "text/plain" => {
            format!("text/plain; charset={}", charset.to_ascii_lowercase())
        }
        _ => essence,
    }
}

/// 400 unless an upload's declared Content-Type parses ([`parse_content_type`]) and is at most
/// [`MAX_CONTENT_TYPE_BYTES`].
pub fn check_content_type(content_type: &str) -> Result<(), Failure> {
    if content_type.len() > MAX_CONTENT_TYPE_BYTES {
        return Err(Failure::invalid_request(format!(
            "Content-Type must be at most {MAX_CONTENT_TYPE_BYTES} bytes"
        )));
    }
    match parse_content_type(content_type) {
        Some(_) => Ok(()),
        None => Err(Failure::invalid_request(
            "Content-Type must be one type/subtype with unquoted name=value parameters",
        )),
    }
}

/// A name or bucket as URL path segments: each `/`-separated segment percent-encoded (all but
/// unreserved characters), so the URL's path decodes back to exactly `name`.
pub fn encode_path(name: &str) -> String {
    name.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Why an S3 bucket or key has no `/storage/v1` path (m8 answers `InvalidBucketName`,
/// `KeyTooLongError` or `InvalidArgument`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    /// Not a [`crate::buckets::valid_bucket_name`], or a literal segment of a storage route.
    Bucket,
    /// Over [`MAX_NAME_BYTES`].
    KeyTooLong,
    /// Otherwise not a [`valid_object_name`].
    Key,
}

/// An S3 bucket as its `/storage/v1` path segment: a [`crate::buckets::valid_bucket_name`]
/// that no storage route uses as a literal where a bucket goes ([`bucket_route_words`]), so
/// `/storage/v1/object/{bucket}/…` always reaches the object route and never a keyless one
/// (`public`, `sign`, `upload`, …).
pub fn s3_bucket_path(bucket: &str) -> Result<String, NameError> {
    static WORDS: std::sync::LazyLock<BTreeSet<&'static str>> =
        std::sync::LazyLock::new(|| bucket_route_words(chelabase_routes::ENDPOINTS));
    if !crate::buckets::valid_bucket_name(bucket) || WORDS.contains(bucket) {
        return Err(NameError::Bucket);
    }
    Ok(encode_path(bucket))
}

/// An S3 key (already decoded by s3s) as the `{*name}` part of a `/storage/v1` path: a
/// [`valid_object_name`], each `/`-separated piece percent-encoded ([`encode_path`]), so
/// axum's `Path` extracts exactly `key` (`%`, `?`, `#` and non-ASCII never reach the URL raw).
pub fn s3_object_path(key: &str) -> Result<String, NameError> {
    if key.len() > MAX_NAME_BYTES {
        return Err(NameError::KeyTooLong);
    }
    if !valid_object_name(key) {
        return Err(NameError::Key);
    }
    Ok(encode_path(key))
}

/// Every literal segment a storage route puts where another storage route has `{bucket}`
/// (the same segments before it): the words a bucket's name would collide with. Read from the
/// route table, so a new route's word is reserved with no edit here.
fn bucket_route_words(endpoints: &[chelabase_routes::Endpoint]) -> BTreeSet<&'static str> {
    let storage: Vec<Vec<&'static str>> = endpoints
        .iter()
        .filter(|e| e.service == chelabase_routes::Service::Storage)
        .map(|e| e.path.split('/').collect())
        .collect();
    let mut words = BTreeSet::new();
    for route in &storage {
        for (at, _) in route.iter().enumerate().filter(|(_, s)| **s == "{bucket}") {
            let above = &route[..at];
            words.extend(
                storage
                    .iter()
                    .filter(|other| other.len() > at && other[..at] == *above)
                    .map(|other| other[at])
                    .filter(|s| !s.starts_with('{')),
            );
        }
    }
    words
}

/// `"etag"`, whatever quoting the store gave it.
pub fn quoted_etag(etag: &str) -> String {
    format!("\"{}\"", etag.trim_matches('"'))
}

/// Whether an `If-None-Match` value names `etag` (or is `*`).
pub fn none_match(header: &str, etag: &str) -> bool {
    let want = etag.trim_matches('"');
    header
        .split(',')
        .map(|t| t.trim().trim_start_matches("W/").trim_matches('"'))
        .any(|t| t == "*" || t == want)
}

/// A list page's cursor: the last entry's sort value and key, `value\nkey` (names hold no
/// control character, so the first newline splits them).
pub fn make_cursor(value: &str, key: &str) -> String {
    format!("{value}\n{key}")
}

pub fn parse_cursor(cursor: &str) -> Option<(&str, &str)> {
    cursor.split_once('\n')
}

/// The counted body: bytes seen so far, and whether it went over its limit.
pub struct Counter {
    pub bytes: Arc<AtomicU64>,
    pub over: Arc<AtomicBool>,
}

/// `body` through a byte counter: past `limit` bytes it fails (and [`Counter::over`] is set),
/// so a chunked body is held to the limit as it streams.
pub fn counted<E: std::fmt::Display + Send + 'static>(
    body: impl futures_util::Stream<Item = Result<Bytes, E>> + Send + 'static,
    limit: Option<u64>,
) -> (ByteStream, Counter) {
    let (bytes, over) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let counter = Counter {
        bytes: bytes.clone(),
        over: over.clone(),
    };
    let stream = body.map(move |chunk| {
        let chunk = chunk.map_err(|e| StorageError::Other(format!("the request body: {e}")))?;
        let total = bytes.fetch_add(chunk.len() as u64, Ordering::SeqCst) + chunk.len() as u64;
        if limit.is_some_and(|l| total > l) {
            over.store(true, Ordering::SeqCst);
            return Err(StorageError::Other("the body is over the limit".into()));
        }
        Ok(chunk)
    });
    (stream.boxed(), counter)
}

/// Why a staged upload didn't become an object.
pub enum Staged {
    /// The body went over the limit.
    TooLarge,
    /// The body or the store failed.
    Store(StorageError),
}

/// Streams `body` to `key` (the version's own); the blob's size and etag, or why not.
/// Whatever failed, nothing stays at the key.
pub async fn stage(
    store: &dyn ObjectStore,
    key: &str,
    body: ByteStream,
    counter: &Counter,
    content_type: Option<String>,
) -> Result<ObjectMeta, Staged> {
    let opts = PutOptions {
        content_type,
        if_none_match: false,
    };
    match store.put_stream(key, body, opts).await {
        Ok(meta) => Ok(meta),
        Err(e) => {
            let _ = store.delete(key).await;
            Err(if counter.over.load(Ordering::SeqCst) {
                Staged::TooLarge
            } else {
                Staged::Store(e)
            })
        }
    }
}

/// After the body is at `version_key` (random and unreferenced until its row exists): runs
/// `write` (the ticket and the caller's row). A refused or failed `write` deletes the blob, so
/// no blob outlives a refused row. A write whose outcome is unknown
/// ([`crate::objects::outcome_unknown`]: it may have committed) keeps it for m8's sweeper,
/// which deletes only unreferenced blobs, so no row ever points at a missing blob.
pub async fn keep_if_written<T>(
    store: &dyn ObjectStore,
    version_key: &str,
    write: impl AsyncFnOnce() -> Result<T, Failure>,
) -> Result<T, Failure> {
    let out = write().await;
    match &out {
        Err(f) if crate::objects::outcome_unknown(f) => {
            tracing::warn!(
                version_key,
                "the row's write may have committed; its blob is left for the sweeper"
            );
        }
        Err(_) => {
            let _ = store.delete(version_key).await;
        }
        Ok(_) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_storage::MemoryStore;

    /// AC: `../x`, `/x`, 1025 bytes (and control characters, empty segments) are refused.
    /// m6: only the safe inline types are served inline; parameters and case don't matter.
    /// m6 audit B3: a comma (a browser takes the list's last type) or anything outside the
    /// strict grammar is refused at upload, and a download never serves the stored text.
    #[test]
    fn content_types_are_strict_and_served_as_their_essence() {
        for bad in [
            "text/plain;a=b,text/html",
            "image/png;x=1, text/html",
            "text/plain;charset=utf-8,application/xhtml+xml",
            "text/html, image/png",
            "text/plain; charset=\"utf-8\"",
            "text/plain;charset",
            "text/ plain",
            "text/plain;;a=b",
            "text/plain\u{0}",
            "/plain",
            "text/",
            "",
        ] {
            assert!(parse_content_type(bad).is_none(), "{bad:?}");
            assert_eq!(check_content_type(bad).unwrap_err().0.status.as_u16(), 400, "{bad:?}");
            assert_eq!(served_content_type(bad), "application/octet-stream", "{bad:?}");
            assert!(!inline_safe(&served_content_type(bad)), "{bad:?}");
        }
        for (good, served) in [
            ("text/plain", "text/plain"),
            ("Text/Plain; Charset=UTF-8", "text/plain; charset=utf-8"),
            ("text/plain;format=flowed;charset=us-ascii", "text/plain; charset=us-ascii"),
            ("image/png; x=1", "image/png"),
            ("text/html; charset=utf-8", "text/html"),
            ("application/vnd.api+json", "application/vnd.api+json"),
        ] {
            assert!(check_content_type(good).is_ok(), "{good}");
            assert_eq!(served_content_type(good), served, "{good}");
        }
        assert!(parse_content_type("text/plain; charset=utf-8").is_some());
    }

    /// Re-audit B4 (round 3): an upload's `Content-Type` is at most [`MAX_CONTENT_TYPE_BYTES`]
    /// (255) bytes, so its event fits the stream; one byte more is 400 `invalid_request`.
    #[test]
    fn a_content_type_is_at_most_255_bytes() {
        assert_eq!(MAX_CONTENT_TYPE_BYTES, 255);
        let of_len = |n: usize| format!("image/png; x={}", "a".repeat(n - "image/png; x=".len()));
        assert!(check_content_type(&of_len(MAX_CONTENT_TYPE_BYTES)).is_ok());
        let long = of_len(MAX_CONTENT_TYPE_BYTES + 1);
        assert_eq!(long.len(), 256);
        assert!(parse_content_type(&long).is_some(), "only the length is wrong");
        let refused = check_content_type(&long).unwrap_err().0;
        assert_eq!((refused.status.as_u16(), refused.code), (400, "invalid_request"));
        assert!(check_content_type(&format!("image/png; x={}", "a".repeat(6000))).is_err());
    }

    /// m18 audit S3: optional whitespace around `;` is spaces only, so the byte cap equals the
    /// JSON length (JSON writes a tab as two bytes) and an accepted type always fits its event.
    #[test]
    fn a_tab_padded_content_type_is_refused_and_a_space_padded_one_fits_the_event() {
        let tabs = format!("image/png{};x=y", "\t".repeat(255 - "image/png;x=y".len()));
        assert_eq!(tabs.len(), 255);
        assert_eq!(check_content_type(&tabs).unwrap_err().0.status.as_u16(), 400);
        assert!(check_content_type("image/png;\tx=y").is_err());
        let spaces = format!("image/png{};x=y", " ".repeat(255 - "image/png;x=y".len()));
        assert_eq!(spaces.len(), 255);
        assert!(check_content_type(&spaces).is_ok());
        assert_eq!(serde_json::to_string(&spaces).unwrap().len() - 2, 255);
    }

    #[test]
    fn only_safe_types_are_inline() {
        for t in [
            "image/png",
            "IMAGE/JPEG",
            "image/webp; q=1",
            "video/mp4",
            "audio/mpeg",
            "application/pdf",
            "text/plain",
            "text/plain; charset=utf-8",
        ] {
            assert!(inline_safe(t), "{t}");
        }
        for t in [
            "text/html",
            "image/svg+xml",
            "IMAGE/SVG+XML; charset=utf-8",
            "image/svg",
            "application/xhtml+xml",
            "text/xml",
            "application/javascript",
            "text/plain-html",
            "application/octet-stream",
            "",
            "image",
            "image/",
        ] {
            assert!(!inline_safe(t), "{t}");
        }
    }

    /// A signed URL's path decodes back to the name.
    #[test]
    fn names_encode_per_segment() {
        assert_eq!(encode_path("u1/a b.png"), "u1/a%20b.png");
        assert_eq!(encode_path("a?b#c%d/é"), "a%3Fb%23c%25d/%C3%A9");
        assert_eq!(encode_path("plain-name_1.~x"), "plain-name_1.~x");
    }

    #[test]
    fn object_names_are_checked() {
        for ok in [
            "a.png",
            "u1/2026/a.png",
            "..a",
            "a..b/c",
            "é/ü.txt",
            &"a".repeat(1024),
        ] {
            assert!(valid_object_name(ok), "{ok}");
        }
        for bad in [
            "",
            "../x",
            "a/../x",
            "/x",
            "a//b",
            "a/",
            "a\u{0}b",
            "a\nb",
            &"a".repeat(1025),
            // N7: dot-only segments, full-width ones included.
            "./x",
            "a/.",
            "a/./b",
            "...",
            "\u{ff0e}/x",
            "a/\u{ff0e}\u{ff0e}",
            // N3: everything NFKC folds to dots.
            "\u{2024}/x",
            "a/\u{2025}",
            "\u{2026}",
            "a/\u{fe52}/b",
        ] {
            assert!(!valid_object_name(bad), "{bad:?}");
        }
        // 1024 bytes of multi-byte UTF-8 is the limit, not 1024 characters.
        assert!(!valid_object_name(&"é".repeat(513)));
    }

    #[test]
    fn keys_and_headers() {
        let v = Uuid::nil();
        assert_eq!(version_key("db/main/", v), format!("db/main/{v}"));
        // SF10: keyed by the Postgres database's OID: one database's prefix never starts with
        // another's (a name like `main/staging` could).
        let db = Uuid::nil();
        assert_eq!(object_prefix(db, 16384), format!("{db}/16384/"));
        let prefixes: Vec<String> = [1u32, 12, 123, 16384, 163840]
            .iter()
            .map(|o| object_prefix(db, *o))
            .collect();
        for a in &prefixes {
            for b in &prefixes {
                assert!(a == b || !b.starts_with(a.as_str()), "{b} nests in {a}");
            }
        }
        // N1: a zero-byte object's range is ignored (200, empty body).
        assert_eq!(
            range_for(
                0,
                Some(ByteRange::From {
                    start: 0,
                    end: None
                })
            ),
            None
        );
        let r = Some(ByteRange::From {
            start: 0,
            end: Some(4),
        });
        assert_eq!(range_for(11, r), r);
        assert_eq!(content_disposition("a/b.png", None), "inline");
        assert_eq!(
            content_disposition("a/b.png", Some("")),
            "attachment; filename=\"b.png\"; filename*=UTF-8''b.png"
        );
        assert!(
            content_disposition("x", Some("ü \"q\".txt")).starts_with(
                "attachment; filename=\" q.txt\"; filename*=UTF-8''%C3%BC%20%22q%22.txt"
            )
        );
        assert_eq!(quoted_etag("\"abc\""), "\"abc\"");
        assert!(
            none_match("\"abc\"", "abc")
                && none_match("W/\"x\", \"abc\"", "\"abc\"")
                && none_match("*", "q")
        );
        assert!(!none_match("\"abd\"", "abc"));
        assert_eq!(parse_cursor(&make_cursor("12", "a/b")), Some(("12", "a/b")));
    }

    /// Serves every storage route's path (as the routes table declares it) on a local port;
    /// each answers its endpoint's name and what axum's `Path` extracted. The object routes
    /// extract `(bucket, name)` as their handlers do.
    async fn serve_storage_routes() -> String {
        use axum::{Json, extract::Path, routing::any};
        let mut paths = std::collections::BTreeMap::new();
        for ep in chelabase_routes::ENDPOINTS
            .iter()
            .filter(|e| e.service == chelabase_routes::Service::Storage)
        {
            paths.entry(ep.path).or_insert(ep.name);
        }
        let mut router = axum::Router::new();
        for (path, name) in paths {
            router = if path == chelabase_routes::storage::DOWNLOAD_OBJECT.path {
                router.route(
                    path,
                    any(|Path((bucket, key)): Path<(String, String)>| async move {
                        Json((
                            "object",
                            vec![("bucket".to_owned(), bucket), ("name".to_owned(), key)],
                        ))
                    }),
                )
            } else {
                router.route(
                    path,
                    any(
                        move |Path(params): Path<Vec<(String, String)>>| async move {
                            Json((name, params))
                        },
                    ),
                )
            };
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}")
    }

    /// AC 1: any key `valid_object_name` accepts, under any bucket `s3_bucket_path` accepts,
    /// reaches the object route with exactly that bucket and key.
    #[test]
    fn s3_object_path_round_trips() {
        use proptest::prelude::*;
        use proptest::test_runner::{Config, TestCaseError, TestRunner};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let base = rt.block_on(serve_storage_routes());
        let client = chelabase_http::client_builder().build().unwrap();
        let fetch = |bucket: &str, key: &str| -> (String, Vec<(String, String)>) {
            let url = format!(
                "{base}/storage/v1/object/{}/{}",
                s3_bucket_path(bucket).unwrap(),
                s3_object_path(key).unwrap()
            );
            rt.block_on(async {
                let res = client.get(&url).send().await.unwrap();
                assert!(res.status().is_success(), "{url}: {}", res.status());
                res.json().await.unwrap()
            })
        };
        let expect = |bucket: &str, key: &str| {
            (
                "object".to_owned(),
                vec![
                    ("bucket".to_owned(), bucket.to_owned()),
                    ("name".to_owned(), key.to_owned()),
                ],
            )
        };

        for key in [
            "a%252Fb",
            "a%2Fb",
            "%2F",
            "a?b=c",
            "x#y",
            "a b+c&d",
            "..z/a..b",
            "日本/語.txt",
            "😀/%/?/#",
            "sign/x",
            "public/main/b/k",
            &"%".repeat(MAX_NAME_BYTES),
        ] {
            assert_eq!(fetch("avatars", key), expect("avatars", key), "{key:?}");
        }
        // Refused by `valid_object_name`, so never built into a path.
        for key in ["../z", "./z", "a/../z", "/z", "a//b", "a/"] {
            assert_eq!(s3_object_path(key), Err(NameError::Key), "{key:?}");
        }

        let buckets = "[a-z0-9][a-z0-9._-]{0,20}"
            .prop_filter("not a route word", |b| s3_bucket_path(b).is_ok());
        let keys = prop_oneof![
            "[a-z%?#/. +&=;é日😀~_-]{1,40}",
            "\\PC{1,12}(/\\PC{1,12}){0,3}",
            "(%[0-9A-Fa-f]{2}|%25|[a-z/]){1,30}",
        ]
        .prop_filter("a valid object name", |k| valid_object_name(k));
        TestRunner::new(Config::with_cases(256))
            .run(&(buckets, keys), |(bucket, key)| {
                let got = fetch(&bucket, &key);
                if got != expect(&bucket, &key) {
                    return Err(TestCaseError::fail(format!("{bucket:?} {key:?}: {got:?}")));
                }
                Ok(())
            })
            .unwrap();
    }

    /// AC 2: a literal segment of any storage route, and an invalid bucket name, are refused.
    #[test]
    fn s3_bucket_path_refuses_route_words_and_invalid() {
        for word in [
            "public", "sign", "upload", "list", "copy", "move", "resolve", "session",
        ]
        .into_iter()
        .chain(crate::buckets::RESERVED_BUCKET_NAMES)
        .chain(bucket_route_words(chelabase_routes::ENDPOINTS))
        {
            assert_eq!(s3_bucket_path(word), Err(NameError::Bucket), "{word}");
        }
        for bad in [
            "",
            "Bad",
            "-x",
            ".x",
            "a/b",
            "a b",
            "a%2Fb",
            "é",
            &"a".repeat(64),
        ] {
            assert_eq!(s3_bucket_path(bad), Err(NameError::Bucket), "{bad:?}");
        }
        for ok in ["avatars", "user-files.v2", "0_x", "a..b", &"a".repeat(63)] {
            assert_eq!(s3_bucket_path(ok).as_deref(), Ok(ok), "{ok}");
        }
        assert_eq!(
            s3_object_path(&"a".repeat(MAX_NAME_BYTES + 1)),
            Err(NameError::KeyTooLong)
        );
        assert_eq!(s3_object_path(&"é".repeat(513)), Err(NameError::KeyTooLong));
        for bad in ["", "a\nb", "a\u{0}b", "a/\u{ff0e}"] {
            assert_eq!(s3_object_path(bad), Err(NameError::Key), "{bad:?}");
        }
        assert_eq!(s3_object_path("a?b#c%d/é").unwrap(), "a%3Fb%23c%25d/%C3%A9");
    }

    /// AC 3: a literal segment a new storage route puts where a bucket goes is reserved by
    /// the route table alone.
    #[test]
    fn reserved_words_follow_the_route_table() {
        use chelabase_routes::{ENDPOINTS, Endpoint, Service, storage};
        let words = bucket_route_words(ENDPOINTS);
        for word in crate::buckets::RESERVED_BUCKET_NAMES {
            assert!(words.contains(word), "{word} in {words:?}");
        }
        // Parameters and the segments above the bucket are not words.
        for not in ["{bucket}", "{name}", "storage", "v1", "object", "complete"] {
            assert!(!words.contains(not), "{not} in {words:?}");
        }
        let mut table = ENDPOINTS.to_vec();
        table.push(Endpoint {
            path: "/storage/v1/object/info/{bucket}/{*name}",
            ..storage::DOWNLOAD_OBJECT
        });
        table.push(Endpoint {
            path: "/storage/v1/upload/batch/{id}",
            ..storage::LIST_PARTS
        });
        // Another service's route is not a storage route.
        table.push(Endpoint {
            path: "/storage/v1/object/elsewhere",
            service: Service::Functions,
            ..storage::COPY_OBJECT
        });
        let grown = bucket_route_words(&table);
        assert!(
            grown.contains("info") && grown.contains("batch"),
            "{grown:?}"
        );
        assert!(!grown.contains("elsewhere"), "{grown:?}");
        assert!(words.is_subset(&grown));
    }

    fn body(
        chunks: Vec<Result<&'static [u8], &'static str>>,
    ) -> impl futures_util::Stream<Item = Result<Bytes, &'static str>> + Send {
        futures_util::stream::iter(chunks.into_iter().map(|c| c.map(Bytes::from_static)))
    }

    /// AC: a body over the bucket's limit (chunked: no declared length) is 413 and leaves no
    /// blob; a body that fails midway leaves none either.
    #[tokio::test]
    async fn a_refused_or_failed_stream_leaves_nothing_staged() {
        let store = MemoryStore::default();
        let (stream, counter) = counted(body(vec![Ok(b"abc"), Ok(b"def")]), Some(5));
        assert!(matches!(
            stage(&store, "p/staging/1", stream, &counter, None).await,
            Err(Staged::TooLarge)
        ));
        let (stream, counter) = counted(body(vec![Ok(b"abc"), Err("reset by peer")]), None);
        assert!(matches!(
            stage(&store, "p/staging/2", stream, &counter, None).await,
            Err(Staged::Store(_))
        ));
        assert!(store.list("p/").await.unwrap().is_empty());
        let (stream, counter) = counted(body(vec![Ok(b"abc"), Ok(b"de")]), Some(5));
        let meta = stage(&store, "p/staging/3", stream, &counter, None)
            .await
            .ok()
            .unwrap();
        assert_eq!((meta.size, counter.bytes.load(Ordering::SeqCst)), (5, 5));
    }

    /// A refused or failed row deletes the version's blob; a written one keeps exactly it.
    #[tokio::test]
    async fn a_refused_row_leaves_no_blob() {
        let store = MemoryStore::default();
        store
            .put("p/v1", Bytes::from_static(b"x"), PutOptions::default())
            .await
            .unwrap();
        let out: Result<(), Failure> =
            keep_if_written(&store, "p/v1", async || Err(Failure::object_conflict())).await;
        assert_eq!(out.unwrap_err().0.status.as_u16(), 409);
        assert!(store.list("p/").await.unwrap().is_empty());
        store
            .put("p/v2", Bytes::from_static(b"y"), PutOptions::default())
            .await
            .unwrap();
        keep_if_written(&store, "p/v2", async || Ok(()))
            .await
            .unwrap();
        let keys: Vec<String> = store
            .list("p/")
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.key)
            .collect();
        assert_eq!(keys, ["p/v2"]);
    }
}
