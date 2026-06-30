//! Front-ends consume the brain's [`OutputEvent`] stream plus host lifecycle
//! hooks (DESIGN §8).
//!
//! Rendering is never inside the core; any number of front-ends can subscribe.
//! Phase 1 ships a streaming, ANSI-colored stdout front-end that also surfaces
//! "under the hood" activity (model calls, tool calls + results, permission
//! decisions, token usage) so a user can follow what the agent is doing.

use std::io::{IsTerminal, Write};

use baton_core::{Decision, DoneReason, ModelSelector, OpId, OutputEvent, Usage, Value};

/// Renders the agent's output and activity. The [`Engine`](crate::Engine) calls
/// these as it drains commands and observes the event stream. Every method has
/// a default no-op, so a minimal front-end only implements what it cares about.
#[allow(unused_variables)]
pub trait Frontend: Send {
    /// A cosmetic output event from the brain (streamed text, tool chunks, …).
    fn on_output(&mut self, event: &OutputEvent) {}

    /// A host-level notice (free-form).
    fn on_notice(&mut self, message: &str) {}

    /// A model call is starting.
    fn on_model_start(&mut self, op: OpId, selector: &ModelSelector) {}

    /// A model call finished; carries its token usage.
    fn on_model_end(&mut self, op: OpId, usage: &Usage) {}

    /// A capability (tool) is about to run, with its arguments.
    fn on_tool_start(&mut self, op: OpId, name: &str, args: &Value) {}

    /// A capability finished. `is_error` marks a tool-level failure (the result
    /// is the error payload).
    fn on_tool_end(&mut self, op: OpId, name: &str, result: &Value, is_error: bool) {}

    /// A permission request was decided.
    fn on_permission(&mut self, capability: &str, decision: &Decision) {}

    /// The turn reached a terminal state.
    fn on_done(&mut self, reason: &DoneReason) {}
}

// --- ANSI styling -----------------------------------------------------------

mod style {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GRAY: &str = "\x1b[90m";
}

/// Streams assistant text to stdout as it arrives, and renders agent activity
/// with ANSI colors. Colors are auto-disabled when stdout is not a TTY or when
/// `NO_COLOR` is set.
pub struct StdoutFrontend {
    color: bool,
    /// Whether we're mid-line streaming assistant text (so the next log line
    /// inserts a newline first).
    streaming: bool,
}

impl Default for StdoutFrontend {
    fn default() -> Self {
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Self {
            color,
            streaming: false,
        }
    }
}

impl StdoutFrontend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Force colors on or off, overriding TTY/`NO_COLOR` detection.
    pub fn with_color(mut self, color: bool) -> Self {
        self.color = color;
        self
    }

    /// Wrap `text` in an ANSI code when colors are enabled.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{}", style::RESET)
        } else {
            text.to_string()
        }
    }

    fn break_stream(&mut self) {
        if self.streaming {
            println!();
            self.streaming = false;
        }
    }

    /// Print one activity line (breaking any in-progress streamed text first).
    fn line(&mut self, text: String) {
        self.break_stream();
        println!("{text}");
    }
}

impl Frontend for StdoutFrontend {
    fn on_output(&mut self, event: &OutputEvent) {
        match event {
            OutputEvent::ModelText { text, .. } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                self.streaming = true;
            }
            OutputEvent::ToolChunk { chunk, .. } => {
                self.break_stream();
                let text = chunk
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| chunk.to_string());
                print!("{}", self.paint(style::GRAY, &text));
                let _ = std::io::stdout().flush();
            }
            // Tool starts are reported richly via `on_tool_start`; reasoning is
            // hidden in this minimal front-end.
            OutputEvent::ToolCallStarted { .. } | OutputEvent::ModelReasoning { .. } => {}
            OutputEvent::Notice(msg) => self.line(self.paint(style::GRAY, msg)),
            _ => {}
        }
    }

    fn on_notice(&mut self, message: &str) {
        let line = self.paint(style::GRAY, message);
        self.line(line);
    }

    fn on_model_start(&mut self, op: OpId, selector: &ModelSelector) {
        let name = match selector {
            ModelSelector::Named(name) => name.as_str(),
            _ => "?",
        };
        let marker = self.paint(style::CYAN, "●");
        let label = self.paint(style::DIM, &format!("model [{name}] {op}"));
        self.line(format!("{marker} {label}"));
    }

    fn on_model_end(&mut self, _op: OpId, usage: &Usage) {
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            return; // no usage reported by the provider
        }
        let text = format!(
            "  ↳ {} in / {} out tokens",
            usage.input_tokens, usage.output_tokens
        );
        let line = self.paint(style::GRAY, &text);
        self.line(line);
    }

    fn on_tool_start(&mut self, op: OpId, name: &str, args: &Value) {
        let marker = self.paint(style::YELLOW, "⚙");
        let name = self.paint(style::BOLD, name);
        let args = self.paint(style::DIM, &truncate(&compact(args), 160));
        self.line(format!(
            "{marker} {name} {args} {}",
            self.paint(style::GRAY, &op.to_string())
        ));
    }

    fn on_tool_end(&mut self, _op: OpId, name: &str, result: &Value, is_error: bool) {
        let (marker, code) = if is_error {
            ("✗", style::RED)
        } else {
            ("✓", style::GREEN)
        };
        let head = self.paint(code, &format!("  {marker} {name}"));
        let summary = self.paint(
            style::GRAY,
            &format!("→ {}", truncate(&compact(result), 160)),
        );
        self.line(format!("{head} {summary}"));
    }

    fn on_permission(&mut self, capability: &str, decision: &Decision) {
        let line = match decision {
            Decision::Allow => self.paint(style::GREEN, &format!("  ↳ allowed `{capability}`")),
            Decision::Deny { reason } => {
                self.paint(style::RED, &format!("  ↳ denied `{capability}`: {reason}"))
            }
            _ => self.paint(style::GRAY, &format!("  ↳ permission for `{capability}`")),
        };
        self.line(line);
    }

    fn on_done(&mut self, reason: &DoneReason) {
        self.break_stream();
        match reason {
            DoneReason::EndTurn => {}
            DoneReason::Cancelled => println!("{}", self.paint(style::YELLOW, "⚠ cancelled")),
            DoneReason::Error(msg) => {
                eprintln!("{}", self.paint(style::RED, &format!("✗ error: {msg}")));
            }
            _ => {}
        }
    }
}

/// Compact one-line JSON for logging.
fn compact(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Truncate to `max` chars (by char boundary), appending an ellipsis if cut.
fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        return s;
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}…")
}
