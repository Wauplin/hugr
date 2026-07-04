//! `hugr build --surface cli`: turn a definition folder into a standalone
//! binary (ROADMAP T2.1, ARCHITECTURE §21.1).
//!
//! The approach (as specced): generate a small shim crate that embeds the
//! definition as a [`bundle`] and wraps [`crate::surface::run_cli`], then invoke
//! `cargo build`. The resulting binary is self-contained — it carries its whole
//! definition and needs no repo checkout to run (it unpacks the bundle into a
//! per-agent home on startup; see `surface`). Building, however, needs the Rust
//! toolchain and a path back to this repo's crates (prebuilt-runtime embedding
//! is a later optimization).
//!
//! Surface choice is compile-time and never part of the definition. `cli` (a
//! standalone binary) and `crate` (a library exposing the typed `Agent`,
//! §21.2) are wired; python/mcp are T2.3–T2.4.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle;
use crate::manifest::AgentDefinition;
use crate::runtime::DEFAULT_TRACE_DIRNAME;

/// Default scratchpad dir name (mirrors `hugr-agent`'s default) — excluded from
/// the embedded bundle so a build never ships prior-run scratch state.
const DEFAULT_SCRATCH_DIRNAME: &str = ".scratch";

/// The surface a build targets. `cli` and `crate` are wired; python/mcp are
/// T2.3–T2.4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Surface {
    Cli,
    Crate,
}

impl Surface {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cli" => Some(Surface::Cli),
            "crate" => Some(Surface::Crate),
            _ => None,
        }
    }
}

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
#[non_exhaustive]
pub struct BuildOutcome {
    /// The generated shim crate directory.
    pub crate_dir: PathBuf,
    /// The built, self-contained agent binary (CLI surface only). For the
    /// `crate` surface there is no binary — downstream crates depend on
    /// `crate_dir` and call `ask` natively.
    pub binary: Option<PathBuf>,
}

/// Failure to build a surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    #[error("surface `{0}` is not implemented yet (supported: cli, crate)")]
    UnsupportedSurface(String),
    #[error("definition has no source folder to bundle")]
    NoSourceDir,
    #[error("writing generated crate: {0}")]
    Io(#[from] std::io::Error),
    #[error("`cargo build` failed (exit {code}). See the output above.")]
    Cargo { code: i32 },
    #[error("could not run `cargo`: {0}")]
    CargoSpawn(std::io::Error),
}

/// Dispatch a build to the requested surface.
pub fn build(
    def: &AgentDefinition,
    surface: Surface,
    opts: &BuildOptions,
) -> Result<BuildOutcome, BuildError> {
    match surface {
        Surface::Cli => build_cli(def, opts),
        Surface::Crate => build_crate(def, opts),
    }
}

/// Generate a shim crate embedding `def` and compile it into a standalone
/// agent binary (`--surface cli`). Diagnostics from `cargo` stream to this
/// process's stderr.
pub fn build_cli(def: &AgentDefinition, opts: &BuildOptions) -> Result<BuildOutcome, BuildError> {
    let pkg = sanitize_crate_name(&def.agent.name);
    let crate_dir = opts.out_dir.join(format!("{pkg}-cli"));
    let src_dir = crate_dir.join("src");

    write_bundle(def, &crate_dir)?;
    std::fs::write(crate_dir.join("Cargo.toml"), cli_cargo_toml(&pkg))?;
    std::fs::write(src_dir.join("main.rs"), MAIN_RS)?;

    run_cargo(&crate_dir, opts, &["build"])?;

    let profile = if opts.release { "release" } else { "debug" };
    let binary = crate_dir.join("target").join(profile).join(&pkg);
    Ok(BuildOutcome {
        crate_dir,
        binary: Some(binary),
    })
}

/// Generate a **library** crate embedding `def` and exposing the typed
/// [`hugr_agent::Agent`] (`--surface crate`, ARCHITECTURE §21.2). Rust
/// orchestrators depend on it and call `ask` with no serialization. The crate
/// is `cargo check`ed to prove it compiles; downstream consumers build it into
/// their own binary.
pub fn build_crate(def: &AgentDefinition, opts: &BuildOptions) -> Result<BuildOutcome, BuildError> {
    let pkg = sanitize_crate_name(&def.agent.name);
    let lib_name = pkg.replace('-', "_");
    let crate_dir = opts.out_dir.join(format!("{pkg}-crate"));
    let src_dir = crate_dir.join("src");

    write_bundle(def, &crate_dir)?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        crate_cargo_toml(&pkg, &lib_name),
    )?;
    std::fs::write(src_dir.join("lib.rs"), LIB_RS)?;

    // A crate surface has no binary of its own — prove it compiles.
    run_cargo(&crate_dir, opts, &["check"])?;

    Ok(BuildOutcome {
        crate_dir,
        binary: None,
    })
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
fn cli_cargo_toml(pkg: &str) -> String {
    let toolkit_dir = env!("CARGO_MANIFEST_DIR");
    format!(
        r#"# Generated by `hugr build --surface cli`. Do not edit by hand.
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
tokio = {{ version = "1", features = ["rt-multi-thread", "macros"] }}
"#
    )
}

/// The crate surface's `Cargo.toml` — a library exposing the typed `Agent`.
/// Depends on `hugr-agent` (for the contract types it re-exports) and
/// `hugr-toolkit` (for the shared `load_agent` path).
fn crate_cargo_toml(pkg: &str, lib_name: &str) -> String {
    let toolkit_dir = env!("CARGO_MANIFEST_DIR");
    let agent_dir = agent_crate_dir();
    format!(
        r#"# Generated by `hugr build --surface crate`. Do not edit by hand.
[package]
name = "{pkg}-agent"
version = "0.0.0"
edition = "2021"

[lib]
name = "{lib_name}"
path = "src/lib.rs"

# Detach from any surrounding workspace so this crate builds standalone.
[workspace]

[dependencies]
hugr-toolkit = {{ path = "{toolkit_dir}" }}
hugr-agent = {{ path = "{agent_dir}" }}
"#
    )
}

/// Absolute path to the `hugr-agent` crate, derived from this crate's location
/// (`crates/hugr-toolkit` → `crates/hugr-agent`).
fn agent_crate_dir() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("hugr-agent"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "../hugr-agent".to_string())
}

/// The CLI shim's `main.rs` — embed the bundle and delegate to the universal
/// CLI.
const MAIN_RS: &str = r#"// Generated by `hugr build --surface cli`. Do not edit by hand.
static BUNDLE: &[u8] = include_bytes!("../bundle.bin");

#[tokio::main]
async fn main() {
    let code = hugr_toolkit::surface::run_cli(BUNDLE).await;
    std::process::exit(code);
}
"#;

/// The crate surface's `lib.rs` — embed the bundle and expose the typed agent
/// plus a convenience `ask`. The `hugr_agent` contract types are re-exported so
/// downstream orchestrators need not depend on `hugr-agent` directly.
const LIB_RS: &str = r#"//! Generated by `hugr build --surface crate`. Do not edit by hand.
//!
//! This crate embeds an agent definition and exposes it as the typed
//! `hugr_agent::Agent` — call [`load`] then `.agent.ask(..)`, or the
//! convenience [`ask`].

static BUNDLE: &[u8] = include_bytes!("../bundle.bin");

pub use hugr_agent::{
    Agent, Answer, AnswerMeta, AnswerStatus, Ask, BlobHandle, BlobPerms, BlobRef, TierSpend,
    TraceId,
};
pub use hugr_toolkit::surface::{LoadError, LoadedAgent};

/// Assemble the embedded agent (unpacks its definition into a per-agent home
/// on first use). Reuse the returned `Agent` across asks.
pub async fn load() -> Result<LoadedAgent, LoadError> {
    hugr_toolkit::surface::load_agent(BUNDLE).await
}

/// Load the embedded agent and run one ask. Convenience for one-shot callers;
/// long-lived orchestrators should `load()` once and reuse the agent.
pub async fn ask(ask: Ask) -> Result<Answer, Box<dyn std::error::Error + Send + Sync>> {
    let loaded = load().await?;
    Ok(loaded.agent.ask(ask).await?)
}
"#;

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
        let toml = cli_cargo_toml("policy-docs");
        assert!(toml.contains("[workspace]"));
        assert!(toml.contains("hugr-toolkit = { path ="));
        assert!(toml.contains("name = \"policy-docs\""));
        assert!(toml.contains("[[bin]]"));
    }

    #[test]
    fn crate_cargo_toml_is_a_lib_with_both_path_deps() {
        let toml = crate_cargo_toml("policy-docs", "policy_docs");
        assert!(toml.contains("[workspace]"));
        assert!(toml.contains("[lib]"));
        assert!(toml.contains("name = \"policy_docs\""));
        assert!(toml.contains("hugr-toolkit = { path ="));
        assert!(toml.contains("hugr-agent = { path ="));
        assert!(!toml.contains("[[bin]]"));
    }
}
