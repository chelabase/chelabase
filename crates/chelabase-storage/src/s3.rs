use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use aws_credential_types::{
    Credentials,
    provider::{ProvideCredentials, error::CredentialsError, future},
};
use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Region, RequestChecksumCalculation, ResponseChecksumValidation,
        StalledStreamProtectionConfig, http::HttpResponse, retry::RetryConfig,
        timeout::TimeoutConfig,
    },
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::ByteStream as SdkBody,
    types::{CompletedMultipartUpload, CompletedPart as SdkPart},
};
use bytes::Bytes;
use futures_util::stream;

use crate::{
    ByteRange, CompletedPart, CredentialSource, GetObject, ObjectMeta, ObjectStore, PutOptions,
    StorageConfig, StorageError,
    config::{ENV_ACCESS_KEY_FILE, ENV_SECRET_KEY_FILE, read_key_file},
};

/// How long the SDK may cache key-file credentials before asking us to re-read the files.
const KEY_FILE_REREAD: Duration = Duration::from_secs(300);

/// Credentials from two files. Each credentials value expires after [`KEY_FILE_REREAD`], so the
/// SDK's cache calls back and a rotated file is picked up without a restart.
#[derive(Debug)]
pub struct KeyFileCredentials {
    access_key_file: PathBuf,
    secret_key_file: PathBuf,
}

impl KeyFileCredentials {
    pub fn new(access_key_file: PathBuf, secret_key_file: PathBuf) -> Self {
        Self {
            access_key_file,
            secret_key_file,
        }
    }

    async fn load(&self) -> aws_credential_types::provider::Result {
        // `ConfigError` never carries file contents, so it is safe to surface.
        let read = |var, path: PathBuf| async move {
            tokio::task::spawn_blocking(move || read_key_file(var, &path))
                .await
                .map_err(CredentialsError::provider_error)?
                .map_err(CredentialsError::provider_error)
        };
        let access = read(ENV_ACCESS_KEY_FILE, self.access_key_file.clone()).await?;
        let secret = read(ENV_SECRET_KEY_FILE, self.secret_key_file.clone()).await?;
        Ok(Credentials::new(
            access,
            secret,
            None,
            Some(SystemTime::now() + KEY_FILE_REREAD),
            "chelabase-key-files",
        ))
    }
}

impl ProvideCredentials for KeyFileCredentials {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(self.load())
    }
}

/// [`ObjectStore`] over any S3-compatible service (SeaweedFS, Amazon S3, MinIO, ...).
#[derive(Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
}

impl S3Store {
    pub async fn new(cfg: &StorageConfig) -> Self {
        // `operation_attempt_timeout` covers sending the request and getting the response
        // headers, not a response body being read, so a long download is not cut off by
        // `request_timeout`. A stalled download is bounded by the SDK's stalled-stream
        // protection instead: a body delivering nothing for `stream_idle_timeout` fails.
        let timeouts = TimeoutConfig::builder()
            .connect_timeout(cfg.connect_timeout)
            .operation_attempt_timeout(cfg.request_timeout)
            .build();
        let mut b = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .force_path_style(cfg.path_style)
            .timeout_config(timeouts)
            .stalled_stream_protection(
                StalledStreamProtectionConfig::enabled()
                    .grace_period(cfg.stream_idle_timeout)
                    .build(),
            )
            .retry_config(RetryConfig::standard().with_max_attempts(cfg.max_attempts))
            // Checksum trailers are not understood by every S3-compatible store.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        b = match &cfg.credentials {
            CredentialSource::KeyFiles {
                access_key_file,
                secret_key_file,
            } => b.credentials_provider(KeyFileCredentials::new(
                access_key_file.clone(),
                secret_key_file.clone(),
            )),
            CredentialSource::DefaultChain => b.credentials_provider(
                aws_config::default_provider::credentials::DefaultCredentialsChain::builder()
                    .region(Region::new(cfg.region.clone()))
                    .build()
                    .await,
            ),
        };
        if let Some(endpoint) = &cfg.endpoint {
            b = b.endpoint_url(endpoint);
        }
        Self {
            client: Client::from_conf(b.build()),
            bucket: cfg.bucket.clone(),
        }
    }
}

fn map_err<E: ProvideErrorMetadata + std::error::Error + 'static>(
    e: SdkError<E, HttpResponse>,
) -> StorageError {
    let status = e.raw_response().map(|r| r.status().as_u16());
    let code = e.code().map(str::to_owned);
    let msg = DisplayErrorContext(&e).to_string();
    match (status, code.as_deref()) {
        (_, Some("NoSuchKey" | "NotFound" | "NoSuchUpload" | "NoSuchBucket")) | (Some(404), _) => {
            StorageError::NotFound
        }
        (_, Some("PreconditionFailed")) | (Some(412), _) => StorageError::PreconditionFailed,
        (Some(429 | 500..=599), _) => StorageError::Transient(msg),
        (_, Some("SlowDown" | "RequestTimeout" | "InternalError" | "ServiceUnavailable")) => {
            StorageError::Transient(msg)
        }
        // No response at all: connect failure, timeout, dispatch error.
        (None, _) if !matches!(e, SdkError::ConstructionFailure(_)) => StorageError::Transient(msg),
        _ => StorageError::Other(msg),
    }
}

fn meta(key: &str, size: Option<i64>, etag: Option<&str>) -> ObjectMeta {
    ObjectMeta {
        key: key.to_string(),
        size: size.unwrap_or(0).max(0) as u64,
        etag: etag.unwrap_or_default().trim_matches('"').to_string(),
    }
}

#[async_trait::async_trait]
impl ObjectStore for S3Store {
    async fn put(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOptions,
    ) -> Result<ObjectMeta, StorageError> {
        let size = body.len() as i64;
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(SdkBody::from(body));
        if let Some(ct) = opts.content_type {
            req = req.content_type(ct);
        }
        if opts.if_none_match {
            req = req.if_none_match("*");
        }
        let out = req.send().await.map_err(map_err)?;
        Ok(meta(key, Some(size), out.e_tag()))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<GetObject, StorageError> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some(r) = range {
            req = req.range(r.header());
        }
        let out = req.send().await.map_err(map_err)?;
        // A ranged response reports the whole object after the slash: `bytes 5-9/16`.
        let total_size = out
            .content_range()
            .and_then(|cr| cr.rsplit('/').next())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or_else(|| out.content_length().unwrap_or(0).max(0) as u64);
        let etag = out
            .e_tag()
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let body = stream::unfold(out.body, |mut b| async move {
            match b.try_next().await {
                Ok(Some(chunk)) => Some((Ok(chunk), b)),
                Ok(None) => None,
                Err(e) => Some((Err(StorageError::Transient(format!("body read: {e}"))), b)),
            }
        });
        Ok(GetObject {
            body: Box::pin(body),
            total_size,
            etag,
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, StorageError> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(map_err)?;
        Ok(meta(key, out.content_length(), out.e_tag()))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match self
            .client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => match map_err(e) {
                StorageError::NotFound => Ok(()),
                other => Err(other),
            },
        }
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<ObjectMeta, StorageError> {
        let source = format!("{}/{}", self.bucket, src);
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(dst)
            .copy_source(urlencoding_path(&source))
            .send()
            .await
            .map_err(map_err)?;
        // CopyObject's own result has no size; head the destination.
        self.head(dst).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        let mut out = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            for o in page.map_err(map_err)?.contents() {
                out.push(meta(o.key().unwrap_or_default(), o.size(), o.e_tag()));
            }
        }
        Ok(out)
    }

    async fn multipart_create(
        &self,
        key: &str,
        content_type: Option<&str>,
    ) -> Result<String, StorageError> {
        let mut req = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key);
        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        let out = req.send().await.map_err(map_err)?;
        out.upload_id()
            .map(str::to_owned)
            .ok_or_else(|| StorageError::Other("no upload id returned".into()))
    }

    async fn multipart_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Bytes,
    ) -> Result<String, StorageError> {
        let out = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number as i32)
            .body(SdkBody::from(body))
            .send()
            .await
            .map_err(map_err)?;
        Ok(out.e_tag().unwrap_or_default().to_string())
    }

    async fn multipart_complete(
        &self,
        key: &str,
        upload_id: &str,
        mut parts: Vec<CompletedPart>,
    ) -> Result<ObjectMeta, StorageError> {
        parts.sort_by_key(|p| p.part_number);
        let parts = parts
            .into_iter()
            .map(|p| {
                SdkPart::builder()
                    .part_number(p.part_number as i32)
                    .e_tag(p.etag)
                    .build()
            })
            .collect::<Vec<_>>();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(map_err)?;
        self.head(key).await
    }

    async fn multipart_abort(&self, key: &str, upload_id: &str) -> Result<(), StorageError> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

/// Percent-encode `bucket/key` for `x-amz-copy-source`, keeping the slashes.
fn urlencoding_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::collect;

    #[test]
    fn copy_source_is_percent_encoded() {
        assert_eq!(urlencoding_path("b/a dir/ü+x"), "b/a%20dir/%C3%BC%2Bx");
    }

    fn cfg(dir: &std::path::Path, endpoint: &str, extra: &[(&str, &str)]) -> StorageConfig {
        for n in ["a", "s"] {
            let p = dir.join(n);
            if !p.exists() {
                std::fs::write(p, "k").unwrap();
            }
        }
        let (a, s) = (dir.join("a"), dir.join("s"));
        let extra: Vec<(String, String)> = extra
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        StorageConfig::from_lookup(|k| {
            if let Some((_, v)) = extra.iter().find(|(ek, _)| ek == k) {
                return Some(v.clone());
            }
            Some(match k {
                "CHELABASE_STORAGE_S3_ENDPOINT" => endpoint.into(),
                "CHELABASE_STORAGE_S3_BUCKET" => "b".into(),
                "CHELABASE_STORAGE_S3_ACCESS_KEY_FILE" => a.to_string_lossy().into(),
                "CHELABASE_STORAGE_S3_SECRET_KEY_FILE" => s.to_string_lossy().into(),
                "CHELABASE_STORAGE_S3_MAX_ATTEMPTS" => "1".into(),
                _ => return None,
            })
        })
        .unwrap()
    }

    #[tokio::test]
    async fn connect_failure_is_transient() {
        let dir = tempfile::tempdir().unwrap();
        let err = S3Store::new(&cfg(dir.path(), "http://127.0.0.1:1", &[]))
            .await
            .head("k")
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Transient(_)), "{err:?}");
    }

    #[tokio::test]
    async fn rotated_key_files_are_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let (a, s) = (dir.path().join("a"), dir.path().join("s"));
        std::fs::write(&a, "access-1\n").unwrap();
        std::fs::write(&s, "secret-1\n").unwrap();
        let p = KeyFileCredentials::new(a.clone(), s.clone());

        let c1 = p.provide_credentials().await.unwrap();
        assert_eq!(
            (c1.access_key_id(), c1.secret_access_key()),
            ("access-1", "secret-1")
        );
        // Expires, so the SDK's cache asks again.
        assert!(c1.expiry().is_some());

        std::fs::write(&a, "access-2\n").unwrap();
        std::fs::write(&s, "secret-2\n").unwrap();
        let c2 = p.provide_credentials().await.unwrap();
        assert_eq!(
            (c2.access_key_id(), c2.secret_access_key()),
            ("access-2", "secret-2")
        );
    }

    #[tokio::test]
    async fn unreadable_key_file_is_a_credentials_error_without_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = KeyFileCredentials::new(dir.path().join("nope"), dir.path().join("nope2"));
        let err = format!("{:?}", p.provide_credentials().await.unwrap_err());
        assert!(
            err.contains("CHELABASE_STORAGE_S3_ACCESS_KEY_FILE"),
            "{err}"
        );
    }

    /// One-shot HTTP server: answers any request with `body.len()` bytes, one byte every
    /// `gap`, and goes silent for `stall` after `stall_after` bytes (if set).
    async fn slow_server(body: &'static [u8], gap: Duration, stall_after: Option<usize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buf = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                seen.extend_from_slice(&buf[..n]);
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"x\"\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).await.unwrap();
            for (i, b) in body.iter().enumerate() {
                if stall_after == Some(i) {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                tokio::time::sleep(gap).await;
                if sock.write_all(&[*b]).await.is_err() {
                    return;
                }
                let _ = sock.flush().await;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn slow_but_steady_download_outlives_the_request_timeout() {
        let dir = tempfile::tempdir().unwrap();
        // 8 bytes at 150 ms: ~1.2 s in total, against a 500 ms request timeout.
        let url = slow_server(b"abcdefgh", Duration::from_millis(150), None).await;
        let c = cfg(
            dir.path(),
            &url,
            &[
                ("CHELABASE_STORAGE_S3_REQUEST_TIMEOUT", "500ms"),
                ("CHELABASE_STORAGE_S3_STREAM_IDLE_TIMEOUT", "2s"),
            ],
        );
        let got = S3Store::new(&c).await.get("k", None).await.unwrap();
        assert_eq!(
            collect(got.body).await.unwrap(),
            Bytes::from_static(b"abcdefgh")
        );
    }

    #[tokio::test]
    async fn stalled_download_fails_as_transient() {
        let dir = tempfile::tempdir().unwrap();
        let url = slow_server(b"abcdefgh", Duration::from_millis(10), Some(3)).await;
        let c = cfg(
            dir.path(),
            &url,
            &[("CHELABASE_STORAGE_S3_STREAM_IDLE_TIMEOUT", "400ms")],
        );
        let got = S3Store::new(&c).await.get("k", None).await.unwrap();
        let start = std::time::Instant::now();
        let res = tokio::time::timeout(Duration::from_secs(10), collect(got.body))
            .await
            .expect("stream hung");
        assert!(matches!(res, Err(StorageError::Transient(_))), "{res:?}");
        // The configured 400 ms idle timeout must apply, not the SDK's 5 s default grace period.
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "took {:?}",
            start.elapsed()
        );
    }
}
