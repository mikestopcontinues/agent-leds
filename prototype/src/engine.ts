// The firmware's own LEDS.LED interpreter, compiled to WebAssembly by the
// SidePulse project (vendor/sdled.wasm). Running programs through it shows
// what the device will do, including its parse errors.

import type { RGB } from "./color";

export const LED_COUNT = 8;
const INPUT_BYTES = 512;

const ERRORS = [
  "ok",
  "null-input",
  "too-long",
  "too-many-lines",
  "too-many-animation-lines",
  "syntax",
  "bad-color",
  "bad-index",
  "bad-time",
  "bad-brightness",
  "bad-repeat",
  "trailing-input",
];

type Exports = {
  memory: WebAssembly.Memory;
  sdled_input_ptr(): number;
  sdled_output_ptr(): number;
  sdled_reset(leds: number, nowMs: number): void;
  sdled_parse(leds: number, length: number, nowMs: number): number;
  sdled_step(leds: number, nowMs: number): void;
};

export type ParseResult = { ok: true } | { ok: false; error: string; line: number; column: number };

export class Engine {
  private constructor(private wasm: Exports) {}

  static async load(bytes: BufferSource): Promise<Engine> {
    const instance = await WebAssembly.instantiate(await WebAssembly.compile(bytes), {});
    const engine = new Engine(instance.exports as unknown as Exports);
    engine.wasm.sdled_reset(LED_COUNT, 0);
    return engine;
  }

  /** Starts a program from the lights' current state, as writing LEDS.LED does. */
  parse(program: string, nowMs: number): ParseResult {
    const bytes = new TextEncoder().encode(program);
    const input = new Uint8Array(this.wasm.memory.buffer, this.wasm.sdled_input_ptr(), INPUT_BYTES);
    input.fill(0);
    input.set(bytes.subarray(0, INPUT_BYTES));
    const packed = this.wasm.sdled_parse(LED_COUNT, Math.min(bytes.length, INPUT_BYTES + 1), Math.floor(nowMs));
    if (packed & 1) return { ok: true };
    const code = (packed >>> 8) & 255;
    return { ok: false, error: ERRORS[code] ?? `error-${code}`, line: (packed >>> 16) & 255, column: packed >>> 24 };
  }

  /** The value of each light at a moment in time. */
  step(nowMs: number): RGB[] {
    this.wasm.sdled_step(LED_COUNT, Math.floor(nowMs));
    const output = new Uint8Array(this.wasm.memory.buffer, this.wasm.sdled_output_ptr(), LED_COUNT * 3);
    return Array.from({ length: LED_COUNT }, (_, led) => [...output.subarray(led * 3, led * 3 + 3)] as RGB);
  }
}
