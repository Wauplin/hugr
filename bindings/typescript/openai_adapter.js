export async function callOpenAiCompatible(request, settings, hooks = {}) {
  const baseUrl = (settings.baseUrl || "https://router.huggingface.co/v1").replace(/\/+$/, "");
  const apiKey = settings.apiKey || "";
  if (!apiKey) throw new Error("missing API key in settings");
  const body = buildBody(request, settings);
  let response;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    response = await fetch(`${baseUrl}/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${apiKey}` },
      body: JSON.stringify(body),
      signal: hooks.signal
    });
    if (response.ok) break;
    const detail = await response.text();
    if (!(response.status === 429 || response.status >= 500) || attempt === 3) {
      throw new Error(`model request failed with ${response.status}: ${detail}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 200 * 2 ** (attempt - 1)));
  }
  const contentType = response.headers.get("content-type") || "";
  if (!contentType.startsWith("text/event-stream")) throw new Error(`unexpected model content type: ${contentType}`);
  return await parseStream(response, hooks);
}

function buildBody(request, settings) {
  const messages = request.blocks.flatMap(toMessages).filter(Boolean);
  const body = {
    model: settings.model || "google/gemma-4-31B-it:cerebras",
    messages,
    stream: true,
    stream_options: { include_usage: true }
  };
  const tools = (request.tools || []).map((tool) => ({
    type: "function",
    function: {
      name: tool.name,
      description: tool.description,
      parameters: tool.parameters
    }
  }));
  if (tools.length) body.tools = tools;
  if (request.extra && typeof request.extra === "object" && !Array.isArray(request.extra)) {
    const reserved = new Set(["model", "messages", "stream", "stream_options", "tools"]);
    Object.assign(body, Object.fromEntries(Object.entries(request.extra).filter(([key, value]) => value !== null && !reserved.has(key))));
  }
  return body;
}

function toMessages(block) {
  switch (block.role) {
    case "System":
      return [{ role: "system", content: collectText(block.content) }];
    case "User":
      return [{ role: "user", content: collectText(block.content) }];
    case "Assistant": {
      const toolCalls = [];
      let text = "";
      for (const part of block.content || []) {
        const [kind, payload] = tagged(part);
        if (kind === "Text") text += payload;
        if (kind === "ToolUse") {
          toolCalls.push({
            id: payload.id,
            type: "function",
            function: {
              name: payload.name,
              arguments: JSON.stringify(payload.args || {})
            }
          });
        }
      }
      const message = { role: "assistant", content: text || null };
      if (toolCalls.length) message.tool_calls = toolCalls;
      return [message];
    }
    case "Tool":
      return (block.content || []).map((part) => {
        const [kind, payload] = tagged(part);
        if (kind !== "ToolResult") return null;
        return {
          role: "tool",
          tool_call_id: payload.id,
          content: stringify(payload.result)
        };
      }).filter(Boolean);
    default:
      return [];
  }
}

async function parseStream(response, hooks) {
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = "";
  let text = "";
  let reasoning = "";
  let stop = "end_turn";
  let usage = { input_tokens: 0, output_tokens: 0, extra: null };
  const tools = new Map();
  let sawEvent = false;
  let sawDone = false;
  stream: while (true) {
    if (hooks.signal?.aborted) throw abortError();
    const { value, done } = await reader.read();
    if (done) break;
    buffered += decoder.decode(value, { stream: true });
    const lines = buffered.split(/\r?\n/);
    buffered = lines.pop() || "";
    for (const line of lines) {
      if (!line.startsWith("data:")) continue;
      const data = line.slice(5).trim();
      if (!data) continue;
      if (data === "[DONE]") {
        sawDone = true;
        await reader.cancel();
        break stream;
      }
      const chunk = JSON.parse(data);
      sawEvent = true;
      if (chunk.usage) usage = normalizeUsage(chunk.usage);
      for (const choice of chunk.choices || []) {
        if (choice.finish_reason) stop = normalizeStop(choice.finish_reason);
        const delta = choice.delta || {};
        if (delta.content) {
          text += delta.content;
          hooks.onText?.(delta.content);
        }
        const reasoningDelta = delta.reasoning_content || delta.reasoning;
        if (reasoningDelta) reasoning += reasoningDelta;
        for (const tool of delta.tool_calls || []) {
          const index = tool.index ?? tools.size;
          const existing = tools.get(index) || { id: "", name: "", args: "" };
          if (tool.id) existing.id = tool.id;
          if (tool.function?.name) existing.name = tool.function.name;
          if (tool.function?.arguments) existing.args += tool.function.arguments;
          tools.set(index, existing);
        }
      }
    }
  }
  if (!sawEvent) throw new Error("model stream contained no valid SSE data event");
  if (!sawDone) throw new Error("model stream ended before [DONE]");
  const tool_calls = [...tools.values()].filter((tool) => tool.name).map((tool, index) => ({
    id: tool.id || `call_${index + 1}`,
    name: tool.name,
    args: parseArgs(tool.args)
  }));
  return {
    output: {
      text,
      reasoning: reasoning || null,
      tool_calls,
      stop: tool_calls.length ? "tool_use" : stop
    },
    usage
  };
}

function abortError() {
  return new DOMException("Interrupted by user", "AbortError");
}

function collectText(parts = []) {
  return parts.map((part) => {
    const [kind, payload] = tagged(part);
    return kind === "Text" ? payload : "";
  }).join("");
}

function tagged(value) {
  const entries = Object.entries(value || {});
  if (entries.length !== 1) return ["", null];
  return entries[0];
}

function stringify(value) {
  return typeof value === "string" ? value : JSON.stringify(value);
}

function parseArgs(raw) {
  if (!raw) return {};
  try {
    return JSON.parse(raw);
  } catch {
    return { raw };
  }
}

function normalizeUsage(raw) {
  return {
    input_tokens: raw.prompt_tokens || raw.input_tokens || 0,
    output_tokens: raw.completion_tokens || raw.output_tokens || 0,
    extra: raw
  };
}

function normalizeStop(reason) {
  return reason === "tool_calls" || reason === "function_call" ? "tool_use" : String(reason || "end_turn");
}
