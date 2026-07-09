/* tslint:disable */
/* eslint-disable */

export class HugrWasm {
    free(): void;
    [Symbol.dispose](): void;
    abort(now_ms: number): string;
    capabilities_json(): string;
    config_json(): string;
    final_text(): string;
    log_json(): string;
    constructor(config_json?: string | null);
    poll_commands_json(): string;
    submit_capability_done(op: number, result_json: string, now_ms: number): string;
    submit_capability_error(op: number, error_json: string, now_ms: number): string;
    submit_model_done(op: number, output_json: string, usage_json: string, est_tokens: number, now_ms: number): string;
    submit_model_error(op: number, error_json: string, now_ms: number): string;
    submit_permission_decision(op: number, allow: boolean, reason: string | null | undefined, now_ms: number): string;
    submit_user_input(text: string, now_ms: number): string;
    system_prompt(): string;
    tool_schemas_json(): string;
    trace_json(): string;
}

export function default_config_json(): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_hugrwasm_free: (a: number, b: number) => void;
    readonly default_config_json: (a: number) => void;
    readonly hugrwasm_abort: (a: number, b: number, c: number) => void;
    readonly hugrwasm_capabilities_json: (a: number, b: number) => void;
    readonly hugrwasm_config_json: (a: number, b: number) => void;
    readonly hugrwasm_final_text: (a: number, b: number) => void;
    readonly hugrwasm_log_json: (a: number, b: number) => void;
    readonly hugrwasm_new: (a: number, b: number, c: number) => void;
    readonly hugrwasm_poll_commands_json: (a: number, b: number) => void;
    readonly hugrwasm_submit_capability_done: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly hugrwasm_submit_capability_error: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly hugrwasm_submit_model_done: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number) => void;
    readonly hugrwasm_submit_model_error: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly hugrwasm_submit_permission_decision: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly hugrwasm_submit_user_input: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly hugrwasm_system_prompt: (a: number, b: number) => void;
    readonly hugrwasm_tool_schemas_json: (a: number, b: number) => void;
    readonly hugrwasm_trace_json: (a: number, b: number) => void;
    readonly __wbindgen_add_to_stack_pointer: (a: number) => number;
    readonly __wbindgen_export: (a: number, b: number, c: number) => void;
    readonly __wbindgen_export2: (a: number, b: number) => number;
    readonly __wbindgen_export3: (a: number, b: number, c: number, d: number) => number;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
