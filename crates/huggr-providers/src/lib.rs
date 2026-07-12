//! # huggr-providers — model adapters
//!
//! Provider adapters translate the canonical [`ModelRequest`](huggr_core::ModelRequest) / [`ModelOutput`](huggr_core::ModelOutput) to/from a concrete provider's wire format, streaming deltas back through a [`ModelSink`](huggr_host::ModelSink).

mod openai;

pub use openai::OpenAiAdapter;
