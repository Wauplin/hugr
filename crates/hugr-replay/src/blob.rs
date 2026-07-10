//! # Content-addressed blob store
//!
//! Large tool outputs / inputs do not belong inline in the durable log — they
//! would bloat every trace and every context projection. Instead the host
//! stores them by **content hash** and the log/trace carries only a small
//! [`BlobRef`] (`{ hash, len, media }`), so a [`Trace`](crate::Trace) can ship
//! with or without its blob bytes.
//!
//! The key of a blob is the SHA-256 of its bytes, rendered as `"sha256:<hex>"`.
//! Storing identical content twice lands on the same path and is a no-op the
//! second time — natural deduplication.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{BlobRef, TraceError};

/// A disk-backed, content-addressed blob store rooted at a configurable
/// directory. The file name of a blob is its content hash, so the same bytes
/// stored twice dedupe to one file.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// A blob store rooted at `root`. The directory is created lazily on the
    /// first [`put`](BlobStore::put); construction itself touches no IO.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory backing this store.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Compute the content address (`"sha256:<hex>"`) of `bytes`. Pure; no IO.
    pub fn hash(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        format!("sha256:{digest:x}")
    }

    /// The on-disk path a given content hash maps to.
    fn path_for(&self, hash: &str) -> PathBuf {
        // The hash includes the `sha256:` scheme; swap the `:` for a filesystem
        // friendly `-` so it is a single valid filename.
        self.root.join(hash.replace(':', "-"))
    }

    /// Store `bytes` by content hash and return a [`BlobRef`] describing it.
    ///
    /// A repeat `put` of identical content is deduped (it does not rewrite an
    /// existing file). `media` is the caller-chosen media type carried verbatim
    /// into the ref.
    pub fn put(&self, bytes: &[u8], media: impl Into<String>) -> Result<BlobRef, TraceError> {
        let hash = Self::hash(bytes);
        let path = self.path_for(&hash);

        // Dedup: identical content addresses the same file, so only write if it
        // is not already present (the bytes are immutable for a given hash).
        if !path.exists() {
            std::fs::create_dir_all(&self.root)?;
            std::fs::write(&path, bytes)?;
        }

        Ok(BlobRef::new(hash, bytes.len() as u64, media))
    }

    /// Fetch the bytes for a content hash, or [`TraceError::BlobNotFound`] if the
    /// store has no such blob.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, TraceError> {
        let path = self.path_for(hash);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(TraceError::BlobNotFound {
                hash: hash.to_string(),
            }),
            Err(e) => Err(TraceError::Io(e)),
        }
    }

    /// Whether a blob with this content hash is present in the store.
    pub fn contains(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn put_get_roundtrips_a_large_payload() {
        let root = TempDir::new("blobstore-roundtrip");
        let store = BlobStore::new(root.path());

        let payload = vec![0xABu8; 1024 * 1024];
        let blob = store.put(&payload, "application/octet-stream").unwrap();

        assert_eq!(blob.len, payload.len() as u64);
        assert!(blob.hash.starts_with("sha256:"));
        assert_eq!(blob.media, "application/octet-stream");

        let back = store.get(&blob.hash).unwrap();
        assert_eq!(back, payload, "rehydrated bytes must equal the original");
    }

    #[test]
    fn same_content_dedups_to_same_hash() {
        let root = TempDir::new("blobstore-dedup");
        let store = BlobStore::new(root.path());

        let a = store.put(b"identical bytes", "text/plain").unwrap();
        let b = store.put(b"identical bytes", "text/plain").unwrap();
        assert_eq!(a.hash, b.hash, "same content -> same hash");

        let count = std::fs::read_dir(root.path()).unwrap().count();
        assert_eq!(count, 1, "identical content must dedup to one file");

        let c = store.put(b"other bytes", "text/plain").unwrap();
        assert_ne!(a.hash, c.hash);
    }

    #[test]
    fn hash_is_stable_and_matches_known_sha256() {
        // SHA-256("abc") is a well-known constant.
        let h = BlobStore::hash(b"abc");
        assert_eq!(
            h,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(BlobStore::hash(b"abc"), h);
    }

    #[test]
    fn get_missing_blob_is_an_error() {
        let root = TempDir::new("blobstore-missing");
        let store = BlobStore::new(root.path());
        let err = store.get("sha256:deadbeef").unwrap_err();
        assert!(matches!(err, TraceError::BlobNotFound { .. }));
        assert!(!store.contains("sha256:deadbeef"));
    }
}
