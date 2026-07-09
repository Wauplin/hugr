//! `hugr build --surface python` — the generated PyO3/maturin surface.
//!
//! The generation logic (Cargo.toml / pyproject / lib.rs / __init__.py /
//! _models.py, and the JSON-Schema → dataclass generator) is unit-tested in
//! `src/build_python.rs` and `src/schema_py.rs` with no toolchain. The heavy
//! end-to-end test below actually runs `build_python` against the checked-in
//! `hugr-docs` crate — compiling the CLI shim, extracting the schema, generating
//! the package, and invoking `maturin`. It is `#[ignore]`d because it needs a
//! Rust toolchain, `maturin`, and a Python interpreter, and is slow.

use std::path::PathBuf;

use hugr_toolkit::build::BuildOptions;
use hugr_toolkit::build_python::build_python;
use hugr_toolkit::manifest::AgentDefinition;

#[test]
#[ignore = "compiles a detached cdylib + runs maturin; slow, needs maturin + python"]
fn real_python_build_generates_typed_package_and_wheel() {
    let agent_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hugr-docs");
    let mut def = AgentDefinition::load(&agent_dir).expect("load hugr-docs manifest");
    def.source_dir = Some(agent_dir);

    let out_dir = std::env::temp_dir().join(format!("hugr-py-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    let opts = BuildOptions {
        out_dir: out_dir.clone(),
        release: false,
    };

    let outcome = build_python(&def, &opts).expect("python surface builds");
    assert_eq!(outcome.module, "hugr_docs");

    // The generated package is a typed, maturin-mixed project.
    let pkg = outcome.crate_dir.join("python").join("hugr_docs");
    let init = std::fs::read_to_string(pkg.join("__init__.py")).unwrap();
    assert!(init.contains("def ask("), "{init}");
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

    // maturin produced a wheel.
    let wheel = outcome.wheel.expect("maturin produced a wheel");
    assert_eq!(wheel.extension().and_then(|e| e.to_str()), Some("whl"));
    assert!(wheel.is_file(), "wheel exists at {}", wheel.display());

    let _ = std::fs::remove_dir_all(&out_dir);
}
