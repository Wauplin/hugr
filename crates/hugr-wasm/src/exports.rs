use wasm_bindgen::prelude::*;

use hugr_core::{
    Brain, BudgetPolicy, Command, Decision, Event, ModelOutput, ModelSelector, OpId, StaticPolicy,
    Timestamp, TurnPolicy, Usage,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{BrowserAgentConfig, browser_capabilities, browser_tool_schemas};

#[wasm_bindgen]
pub struct HugrWasm {
    config: BrowserAgentConfig,
    brain: Brain,
    events: Vec<Event>,
    commands: Vec<Command>,
}

#[wasm_bindgen]
impl HugrWasm {
    #[wasm_bindgen(constructor)]
    pub fn new(config_json: Option<String>) -> Result<HugrWasm, JsValue> {
        let config = match config_json {
            Some(json) if !json.trim().is_empty() => serde_json::from_str(&json)
                .map_err(|err| JsValue::from_str(&format!("invalid config json: {err}")))?,
            _ => BrowserAgentConfig::default(),
        };
        let mut static_policy = StaticPolicy::new()
            .with_model(ModelSelector::named("default"))
            .with_tools(browser_tool_schemas());
        if !config.system_prompt.is_empty() {
            static_policy = static_policy.with_system_prompt(config.system_prompt.clone());
        }
        let policy: Box<dyn TurnPolicy> = match config.context.compaction.as_str() {
            "truncate" | "summarize" => {
                let mut policy = BudgetPolicy::new(config.context.budget_tokens)
                    .with_trigger_tokens(config.context.trigger_tokens)
                    .with_keep_recent_tokens(config.context.keep_recent_tokens)
                    .with_max_block_tokens(config.context.max_block_tokens)
                    .with_tool_ttl(config.context.tool_ttl.clone())
                    .with_keep_last_per_tool(config.context.keep_last_per_tool.clone())
                    .with_base(static_policy);
                if config.context.compaction == "summarize" {
                    policy = policy.with_summary_selector(ModelSelector::named(
                        config
                            .context
                            .summary_model
                            .clone()
                            .unwrap_or_else(|| "default".to_string()),
                    ));
                }
                Box::new(policy)
            }
            _ => Box::new(static_policy),
        };
        Ok(HugrWasm {
            config,
            brain: Brain::new(policy),
            events: Vec::new(),
            commands: Vec::new(),
        })
    }

    pub fn config_json(&self) -> Result<String, JsValue> {
        to_json(&self.config)
    }

    pub fn tool_schemas_json(&self) -> Result<String, JsValue> {
        to_json(&browser_tool_schemas())
    }

    pub fn capabilities_json(&self) -> Result<String, JsValue> {
        to_json(&browser_capabilities())
    }

    pub fn system_prompt(&self) -> String {
        self.config.system_prompt.clone()
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
                reason: reason.unwrap_or_else(|| "denied by user".to_string()),
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

    pub fn trace_json(&self) -> Result<String, JsValue> {
        to_json(&json!({
            "format": "hugr-wasm.trace.v0",
            "events": self.events,
            "log": self.brain.state().log(),
            "commands": self.commands,
        }))
    }

    pub fn final_text(&self) -> String {
        self.brain
            .state()
            .log()
            .iter()
            .rev()
            .find_map(|entry| match &entry.record {
                hugr_core::Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
                    Some(output.text.clone())
                }
                _ => None,
            })
            .unwrap_or_default()
    }
}

#[wasm_bindgen]
pub fn default_config_json() -> Result<String, JsValue> {
    to_json(&BrowserAgentConfig::default())
}

fn to_json<T: serde::Serialize + ?Sized>(value: &T) -> Result<String, JsValue> {
    serde_json::to_string(value)
        .map_err(|err| JsValue::from_str(&format!("failed to serialize json: {err}")))
}

fn from_json<T: DeserializeOwned>(json: &str) -> Result<T, JsValue> {
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

impl HugrWasm {
    fn submit(&mut self, event: Event) {
        self.events.push(event.clone());
        self.brain.submit(event);
    }
}
