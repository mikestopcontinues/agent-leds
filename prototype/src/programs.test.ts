// Runs what the installed service and the sketches render through the
// firmware's interpreter, so a program the device would reject fails here.

import { expect, test } from "bun:test";
import { fromHex, type RGB } from "./color";
import { DESIGNS, STATES, scaled, type Rendering, type Scene, type ServiceRendering, type State } from "./designs";
import { Engine } from "./engine";

const engine = await Engine.load(await Bun.file(`${import.meta.dir}/../vendor/sdled.wasm`).arrayBuffer());
let clock = 0;

const COLORS = { error: "#FF0060", question: "#FFA000", stopped: "#00FF80", working: "#3000FF" };
const BLACK: RGB = [0, 0, 0];
const rgb = (hex: string) => fromHex(hex)!;

function scene(states: State[], announce: State | null = null, front_led = 7, brightness = 255): Scene {
  return { states, announce, colors: COLORS, brightness, front_led, balance: [0.32, 0.54, 0.14] };
}

async function service(input: Scene): Promise<ServiceRendering> {
  const child = Bun.spawn(["leds", "render"], { stdin: new Blob([JSON.stringify(input)]), stdout: "pipe" });
  return JSON.parse(await new Response(child.stdout).text());
}

/** Starts a program and returns the lights at each requested time after it. */
function play(program: string, ...at: number[]): RGB[][] {
  expect(new TextEncoder().encode(program).length).toBeLessThanOrEqual(512);
  expect(program.split("\n").length).toBeLessThanOrEqual(20);
  expect(engine.parse(program, clock)).toEqual({ ok: true });
  const start = clock;
  clock += 60_000;
  return at.map((ms) => engine.step(start + ms));
}

/** Plays a whole rendering as the service would and returns the lights once it has settled. */
function settle(rendering: Rendering): RGB[] {
  for (const step of rendering.steps) play(step.program, step.ms);
  // The steady program eases to the queue in 330 ms; sample the moment it arrives, before any breathing starts.
  return play(rendering.steady, 330)[0]!;
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

test("a full strip holds still: nothing marks hidden items", async () => {
  const { slots, steady } = await service(scene(OVERFLOW, null, 0));
  expect(slots).toEqual(["error", "error", "error", "error", "error", "question", "stopped", "working"]);
  const frames = play(steady, ...Array.from({ length: 30 }, (_, step) => 500 + step * 100));
  expect(new Set(frames.map((frame) => frame.join())).size).toBe(1);
});

test("the service's change fades out, slides the new color toward the front, then fades the queue in", async () => {
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

test("every design is accepted by the firmware and ends on the queue, in the hardest cases", async () => {
  for (const states of [["error"], STATES.slice(), many(8, "working"), OVERFLOW] as State[][]) {
    for (const announce of [null, ...new Set(states)]) {
      for (const front_led of [0, 7]) {
        for (const brightness of [255, 128]) {
          const input = scene(states, announce, front_led, brightness);
          const rendered = await service(input);
          const expected = play(rendered.steady, 1000)[0]!;
          for (const candidate of DESIGNS) {
            play("#FFFFFF", 100);
            expect(settle(candidate.adapt(input, rendered))).toEqual(expected);
          }
        }
      }
    }
  }
});

test("pulling the queue in: a dim nose, a bright light and a dark gap lead a dim queue, which then rises", async () => {
  const input = scene(["question", "stopped", "working"], "stopped");
  const rendered = await service(input);
  const { steps } = design("pull").adapt(input, rendered);
  const { lit } = rendered;
  expect(steps.length).toBeGreaterThan(1);
  const dim = (state: State) => rgb(scaled(lit[state], 0.3));
  const nose = rgb(scaled(lit.stopped, 0.6));
  const body = rgb(lit.stopped);

  /** The lights this long after the change starts, playing each program when the one before ends. */
  const at = (ms: number): RGB[] => {
    play("off", 100);
    for (const step of steps) {
      if (ms <= step.ms) return play(step.program, ms)[0]!;
      play(step.program, step.ms);
      ms -= step.ms;
    }
    throw new Error("past the end of the animation");
  };

  // After the fade-out, the nose reaches light n at the end of frame n.
  const frame = (n: number) => at(300 + (n + 1) * 150);
  expect(frame(2)).toEqual([BLACK, body, nose, BLACK, BLACK, BLACK, BLACK, BLACK]);
  // A dark gap separates the head from the tail.
  expect(frame(5)).toEqual([dim("working"), dim("stopped"), dim("question"), BLACK, body, nose, BLACK, BLACK]);
  // The head has left the far end and the dim queue is in place, then it rises to full.
  const inPlace = [BLACK, BLACK, BLACK, BLACK, BLACK, dim("working"), dim("stopped"), dim("question")];
  expect(frame(10)).toEqual(inPlace);
  // Each step holds before the snake moves on, so the bright light and the gap are seen at their levels.
  expect(at(300 + 6 * 150 + 50)).toEqual(frame(5));
  expect(at(300 + 11 * 150 + 500)).toEqual([BLACK, BLACK, BLACK, BLACK, BLACK, rgb(lit.working), rgb(lit.stopped), rgb(lit.question)]);
});

test("landing in its slot: the head stops where the changed session sits and nothing runs past it", async () => {
  const input = scene(["error", "stopped", "working", "working"], "stopped");
  const rendered = await service(input);
  const [step] = design("land").adapt(input, rendered).steps;
  play("off", 100);
  // The stopped session is second in the queue: light 6. Sample just before the queue fades in.
  const [landed] = play(step!.program, step!.ms - 500);
  expect(landed![6]).toEqual(rgb(rendered.lit.stopped));
  expect(landed![7]).toEqual(BLACK);
  expect(landed![5]).toEqual(BLACK);
});

test("blinking in place never blacks out the rest of the queue", async () => {
  const input = scene(["error", "working"], "working");
  const rendered = await service(input);
  const [step] = design("blink").adapt(input, rendered).steps;
  play(rendered.steady, 500);
  const frames = play(step!.program, ...Array.from({ length: 20 }, (_, index) => index * 50));
  for (const frame of frames) expect(frame[7]).toEqual(rgb(rendered.lit.error));
  // The changed light (working, light 6) dips.
  expect(Math.min(...frames.map((frame) => frame[6]![2]))).toBeLessThan(60);
});
