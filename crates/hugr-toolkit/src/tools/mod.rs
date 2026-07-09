//! The predefined tool library (ROADMAP T1.2, ARCHITECTURE §20.2).
//!
//! Vetted, parameterized [`Capability`] families selectable by a manifest grant
//! (`[tools.<name>]`). Each documents a privilege label so the manifest is
//! the audit surface: reviewing an agent's blast radius = reading which library
//! tools it grants. A grant that is not present registers no capability, and an
//! unregistered capability cannot be invoked (sandbox-by-registration, §7.1).
//!
//! [`build_library_grant`] turns one `ToolKind::Library` [`ToolGrant`] into the
//! concrete capabilities it registers, resolving relative scope paths against
//! the agent crate folder (`base_dir`). External-tool grants (MCP /
//! agent, §20.3) are handled elsewhere (ROADMAP T1.5 / T3.8).

mod fs_read;
mod web_fetch;

use std::path::Path;
use std::sync::Arc;

use hugr_host::Capability;

pub use fs_read::FsRoot;
pub use web_fetch::WebFetch;

use crate::manifest::{ToolGrant, ToolKind};

/// One predefined-library tool id, its privilege label (an open string set —
/// `read_only` / `scratchpad` / `network` / … — nothing branches on it), and
/// the concrete tool names it registers. This is the catalog
/// `--describe`/docs enumerate.
#[derive(Clone, Copy, Debug)]
pub struct LibraryToolSpec {
    /// The manifest grant key (`fs_read`, `web_fetch`, …).
    pub id: &'static str,
    /// Privilege class for the audit surface.
    pub privilege: &'static str,
    /// The capability names this grant registers.
    pub tools: &'static [&'static str],
    /// One-line description.
    pub summary: &'static str,
}

/// The full predefined tool library (ROADMAP T1.2).
pub const CATALOG: &[LibraryToolSpec] = &[
    LibraryToolSpec {
        id: "fs_read",
        privilege: "read_only",
        tools: &[
            "fs_list",
            "fs_search",
            "fs_read",
            "fs_read_range",
            "fs_read_many",
            "fs_outline",
        ],
        summary: "Root-jailed read-only filesystem access (list/search/read/outline).",
    },
    LibraryToolSpec {
        id: "scratchpad",
        privilege: "scratchpad",
        // Provided by the agent runtime itself (T0.4); the grant is an audit
        // marker, not a capability constructed here.
        tools: &["scratch_read", "scratch_write", "scratch_list"],
        summary: "Per-lineage scratch directory (read/write/list).",
    },
    LibraryToolSpec {
        id: "web_fetch",
        privilege: "network",
        tools: &["web_fetch"],
        summary: "Host/method-allowlisted HTTP fetch (GET-only by default).",
    },
];

/// Look up a library tool's spec by grant id.
pub fn spec(id: &str) -> Option<&'static LibraryToolSpec> {
    CATALOG.iter().find(|s| s.id == id)
}

/// Failure to construct a granted library tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The grant names a tool not in the library.
    #[error("unknown library tool `{0}` (not in the predefined tool library)")]
    Unknown(String),
    /// The grant is for an external tool kind handled elsewhere (§20.3).
    #[error("`{0}` is an external tool grant, not a library tool")]
    NotLibrary(String),
    /// The grant's scope/config is invalid.
    #[error("configuring library tool `{tool}`: {source}")]
    Config {
        tool: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Build the capabilities a single library grant registers. Relative scope
/// paths resolve against `base_dir` (the agent crate folder). The `scratchpad`
/// grant returns an empty vec — the agent runtime provides those tools (T0.4);
/// the grant is recorded for audit only.
pub fn build_library_grant(
    grant: &ToolGrant,
    base_dir: &Path,
) -> Result<Vec<Arc<dyn Capability>>, ToolError> {
    if grant.kind != ToolKind::Library {
        return Err(ToolError::NotLibrary(grant.name.clone()));
    }
    let cfg = |source: anyhow::Error| ToolError::Config {
        tool: grant.name.clone(),
        source,
    };
    match grant.name.as_str() {
        "fs_read" => {
            let root = grant
                .config
                .get("root")
                .and_then(|v| v.as_str())
                .unwrap_or(".");
            let resolved = {
                let p = Path::new(root);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    base_dir.join(p)
                }
            };
            let fs_root = FsRoot::new(&resolved).map_err(cfg)?;
            Ok(fs_root.capabilities())
        }
        "web_fetch" => {
            let tool = WebFetch::from_config(&grant.config).map_err(cfg)?;
            Ok(vec![Arc::new(tool)])
        }
        // Provided by the agent runtime (T0.4). Recognized for audit; registers
        // nothing here.
        "scratchpad" => Ok(Vec::new()),
        other => Err(ToolError::Unknown(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grant(name: &str, config: serde_json::Value) -> ToolGrant {
        ToolGrant {
            name: name.to_string(),
            kind: ToolKind::Library,
            config,
        }
    }

    #[test]
    fn catalog_covers_the_v1_library() {
        let ids: Vec<_> = CATALOG.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["fs_read", "scratchpad", "web_fetch"]);
    }

    #[test]
    fn unknown_grant_errors() {
        // `dyn Capability` is not Debug, so match rather than unwrap_err.
        let result = build_library_grant(&grant("nope", json!({})), Path::new("."));
        assert!(matches!(result, Err(ToolError::Unknown(_))));
    }

    #[test]
    fn scratchpad_grant_registers_nothing_here() {
        let caps = build_library_grant(&grant("scratchpad", json!({})), Path::new(".")).unwrap();
        assert!(caps.is_empty());
        // But it is a recognized, audited grant.
        assert_eq!(spec("scratchpad").unwrap().privilege, "scratchpad");
    }

    #[test]
    fn web_fetch_builds_and_reports_network_class() {
        let caps = build_library_grant(
            &grant("web_fetch", json!({ "allow_hosts": ["example.com"] })),
            Path::new("."),
        )
        .unwrap();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].name(), "web_fetch");
        assert!(!caps[0].requires_permission());
        assert_eq!(spec("web_fetch").unwrap().privilege, "network");
    }
}
