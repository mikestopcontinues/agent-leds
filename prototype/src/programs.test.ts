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
  hold_ms: 60,
  move_ms: 90,
  fade_out_ms: 300,
  fade_in_ms: 500,
  fade_in: "front_to_back",
  crunch: 0,
  crunch_ms: 120,
  bounce: 0.4,
  spring_ms: 350,
};
const CONFIG: Config = {
  brightness: 255,
  front_led: 7,
  balance: [0.32, 0.54, 0.14],
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
  const stepMs = SNAKE.hold_ms + SNAKE.move_ms;
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
  // Each step holds before the snake moves on, so each light is seen at its level.
  expect(at(SNAKE.fade_out_ms + 6 * stepMs + SNAKE.hold_ms - 10)).toEqual(step(5));
  // The head has left the far end and the dimmed queue is in place, then it rises to full.
  expect(step(10)).toEqual([BLACK, BLACK, BLACK, BLACK, BLACK, dim("working"), dim("stopped"), dim("question")]);
  const full = [BLACK, BLACK, BLACK, BLACK, BLACK, rgb(lit.working), rgb(lit.stopped), rgb(lit.question)];
  expect(at(SNAKE.fade_out_ms + 11 * stepMs + SNAKE.fade_in_ms)).toEqual(full);
  // Writing the steady program afterwards changes nothing.
  expect(play(rendering.steady, 1000)[0]).toEqual(full);
});

test("a crunch: the arriving queue piles up at the front end, springs back past its length, then rests", async () => {
  const snake: Snake = { ...SNAKE, tail: 1, crunch: 0.5, crunch_ms: 120, bounce: 0.5, spring_ms: 300 };
  const rendering = await render(scene(["error", "stopped", "working"], "working", { snake }));
  const lit = (lights: RGB[]) => lights.filter((light) => light.some((die) => die > 0)).length;
  const arrived = snake.fade_out_ms + 11 * (snake.hold_ms + snake.move_ms);
  const at = (ms: number) => {
    play("off", 100);
    return during(rendering, ms);
  };
  // Three items at rest, squeezed onto two lights, stretched over four, then at rest again.
  expect(lit(at(arrived))).toBe(3);
  const squeezed = at(arrived + 120);
  expect(lit(squeezed)).toBe(2);
  // The front light holds the error and half of the item behind it.
  expect(squeezed[7]![0]).toBe(rgb(rendering.lit.error)[0]);
  expect(squeezed[7]![2]).toBeGreaterThan(0);
  expect(lit(at(arrived + 120 + 180))).toBe(4);
  expect(at(arrived + 120 + 300)).toEqual(at(arrived));
});

test("the queue rises front to back, or all at once", async () => {
  const states: State[] = ["error", "question", "stopped", "working"];
  const arrived = SNAKE.fade_out_ms + 11 * (SNAKE.hold_ms + SNAKE.move_ms);
  /** How far each of the four lights has risen halfway through the rise, front of the queue first. */
  const halfway = async (fade_in: Snake["fade_in"]) => {
    const rendering = await render(scene(states, "working", { snake: { ...SNAKE, tail: 0, fade_in } }));
    play("off", 100);
    const lights = during(rendering, arrived + SNAKE.fade_in_ms / 2);
    const full = play(rendering.steady, 1000)[0]!;
    const brightest = (light: RGB) => Math.max(...light);
    return [7, 6, 5, 4].map((led) => brightest(lights[led]!) / brightest(full[led]!));
  };
  // Front to back: the front light is done, the last has not started, and those between are on their way.
  const ordered = await halfway("front_to_back");
  expect(ordered[0]).toBe(1);
  expect(ordered[3]).toBe(0);
  expect(ordered).toEqual([...ordered].sort((a, b) => b - a));
  // All at once: every light is equally far along.
  const together = await halfway("together");
  for (const level of together) expect(level).toBeCloseTo(together[0]!, 1);
  expect(together[0]).toBeGreaterThan(0.2);
  expect(together[0]).toBeLessThan(0.9);
});

test("the queue rises to full while it crunches", async () => {
  const snake: Snake = { ...SNAKE, fade_in_ms: 400, crunch: 0.5, crunch_ms: 100, bounce: 0.5, spring_ms: 300 };
  const rendering = await render(scene(["error"], "error", { snake }));
  const arrived = snake.fade_out_ms + 11 * (snake.hold_ms + snake.move_ms);
  const front = (ms: number) => {
    play("off", 100);
    return during(rendering, ms)[7]![0];
  };
  const full = rgb(rendering.lit.error)[0];
  // The front light climbs through the squeeze and the spring back, and is full as the queue comes to rest.
  const levels = [front(arrived), front(arrived + 100), front(arrived + 280), front(arrived + 400)];
  expect(levels[0]).toBe(Math.round(full * snake.tail));
  expect(levels).toEqual([...levels].sort((a, b) => a - b));
  expect(new Set(levels).size).toBe(4);
  expect(levels[3]).toBe(full);
});

test("every change is accepted by the firmware and ends on the queue, in the hardest cases", async () => {
  const snakes: Snake[] = [
    SNAKE,
    { ...SNAKE, fade_in: "together" },
    { ...SNAKE, crunch: 0.5 },
    { ...SNAKE, crunch: 1, bounce: 0 },
    { head: [0, 0, 0], gap: false, tail: 0, hold_ms: 0, move_ms: 1, fade_out_ms: 1, fade_in_ms: 1, fade_in: "front_to_back", crunch: 0.05, crunch_ms: 1, bounce: 1, spring_ms: 1 },
    { head: [1, 1, 1], gap: true, tail: 1, hold_ms: 5000, move_ms: 5000, fade_out_ms: 5000, fade_in_ms: 5000, fade_in: "front_to_back", crunch: 1, crunch_ms: 5000, bounce: 1, spring_ms: 5000 },
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
    stdin: new Blob([JSON.stringify(scene(["error"], "error", { snake: { ...SNAKE, move_ms: 0 } }))]),
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(await child.exited).not.toBe(0);
  expect(await new Response(child.stderr).text()).toContain("timings");
});
