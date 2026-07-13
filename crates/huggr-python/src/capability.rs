//! A Python callable exposed as an ordinary host [`Capability`].
//!
//! Sandbox-by-registration holds at the model/tool boundary; the callable
//! itself is trusted host code (it may do arbitrary IO once invoked). Python
//! exceptions become semantic tool errors (`Err(Value)`), never panics.

use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use pyo3::prelude::*;
use pyo3::types::PyModule;
use serde_json::json;

pub struct PyCapability {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub requires_permission: bool,
    pub background: bool,
    pub callable: Py<PyAny>,
}

#[async_trait]
impl Capability for PyCapability {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(&self.name, &self.description, self.schema.clone())
    }

    fn requires_permission(&self) -> bool {
        self.requires_permission
    }

    fn runs_in_background(&self) -> bool {
        self.background
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let callable = Python::with_gil(|py| self.callable.clone_ref(py));
        let name = self.name.clone();
        let args_json = args.to_string();
        // Python work runs on a blocking thread: the GIL must never stall the
        // host's async driver loop.
        let result = tokio::task::spawn_blocking(move || call_python(&callable, &args_json))
            .await
            .map_err(|err| json!({ "error": format!("tool `{name}` panicked: {err}") }))?;
        match result {
            Ok(out) => serde_json::from_str(&out)
                .map_err(|err| json!({ "error": format!("tool returned non-JSON value: {err}") })),
            Err(message) => Err(json!({ "error": message })),
        }
    }
}

/// Call the Python function with the decoded args; a returned coroutine is
/// driven to completion with `asyncio.run` on this blocking thread — a *fresh*
/// event loop, not the caller's. A coroutine bound to another running loop (a
/// loop-scoped client, task, lock, or `contextvars` state created elsewhere)
/// will fail; supported async tools must be self-contained (`await` their own
/// I/O, create their own clients). Simple `async def` tools work.
fn call_python(callable: &Py<PyAny>, args_json: &str) -> Result<String, String> {
    Python::with_gil(|py| {
        let run = || -> PyResult<String> {
            let json_mod = PyModule::import(py, "json")?;
            let args = json_mod.call_method1("loads", (args_json,))?;
            let mut result = callable.bind(py).call1((args,))?;
            let asyncio = PyModule::import(py, "asyncio")?;
            if asyncio
                .call_method1("iscoroutine", (&result,))?
                .extract::<bool>()?
            {
                result = asyncio.call_method1("run", (result,))?;
            }
            json_mod
                .call_method1("dumps", (result,))?
                .extract::<String>()
        };
        run().map_err(|err| err.to_string())
    })
}
