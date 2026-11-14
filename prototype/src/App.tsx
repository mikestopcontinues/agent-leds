import { useEffect, useRef, useState } from "react";
import { DEFAULT_MODEL, appearanceHex, css, deviceColorFor, fromHex, toHex, type Gamut, type Model, type RGB } from "./color";
import { DESIGNS, STATES, type Config, type Palette, type Rendering, type Scene, type State } from "./designs";
import { Engine, LED_COUNT } from "./engine";

type Item = { id: number; state: State };
type NamedPalette = { name: string; about: string; colors: Palette };
type ServiceRendering = Rendering & { config: Config };

const MAX_BYTES = 512;
const DEVICE_HOLD_SECONDS = 10;

const GAMUT: Gamut =
  window.matchMedia("(color-gamut: p3)").matches && CSS.supports("color", "color(display-p3 1 0 0)") ? "p3" : "srgb";

async function api<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, body === undefined ? undefined : { method: "POST", body: JSON.stringify(body) });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error);
  return data as T;
}

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
  const [config, setConfig] = useState<Config | null>(null);
  const [palettes, setPalettes] = useState<NamedPalette[]>([]);
  const [items, setItems] = useState<Item[]>([]);
  const [designId, setDesignId] = useState("service");
  const [rendering, setRendering] = useState<Rendering | null>(null);
  const [scratch, setScratch] = useState("#FF0044 #FF8800 #FFFF00 #00FF66 #00CCFF #004CFF #8800FF #FF00CC\nroll 2s linear\nrepeat");
  const [trueColor, setTrueColor] = useState(true);
  const [model, setModel] = useState<Model>(DEFAULT_MODEL);
  const [diffusion, setDiffusion] = useState(0.35);
  const [message, setMessage] = useState<{ text: string; bad: boolean } | null>(null);
  // The state to announce on the next render; set by queue edits, consumed once.
  const announce = useRef<State | null>(null);
  const design = DESIGNS.find((candidate) => candidate.id === designId)!;

  useEffect(() => {
    fetch("/sdled.wasm")
      .then((response) => response.arrayBuffer())
      .then(Engine.load)
      .then(setEngine);
    api<ServiceRendering>("/api/render", { states: [] })
      .then((service) => setConfig(service.config))
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

  // Re-render and replay whenever the queue, the look or the design changes.
  useEffect(() => {
    if (!engine || !config) return;
    let stale = false;
    let settle: number | undefined;
    const scene: Scene = { ...config, states: items.map((entry) => entry.state), announce: announce.current };
    announce.current = null;
    api<ServiceRendering>("/api/render", scene)
      .then((service) => {
        if (stale) return;
        const result = design.adapt(scene, service);
        setRendering(result);
        setMessage(null);
        if (result.announce && run(result.announce)) {
          settle = window.setTimeout(() => run(result.steady), result.announce_ms + 50);
        } else {
          run(result.steady);
        }
      })
      .catch((error) => stale || fail(error));
    return () => {
      stale = true;
      window.clearTimeout(settle);
    };
  }, [engine, config, items, design]);

  function changeQueue(next: Item[], entered: State | null) {
    announce.current = entered;
    setItems(next);
  }

  async function showOnDevice(programs: Rendering | string) {
    try {
      if (typeof programs === "string") {
        await api("/api/play", { program: programs, seconds: DEVICE_HOLD_SECONDS });
      } else {
        if (programs.announce) {
          await api("/api/play", { program: programs.announce, seconds: DEVICE_HOLD_SECONDS });
          await new Promise((resolve) => setTimeout(resolve, programs.announce_ms));
        }
        await api("/api/play", { program: programs.steady, seconds: DEVICE_HOLD_SECONDS });
      }
      setMessage({ text: `Showing on the device for ${DEVICE_HOLD_SECONDS} seconds, then the real queue returns.`, bad: false });
    } catch (error) {
      fail(error);
    }
  }

  async function saveToService() {
    try {
      await api("/api/config", config);
      setMessage({ text: "Saved. The service now uses these colors, brightness and front light.", bad: false });
    } catch (error) {
      fail(error);
    }
  }

  if (!engine || !config) return <p className="loading">{message?.text ?? "Loading…"}</p>;

  const setColor = (state: State, color: string) => setConfig({ ...config, colors: { ...config.colors, [state]: color } });

  return (
    <main>
      <header>
        <h1>leds workshop</h1>
        <p>
          Programs run in the firmware's own interpreter. {GAMUT === "p3" ? "This display shows Display P3." : "This display is limited to sRGB."}
        </p>
      </header>

      <section className="stage">
        <Strip engine={engine} trueColor={trueColor} model={model} diffusion={diffusion} frontLed={config.front_led} />
        <div className="controls">
          <label className="toggle">
            <input type="checkbox" checked={trueColor} onChange={(event) => setTrueColor(event.target.checked)} />
            True color
          </label>
          <Slider label="Exposure" value={model.exposure} min={0.25} max={4} step={0.05} disabled={!trueColor}
            onChange={(exposure) => setModel({ ...model, exposure })} />
          <Slider label="Response" value={model.response} min={0.6} max={2.4} step={0.05} disabled={!trueColor}
            onChange={(response) => setModel({ ...model, response })} />
          <Slider label="Diffusion" value={diffusion} min={0} max={1} step={0.05} disabled={!trueColor}
            onChange={setDiffusion} />
          <button disabled={!trueColor} onClick={() => { setModel(DEFAULT_MODEL); setDiffusion(0.35); }}>Reset</button>
        </div>
        <p className="note">
          {trueColor
            ? "True color models the Pro's WS2816 LEDs: light linear in the value, near single-wavelength dies, white-balanced by the firmware. Response 1 is the hardware's linear response. The model comes from the datasheet, so tune it against the device."
            : "Showing each value as the screen color with the same hex. On the device, dim values are much brighter and mixed colors lean toward their weaker die."}
        </p>
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
        </section>

        <section>
          <h2>Design</h2>
          <div className="row">
            {DESIGNS.map((candidate) => (
              <button key={candidate.id} className={candidate.id === designId ? "chosen" : ""} onClick={() => setDesignId(candidate.id)}>
                {candidate.name}
              </button>
            ))}
          </div>
          <p className="note">{design.about}</p>
          {rendering && (
            <>
              {rendering.announce && <Program title="On the change" text={rendering.announce} />}
              <Program title="Then, held" text={rendering.steady} />
              <button onClick={() => showOnDevice(rendering)}>Show on device</button>
            </>
          )}
          <h2>Scratch program</h2>
          <textarea value={scratch} rows={5} spellCheck={false} onChange={(event) => setScratch(event.target.value)} />
          <div className="row">
            <span className={scratch.length > MAX_BYTES ? "count bad" : "count"}>{new TextEncoder().encode(scratch).length} / {MAX_BYTES} bytes</span>
            <button onClick={() => run(scratch) && setMessage(null)}>Run here</button>
            <button onClick={() => showOnDevice(scratch)}>Show on device</button>
          </div>
        </section>

        <section>
          <h2>Colors</h2>
          <table className="colors">
            <thead>
              <tr><th>State</th><th>Value sent</th><th>Pick the look</th></tr>
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
                </tr>
              ))}
            </tbody>
          </table>
          <p className="note">"Pick the look" chooses the color you want to see and works out the value to send for it.</p>
          <div className="row">
            {palettes.map((palette) => (
              <button key={palette.name} title={palette.about} onClick={() => setConfig({ ...config, colors: palette.colors })}>
                {STATES.map((state) => <Swatch key={state} color={palette.colors[state]} trueColor={trueColor} model={model} />)} {palette.name}
              </button>
            ))}
          </div>
          <Slider label="Brightness" value={config.brightness} min={0} max={255} step={1} onChange={(brightness) => setConfig({ ...config, brightness })} />
          <label className="field">
            Front of the queue
            <select value={config.front_led} onChange={(event) => setConfig({ ...config, front_led: Number(event.target.value) })}>
              <option value={7}>light 7</option>
              <option value={0}>light 0</option>
            </select>
          </label>
          <button onClick={saveToService}>Save to the service</button>
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
        {props.title} <span className="count">{new TextEncoder().encode(props.text).length} / {MAX_BYTES} bytes</span>
      </figcaption>
      <pre>{props.text}</pre>
    </figure>
  );
}

function Slider(props: { label: string; value: number; min: number; max: number; step: number; disabled?: boolean; onChange(value: number): void }) {
  return (
    <label className="field">
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
