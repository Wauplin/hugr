//! The committed reference manifest must parse cleanly with zero warnings.

use huggr_toolkit::{AgentDefinition, ToolKind};

const REFERENCE: &str = include_str!("../reference/huggr.toml");

#[test]
fn reference_manifest_parses_without_warnings() {
    let def = AgentDefinition::parse(REFERENCE, "reference/huggr.toml").unwrap();
    assert_eq!(def.agent.name, "policy-docs");
    assert_eq!(def.agent.version, "0.1.0");
    assert!(!def.agent.description.is_empty());

    assert_eq!(def.providers["hf"].api_key_env, "HF_TOKEN");
    assert_eq!(def.default_tier(), Some("powerful"));
    let powerful = &def.models.tiers["powerful"];
    assert_eq!(powerful.input_usd_per_m_tokens, Some(0.037));

    // Only fs_read is uncommented in the reference.
    assert_eq!(def.tools.len(), 1);
    assert_eq!(def.tools[0].kind, ToolKind::Library);
    assert_eq!(def.tools[0].name, "fs_read");
    assert_eq!(def.tools[0].config["root"], "./policies");

    // Limits are opt-in; the reference documents them commented out.
    assert_eq!(def.limits.max_model_calls, None);
    assert_eq!(def.limits.max_cost_micro_usd, None);
    assert_eq!(def.limits.timeout_s, None);
}
