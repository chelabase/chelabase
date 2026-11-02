use std::{
    collections::{BTreeMap, HashMap},
    hash::{DefaultHasher, Hash, Hasher},
    sync::Mutex,
};

use bytes::Bytes;
use futures_util::stream;

use crate::{
    ByteRange, CompletedPart, GetObject, ObjectMeta, ObjectStore, PutOptions, StorageError,
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
        st.objects.insert(key.to_string(), body);
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
        self.state.lock().unwrap().objects.remove(key);
        Ok(())
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<ObjectMeta, StorageError> {
        let mut st = self.state.lock().unwrap();
        let body = st.objects.get(src).cloned().ok_or(StorageError::NotFound)?;
        let m = meta(dst, &body);
        st.objects.insert(dst.to_string(), body);
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

    async fn multipart_create(&self, key: &str) -> Result<String, StorageError> {
        let mut st = self.state.lock().unwrap();
        st.next_upload += 1;
        let id = format!("upload-{}", st.next_upload);
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
        let body = Bytes::from(out);
        let m = meta(key, &body);
        st.objects.insert(key.to_string(), body);
        Ok(m)
    }

    async fn multipart_abort(&self, key: &str, upload_id: &str) -> Result<(), StorageError> {
        let mut st = self.state.lock().unwrap();
        match st.uploads.get(upload_id) {
            Some((k, _)) if k == key => {
                st.uploads.remove(upload_id);
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
