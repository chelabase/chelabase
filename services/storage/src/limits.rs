//! A bucket's limits and the service's own: `file_size_limit` on counted bytes,
//! `allowed_mime_types` against both the declared and the sniffed type, and the per-process cap
//! on upload memory ([`InFlight`]).
//!
//! The type rule: with a list set, the declared type must match it, and so must the type the
//! first bytes are sniffed as ([`sniff`]) whenever they are recognised. Bytes nothing recognises
//! (plain text, JSON, CSV, random data) leave the declared type to decide: they render as
//! nothing on their own, and every download is `nosniff` and sandboxed. So HTML declared as
//! `image/png` is refused, and a real PNG declared as `image/png` passes.

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::Failure;

/// How many leading bytes [`sniff`] looks at.
pub const SNIFF_BYTES: usize = 512;

/// A bucket's id and limits.
#[derive(Debug, Clone)]
pub struct Limits {
    pub id: uuid::Uuid,
    pub file_size_limit: Option<i64>,
    pub allowed_mime_types: Option<Vec<String>>,
}

impl Limits {
    /// The size limit in bytes, if any.
    pub fn max_bytes(&self) -> Option<u64> {
        self.file_size_limit.map(|l| l.max(0) as u64)
    }

    /// 413 when `size` is over the bucket's limit.
    pub fn check_size(&self, size: u64) -> Result<(), Failure> {
        match self.max_bytes() {
            Some(limit) if size > limit => Err(Failure::too_large(limit as i64)),
            _ => Ok(()),
        }
    }

    /// 415 unless the declared type matches the list.
    pub fn check_declared(&self, declared: &str) -> Result<(), Failure> {
        match &self.allowed_mime_types {
            Some(list) if !type_allowed(list, declared) => Err(Failure::mime_not_allowed()),
            _ => Ok(()),
        }
    }

    /// 415 unless the type `head` (the first bytes) is sniffed as matches the list.
    pub fn check_sniffed(&self, head: &[u8]) -> Result<(), Failure> {
        match (&self.allowed_mime_types, sniff(head)) {
            (Some(list), Some(sniffed)) if !type_allowed(list, sniffed) => {
                Err(Failure::mime_not_allowed())
            }
            _ => Ok(()),
        }
    }

    /// Whether the sniff needs the body's first bytes at all.
    pub fn sniffs(&self) -> bool {
        self.allowed_mime_types.is_some()
    }
}

/// A content type's essence: lower case, without parameters.
fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

/// Whether `content_type` matches one of `list`'s entries: `type/subtype` exactly, `type/*`
/// any subtype of `type`, `*/*` (or `*`) anything. Case and parameters don't matter.
pub fn type_allowed(list: &[String], content_type: &str) -> bool {
    let ty = essence(content_type);
    let Some((kind, sub)) = ty.split_once('/') else {
        return false;
    };
    if kind.is_empty() || sub.is_empty() || sub.contains('/') {
        return false;
    }
    list.iter()
        .map(|entry| essence(entry))
        .any(|entry| match entry.as_str() {
            "*" | "*/*" => true,
            e => e.strip_suffix("/*").map_or(e == ty, |k| k == kind),
        })
}

/// The type `head` is recognised as: a file signature (images, video, audio, archives,
/// executables, documents: the `infer` crate), HTML or XML by the WHATWG sniffing rules, or an
/// SVG document; `None` when nothing matches.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    if let Some(t) = infer::get(head) {
        return Some(t.mime_type());
    }
    // `infer` has no SVG matcher: an SVG document without an XML declaration.
    let text = head.trim_ascii_start();
    let svg = text.len() > 4
        && text[..4].eq_ignore_ascii_case(b"<svg")
        && matches!(text[4], b' ' | b'>' | b'/' | b'\t' | b'\n' | b'\r');
    svg.then_some("image/svg+xml")
}

/// `body`'s first [`SNIFF_BYTES`] bytes (fewer if it is shorter), and the whole body again.
pub async fn peek<E: Send + 'static>(
    mut body: BoxStream<'static, Result<Bytes, E>>,
) -> Result<(Bytes, BoxStream<'static, Result<Bytes, E>>), E> {
    let mut head = BytesMut::new();
    while head.len() < SNIFF_BYTES {
        match body.next().await {
            Some(chunk) => head.extend_from_slice(&chunk?),
            None => break,
        }
    }
    let head = head.freeze();
    let again = futures_util::stream::once({
        let head = head.clone();
        async move { Ok(head) }
    })
    .chain(body)
    .boxed();
    Ok((head.slice(..head.len().min(SNIFF_BYTES)), again))
}

/// The per-process cap on upload buffers (`CHELABASE_STORAGE_MAX_INFLIGHT_BYTES`): a part is
/// held whole in memory while it goes to the store, and a single upload holds up to one
/// stream part. Past the cap a request is 503 `busy` at once, before its body is read.
#[derive(Clone)]
pub struct InFlight {
    permits: Arc<Semaphore>,
    capacity: usize,
}

impl InFlight {
    pub fn new(bytes: usize) -> Self {
        let capacity = bytes.min(Semaphore::MAX_PERMITS);
        InFlight {
            permits: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// Holds `bytes` of the cap until the permit drops; 503 `busy` when they aren't free.
    pub fn reserve(&self, bytes: u64) -> Result<OwnedSemaphorePermit, Failure> {
        let n = u32::try_from(bytes).map_err(|_| Failure::busy())?;
        if bytes as usize > self.capacity {
            return Err(Failure::busy());
        }
        self.permits
            .clone()
            .try_acquire_many_owned(n)
            .map_err(|_| Failure::busy())
    }

    /// Bytes free now (tests).
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// `type/*`, exact and `*/*` entries; case and parameters don't matter; nothing else
    /// matches (no prefix tricks).
    #[test]
    fn types_match_the_list() {
        let images = list(&["image/*"]);
        for ok in ["image/png", "IMAGE/JPEG", "image/webp; q=1"] {
            assert!(type_allowed(&images, ok), "{ok}");
        }
        for bad in [
            "text/html",
            "imagex/png",
            "image",
            "",
            "application/image/png",
        ] {
            assert!(!type_allowed(&images, bad), "{bad}");
        }
        let exact = list(&["application/pdf", "Text/Plain"]);
        assert!(type_allowed(&exact, "application/pdf"));
        assert!(type_allowed(&exact, "text/plain; charset=utf-8"));
        assert!(!type_allowed(&exact, "application/pdfx"));
        assert!(type_allowed(&list(&["*/*"]), "anything/at-all"));
        assert!(!type_allowed(&[], "image/png"));
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

    /// Signatures, WHATWG HTML and XML, SVG; plain text and unknown bytes are unrecognised.
    #[test]
    fn sniffing_recognises_signatures_and_markup() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0\0\x10JFIF"), Some("image/jpeg"));
        assert_eq!(sniff(b"%PDF-1.7\n"), Some("application/pdf"));
        assert_eq!(
            sniff(b"  <!DOCTYPE html><html><body>x</body></html>"),
            Some("text/html")
        );
        assert_eq!(sniff(b"<script>alert(1)</script>"), Some("text/html"));
        assert_eq!(sniff(b"<?xml version=\"1.0\"?><a/>"), Some("text/xml"));
        assert_eq!(
            sniff(b"\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            Some("image/svg+xml")
        );
        assert_eq!(sniff(b"hello, world\n"), None);
        assert_eq!(sniff(b"{\"a\": 1}"), None);
        assert_eq!(sniff(b""), None);
    }

    /// AC 3: HTML declared as `image/png` in an `image/*` bucket is 415; a real PNG passes;
    /// with no list nothing is refused; text that nothing recognises leaves the declared type
    /// to decide.
    #[test]
    fn html_declared_as_png_is_refused() {
        let b = Limits {
            id: uuid::Uuid::nil(),
            file_size_limit: None,
            allowed_mime_types: Some(list(&["image/*"])),
        };
        assert!(b.check_declared("image/png").is_ok());
        let refused = b
            .check_sniffed(b"<html><body>hi</body></html>")
            .unwrap_err();
        assert_eq!(
            (refused.0.status.as_u16(), refused.0.code),
            (415, "mime_not_allowed")
        );
        assert!(b.check_sniffed(PNG).is_ok());
        assert_eq!(
            b.check_declared("text/html").unwrap_err().0.status.as_u16(),
            415
        );
        let text = Limits {
            allowed_mime_types: Some(list(&["text/csv"])),
            ..b.clone()
        };
        assert!(
            text.check_declared("text/csv").is_ok() && text.check_sniffed(b"a,b\n1,2\n").is_ok()
        );
        let open = Limits {
            allowed_mime_types: None,
            ..b
        };
        assert!(open.check_declared("text/html").is_ok());
        assert!(open.check_sniffed(b"<html>").is_ok() && !open.sniffs());
    }

    #[test]
    fn sizes_are_held_to_the_limit() {
        let b = Limits {
            id: uuid::Uuid::nil(),
            file_size_limit: Some(1 << 20),
            allowed_mime_types: None,
        };
        assert!(b.check_size(1 << 20).is_ok());
        assert_eq!(
            b.check_size((1 << 20) + 1).unwrap_err().0.status.as_u16(),
            413
        );
    }

    /// The peek hands back the first bytes and then the whole body, however it was chunked.
    #[tokio::test]
    async fn peek_returns_the_head_and_the_whole_body() {
        let chunks: Vec<Result<Bytes, ()>> = (0..300u16)
            .map(|i| Ok(Bytes::from(vec![(i % 251) as u8; 3])))
            .collect();
        let want: Vec<u8> = (0..300u16).flat_map(|i| vec![(i % 251) as u8; 3]).collect();
        let (head, body) = peek(futures_util::stream::iter(chunks).boxed())
            .await
            .unwrap();
        assert_eq!(&head[..], &want[..SNIFF_BYTES]);
        let all: Vec<u8> = body
            .map(|c| c.unwrap().to_vec())
            .collect::<Vec<_>>()
            .await
            .concat();
        assert_eq!(all, want);
        let (head, _) =
            peek(futures_util::stream::iter(vec![Ok::<_, ()>(Bytes::from_static(b"ab"))]).boxed())
                .await
                .unwrap();
        assert_eq!(&head[..], b"ab");
    }

    /// AC 6: the cap holds concurrent buffers to its bytes; past it is 503 `busy` at once,
    /// and a released buffer frees its share.
    #[test]
    fn the_memory_cap_refuses_past_its_bytes() {
        let cap = InFlight::new(10 << 20);
        let a = cap.reserve(6 << 20).unwrap();
        let busy = cap.reserve(5 << 20).unwrap_err();
        assert_eq!((busy.0.status.as_u16(), busy.0.code), (503, "busy"));
        let b = cap.reserve(4 << 20).unwrap();
        assert_eq!(cap.available(), 0);
        drop(a);
        assert!(cap.reserve(5 << 20).is_ok());
        drop(b);
        assert_eq!(cap.available(), 10 << 20);
        // A request bigger than the whole cap can never fit: refused, never waiting.
        assert_eq!(cap.reserve(11 << 20).unwrap_err().0.code, "busy");
    }
}
