//! Front-ends consume the brain's [`OutputEvent`] stream (DESIGN §8).
//!
//! Rendering is never inside the core; any number of front-ends can subscribe.
//! Phase 1 ships a simple streaming stdout front-end.

use std::io::Write;

use baton_core::{DoneReason, OutputEvent};

/// Renders the agent's output. The [`Engine`](crate::Engine) calls these as it
/// drains commands and events.
pub trait Frontend: Send {
    /// A cosmetic output event from the brain (streamed text, tool chunks, …).
    fn on_output(&mut self, event: &OutputEvent);

    /// A host-level notice (e.g. "running shell", a checkpoint).
    fn on_notice(&mut self, message: &str);

    /// The turn reached a terminal state.
    fn on_done(&mut self, reason: &DoneReason);
}

/// Streams assistant text to stdout as it arrives; prints tool activity and
/// notices on their own lines.
#[derive(Default)]
pub struct StdoutFrontend {
    /// Whether we're mid-line streaming assistant text (so notices insert a
    /// newline first).
    streaming: bool,
}

impl StdoutFrontend {
    pub fn new() -> Self {
        Self::default()
    }

    fn break_stream(&mut self) {
        if self.streaming {
            println!();
            self.streaming = false;
        }
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
                if let Some(s) = chunk.as_str() {
                    print!("{s}");
                } else {
                    print!("{chunk}");
                }
                let _ = std::io::stdout().flush();
            }
            OutputEvent::ToolCallStarted { name, .. } => {
                self.break_stream();
                println!("  → tool: {name}");
            }
            OutputEvent::ModelReasoning { .. } => {
                // Reasoning is hidden by default in this minimal front-end.
            }
            OutputEvent::Notice(msg) => {
                self.break_stream();
                println!("{msg}");
            }
            // Forward-compatible: ignore output kinds this front-end doesn't render.
            _ => {}
        }
    }

    fn on_notice(&mut self, message: &str) {
        self.break_stream();
        println!("{message}");
    }

    fn on_done(&mut self, reason: &DoneReason) {
        self.break_stream();
        if let DoneReason::Error(msg) = reason {
            eprintln!("error: {msg}");
        }
    }
}
