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
//! Surface choice is compile-time and never part of the definition; today only
//! `--surface cli` is implemented (crate/python/mcp are T2.2–T2.4).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bundle;
use crate::manifest::AgentDefinition;
use crate::runtime::DEFAULT_TRACE_DIRNAME;

/// Default scratchpad dir name (mirrors `hugr-agent`'s default) — excluded from
/// the embedded bundle so a build never ships prior-run scratch state.
const DEFAULT_SCRATCH_DIRNAME: &str = ".scratch";

/// The surface a build targets. Only [`Surface::Cli`] is wired today.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Surface {
    Cli,
}

impl Surface {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cli" => Some(Surface::Cli),
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
    /// The built, self-contained agent binary.
    pub binary: PathBuf,
}

/// Failure to build a surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuildError {
    #[error("surface `{0}` is not implemented yet (only `cli` today)")]
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

/// Generate a shim crate embedding `def` and compile it into a standalone
/// agent binary. Diagnostics from `cargo` stream to this process's stderr.
pub fn build_cli(def: &AgentDefinition, opts: &BuildOptions) -> Result<BuildOutcome, BuildError> {
    let source_dir = def.source_dir.as_ref().ok_or(BuildError::NoSourceDir)?;

    let pkg = sanitize_crate_name(&def.agent.name);
    let crate_dir = opts.out_dir.join(format!("{pkg}-cli"));
    let src_dir = crate_dir.join("src");
    std::fs::create_dir_all(&src_dir)?;

    // Bundle the definition, excluding runtime-only directories so the artifact
    // ships config + tool data but no prior traces/scratch.
    let excludes = bundle_excludes(def);
    let exclude_refs: Vec<&str> = excludes.iter().map(String::as_str).collect();
    let blob = bundle::pack(source_dir, &exclude_refs)?;
    std::fs::write(crate_dir.join("bundle.bin"), &blob)?;

    std::fs::write(crate_dir.join("Cargo.toml"), cargo_toml(&pkg))?;
    std::fs::write(src_dir.join("main.rs"), MAIN_RS)?;

    // Compile.
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.arg("build").current_dir(&crate_dir);
    if opts.release {
        cmd.arg("--release");
    }
    let status = cmd.status().map_err(BuildError::CargoSpawn)?;
    if !status.success() {
        return Err(BuildError::Cargo {
            code: status.code().unwrap_or(-1),
        });
    }

    let profile = if opts.release { "release" } else { "debug" };
    let binary = crate_dir.join("target").join(profile).join(&pkg);
    Ok(BuildOutcome { crate_dir, binary })
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

/// The shim's `Cargo.toml`. The empty `[workspace]` table detaches it from this
/// repo's workspace, and the path dependency points back at the installed
/// `hugr-toolkit` crate (resolved from this binary's compile-time manifest dir).
fn cargo_toml(pkg: &str) -> String {
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

/// The shim's `main.rs` — embed the bundle and delegate to the universal CLI.
const MAIN_RS: &str = r#"// Generated by `hugr build --surface cli`. Do not edit by hand.
static BUNDLE: &[u8] = include_bytes!("../bundle.bin");

#[tokio::main]
async fn main() {
    let code = hugr_toolkit::surface::run_cli(BUNDLE).await;
    std::process::exit(code);
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
    fn cargo_toml_detaches_workspace_and_paths_to_toolkit() {
        let toml = cargo_toml("policy-docs");
        assert!(toml.contains("[workspace]"));
        assert!(toml.contains("hugr-toolkit = { path ="));
        assert!(toml.contains("name = \"policy-docs\""));
    }
}
