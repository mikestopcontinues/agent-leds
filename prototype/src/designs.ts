// Ways of showing the queue. "Service" is what the installed daemon renders;
// the others are sketches to compare against it before porting one to Rust.

export const STATES = ["error", "question", "stopped", "working"] as const;
export type State = (typeof STATES)[number];
export type Palette = Record<State, string>;

export type Config = { colors: Palette; brightness: number; front_led: number };
export type Scene = Config & { states: State[]; announce: State | null };
export type Slot = { state: State; stacked: boolean };

/** The programs for one change: an optional announcement, then the program that holds the queue. */
export type Rendering = { slots: Slot[]; steady: string; announce: string | null; announce_ms: number };

export type Design = {
  id: string;
  name: string;
  about: string;
  /** Builds this design's programs from the scene and the service's own rendering of it. */
  adapt(scene: Scene, service: Rendering): Rendering;
};

const LEDS = 8;
const OFF = "#000000";

/** The colors of the eight lights in physical order, front of the queue on `front_led`. */
function row(slots: Slot[], scene: Scene): string {
  const lights = Array<string>(LEDS).fill(OFF);
  slots.forEach((slot, position) => (lights[ledAt(position, scene)] = scene.colors[slot.state]));
  return lights.join(" ");
}

function ledAt(position: number, scene: Scene): number {
  return scene.front_led === 0 ? position : LEDS - 1 - position;
}

function settings(scene: Scene): string {
  return scene.brightness === 255 ? "" : `brightness ${scene.brightness}\n`;
}

function dimmed(hex: string): string {
  const channel = (at: number) => (parseInt(hex.slice(at, at + 2), 16) >> 3).toString(16).padStart(2, "0");
  return `#${channel(1)}${channel(3)}${channel(5)}`.toUpperCase();
}

export const DESIGNS: Design[] = [
  {
    id: "service",
    name: "Service",
    about:
      "What the running service does. A change pulses the whole strip once in the new state's color, then the queue fades in. A light that stands for several items breathes.",
    adapt: (_scene, service) => service,
  },
  {
    id: "head",
    name: "Breathing head",
    about:
      "The front of the queue breathes while it needs you, so the one moving light is always the next thing to deal with. Working items never move.",
    adapt(scene, service) {
      const head = service.slots[0];
      if (!head || head.state === "working") return service;
      const breath = `${ledAt(0, scene)}:${dimmed(scene.colors[head.state])} 2000ms pulse`;
      return { ...service, steady: `${settings(scene)}${row(service.slots, scene)} ease\n${breath}\nrepeat` };
    },
  },
  {
    id: "sweep",
    name: "Travelling pulse",
    about:
      "A change runs along the strip toward the front of the queue instead of pulsing every light at once, then the queue fades in.",
    adapt(scene, service) {
      if (!scene.announce) return service;
      const color = scene.colors[scene.announce];
      const step = 60;
      const wave = Array.from({ length: LEDS }, (_, order) => {
        const delay = order ? ` ${order * step}ms` : "";
        return `${ledAt(LEDS - 1 - order, scene)}:${color} 300ms pulse${delay}`;
      }).join(";");
      return {
        ...service,
        announce: `${settings(scene)}${wave}\n${row(service.slots, scene)} 500ms ease`,
        announce_ms: 300 + (LEDS - 1) * step + 500,
      };
    },
  },
];
