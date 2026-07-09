//! `hugr build`: compile a definition folder into one self-contained CLI
//! binary (ARCHITECTURE §21). The binary speaks the ask/answer JSON contract
//! and serves `--mcp-serve`.
//!
//! The approach: generate a small shim crate that embeds the definition as a
//! [`bundle`] and wraps the shared [`crate::surface::run_cli`] path, then
//! invoke `cargo`. The artifact carries its whole definition and needs no repo
//! checkout to run (it unpacks the bundle into a per-agent home on startup;
//! see `surface`). Building, however, needs the Rust toolchain and a path back
//! to this repo's crates (prebuilt-runtime embedding is a later optimization).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle;
use crate::manifest::AgentDefinition;
use crate::runtime::DEFAULT_TRACE_DIRNAME;

/// Default scratchpad dir name (mirrors `hugr-agent`'s default) — excluded from
/// the embedded bundle so a build never ships prior-run scratch state.
const DEFAULT_SCRATCH_DIRNAME: &str = ".scratch";

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
    #[error("definition has no source folder to bundle")]
    NoSourceDir,
    #[error(
        "[response].rust_type requires [response].crate_path so the built shim can link the agent crate"
    )]
    MissingResponseCratePath,
    #[error("[response].rust_type `{rust_type}` must look like `crate_name::TypeName`")]
    InvalidResponseRustType { rust_type: String },
    #[error("resolving [response].crate_path `{path}`: {source}")]
    ResponseCratePath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing generated crate: {0}")]
    Io(#[from] std::io::Error),
    #[error("`cargo build` failed (exit {code}). See the output above.")]
    Cargo { code: i32 },
    #[error("could not run `cargo`: {0}")]
    CargoSpawn(std::io::Error),
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

/// Create the shim crate's `src/` dir and write the embedded definition bundle,
/// excluding runtime-only directories so the artifact ships config + tool data
/// but no prior traces/scratch.
fn write_bundle(def: &AgentDefinition, crate_dir: &Path) -> Result<(), BuildError> {
    let source_dir = def.source_dir.as_ref().ok_or(BuildError::NoSourceDir)?;
    std::fs::create_dir_all(crate_dir.join("src"))?;
    let excludes = bundle_excludes(def);
    let exclude_refs: Vec<&str> = excludes.iter().map(String::as_str).collect();
    let blob = bundle::pack(source_dir, &exclude_refs)?;
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

/// Top-level dir names to keep out of the embedded bundle: the trace store, the
/// scratchpad, and common build/VCS junk.
fn bundle_excludes(def: &AgentDefinition) -> Vec<String> {
    let mut ex = vec![
        DEFAULT_TRACE_DIRNAME.to_string(),
        DEFAULT_SCRATCH_DIRNAME.to_string(),
        "target".to_string(),
        "dist".to_string(),
        ".git".to_string(),
    ];
    // Honour a manifest-configured trace/scratch root (only its first path
    // component matters — `pack` excludes by top-level name).
    if let Some(store) = &def.traces.store {
        if let Some(first) = first_component(store) {
            ex.push(first);
        }
    }
    if let Some(root) = &def.scratchpad.root {
        if let Some(first) = first_component(root) {
            ex.push(first);
        }
    }
    ex.sort();
    ex.dedup();
    ex
}

fn first_component(path: &str) -> Option<String> {
    Path::new(path).components().find_map(|c| match c {
        std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    })
}

/// A cargo-legal package/binary name derived from the agent name.
fn sanitize_crate_name(name: &str) -> String {
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
/// `hugr-toolkit` crate (resolved from this binary's compile-time manifest dir).
fn cli_cargo_toml(pkg: &str, response_dep: &Option<ResponseDependency>) -> String {
    let toolkit_dir = env!("CARGO_MANIFEST_DIR");
    let response_dep = response_dep
        .as_ref()
        .map(ResponseDependency::cargo_dep)
        .unwrap_or_default();
    format!(
        r#"# Generated by `hugr build`. Do not edit by hand.
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
hugr-toolkit = {{ path = "{toolkit_dir}" }}
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
        .unwrap_or_else(|| "hugr_toolkit::runtime::RuntimeOptions::default()".to_string());
    format!(
        r#"// Generated by `hugr build`. Do not edit by hand.
static BUNDLE: &[u8] = include_bytes!("../bundle.bin");

#[tokio::main]
async fn main() {{
    let options = {options};
    let code = hugr_toolkit::surface::run_cli_with_options(BUNDLE, options).await;
    std::process::exit(code);
}}
"#
    )
}

#[derive(Clone, Debug)]
struct ResponseDependency {
    dep_name: String,
    package: Option<String>,
    path: PathBuf,
    rust_type: String,
    schema_name: String,
}

impl ResponseDependency {
    fn cargo_dep(&self) -> String {
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

    fn runtime_options(&self) -> String {
        format!(
            "hugr_toolkit::runtime::RuntimeOptions::new().with_response_type::<{rust_type}>(\"{rust_type}\", \"{schema_name}\")",
            rust_type = self.rust_type,
            schema_name = self.schema_name,
        )
    }
}

fn response_dependency(def: &AgentDefinition) -> Result<Option<ResponseDependency>, BuildError> {
    let Some(rust_type) = &def.response.rust_type else {
        return Ok(None);
    };
    let Some((dep_name, _)) = rust_type.split_once("::") else {
        return Err(BuildError::InvalidResponseRustType {
            rust_type: rust_type.clone(),
        });
    };
    if dep_name.is_empty() {
        return Err(BuildError::InvalidResponseRustType {
            rust_type: rust_type.clone(),
        });
    }
    let crate_path = def
        .response
        .crate_path
        .as_deref()
        .ok_or(BuildError::MissingResponseCratePath)?;
    let base = def.source_dir.as_deref().ok_or(BuildError::NoSourceDir)?;
    let raw_path = if Path::new(crate_path).is_absolute() {
        PathBuf::from(crate_path)
    } else {
        base.join(crate_path)
    };
    let path = raw_path
        .canonicalize()
        .map_err(|source| BuildError::ResponseCratePath {
            path: raw_path.clone(),
            source,
        })?;
    Ok(Some(ResponseDependency {
        dep_name: dep_name.to_string(),
        package: def.response.crate_package.clone(),
        path,
        rust_type: rust_type.clone(),
        schema_name: def.response.schema_name.clone().unwrap_or_else(|| {
            rust_type
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect()
        }),
    }))
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
[models.medium]
model = "m"
[traces]
store = "state/traces"
[scratchpad]
root = "work"
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let ex = bundle_excludes(&def);
        assert!(ex.contains(&".hugr-traces".to_string()));
        assert!(ex.contains(&"state".to_string()));
        assert!(ex.contains(&"work".to_string()));
        assert!(ex.contains(&"target".to_string()));
    }

    #[test]
    fn cli_cargo_toml_detaches_workspace_and_paths_to_toolkit() {
        let toml = cli_cargo_toml("policy-docs", &None);
        assert!(toml.contains("[workspace]"));
        assert!(toml.contains("hugr-toolkit = { path ="));
        assert!(toml.contains("name = \"policy-docs\""));
        assert!(toml.contains("[[bin]]"));
    }

    #[test]
    fn typed_response_build_generates_agent_dependency_and_registry() {
        let src = r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[response]
rust_type = "hugr_docs::DocsResponse"
crate_path = ".."
crate_package = "hugr-docs"
schema_name = "hugr_docs_response"
"#;
        let mut def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        def.source_dir =
            Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hugr-docs/definition"));
        let dep = response_dependency(&def).unwrap().unwrap();
        let toml = cli_cargo_toml("docs", &Some(dep.clone()));
        assert!(toml.contains("hugr_docs = { package = \"hugr-docs\", path ="));
        let main = main_rs(&Some(dep));
        assert!(main.contains("with_response_type::<hugr_docs::DocsResponse>"));
        assert!(main.contains("\"hugr_docs_response\""));
    }
}
