use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client,
    config::{
        BehaviorVersion, Region, RequestChecksumCalculation, ResponseChecksumValidation,
        http::HttpResponse, retry::RetryConfig, timeout::TimeoutConfig,
    },
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::ByteStream as SdkBody,
    types::{CompletedMultipartUpload, CompletedPart as SdkPart},
};
use bytes::Bytes;
use futures_util::stream;

use crate::{
    ByteRange, CompletedPart, GetObject, ObjectMeta, ObjectStore, PutOptions, StorageConfig,
    StorageError,
};

/// [`ObjectStore`] over any S3-compatible service (SeaweedFS, Amazon S3, MinIO, ...).
#[derive(Clone)]
pub struct S3Store {
    client: Client,
    bucket: String,
}

impl S3Store {
    pub fn new(cfg: &StorageConfig) -> Self {
        let creds = Credentials::new(
            &cfg.access_key,
            &cfg.secret_key,
            None,
            None,
            "chelabase-key-files",
        );
        let timeouts = TimeoutConfig::builder()
            .connect_timeout(cfg.connect_timeout)
            .operation_attempt_timeout(cfg.request_timeout)
            .build();
        let mut b = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(cfg.region.clone()))
            .credentials_provider(creds)
            .force_path_style(cfg.path_style)
            .timeout_config(timeouts)
            .retry_config(RetryConfig::standard().with_max_attempts(cfg.max_attempts))
            // Checksum trailers are not understood by every S3-compatible store.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
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

    async fn multipart_create(&self, key: &str) -> Result<String, StorageError> {
        let out = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(map_err)?;
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
    use super::*;

    #[test]
    fn copy_source_is_percent_encoded() {
        assert_eq!(urlencoding_path("b/a dir/ü+x"), "b/a%20dir/%C3%BC%2Bx");
    }

    #[tokio::test]
    async fn connect_failure_is_transient() {
        let dir = tempfile::tempdir().unwrap();
        for n in ["a", "s"] {
            std::fs::write(dir.path().join(n), "k").unwrap();
        }
        let (a, s) = (dir.path().join("a"), dir.path().join("s"));
        let cfg = StorageConfig::from_lookup(|k| {
            Some(match k {
                "CHELABASE_STORAGE_S3_ENDPOINT" => "http://127.0.0.1:1".into(),
                "CHELABASE_STORAGE_S3_BUCKET" => "b".into(),
                "CHELABASE_STORAGE_S3_ACCESS_KEY_FILE" => a.to_string_lossy().into(),
                "CHELABASE_STORAGE_S3_SECRET_KEY_FILE" => s.to_string_lossy().into(),
                "CHELABASE_STORAGE_S3_MAX_ATTEMPTS" => "1".into(),
                _ => return None,
            })
        })
        .unwrap();
        let err = S3Store::new(&cfg).head("k").await.unwrap_err();
        assert!(matches!(err, StorageError::Transient(_)), "{err:?}");
    }
}
