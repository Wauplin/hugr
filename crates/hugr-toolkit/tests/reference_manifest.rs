//! The committed reference manifest must parse cleanly with zero warnings —
//! it is the documented example every `hugr new` template and doc snippet is
//! measured against (ROADMAP T1.1).

use hugr_toolkit::{AgentDefinition, ToolKind};

const REFERENCE: &str = include_str!("../reference/hugr.toml");

#[test]
fn reference_manifest_parses_without_warnings() {
    let def = AgentDefinition::parse(REFERENCE, "reference/hugr.toml").unwrap();
    assert_eq!(def.agent.name, "policy-docs");
    assert_eq!(def.agent.version, "0.1.0");
    assert!(!def.agent.description.is_empty());

    assert_eq!(
        def.models.base_url.as_deref(),
        Some("https://router.huggingface.co/v1")
    );
    assert_eq!(
        def.models.api_key_env.as_deref(),
        Some("POLICY_DOCS_API_KEY")
    );
    assert_eq!(def.default_tier(), Some("medium"));
    let medium = &def.models.tiers["medium"];
    assert_eq!(medium.input_usd_per_m_tokens, Some(1.0));
    assert_eq!(medium.max_tokens, Some(2048));

    // Only fs_read is uncommented in the reference.
    assert_eq!(def.tools.len(), 1);
    assert_eq!(def.tools[0].kind, ToolKind::Library);
    assert_eq!(def.tools[0].name, "fs_read");
    assert_eq!(def.tools[0].config["root"], "./policies");

    assert_eq!(def.limits.max_model_calls, Some(20));
    assert_eq!(def.limits.timeout_s, Some(120));
}
