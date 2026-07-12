//! The generic agent session for JS hosts: brain + recorder over JSON, with
//! traces in the portable `huggr-replay` format so a TS-recorded session
//! verifies with the Rust CLI (and vice versa).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasm_bindgen::prelude::*;

use huggr_core::{
    Brain, BudgetPolicy, Command, Decision, Event, ModelOutput, ModelSelector, OpId, StaticPolicy,
    Timestamp, ToolSchema, TurnPolicy, Usage,
};
use huggr_replay::Trace;

/// Session configuration passed by the JS host. `tools` are the advertised
/// schemas (the host owns the implementations); `context` mirrors the
/// manifest's `[context]` keys and defaults to no compaction.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionConfig {
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub tools: Vec<ToolSchema>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub context: Option<SessionContextConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionContextConfig {
    #[serde(default)]
    pub compaction: Option<String>,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
    #[serde(default)]
    pub trigger_tokens: Option<u32>,
    #[serde(default)]
    pub keep_recent_tokens: Option<u32>,
    #[serde(default)]
    pub max_block_tokens: Option<u32>,
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default)]
    pub tool_ttl: BTreeMap<String, u32>,
    #[serde(default)]
    pub keep_last_per_tool: BTreeMap<String, u32>,
}

#[wasm_bindgen]
pub struct AgentSession {
    brain: Brain,
    policy_config: Value,
    events: Vec<Event>,
    commands: Vec<Command>,
    resume_baseline: usize,
}

#[wasm_bindgen]
impl AgentSession {
    #[wasm_bindgen(constructor)]
    pub fn new(config_json: &str) -> Result<AgentSession, JsValue> {
        let config: SessionConfig = from_json(config_json)?;
        let selector = config
            .default_model
            .clone()
            .unwrap_or_else(|| "medium".to_string());
        let mut static_policy = StaticPolicy::new()
            .with_model(ModelSelector::named(selector))
            .with_tools(config.tools.clone());
        if !config.system_prompt.is_empty() {
            static_policy = static_policy.with_system_prompt(config.system_prompt.clone());
        }
        let context = config.context.clone().unwrap_or_default();
        let compaction = context.compaction.as_deref().unwrap_or("none");
        let (policy, policy_config): (Box<dyn TurnPolicy>, Value) = if compaction == "none" {
            let value = to_value(&static_policy)?;
            (Box::new(static_policy), value)
        } else {
            let mut policy = BudgetPolicy::new(context.budget_tokens.unwrap_or(128_000))
                .with_tool_ttl(context.tool_ttl.clone())
                .with_keep_last_per_tool(context.keep_last_per_tool.clone())
                .with_base(static_policy);
            if let Some(v) = context.trigger_tokens {
                policy = policy.with_trigger_tokens(v);
            }
            if let Some(v) = context.keep_recent_tokens {
                policy = policy.with_keep_recent_tokens(v);
            }
            if let Some(v) = context.max_block_tokens {
                policy = policy.with_max_block_tokens(v);
            }
            if compaction == "summarize" {
                let selector = context
                    .summary_model
                    .clone()
                    .or(config.default_model)
                    .unwrap_or_else(|| "medium".to_string());
                policy = policy.with_summary_selector(ModelSelector::named(selector));
            }
            let value = to_value(&policy)?;
            (Box::new(policy), value)
        };
        Ok(AgentSession {
            brain: Brain::new(policy),
            policy_config,
            events: Vec::new(),
            commands: Vec::new(),
            resume_baseline: 0,
        })
    }

    /// Re-fold a parent trace into this fresh session (resume/fork): every
    /// recorded event is re-submitted and the re-derived commands recorded, so
    /// the *new* trace persists the full history and still verifies. No model
    /// or tool ever re-runs. Call before the first user turn.
    pub fn resume_trace(&mut self, trace_json: &str) -> Result<(), JsValue> {
        if !self.events.is_empty() {
            return Err(JsValue::from_str(
                "resume_trace must precede the first turn",
            ));
        }
        let trace = Trace::from_json(trace_json.as_bytes())
            .map_err(|err| JsValue::from_str(&format!("invalid trace: {err}")))?;
        for event in trace.events {
            self.events.push(event.clone());
            self.brain.submit(event);
            let commands = self.brain.poll();
            self.commands.extend(commands);
        }
        self.resume_baseline = self.brain.state().log().len();
        Ok(())
    }

    /// The log index where this session's *new* work starts (0 for a fresh
    /// session) — the accounting baseline, so a resumed ask never re-bills its
    /// ancestry.
    pub fn log_baseline(&self) -> usize {
        self.resume_baseline
    }

    pub fn submit_user_input(&mut self, text: String, now_ms: f64) -> Result<String, JsValue> {
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::UserInput {
            est_tokens: estimate_tokens(&text),
            content: Value::String(text),
        });
        self.poll_commands_json()
    }

    pub fn submit_model_done(
        &mut self,
        op: f64,
        output_json: String,
        usage_json: String,
        est_tokens: u32,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let output: ModelOutput = from_json(&output_json)?;
        let usage: Usage = from_json(&usage_json)?;
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::ModelDone {
            op: op_id(op),
            output,
            usage,
            est_tokens,
        });
        self.poll_commands_json()
    }

    pub fn submit_model_error(
        &mut self,
        op: f64,
        error_json: String,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let error: Value = from_json(&error_json)?;
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::ModelError {
            op: op_id(op),
            error,
        });
        self.poll_commands_json()
    }

    pub fn submit_capability_done(
        &mut self,
        op: f64,
        result_json: String,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let result: Value = from_json(&result_json)?;
        let est_tokens = estimate_tokens(&result.to_string());
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::CapabilityDone {
            op: op_id(op),
            result,
            est_tokens,
        });
        self.poll_commands_json()
    }

    pub fn submit_capability_error(
        &mut self,
        op: f64,
        error_json: String,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let error: Value = from_json(&error_json)?;
        let est_tokens = estimate_tokens(&error.to_string());
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::CapabilityError {
            op: op_id(op),
            error,
            est_tokens,
        });
        self.poll_commands_json()
    }

    pub fn submit_op_cancelled(&mut self, op: f64, now_ms: f64) -> Result<String, JsValue> {
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::OpCancelled { op: op_id(op) });
        self.poll_commands_json()
    }

    pub fn submit_permission_decision(
        &mut self,
        op: f64,
        allow: bool,
        reason: Option<String>,
        now_ms: f64,
    ) -> Result<String, JsValue> {
        let decision = if allow {
            Decision::Allow
        } else {
            Decision::Deny {
                reason: reason.unwrap_or_else(|| "denied by host".to_string()),
            }
        };
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::PermissionDecision {
            op: op_id(op),
            decision,
            est_tokens: 8,
        });
        self.poll_commands_json()
    }

    pub fn abort(&mut self, now_ms: f64) -> Result<String, JsValue> {
        self.submit(Event::Tick {
            now: timestamp(now_ms),
        });
        self.submit(Event::UserAbort);
        self.poll_commands_json()
    }

    pub fn poll_commands_json(&mut self) -> Result<String, JsValue> {
        let commands = self.brain.poll();
        self.commands.extend(commands.clone());
        to_json(&commands)
    }

    pub fn log_json(&self) -> Result<String, JsValue> {
        to_json(self.brain.state().log())
    }

    /// The session as a portable `huggr-replay` trace (meta unstamped — the
    /// host's trace store stamps id/header fields when it persists).
    pub fn trace_json(&self) -> Result<String, JsValue> {
        let created_at = self.events.iter().find_map(|event| match event {
            Event::Tick { now } => Some(now.0),
            _ => None,
        });
        let trace = Trace::new(
            self.events.clone(),
            self.brain.state().log().to_vec(),
            created_at,
        )
        .with_commands(self.commands.clone())
        .with_policy(self.policy_config.clone());
        let bytes = trace
            .to_json()
            .map_err(|err| JsValue::from_str(&format!("trace serialization failed: {err}")))?;
        String::from_utf8(bytes).map_err(|err| JsValue::from_str(&err.to_string()))
    }

    pub fn final_text(&self) -> String {
        self.brain
            .state()
            .log()
            .iter()
            .rev()
            .find_map(|entry| match &entry.record {
                huggr_core::Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
                    Some(output.text.clone())
                }
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// Verify a stored trace replays bit-for-bit — the same gate as `huggr verify`,
/// callable from JS on traces recorded by any surface.
#[wasm_bindgen]
pub fn verify_trace_json(trace_json: &str) -> Result<(), JsValue> {
    let trace = Trace::from_json(trace_json.as_bytes())
        .map_err(|err| JsValue::from_str(&format!("invalid trace: {err}")))?;
    huggr_replay::verify(&trace)
        .map(|_| ())
        .map_err(|err| JsValue::from_str(&format!("verify failed: {err}")))
}

fn to_json<T: Serialize + ?Sized>(value: &T) -> Result<String, JsValue> {
    serde_json::to_string(value)
        .map_err(|err| JsValue::from_str(&format!("failed to serialize json: {err}")))
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, JsValue> {
    serde_json::to_value(value)
        .map_err(|err| JsValue::from_str(&format!("failed to serialize policy: {err}")))
}

fn from_json<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, JsValue> {
    serde_json::from_str(json).map_err(|err| JsValue::from_str(&format!("invalid json: {err}")))
}

fn estimate_tokens(text: &str) -> u32 {
    ((text.chars().count() as u32).saturating_add(3) / 4).max(1)
}

fn timestamp(value: f64) -> Timestamp {
    Timestamp(f64_to_u64(value))
}

fn op_id(value: f64) -> OpId {
    OpId(f64_to_u64(value))
}

fn f64_to_u64(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        value.trunc() as u64
    } else {
        0
    }
}

impl AgentSession {
    fn submit(&mut self, event: Event) {
        self.events.push(event.clone());
        self.brain.submit(event);
    }
}
