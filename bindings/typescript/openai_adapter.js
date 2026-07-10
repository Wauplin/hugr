const DEFAULT_COMPACT_AFTER_TOKENS = 64000;
const DEFAULT_COMPACT_TARGET_TOKENS = 56000;
const DEFAULT_COMPACT_SUMMARY_TOKENS = 8000;
const DEFAULT_COMPACT_MESSAGE_TOKENS = 12000;

export async function callOpenAiCompatible(request, settings, hooks = {}) {
  const baseUrl = (settings.baseUrl || "https://router.huggingface.co/v1").replace(/\/+$/, "");
  const apiKey = settings.apiKey || "";
  if (!apiKey) throw new Error("missing API key in settings");
  const body = buildBody(request, settings, hooks);
  const response = await fetch(`${baseUrl}/chat/completions`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${apiKey}`
    },
    body: JSON.stringify(body),
    signal: hooks.signal
  });
  if (!response.ok) {
    throw new Error(`model request failed with ${response.status}: ${await response.text()}`);
  }
  return await parseStream(response, hooks);
}

function buildBody(request, settings, hooks) {
  const rawMessages = request.blocks.flatMap(toMessages).filter(Boolean);
  const pruned = pruneStaleBrowserObservations(rawMessages, settings);
  if (pruned.pruned) {
    hooks.onCompaction?.(pruned.meta);
  }
  const compaction = compactMessages(pruned.messages, settings);
  if (compaction.compacted) {
    hooks.onCompaction?.(compaction.meta);
  }
  const body = {
    model: settings.model || "google/gemma-4-31B-it:cerebras",
    messages: compaction.messages,
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
  if (request.params?.temperature !== null && request.params?.temperature !== undefined) {
    body.temperature = request.params.temperature;
  }
  if (request.params?.max_tokens !== null && request.params?.max_tokens !== undefined) {
    body.max_tokens = request.params.max_tokens;
  }
  if (request.extra && typeof request.extra === "object" && !Array.isArray(request.extra)) {
    Object.assign(body, Object.fromEntries(Object.entries(request.extra).filter(([, value]) => value !== null)));
  }
  return body;
}

function pruneStaleBrowserObservations(messages, settings) {
  if (settings.relevancePrune === false) {
    return { pruned: false, messages, meta: { before_tokens: estimateMessages(messages), after_tokens: estimateMessages(messages) } };
  }

  const leadingSystem = [];
  const rest = [];
  let stillLeadingSystem = true;
  for (const message of messages) {
    if (stillLeadingSystem && message.role === "system") {
      leadingSystem.push(message);
    } else {
      stillLeadingSystem = false;
      rest.push(message);
    }
  }

  const groups = groupMessages(rest);
  const invalidatedTabs = new Set();
  const newerObservationTabs = new Set();
  const kept = [];
  const dropped = [];

  for (let index = groups.length - 1; index >= 0; index -= 1) {
    const group = groups[index];
    const info = browserGroupInfo(group);
    const tabKey = info.tabId === null || info.tabId === undefined ? "unknown" : String(info.tabId);
    const staleObservation =
      info.kind === "heavy_observation" &&
      (invalidatedTabs.has(tabKey) || invalidatedTabs.has("all") || newerObservationTabs.has(tabKey));

    if (staleObservation) {
      dropped.push({ tool: info.toolName, tab_id: info.tabId, tokens: estimateMessages(group) });
    } else {
      kept.unshift(group);
    }

    if (info.kind === "heavy_observation") {
      newerObservationTabs.add(tabKey);
    }
    if (info.kind === "invalidating_action") {
      invalidatedTabs.add(tabKey);
    }
  }

  if (!dropped.length) {
    return { pruned: false, messages, meta: { before_tokens: estimateMessages(messages), after_tokens: estimateMessages(messages) } };
  }

  const note = {
    role: "system",
    content: [
      "hugr-wasm dropped stale browser observations from provider context.",
      "These were page HTML/text/snapshot tool results that were useful for choosing an action, but a later browser action, navigation, or fresher observation made them obsolete.",
      "If page details are needed again, call page_snapshot, page_read_text, or page_read_html again."
    ].join("\n")
  };
  const output = [...leadingSystem, note, ...kept.flat()];
  return {
    pruned: true,
    messages: output,
    meta: {
      kind: "relevance_prune",
      before_tokens: estimateMessages(messages),
      after_tokens: estimateMessages(output),
      dropped_observations: dropped.length,
      dropped_observation_tokens: dropped.reduce((sum, item) => sum + item.tokens, 0),
      examples: dropped.slice(0, 6)
    }
  };
}

function compactMessages(messages, settings) {
  const compactAfter = Number(settings.compactAfterTokens || DEFAULT_COMPACT_AFTER_TOKENS);
  const totalTokens = estimateMessages(messages);
  if (totalTokens <= compactAfter) {
    return { compacted: false, messages, meta: { before_tokens: totalTokens, after_tokens: totalTokens } };
  }

  const targetTokens = Number(settings.compactTargetTokens || DEFAULT_COMPACT_TARGET_TOKENS);
  const summaryBudget = Number(settings.compactSummaryTokens || DEFAULT_COMPACT_SUMMARY_TOKENS);
  const perMessageBudget = Number(settings.compactMessageTokens || DEFAULT_COMPACT_MESSAGE_TOKENS);
  const leadingSystem = [];
  const rest = [];
  let stillLeadingSystem = true;
  for (const message of messages) {
    if (stillLeadingSystem && message.role === "system") {
      leadingSystem.push(message);
    } else {
      stillLeadingSystem = false;
      rest.push(message);
    }
  }

  const groups = groupMessages(rest);
  const tailBudget = Math.max(12000, targetTokens - summaryBudget - estimateMessages(leadingSystem));
  const tailGroups = [];
  let tailTokens = 0;
  while (groups.length) {
    const group = groups[groups.length - 1];
    const shrunk = shrinkGroup(group, perMessageBudget);
    const groupTokens = estimateMessages(shrunk);
    if (tailGroups.length > 0 && tailTokens + groupTokens > tailBudget) break;
    groups.pop();
    tailGroups.unshift(shrunk);
    tailTokens += groupTokens;
  }

  const compactedText = summarizeGroups(groups, summaryBudget);
  const summaryMessage = {
    role: "system",
    content: [
      "Earlier browser session context was compacted automatically because the provider context window was getting too long.",
      "Preserve user intent, browser state observations, completed tool results, and unresolved errors from this summary.",
      compactedText || "No earlier context was retained."
    ].join("\n\n")
  };
  const output = [...leadingSystem, summaryMessage, ...tailGroups.flat()];
  return {
    compacted: true,
    messages: output,
    meta: {
      before_tokens: totalTokens,
      after_tokens: estimateMessages(output),
      compact_after_tokens: compactAfter,
      retained_recent_groups: tailGroups.length,
      compacted_groups: groups.length
    }
  };
}

function groupMessages(messages) {
  const groups = [];
  for (let index = 0; index < messages.length; index += 1) {
    const message = messages[index];
    if (message.role === "assistant" && Array.isArray(message.tool_calls) && message.tool_calls.length > 0) {
      const ids = new Set(message.tool_calls.map((call) => call.id));
      const group = [message];
      while (index + 1 < messages.length && messages[index + 1].role === "tool" && ids.has(messages[index + 1].tool_call_id)) {
        index += 1;
        group.push(messages[index]);
      }
      groups.push(group);
    } else if (message.role === "tool") {
      groups.push([{ role: "system", content: `Omitted stray tool result for ${message.tool_call_id}: ${truncate(message.content || "", 1200)}` }]);
    } else {
      groups.push([message]);
    }
  }
  return groups;
}

function browserGroupInfo(group) {
  const calls = toolCallsInGroup(group);
  if (!calls.length) return { kind: "other", toolName: "", tabId: null };
  const primary = calls[0];
  if (isHeavyObservationTool(primary.name)) {
    return { kind: "heavy_observation", toolName: primary.name, tabId: primary.tabId };
  }
  if (isInvalidatingBrowserTool(primary.name)) {
    return { kind: "invalidating_action", toolName: primary.name, tabId: primary.tabId };
  }
  return { kind: "other", toolName: primary.name, tabId: primary.tabId };
}

function toolCallsInGroup(group) {
  const first = group[0];
  if (first?.role !== "assistant" || !Array.isArray(first.tool_calls)) return [];
  const results = new Map(group.filter((message) => message.role === "tool").map((message) => [message.tool_call_id, parseJson(message.content)]));
  return first.tool_calls.map((call) => {
    const args = parseJson(call.function?.arguments || "{}") || {};
    const result = results.get(call.id) || {};
    return {
      id: call.id,
      name: call.function?.name || "",
      args,
      result,
      tabId: tabIdFrom(args, result)
    };
  });
}

function tabIdFrom(args, result) {
  return args.tab_id ?? args.tabId ?? result.tab_id ?? result.tabId ?? result.tab?.id ?? null;
}

function isHeavyObservationTool(name) {
  return name === "page_snapshot" || name === "page_read_text" || name === "page_read_html";
}

function isInvalidatingBrowserTool(name) {
  return new Set([
    "page_click",
    "page_type",
    "page_select",
    "page_scroll",
    "page_submit",
    "tab_open_url",
    "tab_reload",
    "tab_back",
    "tab_forward",
    "wait_for_navigation",
    "wait_for_url"
  ]).has(name);
}

function shrinkGroup(group, perMessageBudget) {
  return group.map((message) => {
    const tokens = estimateText(message.content || "");
    if (tokens <= perMessageBudget) return message;
    return {
      ...message,
      content: `${truncate(message.content || "", perMessageBudget * 4)}\n\n[message truncated by hugr-wasm compactor from approximately ${tokens} tokens]`
    };
  });
}

function summarizeGroups(groups, summaryBudgetTokens) {
  const lines = [];
  let budgetChars = summaryBudgetTokens * 4;
  for (const group of groups) {
    const line = summarizeGroup(group);
    if (!line) continue;
    if (budgetChars <= 0) break;
    const piece = truncate(line, Math.min(line.length, budgetChars));
    lines.push(piece);
    budgetChars -= piece.length + 1;
  }
  return lines.join("\n");
}

function summarizeGroup(group) {
  if (!group.length) return "";
  const first = group[0];
  if (first.role === "user") return `User: ${compactText(first.content)}`;
  if (first.role === "assistant" && first.tool_calls?.length) {
    const calls = first.tool_calls.map((call) => `${call.function?.name || "tool"}(${truncate(call.function?.arguments || "{}", 260)})`).join(", ");
    const results = group.slice(1).map((tool) => `${tool.tool_call_id}: ${compactText(tool.content)}`).join("; ");
    return `Assistant used tools: ${calls}${results ? ` -> ${results}` : ""}`;
  }
  if (first.role === "assistant") return `Assistant: ${compactText(first.content || "")}`;
  if (first.role === "tool") return `Tool ${first.tool_call_id}: ${compactText(first.content || "")}`;
  if (first.role === "system") return `System: ${compactText(first.content || "")}`;
  return `${first.role}: ${compactText(first.content || "")}`;
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
  while (true) {
    if (hooks.signal?.aborted) throw abortError();
    const { value, done } = await reader.read();
    if (done) break;
    buffered += decoder.decode(value, { stream: true });
    const lines = buffered.split(/\r?\n/);
    buffered = lines.pop() || "";
    for (const line of lines) {
      if (!line.startsWith("data:")) continue;
      const data = line.slice(5).trim();
      if (!data || data === "[DONE]") continue;
      const chunk = JSON.parse(data);
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

function estimateMessages(messages) {
  return messages.reduce((sum, message) => sum + estimateText(JSON.stringify(message)), 0);
}

function estimateText(text) {
  return Math.max(1, Math.ceil([...String(text || "")].length / 4));
}

function compactText(text) {
  return truncate(String(text || "").replace(/\s+/g, " ").trim(), 1200);
}

function truncate(text, maxChars) {
  const value = String(text || "");
  if (value.length <= maxChars) return value;
  return `${value.slice(0, Math.max(0, maxChars - 32))}...[truncated]`;
}

function parseArgs(raw) {
  if (!raw) return {};
  try {
    return JSON.parse(raw);
  } catch {
    return { raw };
  }
}

function parseJson(raw) {
  if (!raw) return null;
  if (typeof raw !== "string") return raw;
  try {
    return JSON.parse(raw);
  } catch {
    return null;
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
