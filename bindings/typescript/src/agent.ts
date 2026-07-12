// The TypeScript runtime embedding: an Agent assembled from TS data, driving
// the WASM brain. The host side of every effect (model fetch, tool invoke,
// storage) lives here; the brain stays sans-IO inside the wasm module.

import type {
  AgentConfig,
  AgentEvent,
  Answer,
  AnswerMeta,
  AskOptions,
  Feedback,
  FeedbackStore,
  Json,
  ModelsConfig,
  TierConfig,
  ToolSpec,
  TraceHead,
  TraceHeader,
  TraceStore,
} from "./contract.js";
import { STATUS_ERROR, STATUS_SUCCESS } from "./contract.js";
import { callOpenAiCompatible } from "./openai.js";

/// The wasm-bindgen surface the Agent drives (crates/huggr-wasm `AgentSession`).
export interface WasmSessionClass {
  new (configJson: string): WasmSession;
}

export interface WasmSession {
  resume_trace(traceJson: string): void;
  log_baseline(): number;
  submit_user_input(text: string, nowMs: number): string;
  submit_model_done(op: number, outputJson: string, usageJson: string, estTokens: number, nowMs: number): string;
  submit_model_error(op: number, errorJson: string, nowMs: number): string;
  submit_capability_done(op: number, resultJson: string, nowMs: number): string;
  submit_capability_error(op: number, errorJson: string, nowMs: number): string;
  submit_op_cancelled(op: number, nowMs: number): string;
  submit_permission_decision(op: number, allow: boolean, reason: string | null, nowMs: number): string;
  abort(nowMs: number): string;
  poll_commands_json(): string;
  log_json(): string;
  trace_json(): string;
  final_text(): string;
}

export interface WasmModule {
  AgentSession: WasmSessionClass;
  verify_trace_json(traceJson: string): void;
}

/// Host runtime pieces injected per platform (`huggr-agents/node`,
/// `huggr-agents/browser` provide defaults).
export interface AgentRuntime {
  loadWasm(): Promise<WasmModule>;
  traces: TraceStore;
  feedback?: FeedbackStore;
  /// Resolve `models.api_key_env` (Node: process.env; browsers have no env).
  env?: (name: string) => string | undefined;
}

const MODEL_KNOBS = new Set(["base_url", "api_key", "api_key_env", "default"]);

export class Agent {
  readonly config: AgentConfig;
  private readonly runtime: AgentRuntime;
  private readonly tools: Map<string, ToolSpec>;

  constructor(config: AgentConfig, runtime: AgentRuntime) {
    this.config = config;
    this.runtime = runtime;
    this.tools = new Map((config.tools ?? []).map((tool) => [tool.name, tool]));
  }

  async ask(question: string, options: AskOptions = {}): Promise<Answer> {
    let answer: Answer | undefined;
    for await (const event of this.run(question, options)) {
      if (event.type === "answer_ready") answer = event.answer;
    }
    if (!answer) throw new Error("ask finished without an answer");
    return answer;
  }

  /// Stream one ask's events; the final event is `answer_ready`. Same event
  /// vocabulary as the Rust `--stream` surface and Python `agent.run(...)`.
  async *run(question: string, options: AskOptions = {}): AsyncGenerator<AgentEvent> {
    const startedAt = Date.now();
    const wasm = await this.runtime.loadWasm();
    const session = new wasm.AgentSession(JSON.stringify(this.sessionConfig()));
    yield { type: "ask_started", trace_parent: options.traceId ?? null };

    if (options.traceId) {
      const parent = await this.runtime.traces.get(options.traceId);
      session.resume_trace(JSON.stringify(parent));
    }

    const meta: AnswerMeta = { duration_ms: 0, cost_micro_usd: 0, tokens_in: 0, tokens_out: 0, model_calls: 0, tool_calls: 0 };
    const limits = this.config.limits ?? {};
    const deadline = limits.timeout_s ? startedAt + limits.timeout_s * 1000 : null;
    const effectController = new AbortController();
    const abortEffect = () => effectController.abort();
    options.signal?.addEventListener("abort", abortEffect, { once: true });
    const deadlineTimer = deadline === null ? undefined : setTimeout(abortEffect, Math.max(0, deadline - Date.now()));
    let trip: string | null = null;
    let doneReason: Json = null;

    const queue: Json[] = JSON.parse(session.submit_user_input(question, Date.now()));
    let steps = 0;
    while (queue.length > 0) {
      if (options.signal?.aborted && trip === null) {
        trip = "aborted by caller";
        queue.length = 0;
        queue.push(...(JSON.parse(session.abort(Date.now())) as Json[]));
        continue;
      }
      if (deadline !== null && Date.now() > deadline && trip === null) {
        trip = `timeout after ${limits.timeout_s}s`;
        queue.length = 0;
        queue.push(...(JSON.parse(session.abort(Date.now())) as Json[]));
        continue;
      }
      steps += 1;
      if (steps > 1000) throw new Error("stopped after 1000 commands (runaway session)");
      const [kind, payload] = taggedCommand(queue.shift()!);
      switch (kind) {
        case "StartModelCall": {
          const op = payload.op as number;
          const selector = selectorName(payload.model);
          if (limits.max_model_calls !== undefined && meta.model_calls >= limits.max_model_calls && trip === null) {
            trip = `limit: max_model_calls (${limits.max_model_calls}) reached`;
          }
          if (limits.max_cost_micro_usd !== undefined && meta.cost_micro_usd >= limits.max_cost_micro_usd && trip === null) {
            trip = `limit: max_cost_micro_usd (${limits.max_cost_micro_usd}) reached`;
          }
          if (trip !== null) {
            queue.push(...(JSON.parse(session.submit_model_error(op, JSON.stringify({ error: trip }), Date.now())) as Json[]));
            break;
          }
          yield { type: "model_started", op, tier: selector };
          const deltas: AgentEvent[] = [];
          try {
            const result = await callOpenAiCompatible(payload.request as Json, this.settingsFor(selector), {
              onText: (text) => deltas.push({ type: "text_delta", op, text }),
              signal: effectController.signal,
            });
            yield* deltas;
            meta.model_calls += 1;
            meta.tokens_in += result.usage.input_tokens;
            meta.tokens_out += result.usage.output_tokens;
            meta.cost_micro_usd += this.costMicroUsd(selector, result.usage.input_tokens, result.usage.output_tokens);
            yield { type: "model_ended", op, usage: result.usage };
            queue.push(
              ...(JSON.parse(
                session.submit_model_done(
                  op,
                  JSON.stringify(result.output),
                  JSON.stringify(result.usage),
                  estimateTokens(result.output.text),
                  Date.now(),
                ),
              ) as Json[]),
            );
          } catch (error) {
            yield* deltas;
            const message = String((error as Error)?.message ?? error);
            yield { type: "notice", message: `model error: ${message}` };
            queue.push(...(JSON.parse(session.submit_model_error(op, JSON.stringify({ error: message }), Date.now())) as Json[]));
          }
          break;
        }
        case "StartCapability": {
          const op = payload.op as number;
          const name = payload.name as string;
          const args = (payload.args as Json) ?? {};
          yield { type: "tool_started", op, name, args };
          const tool = this.tools.get(name);
          try {
            if (!tool) throw new Error(`unknown tool: ${name}`);
            const result = (await abortable(tool.invoke(args, effectController.signal), effectController.signal)) ?? null;
            meta.tool_calls += 1;
            yield { type: "tool_ended", op, name, is_error: false, result };
            queue.push(...(JSON.parse(session.submit_capability_done(op, JSON.stringify(result), Date.now())) as Json[]));
          } catch (error) {
            const message = String((error as Error)?.message ?? error);
            meta.tool_calls += 1;
            yield { type: "tool_ended", op, name, is_error: true, result: { error: message } };
            queue.push(
              ...(JSON.parse(session.submit_capability_error(op, JSON.stringify({ error: message }), Date.now())) as Json[]),
            );
          }
          break;
        }
        case "RequestPermission": {
          // Registration is the grant: every tool on this agent was registered
          // by the embedding code, so permissioned tools auto-allow.
          const op = payload.op as number;
          queue.push(...(JSON.parse(session.submit_permission_decision(op, true, null, Date.now())) as Json[]));
          break;
        }
        case "Cancel": {
          const op = payload.op as number;
          queue.push(...(JSON.parse(session.submit_op_cancelled(op, Date.now())) as Json[]));
          break;
        }
        case "Emit":
        case "Checkpoint":
          break;
        case "Done": {
          doneReason = (payload.reason as Json) ?? null;
          yield { type: "done", reason: doneReason };
          break;
        }
        default:
          throw new Error(`unknown Huggr command: ${kind}`);
      }
    }

    meta.duration_ms = Date.now() - startedAt;
    const { status, response } = trip
      ? { status: STATUS_ERROR, response: { error: trip } as Record<string, Json> }
      : finalResponse(session.final_text(), doneReason);

    const header: TraceHeader = {
      agent_name: this.config.name,
      agent_version: this.config.version ?? "0.0.0",
      question,
      status,
      extra: options.extra ?? null,
    };
    if (options.traceId) header.depends_on = options.traceId;
    const traceId = await this.runtime.traces.put(JSON.parse(session.trace_json()) as Json, header);

    const answer: Answer = { status, response, trace_id: traceId, metadata: meta };
    if (deadlineTimer !== undefined) clearTimeout(deadlineTimer);
    options.signal?.removeEventListener("abort", abortEffect);
    yield { type: "answer_ready", answer };
  }

  async feedback(traceId: string, payload: Json): Promise<Feedback> {
    const store = this.runtime.feedback;
    if (!store) throw new Error("this runtime has no feedback store");
    await this.runtime.traces.get(traceId); // unknown trace → throws
    const feedback: Feedback = { trace_id: traceId, payload, created_at_ms: Date.now() };
    await store.append(feedback);
    return feedback;
  }

  async feedbackFor(traceId: string): Promise<Feedback[]> {
    const store = this.runtime.feedback;
    if (!store) throw new Error("this runtime has no feedback store");
    return store.list(traceId);
  }

  traces(): Promise<TraceHead[]> {
    return this.runtime.traces.list();
  }

  /// Verify a stored trace replays bit-for-bit — the same gate as `huggr verify`.
  async verify(traceId: string): Promise<void> {
    const wasm = await this.runtime.loadWasm();
    const trace = await this.runtime.traces.get(traceId);
    wasm.verify_trace_json(JSON.stringify(trace));
  }

  private sessionConfig(): Json {
    return {
      system_prompt: this.config.system ?? "",
      tools: (this.config.tools ?? []).map((tool) => ({
        name: tool.name,
        description: tool.description,
        parameters: tool.schema,
      })),
      default_model: defaultTier(this.config.models),
      context: (this.config.context as Json) ?? null,
    };
  }

  private tierConfig(selector: string): TierConfig {
    const tier = this.config.models[selector];
    if (!tier || typeof tier === "string") throw new Error(`unknown model tier: ${selector}`);
    return tier;
  }

  private settingsFor(selector: string) {
    const models = this.config.models;
    const apiKey = models.api_key ?? (models.api_key_env ? (this.runtime.env?.(models.api_key_env) ?? "") : "");
    return {
      baseUrl: models.base_url ?? "https://router.huggingface.co/v1",
      apiKey,
      tier: this.tierConfig(selector),
    };
  }

  private costMicroUsd(selector: string, tokensIn: number, tokensOut: number): number {
    const tier = this.tierConfig(selector);
    const cost = tokensIn * (tier.input_usd_per_m_tokens ?? 0) + tokensOut * (tier.output_usd_per_m_tokens ?? 0);
    return Math.round(cost);
  }
}

function abortable<T>(effect: Promise<T> | T, signal: AbortSignal): Promise<T> {
  if (signal.aborted) return Promise.reject(new DOMException("aborted", "AbortError"));
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(new DOMException("aborted", "AbortError"));
    signal.addEventListener("abort", abort, { once: true });
    Promise.resolve(effect).then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

function defaultTier(models: ModelsConfig): string {
  if (models.default) return models.default;
  const tiers = Object.keys(models).filter((key) => !MODEL_KNOBS.has(key));
  if (tiers.includes("medium")) return "medium";
  if (tiers.length === 0) throw new Error("models config declares no tier");
  tiers.sort();
  return tiers[0];
}

function selectorName(selector: Json): string {
  if (typeof selector === "string") return selector;
  if (Array.isArray(selector) && typeof selector[0] === "string") return selector[0];
  return String(selector);
}

function taggedCommand(value: Json): [string, Record<string, Json>] {
  if (typeof value === "string") return [value, {}];
  const entries = Object.entries((value as Record<string, Json>) ?? {});
  if (entries.length !== 1) throw new Error(`expected tagged command, got ${JSON.stringify(value)}`);
  return entries[0] as [string, Record<string, Json>];
}

/// Same discipline as the Rust runtime: the final no-tool-call model text is
/// the answer; it must be a JSON object (bare text wraps as `{text}`); no
/// final text is an error *answer*, never an exception.
function finalResponse(finalText: string, doneReason: Json): { status: string; response: Record<string, Json> } {
  if (!finalText) {
    const detail = typeof doneReason === "object" && doneReason !== null && "Error" in doneReason
      ? `; last error: ${(doneReason as Record<string, Json>).Error}`
      : "";
    return { status: STATUS_ERROR, response: { error: `model did not produce a final answer${detail}` } };
  }
  const trimmed = stripJsonFence(finalText.trim());
  try {
    const value = JSON.parse(trimmed) as Json;
    if (value && typeof value === "object" && !Array.isArray(value)) {
      return { status: STATUS_SUCCESS, response: value as Record<string, Json> };
    }
    return { status: STATUS_ERROR, response: { error: "final response JSON must be an object" } };
  } catch {
    return { status: STATUS_SUCCESS, response: { text: finalText.trim() } };
  }
}

function stripJsonFence(text: string): string {
  if (!text.startsWith("```")) return text;
  let rest = text.slice(3);
  if (rest.startsWith("json") || rest.startsWith("JSON")) rest = rest.slice(4);
  rest = rest.replace(/^[\r\n]+/, "");
  if (rest.endsWith("```")) rest = rest.slice(0, -3);
  return rest.trim();
}

function estimateTokens(text: string): number {
  return Math.max(1, Math.ceil([...String(text)].length / 4));
}

/// In-memory reference stores (tests and as the "how to write a backend" example).
export class MemTraceStore implements TraceStore {
  private readonly traces = new Map<string, Json>();

  async put(trace: Json, header: TraceHeader): Promise<string> {
    const stamped = stampHeader(trace, header);
    let id = await contentId(stamped);
    let counter = 1;
    const base = id;
    while (this.traces.has(id)) {
      id = `${base}-${counter}`;
      counter += 1;
    }
    (stamped.meta as Record<string, Json>).trace_id = id;
    this.traces.set(id, stamped as Json);
    return id;
  }

  async get(id: string): Promise<Json> {
    const trace = this.traces.get(id);
    if (!trace) throw new Error(`trace not found: ${id}`);
    return trace;
  }

  async list(): Promise<TraceHead[]> {
    return [...this.traces.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([id, trace]) => headOf(id, trace));
  }
}

export class MemFeedbackStore implements FeedbackStore {
  private readonly entries = new Map<string, Feedback[]>();

  async append(feedback: Feedback): Promise<void> {
    const list = this.entries.get(feedback.trace_id) ?? [];
    list.push(feedback);
    this.entries.set(feedback.trace_id, list);
  }

  async list(traceId: string): Promise<Feedback[]> {
    return [...(this.entries.get(traceId) ?? [])];
  }
}

export function stampHeader(trace: Json, header: TraceHeader): Record<string, Json> {
  const stamped = JSON.parse(JSON.stringify(trace)) as Record<string, Json>;
  const meta = (stamped.meta as Record<string, Json>) ?? {};
  meta.agent_name = header.agent_name;
  meta.agent_version = header.agent_version;
  meta.question = header.question;
  meta.status = header.status;
  if (header.depends_on) meta.depends_on = header.depends_on;
  if (header.extra !== null && header.extra !== undefined) meta.extra = header.extra;
  stamped.meta = meta;
  return stamped;
}

export function headOf(id: string, trace: Json): TraceHead {
  const meta = ((trace as Record<string, Json>).meta as Record<string, Json>) ?? {};
  return {
    trace_id: id,
    depends_on: (meta.depends_on as string) ?? null,
    agent_name: (meta.agent_name as string) ?? "",
    agent_version: (meta.agent_version as string) ?? "",
    created_at: (meta.created_at as number) ?? null,
    question: (meta.question as string) ?? "",
    status: (meta.status as string) ?? "",
  };
}

/// Content-derived id: sha256 of the headed trace JSON, first 16 hex chars —
/// the same scheme as the Rust `TraceStore` (ids are store keys, not
/// cross-language identical).
export async function contentId(stamped: Record<string, Json>): Promise<string> {
  const bytes = new TextEncoder().encode(JSON.stringify(stamped));
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("").slice(0, 16);
}
