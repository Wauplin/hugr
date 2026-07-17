//! Signal-aware PyO3 bridge shared by Huggr's Python surfaces.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use huggr_agent::{Agent, AgentEvent, Answer, Ask};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

fn runtime_err(message: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(message.to_string())
}

/// A Tokio runtime whose final release never waits for blocking work while Python holds the GIL.
pub struct BridgeRuntime(Option<tokio::runtime::Runtime>);

impl BridgeRuntime {
    pub fn new(runtime: tokio::runtime::Runtime) -> Self {
        Self(Some(runtime))
    }
}

impl Deref for BridgeRuntime {
    type Target = tokio::runtime::Runtime;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("bridge runtime is available")
    }
}

impl Drop for BridgeRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            // PyO3 drops #[pyclass] values with the GIL held. A cancelled Python
            // capability may still occupy spawn_blocking and need that GIL to
            // return, so Runtime::drop would deadlock waiting for it here.
            runtime.shutdown_background();
        }
    }
}

/// A blocking pull of one ask's events with prompt Python signal handling.
#[pyclass]
pub struct EventStream {
    rx: Mutex<UnboundedReceiver<AgentEvent>>,
    handle: Mutex<Option<JoinHandle<Result<Answer, String>>>>,
    runtime: Arc<BridgeRuntime>,
}

impl EventStream {
    pub fn new(
        rx: UnboundedReceiver<AgentEvent>,
        handle: JoinHandle<Result<Answer, String>>,
        runtime: Arc<BridgeRuntime>,
    ) -> Self {
        Self {
            rx: Mutex::new(rx),
            handle: Mutex::new(Some(handle)),
            runtime,
        }
    }

    fn abort(&self) {
        if let Some(handle) = self.handle.lock().unwrap().as_ref() {
            handle.abort();
        }
    }

    fn abort_and_join(&self, py: Python<'_>) {
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = py.allow_threads(|| self.runtime.block_on(handle));
        }
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.abort();
    }
}

#[pymethods]
impl EventStream {
    /// Return the next event as JSON, checking Python signals between short waits.
    fn next_event(&self, py: Python<'_>) -> PyResult<Option<String>> {
        loop {
            let event = py.allow_threads(|| {
                let mut rx = self.rx.lock().unwrap();
                self.runtime.block_on(async {
                    tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
                })
            });
            match event {
                Ok(Some(event)) => {
                    return serde_json::to_string(&event).map(Some).map_err(runtime_err);
                }
                Ok(None) => {
                    let handle = self.handle.lock().unwrap().take();
                    if let Some(handle) = handle {
                        py.allow_threads(|| self.runtime.block_on(handle))
                            .map_err(runtime_err)?
                            .map_err(runtime_err)?;
                    }
                    return Ok(None);
                }
                Err(_) => {
                    if let Err(err) = py.check_signals() {
                        self.abort();
                        return Err(err);
                    }
                }
            }
        }
    }

    /// Abort the in-flight ask. Safe to call after completion.
    fn cancel(&self, py: Python<'_>) {
        self.abort_and_join(py);
    }
}

struct AbortOnDrop<T>(Option<JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    async fn join(mut self) -> Result<T, tokio::task::JoinError> {
        self.0.take().expect("join handle is present").await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.0.as_ref() {
            handle.abort();
        }
    }
}

/// Drive an agent stream into a caller-owned channel and abort it when the forwarding task is cancelled.
pub async fn forward_agent_events(
    agent: Agent,
    ask: Ask,
    tx: UnboundedSender<AgentEvent>,
) -> Result<Answer, String> {
    let (mut events, handle) = agent.ask_events(ask);
    let handle = AbortOnDrop(Some(handle));
    while let Some(event) = events.recv().await {
        if tx.send(event).is_err() {
            return Err("event stream receiver closed".to_string());
        }
    }
    handle
        .join()
        .await
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
}

/// Build the terminal event used when bundle setup returns an error answer.
pub fn answer_ready_event(answer: Answer) -> AgentEvent {
    AgentEvent::AnswerReady {
        answer: Box::new(answer),
    }
}
