// The driver loop — the browser analogue of baton-host's Engine (engine.rs).
// This is the ENTIRE integration surface (ARCHITECTURE §2.3): drain the
// commands the brain wants performed, spawn one async task per in-flight op,
// await the next event from any source, feed it back in, repeat. The brain
// stays synchronous and pure inside the WASM module; all the concurrency
// (streaming fetches, tab tools, permission prompts) lives out here.

import { callModel } from "./model.js";
import { TOOL_SCHEMAS, PERMISSIONED } from "./schemas.js";

export const SYSTEM_PROMPT = `You are Baton, a helpful browser agent running inside a Chrome side panel.
You can observe web pages (read their text, links, headings, and interactive elements) and manage tabs (list, open, navigate, activate, close).
You CANNOT click buttons, type into fields, or submit forms — you can only read and navigate. If a task needs clicking, explain what the user would need to do.
Prefer concrete actions over long explanations. Use list_tabs / get_current_page to orient yourself, then read or navigate as needed.
Before doing several navigation steps, consider calling show_plan first, and ask_user_confirmation before anything the user might not expect. When done, give a short summary.`;

/** Build the brain's StaticPolicy config from the current settings. */
export function buildPolicy(config) {
  return {
    model: { Named: "big" },
    tools: TOOL_SCHEMAS,
    permissioned: PERMISSIONED,
    background: [],
    // `agents` is #[serde(default)] in StaticPolicy, so we can omit it.
    params: { temperature: config.temperature, max_tokens: null },
    system: SYSTEM_PROMPT,
  };
}

export class Engine {
  /**
   * @param {object} deps
   * @param {object} deps.brain    - a BatonBrain instance (WASM)
   * @param {object} deps.config   - the loaded Config
   * @param {object} deps.tools    - name -> async(args) capability table
   * @param {object} deps.frontend - rendering + permission/ask callbacks
   */
  constructor({ brain, config, tools, frontend }) {
    this.brain = brain;
    this.config = config;
    this.tools = tools;
    this.frontend = frontend;

    /** @type {Array<any>} pending event values (serde-shaped) */
    this.queue = [];
    /** @type {Array<(v:any)=>void>} resolvers awaiting an event */
    this.waiters = [];
    /** @type {Map<number, AbortController>} per-op cancellation handles */
    this.aborters = new Map();
    /** @type {Map<number, string>} per-op capability label, for result rendering */
    this.opLabels = new Map();
  }

  now() {
    return Date.now();
  }

  /** Push an event value onto the ordered inbox (the host's event merge). */
  pushEvent(value) {
    const w = this.waiters.shift();
    if (w) w(value);
    else this.queue.push(value);
  }

  /** Await the next event from any source. The only await, host-side. */
  nextEvent() {
    if (this.queue.length) return Promise.resolve(this.queue.shift());
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  /** Feed one event into the brain, stamping a Tick first (§6.1: injected time). */
  feed(value) {
    this.brain.submit(JSON.stringify({ Tick: { now: this.now() } }));
    this.brain.submit(JSON.stringify(value));
  }

  /** Submit a user message and drive the resulting turn to completion. */
  async userTurn(text) {
    this.feed({ UserInput: { content: text, mode: "Queue" } });
    await this.driveToIdle();
  }

  /** Inject a UserAbort from outside a turn (the ESC / stop button). */
  abort() {
    // UserAbort is a unit enum variant → the bare string "UserAbort".
    this.pushEvent("UserAbort");
  }

  /** Process commands and events until nothing is in flight (turn complete). */
  async driveToIdle() {
    for (;;) {
      // Drain and perform every queued command. Performing one may queue more
      // (a tool result resuming the model), so loop until empty.
      for (;;) {
        const commands = JSON.parse(this.brain.poll());
        if (commands.length === 0) break;
        for (const cmd of commands) this.perform(cmd);
      }

      if (this.brain.inflightLen() === 0) {
        this.frontend.flush?.();
        break;
      }

      const event = await this.nextEvent();
      this.observe(event);
      this.feed(event);
    }
  }

  /** Perform a single command. Effectful ones spawn a task and return at once. */
  perform(cmd) {
    // Unit-variant commands arrive as bare strings.
    if (cmd === "Checkpoint") return;
    const [type, body] = Object.entries(cmd)[0];
    switch (type) {
      case "StartModelCall":
        return this.startModel(body);
      case "StartCapability":
        return this.startCapability(body);
      case "RequestPermission":
        return this.requestPermission(body);
      case "AskUser":
        return this.askUser(body);
      case "Cancel":
        return this.cancel(body);
      case "Emit":
        return this.frontend.onOutput(body);
      case "Done":
        return this.frontend.onDone(body.reason);
      case "StartAgent":
        // This host doesn't run sub-agents; surface it as a semantic error so
        // the turn resolves instead of hanging.
        return this.pushEvent({
          AgentError: { op: body.op, error: { error: "sub-agents are not supported in the browser host" } },
        });
      default:
        console.warn("baton: unhandled command", type, body);
    }
  }

  /** Report a completion event to the front-end before the brain folds it. */
  observe(event) {
    if (typeof event === "string") return; // e.g. "UserAbort"
    const [type, body] = Object.entries(event)[0];
    switch (type) {
      case "ModelDone":
        return this.frontend.onModelEnd(body.op, body.usage);
      case "CapabilityDone":
        return this.frontend.onToolEnd(body.op, this.opLabels.get(body.op) || "", body.result, false);
      case "CapabilityError":
        return this.frontend.onToolEnd(body.op, this.opLabels.get(body.op) || "", body.error, true);
      default:
    }
  }

  // --- command handlers ----------------------------------------------------

  startModel({ op, model, request }) {
    const controller = new AbortController();
    this.aborters.set(op, controller);
    this.frontend.onModelStart(op, model);
    callModel(request, this.config, {
      onText: (t) => this.pushEvent({ ModelDelta: { op, delta: { Text: t } } }),
      onReasoning: (t) => this.pushEvent({ ModelDelta: { op, delta: { Reasoning: t } } }),
      signal: controller.signal,
    })
      .then(({ output, usage }) => {
        this.aborters.delete(op);
        this.pushEvent({ ModelDone: { op, output, usage } });
      })
      .catch((e) => {
        this.aborters.delete(op);
        // A cancellation already produced an OpCancelled; don't also error.
        if (controller.signal.aborted) return;
        this.pushEvent({ ModelError: { op, error: { message: String(e?.message || e) } } });
      });
  }

  startCapability({ op, name, args }) {
    this.frontend.onToolStart(op, name, args);
    this.opLabels.set(op, name);
    const tool = this.tools[name];
    if (!tool) {
      this.pushEvent({ CapabilityError: { op, error: { error: `unknown capability: ${name}` }, conflict: null } });
      return;
    }
    Promise.resolve()
      .then(() => tool(args))
      .then((result) => {
        this.pushEvent({ CapabilityDone: { op, result: result ?? null, version: null } });
      })
      .catch((e) => {
        this.pushEvent({ CapabilityError: { op, error: { error: String(e?.message || e) }, conflict: null } });
      });
  }

  requestPermission({ op, request }) {
    (async () => {
      let decision;
      if (this.config.autoApprove) {
        decision = "Allow";
      } else {
        const ok = await this.frontend.confirmPermission(request.capability, request.args);
        decision = ok ? "Allow" : { Deny: { reason: "user declined" } };
      }
      this.frontend.onPermission?.(request.capability, decision);
      this.pushEvent({ PermissionDecision: { op, decision } });
    })();
  }

  askUser({ op, prompt }) {
    (async () => {
      const answer = await this.frontend.ask(prompt.message);
      this.pushEvent({ UserAnswer: { op, answer } });
    })();
  }

  cancel({ op }) {
    const c = this.aborters.get(op);
    if (c) {
      c.abort();
      this.aborters.delete(op);
    }
    this.pushEvent({ OpCancelled: { op } });
  }
}
