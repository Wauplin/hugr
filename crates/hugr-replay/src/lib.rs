//! # hugr-replay — the durable trace format
//!
//! A **trace** is the saved form of a Hugr session: the ordered host→brain
//! [`Event`] stream made durable, plus the [`LogEntry`] log and references to
//! content-addressed blobs. Loading a trace and re-feeding its events into a
//! fresh [`Brain`](hugr_core::Brain) reconstructs the session deterministically
//! — [`replay`]/[`verify`] do exactly this, and an [`Inspector`] steps through
//! it one event at a time.
//!
//! ## Trace shape
//!
//! ```text
//! Trace
//! ├── meta:     TraceMeta       // format version, codename, created-at
//! ├── events:   Vec<Event>      // the ordered host→brain stream (the replay input)
//! ├── commands: Vec<Command>    // the ordered brain→host commands the host emitted (the replay *output*)
//! ├── log:      Vec<LogEntry>   // the consolidated, seq-stamped durable log (the truth)
//! └── blobs:    BlobManifest    // refs to content-addressed payloads (BlobStore; not inlined)
//! ```
//!
//! Three complementary views are stored deliberately:
//!
//! - **`events`** is the *input* to replay — the exact ordered stream the host
//!   fed the brain.
//! - **`commands`** is the recorded *output* — the exact ordered [`Command`]
//!   sequence the live host drained from the brain. [`verify`] asserts a replay
//!   reproduces it, catching command-order nondeterminism (e.g. a
//!   `HashMap`-ordered cancel-all) that the log alone never records. Empty for
//!   traces recorded before this field existed; [`verify`] then falls back to
//!   log-only checking.
//! - **`log`** is the *output* truth — the consolidated record stream from
//!   which `BrainState` is always rederivable, so a trace can be inspected,
//!   diffed, or re-folded by a newer core without re-running the brain.
//!
//! `BrainState` itself is **never** stored — it is always a fold over `log`.
//!
//! [`TraceMeta::format_version`] is bumped on any breaking change to the
//! container layout; [`Trace::load`] refuses an unknown future version with
//! [`TraceError::UnsupportedVersion`] rather than silently mis-parsing. The
//! container is plain JSON, so a trace recorded on one host replays anywhere.

use std::path::Path;

use hugr_core::{Command, Event, LogEntry};
use serde::{Deserialize, Serialize};

mod blob;
mod replay;
#[doc(hidden)]
pub mod test_support;
pub use blob::BlobStore;
pub use replay::{
    Inspector, Replay, Step, drive, policy_from_trace, replay, replay_with_policy, verify,
    verify_with_policy,
};

/// The current trace container format version. Bump on any breaking change to
/// the [`Trace`] layout; older readers reject newer versions (see
/// [`TraceError::UnsupportedVersion`]).
pub const FORMAT_VERSION: u32 = 1;

/// The codename written into every trace, so a file is self-identifying.
pub const CODENAME: &str = "hugr-trace";

/// A saved Hugr session: a versioned container over the ordered event stream,
/// the durable log, and blob references.
///
/// `BrainState` is intentionally absent — it is always rederivable by folding
/// `log`, so storing it would be redundant and a desync risk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Trace {
    /// Format version, codename, creation time. Always present and checked first.
    pub meta: TraceMeta,
    /// The ordered host→brain [`Event`] stream — the *input* to replay.
    pub events: Vec<Event>,
    /// The ordered brain→host [`Command`] sequence the live host drained — the
    /// recorded *output* [`verify`](crate::verify) checks a replay against. An
    /// empty vec means "not recorded" (older traces); verify then falls back to
    /// log-only comparison. Skipped from serialized JSON when empty so such
    /// traces stay byte-identical to the pre-`commands` format.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Command>,
    /// The consolidated, seq-stamped durable log — the *truth*.
    pub log: Vec<LogEntry>,
    /// References to content-addressed payloads (the bytes live in the
    /// [`BlobStore`], not inlined here).
    pub blobs: BlobManifest,
    /// The session's [`TurnPolicy`](hugr_core::TurnPolicy) configuration, as an
    /// opaque JSON value this crate stores and forwards but never interprets.
    /// The brain branches on the policy's pure decisions, so a faithful replay
    /// needs the same policy the trace was recorded under. `None` for traces
    /// recorded without a captured policy (replay then falls back to the
    /// default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<serde_json::Value>,
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
    /// `seq 0` tick). `None` for an empty trace.
    pub created_at: Option<u64>,
    /// Store-assigned identifier of this trace. Set by a `TraceStore` when the
    /// trace is persisted; `None` for traces recorded outside a store. Skipped
    /// when absent so such traces stay byte-identical to the pre-store format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// The parent trace this trace resumed from; `None` for a root trace.
    /// Lineage is a DAG recorded entirely in headers; two children with the
    /// same `depends_on` are a fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<String>,
    /// The name of the agent that recorded this trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// The version of the agent that recorded this trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    /// The question this ask answered, so lineage listings are human-readable
    /// without folding events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The ask's outcome status (the `Answer.status` wire string, e.g.
    /// `"success"` / `"off_topic"` / `"error"`) — opaque to this crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl TraceMeta {
    /// Metadata for a freshly created trace at the current [`FORMAT_VERSION`].
    pub fn new(created_at: Option<u64>) -> Self {
        Self {
            codename: CODENAME.to_string(),
            format_version: FORMAT_VERSION,
            created_at,
            trace_id: None,
            depends_on: None,
            agent_name: None,
            agent_version: None,
            question: None,
            status: None,
        }
    }
}

impl Default for TraceMeta {
    fn default() -> Self {
        Self::new(None)
    }
}

/// A reference to a content-addressed payload. Large tool outputs / inputs are
/// stored by hash; the log carries the reference, the bytes live in the
/// [`BlobStore`]. The trace ships with or without those bytes.
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

/// The set of blobs a trace references. Populated by the host as it offloads
/// large payloads to the content-addressed [`BlobStore`]; the bytes are never
/// inlined here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BlobManifest {
    /// Blob references, keyed in insertion order. The bytes are *not* inlined.
    pub refs: Vec<BlobRef>,
}

impl BlobManifest {
    /// An empty manifest.
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
            commands: Vec::new(),
            log,
            blobs: BlobManifest::new(),
            policy: None,
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
            commands: Vec::new(),
            log,
            blobs,
            policy: None,
        }
    }

    /// Attach the recorded brain→host [`Command`] sequence (in emission order)
    /// so [`verify`](crate::verify) can assert replay reproduces it. The host's
    /// recorder captures these as it drains `brain.poll()`.
    pub fn with_commands(mut self, commands: Vec<Command>) -> Self {
        self.commands = commands;
        self
    }

    /// Attach the session's policy configuration (an opaque JSON value the host
    /// produced by serializing its [`TurnPolicy`](hugr_core::TurnPolicy)), so
    /// replay can reproduce the brain's policy-driven decisions.
    pub fn with_policy(mut self, policy: serde_json::Value) -> Self {
        self.policy = Some(policy);
        self
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

    /// Write the trace to disk as JSON.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), TraceError> {
        std::fs::write(path, self.to_json()?)?;
        Ok(())
    }

    /// Atomically write the trace to disk as JSON by writing a sibling temp
    /// file and renaming it into place. Native hosts use this for crash-resume
    /// checkpoints so a process kill cannot leave a half-written trace at the
    /// target path.
    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<(), TraceError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("trace.json");
        let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

        std::fs::write(&tmp, self.to_json()?)?;
        if let Err(err) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err.into());
        }
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

    /// A blob referenced by hash is not present in the [`BlobStore`].
    #[error("blob not found in store: {hash}")]
    BlobNotFound { hash: String },

    /// Replaying the trace's events through a fresh brain produced a log that
    /// differs from the recorded log — the fold is no longer deterministic for
    /// this trace (the regression [`verify`] exists to catch).
    #[error(
        "replay mismatch: recorded log has {recorded} entries, reconstruction has {reconstructed}"
    )]
    ReplayMismatch {
        recorded: usize,
        reconstructed: usize,
    },

    /// Replaying the trace's events through a fresh brain produced a **command
    /// sequence** that diverges from the recorded one. Unlike
    /// [`ReplayMismatch`](Self::ReplayMismatch), this catches nondeterminism
    /// that never reaches the log (command *ordering*, e.g. a `HashMap`-ordered
    /// cancel-all). `index` is the position of the first divergent command.
    #[error(
        "replay command mismatch at index {index}: recorded {recorded} commands, reconstruction produced {reconstructed}"
    )]
    CommandMismatch {
        index: usize,
        recorded: usize,
        reconstructed: usize,
    },
}
