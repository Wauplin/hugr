// OpenAI-compatible streaming /chat/completions client over fetch, with the
// same retry boundary as huggr-providers: transport failures and 429/5xx
// responses before streaming starts, with exponential backoff.
// Request shaping comes from the brain's ModelRequest blocks — no adapter-side
// pruning.

import type { Json, ModelOutput, TierConfig, Usage } from "./contract.js";

export interface ModelCallSettings {
  baseUrl: string;
  apiKey: string;
  tier: TierConfig;
}

export interface ModelCallHooks {
  onText?: (text: string) => void;
  signal?: AbortSignal;
}

export interface ModelResult {
  output: ModelOutput;
  usage: Usage;
}

const MAX_ATTEMPTS = 3;

export async function callOpenAiCompatible(
  request: Json,
  settings: ModelCallSettings,
  hooks: ModelCallHooks = {},
): Promise<ModelResult> {
  const baseUrl = settings.baseUrl.replace(/\/+$/, "");
  const body = buildBody(request as Record<string, Json>, settings.tier);
  let lastError: Error | null = null;
  for (let attempt = 1; attempt <= MAX_ATTEMPTS; attempt += 1) {
    let response: Response;
    try {
      response = await fetch(`${baseUrl}/chat/completions`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${settings.apiKey}`,
        },
        body: JSON.stringify(body),
        signal: hooks.signal,
      });
    } catch (error) {
      if (hooks.signal?.aborted || isAbortError(error) || attempt === MAX_ATTEMPTS) throw error;
      lastError = error instanceof Error ? error : new Error(String(error));
      await sleep(200 * 2 ** (attempt - 1), hooks.signal);
      continue;
    }
    if (response.ok) {
      const contentType = response.headers.get("content-type") ?? "";
      if (!contentType.startsWith("text/event-stream")) throw new Error(`unexpected model content type: ${contentType}`);
      return await parseStream(response, hooks);
    }
    const detail = await response.text();
    lastError = new Error(`model request failed with ${response.status}: ${detail}`);
    const retryable = response.status === 429 || response.status >= 500;
    if (!retryable || attempt === MAX_ATTEMPTS) throw lastError;
    await sleep(200 * 2 ** (attempt - 1), hooks.signal);
  }
  throw lastError ?? new Error("model request failed");
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException
    ? error.name === "AbortError"
    : typeof error === "object" && error !== null && "name" in error && error.name === "AbortError";
}

function buildBody(request: Record<string, Json>, tier: TierConfig): Record<string, Json> {
  const blocks = (request.blocks as Json[]) ?? [];
  const messages = blocks.flatMap((block) => toMessages(block as Record<string, Json>));
  const body: Record<string, Json> = {
    model: tier.model,
    messages,
    stream: true,
    stream_options: { include_usage: true },
  };
  const tools = ((request.tools as Json[]) ?? []).map((tool) => {
    const t = tool as Record<string, Json>;
    return { type: "function", function: { name: t.name, description: t.description, parameters: t.parameters } };
  });
  if (tools.length) body.tools = tools;
  const extra = request.extra;
  if (extra && typeof extra === "object" && !Array.isArray(extra)) {
    for (const [key, value] of Object.entries(extra)) {
      if (value !== null && !["model", "messages", "stream", "stream_options", "tools"].includes(key)) body[key] = value;
    }
  }
  return body;
}

function toMessages(block: Record<string, Json>): Json[] {
  switch (block.role) {
    case "System":
      return [{ role: "system", content: collectText(block.content as Json[]) }];
    case "User":
      return [{ role: "user", content: collectText(block.content as Json[]) }];
    case "Assistant": {
      const toolCalls: Json[] = [];
      let text = "";
      for (const part of (block.content as Json[]) ?? []) {
        const [kind, payload] = tagged(part);
        if (kind === "Text") text += payload as string;
        if (kind === "ToolUse") {
          const call = payload as Record<string, Json>;
          toolCalls.push({
            id: call.id,
            type: "function",
            function: { name: call.name, arguments: JSON.stringify(call.args ?? {}) },
          });
        }
      }
      const message: Record<string, Json> = { role: "assistant", content: text || null };
      if (toolCalls.length) message.tool_calls = toolCalls;
      return [message];
    }
    case "Tool":
      return (((block.content as Json[]) ?? [])
        .map((part) => {
          const [kind, payload] = tagged(part);
          if (kind !== "ToolResult") return null;
          const result = payload as Record<string, Json>;
          return {
            role: "tool",
            tool_call_id: result.id,
            content: typeof result.result === "string" ? result.result : JSON.stringify(result.result),
          };
        })
        .filter(Boolean)) as Json[];
    default:
      return [];
  }
}

async function parseStream(response: Response, hooks: ModelCallHooks): Promise<ModelResult> {
  const reader = response.body!.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  let text = "";
  let reasoning = "";
  let stop = "end_turn";
  let usage: Usage = { input_tokens: 0, output_tokens: 0, extra: null };
  const tools = new Map<number, { id: string; name: string; args: string }>();
  let sawEvent = false;
  let sawDone = false;
  stream: for (;;) {
    if (hooks.signal?.aborted) throw new DOMException("aborted", "AbortError");
    const { value, done } = await reader.read();
    if (done) break;
    buffered += decoder.decode(value, { stream: true });
    const lines = buffered.split(/\r?\n/);
    buffered = lines.pop() ?? "";
    for (const line of lines) {
      if (!line.startsWith("data:")) continue;
      const data = line.slice(5).trim();
      if (!data) continue;
      if (data === "[DONE]") {
        sawDone = true;
        await reader.cancel();
        break stream;
      }
      const chunk = JSON.parse(data) as Record<string, Json>;
      sawEvent = true;
      if (chunk.usage) usage = normalizeUsage(chunk.usage as Record<string, Json>);
      for (const rawChoice of (chunk.choices as Json[]) ?? []) {
        const choice = rawChoice as Record<string, Json>;
        if (choice.finish_reason) stop = normalizeStop(choice.finish_reason as string);
        const delta = (choice.delta as Record<string, Json>) ?? {};
        if (delta.content) {
          text += delta.content as string;
          hooks.onText?.(delta.content as string);
        }
        const reasoningDelta = (delta.reasoning_content ?? delta.reasoning) as string | undefined;
        if (reasoningDelta) reasoning += reasoningDelta;
        for (const rawTool of (delta.tool_calls as Json[]) ?? []) {
          const tool = rawTool as Record<string, Json>;
          const index = (tool.index as number) ?? tools.size;
          const existing = tools.get(index) ?? { id: "", name: "", args: "" };
          if (tool.id) existing.id = tool.id as string;
          const fn = (tool.function as Record<string, Json>) ?? {};
          if (fn.name) existing.name = fn.name as string;
          if (fn.arguments) existing.args += fn.arguments as string;
          tools.set(index, existing);
        }
      }
    }
  }
  if (!sawEvent) throw new Error("model stream contained no valid SSE data event");
  if (!sawDone) throw new Error("model stream ended before [DONE]");
  const toolCalls = [...tools.values()]
    .filter((tool) => tool.name)
    .map((tool, index) => ({ id: tool.id || `call_${index + 1}`, name: tool.name, args: parseArgs(tool.args) }));
  return {
    output: { text, reasoning: reasoning || null, tool_calls: toolCalls, stop: toolCalls.length ? "tool_use" : stop },
    usage,
  };
}

function collectText(parts: Json[] = []): string {
  return parts
    .map((part) => {
      const [kind, payload] = tagged(part);
      return kind === "Text" ? (payload as string) : "";
    })
    .join("");
}

function tagged(value: Json): [string, Json] {
  const entries = Object.entries((value as Record<string, Json>) ?? {});
  if (entries.length !== 1) return ["", null];
  return entries[0] as [string, Json];
}

function parseArgs(raw: string): Json {
  if (!raw) return {};
  try {
    return JSON.parse(raw) as Json;
  } catch {
    return { raw };
  }
}

function normalizeUsage(raw: Record<string, Json>): Usage {
  return {
    input_tokens: (raw.prompt_tokens as number) || (raw.input_tokens as number) || 0,
    output_tokens: (raw.completion_tokens as number) || (raw.output_tokens as number) || 0,
    extra: raw,
  };
}

function normalizeStop(reason: string): string {
  return reason === "tool_calls" || reason === "function_call" ? "tool_use" : String(reason || "end_turn");
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(new DOMException("aborted", "AbortError"));
    setTimeout(resolve, ms);
  });
}
