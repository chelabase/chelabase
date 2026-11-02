use bytes::Bytes;
use futures_util::stream::BoxStream;

/// Why a storage call failed. Callers retry only [`Transient`](Self::Transient).
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("object not found")]
    NotFound,
    #[error("precondition failed")]
    PreconditionFailed,
    /// Connect failure, timeout, throttling or a 5xx: worth retrying.
    #[error("transient storage error: {0}")]
    Transient(String),
    /// Anything else (bad request, access denied, malformed range).
    #[error("storage error: {0}")]
    Other(String),
}

/// A streaming object body.
pub type ByteStream = BoxStream<'static, Result<Bytes, StorageError>>;

/// An HTTP byte range; both ends inclusive, as in `Range: bytes=5-9`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=start-end`, or `bytes=start-` when `end` is `None`.
    From { start: u64, end: Option<u64> },
    /// `bytes=-n`: the last `n` bytes.
    Suffix(u64),
}

impl ByteRange {
    /// Parse a `bytes=…` header value (single range only).
    pub fn parse(s: &str) -> Result<Self, StorageError> {
        let bad = || StorageError::Other(format!("invalid range {s:?}"));
        let spec = s.strip_prefix("bytes=").ok_or_else(bad)?;
        let (a, b) = spec.split_once('-').ok_or_else(bad)?;
        match (a, b) {
            ("", n) => Ok(Self::Suffix(n.parse().map_err(|_| bad())?)),
            (a, "") => Ok(Self::From {
                start: a.parse().map_err(|_| bad())?,
                end: None,
            }),
            (a, b) => {
                let (start, end): (u64, u64) =
                    (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?);
                if end < start {
                    return Err(bad());
                }
                Ok(Self::From {
                    start,
                    end: Some(end),
                })
            }
        }
    }

    /// The `Range` header value.
    pub fn header(&self) -> String {
        match self {
            Self::From {
                start,
                end: Some(end),
            } => format!("bytes={start}-{end}"),
            Self::From { start, end: None } => format!("bytes={start}-"),
            Self::Suffix(n) => format!("bytes=-{n}"),
        }
    }

    /// Resolve against an object of `size` bytes to a half-open `start..end`, clamped.
    pub fn resolve(&self, size: u64) -> Result<std::ops::Range<u64>, StorageError> {
        let (start, end) = match *self {
            Self::From { start, end } => {
                (start, end.map_or(size, |e| e.saturating_add(1).min(size)))
            }
            Self::Suffix(n) => (size.saturating_sub(n), size),
        };
        if start >= size && size > 0 || start > end {
            return Err(StorageError::Other("range not satisfiable".into()));
        }
        Ok(start..end)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    pub etag: String,
}

#[derive(Clone, Debug, Default)]
pub struct PutOptions {
    pub content_type: Option<String>,
    /// Fail with [`StorageError::PreconditionFailed`] if the key already exists.
    pub if_none_match: bool,
}

pub struct GetObject {
    pub body: ByteStream,
    /// Total size of the object, not of the returned range.
    pub total_size: u64,
    pub etag: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedPart {
    pub part_number: u32,
    pub etag: String,
}

#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOptions,
    ) -> Result<ObjectMeta, StorageError>;
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<GetObject, StorageError>;
    async fn head(&self, key: &str) -> Result<ObjectMeta, StorageError>;
    /// Deleting a missing key succeeds.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
    async fn copy(&self, src: &str, dst: &str) -> Result<ObjectMeta, StorageError>;
    /// Every object under `prefix`, in key order (pages are followed internally).
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, StorageError>;

    /// Start a multipart upload; returns the upload id.
    async fn multipart_create(&self, key: &str) -> Result<String, StorageError>;
    /// Parts may be uploaded in any order; returns the part's etag.
    async fn multipart_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Bytes,
    ) -> Result<String, StorageError>;
    /// Assemble the object from `parts` in ascending part-number order, whatever order given.
    async fn multipart_complete(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<CompletedPart>,
    ) -> Result<ObjectMeta, StorageError>;
    /// Drop the upload and its parts; nothing remains.
    async fn multipart_abort(&self, key: &str, upload_id: &str) -> Result<(), StorageError>;
}

/// Collect a body stream into one buffer.
pub async fn collect(mut body: ByteStream) -> Result<Bytes, StorageError> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out.into())
}
