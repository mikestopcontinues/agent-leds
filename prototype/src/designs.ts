// Ways of showing the queue. "Slide" is what the installed daemon renders; the
// others are sketches to compare against it before porting one to Rust.

export const STATES = ["error", "question", "stopped", "working"] as const;
export type State = (typeof STATES)[number];
export type Palette = Record<State, string>;

export type Config = { colors: Palette; brightness: number; front_led: number; balance: [number, number, number] };
export type Scene = Config & { states: State[]; announce: State | null };

/** What `leds render` returns for a scene. */
export type ServiceRendering = {
  /** The state shown by each lit light, front of the queue first. */
  slots: State[];
  /** The state colors as sent, scaled to look equally bright. */
  lit: Palette;
  steady: string;
  announce: string | null;
  announce_ms: number;
  config: Config;
};

/** One program of a change animation and how long it runs before the next is written. */
export type Step = { program: string; ms: number };

/** A change animation, which may span several programs, then the program that holds the queue. */
export type Rendering = { steps: Step[]; steady: string };

export type Design = {
  id: string;
  name: string;
  about: string;
  /** Builds this design's programs from the scene and the service's own rendering of it. */
  adapt(scene: Scene, service: ServiceRendering): Rendering;
};

const LEDS = 8;
/** The device's limits on one program. */
const MAX_BYTES = 512;
const MAX_LINES = 20;
const OFF = "#000000";
const FADE_OUT = "off 300ms ease";
const FADE_OUT_MS = 300;
const FADE_IN_MS = 500;

/** The same hue with its light multiplied by `factor`. */
export function scaled(hex: string, factor: number): string {
  const channel = (at: number) =>
    Math.min(255, Math.round(parseInt(hex.slice(at, at + 2), 16) * factor)).toString(16).padStart(2, "0");
  return `#${channel(1)}${channel(3)}${channel(5)}`.toUpperCase();
}

/** A white that looks `level` times as bright as `hex`, going by the scene's die balance. */
function whiteAs(hex: string, level: number, scene: Scene): string {
  const brightness = scene.balance.reduce(
    (sum, weight, die) => sum + (parseInt(hex.slice(1 + die * 2, 3 + die * 2), 16) / 255) * weight,
    0,
  );
  const allDies = scene.balance.reduce((sum, weight) => sum + weight, 0);
  return scaled("#FFFFFF", (level * brightness) / allDies);
}

// The sketches think in distance along the strip: 0 is the light nearest you
// and 7 is the front of the queue.

/** The physical light at a distance along the strip. */
function led(distance: number, scene: Scene): number {
  return scene.front_led === 0 ? LEDS - 1 - distance : distance;
}

/** A row of eight colors in physical order, given the color at each distance. */
function row(scene: Scene, colorAt: (distance: number) => string): string {
  const lights = Array<string>(LEDS).fill(OFF);
  for (let distance = 0; distance < LEDS; distance++) lights[led(distance, scene)] = colorAt(distance);
  return lights.join(" ");
}

/** The queue's color at a distance along the strip, or off past the end of the queue. */
function queueAt(distance: number, service: ServiceRendering, level = 1): string {
  const state = service.slots[LEDS - 1 - distance];
  return state ? scaled(service.lit[state], level) : OFF;
}

const queueRow = (scene: Scene, service: ServiceRendering, level = 1) =>
  row(scene, (distance) => queueAt(distance, service, level));

function program(scene: Scene, ...lines: string[]): string {
  return (scene.brightness === 255 ? "" : `brightness ${scene.brightness}\n`) + lines.join("\n");
}

const after = (ms: number) => (ms ? ` ${ms}ms` : "");

/** Packs an animation's lines into as few programs as the device's limits allow, each with its running time. */
function packed(scene: Scene, lines: { text: string; ms: number }[]): Step[] {
  const parts: { lines: string[]; ms: number }[] = [];
  for (const line of lines) {
    const last = parts.at(-1);
    const fits =
      last &&
      last.lines.length + 2 <= MAX_LINES &&
      new TextEncoder().encode(program(scene, ...last.lines, line.text)).length <= MAX_BYTES;
    if (fits) {
      last.lines.push(line.text);
      last.ms += line.ms;
    } else {
      parts.push({ lines: [line.text], ms: line.ms });
    }
  }
  return parts.map((part) => ({ program: program(scene, ...part.lines), ms: part.ms }));
}

/** Where the changed session sits: the newest item of a state is the last light of that state. */
function landing(state: State, service: ServiceRendering): number {
  return LEDS - 1 - service.slots.lastIndexOf(state);
}

type Announcer = (state: State, color: string, scene: Scene, service: ServiceRendering) => Step[];

/** A design that keeps the service's steady program and replaces its change animation. */
function announcing(announce: Announcer): Design["adapt"] {
  return (scene, service) => ({
    steady: service.steady,
    steps: scene.announce ? announce(scene.announce, service.lit[scene.announce], scene, service) : [],
  });
}

/** The service's slide with its own speed: the color passes along the dark strip toward the front. */
function slide(stepMs: number, color: string, scene: Scene, service: ServiceRendering): Step[] {
  const pulseMs = Math.round(stepMs * 3.5);
  const pass = Array.from({ length: LEDS }, (_, at) => `${led(at, scene)}:${color} ${pulseMs}ms pulse${after(at * stepMs)}`);
  return [
    {
      program: program(scene, FADE_OUT, pass.join(";"), `${queueRow(scene, service)} ${FADE_IN_MS}ms ease`),
      ms: FADE_OUT_MS + pulseMs + (LEDS - 1) * stepMs + FADE_IN_MS,
    },
  ];
}

export const DESIGNS: Design[] = [
  {
    id: "service",
    name: "Slide",
    about:
      "What the running service does. The lights fade out, the new state's color slides along the dark strip toward the front of the queue, then the queue fades in.",
    adapt: (_scene, service) => ({
      steady: service.steady,
      steps: service.announce ? [{ program: service.announce, ms: service.announce_ms }] : [],
    }),
  },
  {
    id: "pull",
    name: "Pull the queue in",
    about:
      "The lights fade out. A head in the new state's color enters at the near end: a dimmer nose, a bright light, then a faint white light. It pulls the new queue in behind the white as a dimmed tail. The head runs off the far end, and once the queue is in place it rises to full brightness.",
    adapt: announcing((_state, color, scene, service) => {
      // Each step holds still, then moves. Without the hold a light is at its level only for an
      // instant, so the bright light never looks bright.
      const holdMs = 60;
      const moveMs = 90;
      // The head from the nose back: dimmer, bright, then a faint white that sets it apart from the
      // tail. The tail is the queue at this level.
      const tail = 0.3;
      const head = [scaled(color, 0.6), color, whiteAs(color, tail, scene)];
      // With the nose at `nose`, queue item k trails the head at distance nose - head.length - k.
      const frame = (nose: number) =>
        row(scene, (distance) => {
          const behind = nose - distance;
          if (behind < 0) return OFF;
          return behind < head.length
            ? head[behind]!
            : queueAt(LEDS - 1 - (behind - head.length), service, tail);
        });
      // The last frame has the whole head past the far end and the queue in place.
      const frames = Array.from({ length: LEDS + head.length }, (_, nose) => ({
        text: `${frame(nose)} ${moveMs}ms linear ${holdMs}ms`,
        ms: holdMs + moveMs,
      }));
      return packed(scene, [
        { text: FADE_OUT, ms: FADE_OUT_MS },
        ...frames,
        { text: `${queueRow(scene, service)} ${FADE_IN_MS}ms ease`, ms: FADE_IN_MS },
      ]);
    }),
  },
  {
    id: "land",
    name: "Land in its slot",
    about:
      "The lights fade out. A head in the new state's color slides in from the near end and stops on the light where the changed session now sits, then the rest of the queue fades in around it. It shows how far up the queue the change is.",
    adapt: announcing((state, color, scene, service) => {
      const stepMs = 160;
      const stop = landing(state, service);
      const pass = Array.from({ length: stop }, (_, at) => `${led(at, scene)}:${color} 480ms pulse${after(at * stepMs)}`);
      pass.push(`${led(stop, scene)}:${color} 320ms ease${after(stop * stepMs)}`);
      const passMs = Math.max(stop * stepMs + 320, stop ? (stop - 1) * stepMs + 480 : 0);
      return [
        {
          program: program(scene, FADE_OUT, pass.join(";"), `${queueRow(scene, service)} ${FADE_IN_MS}ms ease`),
          ms: FADE_OUT_MS + passMs + FADE_IN_MS,
        },
      ];
    }),
  },
  {
    id: "bloom",
    name: "Bloom from its slot",
    about:
      "The lights fade out. The changed session's light comes on alone, its color spreads outward along the strip and dies back, then the queue fades in.",
    adapt: announcing((state, color, scene, service) => {
      const stepMs = 110;
      const center = landing(state, service);
      const others = Array.from({ length: LEDS }, (_, at) => at).filter((at) => at !== center);
      const spread = others.map((at) => `${led(at, scene)}:${color} 520ms pulse${after(Math.abs(at - center) * stepMs)}`);
      const reach = Math.max(center, LEDS - 1 - center);
      return [
        {
          program: program(
            scene,
            FADE_OUT,
            `${led(center, scene)}:${color} 250ms ease`,
            spread.join(";"),
            `${queueRow(scene, service)} ${FADE_IN_MS}ms ease`,
          ),
          ms: FADE_OUT_MS + 250 + reach * stepMs + 520 + FADE_IN_MS,
        },
      ];
    }),
  },
  {
    id: "fill",
    name: "Fill and resolve",
    about:
      "The lights fade out. The new state's color fills the strip from the near end and stays, then each light resolves to its place in the queue, starting at the front.",
    adapt: announcing((_state, color, scene, service) => {
      const stepMs = 90;
      const each = Array.from({ length: LEDS }, (_, at) => at);
      const fill = each.map((at) => `${led(at, scene)}:${color} 200ms ease${after(at * stepMs)}`);
      const resolve = [...each]
        .reverse()
        .map((at) => `${led(at, scene)}:${queueAt(at, service)} 300ms ease${after((LEDS - 1 - at) * stepMs)}`);
      return [
        {
          program: program(scene, FADE_OUT, fill.join(";"), resolve.join(";")),
          ms: FADE_OUT_MS + (LEDS - 1) * stepMs + 200 + (LEDS - 1) * stepMs + 300,
        },
      ];
    }),
  },
  {
    id: "blink",
    name: "Blink in place",
    about:
      "No blackout. The queue moves straight to its new arrangement and the changed session's light dips twice. The calmest option, for when changes are frequent.",
    adapt: announcing((state, color, scene, service) => {
      const dip = `${led(landing(state, service), scene)}:${scaled(color, 0.08)} 320ms pulse`;
      return [{ program: program(scene, `${queueRow(scene, service)} 400ms ease`, dip, dip), ms: 400 + 2 * 320 }];
    }),
  },
  {
    id: "tempo",
    name: "Urgency tempo",
    about: "The slide, at a speed set by the state: quick for an error, slower for a question or a stop, unhurried for working.",
    adapt: announcing((state, color, scene, service) =>
      slide({ error: 70, question: 110, stopped: 160, working: 230 }[state], color, scene, service),
    ),
  },
  {
    id: "head",
    name: "Breathing head",
    about:
      "Not a change animation: the front of the queue breathes while it needs you, so the one moving light is always the next thing to deal with. Working items never move.",
    adapt(scene, service) {
      const steps = service.announce ? [{ program: service.announce, ms: service.announce_ms }] : [];
      const head = service.slots[0];
      if (!head || head === "working") return { steps, steady: service.steady };
      const breath = `${led(LEDS - 1, scene)}:${scaled(service.lit[head], 0.125)} 2000ms pulse`;
      return { steps, steady: program(scene, `${queueRow(scene, service)} ease`, breath, "repeat") };
    },
  },
];
