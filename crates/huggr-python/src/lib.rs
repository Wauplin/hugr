//! `huggr_agents._native` — the PyO3 embedding of the huglet runtime.
//!
//! The boundary is JSON strings both ways: the pure-Python layer
//! (`bindings/python`) owns the typed surface, this module owns the runtime.
//! Assembly reuses `huggr_toolkit::runtime::build_agent_with_options`, so a
//! Python-defined agent behaves exactly like a manifest-defined one.

mod capability;
mod config;

use std::sync::Arc;

use huggr_agent::{Agent, Ask, StatsOptions, TraceId};
use huggr_toolkit::python_bridge::{BridgeRuntime, EventStream, forward_agent_events};
use huggr_toolkit::runtime::build_agent_with_options;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;

use capability::PyCapability;

fn value_err(message: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(message.to_string())
}

fn runtime_err(message: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(message.to_string())
}

fn parse(json: &str, what: &str) -> PyResult<serde_json::Value> {
    serde_json::from_str(json).map_err(|err| value_err(format!("invalid {what} JSON: {err}")))
}

fn dump<T: serde::Serialize>(value: &T) -> PyResult<String> {
    serde_json::to_string(value).map_err(runtime_err)
}

/// One Python tool registration: (name, description, schema JSON,
/// requires_permission, background, callable).
type ToolSpec = (String, String, String, bool, bool, Py<PyAny>);

#[pyclass]
struct NativeAgent {
    agent: Agent,
    warnings: Vec<String>,
    runtime: Arc<BridgeRuntime>,
}

#[pymethods]
impl NativeAgent {
    #[new]
    fn new(config_json: &str, tools: Vec<ToolSpec>) -> PyResult<Self> {
        let cfg = parse(config_json, "agent config")?;
        let def = config::definition_from_config(&cfg).map_err(value_err)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(runtime_err)?;
        let (mut agent, warnings) = runtime
            .block_on(build_agent_with_options(&def, &Default::default()))
            .map_err(runtime_err)?;
        for (name, description, schema_json, requires_permission, background, callable) in tools {
            let schema = parse(&schema_json, "tool schema")?;
            agent.capabilities.push(Arc::new(PyCapability {
                name,
                description,
                schema,
                requires_permission,
                background,
                callable,
            }));
        }
        Ok(Self {
            agent,
            warnings,
            runtime: Arc::new(BridgeRuntime::new(runtime)),
        })
    }

    fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }

    fn describe(&self) -> PyResult<String> {
        dump(&self.agent.describe())
    }

    fn ask_events(&self, ask_json: &str) -> PyResult<EventStream> {
        let ask: Ask = serde_json::from_str(ask_json)
            .map_err(|err| value_err(format!("invalid ask: {err}")))?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = {
            let _guard = self.runtime.enter();
            tokio::spawn(forward_agent_events(self.agent.clone(), ask, tx))
        };
        Ok(EventStream::new(rx, handle, self.runtime.clone()))
    }

    fn feedback(&self, py: Python<'_>, trace_id: &str, payload_json: &str) -> PyResult<String> {
        let payload = parse(payload_json, "feedback payload")?;
        let trace_id = TraceId::try_new(trace_id).map_err(value_err)?;
        let feedback = py
            .allow_threads(|| {
                self.runtime
                    .block_on(self.agent.feedback(trace_id, payload))
            })
            .map_err(runtime_err)?;
        dump(&feedback)
    }

    fn feedback_for(&self, py: Python<'_>, trace_id: &str) -> PyResult<String> {
        let trace_id = TraceId::try_new(trace_id).map_err(value_err)?;
        let feedback = py
            .allow_threads(|| self.runtime.block_on(self.agent.feedback_for(&trace_id)))
            .map_err(runtime_err)?;
        dump(&feedback)
    }

    fn traces(&self, py: Python<'_>) -> PyResult<String> {
        let heads = py
            .allow_threads(|| self.runtime.block_on(self.agent.traces()))
            .map_err(runtime_err)?;
        dump(&heads)
    }

    fn stats(&self, py: Python<'_>, options_json: &str) -> PyResult<String> {
        let options: StatsOptions = serde_json::from_str(options_json)
            .map_err(|err| value_err(format!("invalid stats options: {err}")))?;
        let stats = py
            .allow_threads(|| self.runtime.block_on(self.agent.stats(options)))
            .map_err(runtime_err)?;
        dump(&stats)
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<NativeAgent>()?;
    m.add_class::<EventStream>()?;
    Ok(())
}
