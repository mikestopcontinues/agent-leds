import { useEffect, useRef, useState } from "react";
import { DEFAULT_MODEL, appearanceHex, css, deviceColorFor, fromHex, toHex, type Gamut, type Model, type RGB } from "./color";
import { Engine, LED_COUNT } from "./engine";
import { STATES, scaled, type Config, type Palette, type Rendering, type Scene, type Snake, type State } from "./service";

type Item = { id: number; state: State };
type NamedPalette = { name: string; about: string; colors: Palette };

const MAX_BYTES = 512;
const DEVICE_HOLD_SECONDS = 10;
// Matched against the device by eye, like the default color model.
const DEFAULT_DIFFUSION = 0.2;

const GAMUT: Gamut =
  window.matchMedia("(color-gamut: p3)").matches && CSS.supports("color", "color(display-p3 1 0 0)") ? "p3" : "srgb";

async function api<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, body === undefined ? undefined : { method: "POST", body: JSON.stringify(body) });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error);
  return data as T;
}

const bytes = (text: string) => new TextEncoder().encode(text).length;
const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);

const PRESETS: Record<string, State[]> = {
  Empty: [],
  "One of each": ["error", "question", "stopped", "working"],
  "Busy day": ["error", "question", "question", "stopped", "stopped", "working", "working", "working"],
  "Overflow (14)": [
    "error",
    "error",
    "question",
    "question",
    "question",
    "stopped",
    "stopped",
    "stopped",
    "stopped",
    ...Array<State>(5).fill("working"),
  ],
};

let nextId = 1;
const item = (state: State): Item => ({ id: nextId++, state });
const inQueueOrder = (items: Item[]) =>
  [...items].sort((a, b) => STATES.indexOf(a.state) - STATES.indexOf(b.state) || a.id - b.id);

export function App() {
  const [engine, setEngine] = useState<Engine | null>(null);
  // The settings the service has, and the ones being tried here.
  const [saved, setSaved] = useState<Config | null>(null);
  const [config, setConfig] = useState<Config | null>(null);
  const [palettes, setPalettes] = useState<NamedPalette[]>([]);
  const [items, setItems] = useState<Item[]>(() => PRESETS["One of each"]!.map(item));
  const [rendering, setRendering] = useState<Rendering | null>(null);
  const [replays, setReplays] = useState(0);
  const [trueColor, setTrueColor] = useState(true);
  const [model, setModel] = useState<Model>(DEFAULT_MODEL);
  const [diffusion, setDiffusion] = useState(DEFAULT_DIFFUSION);
  const [message, setMessage] = useState<{ text: string; bad: boolean } | null>(null);
  // Whether the next render plays the change; set by whatever should show it, consumed once.
  const playChange = useRef(false);
  const lastEntered = useRef<State | null>(null);
  // The queue before the last change made on this page.
  const before = useRef<State[] | null>(null);

  useEffect(() => {
    fetch("/sdled.wasm")
      .then((response) => response.arrayBuffer())
      .then(Engine.load)
      .then(setEngine);
    api<Rendering>("/api/render", { states: [], announce: null, from: [] })
      .then((service) => {
        setSaved(service.config);
        setConfig(service.config);
      })
      .catch(fail);
    api<NamedPalette[]>("/api/palettes").then(setPalettes).catch(fail);
  }, []);

  function fail(error: unknown) {
    setMessage({ text: error instanceof Error ? error.message : String(error), bad: true });
  }

  function run(program: string) {
    const parsed = engine!.parse(program, performance.now());
    if (!parsed.ok) setMessage({ text: `The firmware rejects this: ${parsed.error} at line ${parsed.line}`, bad: true });
    return parsed.ok;
  }

  // Render through the service and play the result whenever the queue or a setting changes.
  useEffect(() => {
    if (!engine || !config) return;
    let stale = false;
    const timers: number[] = [];
    const states = items.map((entry) => entry.state);
    // Always render the change, so its programs and timing are on hand; play it only when asked.
    const scene: Scene = { ...changeTo(states), config };
    const play = playChange.current;
    playChange.current = false;
    api<Rendering>("/api/render", scene)
      .then((result) => {
        if (stale) return;
        setRendering(result);
        setMessage(null);
        // Write each program when the one before it ends, as the service does.
        let at = 0;
        for (const step of play ? result.steps : []) {
          timers.push(window.setTimeout(() => run(step.program), at));
          at += step.ms;
        }
        timers.push(window.setTimeout(() => run(result.steady), at));
      })
      .catch((error) => stale || fail(error));
    return () => {
      stale = true;
      timers.forEach((timer) => window.clearTimeout(timer));
    };
  }, [engine, config, items, replays]);

  /** The change to show for a queue: the last one made on this page, or until then the newest item arriving from dark. */
  function changeTo(states: State[]): Omit<Scene, "config"> {
    return before.current
      ? { states, announce: lastEntered.current, from: before.current }
      : { states, announce: states.at(-1) ?? null, from: [] };
  }

  if (!engine || !config || !saved) return <p className="loading">{message?.text ?? "Loading…"}</p>;

  const queued = items.map((entry) => entry.state);
  const change = changeTo(queued);
  const unsaved = !same(config, saved);

  function changeQueue(next: Item[], entered: State | null) {
    before.current = queued;
    lastEntered.current = entered;
    playChange.current = true;
    setItems(next);
  }

  function replay() {
    playChange.current = true;
    setReplays(replays + 1);
  }

  const set = (change: Partial<Config>) => setConfig({ ...config, ...change });
  const setColor = (state: State, color: string) => set({ colors: { ...config.colors, [state]: color } });
  const setBalance = (die: number, value: number) =>
    set({ balance: config.balance.map((old, index) => (index === die ? value : old)) as Config["balance"] });
  // A change to the snake replays it, so each adjustment is seen at once.
  function setSnake(change: Partial<Snake>) {
    playChange.current = true;
    set({ snake: { ...config!.snake, ...change } });
  }
  const snake = config.snake;
  const setHead = (light: number, level: number) =>
    setSnake({ head: snake.head.map((old, index) => (index === light ? level : old)) as Snake["head"] });

  async function act(action: () => Promise<string>) {
    try {
      setMessage({ text: await action(), bad: false });
    } catch (error) {
      fail(error);
    }
  }

  const showOnDevice = () =>
    act(async () => {
      await api("/api/show", { scene: { ...change, config }, seconds: DEVICE_HOLD_SECONDS });
      return `Showing these settings on the device for ${DEVICE_HOLD_SECONDS} seconds, then the real queue returns.`;
    });

  const save = () =>
    act(async () => {
      const result = await api<Config>("/api/config", config);
      setSaved(result);
      setConfig(result);
      return "Saved. The service is using these settings now.";
    });

  // The three dies side by side, each scaled so that all look as bright as the dimmest one at full value.
  const dimmest = Math.min(...config.balance);
  const dies = ["#FF0000", "#00FF00", "#0000FF"].map((die, index) => scaled(die, dimmest / config.balance[index]!));
  const diesProgram = `${dies[0]} ${dies[0]} #000000 ${dies[1]} ${dies[1]} #000000 ${dies[2]} ${dies[2]}`;
  const showDiesOnDevice = () =>
    act(async () => {
      await api("/api/play", { program: diesProgram, seconds: DEVICE_HOLD_SECONDS });
      return `Showing the dies on the device for ${DEVICE_HOLD_SECONDS} seconds, then the real queue returns.`;
    });

  const changeMs = rendering?.steps.reduce((sum, step) => sum + step.ms, 0) ?? 0;

  return (
    <main>
      <header>
        <h1>leds workshop</h1>
        <p>
          Tune the service's settings against the device. The lights below are what the service renders, run in the
          firmware's own interpreter. {GAMUT === "p3" ? "This display shows Display P3." : "This display is limited to sRGB."}
        </p>
      </header>

      <section className="stage">
        <Strip engine={engine} trueColor={trueColor} model={model} diffusion={diffusion} frontLed={config.front_led} />
        <div className="actions">
          <button disabled={!change} onClick={replay}>Replay the change</button>
          <button onClick={showOnDevice}>Show on device</button>
          <span className={unsaved ? "status unsaved" : "status"}>{unsaved ? "Unsaved changes" : "Saved to the service"}</span>
          <button disabled={!unsaved} onClick={() => setConfig(saved)}>Revert</button>
          <button className="primary" disabled={!unsaved} onClick={save}>Save to the service</button>
        </div>
        {message && <p className={message.bad ? "message bad" : "message"}>{message.text}</p>}
      </section>

      <div className="panels">
        <section>
          <h2>Queue</h2>
          <div className="row">
            {STATES.map((state) => (
              <button key={state} className="add" onClick={() => changeQueue([...items, item(state)], state)}>
                <Swatch color={config.colors[state]} trueColor={trueColor} model={model} /> + {state}
              </button>
            ))}
          </div>
          <ol className="queue">
            {inQueueOrder(items).map((entry) => (
              <li key={entry.id}>
                <Swatch color={config.colors[entry.state]} trueColor={trueColor} model={model} />
                <select
                  value={entry.state}
                  onChange={(event) => {
                    const state = event.target.value as State;
                    changeQueue(items.map((other) => (other.id === entry.id ? { ...other, state } : other)), state);
                  }}
                >
                  {STATES.map((state) => <option key={state}>{state}</option>)}
                </select>
                <button aria-label="Remove" onClick={() => changeQueue(items.filter((other) => other.id !== entry.id), null)}>×</button>
              </li>
            ))}
            {items.length === 0 && <li className="empty">Nothing in the queue: the lights are off.</li>}
          </ol>
          <div className="row">
            {Object.entries(PRESETS).map(([name, states]) => (
              <button key={name} onClick={() => changeQueue(states.map(item), null)}>{name}</button>
            ))}
          </div>
          <p className="note">A test queue for this page only. Adding an item or changing its state plays the change.</p>
        </section>

        <section>
          <h2>Change animation</h2>
          <p className="note">
            The lights fade out. A head in the new state's color crosses the strip toward the front of the queue and
            pulls the new queue in as its tail. The queue then rises to full in a wave from its front to its back: each light, the front one included, goes dark just before the wave reaches it, then rises at an even pace.
          </p>
          <h3>Head</h3>
          {["Front", "Middle", "Back"].map((light, index) => (
            <Slider key={light} label={light} value={snake.head[index]!} min={0} max={1} step={0.05}
              title={index === 1 ? "brightness of the head's middle light" : `brightness of the head's ${light.toLowerCase()} light; at 0 the head has no ${light.toLowerCase()} light`}
              onChange={(level) => setHead(index, level)} />
          ))}
          <label className="toggle">
            <input type="checkbox" checked={snake.gap} onChange={(event) => setSnake({ gap: event.target.checked })} />
            Blank space between head and tail
          </label>
          <h3>Tail</h3>
          <Slider label="Brightness" title="brightness of the queue while it is pulled in" value={snake.tail}
            min={0} max={1} step={0.05} onChange={(tail) => setSnake({ tail })} />
          <h3>Timing</h3>
          <Slider label="Fade out ms" title="how long the old queue takes to go dark" value={snake.fade_out_ms}
            min={10} max={1500} step={10} onChange={(fade_out_ms) => setSnake({ fade_out_ms })} />
          <Slider label="Step ms" title="the pace of the rest: how long the head, and then the wave, takes to move on one light; each light rises over seven steps" value={snake.step_ms}
            min={20} max={400} step={5} onChange={(step_ms) => setSnake({ step_ms })} />
          <p className="note">
            {change
              ? `The change takes ${(changeMs / 1000).toFixed(2)} s in ${rendering?.steps.length === 1 ? "one program" : `${rendering?.steps.length ?? 0} programs`}.`
              : "Add an item to the queue to see the change."}
          </p>
          <details>
            <summary>Programs the service writes</summary>
            {rendering?.steps.map((step, index) => (
              <Program key={index} title={`Change, part ${index + 1} · ${(step.ms / 1000).toFixed(2)} s`} text={step.program} />
            ))}
            {rendering && <Program title="Then, held" text={rendering.steady} />}
          </details>
        </section>

        <section>
          <h2>Clearing lights</h2>
          <p className="note">
            A session whose turn has ended keeps its light until you have looked at it in the desktop app, and then
            for the minutes below. That covers stopped sessions and ones that only ended on a question. A prompt the
            agent is blocked on, or an error, stays until it is dealt with. Any session that has been silent for the
            days below is dropped, whatever its state.
          </p>
          <label className="field">
            After read, minutes
            <Whole value={config.keep_read_minutes} min={0} max={1440} onChange={(keep_read_minutes) => set({ keep_read_minutes })} />
          </label>
          <label className="field">
            Silent, days
            <Whole value={config.abandon_days} min={1} max={365} onChange={(abandon_days) => set({ abandon_days })} />
          </label>
        </section>

        <section>
          <h2>Colors</h2>
          <table className="colors">
            <thead>
              <tr><th>State</th><th>Hue</th><th>Pick the look</th><th>Sent</th></tr>
            </thead>
            <tbody>
              {STATES.map((state) => (
                <tr key={state}>
                  <td><Swatch color={config.colors[state]} trueColor={trueColor} model={model} /> {state}</td>
                  <td><HexInput value={config.colors[state]} onChange={(color) => setColor(state, color)} /></td>
                  <td>
                    <input
                      type="color"
                      value={appearanceHex(fromHex(config.colors[state])!, model).toLowerCase()}
                      onChange={(event) => setColor(state, toHex(deviceColorFor(event.target.value, model)!))}
                    />
                  </td>
                  <td><code>{rendering?.lit[state]}</code></td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="note">
            A state's color sets its hue. "Pick the look" chooses the color you want to see and works out the hue for it.
            "Sent" is that hue scaled so every state looks equally bright.
          </p>
          <div className="row">
            {palettes.map((palette) => (
              <button key={palette.name} title={palette.about} className={same(palette.colors, config.colors) ? "chosen" : ""}
                onClick={() => set({ colors: palette.colors })}>
                {STATES.map((state) => <Swatch key={state} color={palette.colors[state]} trueColor={trueColor} model={model} />)} {palette.name}
              </button>
            ))}
          </div>
        </section>

        <section>
          <h2>Brightness</h2>
          <Slider label="Overall" value={config.brightness} min={0} max={255} step={1} onChange={(brightness) => set({ brightness })} />
          <label className="field">
            Front of queue
            <select value={config.front_led} onChange={(event) => set({ front_led: Number(event.target.value) })}>
              <option value={7}>light 7</option>
              <option value={0}>light 0</option>
            </select>
          </label>
          <h2>Equal brightness</h2>
          <p className="note">
            How bright each die looks at full value, relative to the others. Show the dies on the device and move the
            sliders until the red, green and blue pairs look equally bright.
          </p>
          {["Red", "Green", "Blue"].map((die, index) => (
            <Slider key={die} label={die} value={config.balance[index]!} min={0.02} max={1} step={0.01} onChange={(value) => setBalance(index, value)} />
          ))}
          <div className="row">
            <button onClick={() => run(diesProgram) && setMessage(null)}>Show dies here</button>
            <button onClick={showDiesOnDevice}>Show dies on device</button>
          </div>
        </section>

        <section>
          <h2>Preview look</h2>
          <label className="toggle">
            <input type="checkbox" checked={trueColor} onChange={(event) => setTrueColor(event.target.checked)} />
            True color
          </label>
          <Slider label="Exposure" value={model.exposure} min={0.25} max={4} step={0.05} disabled={!trueColor}
            onChange={(exposure) => setModel({ ...model, exposure })} />
          <Slider label="Response" value={model.response} min={0.2} max={2.4} step={0.05} disabled={!trueColor}
            onChange={(response) => setModel({ ...model, response })} />
          <Slider label="Diffusion" value={diffusion} min={0} max={1} step={0.05} disabled={!trueColor}
            onChange={setDiffusion} />
          <button disabled={!trueColor} onClick={() => { setModel(DEFAULT_MODEL); setDiffusion(DEFAULT_DIFFUSION); }}>Reset</button>
          <p className="note">
            {trueColor
              ? "How this page draws the lights; nothing here is saved to the service. The defaults were matched against the device by eye. True color models the Pro's WS2816 LEDs: light linear in the value, near single-wavelength dies, white-balanced by the firmware. The model comes from the datasheet, so tune it against the device."
              : "Showing each value as the screen color with the same hex. On the device, dim values are much brighter and mixed colors lean toward their weaker die."}
          </p>
        </section>
      </div>
    </main>
  );
}

function Strip(props: { engine: Engine; trueColor: boolean; model: Model; diffusion: number; frontLed: number }) {
  const [frame, setFrame] = useState<RGB[]>(() => Array<RGB>(LED_COUNT).fill([0, 0, 0]));
  useEffect(() => {
    let request = 0;
    let last = "";
    const tick = () => {
      const next = props.engine.step(performance.now());
      // Most frames are identical once a program settles; skip the re-render.
      if (next.join() !== last) {
        last = next.join();
        setFrame(next);
      }
      request = requestAnimationFrame(tick);
    };
    request = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(request);
  }, [props.engine]);

  const near = props.frontLed === 0 ? LED_COUNT - 1 : 0;
  return (
    <div className="strip">
      <div className="lights" style={{ filter: props.trueColor ? `blur(${props.diffusion * 14}px)` : undefined }}>
        {frame.map((rgb, led) => {
          const color = css(rgb, props.trueColor, props.model, GAMUT);
          return <i key={led} style={{ background: color, boxShadow: props.trueColor ? `0 0 28px 6px ${color}` : undefined }} />;
        })}
      </div>
      <div className="indexes">
        {frame.map((_, led) => (
          <span key={led}>
            {led}
            {led === props.frontLed && <b>front of queue · far from you</b>}
            {led === near && <b>near you</b>}
          </span>
        ))}
      </div>
    </div>
  );
}

function Swatch(props: { color: string; trueColor: boolean; model: Model }) {
  return <i className="swatch" style={{ background: css(fromHex(props.color)!, props.trueColor, props.model, GAMUT) }} />;
}

function Program(props: { title: string; text: string }) {
  return (
    <figure className="program">
      <figcaption>
        {props.title} <span className="count">{bytes(props.text)} / {MAX_BYTES} bytes</span>
      </figcaption>
      <pre>{props.text}</pre>
    </figure>
  );
}

/** A whole number within a range. */
function Whole(props: { value: number; min: number; max: number; onChange: (value: number) => void }) {
  const { min, max } = props;
  return (
    <input type="number" min={min} max={max} step={1} value={props.value}
      onChange={(event) => props.onChange(Math.min(max, Math.max(min, Math.round(Number(event.target.value) || 0))))} />
  );
}

function Slider(props: {
  label: string;
  title?: string;
  value: number;
  min: number;
  max: number;
  step: number;
  disabled?: boolean;
  onChange(value: number): void;
}) {
  return (
    <label className="field" title={props.title}>
      {props.label}
      <input type="range" min={props.min} max={props.max} step={props.step} value={props.value} disabled={props.disabled}
        onChange={(event) => props.onChange(Number(event.target.value))} />
      <output>{props.value}</output>
    </label>
  );
}

/** A hex field that only reports complete, valid colors, so typing never breaks the scene. */
function HexInput(props: { value: string; onChange(color: string): void }) {
  const [text, setText] = useState(props.value);
  useEffect(() => setText(props.value), [props.value]);
  return (
    <input
      className={fromHex(text) ? "hex" : "hex bad"}
      value={text}
      spellCheck={false}
      onChange={(event) => {
        setText(event.target.value);
        const rgb = fromHex(event.target.value);
        if (rgb) props.onChange(toHex(rgb));
      }}
    />
  );
}
