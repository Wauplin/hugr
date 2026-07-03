//! Blob exchange with permissions (ARCHITECTURE §18.3, ROADMAP T0.5).
//!
//! An orchestrator hands files **in** on [`Ask::blobs`] and gets produced files
//! **out** on [`Answer::blobs`]. The mechanism is deliberately file-shaped so a
//! subagent's tools deal in plain files inside their jail, never in wire types:
//!
//! - **Inbound.** Before the turn starts, each [`BlobHandle`] on the ask is
//!   materialized into the ask's scratch working directory (§19.3) as a plain
//!   file with the declared [`BlobPerms`] applied as unix mode bits, so
//!   `scratch_read`/`scratch_list` see it like any other note. All three
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
//!
//! Enforcement is v1 per the roadmap: materialize-with-mode-bits inside the
//! jail. The mode bits are honest owner permissions on unix (a `read`-only
//! blob is written `0o400`); on non-unix targets the perms are advisory and the
//! file is written with the platform default. Anything stronger (bind mounts,
//! seccomp) is a host upgrade behind the same [`BlobHandle`] type (§18.3).

use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hugr_replay::{BlobStore, TraceError};

use crate::contract::{BlobHandle, BlobPerms, BlobRef};

/// Subdirectory of the ask's scratch working directory swept for outbound
/// blobs after the turn. The agent writes files here (e.g. `out/report.md`) to
/// return them; `scratch_write` creates the directory on first write.
pub(crate) const OUT_DIRNAME: &str = "out";

/// Failures preparing inbound blobs or sweeping outbound ones. These are
/// *infrastructure* failures of an ask (a malformed hand-in, a missing store
/// object, an IO error) — surfaced as [`AskError`](crate::AskError), which
/// surfaces convert to error answers at their boundary (§18.1).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
}

/// Materialize every inbound blob into `working` (the ask's scratch working
/// directory) with its declared perms, before the turn starts (§18.3).
pub(crate) fn materialize_inbound(
    working: &Path,
    blobs: &[BlobHandle],
    store: &BlobStore,
) -> Result<(), BlobError> {
    for (index, handle) in blobs.iter().enumerate() {
        let bytes = load_bytes(handle, store)?;
        let name = inbound_name(handle, index, &bytes)?;
        let dest = working.join(&name);
        // A fresh file each ask: an inbound blob rides the Ask, never the
        // scratch lineage, so an existing (possibly read-only) file at this
        // path from a copy-on-fork seed must be replaced, not appended to.
        if dest.exists() {
            std::fs::remove_file(&dest)?;
        }
        std::fs::write(&dest, &bytes)?;
        apply_perms(&dest, handle.perms)?;
    }
    Ok(())
}

/// Sweep the `out/` subtree of `working` into the content-addressed store,
/// returning one [`BlobHandle`] per produced file (deterministic order). Missing
/// `out/` yields no blobs. Identical files dedupe by hash in the store.
pub(crate) fn sweep_outbound(
    working: &Path,
    store: &BlobStore,
) -> Result<Vec<BlobHandle>, BlobError> {
    let out_root = working.join(OUT_DIRNAME);
    if !out_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_files(&out_root, &mut files)?;
    // Deterministic order regardless of directory-entry order.
    files.sort();

    let mut handles = Vec::new();
    for path in files {
        let bytes = std::fs::read(&path)?;
        let media = guess_media(&path);
        // Content-addressed put — dedup by hash lives in the store (§3.3).
        let stored = store.put(&bytes, media.clone())?;
        let rel = rel_name(&out_root, &path);
        handles.push(
            BlobHandle::new(
                BlobRef::Sha256 {
                    sha256: stored.hash,
                },
                media,
            )
            .with_name(rel),
        );
    }
    Ok(handles)
}

/// Read the bytes behind one inbound handle, resolving all three ref kinds.
fn load_bytes(handle: &BlobHandle, store: &BlobStore) -> Result<Vec<u8>, BlobError> {
    match &handle.blob_ref {
        BlobRef::Bytes { base64 } => BASE64.decode(base64).map_err(|source| BlobError::Decode {
            name: handle.name.clone().unwrap_or_else(|| "<bytes>".to_string()),
            source,
        }),
        BlobRef::Path { path } => Ok(std::fs::read(path)?),
        BlobRef::Sha256 { sha256 } => Ok(store.get(sha256)?),
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
    let digest = BlobStore::hash(bytes);
    let short = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let short = &short[..short.len().min(12)];
    Ok(format!("blob-{index}-{short}"))
}

/// Accept only a clean single path segment (a bare file name): no traversal, no
/// separators, no absolute/prefix components — the jail discipline of §19.3.
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

/// Apply [`BlobPerms`] as owner mode bits on unix; a no-op elsewhere (advisory,
/// documented — enforcement v1 is materialize-with-mode-bits, §18.3).
#[cfg(unix)]
fn apply_perms(path: &Path, perms: BlobPerms) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut mode = 0o000;
    if perms.read {
        mode |= 0o400;
    }
    if perms.write {
        mode |= 0o200;
    }
    if perms.execute {
        mode |= 0o100;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn apply_perms(_path: &Path, _perms: BlobPerms) -> std::io::Result<()> {
    Ok(())
}

/// Recursively collect regular files under `dir` into `out`.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
        // Symlinks and exotic entries are skipped — the jail deals in plain
        // files/dirs only (mirrors the scratch copy_tree discipline).
    }
    Ok(())
}

/// The `/`-joined path of `path` relative to the `out/` root — the name hint
/// returned on the outbound handle so an orchestrator can reconstruct layout.
fn rel_name(out_root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(out_root).unwrap_or(path);
    rel.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A best-effort media type from the file extension; unknown → octet-stream.
/// The media type is advisory metadata on the handle, never load-bearing.
fn guess_media(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
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
