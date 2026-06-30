//! Externalized permission policy (DESIGN §5.4).
//!
//! The brain emits a `RequestPermission` command; an external, pluggable
//! `Policy` decides allow/deny. The brain's loop is identical whether the host
//! prompts a human, consults an allowlist, or auto-approves.

use async_trait::async_trait;
use baton_core::{Decision, PermissionRequest};

/// Decides whether a gated capability invocation may proceed.
#[async_trait]
pub trait Policy: Send + Sync {
    async fn decide(&self, request: &PermissionRequest) -> Decision;
}

/// Approves everything (the `-y/--yes` mode). Decisions still flow through the
/// brain as events, so they are recorded in the trace.
pub struct AllowAll;

#[async_trait]
impl Policy for AllowAll {
    async fn decide(&self, _request: &PermissionRequest) -> Decision {
        Decision::Allow
    }
}

/// Denies everything (useful for headless/locked-down runs).
pub struct DenyAll;

#[async_trait]
impl Policy for DenyAll {
    async fn decide(&self, _request: &PermissionRequest) -> Decision {
        Decision::Deny {
            reason: "denied by policy".to_string(),
        }
    }
}

/// Prompts the user on the terminal for each gated capability (`y/N`).
pub struct Interactive;

#[async_trait]
impl Policy for Interactive {
    async fn decide(&self, request: &PermissionRequest) -> Decision {
        let capability = request.capability.clone();
        let args = request.args.clone();
        // Reading stdin is blocking; keep it off the async runtime threads.
        let answer = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let pretty = serde_json::to_string(&args).unwrap_or_default();
            print!("\n⚠  allow `{capability}` with args {pretty}? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            line.trim().to_lowercase()
        })
        .await
        .unwrap_or_default();

        if answer == "y" || answer == "yes" {
            Decision::Allow
        } else {
            Decision::Deny {
                reason: "denied by user".to_string(),
            }
        }
    }
}
