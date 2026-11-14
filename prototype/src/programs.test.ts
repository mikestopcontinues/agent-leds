// Runs what the installed service and the sketches render through the
// firmware's interpreter, so a program the device would reject fails here.

import { expect, test } from "bun:test";
import { fromHex, type RGB } from "./color";
import { DESIGNS, STATES, type Rendering, type Scene, type State } from "./designs";
import { Engine } from "./engine";

const engine = await Engine.load(await Bun.file(`${import.meta.dir}/../vendor/sdled.wasm`).arrayBuffer());
let clock = 0;

const COLORS = { error: "#FF0000", question: "#FF5000", stopped: "#00FF00", working: "#0000FF" };
const BLACK: RGB = [0, 0, 0];
const rgb = (hex: string) => fromHex(hex)!;

function scene(states: State[], announce: State | null = null, front_led = 7, brightness = 255): Scene {
  return { states, announce, colors: COLORS, brightness, front_led };
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

const many = (count: number, state: State) => Array<State>(count).fill(state);
const OVERFLOW = [...many(9, "error"), ...many(9, "question"), ...many(9, "stopped"), ...many(9, "working")];

test("the service holds the queue with its front on the front light", async () => {
  const { steady } = await service(scene(["working", "stopped", "error", "question"]));
  const [far] = play(steady, 1000);
  expect(far).toEqual([BLACK, BLACK, BLACK, BLACK, rgb(COLORS.working), rgb(COLORS.stopped), rgb(COLORS.question), rgb(COLORS.error)]);

  const [near] = play((await service(scene(["working", "error"], null, 0))).steady, 1000);
  expect(near!.slice(0, 3)).toEqual([rgb(COLORS.error), rgb(COLORS.working), BLACK]);
});

test("a change pulses the whole strip in the new color, then the queue fades in", async () => {
  play("off", 100);
  const { announce, steady, announce_ms } = await service(scene(["question"], "question"));
  const [peak, settled] = play(announce!, 350, announce_ms + 50);
  expect(peak).toEqual(Array(8).fill(rgb(COLORS.question)));
  expect(settled).toEqual([...Array(7).fill(BLACK), rgb(COLORS.question)]);
  // Writing the steady program afterwards changes nothing.
  expect(play(steady, 1000)[0]).toEqual(settled!);
});

test("a stacked light breathes and the others hold still", async () => {
  const { slots, steady } = await service(scene(many(9, "working"), null, 0));
  expect(slots.map((slot) => slot.stacked)).toEqual([...Array(7).fill(false), true]);
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
        for (const design of DESIGNS) {
          const result = design.adapt(input, rendered);
          if (result.announce) play(result.announce, result.announce_ms);
          play(result.steady, 2000);
        }
      }
    }
  }
});

test("the travelling pulse reaches the front of the queue last", async () => {
  play("off", 100);
  const input = scene(["error"], "error");
  const sweep = DESIGNS.find((design) => design.id === "sweep")!.adapt(input, await service(input));
  const [early, late] = play(sweep.announce!, 150, 570);
  // Early on only the near end (light 0) is lit; by the end the pulse is at light 7.
  expect(early![0]![0]).toBeGreaterThan(200);
  expect(early![7]![0]).toBe(0);
  expect(late![7]![0]).toBeGreaterThan(200);
  expect(late![0]![0]).toBe(0);
});
