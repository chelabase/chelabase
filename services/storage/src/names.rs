//! Object names, blob keys and the small pieces of the object routes that need no database:
//! the upload's byte counter and streaming, `Content-Disposition`, and the list cursor.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bytes::Bytes;
use chelabase_storage::{ByteRange, ByteStream, ObjectMeta, ObjectStore, PutOptions, StorageError};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::error::Failure;

/// The longest object name, in bytes of UTF-8.
pub const MAX_NAME_BYTES: usize = 1024;

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
