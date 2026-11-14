// Ways of showing the queue. "Service" is what the installed daemon renders;
// the others are sketches to compare against it before porting one to Rust.

export const STATES = ["error", "question", "stopped", "working"] as const;
export type State = (typeof STATES)[number];
export type Palette = Record<State, string>;

export type Config = { colors: Palette; brightness: number; front_led: number; balance: [number, number, number] };
export type Scene = Config & { states: State[]; announce: State | null };
/** One lit light and how many queue items it stands for; more than one means it is stacked. */
export type Slot = { state: State; count: number };

/** The programs for one change: an optional announcement, then the program that holds the queue. */
export type Rendering = {
  slots: Slot[];
  /** The state colors as sent, scaled to look equally bright. */
  lit: Palette;
  steady: string;
  announce: string | null;
  announce_ms: number;
};

export type Design = {
  id: string;
  name: string;
  about: string;
  /** Builds this design's programs from the scene and the service's own rendering of it. */
  adapt(scene: Scene, service: Rendering): Rendering;
};

const LEDS = 8;
const OFF = "#000000";

function ledAt(position: number, scene: Scene): number {
  return scene.front_led === 0 ? position : LEDS - 1 - position;
}

function settings(scene: Scene): string {
  return scene.brightness === 255 ? "" : `brightness ${scene.brightness}\n`;
}

/** The same hue with its light multiplied by `factor`. */
export function scaled(hex: string, factor: number): string {
  const channel = (at: number) =>
    Math.min(255, Math.round(parseInt(hex.slice(at, at + 2), 16) * factor)).toString(16).padStart(2, "0");
  return `#${channel(1)}${channel(3)}${channel(5)}`.toUpperCase();
}

/** How bright one light is, given the lights of its state in queue order and its place among them. */
type Level = (slot: Slot, group: Slot[], index: number) => number;

/** The eight lights in physical order, each state's color scaled by `level`. */
function row(scene: Scene, service: Rendering, level: Level = () => 1): string {
  const lights = Array<string>(LEDS).fill(OFF);
  service.slots.forEach((slot, position) => {
    const group = service.slots.filter((other) => other.state === slot.state);
    lights[ledAt(position, scene)] = scaled(service.lit[slot.state], level(slot, group, group.indexOf(slot)));
  });
  return lights.join(" ");
}

/** A design that holds the queue still, differing from the service only in brightness. */
function still(scene: Scene, service: Rendering, level: Level): Rendering {
  return { ...service, steady: `${settings(scene)}${row(scene, service, level)} ease` };
}

const stacked = (slot: Slot) => slot.count > 1;
const overflows = (group: Slot[]) => group.some(stacked);

export const DESIGNS: Design[] = [
  {
    id: "service",
    name: "Service",
    about:
      "What the running service does. A change fades the lights out, slides the new state's color along the dark strip toward the front of the queue, then fades the queue in. A light that stands for several items breathes.",
    adapt: (_scene, service) => service,
  },
  {
    id: "bright-stack",
    name: "Bright stack",
    about:
      "No motion. In a state with more items than lights, the lights that stand for one item dim and the one holding the rest stays at full brightness.",
    adapt: (scene, service) => still(scene, service, (slot, group) => (overflows(group) && !stacked(slot) ? 0.3 : 1)),
  },
  {
    id: "contrast-count",
    name: "Contrast counts",
    about:
      "Bright stack, with the dimming set by how many items are hidden: two hidden items dim the others a little, ten dim them almost out.",
    adapt: (scene, service) =>
      still(scene, service, (slot, group) =>
        overflows(group) && !stacked(slot) ? Math.max(0.1, 1 / group.at(-1)!.count ** 0.9) : 1,
      ),
  },
  {
    id: "trail",
    name: "Trailing fade",
    about:
      "No motion. A state with more items than lights fades out along its last two lights, like an ellipsis: it keeps going past what is shown.",
    adapt: (scene, service) =>
      still(scene, service, (_slot, group, index) => {
        if (!overflows(group)) return 1;
        return [0.12, 0.4][group.length - 1 - index] ?? 1;
      }),
  },
  {
    id: "breath-rate",
    name: "Faster breathing",
    about: "The service's breathing, quicker the more items a light holds: slow for two, urgent for six or more.",
    adapt(scene, service) {
      const breaths = service.slots.flatMap((slot, position) => {
        if (!stacked(slot)) return [];
        const period = Math.max(600, 2400 - (slot.count - 2) * 450);
        return [`${ledAt(position, scene)}:${scaled(service.lit[slot.state], 0.125)} ${period}ms pulse`];
      });
      if (breaths.length === 0) return service;
      return { ...service, steady: `${settings(scene)}${row(scene, service)} ease\n${breaths.join(";")}\nrepeat` };
    },
  },
  {
    id: "head",
    name: "Breathing head",
    about:
      "The front of the queue breathes while it needs you, so the one moving light is always the next thing to deal with. Working items never move.",
    adapt(scene, service) {
      const head = service.slots[0];
      if (!head || head.state === "working") return service;
      const breath = `${ledAt(0, scene)}:${scaled(service.lit[head.state], 0.125)} 2000ms pulse`;
      return { ...service, steady: `${settings(scene)}${row(scene, service)} ease\n${breath}\nrepeat` };
    },
  },
];
