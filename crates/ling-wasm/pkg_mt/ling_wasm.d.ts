/* tslint:disable */
/* eslint-disable */

/**
 * [count, bytes] currently resident.
 */
export function asset_stats(): Float64Array;

/**
 * Resume the Web Audio AudioContext. Must be called from a user-gesture handler.
 */
export function audio_ready(): void;

/**
 * Clear the per-frame key press state. Call this at the start of each frame.
 */
export function clear_frame_keys(): void;

/**
 * Drop a streamed asset from memory (proximity eviction).
 */
export function drop_asset(path: string): void;

export function initThreadPool(num_threads: number): Promise<any>;

/**
 * Initialise the rendering context on the given OffscreenCanvas.
 * Must be called once before run_program.
 */
export function init_canvas(canvas: OffscreenCanvas): void;

/**
 * Notify Ling of a gamepad axis analog change from JS Gamepad API.
 */
export function on_gamepad_axis(index: number, axis: number, value: number): void;

/**
 * Notify Ling of a gamepad button state change from JS Gamepad API.
 */
export function on_gamepad_button(index: number, button: string, pressed: boolean, value: number): void;

/**
 * Notify Ling when a gamepad connects in JS.
 */
export function on_gamepad_connected(index: number, name: string): void;

/**
 * Notify Ling when a gamepad disconnects in JS.
 */
export function on_gamepad_disconnected(index: number): void;

/**
 * Notify Ling of a keydown event. Call this from JavaScript's keydown event listener.
 */
export function on_key_down(key: string): void;

/**
 * Notify Ling of a keyup event. Call this from JavaScript's keyup event listener.
 */
export function on_key_up(key: string): void;

/**
 * Notify Ling of mouse button press/release.
 */
export function on_mouse_button(button: number, pressed: boolean, x: number, y: number): void;

/**
 * Notify Ling of pointer movement.
 */
export function on_mouse_move(x: number, y: number): void;

/**
 * Typed, pasted or IME-composed text for `text_poll` (send "\u{8}" for Backspace).
 */
export function on_text_input(text: string): void;

/**
 * Queue raw PCM audio samples for playback through Web Audio API.
 */
export function play_audio_buffer(pcm_data: Float32Array, sample_rate: number): void;

/**
 * Hand a streamed asset's bytes to the runtime (answers an `asset_request`).
 */
export function register_asset(path: string, bytes: Uint8Array): void;

/**
 * Register a module source so that `use "path"` statements resolve on wasm32.
 * Call this for every module before calling `run_program`.
 */
export function register_module(path: string, source: string): void;

/**
 * Resize the game's framebuffer (applied at the next frame boundary).
 */
export function resize_canvas(w: number, h: number): void;

/**
 * Parse and execute a Ling source program.
 * All modules used via `use "..."` must have been registered with
 * `register_module` first (on wasm32).
 */
export function run_program(source: string): void;

/**
 * The physical display size (device px) the page shows the game on — read by
 * the `display_size()` builtin to size the UI layer for crisp pixel text.
 */
export function set_display(w: number, h: number): void;

/**
 * Gamepad `index` as the page sees it (the Gamepad API exists only on the page, not
 * in the game's worker): button values 0..1 in W3C standard order, axes [lx, ly, rx, ry].
 */
export function set_gamepad(index: number, connected: boolean, buttons: Float32Array, axes: Float32Array): void;

/**
 * Turn the WebGL2 triangle path on or off (default on; falls back to the CPU rasteriser by itself
 * if the shaders don't compile). Call before `run_program`.
 */
export function set_gpu_scene(on: boolean): void;

/**
 * Set master output volume (0.0 to 1.0) for Web Audio engine.
 */
export function set_master_volume(volume: number): void;

/**
 * Newline-separated paths the running program has requested since the last call.
 */
export function take_asset_requests(): string;

export class wbg_rayon_PoolBuilder {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    build(): void;
    mainJS(): string;
    numThreads(): number;
    receiver(): number;
}

export function wbg_rayon_start_worker(receiver: number): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly asset_stats: () => [number, number];
    readonly audio_ready: () => void;
    readonly clear_frame_keys: () => void;
    readonly drop_asset: (a: number, b: number) => void;
    readonly init_canvas: (a: any) => void;
    readonly on_gamepad_axis: (a: number, b: number, c: number) => void;
    readonly on_gamepad_button: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly on_gamepad_connected: (a: number, b: number, c: number) => void;
    readonly on_gamepad_disconnected: (a: number) => void;
    readonly on_key_down: (a: number, b: number) => void;
    readonly on_key_up: (a: number, b: number) => void;
    readonly on_mouse_button: (a: number, b: number, c: number, d: number) => void;
    readonly on_mouse_move: (a: number, b: number) => void;
    readonly on_text_input: (a: number, b: number) => void;
    readonly play_audio_buffer: (a: number, b: number, c: number) => void;
    readonly register_asset: (a: number, b: number, c: number, d: number) => void;
    readonly register_module: (a: number, b: number, c: number, d: number) => void;
    readonly resize_canvas: (a: number, b: number) => void;
    readonly run_program: (a: number, b: number) => void;
    readonly set_display: (a: number, b: number) => void;
    readonly set_gamepad: (a: number, b: number, c: number, d: number, e: number, f: number) => void;
    readonly set_gpu_scene: (a: number) => void;
    readonly set_master_volume: (a: number) => void;
    readonly take_asset_requests: () => [number, number];
    readonly __wbg_wbg_rayon_poolbuilder_free: (a: number, b: number) => void;
    readonly initThreadPool: (a: number) => any;
    readonly wbg_rayon_poolbuilder_build: (a: number) => void;
    readonly wbg_rayon_poolbuilder_mainJS: (a: number) => any;
    readonly wbg_rayon_poolbuilder_numThreads: (a: number) => number;
    readonly wbg_rayon_poolbuilder_receiver: (a: number) => number;
    readonly wbg_rayon_start_worker: (a: number) => void;
    readonly memory: WebAssembly.Memory;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_thread_destroy: (a?: number, b?: number, c?: number) => void;
    readonly __wbindgen_start: (a: number) => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput, memory?: WebAssembly.Memory, thread_stack_size?: number }} module - Passing `SyncInitInput` directly is deprecated.
 * @param {WebAssembly.Memory} memory - Deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput, memory?: WebAssembly.Memory, thread_stack_size?: number } | SyncInitInput, memory?: WebAssembly.Memory): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput>, memory?: WebAssembly.Memory, thread_stack_size?: number }} module_or_path - Passing `InitInput` directly is deprecated.
 * @param {WebAssembly.Memory} memory - Deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput>, memory?: WebAssembly.Memory, thread_stack_size?: number } | InitInput | Promise<InitInput>, memory?: WebAssembly.Memory): Promise<InitOutput>;
