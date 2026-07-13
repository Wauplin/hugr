//! Blob exchange.
//!
//! An orchestrator hands files **in** on [`Ask::blobs`] and gets produced files
//! **out** on [`Answer::blobs`]. The mechanism is deliberately file-shaped so a
//! huglet's tools deal in plain files inside their jail, never in wire types:
//!
//! - **Inbound.** Before the turn starts, each [`BlobHandle`] on the ask is
//!   materialized into the ask's scratch working directory as a plain
//!   file, so `scratch_read`/`scratch_list` see it like any other note. All three
//!   [`BlobRef`] kinds are supported: `Bytes` (base64-decoded), `Path` (read
//!   from an orchestrator-local file), and `Sha256` (loaded from the
//!   [`BlobStore`]). Inbound files land at the scratch root under the handle's
//!   `name` hint, or a stable derived name when absent.
//!
//! - **Outbound.** By convention the agent writes files it wants to return into
//!   an `out/` subdirectory of its scratchpad (via `scratch_write`, which
//!   creates the directory on demand). After the turn, that subtree is swept
//!   into the content-addressed [`BlobStore`] and each file is returned as an
//!   [`Answer::blobs`] entry with a [`BlobRef::Sha256`] ref. Identical bytes
//!   dedupe to one stored object (the store keys by content hash), so two equal
//!   outbound files return the same `sha256`.
//!
//! The `sha256` carried on a `BlobRef::Sha256` is the store's full content
//! address string (`"sha256:<hex>"`), so it resolves directly via
//! [`BlobStore::get`] — inbound and outbound speak the same address form.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use huggr_replay::{BlobRef as StoredBlobRef, BlobStore, TraceError};

use crate::contract::{BlobHandle, BlobRef};
use crate::scratch::{ScratchEntryKind, ScratchSession};

/// Subdirectory of the ask's scratch working directory swept for outbound
/// blobs after the turn. The agent writes files here (e.g. `out/report.md`) to
/// return them; `scratch_write` creates the directory on first write.
pub(crate) const OUT_DIRNAME: &str = "out";

#[async_trait]
pub trait BlobBackend: Send + Sync {
    async fn put(&self, bytes: &[u8], media: String) -> Result<StoredBlobRef, TraceError>;

    async fn put_file(&self, path: &Path, media: String) -> Result<StoredBlobRef, TraceError> {
        let bytes = std::fs::read(path).map_err(TraceError::Io)?;
        self.put(&bytes, media).await
    }

    async fn get(&self, hash: &str) -> Result<Vec<u8>, TraceError>;

    async fn contains(&self, hash: &str) -> bool;

    async fn local_path(&self, _hash: &str) -> Option<PathBuf> {
        None
    }
}

pub type FsBlobStore = BlobStore;

#[async_trait]
impl BlobBackend for BlobStore {
    async fn put(&self, bytes: &[u8], media: String) -> Result<StoredBlobRef, TraceError> {
        BlobStore::put(self, bytes, media)
    }

    async fn put_file(&self, path: &Path, media: String) -> Result<StoredBlobRef, TraceError> {
        BlobStore::put_file(self, path, media)
    }

    async fn get(&self, hash: &str) -> Result<Vec<u8>, TraceError> {
        BlobStore::get(self, hash)
    }

    async fn contains(&self, hash: &str) -> bool {
        BlobStore::contains(self, hash)
    }

    async fn local_path(&self, hash: &str) -> Option<PathBuf> {
        self.contains(hash).then(|| self.path_of(hash))
    }
}

#[derive(Debug, Default)]
pub struct MemBlobStore {
    blobs: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
}

impl MemBlobStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl BlobBackend for MemBlobStore {
    async fn put(&self, bytes: &[u8], media: String) -> Result<StoredBlobRef, TraceError> {
        let hash = BlobStore::hash(bytes);
        self.blobs
            .lock()
            .unwrap()
            .entry(hash.clone())
            .or_insert_with(|| (bytes.to_vec(), media.clone()));
        Ok(StoredBlobRef::new(hash, bytes.len() as u64, media))
    }

    async fn get(&self, hash: &str) -> Result<Vec<u8>, TraceError> {
        self.blobs
            .lock()
            .unwrap()
            .get(hash)
            .map(|(bytes, _)| bytes.clone())
            .ok_or_else(|| TraceError::BlobNotFound {
                hash: hash.to_string(),
            })
    }

    async fn contains(&self, hash: &str) -> bool {
        self.blobs.lock().unwrap().contains_key(hash)
    }
}

/// Failures preparing inbound blobs or sweeping outbound ones. These are
/// *infrastructure* failures of an ask (a malformed hand-in, a missing store
/// object, an IO error) — surfaced as [`AskError`](crate::AskError), which
/// surfaces convert to error answers at their boundary.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// A `Bytes` blob's payload was not valid base64.
    #[error("inbound blob `{name}` has invalid base64: {source}")]
    Decode {
        name: String,
        source: base64::DecodeError,
    },

    /// A handle's `name` hint (or a derived name) was unusable — empty, or it
    /// tried to escape the scratch root with a path component.
    #[error("inbound blob has an invalid name `{name}`")]
    BadName { name: String },

    /// The content-addressed store could not read/write the blob (a `Sha256`
    /// hand-in missing from the store, or an outbound `put` failure).
    #[error("blob store error: {0}")]
    Store(#[from] TraceError),

    /// Filesystem access failed materializing or sweeping a blob.
    #[error("blob IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The scratch backend rejected a read/write/list operation.
    #[error("scratchpad error: {0}")]
    Scratch(String),
}

/// Screen blob handles arriving from a model-controlled boundary (agent-as-tool,
/// `delegate`, MCP `ask`) before they are forwarded or materialized.
///
/// `Bytes` is always accepted: inline data grants nothing the caller did not
/// already have. `Sha256` must be a well-formed content address so it cannot
/// traverse the blob store. `Path` names an orchestrator-local file; a model
/// may only use it for files under `readable_roots` (the jails the calling
/// agent's own read grants cover), so delegation never widens read access.
pub fn validate_model_blobs(
    blobs: &[BlobHandle],
    readable_roots: &[std::path::PathBuf],
) -> Result<(), String> {
    for handle in blobs {
        match &handle.blob_ref {
            BlobRef::Bytes { .. } => {}
            BlobRef::Sha256 { sha256 } => {
                if !is_content_address(sha256) {
                    return Err(format!(
                        "invalid blob content address `{sha256}` (expected `sha256:` followed by 64 hex digits)"
                    ));
                }
            }
            BlobRef::Path { path } => {
                let resolved = std::fs::canonicalize(path)
                    .map_err(|e| format!("blob path `{path}` is not readable: {e}"))?;
                let allowed = readable_roots.iter().any(|root| {
                    std::fs::canonicalize(root).is_ok_and(|root| resolved.starts_with(&root))
                });
                if !allowed {
                    return Err(format!(
                        "blob path `{path}` is outside this agent's readable roots; pass the content as a `bytes` or `sha256` blob instead"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `sha256:` followed by exactly 64 hex digits — the only shape the store emits.
fn is_content_address(address: &str) -> bool {
    address
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Materialize every inbound blob into `working` (the ask's scratch working
/// directory) before the turn starts.
pub(crate) async fn materialize_inbound(
    scratch: &ScratchSession,
    blobs: &[BlobHandle],
    store: &dyn BlobBackend,
) -> Result<(), BlobError> {
    for (index, handle) in blobs.iter().enumerate() {
        let (store_path, bytes) = match &handle.blob_ref {
            BlobRef::Sha256 { sha256 } => {
                let bytes = store.get(sha256).await?;
                (store.local_path(sha256).await, bytes)
            }
            _ => (None, load_bytes(handle, store).await?),
        };
        let name = inbound_name(handle, index, &bytes)?;
        if let Some(path) = store_path {
            scratch
                .import_file(&name, &path)
                .await
                .map_err(BlobError::Scratch)?;
        } else {
            scratch
                .write_bytes(&name, &bytes)
                .await
                .map_err(BlobError::Scratch)?;
        }
    }
    Ok(())
}

/// Sweep the `out/` subtree of `working` into the content-addressed store,
/// returning one [`BlobHandle`] per produced file (deterministic order). Missing
/// `out/` yields no blobs. Identical files dedupe by hash in the store.
pub(crate) async fn sweep_outbound(
    scratch: &ScratchSession,
    store: &dyn BlobBackend,
) -> Result<Vec<BlobHandle>, BlobError> {
    let mut files = Vec::new();
    match collect_scratch_files(scratch, OUT_DIRNAME, &mut files).await {
        Ok(()) => {}
        Err(error) if error.starts_with("path does not exist inside scratch root:") => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(BlobError::Scratch(error)),
    }
    if files.is_empty() {
        return Ok(Vec::new());
    }
    files.sort();

    let mut handles = Vec::new();
    for rel_path in files {
        let media = guess_media(&rel_path);
        let stored = if let Some(path) = scratch.local_path(&rel_path).await {
            store.put_file(&path, media.clone()).await?
        } else {
            let bytes = scratch
                .read_bytes(&rel_path)
                .await
                .map_err(BlobError::Scratch)?;
            store.put(&bytes, media.clone()).await?
        };
        let rel = rel_path
            .strip_prefix(&format!("{OUT_DIRNAME}/"))
            .unwrap_or(&rel_path)
            .to_string();
        handles.push(BlobHandle {
            blob_ref: BlobRef::Sha256 {
                sha256: stored.hash,
            },
            media_type: media,
            name: Some(rel),
        });
    }
    Ok(handles)
}

/// Read the bytes behind one inbound handle, resolving all three ref kinds.
async fn load_bytes(handle: &BlobHandle, store: &dyn BlobBackend) -> Result<Vec<u8>, BlobError> {
    match &handle.blob_ref {
        BlobRef::Bytes { base64 } => BASE64.decode(base64).map_err(|source| BlobError::Decode {
            name: handle.name.clone().unwrap_or_else(|| "<bytes>".to_string()),
            source,
        }),
        BlobRef::Path { path } => Ok(std::fs::read(path)?),
        BlobRef::Sha256 { sha256 } => Ok(store.get(sha256).await?),
    }
}

/// The single-segment file name an inbound blob lands under inside the jail:
/// the handle's sanitized `name` hint, else a stable derived name.
fn inbound_name(handle: &BlobHandle, index: usize, bytes: &[u8]) -> Result<String, BlobError> {
    if let Some(hint) = &handle.name {
        return sanitize_name(hint);
    }
    // No hint: derive a stable, jail-safe name. For a `Path` hand-in reuse the
    // source file name when it is clean; otherwise fall back to a
    // content/index-based name that never collides across a single ask.
    if let BlobRef::Path { path } = &handle.blob_ref {
        if let Some(file_name) = Path::new(path).file_name().and_then(|s| s.to_str()) {
            if let Ok(name) = sanitize_name(file_name) {
                return Ok(name);
            }
        }
    }
    let digest = match &handle.blob_ref {
        BlobRef::Sha256 { sha256 } => sha256.clone(),
        _ => BlobStore::hash(bytes),
    };
    let short = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let short = &short[..short.len().min(12)];
    Ok(format!("blob-{index}-{short}"))
}

/// Accept only a clean single path segment (a bare file name): no traversal, no
/// separators, no absolute/prefix components.
fn sanitize_name(hint: &str) -> Result<String, BlobError> {
    let bad = || BlobError::BadName {
        name: hint.to_string(),
    };
    let trimmed = hint.trim();
    if trimmed.is_empty() {
        return Err(bad());
    }
    let mut components = Path::new(trimmed).components();
    let (Some(Component::Normal(part)), None) = (components.next(), components.next()) else {
        return Err(bad());
    };
    part.to_str().map(str::to_string).ok_or_else(bad)
}

async fn collect_scratch_files(
    scratch: &ScratchSession,
    rel_dir: &str,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let mut stack = vec![rel_dir.to_string()];
    while let Some(dir) = stack.pop() {
        for entry in scratch.list(&dir).await? {
            match entry.kind {
                ScratchEntryKind::Dir => stack.push(entry.path),
                ScratchEntryKind::File => out.push(entry.path),
            }
        }
    }
    Ok(())
}

/// A best-effort media type from the file extension; unknown → octet-stream.
/// The media type is advisory metadata on the handle, never load-bearing.
fn guess_media(path: &str) -> String {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("")
        .to_ascii_lowercase();
    let media = match ext.as_str() {
        "txt" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    };
    media.to_string()
}
