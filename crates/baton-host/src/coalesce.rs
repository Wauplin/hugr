//! Host-side delta **coalescing** for the front-end render path
//! (ARCHITECTURE §4.4 / §4.5).
//!
//! A model response of thousands of tokens arrives as thousands of
//! `ModelDelta` events; rendering each one individually means a `print!` +
//! `flush` per token. The host may instead **batch the render**: merge
//! consecutive streamed text chunks for the same op into one larger
//! [`OutputEvent`] before handing it to the [`Frontend`](crate::Frontend).
//!
//! Two invariants make this safe (and keep replay bit-for-bit):
//!
//! - **Coalescing is a *render* optimization only.** It lives entirely between
//!   [`Command::Emit`](baton_core::Command::Emit) and
//!   [`Frontend::on_output`](crate::Frontend::on_output). It never touches the
//!   brain's event stream — every `ModelDelta` is still `submit`ted to the
//!   brain, so `text_so_far` stays complete and a cancelled op's partial loses
//!   no tokens (the cancellation partial is read from the brain's buffer, not
//!   from anything here). See `Engine::observe_output`.
//! - **Deltas are transport, never durable.** Nothing here writes to the log;
//!   the consolidated `Record` is appended by the brain from `ModelDone`,
//!   independent of how the render was batched. Replay therefore reproduces an
//!   identical command sequence and log regardless of coalescing.
//!
//! The coalescer accumulates only *consecutive same-op text* (`ModelText` /
//! `ModelReasoning`). Any other event — a different op, a tool chunk, a tool
//! start, a notice — first **flushes** the pending buffer (so ordering is
//! preserved), then renders itself. Callers must also [`flush`](Coalescer::flush)
//! at op/turn boundaries (e.g. before `on_model_end` / `on_done`) so buffered
//! text reaches the screen in order.

use baton_core::{OpId, OutputEvent};

/// Which kind of streamed text is buffered (they render via different
/// `OutputEvent` variants, so we must not merge across kinds).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Text,
    Reasoning,
}

/// Buffers consecutive same-op streamed text and emits merged
/// [`OutputEvent`]s, cutting per-token render churn. Pure and IO-free: it takes
/// `OutputEvent`s in and yields the (possibly merged) `OutputEvent`s a
/// front-end should render, so it is fully unit-testable without stdout.
#[derive(Default)]
pub struct Coalescer {
    /// The op whose text is currently buffered, with its kind, if any.
    pending: Option<(OpId, TextKind)>,
    buf: String,
}

impl Coalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one cosmetic [`OutputEvent`] in; returns the events the front-end
    /// should render now (zero, one, or — when a buffer is flushed ahead of a
    /// non-text event — two). Streamed text for the same op is accumulated and
    /// withheld until a boundary, so a long token stream renders as a few large
    /// chunks instead of thousands of tiny ones.
    pub fn push(&mut self, event: OutputEvent) -> Vec<OutputEvent> {
        match event {
            OutputEvent::ModelText { op, text } => self.push_text(op, TextKind::Text, text),
            OutputEvent::ModelReasoning { op, text } => {
                self.push_text(op, TextKind::Reasoning, text)
            }
            // Any non-text event is a boundary: flush buffered text first (to
            // preserve ordering), then pass the event through unchanged.
            other => {
                let mut out = self.flush();
                out.push(other);
                out
            }
        }
    }

    /// Accumulate a text chunk, flushing first if it belongs to a different op
    /// or a different text kind than what is currently buffered.
    fn push_text(&mut self, op: OpId, kind: TextKind, text: String) -> Vec<OutputEvent> {
        let mut out = Vec::new();
        if self.pending != Some((op, kind)) {
            // Switching op/kind: emit what we had so far, then start fresh.
            out.extend(self.flush());
            self.pending = Some((op, kind));
        }
        self.buf.push_str(&text);
        out
    }

    /// Emit any buffered text as a single merged [`OutputEvent`] and clear the
    /// buffer. Returns an empty vec when nothing is buffered. Call this at op /
    /// turn boundaries so withheld text reaches the front-end in order.
    pub fn flush(&mut self) -> Vec<OutputEvent> {
        match self.pending.take() {
            Some((op, kind)) if !self.buf.is_empty() => {
                let text = std::mem::take(&mut self.buf);
                let event = match kind {
                    TextKind::Text => OutputEvent::ModelText { op, text },
                    TextKind::Reasoning => OutputEvent::ModelReasoning { op, text },
                };
                vec![event]
            }
            _ => {
                self.buf.clear();
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baton_core::OpId;

    fn text(op: u64, t: &str) -> OutputEvent {
        OutputEvent::ModelText {
            op: OpId(op),
            text: t.to_string(),
        }
    }

    /// Concatenate the text of every `ModelText`/`ModelReasoning` event, in
    /// order — the only thing the user actually sees.
    fn rendered_text(events: &[OutputEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                OutputEvent::ModelText { text, .. } | OutputEvent::ModelReasoning { text, .. } => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// Consecutive same-op text chunks merge into one event on flush.
    #[test]
    fn merges_consecutive_text() {
        let mut c = Coalescer::new();
        assert!(c.push(text(1, "Hel")).is_empty());
        assert!(c.push(text(1, "lo, ")).is_empty());
        assert!(c.push(text(1, "world")).is_empty());
        let out = c.flush();
        assert_eq!(out, vec![text(1, "Hello, world")]);
    }

    /// A non-text event flushes the buffer first, preserving order.
    #[test]
    fn non_text_event_flushes_first() {
        let mut c = Coalescer::new();
        c.push(text(1, "abc"));
        let notice = OutputEvent::Notice("hi".into());
        let out = c.push(notice.clone());
        assert_eq!(out, vec![text(1, "abc"), notice]);
        // Buffer is now empty.
        assert!(c.flush().is_empty());
    }

    /// Switching op flushes the previous op's text before buffering the new one.
    #[test]
    fn switching_op_flushes() {
        let mut c = Coalescer::new();
        c.push(text(1, "one"));
        let out = c.push(text(2, "two"));
        assert_eq!(out, vec![text(1, "one")]);
        assert_eq!(c.flush(), vec![text(2, "two")]);
    }

    /// Text and reasoning are different render kinds and never merge.
    #[test]
    fn text_and_reasoning_do_not_merge() {
        let mut c = Coalescer::new();
        c.push(OutputEvent::ModelText {
            op: OpId(1),
            text: "ans".into(),
        });
        let out = c.push(OutputEvent::ModelReasoning {
            op: OpId(1),
            text: "think".into(),
        });
        assert_eq!(out, vec![text(1, "ans")]);
    }

    /// Flushing with nothing buffered is a no-op.
    #[test]
    fn empty_flush_is_noop() {
        let mut c = Coalescer::new();
        assert!(c.flush().is_empty());
    }

    /// **The headline property:** no matter how a token stream is chopped into
    /// chunks, the *rendered* text (in order) is identical. This is what makes
    /// coalescing invisible to the user and irrelevant to replay.
    #[test]
    fn rendered_text_is_chunking_invariant() {
        let full = "The quick brown fox jumps over the lazy dog.";

        // Per-character chunks (worst-case churn).
        let mut a = Coalescer::new();
        let mut out_a = Vec::new();
        for ch in full.chars() {
            out_a.extend(a.push(text(7, &ch.to_string())));
        }
        out_a.extend(a.flush());

        // A few big chunks.
        let mut b = Coalescer::new();
        let mut out_b = Vec::new();
        for chunk in [&full[..10], &full[10..25], &full[25..]] {
            out_b.extend(b.push(text(7, chunk)));
        }
        out_b.extend(b.flush());

        // One chunk.
        let mut d = Coalescer::new();
        let out_d = {
            let mut v = d.push(text(7, full));
            v.extend(d.flush());
            v
        };

        assert_eq!(rendered_text(&out_a), full);
        assert_eq!(rendered_text(&out_b), full);
        assert_eq!(rendered_text(&out_d), full);
        // And coalescing collapsed the per-char churn to a single render event.
        assert_eq!(out_a, vec![text(7, full)]);
    }
}
