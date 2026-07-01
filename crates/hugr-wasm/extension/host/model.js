// The browser model adapter: the exact analogue of hugr-providers' OpenAiAdapter
// (openai.rs), but using `fetch` + a streamed ReadableStream instead of reqwest.
// It translates the canonical ModelRequest into the chat-completions wire format,
// streams the SSE response (forwarding text deltas live), and returns the
// consolidated ModelOutput + Usage in the exact serde shape the brain expects.
//
// CORS note: an MV3 extension page with host_permissions for the endpoint can
// fetch it cross-origin without a CORS preflight failing — which is what lets
// this run with NO backend of our own (ROADMAP Phase 4: "no server").

/** Collect the plain text of a list of content parts. */
function collectText(parts) {
  let out = "";
  for (const p of parts) if (p.Text != null) out += p.Text;
  return out;
}

/** Render an opaque tool-result value to the string the API expects. */
function stringify(value) {
  return typeof value === "string" ? value : JSON.stringify(value);
}

function selectorName(selector) {
  return selector?.Named || selector || "medium";
}

export function resolveModel(config, selector) {
  const name = selectorName(selector);
  return config.models?.[name] || config.model || config.models?.medium;
}

/** Build the chat-completions request body from a canonical ModelRequest. */
export function buildBody(request, config, selector = { Named: "medium" }) {
  const messages = [];
  for (const block of request.blocks || []) {
    switch (block.role) {
      case "System":
        messages.push({ role: "system", content: collectText(block.content) });
        break;
      case "User":
        messages.push({ role: "user", content: collectText(block.content) });
        break;
      case "Assistant": {
        let text = "";
        const toolCalls = [];
        for (const part of block.content) {
          if (part.Text != null) text += part.Text;
          else if (part.ToolUse) {
            toolCalls.push({
              id: part.ToolUse.id,
              type: "function",
              function: {
                name: part.ToolUse.name,
                arguments: JSON.stringify(part.ToolUse.args ?? {}),
              },
            });
          }
        }
        const msg = { role: "assistant", content: text === "" ? null : text };
        if (toolCalls.length) msg.tool_calls = toolCalls;
        messages.push(msg);
        break;
      }
      case "Tool":
        for (const part of block.content) {
          if (part.ToolResult) {
            messages.push({
              role: "tool",
              tool_call_id: part.ToolResult.id,
              content: stringify(part.ToolResult.result),
            });
          }
        }
        break;
      // Forward-compatible: skip roles we don't map.
    }
  }

  const tools = (request.tools || []).map((t) => ({
    type: "function",
    function: { name: t.name, description: t.description, parameters: t.parameters },
  }));

  const body = {
    model: resolveModel(config, selector),
    messages,
    stream: true,
    stream_options: { include_usage: true },
  };
  if (tools.length) body.tools = tools;
  const temp = request.params?.temperature ?? config.temperature;
  if (temp != null) body.temperature = temp;
  if (request.params?.max_tokens != null) body.max_tokens = request.params.max_tokens;
  return body;
}

function mapStop(finish) {
  switch (finish) {
    case "stop":
      return "EndTurn";
    case "tool_calls":
    case "function_call":
      return "ToolUse";
    case "length":
      return "MaxTokens";
    default:
      return { Other: finish };
  }
}

/**
 * Call the model, streaming the response. `onText`/`onReasoning` fire per delta
 * for live rendering; `signal` aborts the fetch (for cancellation).
 *
 * @returns {Promise<{ output: object, usage: object }>} ModelOutput + Usage in
 *   serde shape, ready to drop into an Event::ModelDone.
 */
export async function callModel(request, config, { model, onText, onReasoning, signal } = {}) {
  if (!config.apiKey) {
    throw new Error("No API key set. Open the extension's Options page and add one.");
  }
  const body = buildBody(request, config, model);
  const url = `${config.baseUrl.replace(/\/$/, "")}/chat/completions`;

  const resp = await fetch(url, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${config.apiKey}`,
    },
    body: JSON.stringify(body),
    signal,
  });

  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`model endpoint returned ${resp.status}: ${text.slice(0, 500)}`);
  }

  // Accumulate the streamed chat-completions response (mirrors openai.rs).
  const acc = { text: "", reasoning: "", toolCalls: new Map(), stop: null, inTok: 0, outTok: 0, cost: null };
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";

  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += decoder.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).replace(/\r$/, "");
      buf = buf.slice(nl + 1);
      if (!line.startsWith("data:")) continue;
      const data = line.slice(5).trim();
      if (data === "[DONE]") continue;
      let json;
      try {
        json = JSON.parse(data);
      } catch {
        continue;
      }
      ingest(json, acc, { onText, onReasoning });
    }
  }

  return finish(acc);
}

function ingest(value, acc, { onText, onReasoning }) {
  if (value.usage) {
    acc.inTok = value.usage.prompt_tokens || 0;
    acc.outTok = value.usage.completion_tokens || 0;
    const cost =
      value.usage.cost ??
      value.usage.total_cost ??
      value.usage.cost_details?.total_cost ??
      null;
    if (cost != null) acc.cost = cost;
  }

  const choice = value.choices?.[0];
  if (!choice) return;
  const delta = choice.delta;
  if (delta) {
    if (delta.content) {
      acc.text += delta.content;
      onText?.(delta.content);
    }
    const reasoning = delta.reasoning_content ?? delta.reasoning;
    if (reasoning) {
      acc.reasoning += reasoning;
      onReasoning?.(reasoning);
    }
    if (Array.isArray(delta.tool_calls)) {
      for (const tc of delta.tool_calls) {
        const idx = tc.index ?? 0;
        let entry = acc.toolCalls.get(idx);
        if (!entry) {
          entry = { id: "", name: "", args: "" };
          acc.toolCalls.set(idx, entry);
        }
        if (tc.id) entry.id = tc.id;
        if (tc.function?.name) entry.name = tc.function.name;
        if (tc.function?.arguments) entry.args += tc.function.arguments;
      }
    }
  }
  if (choice.finish_reason) acc.stop = mapStop(choice.finish_reason);
}

function finish(acc) {
  const tool_calls = [];
  for (const [idx, tc] of acc.toolCalls) {
    // Guarantee a stable, non-empty id (the brain correlates results by it).
    const id = tc.id || `call_${idx}`;
    let args;
    try {
      args = tc.args.trim() ? JSON.parse(tc.args) : {};
    } catch {
      args = { _raw: tc.args };
    }
    tool_calls.push({ id, name: tc.name, args });
  }

  const output = {
    text: acc.text,
    reasoning: acc.reasoning ? acc.reasoning : null,
    tool_calls,
    stop: acc.stop ?? "EndTurn",
  };
  const usage = {
    input_tokens: acc.inTok,
    output_tokens: acc.outTok,
    extra: acc.cost != null ? { cost: acc.cost, cost_source: "router" } : null,
  };
  return { output, usage };
}
