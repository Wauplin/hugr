use std::path::PathBuf;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::{DocsConfigOptions, answer_with_options};

#[pyfunction]
#[pyo3(signature = (
    question,
    docs_path=None,
    api_key=None,
    base_url=None,
    model=None,
    input_usd_per_m_tokens=None,
    output_usd_per_m_tokens=None
))]
fn answer(
    py: Python<'_>,
    question: &str,
    docs_path: Option<&str>,
    api_key: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    input_usd_per_m_tokens: Option<f64>,
    output_usd_per_m_tokens: Option<f64>,
) -> PyResult<Py<PyAny>> {
    let docs_path = match docs_path {
        Some(path) => PathBuf::from(path),
        None => std::env::var_os("HUGR_DOCS_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::new()),
    };
    let options = DocsConfigOptions {
        api_key: api_key.map(str::to_string),
        base_url: base_url.map(str::to_string),
        model: model.map(str::to_string),
        input_usd_per_m_tokens,
        output_usd_per_m_tokens,
    };
    let question = question.to_string();
    let result = py
        .allow_threads(|| {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(answer_with_options(docs_path, options, &question))
        })
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let json_text = serde_json::to_string(&result)
        .map_err(|error| PyRuntimeError::new_err(error.to_string()))?;
    let json = py.import("json")?;
    json.call_method1("loads", (json_text,)).map(Bound::unbind)
}

#[pymodule]
fn hugr_docs(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(answer, m)?)?;
    Ok(())
}
