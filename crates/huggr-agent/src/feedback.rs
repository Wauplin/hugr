use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::TraceId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Feedback {
    pub trace_id: TraceId,
    pub payload: Value,
    pub created_at_ms: u64,
}

impl Feedback {
    pub fn new(trace_id: TraceId, payload: Value) -> Self {
        Self {
            trace_id,
            payload,
            created_at_ms: now_ms(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    #[error("unknown trace id: {0}")]
    UnknownTrace(TraceId),
    #[error("feedback store error: {0}")]
    Store(String),
    #[error("trace store error: {0}")]
    Trace(#[from] crate::StoreError),
}

#[async_trait]
pub trait FeedbackBackend: Send + Sync {
    async fn append(&self, feedback: Feedback) -> Result<(), FeedbackError>;
    async fn list(&self, trace_id: &TraceId) -> Result<Vec<Feedback>, FeedbackError>;
}

#[derive(Clone, Debug)]
pub struct FsFeedbackStore {
    root: PathBuf,
}

impl FsFeedbackStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, trace_id: &TraceId) -> PathBuf {
        self.root.join(format!("{}.jsonl", trace_id.as_str()))
    }
}

#[async_trait]
impl FeedbackBackend for FsFeedbackStore {
    async fn append(&self, feedback: Feedback) -> Result<(), FeedbackError> {
        fs::create_dir_all(&self.root).map_err(|err| FeedbackError::Store(err.to_string()))?;
        let path = self.path(&feedback.trace_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|err| FeedbackError::Store(err.to_string()))?;
        let line = serde_json::to_string(&feedback)
            .map_err(|err| FeedbackError::Store(err.to_string()))?;
        writeln!(file, "{line}").map_err(|err| FeedbackError::Store(err.to_string()))
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<Feedback>, FeedbackError> {
        let path = self.path(trace_id);
        // A missing sidecar is "no feedback yet"; any other read error
        // (permissions, corruption) is surfaced rather than hidden as empty.
        let src = match fs::read_to_string(&path) {
            Ok(src) => src,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(FeedbackError::Store(err.to_string())),
        };
        src.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line).map_err(|err| FeedbackError::Store(err.to_string()))
            })
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct MemFeedbackStore {
    entries: Mutex<BTreeMap<String, Vec<Feedback>>>,
}

impl MemFeedbackStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FeedbackBackend for MemFeedbackStore {
    async fn append(&self, feedback: Feedback) -> Result<(), FeedbackError> {
        self.entries
            .lock()
            .unwrap()
            .entry(feedback.trace_id.as_str().to_string())
            .or_default()
            .push(feedback);
        Ok(())
    }

    async fn list(&self, trace_id: &TraceId) -> Result<Vec<Feedback>, FeedbackError> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .get(trace_id.as_str())
            .cloned()
            .unwrap_or_default())
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
