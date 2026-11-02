use std::{
    collections::{BTreeMap, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    sync::Mutex,
    time::SystemTime,
};

use bytes::Bytes;
use futures_util::stream;

use crate::{
    ByteRange, CompletedPart, DatedObject, GetObject, ObjectMeta, ObjectStore, OpenUpload,
    PutOptions, StorageError,
};

/// In-process [`ObjectStore`] for tests.
#[derive(Default)]
pub struct MemoryStore {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    objects: BTreeMap<String, Bytes>,
    /// upload id -> (key, part number -> body)
    uploads: HashMap<String, (String, BTreeMap<u32, Bytes>)>,
    next_upload: u64,
    uploads_started: usize,
    /// The store's clock: [`MemoryStore::set_now`]'s time, else the system's.
    now: Option<SystemTime>,
    /// When each object was last written, by the store's clock.
    modified: HashMap<String, SystemTime>,
    /// When each open upload began.
    initiated: HashMap<String, SystemTime>,
    /// List uploads without their times, as SeaweedFS does.
    hide_upload_times: bool,
}

impl State {
    fn now(&self) -> SystemTime {
        self.now.unwrap_or_else(SystemTime::now)
    }

    fn write(&mut self, key: &str, body: Bytes) {
        let now = self.now();
        self.modified.insert(key.to_string(), now);
        self.objects.insert(key.to_string(), body);
    }
}

impl MemoryStore {
    /// How many multipart uploads have been created (for tests).
    pub fn uploads_started(&self) -> usize {
        self.state.lock().unwrap().uploads_started
    }

    /// Sets the store's clock (the times it lists) from now on.
    pub fn set_now(&self, now: SystemTime) {
        self.state.lock().unwrap().now = Some(now);
    }

    /// From now on, open uploads list without when they began (as SeaweedFS lists them).
    pub fn hide_upload_times(&self) {
        self.state.lock().unwrap().hide_upload_times = true;
    }

    /// The store's clock.
    pub fn now(&self) -> SystemTime {
        self.state.lock().unwrap().now()
    }

    /// Multipart uploads neither completed nor aborted (for tests).
    pub fn uploads_open(&self) -> usize {
        self.state.lock().unwrap().uploads.len()
    }
}

fn etag(b: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn meta(key: &str, body: &Bytes) -> ObjectMeta {
    ObjectMeta {
        key: key.to_string(),
        size: body.len() as u64,
        etag: etag(body),
    }
}

#[async_trait::async_trait]
impl ObjectStore for MemoryStore {
    async fn put(
        &self,
        key: &str,
        body: Bytes,
        opts: PutOptions,
    ) -> Result<ObjectMeta, StorageError> {
        let mut st = self.state.lock().unwrap();
        if opts.if_none_match && st.objects.contains_key(key) {
            return Err(StorageError::PreconditionFailed);
        }
        let m = meta(key, &body);
        st.write(key, body);
        Ok(m)
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<GetObject, StorageError> {
        let body = self
            .state
            .lock()
            .unwrap()
            .objects
            .get(key)
            .cloned()
            .ok_or(StorageError::NotFound)?;
        let total_size = body.len() as u64;
        let etag = etag(&body);
        let slice = match range {
            Some(r) => {
                let r = r.resolve(total_size)?;
                body.slice(r.start as usize..r.end as usize)
            }
            None => body,
        };
        Ok(GetObject {
            body: Box::pin(stream::once(async move { Ok(slice) })),
            total_size,
            etag,
        })
    }

    async fn head(&self, key: &str) -> Result<ObjectMeta, StorageError> {
        let st = self.state.lock().unwrap();
        st.objects
            .get(key)
            .map(|b| meta(key, b))
            .ok_or(StorageError::NotFound)
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let mut st = self.state.lock().unwrap();
        st.objects.remove(key);
        st.modified.remove(key);
        Ok(())
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<ObjectMeta, StorageError> {
        let mut st = self.state.lock().unwrap();
        let body = st.objects.get(src).cloned().ok_or(StorageError::NotFound)?;
        let m = meta(dst, &body);
        st.write(dst, body);
        Ok(m)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>, StorageError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .objects
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, b)| meta(k, b))
            .collect())
    }

    async fn list_dated(&self, prefix: &str) -> Result<Vec<DatedObject>, StorageError> {
        let st = self.state.lock().unwrap();
        Ok(st
            .objects
            .range(prefix.to_string()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, b)| DatedObject {
                key: k.clone(),
                size: b.len() as u64,
                modified: st.modified[k],
            })
            .collect())
    }

    async fn list_uploads(&self, prefix: &str) -> Result<Vec<OpenUpload>, StorageError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<OpenUpload> = st
            .uploads
            .iter()
            .filter(|(_, (k, _))| k.starts_with(prefix))
            .map(|(id, (k, _))| OpenUpload {
                key: k.clone(),
                upload_id: id.clone(),
                initiated: (!st.hide_upload_times).then(|| st.initiated[id]),
            })
            .collect();
        out.sort_by(|a, b| (&a.key, &a.upload_id).cmp(&(&b.key, &b.upload_id)));
        Ok(out)
    }

    async fn multipart_create(
        &self,
        key: &str,
        _content_type: Option<&str>,
    ) -> Result<String, StorageError> {
        let mut st = self.state.lock().unwrap();
        st.next_upload += 1;
        st.uploads_started += 1;
        let id = format!("upload-{}", st.next_upload);
        let now = st.now();
        st.initiated.insert(id.clone(), now);
        st.uploads
            .insert(id.clone(), (key.to_string(), BTreeMap::new()));
        Ok(id)
    }

    async fn multipart_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: u32,
        body: Bytes,
    ) -> Result<String, StorageError> {
        if !(1..=10_000).contains(&part_number) {
            return Err(StorageError::Other("part number must be 1..=10000".into()));
        }
        let mut st = self.state.lock().unwrap();
        let (k, parts) = st
            .uploads
            .get_mut(upload_id)
            .ok_or(StorageError::NotFound)?;
        if k != key {
            return Err(StorageError::NotFound);
        }
        let e = etag(&body);
        parts.insert(part_number, body);
        Ok(e)
    }

    async fn multipart_complete(
        &self,
        key: &str,
        upload_id: &str,
        mut completed: Vec<CompletedPart>,
    ) -> Result<ObjectMeta, StorageError> {
        let mut st = self.state.lock().unwrap();
        match st.uploads.get(upload_id) {
            Some((k, _)) if k == key => {}
            _ => return Err(StorageError::NotFound),
        }
        completed.sort_by_key(|p| p.part_number);
        let mut out = Vec::new();
        {
            let (_, parts) = &st.uploads[upload_id];
            for p in &completed {
                match parts.get(&p.part_number) {
                    Some(b) if etag(b) == p.etag => out.extend_from_slice(b),
                    _ => {
                        return Err(StorageError::Other(format!(
                            "invalid part {}",
                            p.part_number
                        )));
                    }
                }
            }
        }
        st.uploads.remove(upload_id);
        st.initiated.remove(upload_id);
        let body = Bytes::from(out);
        let m = meta(key, &body);
        st.write(key, body);
        Ok(m)
    }

    async fn multipart_abort(&self, key: &str, upload_id: &str) -> Result<(), StorageError> {
        let mut st = self.state.lock().unwrap();
        match st.uploads.get(upload_id) {
            Some((k, _)) if k == key => {
                st.uploads.remove(upload_id);
                st.initiated.remove(upload_id);
                Ok(())
            }
            _ => Err(StorageError::NotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passes_conformance_suite() {
        crate::conformance::run(&MemoryStore::default(), "t").await;
    }

    fn chunks(n: usize, chunk: usize, then_err: bool) -> crate::ByteStream {
        let mut items: Vec<Result<Bytes, StorageError>> =
            (0..n).map(|_| Ok(Bytes::from(vec![7u8; chunk]))).collect();
        if then_err {
            items.push(Err(StorageError::Transient("boom".into())));
        }
        Box::pin(futures_util::stream::iter(items))
    }

    #[tokio::test]
    async fn times_come_from_the_stores_own_clock() {
        use std::time::{Duration, SystemTime};
        let s = MemoryStore::default();
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        s.set_now(t0);
        s.put("p/a", Bytes::from_static(b"x"), PutOptions::default())
            .await
            .unwrap();
        let id = s.multipart_create("p/b", None).await.unwrap();
        s.set_now(t0 + Duration::from_secs(60));
        s.copy("p/a", "p/c").await.unwrap();
        let listed = s.list_dated("p/").await.unwrap();
        let times: Vec<_> = listed.iter().map(|o| (o.key.as_str(), o.modified)).collect();
        assert_eq!(times, [("p/a", t0), ("p/c", t0 + Duration::from_secs(60))]);
        let uploads = s.list_uploads("p/").await.unwrap();
        assert_eq!(uploads.len(), 1);
        assert_eq!((uploads[0].upload_id.as_str(), uploads[0].initiated), (id.as_str(), Some(t0)));
        assert_eq!(s.now(), t0 + Duration::from_secs(60));
    }

    #[tokio::test]
    async fn put_stream_uses_multipart_for_large_bodies() {
        let s = MemoryStore::default();
        let m = s
            .put_stream("big", chunks(12, 1 << 20, false), PutOptions::default())
            .await
            .unwrap();
        assert_eq!(m.size, 12 << 20);
        assert_eq!(s.uploads_started(), 1);
        assert_eq!(s.uploads_open(), 0);
        assert_eq!(s.head("big").await.unwrap().size, 12 << 20);
    }

    #[tokio::test]
    async fn put_stream_small_body_is_a_single_put() {
        let s = MemoryStore::default();
        s.put_stream("small", chunks(3, 1000, false), PutOptions::default())
            .await
            .unwrap();
        assert_eq!(s.uploads_started(), 0);
        assert_eq!(s.head("small").await.unwrap().size, 3000);
    }

    #[tokio::test]
    async fn put_stream_aborts_on_error() {
        let s = MemoryStore::default();
        let err = s
            .put_stream("bad", chunks(12, 1 << 20, true), PutOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::Transient(_)));
        assert_eq!(s.uploads_started(), 1);
        assert_eq!(s.uploads_open(), 0);
        assert!(matches!(s.head("bad").await, Err(StorageError::NotFound)));
    }

    #[test]
    fn byte_range_parsing() {
        assert_eq!(
            ByteRange::parse("bytes=5-9").unwrap(),
            ByteRange::From {
                start: 5,
                end: Some(9)
            }
        );
        assert_eq!(ByteRange::parse("bytes=-3").unwrap(), ByteRange::Suffix(3));
        assert!(ByteRange::parse("bytes=9-5").is_err());
        assert!(ByteRange::parse("5-9").is_err());
        assert_eq!(
            ByteRange::From {
                start: 5,
                end: Some(9)
            }
            .header(),
            "bytes=5-9"
        );
        assert_eq!(
            ByteRange::From {
                start: 5,
                end: Some(99)
            }
            .resolve(16)
            .unwrap(),
            5..16
        );
        assert!(
            ByteRange::From {
                start: 16,
                end: None
            }
            .resolve(16)
            .is_err()
        );
    }
}
