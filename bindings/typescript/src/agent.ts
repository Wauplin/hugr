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
  ModelCatalog,
  ModelTier,
  ModelsConfig,
  TierConfig,
  ToolSpec,
  TraceHead,
  TraceHeader,
  TraceStore,
  Usage,
} from "./contract.js";
import { STATUS_ERROR, STATUS_SUCCESS } from "./contract.js";
import { callOpenAiCompatible } from "./openai.js";
import type { ModelCallHooks, ModelCallSettings, ModelResult } from "./openai.js";

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
  /// Resolve provider key and HUGGR_MODEL_<TIER> variables (Node: process.env; browsers have no env).
  env?: (name: string) => string | undefined;
  /// Trusted host override for all model mappings.
  modelCatalog?: ModelCatalog;
  /// Override provider credentials for every model tier in this agent.
  apiToken?: string;
}

const MODEL_TIERS: ModelTier[] = ["fast", "balanced", "powerful", "max"];

// Keep this browser-safe bootstrap catalog in sync with huggr-toolkit's DEFAULT_MODELS_TOML.
const DEFAULT_MODEL_CATALOG: ModelCatalog = {
  providers: {
    hf: { base_url: "https://router.huggingface.co/v1", api_key_env: "HF_TOKEN" },
  },
  models: {
    fast: { provider: "hf", model: "deepseek-ai/DeepSeek-V4-Flash:fireworks-ai", input_usd_per_m_tokens: 0.14, output_usd_per_m_tokens: 0.28 },
    balanced: { provider: "hf", model: "google/gemma-4-31B-it:cerebras", input_usd_per_m_tokens: 1.0, output_usd_per_m_tokens: 1.5 },
    powerful: { provider: "hf", model: "zai-org/GLM-5.2:together", input_usd_per_m_tokens: 1.4, output_usd_per_m_tokens: 4.4 },
  },
};

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
    const models = this.resolveModels();
    const session = new wasm.AgentSession(JSON.stringify(this.sessionConfig(models)));
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
          try {
            const stream = streamModelCall(payload.request as Json, this.settingsFor(selector, models), {
              signal: effectController.signal,
            });
            let result: ModelResult;
            for (;;) {
              const item = await stream.next();
              if (item.done) {
                result = item.value;
                break;
              }
              yield { type: "text_delta", op, text: item.value };
            }
            meta.model_calls += 1;
            meta.tokens_in += result.usage.input_tokens;
            meta.tokens_out += result.usage.output_tokens;
            meta.cost_micro_usd += this.costMicroUsd(selector, result.usage, models);
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

  private sessionConfig(models: ModelCatalog): Json {
    return {
      system_prompt: this.config.system ?? "",
      tools: (this.config.tools ?? []).map((tool) => ({
        name: tool.name,
        description: tool.description,
        parameters: tool.schema,
      })),
      default_model: this.config.models.default,
      context: (this.config.context as Json) ?? null,
    };
  }

  private tierConfig(selector: string, models: ModelCatalog): TierConfig {
    if (!isModelTier(selector)) throw new Error(`unknown model tier: ${selector}`);
    const tier = models.models[selector];
    if (!tier) throw new Error(`unresolved model tier: ${selector}`);
    return tier;
  }

  private settingsFor(selector: string, models: ModelCatalog) {
    const tier = this.tierConfig(selector, models);
    const provider = models.providers[tier.provider];
    if (!provider) throw new Error(`model tier ${selector} references unknown provider ${tier.provider}`);
    const apiKey = this.runtime.apiToken
      ?? provider.api_key
      ?? (provider.api_key_env ? (this.runtime.env?.(provider.api_key_env) ?? "") : "");
    return {
      baseUrl: provider.base_url,
      apiKey,
      tier,
    };
  }

  private costMicroUsd(selector: string, usage: Usage, models: ModelCatalog): number {
    const extra = usage.extra;
    if (extra && typeof extra === "object" && !Array.isArray(extra)) {
      const reported = extra.cost;
      if (typeof reported === "number" && Number.isFinite(reported) && reported >= 0) {
        return Math.round(reported * 1_000_000);
      }
    }
    const tier = this.tierConfig(selector, models);
    const cost = usage.input_tokens * (tier.input_usd_per_m_tokens ?? 0)
      + usage.output_tokens * (tier.output_usd_per_m_tokens ?? 0);
    return Math.round(cost);
  }

  /// Return the effective four-tier mapping after runtime and environment overrides.
  resolvedModels(): ModelCatalog {
    return this.resolveModels();
  }

  private resolveModels(): ModelCatalog {
    const runtime = this.runtime.modelCatalog;
    const providers = runtime
      ? { ...runtime.providers }
      : { ...DEFAULT_MODEL_CATALOG.providers, ...(this.config.providers ?? {}) };
    const sourceModels: Partial<Record<ModelTier, TierConfig>> = runtime
      ? runtime.models
      : Object.fromEntries(MODEL_TIERS.flatMap((tier) => this.config.models[tier] ? [[tier, this.config.models[tier]]] : []));
    const fallbackModels = runtime ? runtime.models : { ...DEFAULT_MODEL_CATALOG.models, ...sourceModels };
    const resolved: Partial<Record<ModelTier, TierConfig>> = {};
    for (const tier of MODEL_TIERS) {
      const base = closestTier(fallbackModels, tier);
      const envModel = this.runtime.env?.(`HUGGR_MODEL_${tier.toUpperCase()}`);
      resolved[tier] = envModel ? { ...base, model: envModel } : { ...base };
      if (!providers[resolved[tier]!.provider]) {
        throw new Error(`model tier ${tier} references unknown provider ${resolved[tier]!.provider}`);
      }
    }
    return { providers, models: resolved };
  }
}

async function* streamModelCall(
  request: Json,
  settings: ModelCallSettings,
  hooks: Omit<ModelCallHooks, "onText">,
): AsyncGenerator<string, ModelResult> {
  const deltas: string[] = [];
  let wake: (() => void) | null = null;
  let finished = false;
  const notify = () => {
    const pending = wake;
    wake = null;
    pending?.();
  };
  const outcome = callOpenAiCompatible(request, settings, {
    ...hooks,
    onText: (text) => {
      deltas.push(text);
      notify();
    },
  }).then(
    (result) => ({ result } as const),
    (error: unknown) => ({ error } as const),
  ).finally(() => {
    finished = true;
    notify();
  });

  while (!finished || deltas.length > 0) {
    if (deltas.length === 0) {
      await new Promise<void>((resolve) => {
        wake = resolve;
        if (finished || deltas.length > 0) notify();
      });
    }
    const delta = deltas.shift();
    if (delta !== undefined) yield delta;
  }

  const settled = await outcome;
  if ("error" in settled) throw settled.error;
  return settled.result;
}

function abortable<T>(effect: Promise<T> | T, signal: AbortSignal): Promise<T> {
  if (signal.aborted) return Promise.reject(new DOMException("aborted", "AbortError"));
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(new DOMException("aborted", "AbortError"));
    signal.addEventListener("abort", abort, { once: true });
    Promise.resolve(effect).then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

function isModelTier(value: string): value is ModelTier {
  return MODEL_TIERS.includes(value as ModelTier);
}

function closestTier(models: Partial<Record<ModelTier, TierConfig>>, requested: ModelTier): TierConfig {
  const index = MODEL_TIERS.indexOf(requested);
  for (let distance = 0; distance < MODEL_TIERS.length; distance += 1) {
    const lower = index - distance;
    if (lower >= 0 && models[MODEL_TIERS[lower]]) return models[MODEL_TIERS[lower]]!;
    const upper = index + distance;
    if (distance > 0 && upper < MODEL_TIERS.length && models[MODEL_TIERS[upper]]) return models[MODEL_TIERS[upper]]!;
  }
  throw new Error("model catalog defines no fixed tier");
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
