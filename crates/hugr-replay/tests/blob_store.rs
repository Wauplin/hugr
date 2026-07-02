//! Integration of the content-addressed [`BlobStore`] with a trace's
//! [`BlobManifest`] (P3-2 DONE criteria).
//!
//! The headline property: a large tool result is offloaded to the store by
//! digest, the trace carries only the small [`BlobRef`], and on load the bytes
//! rehydrate identically from the manifest's hash. This is exactly what
//! P3-3 (replay) / P3-4 (resume) need to reconstruct a session whose context
//! referenced large payloads by hash.

use hugr_replay::test_support::TempDir;
use hugr_replay::{BlobManifest, BlobStore, Trace};

#[test]
fn large_payload_referenced_by_hash_and_rehydrated_via_manifest() {
    let root = TempDir::new("blobstore-it-manifest");
    let store = BlobStore::new(root.path());

    // A large tool result (e.g. a 500 KiB log dump) — far too big to inline in
    // the durable log / every projection.
    let big = "lorem ipsum dolor sit amet ".repeat(20_000);
    let blob = store.put(big.as_bytes(), "text/plain").unwrap();
    assert!(blob.len > 500_000);

    // The trace carries only the small reference, not the bytes.
    let mut manifest = BlobManifest::new();
    manifest.push(blob.clone());
    let trace = Trace::with_blobs(vec![], vec![], Some(0), manifest);

    // The trace JSON is tiny — it does NOT contain the payload.
    let json = trace.to_json().unwrap();
    assert!(
        json.len() < big.len() / 10,
        "trace must reference the blob, not inline it"
    );

    // Round-trip the trace (the on-disk skeleton) ...
    let loaded = Trace::from_json(&json).unwrap();
    let reffed = &loaded.blobs.refs[0];
    assert_eq!(reffed.hash, blob.hash);

    // ... then rehydrate the bytes from the store by the manifest's hash.
    let rehydrated = store.get(&reffed.hash).unwrap();
    assert_eq!(rehydrated, big.as_bytes(), "rehydrated bytes must match");
}

#[test]
fn storing_the_same_large_payload_twice_dedups() {
    let root = TempDir::new("blobstore-it-dedup");
    let store = BlobStore::new(root.path());

    let big = vec![7u8; 256 * 1024];
    let a = store.put(&big, "application/octet-stream").unwrap();
    let b = store.put(&big, "application/octet-stream").unwrap();

    assert_eq!(a.hash, b.hash);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
