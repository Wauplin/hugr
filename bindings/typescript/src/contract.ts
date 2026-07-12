// Typed mirrors of the Huggr JSON contract. Field names are identical to the
// wire form; validation stays on the Rust side — these are deserialization
// shapes, never a second validator.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export const STATUS_SUCCESS = "success";
export const STATUS_ERROR = "error";

export interface AnswerMeta {
  duration_ms: number;
  cost_micro_usd: number;
  tokens_in: number;
  tokens_out: number;
  model_calls: number;
  tool_calls: number;
}

export interface BlobHandle {
  ref: { kind: "bytes"; base64: string } | { kind: "path"; path: string } | { kind: "sha256"; sha256: string };
  media_type: string;
  name?: string;
}

export interface Ask {
  question: string;
  trace_id?: string;
  blobs?: BlobHandle[];
  skills?: string[];
  extra?: Json;
}

export interface Answer {
  status: string;
  response: Record<string, Json>;
  trace_id: string;
  blobs?: BlobHandle[];
  metadata: AnswerMeta;
  extra?: Json;
}

export interface Usage {
  input_tokens: number;
  output_tokens: number;
  extra: Json;
}

export interface ToolCall {
  id: string;
  name: string;
  args: Json;
}

export interface ModelOutput {
  text: string;
  reasoning?: string | null;
  tool_calls: ToolCall[];
  stop: string;
}

/// The shared event vocabulary (same wire shapes as Rust `AgentEvent` and the
/// Python `agent.run(...)` events).
export type AgentEvent =
  | { type: "ask_started"; trace_parent: string | null }
  | { type: "model_started"; op: number; tier: string }
  | { type: "text_delta"; op: number; text: string }
  | { type: "model_ended"; op: number; usage: Usage }
  | { type: "tool_started"; op: number; name: string; args: Json }
  | { type: "tool_ended"; op: number; name: string; is_error: boolean; result: Json }
  | { type: "notice"; message: string }
  | { type: "done"; reason: Json }
  | { type: "answer_ready"; answer: Answer };

/// One model-invocable tool: explicit name/description/JSON-schema plus the
/// invoke function. The function is trusted host code — Huggr jails what the
/// model can invoke, not what your TS does once invoked.
export interface ToolSpec {
  name: string;
  description: string;
  schema: Json;
  requiresPermission?: boolean;
  invoke(args: Json, signal?: AbortSignal): Promise<Json> | Json;
}

/// One `[models.<tier>]`-shaped entry (same keys as the manifest).
export interface TierConfig {
  model: string;
  input_usd_per_m_tokens?: number;
  output_usd_per_m_tokens?: number;
}

/// The `[models]`-shaped block: provider knobs plus one tier per other key.
export interface ModelsConfig {
  base_url?: string;
  api_key?: string;
  api_key_env?: string;
  default?: string;
  [tier: string]: TierConfig | string | undefined;
}

export interface LimitsConfig {
  max_model_calls?: number;
  max_cost_micro_usd?: number;
  timeout_s?: number;
}

/// The `[context]`-shaped block, passed through to the core `BudgetPolicy`.
export interface ContextConfig {
  compaction?: "none" | "truncate" | "summarize";
  budget_tokens?: number;
  trigger_tokens?: number;
  keep_recent_tokens?: number;
  max_block_tokens?: number;
  summary_model?: string;
  tool_ttl?: Record<string, number>;
  keep_last_per_tool?: Record<string, number>;
}

export interface AgentConfig {
  name: string;
  version?: string;
  description?: string;
  system?: string;
  models: ModelsConfig;
  tools?: ToolSpec[];
  limits?: LimitsConfig;
  context?: ContextConfig;
}

export interface AskOptions {
  traceId?: string;
  extra?: Json;
  signal?: AbortSignal;
}

/// Header stamped into a trace's meta when it is persisted.
export interface TraceHeader {
  agent_name: string;
  agent_version: string;
  question: string;
  status: string;
  depends_on?: string;
  extra?: Json;
}

export interface TraceHead {
  trace_id: string;
  depends_on: string | null;
  agent_name: string;
  agent_version: string;
  created_at: number | null;
  question: string;
  status: string;
}

/// Storage seam for persisted traces. `put` stamps the header + a
/// content-derived id into the trace's meta and returns the id; traces are
/// immutable — a put never overwrites.
export interface TraceStore {
  put(trace: Json, header: TraceHeader): Promise<string>;
  get(id: string): Promise<Json>;
  list(): Promise<TraceHead[]>;
}

export interface Feedback {
  trace_id: string;
  payload: Json;
  created_at_ms: number;
}

/// Append-only feedback sidecar, keyed to a trace, never in it.
export interface FeedbackStore {
  append(feedback: Feedback): Promise<void>;
  list(traceId: string): Promise<Feedback[]>;
}
