// Runs what the installed service and the sketches render through the
// firmware's interpreter, so a program the device would reject fails here.

import { expect, test } from "bun:test";
import { fromHex, type RGB } from "./color";
import { DESIGNS, STATES, scaled, type Rendering, type Scene, type State } from "./designs";
import { Engine } from "./engine";

const engine = await Engine.load(await Bun.file(`${import.meta.dir}/../vendor/sdled.wasm`).arrayBuffer());
let clock = 0;

const COLORS = { error: "#FF0060", question: "#FFA000", stopped: "#00FF80", working: "#3000FF" };
const BLACK: RGB = [0, 0, 0];
const rgb = (hex: string) => fromHex(hex)!;

function scene(states: State[], announce: State | null = null, front_led = 7, brightness = 255): Scene {
  return { states, announce, colors: COLORS, brightness, front_led, balance: [0.32, 0.54, 0.14] };
}

async function service(input: Scene): Promise<Rendering> {
  const child = Bun.spawn(["leds", "render"], { stdin: new Blob([JSON.stringify(input)]), stdout: "pipe" });
  return JSON.parse(await new Response(child.stdout).text());
}

/** Starts a program and returns the lights at each requested time after it. */
function play(program: string, ...at: number[]): RGB[][] {
  expect(new TextEncoder().encode(program).length).toBeLessThanOrEqual(512);
  expect(engine.parse(program, clock)).toEqual({ ok: true });
  const start = clock;
  clock += 60_000;
  return at.map((ms) => engine.step(start + ms));
}

const design = (id: string) => DESIGNS.find((candidate) => candidate.id === id)!;
const many = (count: number, state: State) => Array<State>(count).fill(state);
const OVERFLOW = [...many(9, "error"), ...many(9, "question"), ...many(9, "stopped"), ...many(9, "working")];

test("the service holds the queue with its front on the front light", async () => {
  const { steady, lit } = await service(scene(["working", "stopped", "error", "question"]));
  const [far] = play(steady, 1000);
  expect(far).toEqual([BLACK, BLACK, BLACK, BLACK, rgb(lit.working), rgb(lit.stopped), rgb(lit.question), rgb(lit.error)]);

  const near = await service(scene(["working", "error"], null, 0));
  expect(play(near.steady, 1000)[0]!.slice(0, 3)).toEqual([rgb(near.lit.error), rgb(near.lit.working), BLACK]);
});

test("the states are sent equally bright, limited by the dimmest hue", async () => {
  const { lit } = await service(scene([]));
  const brightness = (hex: string) => rgb(hex).reduce((sum, value, die) => sum + (value / 255) * [0.32, 0.54, 0.14][die]!, 0);
  const levels = STATES.map((state) => brightness(lit[state]));
  for (const level of levels) expect(level).toBeCloseTo(levels[0]!, 2);
  // Violet is the dimmest hue in this palette, so it is the one left at full strength.
  expect(lit.working).toBe(COLORS.working);
});

test("a change fades out, slides the new color toward the front, then fades the queue in", async () => {
  const { announce, steady, lit, announce_ms } = await service(scene(["stopped", "question"], "question"));
  play(`${lit.stopped} ${lit.stopped}`, 100);
  const [dark, entering, leaving, settled] = play(announce!, 300, 580, 1700, announce_ms + 50);
  expect(dark).toEqual(Array(8).fill(BLACK));
  // The slide starts at the near end (light 0) and has not reached the far end.
  expect(entering![0]).toEqual(rgb(lit.question));
  expect(entering![7]).toEqual(BLACK);
  // Later it is at the far end and the near end is dark again.
  expect(leaving![7]).toEqual(rgb(lit.question));
  expect(leaving![0]).toEqual(BLACK);
  expect(settled).toEqual([...Array(6).fill(BLACK), rgb(lit.stopped), rgb(lit.question)]);
  // Writing the steady program afterwards changes nothing.
  expect(play(steady, 1000)[0]).toEqual(settled!);
});

test("a stacked light breathes and the others hold still", async () => {
  const { slots, steady } = await service(scene(many(9, "working"), null, 0));
  expect(slots.map((slot) => slot.count)).toEqual([...Array(7).fill(1), 2]);
  const frames = play(steady, ...Array.from({ length: 40 }, (_, step) => 500 + step * 100));
  const blues = (led: number) => new Set(frames.map((frame) => frame[led]![2]));
  expect(blues(0).size).toBe(1);
  expect(blues(7).size).toBeGreaterThan(5);
});

test("every design's programs are accepted by the firmware in the hardest cases", async () => {
  for (const states of [[], ["error"], STATES.slice(), many(8, "working"), OVERFLOW] as State[][]) {
    for (const announce of [null, ...STATES]) {
      for (const front_led of [0, 7]) {
        const input = scene(states, announce, front_led, 128);
        const rendered = await service(input);
        for (const candidate of DESIGNS) {
          const result = candidate.adapt(input, rendered);
          if (result.announce) play(result.announce, result.announce_ms);
          play(result.steady, 2000);
        }
      }
    }
  }
});

test("the brightness sketches dim only the countable lights of an overflowing state", async () => {
  const input = scene(["error", ...many(9, "working")], null, 0);
  const rendered = await service(input);
  const lights = (id: string) => play(design(id).adapt(input, rendered).steady, 1000)[0]!;
  const working = rendered.lit.working;

  // Light 0 is the error, 1-6 are single working items, 7 holds the other three.
  const bright = lights("bright-stack");
  expect(bright[0]).toEqual(rgb(rendered.lit.error));
  expect(bright[1]).toEqual(rgb(scaled(working, 0.3)));
  expect(bright[7]).toEqual(rgb(working));

  const trail = lights("trail");
  expect(trail.slice(5)).toEqual([rgb(working), rgb(scaled(working, 0.4)), rgb(scaled(working, 0.12))]);

  // With nothing hidden, every sketch shows exactly what the service shows.
  const calm = scene(["error", "working"], null, 0);
  const service_calm = await service(calm);
  for (const id of ["bright-stack", "contrast-count", "trail", "breath-rate"]) {
    expect(play(design(id).adapt(calm, service_calm).steady, 1000)[0]).toEqual(play(service_calm.steady, 1000)[0]!);
  }
});
