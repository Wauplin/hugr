//! `huggr build`: compile an agent crate folder into one self-contained CLI
//! binary speaking the ask/answer JSON contract and serving `--mcp-serve`.
//!
//! The approach: generate a small shim crate that embeds the agent files as a
//! [`bundle`] and wraps the shared [`crate::surface::run_cli`] path, then
//! invoke `cargo`. The artifact carries its whole agent bundle and needs no repo
//! checkout to run (it unpacks the bundle into a per-agent home on startup;
//! see `surface`). Building, however, needs the Rust toolchain and a path back
//! to this repo's crates (prebuilt-runtime embedding is a later optimization).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle;
use crate::manifest::AgentDefinition;
use crate::models::{
    MODEL_SNAPSHOT_FILE, ModelConfigError, catalog_from_resolved, load_or_create_global_catalog,
    resolve_source_definition,
};
use crate::runtime::{DEFAULT_SCRATCH_DIRNAME, DEFAULT_TRACE_DIRNAME};

/// Options controlling a build.
#[derive(Clone, Debug)]
pub struct BuildOptions {
    /// Where the generated shim crate is written. The built binary lands under
    /// its `target/`.
    pub out_dir: PathBuf,
    /// Build in release mode (`--release`).
    pub release: bool,
}

/// The result of a successful build.
#[derive(Clone, Debug)]
pub struct BuildOutcome {
    /// The generated shim crate directory.
    pub crate_dir: PathBuf,
    /// The built, self-contained agent binary.
    pub binary: PathBuf,
}

/// Failure to build a surface.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("agent has no source folder to bundle")]
    NoSourceDir,
    #[error(
        "agent crate must define `pub const RESPONSE_RUST_TYPE: &str = \"crate_name::TypeName\";` in src/lib.rs"
    )]
    MissingResponseRustType { path: PathBuf },
    #[error("agent RESPONSE_RUST_TYPE `{rust_type}` must look like `crate_name::TypeName`")]
    InvalidResponseRustType { rust_type: String },
    #[error("reading agent Cargo.toml at {path}: {source}")]
    AgentCargoToml {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("reading agent response source at {path}: {source}")]
    AgentResponseSource {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing agent Cargo.toml at {path}: {message}")]
    AgentCargoTomlParse { path: PathBuf, message: String },
    #[error("writing generated crate: {0}")]
    Io(#[from] std::io::Error),
    #[error("`cargo build` failed (exit {code}). See the output above.")]
    Cargo { code: i32 },
    #[error("could not run `cargo`: {0}")]
    CargoSpawn(std::io::Error),
    #[error("extracting the response schema from `{binary}`: {message}")]
    SchemaExtraction { binary: PathBuf, message: String },
    #[error(
        "`maturin` was not found on PATH. Install it (`pipx install maturin` or `pip install maturin`) and re-run, or build the generated project at {crate_dir} yourself."
    )]
    MaturinMissing { crate_dir: PathBuf },
    #[error("`maturin build` failed (exit {code}). See the output above.")]
    Maturin { code: i32 },
    #[error("could not run `maturin`: {0}")]
    MaturinSpawn(std::io::Error),
    #[error(transparent)]
    Models(#[from] ModelConfigError),
}

/// Generate a shim crate embedding `def` and compile it into a standalone
/// agent binary. Diagnostics from `cargo` stream to this process's stderr.
pub fn build(def: &AgentDefinition, opts: &BuildOptions) -> Result<BuildOutcome, BuildError> {
    let pkg = sanitize_crate_name(&def.agent.name);
    let crate_dir = opts.out_dir.join(format!("{pkg}-cli"));
    let src_dir = crate_dir.join("src");

    write_bundle(def, &crate_dir)?;
    let response_dep = response_dependency(def)?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        cli_cargo_toml(&pkg, &response_dep),
    )?;
    std::fs::write(src_dir.join("main.rs"), main_rs(&response_dep))?;

    run_cargo(&crate_dir, opts, &["build"])?;

    let profile = if opts.release { "release" } else { "debug" };
    let binary = crate_dir.join("target").join(profile).join(&pkg);
    Ok(BuildOutcome { crate_dir, binary })
}

/// Create the shim crate's `src/` dir and write the embedded agent bundle,
/// excluding runtime-only directories so the artifact ships config + tool data
/// but no prior traces/scratch. Shared by every generated surface (CLI, Python).
pub(crate) fn write_bundle(def: &AgentDefinition, crate_dir: &Path) -> Result<(), BuildError> {
    let source_dir = def.source_dir.as_ref().ok_or(BuildError::NoSourceDir)?;
    std::fs::create_dir_all(crate_dir.join("src"))?;
    let excludes = bundle_excludes(def);
    let exclude_refs: Vec<&str> = excludes.iter().map(String::as_str).collect();
    let global = load_or_create_global_catalog()?;
    let resolved = resolve_source_definition(def, &global)?;
    let snapshot = catalog_from_resolved(&resolved).to_toml()?;
    let blob = bundle::pack_with_files(
        source_dir,
        &exclude_refs,
        &[(MODEL_SNAPSHOT_FILE, snapshot.as_bytes())],
    )?;
    std::fs::write(crate_dir.join("bundle.bin"), &blob)?;
    Ok(())
}

/// Run a `cargo` subcommand in the generated crate, honouring `--release`.
fn run_cargo(crate_dir: &Path, opts: &BuildOptions, args: &[&str]) -> Result<(), BuildError> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(args).current_dir(crate_dir);
    if opts.release {
        cmd.arg("--release");
    }
    let status = cmd.status().map_err(BuildError::CargoSpawn)?;
    if !status.success() {
        return Err(BuildError::Cargo {
            code: status.code().unwrap_or(-1),
        });
    }
    Ok(())
}

/// Relative paths to keep out of the embedded bundle: the runtime-state roots
/// (traces, scratch, memory, feedback) and common build/VCS junk. Configured
/// roots are excluded by their exact relative path, so `store = "data/traces"`
/// keeps `data/traces` out without dropping the rest of `data`.
fn bundle_excludes(def: &AgentDefinition) -> Vec<String> {
    let mut ex = vec![
        DEFAULT_TRACE_DIRNAME.to_string(),
        DEFAULT_SCRATCH_DIRNAME.to_string(),
        crate::runtime::DEFAULT_MEMORY_DIRNAME.to_string(),
        crate::runtime::DEFAULT_FEEDBACK_DIRNAME.to_string(),
        "target".to_string(),
        "dist".to_string(),
        ".git".to_string(),
    ];
    if let Some(store) = &def.traces.store {
        if let Some(rel) = normalized_rel(store) {
            ex.push(rel);
        }
    }
    if let Some(root) = &def.scratchpad.root {
        if let Some(rel) = normalized_rel(root) {
            ex.push(rel);
        }
    }
    if let Some(root) = def
        .tools
        .iter()
        .find(|grant| grant.name == "memory")
        .and_then(|grant| grant.config.get("root"))
        .and_then(serde_json::Value::as_str)
        && let Some(rel) = normalized_rel(root)
    {
        ex.push(rel);
    }
    ex.sort();
    ex.dedup();
    ex
}

/// A configured state root as a crate-relative exclusion path. Absolute or
/// parent-relative roots live outside the crate dir and need no exclusion;
/// `.` cannot be excluded without emptying the bundle.
fn normalized_rel(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// A cargo-legal package/binary name derived from the agent name. Shared by the
/// CLI and Python surfaces.
pub(crate) fn sanitize_crate_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // A crate name cannot start with a digit.
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        out.insert_str(0, "agent-");
    }
    out
}

/// The CLI shim's `Cargo.toml`. The empty `[workspace]` table detaches it from
/// this repo's workspace, and the path dependency points back at the installed
/// `huggr-toolkit` crate (resolved from this binary's compile-time manifest dir).
fn cli_cargo_toml(pkg: &str, response_dep: &Option<ResponseDependency>) -> String {
    let toolkit_dir = env!("CARGO_MANIFEST_DIR");
    let response_dep = response_dep
        .as_ref()
        .map(ResponseDependency::cargo_dep)
        .unwrap_or_default();
    format!(
        r#"# Generated by `huggr build`. Do not edit by hand.
[package]
name = "{pkg}"
version = "0.0.0"
edition = "2021"

[[bin]]
name = "{pkg}"
path = "src/main.rs"

# Detach from any surrounding workspace so this crate builds standalone.
[workspace]

[dependencies]
huggr-toolkit = {{ path = "{toolkit_dir}" }}
{response_dep}
tokio = {{ version = "1", features = ["rt-multi-thread", "macros"] }}
"#
    )
}

/// The CLI shim's `main.rs` — embed the bundle and delegate to the universal
/// CLI.
fn main_rs(response_dep: &Option<ResponseDependency>) -> String {
    let options = response_dep
        .as_ref()
        .map(ResponseDependency::runtime_options)
        .unwrap_or_else(|| "huggr_toolkit::runtime::RuntimeOptions::default()".to_string());
    format!(
        r#"// Generated by `huggr build`. Do not edit by hand.
static BUNDLE: &[u8] = include_bytes!("../bundle.bin");

#[tokio::main]
async fn main() {{
    let options = {options};
    let code = huggr_toolkit::surface::run_cli_with_options(BUNDLE, options).await;
    std::process::exit(code);
}}
"#
    )
}

/// The agent crate that owns a typed response contract, resolved so a generated
/// surface can add it as a path dependency and register the Rust response type.
/// Shared by the CLI and Python surfaces.
#[derive(Clone, Debug)]
pub(crate) struct ResponseDependency {
    dep_name: String,
    package: Option<String>,
    path: PathBuf,
    rust_type: String,
    model_rust_type: String,
    schema_name: String,
    has_answer_hooks: bool,
    has_storage: bool,
}

impl ResponseDependency {
    pub(crate) fn cargo_dep(&self) -> String {
        let path = self.path.display();
        let package = self
            .package
            .as_ref()
            .map(|package| format!(" package = \"{package}\","))
            .unwrap_or_default();
        format!(
            "{dep} = {{{package} path = \"{path}\" }}",
            dep = self.dep_name
        )
    }

    pub(crate) fn runtime_options(&self) -> String {
        let contract = if self.model_rust_type == self.rust_type {
            format!(
                "huggr_toolkit::runtime::ResponseContract::from_type::<{rust_type}>(\"{schema_name}\")",
                rust_type = self.rust_type,
                schema_name = self.schema_name,
            )
        } else {
            format!(
                "huggr_toolkit::runtime::ResponseContract::from_type::<{model_rust_type}>(\"{schema_name}\").with_public_type::<{rust_type}>()",
                model_rust_type = self.model_rust_type,
                rust_type = self.rust_type,
                schema_name = self.schema_name,
            )
        };
        let mut options = format!(
            "huggr_toolkit::runtime::RuntimeOptions::new().with_response_contract({dep_name}::RESPONSE_RUST_TYPE, {contract})",
            dep_name = self.dep_name,
            contract = contract,
        );
        if self.has_answer_hooks {
            options.push_str(&format!(
                ".with_answer_hooks({}::answer_hooks())",
                self.dep_name
            ));
        }
        if self.has_storage {
            options.push_str(&format!(".with_storage({}::storage())", self.dep_name));
        }
        options
    }
}

pub(crate) fn response_dependency(
    def: &AgentDefinition,
) -> Result<Option<ResponseDependency>, BuildError> {
    if def.response_schema.is_some() {
        return Ok(None);
    }
    let base = def.source_dir.as_deref().ok_or(BuildError::NoSourceDir)?;
    let path = base
        .canonicalize()
        .map_err(|source| BuildError::AgentCargoToml {
            path: base.to_path_buf(),
            source,
        })?;
    let rust_type = read_response_rust_type(&path)?;
    let model_rust_type = read_optional_rust_type_const(&path, "MODEL_RESPONSE_RUST_TYPE")?
        .unwrap_or_else(|| rust_type.clone());
    let Some((dep_name, _)) = rust_type.split_once("::") else {
        return Err(BuildError::InvalidResponseRustType { rust_type });
    };
    if dep_name.is_empty() {
        return Err(BuildError::InvalidResponseRustType { rust_type });
    }
    let package_name = cargo_package_name(&path)?;
    let package = (package_name != dep_name).then_some(package_name);
    let has_answer_hooks = has_pub_fn(&path, "answer_hooks")?;
    let has_storage = has_pub_fn(&path, "storage")?;
    Ok(Some(ResponseDependency {
        dep_name: dep_name.to_string(),
        package,
        path,
        model_rust_type,
        has_answer_hooks,
        has_storage,
        schema_name: schema_name_from_rust_type(&rust_type),
        rust_type,
    }))
}

fn read_response_rust_type(agent_dir: &Path) -> Result<String, BuildError> {
    read_rust_type_const(agent_dir, "RESPONSE_RUST_TYPE")?.ok_or_else(|| {
        BuildError::MissingResponseRustType {
            path: agent_dir.join("src/lib.rs"),
        }
    })
}

fn read_optional_rust_type_const(
    agent_dir: &Path,
    const_name: &str,
) -> Result<Option<String>, BuildError> {
    read_rust_type_const(agent_dir, const_name)
}

fn read_rust_type_const(agent_dir: &Path, const_name: &str) -> Result<Option<String>, BuildError> {
    let path = agent_dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&path).map_err(|source| BuildError::AgentResponseSource {
        path: path.clone(),
        source,
    })?;
    let needle = format!("pub const {const_name}");
    let Some(start) = src.find(&needle) else {
        return Ok(None);
    };
    let after_name = &src[start + needle.len()..];
    let Some(eq) = after_name.find('=') else {
        return Ok(None);
    };
    let after_eq = after_name[eq + 1..].trim_start();
    let Some(rest) = after_eq.strip_prefix('"') else {
        return Ok(None);
    };
    let Some(end) = rest.find('"') else {
        return Ok(None);
    };
    let value = rest[..end].to_string();
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn has_pub_fn(agent_dir: &Path, fn_name: &str) -> Result<bool, BuildError> {
    let path = agent_dir.join("src/lib.rs");
    let src = std::fs::read_to_string(&path).map_err(|source| BuildError::AgentResponseSource {
        path: path.clone(),
        source,
    })?;
    Ok(src.contains(&format!("pub fn {fn_name}")))
}

fn cargo_package_name(agent_dir: &Path) -> Result<String, BuildError> {
    let path = agent_dir.join("Cargo.toml");
    let src = std::fs::read_to_string(&path).map_err(|source| BuildError::AgentCargoToml {
        path: path.clone(),
        source,
    })?;
    let table: toml::Table =
        toml::from_str(&src).map_err(|err| BuildError::AgentCargoTomlParse {
            path: path.clone(),
            message: err.to_string(),
        })?;
    table
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| BuildError::AgentCargoTomlParse {
            path,
            message: "missing [package].name".to_string(),
        })
}

fn schema_name_from_rust_type(rust_type: &str) -> String {
    rust_type
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_is_cargo_legal() {
        assert_eq!(sanitize_crate_name("policy-docs"), "policy-docs");
        assert_eq!(sanitize_crate_name("my agent!"), "my_agent_");
        assert_eq!(sanitize_crate_name("2fast"), "agent-2fast");
    }

    #[test]
    fn excludes_cover_runtime_dirs_and_manifest_roots() {
        let src = r#"
[agent]
name = "x"
[models]
default = "balanced"
[traces]
store = "state/traces"
[scratchpad]
root = "work"
"#;
        let def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        let ex = bundle_excludes(&def);
        assert!(ex.contains(&"traces".to_string()));
        assert!(ex.contains(&"scratch".to_string()));
        assert!(ex.contains(&"memory".to_string()));
        assert!(ex.contains(&"feedback".to_string()));
        // Nested roots are excluded by exact relative path, not by their
        // first component: `state/traces` must not shadow all of `state`.
        assert!(ex.contains(&"state/traces".to_string()));
        assert!(!ex.contains(&"state".to_string()));
        assert!(ex.contains(&"work".to_string()));
        assert!(ex.contains(&"target".to_string()));
    }

    #[test]
    fn dot_and_absolute_roots_produce_no_exclusion() {
        assert_eq!(normalized_rel("."), None);
        assert_eq!(normalized_rel("./"), None);
        assert_eq!(normalized_rel("/var/traces"), None);
        assert_eq!(normalized_rel("../shared"), None);
        assert_eq!(normalized_rel("./data/traces"), Some("data/traces".into()));
    }

    #[test]
    fn cli_cargo_toml_detaches_workspace_and_paths_to_toolkit() {
        let toml = cli_cargo_toml("policy-docs", &None);
        assert!(toml.contains("[workspace]"));
        assert!(toml.contains("huggr-toolkit = { path ="));
        assert!(toml.contains("name = \"policy-docs\""));
        assert!(toml.contains("[[bin]]"));
    }

    #[test]
    fn typed_response_build_generates_agent_dependency_and_registry() {
        let src = r#"
[agent]
name = "docs"
[models]
default = "balanced"
"#;
        let mut def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        def.source_dir =
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/huglet-docs"));
        let dep = response_dependency(&def).unwrap().unwrap();
        let toml = cli_cargo_toml("docs", &Some(dep.clone()));
        assert!(toml.contains("huglet_docs = { package = \"huglet-docs\", path ="));
        let main = main_rs(&Some(dep));
        assert!(main.contains("ResponseContract::from_type::<huglet_docs::DocsModelResponse>"));
        assert!(main.contains(".with_public_type::<huglet_docs::DocsResponse>()"));
        assert!(main.contains(".with_answer_hooks(huglet_docs::answer_hooks())"));
        assert!(main.contains("huglet_docs::RESPONSE_RUST_TYPE"));
        assert!(main.contains("\"huglet_docs__DocsResponse\""));
    }

    #[test]
    fn typed_response_build_wires_storage_override_when_exported() {
        let root = std::env::temp_dir().join(format!("huggr-build-storage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"storage-agent\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            r#"pub const RESPONSE_RUST_TYPE: &str = "storage_agent::Response";
pub fn storage() -> huggr_agent::StorageOverrides { todo!() }
"#,
        )
        .unwrap();
        let src = "[agent]\nname = \"storage\"\n[models]\ndefault = \"balanced\"\n";
        let mut def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        def.source_dir = Some(root.clone());

        let dep = response_dependency(&def).unwrap().unwrap();
        let toml = cli_cargo_toml("storage", &Some(dep.clone()));
        let main = main_rs(&Some(dep));
        assert!(toml.contains("storage_agent = { package = \"storage-agent\", path ="));
        assert!(main.contains(".with_storage(storage_agent::storage())"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_response_rust_type_is_an_explicit_error() {
        let root = std::env::temp_dir().join(format!("huggr-build-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"missing\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let src = "[agent]\nname = \"missing\"\n[models]\ndefault = \"balanced\"\n";
        let mut def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        def.source_dir = Some(root.clone());

        let err = response_dependency(&def).unwrap_err();
        assert!(matches!(err, BuildError::MissingResponseRustType { .. }));
        let _ = std::fs::remove_dir_all(root);
    }
}
