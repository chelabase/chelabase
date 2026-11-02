//! Behaviour every [`ObjectStore`] must have. Run against [`MemoryStore`](crate::MemoryStore)
//! in unit tests and against a live store from `tests/live.rs`. Keys are created under a
//! caller-supplied prefix and removed afterwards.

use bytes::Bytes;

use crate::{ByteRange, CompletedPart, ObjectStore, PutOptions, StorageError, collect};

pub async fn run(store: &dyn ObjectStore, prefix: &str) {
    ranged_read(store, prefix).await;
    missing_and_preconditions(store, prefix).await;
    copy_list_delete(store, prefix).await;
    multipart_out_of_order(store, prefix).await;
    multipart_abort(store, prefix).await;
    unknown_length_stream(store, prefix).await;
    dated_listing_and_open_uploads(store, prefix).await;
}

/// Objects list with the store's own times; open multipart uploads list until aborted.
async fn dated_listing_and_open_uploads(store: &dyn ObjectStore, prefix: &str) {
    use std::time::{Duration, SystemTime};
    let dir = format!("{prefix}/dated/");
    let key = format!("{dir}a");
    store
        .put(&key, Bytes::from_static(b"abc"), PutOptions::default())
        .await
        .unwrap();
    // Another machine's clock: near ours, not equal.
    let near = |t: SystemTime| {
        let now = SystemTime::now();
        t > now - Duration::from_secs(3600) && t < now + Duration::from_secs(3600)
    };
    let listed = store.list_dated(&dir).await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!((listed[0].key.as_str(), listed[0].size), (key.as_str(), 3));
    assert!(near(listed[0].modified), "{listed:?}");

    let open = format!("{dir}open");
    let id = store.multipart_create(&open, None).await.unwrap();
    let uploads = store.list_uploads(&dir).await.unwrap();
    let mine: Vec<_> = uploads.iter().filter(|u| u.upload_id == id).collect();
    assert_eq!(mine.len(), 1, "{uploads:?}");
    assert_eq!(mine[0].key, open);
    // A store may not say when an upload began; one that does is near our clock.
    assert!(mine[0].initiated.is_none_or(near), "{uploads:?}");
    // Outside the prefix: not listed.
    assert!(store.list_uploads(&format!("{prefix}/elsewhere/")).await.unwrap().is_empty());
    store.multipart_abort(&open, &id).await.unwrap();
    assert!(store.list_uploads(&dir).await.unwrap().iter().all(|u| u.upload_id != id));
    store.delete(&key).await.unwrap();
    assert!(store.list_dated(&dir).await.unwrap().is_empty());
}

async fn ranged_read(store: &dyn ObjectStore, prefix: &str) {
    let key = format!("{prefix}/range");
    store
        .put(
            &key,
            Bytes::from_static(b"0123456789abcdef"),
            PutOptions::default(),
        )
        .await
        .unwrap();

    let r = ByteRange::parse("bytes=5-9").unwrap();
    let got = store.get(&key, Some(r)).await.unwrap();
    assert_eq!(got.total_size, 16);
    assert_eq!(
        collect(got.body).await.unwrap(),
        Bytes::from_static(b"56789")
    );

    let got = store
        .get(&key, Some(ByteRange::parse("bytes=12-").unwrap()))
        .await
        .unwrap();
    assert_eq!(
        collect(got.body).await.unwrap(),
        Bytes::from_static(b"cdef")
    );
    let got = store
        .get(&key, Some(ByteRange::parse("bytes=-3").unwrap()))
        .await
        .unwrap();
    assert_eq!(collect(got.body).await.unwrap(), Bytes::from_static(b"def"));

    let got = store.get(&key, None).await.unwrap();
    assert_eq!(collect(got.body).await.unwrap().len(), 16);
    let head = store.head(&key).await.unwrap();
    assert_eq!((head.key.as_str(), head.size), (key.as_str(), 16));
    store.delete(&key).await.unwrap();
}

async fn missing_and_preconditions(store: &dyn ObjectStore, prefix: &str) {
    let key = format!("{prefix}/missing");
    assert!(matches!(
        store.head(&key).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        store.get(&key, None).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        store.copy(&key, &format!("{key}2")).await,
        Err(StorageError::NotFound)
    ));
    store.delete(&key).await.unwrap();

    let once = PutOptions {
        if_none_match: true,
        ..Default::default()
    };
    store
        .put(&key, Bytes::from_static(b"a"), once.clone())
        .await
        .unwrap();
    assert!(matches!(
        store.put(&key, Bytes::from_static(b"b"), once).await,
        Err(StorageError::PreconditionFailed)
    ));
    let got = store.get(&key, None).await.unwrap();
    assert_eq!(collect(got.body).await.unwrap(), Bytes::from_static(b"a"));
    store.delete(&key).await.unwrap();
}

async fn copy_list_delete(store: &dyn ObjectStore, prefix: &str) {
    let dir = format!("{prefix}/list");
    for name in ["b", "a", "c"] {
        store
            .put(
                &format!("{dir}/{name}"),
                Bytes::from(name.to_string()),
                PutOptions::default(),
            )
            .await
            .unwrap();
    }
    store
        .copy(&format!("{dir}/a"), &format!("{dir}/d"))
        .await
        .unwrap();
    let keys: Vec<_> = store
        .list(&format!("{dir}/"))
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert_eq!(keys, ["a", "b", "c", "d"].map(|n| format!("{dir}/{n}")));
    let d = store.get(&format!("{dir}/d"), None).await.unwrap();
    assert_eq!(collect(d.body).await.unwrap(), Bytes::from_static(b"a"));

    for k in keys {
        store.delete(&k).await.unwrap();
    }
    assert!(store.list(&format!("{dir}/")).await.unwrap().is_empty());
}

async fn multipart_out_of_order(store: &dyn ObjectStore, prefix: &str) {
    let key = format!("{prefix}/multi");
    // S3 requires every part but the last to be at least 5 MiB.
    let p1 = Bytes::from(vec![b'1'; 5 * 1024 * 1024]);
    let p2 = Bytes::from(vec![b'2'; 5 * 1024 * 1024]);
    let p3 = Bytes::from_static(b"tail");

    let id = store.multipart_create(&key, None).await.unwrap();
    let e3 = store
        .multipart_part(&key, &id, 3, p3.clone())
        .await
        .unwrap();
    let e1 = store
        .multipart_part(&key, &id, 1, p1.clone())
        .await
        .unwrap();
    let e2 = store
        .multipart_part(&key, &id, 2, p2.clone())
        .await
        .unwrap();
    // Completed out of numeric order on purpose.
    let meta = store
        .multipart_complete(
            &key,
            &id,
            vec![
                CompletedPart {
                    part_number: 3,
                    etag: e3,
                },
                CompletedPart {
                    part_number: 1,
                    etag: e1,
                },
                CompletedPart {
                    part_number: 2,
                    etag: e2,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(meta.size, (p1.len() + p2.len() + p3.len()) as u64);

    let got = collect(store.get(&key, None).await.unwrap().body)
        .await
        .unwrap();
    assert_eq!(got, [p1.as_ref(), p2.as_ref(), p3.as_ref()].concat());
    store.delete(&key).await.unwrap();
}

async fn multipart_abort(store: &dyn ObjectStore, prefix: &str) {
    let key = format!("{prefix}/aborted");
    let id = store.multipart_create(&key, None).await.unwrap();
    store
        .multipart_part(&key, &id, 1, Bytes::from_static(b"x"))
        .await
        .unwrap();
    store.multipart_abort(&key, &id).await.unwrap();

    assert!(matches!(
        store.head(&key).await,
        Err(StorageError::NotFound)
    ));
    assert!(
        store
            .list(&format!("{prefix}/aborted"))
            .await
            .unwrap()
            .is_empty()
    );
    // The upload is gone: parts and completion are refused.
    assert!(matches!(
        store
            .multipart_part(&key, &id, 2, Bytes::from_static(b"y"))
            .await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        store.multipart_complete(&key, &id, vec![]).await,
        Err(StorageError::NotFound)
    ));
}

/// A 12 MiB body with no known length must land intact (via multipart, parts of 8 MiB).
async fn unknown_length_stream(store: &dyn ObjectStore, prefix: &str) {
    let key = format!("{prefix}/stream");
    let chunks: Vec<Result<Bytes, StorageError>> = (0..12u8)
        .map(|i| Ok(Bytes::from(vec![i; 1024 * 1024])))
        .collect();
    let meta = store
        .put_stream(
            &key,
            Box::pin(futures_util::stream::iter(chunks)),
            PutOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(meta.size, 12 * 1024 * 1024);

    let got = collect(store.get(&key, None).await.unwrap().body)
        .await
        .unwrap();
    let want: Vec<u8> = (0..12u8).flat_map(|i| vec![i; 1024 * 1024]).collect();
    assert!(got == want, "streamed object differs");
    store.delete(&key).await.unwrap();
}
