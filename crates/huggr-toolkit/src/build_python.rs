//! `huggr build --surface python`: generate a PyO3 + maturin project exposing
//! typed blocking and streaming APIs for a built agent.
//!
//! The Python surface is a *generated* project, exactly like the CLI shim — the
//! agent crate stays clean (just its response contract) and the toolkit owns
//! surface generation. Layout is a maturin "mixed" project:
//!
//! ```text
//! <agent>-python/
//!   Cargo.toml           # cdylib `_native`, links huggr-toolkit + the agent crate
//!   pyproject.toml       # maturin backend, module-name = <module>._native
//!   bundle.bin           # the embedded agent bundle (same as the CLI shim)
//!   src/lib.rs           # PyO3: signal-aware AgentEvent stream, in-process
//!   python/<module>/
//!     __init__.py        # typed ask(...) and async run(...)
//!     _models.py         # generated dataclasses: <Response> and Answer
//!     _types.py          # shared stable contract and event dataclasses
//!     py.typed           # PEP 561 marker
//! ```
//!
//! Strict typing without a second validator: Rust already casts model output
//! into the agent's response type before it reaches `Answer.response`, so
//! `_models.py` only *deserializes* the already-valid JSON into typed
//! dataclasses. The schema those dataclasses mirror is read from the built
//! artifact's `--config` (the schemars output — one source of truth), so the
//! Python types can never drift from the Rust ones.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::build::{
    BuildError, BuildOptions, ResponseDependency, build as build_cli, response_dependency,
    sanitize_crate_name, write_bundle,
};
use crate::manifest::AgentDefinition;
use crate::schema_py;

const SHARED_PYTHON_TYPES: &str =
    include_str!("../../../bindings/python/python/huggr_agents/_types.py");

/// The result of a successful Python-surface build.
#[derive(Clone, Debug)]
pub struct PythonBuildOutcome {
    /// The generated maturin project directory.
    pub crate_dir: PathBuf,
    /// The importable Python module name (e.g. `huglet_docs`).
    pub module: String,
    /// The built wheel, if maturin produced one.
    pub wheel: Option<PathBuf>,
}

/// Generate the PyO3/maturin project for `def` and build it into a wheel.
///
/// Building the CLI shim first is deliberate: running it with `--config` yields
/// the compiled response schema (schemars), which the generated `_models.py`
/// mirrors. The CLI binary is a useful byproduct left under the same out dir.
pub fn build_python(
    def: &AgentDefinition,
    opts: &BuildOptions,
) -> Result<PythonBuildOutcome, BuildError> {
    // 1. Build the CLI shim and read the response schema from `--config`.
    let cli = build_cli(def, opts)?;
    let schema = response_schema(&cli.binary)?;

    // 2. Lay out the maturin project.
    let pkg = sanitize_crate_name(&def.agent.name);
    let module = python_module_name(&def.agent.name);
    let crate_dir = opts.out_dir.join(format!("{pkg}-python"));

    write_bundle(def, &crate_dir)?;
    let response_dep = response_dependency(def)?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        cargo_toml(&pkg, &response_dep),
    )?;
    std::fs::write(crate_dir.join("src/lib.rs"), lib_rs(&response_dep))?;
    std::fs::write(
        crate_dir.join("pyproject.toml"),
        pyproject_toml(def, &module),
    )?;

    // 3. Generate the typed Python package.
    let pkg_dir = crate_dir.join("python").join(&module);
    std::fs::create_dir_all(&pkg_dir)?;
    let generated = schema_py::generate(&schema, "Response");
    std::fs::write(pkg_dir.join("_types.py"), SHARED_PYTHON_TYPES)?;
    std::fs::write(pkg_dir.join("_models.py"), models_py(&generated))?;
    std::fs::write(
        pkg_dir.join("__init__.py"),
        init_py(def, &module, &generated.root_class),
    )?;
    std::fs::write(pkg_dir.join("py.typed"), "")?;

    // 4. Build the wheel with maturin.
    let wheel = run_maturin(&crate_dir, opts)?;
    Ok(PythonBuildOutcome {
        crate_dir,
        module,
        wheel,
    })
}

/// Run the freshly built CLI binary with `--config --json` and pull out the
/// response schema (`config["response"]`). `--config` needs no API key and no
/// runtime args, so this is a cheap, offline introspection.
fn response_schema(binary: &Path) -> Result<Value, BuildError> {
    let output = Command::new(binary)
        .arg("--config")
        .arg("--json")
        .output()
        .map_err(|source| BuildError::SchemaExtraction {
            binary: binary.to_path_buf(),
            message: format!("spawning the agent binary: {source}"),
        })?;
    if !output.status.success() {
        return Err(BuildError::SchemaExtraction {
            binary: binary.to_path_buf(),
            message: format!(
                "`--config` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    let config: Value =
        serde_json::from_slice(&output.stdout).map_err(|source| BuildError::SchemaExtraction {
            binary: binary.to_path_buf(),
            message: format!("parsing `--config` output: {source}"),
        })?;
    // Missing schema is not fatal — the generator degrades to a permissive alias.
    Ok(config.get("response").cloned().unwrap_or(Value::Null))
}

/// `<agent-name>` with dashes → underscores and non-ident chars stripped, so it
/// is a legal Python module name (e.g. `huglet-docs` → `huglet_docs`).
fn python_module_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        out.insert(0, '_');
    }
    out
}

/// The cdylib crate's `Cargo.toml`. Detached from any surrounding workspace, it
/// paths back to the installed `huggr-toolkit` and (for a typed contract) the
/// agent crate so the Rust response type registers.
fn cargo_toml(pkg: &str, response_dep: &Option<ResponseDependency>) -> String {
    let toolkit_dir = env!("CARGO_MANIFEST_DIR");
    let response_dep = response_dep
        .as_ref()
        .map(ResponseDependency::cargo_dep)
        .unwrap_or_default();
    format!(
        r#"# Generated by `huggr build --surface python`. Do not edit by hand.
[package]
name = "{pkg}-python"
version = "0.0.0"
edition = "2021"

[lib]
name = "_native"
crate-type = ["cdylib"]

# Detach from any surrounding workspace so this crate builds standalone.
[workspace]

[dependencies]
huggr-toolkit = {{ path = "{toolkit_dir}", features = ["python-bridge"] }}
{response_dep}
pyo3 = {{ version = "0.23", features = ["extension-module", "abi3-py39"] }}
tokio = {{ version = "1", features = ["rt-multi-thread"] }}
serde_json = "1"
"#
    )
}

/// The PyO3 `src/lib.rs`: start one signal-aware event stream over the real
/// runtime. Python owns the typed casting across the narrow waist.
fn lib_rs(response_dep: &Option<ResponseDependency>) -> String {
    let options = response_dep
        .as_ref()
        .map(ResponseDependency::runtime_options)
        .unwrap_or_else(|| "huggr_toolkit::runtime::RuntimeOptions::default()".to_string());
    format!(
        r#"// Generated by `huggr build --surface python`. Do not edit by hand.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use huggr_toolkit::python_bridge::{{EventStream, answer_ready_event, forward_agent_events}};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

static BUNDLE: &[u8] = include_bytes!("../bundle.bin");

#[pyfunction]
#[pyo3(signature = (question, trace_id=None, blob_paths=None, skill_paths=None, extra_json=None, runtime_json=None, api_token=None))]
fn ask_events_json(
    question: String,
    trace_id: Option<String>,
    blob_paths: Option<Vec<String>>,
    skill_paths: Option<Vec<String>>,
    extra_json: Option<String>,
    runtime_json: Option<String>,
    api_token: Option<String>,
) -> PyResult<EventStream> {{
    let runtime_values: BTreeMap<String, String> = match runtime_json {{
        Some(s) => serde_json::from_str(&s)
            .map_err(|e| PyRuntimeError::new_err(format!("invalid runtime json: {{e}}")))?,
        None => BTreeMap::new(),
    }};
    let extra: serde_json::Value = match extra_json {{
        Some(s) => serde_json::from_str(&s)
            .map_err(|e| PyRuntimeError::new_err(format!("invalid extra json: {{e}}")))?,
        None => serde_json::Value::Null,
    }};
    let blobs: Vec<PathBuf> = blob_paths
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let skills: Vec<PathBuf> = skill_paths
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let mut options = {options};
    if let Some(api_token) = api_token {{
        options = options.with_api_token(api_token);
    }}
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| PyRuntimeError::new_err(format!("starting async runtime: {{e}}")))?,
    );
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = {{
        let _guard = runtime.enter();
        tokio::spawn(async move {{
            match huggr_toolkit::surface::prepare_bundle_ask_with_options(
                BUNDLE, &options, question, trace_id,
                huggr_toolkit::surface::AskPaths {{ blobs: &blobs, skills: &skills }},
                extra, &runtime_values,
            )
            .await
            {{
                Ok((agent, ask)) => forward_agent_events(agent, ask, tx).await,
                Err(answer) => {{
                    let _ = tx.send(answer_ready_event(answer.clone()));
                    Ok(answer)
                }}
            }}
        }})
    }};
    Ok(EventStream::new(rx, handle, runtime))
}}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {{
    m.add_class::<EventStream>()?;
    m.add_function(wrap_pyfunction!(ask_events_json, m)?)?;
    Ok(())
}}
"#
    )
}

/// The maturin `pyproject.toml` (mixed Rust/Python project).
fn pyproject_toml(def: &AgentDefinition, module: &str) -> String {
    let dist = def.agent.name.replace('_', "-");
    let version = if def.agent.version.trim().is_empty() {
        "0.0.0"
    } else {
        def.agent.version.trim()
    };
    let description = toml_escape(&def.agent.description);
    format!(
        r#"# Generated by `huggr build --surface python`. Do not edit by hand.
[build-system]
requires = ["maturin>=1.7,<2"]
build-backend = "maturin"

[project]
name = "{dist}"
version = "{version}"
description = "{description}"
requires-python = ">=3.9"

[tool.maturin]
python-source = "python"
module-name = "{module}._native"
"#
    )
}

/// The generated `_models.py`: the response dataclasses from the schema, plus
/// the stable Ask/Answer contract types wired to the root response type.
fn models_py(generated: &schema_py::Generated) -> String {
    let root = &generated.root_class;
    format!(
        r#"# Generated by `huggr build --surface python`. Do not edit by hand.
"""Typed models mirroring the Rust Ask/Answer contract and this agent's
response schema. Validation happens once, on the Rust side; these classes only
cast the already-valid JSON into typed objects."""
from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Dict, List, Literal, Optional

from ._types import AnswerMeta, BlobHandle

# Response models, generated from the agent's JSON Schema.

{response_models}


@dataclass
class Answer:
    """The uniform result of one ask. Branch on ``status``: on success
    ``response`` is set to the typed response and ``error`` is None; on error
    ``response`` is None and ``error`` carries the message."""

    status: str
    trace_id: str
    metadata: AnswerMeta
    response: Optional[{root}] = None
    error: Optional[str] = None
    blobs: List[BlobHandle] = field(default_factory=list)
    extra: Any = None

    @property
    def ok(self) -> bool:
        return self.status == "success"

    @classmethod
    def _from_dict(cls, d: dict) -> "Answer":
        status = d.get("status", "")
        raw = d.get("response") or {{}}
        if status == "success":
            response: Optional[{root}] = {root}._from_dict(raw)
            error = None
        else:
            response = None
            error = raw.get("error") if isinstance(raw, dict) else None
        return cls(
            status=status,
            trace_id=d.get("trace_id", ""),
            metadata=AnswerMeta.from_dict(d.get("metadata") or {{}}),
            response=response,
            error=error,
            blobs=[BlobHandle.from_dict(b) for b in d.get("blobs", [])],
            extra=d.get("extra"),
        )
"#,
        response_models = generated.code,
        root = root,
    )
}

/// The generated `__init__.py`: typed sync ask and async event-stream APIs.
/// Declared runtime arguments become typed parameters on both methods.
fn init_py(def: &AgentDefinition, module: &str, root_class: &str) -> String {
    let positional: Vec<_> = def.runtime.args.iter().filter(|a| a.positional).collect();
    let keyword: Vec<_> = def.runtime.args.iter().filter(|a| !a.positional).collect();

    // Signature.
    let mut params = String::new();
    for arg in &positional {
        params.push_str(&format!("    {}: str,\n", py_arg(&arg.name)));
    }
    params.push_str("    question: str,\n");
    params.push_str("    *,\n");
    for arg in &keyword {
        if arg.required {
            params.push_str(&format!("    {}: str,\n", py_arg(&arg.name)));
        } else {
            params.push_str(&format!(
                "    {}: Optional[str] = None,\n",
                py_arg(&arg.name)
            ));
        }
    }
    params.push_str("    trace_id: Optional[str] = None,\n");
    params.push_str("    blobs: Optional[List[str]] = None,\n");
    params.push_str("    skills: Optional[List[str]] = None,\n");
    params.push_str("    extra: Optional[dict] = None,\n");
    params.push_str("    api_token: Optional[str] = None,\n");

    // Docstring.
    let mut doc = String::new();
    if !def.agent.description.trim().is_empty() {
        doc.push_str(&format!("    {}\n\n", def.agent.description.trim()));
    }
    doc.push_str("    Args:\n");
    for arg in positional.iter().chain(keyword.iter()) {
        let help = if arg.help.trim().is_empty() {
            "Runtime argument."
        } else {
            arg.help.trim()
        };
        doc.push_str(&format!("        {}: {}\n", py_arg(&arg.name), help));
    }
    doc.push_str("        question: The question to ask the agent.\n");
    doc.push_str(
        "        trace_id: Resume/fork from an existing trace id (writes a new child trace).\n",
    );
    doc.push_str("        blobs: Local file paths to hand in as inbound blobs.\n");
    doc.push_str("        skills: Local SKILL.md folder paths to add for this ask.\n");
    doc.push_str("        extra: Opaque caller metadata, echoed into the trace.\n");
    doc.push_str(
        "        api_token: Model credential for this ask; overrides every provider's\n            api_key_env and never enters the trace.\n",
    );

    // Runtime-values dict.
    let mut runtime_build = String::from("    runtime: Dict[str, str] = {}\n");
    for arg in &positional {
        runtime_build.push_str(&format!(
            "    runtime[\"{name}\"] = {ident}\n",
            name = arg.name,
            ident = py_arg(&arg.name)
        ));
    }
    for arg in &keyword {
        let ident = py_arg(&arg.name);
        if arg.required {
            runtime_build.push_str(&format!("    runtime[\"{}\"] = {ident}\n", arg.name));
        } else {
            runtime_build.push_str(&format!(
                "    if {ident} is not None:\n        runtime[\"{}\"] = {ident}\n",
                arg.name
            ));
        }
    }

    format!(
        r#"# Generated by `huggr build --surface python`. Do not edit by hand.
"""Typed Python bindings for the `{agent_name}` huglet.

    import {module}
    answer = {module}.ask(..., "your question")
    if answer.ok:
        print(answer.response)
    else:
        print("error:", answer.error)
"""
from __future__ import annotations

import asyncio
import json
from typing import AsyncIterator, Dict, List, Optional

from . import _native
from ._models import Answer, AnswerMeta, BlobHandle, {root_class}
from ._types import (
    AgentEventFor,
    AskStartedEvent,
    AnswerReadyEvent,
    DoneEvent,
    DoneReason,
    ModelEndedEvent,
    ModelStartedEvent,
    NoticeEvent,
    TextDeltaEvent,
    ToolEndedEvent,
    ToolStartedEvent,
    Usage,
    agent_event_from_dict,
)

AgentEvent = AgentEventFor[Answer]

__all__ = [
    "ask", "run", "AgentEvent", "AskStartedEvent", "Answer", "AnswerMeta",
    "AnswerReadyEvent", "BlobHandle", "DoneEvent", "DoneReason",
    "ModelEndedEvent", "ModelStartedEvent", "NoticeEvent", "TextDeltaEvent",
    "ToolEndedEvent", "ToolStartedEvent", "Usage", "{root_class}",
]


def ask(
{params}) -> Answer:
    r"""Ask the agent one question and return a typed :class:`Answer`.

{doc}    """
{runtime_build}    stream = _native.ask_events_json(
        question,
        trace_id,
        list(blobs) if blobs else None,
        list(skills) if skills else None,
        json.dumps(extra) if extra is not None else None,
        json.dumps(runtime),
        api_token,
    )
    try:
        while True:
            raw = stream.next_event()
            if raw is None:
                raise RuntimeError("event stream ended without an answer")
            event = agent_event_from_dict(json.loads(raw), Answer._from_dict)
            if isinstance(event, AnswerReadyEvent):
                return event.answer
    finally:
        stream.cancel()


async def run(
{params}) -> AsyncIterator[AgentEvent]:
    r"""Stream typed lifecycle events for one ask.

{doc}    """
{runtime_build}    stream = _native.ask_events_json(
        question,
        trace_id,
        list(blobs) if blobs else None,
        list(skills) if skills else None,
        json.dumps(extra) if extra is not None else None,
        json.dumps(runtime),
        api_token,
    )
    try:
        while True:
            raw = await asyncio.to_thread(stream.next_event)
            if raw is None:
                return
            yield agent_event_from_dict(json.loads(raw), Answer._from_dict)
    finally:
        stream.cancel()
"#,
        agent_name = def.agent.name,
        module = module,
        root_class = root_class,
        params = params,
        doc = doc,
        runtime_build = runtime_build,
    )
}

/// Run `maturin build` in the generated project, returning the wheel path if it
/// can be located. A missing `maturin` is a clear, actionable error.
fn run_maturin(crate_dir: &Path, opts: &BuildOptions) -> Result<Option<PathBuf>, BuildError> {
    let mut cmd = Command::new("maturin");
    cmd.arg("build").current_dir(crate_dir);
    if opts.release {
        cmd.arg("--release");
    }
    let status = cmd.status().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            BuildError::MaturinMissing {
                crate_dir: crate_dir.to_path_buf(),
            }
        } else {
            BuildError::MaturinSpawn(source)
        }
    })?;
    if !status.success() {
        return Err(BuildError::Maturin {
            code: status.code().unwrap_or(-1),
        });
    }
    Ok(newest_wheel(crate_dir))
}

/// The most recently modified wheel under `target/wheels`, if any.
fn newest_wheel(crate_dir: &Path) -> Option<PathBuf> {
    let dir = crate_dir.join("target").join("wheels");
    let mut wheels: Vec<_> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("whl"))
        .collect();
    wheels.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    wheels.pop()
}

/// A Python identifier for a runtime-arg name (dashes → underscores).
fn py_arg(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs_def() -> AgentDefinition {
        let src = r#"
[agent]
name = "huglet-docs"
version = "0.0.2"
description = "Answers questions from a docs folder."
[models]
default = "balanced"
[tools.fs_read]
root = "."
[runtime.args.docs_path]
target = "tools.fs_read.root"
positional = true
required = true
help = "Folder containing the documentation to search."
"#;
        AgentDefinition::parse(src, "huggr.toml").unwrap()
    }

    #[test]
    fn module_name_is_python_legal() {
        assert_eq!(python_module_name("huglet-docs"), "huglet_docs");
        assert_eq!(python_module_name("2fast"), "_2fast");
        assert_eq!(python_module_name("my agent!"), "my_agent_");
    }

    #[test]
    fn cargo_toml_declares_cdylib_and_pyo3() {
        let toml = cargo_toml("huglet-docs", &None);
        assert!(toml.contains("crate-type = [\"cdylib\"]"));
        assert!(toml.contains("name = \"_native\""));
        assert!(toml.contains("pyo3 = { version"));
        assert!(toml.contains("huggr-toolkit = { path ="));
        assert!(toml.contains("features = [\"python-bridge\"]"));
    }

    #[test]
    fn pyproject_uses_maturin_mixed_layout() {
        let py = pyproject_toml(&docs_def(), "huglet_docs");
        assert!(py.contains("build-backend = \"maturin\""));
        assert!(py.contains("python-source = \"python\""));
        assert!(py.contains("module-name = \"huglet_docs._native\""));
        assert!(py.contains("name = \"huglet-docs\""));
        assert!(py.contains("version = \"0.0.2\""));
    }

    #[test]
    fn lib_rs_bridges_to_event_stream() {
        let rs = lib_rs(&None);
        assert!(rs.contains("fn ask_events_json("));
        assert!(rs.contains("prepare_bundle_ask_with_options"));
        assert!(rs.contains("forward_agent_events"));
        assert!(rs.contains("EventStream::new"));
        assert!(rs.contains("fn _native("));
        assert!(rs.contains("RuntimeOptions::default()"));
        assert!(rs.contains("api_token: Option<String>"));
        assert!(rs.contains("options.with_api_token(api_token)"));
    }

    #[test]
    fn init_py_exposes_positional_runtime_arg_before_question() {
        let init = init_py(&docs_def(), "huglet_docs", "DocsResponse");
        // docs_path (positional, required) leads, then question, then kw-only.
        let dp = init.find("docs_path: str,").unwrap();
        let q = init.find("question: str,").unwrap();
        assert!(dp < q, "positional runtime arg precedes question");
        assert!(init.contains("runtime[\"docs_path\"] = docs_path"));
        assert!(init.contains("-> Answer:"));
        assert!(init.contains("from ._models import Answer, AnswerMeta, BlobHandle, DocsResponse"));
        assert!(init.contains("async def run("));
        assert!(init.contains("-> AsyncIterator[AgentEvent]:"));
        assert!(init.contains("AgentEvent = AgentEventFor[Answer]"));
        assert!(init.contains("_native.ask_events_json("));
        assert!(init.contains("await asyncio.to_thread(stream.next_event)"));
        assert!(init.contains("stream.cancel()"));
        // api_token is a kw-only ask parameter forwarded to the bridge.
        assert!(init.contains("api_token: Optional[str] = None,"));
        let api_token = init.find("api_token: Optional[str] = None,").unwrap();
        assert!(q < api_token, "api_token follows the question");
    }

    #[test]
    fn models_py_types_answer_response_with_root_class() {
        let generated = schema_py::Generated {
            root_class: "DocsResponse".to_string(),
            code: "@dataclass\nclass Document:\n    path: str\n    url: str\n\n@dataclass\nclass DocsResponse:\n    response: str\n    related_documents: List[Document]".to_string(),
        };
        let models = models_py(&generated);
        assert!(models.contains("class DocsResponse:"));
        assert!(models.contains("class Document:"));
        assert!(models.contains("related_documents: List[Document]"));
        assert!(models.contains("response: Optional[DocsResponse] = None"));
        assert!(models.contains("class Answer:"));
        assert!(models.contains("from ._types import AnswerMeta, BlobHandle"));
    }
}
