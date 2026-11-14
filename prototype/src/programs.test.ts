// Runs what the installed service renders through the firmware's interpreter,
// so a program the device would reject, or that shows the wrong lights, fails here.

import { expect, test } from "bun:test";
import { fromHex, type RGB } from "./color";
import { Engine } from "./engine";
import { STATES, scaled, type Config, type Rendering, type Scene, type Snake, type State } from "./service";

const engine = await Engine.load(await Bun.file(`${import.meta.dir}/../vendor/sdled.wasm`).arrayBuffer());
let clock = 0;

const SNAKE: Snake = {
  head: [0.6, 1, 0],
  gap: true,
  tail: 0.3,
  fade_out_ms: 300,
  step_ms: 150,
};
const CONFIG: Config = {
  brightness: 255,
  front_led: 7,
  balance: [0.32, 0.54, 0.14],
  keep_read_minutes: 15,
  abandon_days: 3,
  colors: { error: "#FF0060", question: "#FFA000", stopped: "#00FF80", working: "#3000FF" },
  snake: SNAKE,
};
const BLACK: RGB = [0, 0, 0];
const rgb = (hex: string) => fromHex(hex)!;

function scene(states: State[], announce: State | null = null, config: Partial<Config> = {}): Scene {
  return { states, announce, config: { ...CONFIG, ...config } };
}

async function render(input: Scene): Promise<Rendering> {
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

/** The lights this long after a change starts, writing each program when the one before it ends. */
function during(rendering: Rendering, ms: number): RGB[] {
  for (const step of rendering.steps) {
    if (ms <= step.ms) return play(step.program, ms)[0]!;
    play(step.program, step.ms);
    ms -= step.ms;
  }
  throw new Error("past the end of the animation");
}

/** The lights once a change has played out and the queue is held. */
function settled(rendering: Rendering): RGB[] {
  for (const step of rendering.steps) play(step.program, step.ms);
  // The steady program eases to the queue in 330 ms.
  return play(rendering.steady, 330)[0]!;
}

/** How long the rise takes for this many lights: each goes dark a step after the one in front, then rises over seven. */
const riseMs = (lights: number, snake: Snake = SNAKE) => snake.step_ms * (lights + 7);

const many = (count: number, state: State) => Array<State>(count).fill(state);
const OVERFLOW = [...many(9, "error"), ...many(9, "question"), ...many(9, "stopped"), ...many(9, "working")];

test("the service holds the queue with its front on the front light", async () => {
  const { steady, lit } = await render(scene(["working", "stopped", "error", "question"]));
  const [far] = play(steady, 1000);
  expect(far).toEqual([BLACK, BLACK, BLACK, BLACK, rgb(lit.working), rgb(lit.stopped), rgb(lit.question), rgb(lit.error)]);

  const near = await render(scene(["working", "error"], null, { front_led: 0 }));
  expect(play(near.steady, 1000)[0]!.slice(0, 3)).toEqual([rgb(near.lit.error), rgb(near.lit.working), BLACK]);
});

test("the states are sent equally bright, limited by the dimmest hue", async () => {
  const { lit } = await render(scene([]));
  const brightness = (hex: string) => rgb(hex).reduce((sum, value, die) => sum + (value / 255) * CONFIG.balance[die]!, 0);
  const levels = STATES.map((state) => brightness(lit[state]));
  for (const level of levels) expect(level).toBeCloseTo(levels[0]!, 2);
  // Violet is the dimmest hue in this palette, so it is the one left at full strength.
  expect(lit.working).toBe(CONFIG.colors.working);
});

test("a full strip holds still: nothing marks hidden items", async () => {
  const { slots, steady } = await render(scene(OVERFLOW, null, { front_led: 0 }));
  expect(slots).toEqual(["error", "error", "error", "error", "error", "question", "stopped", "working"]);
  const frames = play(steady, ...Array.from({ length: 30 }, (_, step) => 500 + step * 100));
  expect(new Set(frames.map((frame) => frame.join())).size).toBe(1);
});

test("a change: a dim nose, a bright light and a dark one lead the dimmed queue in, which then rises", async () => {
  const rendering = await render(scene(["question", "stopped", "working"], "stopped"));
  const { lit, steps } = rendering;
  expect(steps.length).toBeGreaterThan(1);
  const dim = (state: State) => rgb(scaled(lit[state], SNAKE.tail));
  const nose = rgb(scaled(lit.stopped, SNAKE.head[0]));
  const bright = rgb(lit.stopped);
  const stepMs = SNAKE.step_ms;
  const at = (ms: number) => {
    play(`${lit.working} ${lit.working}`, 100);
    return during(rendering, ms);
  };

  // The old queue fades out first.
  expect(at(SNAKE.fade_out_ms)).toEqual(Array(8).fill(BLACK));
  // The nose reaches light n at the end of step n.
  const step = (n: number) => at(SNAKE.fade_out_ms + (n + 1) * stepMs);
  expect(step(1)).toEqual([bright, nose, BLACK, BLACK, BLACK, BLACK, BLACK, BLACK]);
  expect(step(5)).toEqual([dim("working"), dim("stopped"), dim("question"), BLACK, bright, nose, BLACK, BLACK]);
  // Each step holds for half its time before the snake moves on, so each light is seen at its level.
  expect(at(SNAKE.fade_out_ms + 6 * stepMs + stepMs / 2 - 10)).toEqual(step(5));
  // The head has left the far end and the dimmed queue is in place, then it rises to full.
  expect(step(10)).toEqual([BLACK, BLACK, BLACK, BLACK, BLACK, dim("working"), dim("stopped"), dim("question")]);
  const full = [BLACK, BLACK, BLACK, BLACK, BLACK, rgb(lit.working), rgb(lit.stopped), rgb(lit.question)];
  expect(at(SNAKE.fade_out_ms + 11 * stepMs + riseMs(3))).toEqual(full);
  // Writing the steady program afterwards changes nothing.
  expect(play(rendering.steady, 1000)[0]).toEqual(full);
});

test("the queue rises in a wave from its front, each light dark just before the wave reaches it", async () => {
  const snake: Snake = { ...SNAKE, tail: 0.5, step_ms: 40 };
  const rendering = await render(scene(["error", "question", "stopped", "working"], "working", { snake }));
  const arrived = snake.fade_out_ms + 11 * snake.step_ms;
  const full = play(rendering.steady, 1000)[0]!;
  /** How bright each of the four lights is, as a share of full, front of the queue first. */
  const levels = (steps: number) => {
    play("off", 100);
    const lights = during(rendering, arrived + steps * snake.step_ms);
    return [7, 6, 5, 4].map((led) => Math.max(...lights[led]!) / Math.max(...full[led]!));
  };
  const tail = levels(0);
  for (const level of tail) expect(level).toBeCloseTo(0.5, 1);

  // The front light goes dark first, while the rest wait at the tail's brightness.
  const [front, ...waiting] = levels(1);
  expect(front).toBe(0);
  expect(waiting).toEqual(tail.slice(1));

  // A step later the front light is the crest and the dark light has moved on: the wave runs toward the back.
  const later = levels(2);
  expect(later[0]).toBeCloseTo(1 / 7, 1);
  expect(later[1]).toBe(0);
  expect(later.slice(2)).toEqual(tail.slice(2));

  // A light rises from dark at an even pace over seven steps: the front one is halfway at four and a half.
  expect(levels(4.5)[0]).toBeCloseTo(0.5, 1);
  // Four lights are done in eleven steps.
  expect(riseMs(4, snake)).toBe(440);
  expect(levels(10)[3]).toBeLessThan(1);
  expect(levels(11)).toEqual([1, 1, 1, 1]);
});

test("every change is accepted by the firmware and ends on the queue, in the hardest cases", async () => {
  const snakes: Snake[] = [
    SNAKE,
    { ...SNAKE, tail: 0.5, step_ms: 55 },
    { head: [0, 0, 0], gap: false, tail: 0, fade_out_ms: 1, step_ms: 1 },
    { head: [1, 1, 1], gap: true, tail: 1, fade_out_ms: 5000, step_ms: 5000 },
  ];
  for (const states of [["error"], STATES.slice(), many(8, "working"), OVERFLOW] as State[][]) {
    for (const announce of [null, ...new Set(states)]) {
      for (const front_led of [0, 7]) {
        for (const brightness of [255, 128]) {
          for (const snake of snakes) {
            const rendering = await render(scene(states, announce, { front_led, brightness, snake }));
            const expected = play(rendering.steady, 1000)[0]!;
            play("#FFFFFF", 100);
            expect(settled(rendering)).toEqual(expected);
          }
        }
      }
    }
  }
});

test("the service refuses settings it cannot render", async () => {
  const child = Bun.spawn(["leds", "render"], {
    stdin: new Blob([JSON.stringify(scene(["error"], "error", { snake: { ...SNAKE, step_ms: 0 } }))]),
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(await child.exited).not.toBe(0);
  expect(await new Response(child.stderr).text()).toContain("timings");
});
