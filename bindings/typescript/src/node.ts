// Node runtime pieces: wasm loader (from the package's pkg/ output),
// fs-backed trace + feedback stores under the shared `~/.huggr/<agent>/` home.

import { promises as fs } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import type { AgentConfig, Feedback, FeedbackStore, Json, TraceHead, TraceHeader, TraceStore } from "./contract.js";
import { Agent, contentId, headOf, stampHeader, type AgentRuntime, type WasmModule } from "./agent.js";

let cachedWasm: Promise<WasmModule> | null = null;

/// Load the wasm-bindgen (web target) output from ./pkg, initialized from
/// bytes — no fetch, works in plain Node.
export function loadWasm(): Promise<WasmModule> {
  cachedWasm ??= (async () => {
    const pkgDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "pkg");
    const jsPath = path.join(pkgDir, "huggr_wasm.js");
    const wasmPath = path.join(pkgDir, "huggr_wasm_bg.wasm");
    const module = await import(pathToFileURL(jsPath).href);
    await module.default({ module_or_path: await fs.readFile(wasmPath) });
    return module as unknown as WasmModule;
  })();
  return cachedWasm;
}

/// Per-agent home: `$HUGGR_AGENT_HOME`, else `$HUGGR_HOME/<name>`, else
/// `~/.huggr/<name>` — the same resolution as the Rust runtime.
export function agentHome(agentName: string): string {
  const explicit = process.env.HUGGR_AGENT_HOME;
  if (explicit) return explicit;
  const name = sanitizeAgentName(agentName);
  const base = process.env.HUGGR_HOME;
  if (base) return path.join(base, name);
  return path.join(process.env.HOME ?? os.tmpdir(), ".huggr", name);
}

export function sanitizeAgentName(name: string): string {
  const cleaned = [...name].map((c) => (/[a-zA-Z0-9._-]/.test(c) ? c : "_")).join("");
  return !cleaned || cleaned === "." || cleaned === ".." ? "agent" : cleaned;
}

function validateTraceId(id: string): string {
  if (!/^[A-Za-z0-9_-]+$/.test(id)) throw new Error("invalid trace id");
  return id;
}

/// Traces as `<root>/<id>.json` in the portable trace format — the same
/// layout as the Rust `TraceStore`, so `huggr verify` reads them directly.
export class FsTraceStore implements TraceStore {
  constructor(readonly root: string) {}

  pathOf(id: string): string {
    return path.join(this.root, `${validateTraceId(id)}.json`);
  }

  async put(trace: Json, header: TraceHeader): Promise<string> {
    await fs.mkdir(this.root, { recursive: true });
    const stamped = stampHeader(trace, header);
    const base = await contentId(stamped);
    let id = base;
    let counter = 1;
    // wx = atomic claim; a collision bumps the -N suffix (immutability holds).
    for (;;) {
      try {
        await fs.writeFile(this.pathOf(id), "", { flag: "wx" });
        break;
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
        id = `${base}-${counter}`;
        counter += 1;
      }
    }
    (stamped.meta as Record<string, Json>).trace_id = id;
    const tmp = `${this.pathOf(id)}.tmp`;
    await fs.writeFile(tmp, JSON.stringify(stamped));
    await fs.rename(tmp, this.pathOf(id));
    return id;
  }

  async get(id: string): Promise<Json> {
    try {
      return JSON.parse(await fs.readFile(this.pathOf(id), "utf8")) as Json;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") throw new Error(`trace not found: ${id}`);
      throw error;
    }
  }

  async list(): Promise<TraceHead[]> {
    let entries: string[];
    try {
      entries = await fs.readdir(this.root);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
      throw error;
    }
    const heads: TraceHead[] = [];
    for (const entry of entries.filter((name) => name.endsWith(".json")).sort()) {
      const id = entry.slice(0, -".json".length);
      // One corrupt file (an interrupted write, stray junk) must not hide the
      // healthy traces from the listing.
      try {
        heads.push(headOf(id, await this.get(id)));
      } catch (error) {
        console.warn(`skipping unreadable trace ${id}: ${(error as Error).message}`);
      }
    }
    return heads;
  }
}

/// Append-only feedback sidecar: `<root>/<trace_id>.jsonl`, one JSON line per
/// feedback event — the same layout as the Rust `FsFeedbackStore`.
export class FsFeedbackStore implements FeedbackStore {
  constructor(readonly root: string) {}

  async append(feedback: Feedback): Promise<void> {
    await fs.mkdir(this.root, { recursive: true });
    await fs.appendFile(path.join(this.root, `${validateTraceId(feedback.trace_id)}.jsonl`), `${JSON.stringify(feedback)}\n`);
  }

  async list(traceId: string): Promise<Feedback[]> {
    try {
      const raw = await fs.readFile(path.join(this.root, `${validateTraceId(traceId)}.jsonl`), "utf8");
      return raw
        .split("\n")
        .filter(Boolean)
        .map((line) => JSON.parse(line) as Feedback);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
      throw error;
    }
  }
}

/// The default Node runtime for an agent: fs stores under the agent home,
/// wasm from ./pkg, `api_key_env` resolved from process.env.
export function nodeRuntime(agentName: string, home?: string): AgentRuntime {
  const root = home ?? agentHome(agentName);
  return {
    loadWasm,
    traces: new FsTraceStore(path.join(root, "traces")),
    feedback: new FsFeedbackStore(path.join(root, "feedback")),
    env: (name) => process.env[name],
  };
}

/// Convenience assembly: `createAgent(config)` with the default Node runtime.
export function createAgent(config: AgentConfig, runtime?: Partial<AgentRuntime>): Agent {
  const defaults = nodeRuntime(config.name);
  return new Agent(config, { ...defaults, ...runtime });
}
