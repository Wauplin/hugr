import assert from "node:assert/strict";
import { after, before, beforeEach, test } from "node:test";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

import { Agent, MemTraceStore, MemFeedbackStore } from "../dist/index.js";
import { loadWasm, FsTraceStore, FsFeedbackStore, createAgent } from "../dist/node.js";
import { MockOpenAi } from "./mock_openai.mjs";

let server;
before(async () => {
  server = await new MockOpenAi().listen();
});
after(() => server.close());
beforeEach(() => {
  server.outputs.length = 0;
  server.requests.length = 0;
});

function memRuntime() {
  return { loadWasm, traces: new MemTraceStore(), feedback: new MemFeedbackStore() };
}

function makeAgent({ tools = [], limits, context, system = "Answer as JSON.", runtime = memRuntime() } = {}) {
  return new Agent(
    {
      name: "ts-test-agent",
      system,
      providers: { test: { base_url: server.baseUrl, api_key: "test-key" } },
      models: {
        default: "balanced",
        balanced: { provider: "test", model: "mock-model", input_usd_per_m_tokens: 1.0, output_usd_per_m_tokens: 2.0 },
      },
      tools,
      limits,
      context,
    },
    runtime,
  );
}

function lookupTool(calls) {
  return {
    name: "lookup",
    description: "Look a word up.",
    schema: { type: "object", properties: { word: { type: "string" } }, required: ["word"] },
    invoke: async (args) => {
      calls.push(args);
      return { definition: `meaning of ${args.word}` };
    },
  };
}

test("tool round-trip with accounting", async () => {
  const calls = [];
  const agent = makeAgent({ tools: [lookupTool(calls)] });
  server.scriptToolCall("lookup", { word: "huggr" });
  server.scriptText(JSON.stringify({ answer: "huggr is a toolkit" }));

  const answer = await agent.ask("What is huggr?");

  assert.equal(answer.status, "success");
  assert.deepEqual(answer.response, { answer: "huggr is a toolkit" });
  assert.deepEqual(calls, [{ word: "huggr" }]);
  assert.equal(answer.metadata.model_calls, 2);
  assert.equal(answer.metadata.tool_calls, 1);
  assert.ok(answer.metadata.cost_micro_usd > 0);
  const second = server.requests[1];
  assert.ok(second.messages.some((m) => m.role === "tool"));
});

test("runtime model catalog overrides author mappings", () => {
  const runtime = {
    ...memRuntime(),
    modelCatalog: {
      providers: { runtime: { base_url: server.baseUrl, api_key: "runtime-key" } },
      models: { powerful: { provider: "runtime", model: "runtime-model" } },
    },
  };
  const agent = makeAgent({ runtime });
  const resolved = agent.resolvedModels();
  assert.equal(resolved.models.balanced.model, "runtime-model");
  assert.equal(resolved.models.max.model, "runtime-model");
});

test("tool exceptions are semantic errors", async () => {
  const agent = makeAgent({
    tools: [{ name: "boom", description: "d", schema: { type: "object" }, invoke: () => { throw new Error("kaput"); } }],
  });
  server.scriptToolCall("boom", {});
  server.scriptText('{"answer": "recovered"}');
  const answer = await agent.ask("q");
  assert.equal(answer.status, "success");
  const toolMsg = server.requests[1].messages.find((m) => m.role === "tool");
  assert.ok(toolMsg.content.includes("kaput"));
});

test("errors are answers", async () => {
  const agent = makeAgent();
  // Nothing scripted → HTTP 500 (retried, still failing) → error answer.
  const answer = await agent.ask("q");
  assert.equal(answer.status, "error");
  assert.ok(String(answer.response.error).length > 0);
  assert.ok(answer.trace_id);
});

test("transient transport failures are retried", async () => {
  const agent = makeAgent();
  server.scriptTransportFailure();
  server.scriptText('{"answer": "retried"}');

  const answer = await agent.ask("q");

  assert.equal(answer.status, "success");
  assert.deepEqual(answer.response, { answer: "retried" });
  assert.equal(server.requests.length, 2);
});

test("event stream ordering", async () => {
  const calls = [];
  const agent = makeAgent({ tools: [lookupTool(calls)] });
  server.scriptToolCall("lookup", { word: "huggr" });
  server.scriptText('{"answer": "ok"}');
  const events = [];
  for await (const event of agent.run("q")) events.push(event);
  const types = events.map((e) => e.type);
  assert.equal(types[0], "ask_started");
  assert.equal(types.at(-1), "answer_ready");
  assert.ok(types.includes("model_started") && types.includes("text_delta"));
  assert.ok(types.indexOf("tool_started") < types.indexOf("tool_ended"));
  assert.equal(events.at(-1).answer.status, "success");
});

test("text deltas are yielded before the model stream finishes", async () => {
  const agent = makeAgent();
  const gate = server.scriptPausedText('{"answer": "streamed"}');
  const stream = agent.run("q");
  assert.equal((await stream.next()).value.type, "ask_started");
  assert.equal((await stream.next()).value.type, "model_started");

  const pendingDelta = stream.next();
  await gate.started;
  const outcome = await Promise.race([
    pendingDelta.then((event) => ({ event })),
    new Promise((resolve) => setTimeout(() => resolve({ timeout: true }), 200)),
  ]);
  gate.release();

  assert.ok("event" in outcome, "text delta stayed buffered until stream completion");
  assert.equal(outcome.event.value.type, "text_delta");
  for await (const _event of stream) { /* drain and persist the trace */ }
});

test("resume and fork with fs store; verify via wasm", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "huggr-ts-test-"));
  const runtime = {
    loadWasm,
    traces: new FsTraceStore(path.join(home, "traces")),
    feedback: new FsFeedbackStore(path.join(home, "feedback")),
  };
  const agent = makeAgent({ runtime });

  server.scriptText('{"answer": "first"}');
  const first = await agent.ask("first question");
  assert.equal(first.status, "success");

  server.scriptText('{"answer": "second"}');
  const second = await agent.ask("follow-up", { traceId: first.trace_id });
  assert.equal(second.status, "success");
  assert.notEqual(second.trace_id, first.trace_id);
  // The resumed turn re-fed the parent conversation to the model.
  const resumed = server.requests.at(-1).messages;
  assert.ok(resumed.some((m) => JSON.stringify(m).includes("first question")));

  const heads = await agent.traces();
  const byId = Object.fromEntries(heads.map((h) => [h.trace_id, h]));
  assert.equal(byId[second.trace_id].depends_on, first.trace_id);

  // Both traces replay bit-for-bit through the wasm verify fold.
  await agent.verify(first.trace_id);
  await agent.verify(second.trace_id);
  fs.rmSync(home, { recursive: true, force: true });
});

test("failed resumed turns do not reuse the parent answer", async () => {
  const agent = makeAgent();
  server.scriptText('{"answer": "parent"}');
  const parent = await agent.ask("first question");

  const resumed = await agent.ask("follow-up", { traceId: parent.trace_id });

  assert.equal(resumed.status, "error");
  assert.match(String(resumed.response.error), /model did not produce a final answer/);
  assert.notDeepEqual(resumed.response, parent.response);
  await agent.verify(resumed.trace_id);
});

test("resumed turns restore the parent trace policy", async () => {
  const runtime = memRuntime();
  const parentAgent = makeAgent({ system: "Original system prompt.", runtime });
  server.scriptText('{"answer": "parent"}');
  const parent = await parentAgent.ask("first question");

  const changedAgent = makeAgent({ system: "Changed system prompt.", runtime });
  server.scriptText('{"answer": "child"}');
  const child = await changedAgent.ask("follow-up", { traceId: parent.trace_id });

  const systemMessage = server.requests.at(-1).messages.find((message) => message.role === "system");
  assert.equal(systemMessage.content, "Original system prompt.");
  assert.equal(child.status, "success");
  await changedAgent.verify(child.trace_id);
});

test("limits trip to error answers", async () => {
  const agent = makeAgent({ limits: { max_model_calls: 1 }, tools: [lookupTool([])] });
  server.scriptToolCall("lookup", { word: "x" });
  server.scriptText('{"answer": "never reached"}');
  const answer = await agent.ask("q");
  assert.equal(answer.status, "error");
  assert.ok(String(answer.response.error).includes("max_model_calls"));
});

test("provider-reported cost drives metadata and the spending cap", async () => {
  const agent = makeAgent({ limits: { max_cost_micro_usd: 40 }, tools: [lookupTool([])] });
  server.scriptToolCall("lookup", { word: "x" }, "call_1", {
    prompt_tokens: 7,
    completion_tokens: 3,
    cost: 0.000_050,
    cost_source: "router",
  });

  const answer = await agent.ask("q");

  assert.equal(answer.status, "error");
  assert.ok(String(answer.response.error).includes("max_cost_micro_usd"));
  assert.equal(answer.metadata.model_calls, 1);
  assert.equal(answer.metadata.cost_micro_usd, 50);
  assert.equal(server.requests.length, 1);
});

test("feedback round-trip", async () => {
  const agent = makeAgent();
  server.scriptText('{"answer": "x"}');
  const answer = await agent.ask("q");
  const fb = await agent.feedback(answer.trace_id, { score: 5 });
  assert.equal(fb.trace_id, answer.trace_id);
  const stored = await agent.feedbackFor(answer.trace_id);
  assert.deepEqual(stored.map((f) => f.payload), [{ score: 5 }]);
  await assert.rejects(() => agent.feedback("no-such-trace", { score: 0 }));
});

test("fs stores reject trace path traversal", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "huggr-ts-keys-"));
  const traces = new FsTraceStore(path.join(home, "traces"));
  const feedback = new FsFeedbackStore(path.join(home, "feedback"));
  await assert.rejects(() => traces.get("../outside"), /invalid trace id/);
  await assert.rejects(() => feedback.list("../outside"), /invalid trace id/);
  fs.rmSync(home, { recursive: true, force: true });
});

test("timeout interrupts a running tool and records cancellation", async () => {
  const agent = makeAgent({
    limits: { timeout_s: 0.05 },
    tools: [{ name: "slow", description: "d", schema: { type: "object" }, invoke: () => new Promise((resolve) => setTimeout(() => resolve({ ok: true }), 300)) }],
  });
  server.scriptToolCall("slow", {});
  const started = Date.now();
  const answer = await agent.ask("q");
  assert.equal(answer.status, "error");
  assert.ok(Date.now() - started < 250);
  await agent.verify(answer.trace_id);
});

test("createAgent defaults to the agent home layout", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "huggr-ts-home-"));
  process.env.HUGGR_HOME = home;
  try {
    const agent = createAgent({
      name: "ts-home-agent",
      system: "s",
      providers: { test: { base_url: server.baseUrl, api_key: "k" } },
      models: { default: "balanced", balanced: { provider: "test", model: "m" } },
    });
    server.scriptText('{"answer": "hi"}');
    const answer = await agent.ask("q");
    assert.equal(answer.status, "success");
    assert.ok(fs.existsSync(path.join(home, "ts-home-agent", "traces", `${answer.trace_id}.json`)));
  } finally {
    delete process.env.HUGGR_HOME;
    fs.rmSync(home, { recursive: true, force: true });
  }
});
