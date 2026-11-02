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

    /// Upload a body of unknown length. S3 rejects `PutObject` without a Content-Length, so
    /// anything of at least [`STREAM_PART_SIZE`] goes through multipart in parts of that size
    /// (at least the 5 MiB S3 minimum); the upload is aborted if the stream or a part fails.
    /// Smaller bodies are a single `put`. With `if_none_match`, a multipart upload checks for the
    /// key first, so a racing writer can still win.
    async fn put_stream(
        &self,
        key: &str,
        mut body: ByteStream,
        opts: PutOptions,
    ) -> Result<ObjectMeta, StorageError> {
        use bytes::BytesMut;
        use futures_util::StreamExt;

        let mut buf = BytesMut::new();
        while buf.len() < STREAM_PART_SIZE {
            match body.next().await {
                Some(chunk) => buf.extend_from_slice(&chunk?),
                None => return self.put(key, buf.freeze(), opts).await,
            }
        }
        if opts.if_none_match && self.head(key).await.is_ok() {
            return Err(StorageError::PreconditionFailed);
        }
        let id = self
            .multipart_create(key, opts.content_type.as_deref())
            .await?;
        let result = async {
            let mut parts = Vec::new();
            let mut ended = false;
            loop {
                while !ended && buf.len() < STREAM_PART_SIZE {
                    match body.next().await {
                        Some(chunk) => buf.extend_from_slice(&chunk?),
                        None => ended = true,
                    }
                }
                if buf.is_empty() {
                    break;
                }
                let n = parts.len() as u32 + 1;
                if n > 10_000 {
                    return Err(StorageError::Other("stream exceeds 10000 parts".into()));
                }
                let part = buf.split_to(buf.len().min(STREAM_PART_SIZE)).freeze();
                let etag = self.multipart_part(key, &id, n, part).await?;
                parts.push(CompletedPart {
                    part_number: n,
                    etag,
                });
            }
            self.multipart_complete(key, &id, parts).await
        }
        .await;
        if result.is_err() {
            let _ = self.multipart_abort(key, &id).await;
        }
        result
    }

    /// Start a multipart upload; returns the upload id.
    async fn multipart_create(
        &self,
        key: &str,
        content_type: Option<&str>,
    ) -> Result<String, StorageError>;
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
    /// Copy bytes `range` (half-open) of the object `src` into part `part_number` of an upload
    /// (S3's `UploadPartCopy`); returns the part's etag.
    async fn multipart_part_copy(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        src: &str,
        range: std::ops::Range<u64>,
    ) -> Result<String, StorageError>;
}

/// The largest object one `CopyObject` copies on Amazon S3 (5 GB); above it [`copy_in_parts`].
pub const COPY_OBJECT_MAX: u64 = 5_000_000_000;

/// The part size [`copy_in_parts`] uses above [`COPY_OBJECT_MAX`]: 10000 parts of it cover
/// S3's 5 TiB object limit.
pub const COPY_PART_SIZE: u64 = 1 << 30;

/// Copies `src` (`size` bytes) to `dst` with a multipart upload of `UploadPartCopy` parts of
/// `part_size` bytes (the last smaller), aborted if any part fails.
pub async fn copy_in_parts(
    store: &(impl ObjectStore + ?Sized),
    src: &str,
    dst: &str,
    size: u64,
    part_size: u64,
) -> Result<ObjectMeta, StorageError> {
    let part_size = part_size.max(1);
    let id = store.multipart_create(dst, None).await?;
    let result = async {
        let mut parts = Vec::new();
        let mut start = 0;
        while start < size || parts.is_empty() {
            let end = (start + part_size).min(size);
            let part_number = parts.len() as u32 + 1;
            let etag = store
                .multipart_part_copy(dst, &id, part_number, src, start..end)
                .await?;
            parts.push(CompletedPart { part_number, etag });
            start = end;
        }
        store.multipart_complete(dst, &id, parts).await
    }
    .await;
    if result.is_err() {
        let _ = store.multipart_abort(dst, &id).await;
    }
    result
}

/// Part size for [`ObjectStore::put_stream`]: above S3's 5 MiB minimum.
pub const STREAM_PART_SIZE: usize = 8 * 1024 * 1024;

/// Collect a body stream into one buffer.
pub async fn collect(mut body: ByteStream) -> Result<Bytes, StorageError> {
    use futures_util::StreamExt;
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out.into())
}
