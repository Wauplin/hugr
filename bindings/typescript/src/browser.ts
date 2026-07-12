// Browser runtime pieces: wasm loader over fetch and an IndexedDB-backed
// trace + feedback store implementing the same storage seams as the Node fs
// stores.

import type { AgentConfig, Feedback, FeedbackStore, Json, TraceHead, TraceHeader, TraceStore } from "./contract.js";
import { Agent, contentId, headOf, stampHeader, type AgentRuntime, type WasmModule } from "./agent.js";

let cachedWasm: Promise<WasmModule> | null = null;

/// Load the wasm-bindgen (web target) output from a URL (defaults to the
/// package's pkg/ next to the built module).
export function loadWasm(pkgUrl?: string): Promise<WasmModule> {
  cachedWasm ??= (async () => {
    const base = pkgUrl ?? new URL("../pkg/", import.meta.url).href;
    const module = await import(/* webpackIgnore: true */ new URL("huggr_wasm.js", base).href);
    await module.default({ module_or_path: fetch(new URL("huggr_wasm_bg.wasm", base)) });
    return module as unknown as WasmModule;
  })();
  return cachedWasm;
}

const DB_VERSION = 1;
const TRACES = "traces";
const FEEDBACK = "feedback";

function openDb(name: string): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(name, DB_VERSION);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(TRACES)) db.createObjectStore(TRACES);
      if (!db.objectStoreNames.contains(FEEDBACK)) db.createObjectStore(FEEDBACK);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function requestAsPromise<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

/// IndexedDB trace store (one database per agent, keyed by trace id).
export class IndexedDbTraceStore implements TraceStore {
  private db: Promise<IDBDatabase>;

  constructor(agentName: string) {
    this.db = openDb(`huggr:${agentName}`);
  }

  async put(trace: Json, header: TraceHeader): Promise<string> {
    const db = await this.db;
    const stamped = stampHeader(trace, header);
    const base = await contentId(stamped);
    let id = base;
    let counter = 1;
    for (;;) {
      const existing = await requestAsPromise(db.transaction(TRACES).objectStore(TRACES).getKey(id));
      if (existing === undefined) break;
      id = `${base}-${counter}`;
      counter += 1;
    }
    (stamped.meta as Record<string, Json>).trace_id = id;
    await requestAsPromise(db.transaction(TRACES, "readwrite").objectStore(TRACES).add(stamped as Json, id));
    return id;
  }

  async get(id: string): Promise<Json> {
    const db = await this.db;
    const trace = await requestAsPromise(db.transaction(TRACES).objectStore(TRACES).get(id));
    if (trace === undefined) throw new Error(`trace not found: ${id}`);
    return trace as Json;
  }

  async list(): Promise<TraceHead[]> {
    const db = await this.db;
    const store = db.transaction(TRACES).objectStore(TRACES);
    const [keys, values] = await Promise.all([
      requestAsPromise(store.getAllKeys()),
      requestAsPromise(store.getAll()),
    ]);
    return keys
      .map((key, index) => headOf(String(key), values[index] as Json))
      .sort((a, b) => a.trace_id.localeCompare(b.trace_id));
  }
}

export class IndexedDbFeedbackStore implements FeedbackStore {
  private db: Promise<IDBDatabase>;

  constructor(agentName: string) {
    this.db = openDb(`huggr:${agentName}`);
  }

  async append(feedback: Feedback): Promise<void> {
    const db = await this.db;
    const store = db.transaction(FEEDBACK, "readwrite").objectStore(FEEDBACK);
    const existing = ((await requestAsPromise(store.get(feedback.trace_id))) as Feedback[] | undefined) ?? [];
    existing.push(feedback);
    await requestAsPromise(store.put(existing, feedback.trace_id));
  }

  async list(traceId: string): Promise<Feedback[]> {
    const db = await this.db;
    const store = db.transaction(FEEDBACK).objectStore(FEEDBACK);
    return ((await requestAsPromise(store.get(traceId))) as Feedback[] | undefined) ?? [];
  }
}

/// The default browser runtime for an agent: IndexedDB stores, wasm over fetch.
export function browserRuntime(agentName: string, pkgUrl?: string): AgentRuntime {
  return {
    loadWasm: () => loadWasm(pkgUrl),
    traces: new IndexedDbTraceStore(agentName),
    feedback: new IndexedDbFeedbackStore(agentName),
  };
}

export function createAgent(config: AgentConfig, runtime?: Partial<AgentRuntime>): Agent {
  const defaults = browserRuntime(config.name);
  return new Agent(config, { ...defaults, ...runtime });
}
