// What the installed `leds` service takes and renders, as the workshop sees it.

export const STATES = ["error", "question", "stopped", "working"] as const;
export type State = (typeof STATES)[number];
export type Palette = Record<State, string>;

/** How a change is announced; see `Snake` in the service's config. */
export type Snake = {
  nose: number;
  tail: number;
  hold_ms: number;
  move_ms: number;
  fade_out_ms: number;
  fade_in_ms: number;
};

export type Config = {
  brightness: number;
  front_led: number;
  balance: [number, number, number];
  colors: Palette;
  snake: Snake;
};

/** A queue to render, announcing a session entering `announce`. Without a config the service uses its saved one. */
export type Scene = { states: State[]; announce: State | null; config?: Config };

/** One program of an animation and how long it runs before the next is written. */
export type Step = { program: string; ms: number };

/** What `leds render` returns for a scene. */
export type Rendering = {
  /** The state shown by each lit light, front of the queue first. */
  slots: State[];
  /** The state colors as sent, scaled to look equally bright. */
  lit: Palette;
  /** The programs that announce the change, written one after another. */
  steps: Step[];
  /** The program that then holds the queue. */
  steady: string;
  config: Config;
};

/** The same hue with its light multiplied by `factor`. */
export function scaled(hex: string, factor: number): string {
  const channel = (at: number) =>
    Math.min(255, Math.round(parseInt(hex.slice(at, at + 2), 16) * factor)).toString(16).padStart(2, "0");
  return `#${channel(1)}${channel(3)}${channel(5)}`.toUpperCase();
}
