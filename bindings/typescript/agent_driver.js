import { callOpenAiCompatible } from "./openai_adapter.js";

// The generic Hugr agent driver: it drives the WASM brain (submit/poll) and
// performs IO through the injected `host`:
//   host.loadWasm(): Promise<HugrWasm class>   — wasm-bindgen module, initialized
//   host.invokeCapability(name, args): Promise — the capability dispatcher
//   host.loadSettings(): Promise<settings>     — apiKey/baseUrl/model/limits
//   host.saveSession(record): Promise          — session/trace persistence
//   host.systemPrompt: string                  — the agent's system prompt
export async function runAgent(question, host, hooks = {}) {
  const HugrWasm = await host.loadWasm();
  const settings = await host.loadSettings();
  const session = new HugrWasm(JSON.stringify(toRustConfig(settings, host)));
  const traceId = crypto.randomUUID();
  const createdAt = new Date().toISOString();
  const timeline = [];
  let streamedText = "";
  let autosavePending = Promise.resolve();
  const autosave = (patch = {}) => {
    const record = {
      traceId,
      question,
      answer: session.final_text() || streamedText,
      status: patch.status || "running",
      ok: patch.ok ?? false,
      createdAt,
      updatedAt: new Date().toISOString(),
      events: timeline,
      trace: safeTrace(session),
      ...patch
    };
    autosavePending = autosavePending
      .catch(() => {})
      .then(() => host.saveSession(record))
      .catch((error) => hooks.onAutosaveError?.(error));
    return autosavePending;
  };
  const push = (event) => {
    const item = { at: new Date().toISOString(), ...event };
    timeline.push(item);
    hooks.onEvent?.(item);
    autosave();
  };
  push({ type: "start", label: "Started" });
  await autosave();
  const commands = JSON.parse(session.submit_user_input(question, Date.now()));
  await autosave();
  try {
    const done = await driveCommands(session, commands, settings, host, hooks, push, {
      onText: (text) => {
        streamedText += text;
        if (streamedText.length % 500 < text.length) autosave();
      },
      autosave
    });
    const trace = JSON.parse(session.trace_json());
    const finalText = session.final_text();
    const status = normalizeDone(done);
    push({ type: status.ok ? "done" : "error", label: status.label, detail: done });
    await autosave({
      answer: finalText,
      status: status.label,
      ok: status.ok,
      trace
    });
    await autosavePending;
    return { traceId, answer: finalText, trace, done, status, events: timeline };
  } catch (error) {
    if (isAbortError(error)) {
      session.abort(Date.now());
      const done = { reason: "Interrupted by user" };
      const status = { ok: false, label: "interrupted" };
      push({ type: "interrupt", label: "Interrupted by user", detail: done });
      await autosave({
        answer: session.final_text() || streamedText,
        status: status.label,
        ok: false,
        trace: safeTrace(session)
      });
      await autosavePending;
      return { traceId, answer: session.final_text() || streamedText, trace: safeTrace(session), done, status, events: timeline };
    }
    const detail = errorPayload(error);
    push({ type: "error", label: "Run crashed", detail });
    const done = { reason: detail.error };
    const status = { ok: false, label: `crashed: ${detail.error}` };
    await autosave({
      answer: session.final_text() || streamedText,
      status: status.label,
      ok: false,
      trace: safeTrace(session)
    });
    await autosavePending;
    throw error;
  }
}

async function driveCommands(session, initialCommands, settings, host, hooks, push, persistence) {
  const queue = [...initialCommands];
  let done = null;
  let steps = 0;
  let modelCalls = 0;
  let costMicroUsd = 0;
  const defaults = host.defaults || {};
  const maxModelCalls = settings.maxModelCalls ?? defaults.maxModelCalls ?? null;
  const maxCostMicroUsd = settings.maxCostMicroUsd ?? defaults.maxCostMicroUsd ?? null;
  while (queue.length > 0) {
    throwIfAborted(hooks.signal);
    steps += 1;
    if (steps > 120) {
      throw new Error("stopped after 120 Hugr commands to avoid an infinite browser loop");
    }
    const command = queue.shift();
    const [kind, payload] = tagged(command);
    hooks.onCommand?.(kind, payload);
    switch (kind) {
      case "StartModelCall": {
        if (maxModelCalls !== null && modelCalls >= maxModelCalls) {
          throw new Error(`limit: max_model_calls (${maxModelCalls}) reached`);
        }
        if (maxCostMicroUsd !== null && costMicroUsd >= maxCostMicroUsd) {
          throw new Error(`limit: max_cost_micro_usd (${maxCostMicroUsd}) reached`);
        }
        const modelSettings = settingsForModel(settings, payload.model);
        push?.({ type: "model", label: "Model call", detail: { op: payload.op, model: payload.model } });
        try {
          const result = await callOpenAiCompatible(payload.request, modelSettings, {
            onText: (text) => {
              hooks.onText?.(text);
              persistence?.onText?.(text);
            },
            signal: hooks.signal
          });
          modelCalls += 1;
          costMicroUsd += reportedCostMicroUsd(result.usage);
          push?.({
            type: result.output.tool_calls?.length ? "model_tools" : "model_answer",
            label: result.output.tool_calls?.length
              ? `Model requested ${result.output.tool_calls.length} tool(s)`
              : "Model answered",
            detail: {
              op: payload.op,
              tool_calls: result.output.tool_calls || [],
              usage: result.usage
            }
          });
          queue.push(
            ...JSON.parse(
              session.submit_model_done(
                payload.op,
                JSON.stringify(result.output),
                JSON.stringify(result.usage),
                estimateTokens(result.output.text || ""),
                Date.now()
              )
            )
          );
          await persistence?.autosave?.();
        } catch (error) {
          push?.({ type: "error", label: "Model error", detail: errorPayload(error) });
          queue.push(
            ...JSON.parse(
              session.submit_model_error(payload.op, JSON.stringify(errorPayload(error)), Date.now())
            )
          );
          await persistence?.autosave?.();
        }
        break;
      }
      case "StartCapability": {
        push?.({
          type: "tool_start",
          label: payload.name,
          detail: { op: payload.op, args: payload.args || {} }
        });
        try {
          const result = await abortable(host.invokeCapability(payload.name, payload.args || {}), hooks.signal);
          push?.({
            type: "tool_done",
            label: `${payload.name} finished`,
            detail: { op: payload.op, result: summarize(result) }
          });
          queue.push(
            ...JSON.parse(
              session.submit_capability_done(payload.op, JSON.stringify(result), Date.now())
            )
          );
          await persistence?.autosave?.();
        } catch (error) {
          push?.({
            type: "error",
            label: `${payload.name} failed`,
            detail: errorPayload(error)
          });
          queue.push(
            ...JSON.parse(
              session.submit_capability_error(payload.op, JSON.stringify(errorPayload(error)), Date.now())
            )
          );
          await persistence?.autosave?.();
        }
        break;
      }
      case "RequestPermission": {
        push?.({
          type: "permission",
          label: `Auto-allowed ${payload.request.capability}`,
          detail: payload.request
        });
        queue.push(
          ...JSON.parse(
            session.submit_permission_decision(
              payload.op,
              true,
              null,
              Date.now()
            )
          )
        );
        await persistence?.autosave?.();
        break;
      }
      case "Cancel":
        push?.({ type: "cancel", label: "Cancelled", detail: { op: payload.op } });
        queue.push(...JSON.parse(session.submit_op_cancelled(payload.op, Date.now())));
        await persistence?.autosave?.();
        break;
      case "Emit":
        hooks.onEmit?.(payload);
        break;
      case "Checkpoint":
        hooks.onCheckpoint?.();
        await persistence?.autosave?.();
        break;
      case "Done":
        push?.({ type: "done", label: doneLabel(payload), detail: payload });
        done = payload;
        await persistence?.autosave?.({ status: doneLabel(payload), ok: normalizeDone(payload).ok });
        break;
      default:
        throw new Error(`unknown Hugr command: ${kind}`);
    }
  }
  if (!done) {
    return { reason: "No terminal Done command; command queue drained unexpectedly" };
  }
  return done;
}

function reportedCostMicroUsd(usage) {
  const raw = usage?.extra || {};
  const usd = Number(raw.cost ?? raw.cost_usd ?? raw.total_cost ?? 0);
  return Number.isFinite(usd) && usd > 0 ? Math.round(usd * 1_000_000) : 0;
}

function tagged(value) {
  if (typeof value === "string") return [value, {}];
  const entries = Object.entries(value || {});
  if (entries.length !== 1) throw new Error(`expected tagged command, got ${JSON.stringify(value)}`);
  return entries[0];
}

function toRustConfig(settings, host) {
  const defaults = host.defaults || {};
  const context = settings.context || defaults.context || {};
  return {
    base_url: settings.baseUrl || defaults.baseUrl || "https://router.huggingface.co/v1",
    model: settings.model || defaults.model || "google/gemma-4-31B-it:cerebras",
    api_key: settings.apiKey || "",
    max_model_calls: settings.maxModelCalls ?? defaults.maxModelCalls ?? null,
    max_cost_micro_usd: settings.maxCostMicroUsd ?? defaults.maxCostMicroUsd ?? null,
    system_prompt: host.systemPrompt || "",
    context: {
      compaction: context.compaction || "summarize",
      budget_tokens: Number(context.budgetTokens || context.budget_tokens || 64000),
      trigger_tokens: Number(context.triggerTokens || context.trigger_tokens || 56000),
      keep_recent_tokens: Number(context.keepRecentTokens || context.keep_recent_tokens || 8000),
      max_block_tokens: Number(context.maxBlockTokens || context.max_block_tokens || 2000),
      summary_model: context.summaryModel || context.summary_model || null,
      tool_ttl: context.toolTtl || context.tool_ttl || {},
      keep_last_per_tool: context.keepLastPerTool || context.keep_last_per_tool || {
        page_snapshot: 1,
        page_read_text: 1,
        page_read_html: 1
      }
    }
  };
}

function settingsForModel(settings, selector) {
  const key = typeof selector === "string" ? selector : selector?.[0] || selector;
  const override = settings.models?.[key];
  if (!override) return settings;
  if (typeof override === "string") return { ...settings, model: override };
  return { ...settings, ...override };
}

function errorPayload(error) {
  return { error: String(error?.message || error) };
}

function estimateTokens(text) {
  return Math.max(1, Math.ceil([...String(text)].length / 4));
}

function normalizeDone(done) {
  const reason = done?.reason;
  if (reason === "EndTurn") return { ok: true, label: "completed" };
  if (reason === "Cancelled") return { ok: false, label: "cancelled" };
  if (typeof reason === "object" && reason?.Error) return { ok: false, label: `error: ${reason.Error}` };
  if (typeof reason === "string" && reason) return { ok: reason === "completed", label: reason };
  return { ok: false, label: "stopped without completion" };
}

function doneLabel(done) {
  return normalizeDone(done).label;
}

function summarize(value) {
  const json = JSON.stringify(value);
  if (json.length <= 900) return value;
  return { summary: `${json.slice(0, 900)}...`, truncated: true };
}

function throwIfAborted(signal) {
  if (signal?.aborted) throw abortError();
}

function abortable(promise, signal) {
  if (!signal) return promise;
  throwIfAborted(signal);
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      signal.addEventListener("abort", () => reject(abortError()), { once: true });
    })
  ]);
}

function isAbortError(error) {
  return error?.name === "AbortError" || /interrupted|aborted/i.test(String(error?.message || error));
}

function abortError() {
  return new DOMException("Interrupted by user", "AbortError");
}

function safeTrace(session) {
  try {
    return JSON.parse(session.trace_json());
  } catch (error) {
    return { error: String(error?.message || error) };
  }
}
