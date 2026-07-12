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

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    fn path_for(&self, hash: &str) -> Result<PathBuf, TraceError> {
        validate_hash(hash)?;
        let file_name = hash.replace(':', "-");
        Ok(self.root.join(&file_name[..9]).join(file_name))
    }

    /// Store `bytes` by content hash and return a [`BlobRef`] describing it.
    ///
    /// A repeat `put` of identical content is deduped (it does not rewrite an
    /// existing file). `media` is the caller-chosen media type carried verbatim
    /// into the ref.
    pub fn put(&self, bytes: &[u8], media: impl Into<String>) -> Result<BlobRef, TraceError> {
        let hash = Self::hash(bytes);
        let path = self.path_for(&hash)?;
        install_bytes(&path, bytes)?;

        Ok(BlobRef::new(hash, bytes.len() as u64, media))
    }

    /// Store an existing file by content hash without sharing the caller's
    /// mutable inode with the store object.
    pub fn put_file(
        &self,
        source: impl AsRef<Path>,
        media: impl Into<String>,
    ) -> Result<BlobRef, TraceError> {
        let source = source.as_ref();
        let (hash, len) = hash_file(source)?;
        let path = self.path_for(&hash)?;
        let bytes = std::fs::read(source)?;
        install_bytes(&path, &bytes)?;
        Ok(BlobRef::new(hash, len, media))
    }

    /// Fetch the bytes for a content hash, or [`TraceError::BlobNotFound`] if the
    /// store has no such blob.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>, TraceError> {
        let path = self.path_for(hash)?;
        match std::fs::read(&path) {
            Ok(bytes) if Self::hash(&bytes) == hash => Ok(bytes),
            Ok(_) => Err(TraceError::InvalidBlobHash {
                hash: hash.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(TraceError::BlobNotFound {
                hash: hash.to_string(),
            }),
            Err(e) => Err(TraceError::Io(e)),
        }
    }

    /// Whether a blob with this content hash is present in the store.
    pub fn contains(&self, hash: &str) -> bool {
        self.path_for(hash).is_ok_and(|path| path.exists())
    }

    pub fn path_of(&self, hash: &str) -> PathBuf {
        self.path_for(hash)
            .unwrap_or_else(|_| self.root.join(".invalid-content-address"))
    }
}

fn validate_hash(hash: &str) -> Result<(), TraceError> {
    let Some(hex) = hash.strip_prefix("sha256:") else {
        return Err(TraceError::InvalidBlobHash {
            hash: hash.to_string(),
        });
    };
    if hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(TraceError::InvalidBlobHash {
            hash: hash.to_string(),
        })
    }
}

fn install_bytes(path: &Path, bytes: &[u8]) -> Result<(), TraceError> {
    if path.exists() {
        let existing = std::fs::read(path)?;
        if existing == bytes {
            return Ok(());
        }
        return Err(TraceError::InvalidBlobHash {
            hash: BlobStore::hash(bytes),
        });
    }
    let parent = path.parent().expect("blob path has a shard directory");
    std::fs::create_dir_all(parent)?;
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let temp = parent.join(format!(
        ".blob-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    set_readonly(&temp)?;
    match std::fs::hard_link(&temp, path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read(path)? != bytes {
                let _ = std::fs::remove_file(&temp);
                return Err(TraceError::InvalidBlobHash {
                    hash: BlobStore::hash(bytes),
                });
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
    }
    std::fs::remove_file(temp)?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<(String, u64), TraceError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((format!("sha256:{:x}", hasher.finalize()), len))
}

fn set_readonly(path: &Path) -> Result<(), TraceError> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms)?;
    Ok(())
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
        let hash = format!("sha256:{}", "0".repeat(64));
        let err = store.get(&hash).unwrap_err();
        assert!(matches!(err, TraceError::BlobNotFound { .. }));
        assert!(!store.contains(&hash));
    }

    #[test]
    fn invalid_hashes_never_map_outside_the_store() {
        let root = TempDir::new("blobstore-invalid-key");
        let store = BlobStore::new(root.path());
        assert!(matches!(
            store.get("sha256:../../outside"),
            Err(TraceError::InvalidBlobHash { .. })
        ));
        assert!(!store.contains("../../outside"));
        assert!(store.path_of("../../outside").starts_with(root.path()));
    }
}
