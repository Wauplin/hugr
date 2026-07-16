//! `huggr build --surface python` — the generated PyO3/maturin surface.
//!
//! The generation logic (Cargo.toml / pyproject / lib.rs / __init__.py /
//! _models.py, and the JSON-Schema → dataclass generator) is unit-tested in
//! `src/build_python.rs` and `src/schema_py.rs` with no toolchain. The heavy
//! end-to-end test below actually runs `build_python` against the checked-in
//! `huglet-docs` crate — compiling the CLI shim, extracting the schema, generating
//! the package, and invoking `maturin`. It is `#[ignore]`d because it needs a
//! Rust toolchain, `maturin`, and a Python interpreter, and is slow.

use std::path::PathBuf;
use std::process::Command;

use huggr_toolkit::build::BuildOptions;
use huggr_toolkit::build_python::build_python;
use huggr_toolkit::manifest::AgentDefinition;

#[test]
#[ignore = "compiles a detached cdylib + runs maturin; slow, needs maturin + python"]
fn real_python_build_generates_typed_package_and_wheel() {
    let agent_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/huglet-docs");
    let mut def = AgentDefinition::load(&agent_dir).expect("load huglet-docs manifest");
    def.source_dir = Some(agent_dir);

    let out_dir = std::env::temp_dir().join(format!("huggr-py-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    let opts = BuildOptions {
        out_dir: out_dir.clone(),
        release: false,
    };

    let outcome = build_python(&def, &opts).expect("python surface builds");
    assert_eq!(outcome.module, "huglet_docs");

    // The generated package is a typed, maturin-mixed project.
    let pkg = outcome.crate_dir.join("python").join("huglet_docs");
    let init = std::fs::read_to_string(pkg.join("__init__.py")).unwrap();
    assert!(init.contains("def ask("), "{init}");
    assert!(init.contains("async def run("), "{init}");
    assert!(
        init.contains("docs_path: str"),
        "runtime arg is typed: {init}"
    );
    assert!(init.contains("-> Answer:"), "{init}");

    let models = std::fs::read_to_string(pkg.join("_models.py")).unwrap();
    assert!(models.contains("class DocsResponse:"), "{models}");
    assert!(models.contains("class Document:"), "{models}");
    assert!(models.contains("path: str"), "{models}");
    assert!(models.contains("url: str"), "{models}");
    assert!(
        models.contains("related_documents: List[Document]"),
        "{models}"
    );
    assert!(
        models.contains("response: Optional[DocsResponse] = None"),
        "{models}"
    );
    assert!(pkg.join("py.typed").exists(), "PEP 561 marker present");
    assert!(
        pkg.join("_types.py").exists(),
        "shared event models present"
    );

    // maturin produced a wheel.
    let wheel = outcome.wheel.expect("maturin produced a wheel");
    assert_eq!(wheel.extension().and_then(|e| e.to_str()), Some("whl"));
    assert!(wheel.is_file(), "wheel exists at {}", wheel.display());

    let site = out_dir.join("python-site");
    let install = Command::new("python3")
        .args(["-m", "pip", "install", "--no-deps", "--target"])
        .arg(&site)
        .arg(&wheel)
        .output()
        .expect("install generated wheel");
    assert!(
        install.status.success(),
        "pip install failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );
    let smoke = Command::new("python3")
        .env("PYTHONPATH", &site)
        .args([
            "-c",
            r#"import asyncio
import huglet_docs

async def collect():
    return [event async for event in huglet_docs.run(".", "q", trace_id="../bad")]

events = asyncio.run(collect())
assert isinstance(events[-1], huglet_docs.AnswerReadyEvent)
assert type(events[-1].answer) is huglet_docs.Answer
assert not events[-1].answer.ok
answer = huglet_docs.ask(".", "q", trace_id="../bad")
assert not answer.ok
"#,
        ])
        .output()
        .expect("run generated wheel smoke test");
    assert!(
        smoke.status.success(),
        "generated wheel smoke test failed: {}",
        String::from_utf8_lossy(&smoke.stderr)
    );

    let _ = std::fs::remove_dir_all(&out_dir);
}
