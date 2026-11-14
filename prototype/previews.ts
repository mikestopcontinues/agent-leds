// Draws the README's previews: small animated WebP strips of what the card
// shows, rendered by the `agent-leds` on the PATH and run through the
// firmware's own interpreter, as the workshop does. Run `bun run previews`
// after a change to rendering, the default settings or the palettes.

import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEFAULT_MODEL, appearance, type RGB } from "./src/color";
import { Engine, LED_COUNT } from "./src/engine";
import type { Config, Rendering, Scene, State } from "./src/service";

const OUT = join(import.meta.dir, "../docs/previews");
/** Pixels per point: the images are drawn at twice the size they are shown. */
const SCALE = 2;
const LIGHT = 22 * SCALE;
const GAP = 10 * SCALE;
const PAD = 9 * SCALE;
const WIDTH = LED_COUNT * LIGHT + (LED_COUNT - 1) * GAP + 2 * PAD;
const HEIGHT = LIGHT + 2 * PAD;
const FRAME_MS = 40;
const BACKGROUND = 0.006;
const UNLIT = 0.012;

async function render(scene: Scene): Promise<Rendering> {
  const child = Bun.spawn(["agent-leds", "render"], { stdin: new Blob([JSON.stringify(scene)]), stdout: "pipe" });
  const output = await new Response(child.stdout).text();
  if ((await child.exited) !== 0) throw new Error(`agent-leds render failed for ${JSON.stringify(scene)}`);
  return JSON.parse(output);
}

const decode = (value: number) => (value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4);
const encode = (value: number) => (value <= 0.0031308 ? value * 12.92 : 1.055 * value ** (1 / 2.4) - 0.055);

/** How much of a rounded square of `size` at (x, y) covers the pixel at (px, py), with soft edges. */
function covered(px: number, py: number, x: number, y: number, size: number, radius: number): number {
  const dx = Math.max(Math.abs(px - (x + size / 2)) - (size / 2 - radius), 0);
  const dy = Math.max(Math.abs(py - (y + size / 2)) - (size / 2 - radius), 0);
  return Math.min(1, Math.max(0, radius - Math.hypot(dx, dy) + 0.5));
}

/** Per light, how much of each pixel its face covers. */
const faces = Array.from({ length: LED_COUNT }, (_, led) => {
  const x = PAD + led * (LIGHT + GAP);
  return Float32Array.from({ length: WIDTH * HEIGHT }, (_, at) =>
    covered((at % WIDTH) + 0.5, Math.floor(at / WIDTH) + 0.5, x, PAD, LIGHT, 4 * SCALE),
  );
});

/** How much of the card covers a pixel: its rounded corners are left clear. */
function rounded(at: number): number {
  const px = (at % WIDTH) + 0.5;
  const py = Math.floor(at / WIDTH) + 0.5;
  const radius = 8 * SCALE;
  const dx = Math.max(Math.abs(px - WIDTH / 2) - (WIDTH / 2 - radius), 0);
  const dy = Math.max(Math.abs(py - HEIGHT / 2) - (HEIGHT / 2 - radius), 0);
  return Math.min(1, Math.max(0, radius - Math.hypot(dx, dy) + 0.5));
}

/** One frame as RGBA bytes, from the value of each light. */
function draw(frame: RGB[]): Uint8Array {
  const light = frame.map((rgb) => appearance(rgb, DEFAULT_MODEL, "srgb").map(decode));
  const pixels = new Uint8Array(WIDTH * HEIGHT * 4);
  for (let at = 0; at < WIDTH * HEIGHT; at++) {
    const value = [BACKGROUND, BACKGROUND, BACKGROUND];
    for (let led = 0; led < LED_COUNT; led++) {
      const face = faces[led]![at]!;
      const shown = light[led]!;
      for (let channel = 0; channel < 3; channel++) {
        value[channel]! += face * Math.max(shown[channel]!, UNLIT - BACKGROUND);
      }
    }
    for (let channel = 0; channel < 3; channel++) {
      pixels[at * 4 + channel] = Math.round(Math.min(1, encode(value[channel]!)) * 255);
    }
    pixels[at * 4 + 3] = Math.round(rounded(at) * 255);
  }
  return pixels;
}

function pam(pixels: Uint8Array): Uint8Array {
  const header = `P7\nWIDTH ${WIDTH}\nHEIGHT ${HEIGHT}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n`;
  return new Uint8Array([...new TextEncoder().encode(header), ...pixels]);
}

async function run(command: string[]): Promise<void> {
  const child = Bun.spawn(command, { stdout: "ignore", stderr: "pipe" });
  if ((await child.exited) !== 0) throw new Error(`${command[0]} failed: ${await new Response(child.stderr).text()}`);
}

/** Writes frames, each shown for its duration, as an animated WebP that loops. */
async function animate(name: string, frames: { pixels: Uint8Array; ms: number }[]): Promise<void> {
  const folder = await mkdtemp(join(tmpdir(), "agent-leds-previews-"));
  const args = ["img2webp", "-loop", "0", "-min_size"];
  for (const [index, frame] of frames.entries()) {
    const file = join(folder, `${index}.pam`);
    await Bun.write(file, pam(frame.pixels));
    args.push("-lossy", "-q", "75", "-m", "6", "-d", String(frame.ms), file);
  }
  await run([...args, "-o", join(OUT, `${name}.webp`)]);
  await rm(folder, { recursive: true });
}

async function still(name: string, pixels: Uint8Array): Promise<void> {
  const folder = await mkdtemp(join(tmpdir(), "agent-leds-previews-"));
  const file = join(folder, "frame.pam");
  await Bun.write(file, pam(pixels));
  await run(["cwebp", "-quiet", "-lossless", "-z", "9", file, "-o", join(OUT, `${name}.webp`)]);
  await rm(folder, { recursive: true });
}

const engine = await Engine.load(await Bun.file(join(import.meta.dir, "vendor/sdled.wasm")).arrayBuffer());

/** The frames of a change: the old queue held, the change as the service writes it, then the new queue held. */
async function change(from: State[], to: State[], announce: State | null, config: Config) {
  const before = await render({ states: from, announce: null, from: [], config });
  const after = await render({ states: to, announce, from, config });
  const programs = [{ program: before.steady, ms: 900 }, ...after.steps, { program: after.steady, ms: 1600 }];
  const frames: { pixels: Uint8Array; ms: number }[] = [];
  let start = 0;
  for (const { program, ms } of programs) {
    const parsed = engine.parse(program, start);
    if (!parsed.ok) throw new Error(`the firmware refused a program: ${parsed.error}`);
    for (let at = start; at < start + ms; at += FRAME_MS) {
      const pixels = draw(engine.step(at));
      const last = frames.at(-1);
      if (last && Bun.deepEquals(last.pixels, pixels)) last.ms += FRAME_MS;
      else frames.push({ pixels, ms: FRAME_MS });
    }
    start += ms;
  }
  return frames;
}

const defaults: Config = (await render({ states: [], announce: null, from: [] })).config;
await mkdir(OUT, { recursive: true });

const changes: [string, State[], State[], State | null][] = [
  ["stopped", ["working", "working"], ["stopped", "working", "working"], "stopped"],
  ["question", ["stopped", "working", "working"], ["question", "stopped", "working", "working"], "question"],
  ["error", ["working", "working"], ["error", "working"], "error"],
  ["read", ["stopped", "working", "working"], ["working", "working"], null],
];
for (const [name, from, to, announce] of changes) {
  await animate(name, await change(from, to, announce, defaults));
  console.log(`${name}.webp`);
}

const palettes: { name: string; colors: Config["colors"] }[] = JSON.parse(
  await new Response(Bun.spawn(["agent-leds", "palette", "--json"], { stdout: "pipe" }).stdout).text(),
);
for (const palette of palettes) {
  const { steady } = await render({
    states: ["error", "question", "stopped", "working"],
    announce: null,
    from: [],
    config: { ...defaults, colors: palette.colors },
  });
  engine.parse(steady, 0);
  await still(`palette-${palette.name}`, draw(engine.step(5000)));
  console.log(`palette-${palette.name}.webp`);
}
