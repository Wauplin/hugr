//! # baton-replay — the durable trace format
//!
//! A **trace** is the saved form of a Baton session (ARCHITECTURE §12). Because
//! the brain is a pure fold over an ordered event stream, a trace is just *that
//! stream made durable* — there is no separate "save format" to invent.
//!
//! This crate owns the on-disk container: a versioned, portable struct holding
//! the ordered host→brain [`Event`] stream, the durable [`LogEntry`] log, and a
//! place to reference content-addressed blobs by hash. Loading a trace and
//! re-feeding its events into a fresh [`Brain`](baton_core::Brain) reconstructs
//! the session deterministically (replay/resume, built in P3-3/P3-4).
//!
//! ## Why this crate exists (and where IO lives)
//!
//! `baton-core` is **sans-IO and pure** — it must never touch the filesystem.
//! Persistence is therefore a *host-side* concern. `baton-replay` is that host
//! piece: it depends on `baton-core` only as pure data (it serializes its
//! `serde`-derived types) and is the *only* place in the trace story allowed to
//! use `std::fs`. Adding this crate does not pull `baton-core` away from
//! sans-IO; `cargo tree -p baton-core` stays free of any environmental deps.
//!
//! ## Trace shape
//!
//! ```text
//! Trace
//! ├── meta: TraceMeta        // format version, codename, created-at
//! ├── events: Vec<Event>     // the ordered host→brain stream (the replay input)
//! ├── log:    Vec<LogEntry>  // the consolidated, seq-stamped durable log (the truth)
//! └── blobs:  BlobManifest   // refs to content-addressed payloads (P3-2; not inlined)
//! ```
//!
//! Two complementary views are stored deliberately:
//!
//! - **`events`** is the *input* to replay — the exact ordered stream the host
//!   fed the brain (including the raw transport deltas, if the recorder kept
//!   them). Re-feeding it into a fresh brain yields identical commands (§6.3).
//! - **`log`** is the *output* truth — the consolidated record stream
//!   ([one record per logical message/tool-result](baton_core::Record), §4.5),
//!   from which `BrainState` is always rederivable. A trace can be inspected,
//!   diffed, or re-folded by a newer core without re-running the brain.
//!
//! `BrainState` itself is **never** stored — it is always a fold over `log`
//! (ARCHITECTURE §12.1). That keeps traces small, forward-compatible, and
//! impossible to desync from reality.
//!
//! ## Versioning & portability
//!
//! [`TraceMeta::format_version`] is a single integer bumped on any
//! breaking change to the container layout. [`Trace::load`] checks it and
//! refuses an unknown future version with [`TraceError::UnsupportedVersion`]
//! rather than silently mis-parsing. The container is plain JSON, so a trace
//! recorded on a server replays in a browser or a Python host — neither the
//! brain nor the trace depends on the environment (§12.3).

use std::path::Path;

use baton_core::{Event, LogEntry};
use serde::{Deserialize, Serialize};

/// The current trace container format version. Bump on any breaking change to
/// the [`Trace`] layout; older readers reject newer versions (see
/// [`TraceError::UnsupportedVersion`]).
pub const FORMAT_VERSION: u32 = 1;

/// The codename written into every trace, so a file is self-identifying.
pub const CODENAME: &str = "baton-trace";

/// A saved Baton session: a versioned container over the ordered event stream,
/// the durable log, and blob references (ARCHITECTURE §12.1).
///
/// `BrainState` is intentionally absent — it is always rederivable by folding
/// `log`, so storing it would be redundant and a desync risk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Trace {
    /// Format version, codename, creation time. Always present and checked first.
    pub meta: TraceMeta,
    /// The ordered host→brain [`Event`] stream — the *input* to replay (§6.3).
    pub events: Vec<Event>,
    /// The consolidated, seq-stamped durable log — the *truth* (§4.5/§12.1).
    pub log: Vec<LogEntry>,
    /// References to content-addressed payloads (the bytes live elsewhere). The
    /// concrete blob store lands in P3-2; the manifest structure is here so the
    /// format is stable for it.
    pub blobs: BlobManifest,
}

/// Trace container metadata. Versioned for forward-compat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TraceMeta {
    /// Self-identifying codename (always [`CODENAME`]).
    pub codename: String,
    /// Container layout version (see [`FORMAT_VERSION`]).
    pub format_version: u32,
    /// When the session was created, as a host-defined logical timestamp (the
    /// `seq 0` tick — never a syscall in the core). `None` for an empty trace.
    pub created_at: Option<u64>,
}

impl TraceMeta {
    /// Metadata for a freshly created trace at the current [`FORMAT_VERSION`].
    pub fn new(created_at: Option<u64>) -> Self {
        Self {
            codename: CODENAME.to_string(),
            format_version: FORMAT_VERSION,
            created_at,
        }
    }
}

impl Default for TraceMeta {
    fn default() -> Self {
        Self::new(None)
    }
}

/// A reference to a content-addressed payload (ARCHITECTURE §3.3). Large tool
/// outputs / inputs are stored by hash; the log carries the reference, the bytes
/// live in a blob store (P3-2). The trace ships with or without those bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BlobRef {
    /// Content hash (host-chosen algorithm; opaque to this crate).
    pub hash: String,
    /// Payload length in bytes.
    pub len: u64,
    /// Media type (e.g. `"text/plain"`, `"application/json"`).
    pub media: String,
}

impl BlobRef {
    /// A blob reference. `hash` is the content address; `len`/`media` describe it.
    pub fn new(hash: impl Into<String>, len: u64, media: impl Into<String>) -> Self {
        Self {
            hash: hash.into(),
            len,
            media: media.into(),
        }
    }
}

/// The set of blobs a trace references. Empty until P3-2 wires the blob store;
/// present now so the container layout is stable for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BlobManifest {
    /// Blob references, keyed in insertion order. The bytes are *not* inlined.
    pub refs: Vec<BlobRef>,
}

impl BlobManifest {
    /// An empty manifest (the common case until P3-2).
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a blob reference.
    pub fn push(&mut self, blob: BlobRef) {
        self.refs.push(blob);
    }
}

impl Trace {
    /// A trace from an ordered event stream and a durable log, with no blobs.
    ///
    /// `created_at` is the session's `seq 0` tick (a host-defined logical
    /// timestamp), or `None` for an empty session.
    pub fn new(events: Vec<Event>, log: Vec<LogEntry>, created_at: Option<u64>) -> Self {
        Self {
            meta: TraceMeta::new(created_at),
            events,
            log,
            blobs: BlobManifest::new(),
        }
    }

    /// A trace with an explicit blob manifest (for hosts that already offloaded
    /// large payloads to a content-addressed store).
    pub fn with_blobs(
        events: Vec<Event>,
        log: Vec<LogEntry>,
        created_at: Option<u64>,
        blobs: BlobManifest,
    ) -> Self {
        Self {
            meta: TraceMeta::new(created_at),
            events,
            log,
            blobs,
        }
    }

    /// Serialize the trace to pretty JSON bytes. Pure; no IO.
    pub fn to_json(&self) -> Result<Vec<u8>, TraceError> {
        Ok(serde_json::to_vec_pretty(self)?)
    }

    /// Parse a trace from JSON bytes, rejecting an unsupported future
    /// [`format_version`](TraceMeta::format_version). Pure; no IO.
    pub fn from_json(bytes: &[u8]) -> Result<Self, TraceError> {
        let trace: Trace = serde_json::from_slice(bytes)?;
        if trace.meta.format_version > FORMAT_VERSION {
            return Err(TraceError::UnsupportedVersion {
                found: trace.meta.format_version,
                supported: FORMAT_VERSION,
            });
        }
        Ok(trace)
    }

    /// Write the trace to disk as JSON. **This is the IO boundary** — the only
    /// filesystem access in the trace story, kept out of `baton-core`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), TraceError> {
        std::fs::write(path, self.to_json()?)?;
        Ok(())
    }

    /// Read a trace from disk and parse it (version-checked).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, TraceError> {
        let bytes = std::fs::read(path)?;
        Self::from_json(&bytes)
    }
}

/// Errors from reading, writing, or parsing a [`Trace`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TraceError {
    /// Filesystem read/write failed.
    #[error("trace IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON (de)serialization failed.
    #[error("trace (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The trace was written by a newer, incompatible format version.
    #[error("unsupported trace format version {found} (this build supports up to {supported})")]
    UnsupportedVersion { found: u32, supported: u32 },
}
