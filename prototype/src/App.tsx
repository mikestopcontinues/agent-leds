import { useEffect, useRef, useState } from "react";
import { DEFAULT_MODEL, appearanceHex, css, deviceColorFor, fromHex, toHex, type Gamut, type Model, type RGB } from "./color";
import { DESIGNS, STATES, scaled, type Config, type Palette, type Rendering, type Scene, type ServiceRendering, type State } from "./designs";
import { Engine, LED_COUNT } from "./engine";

type Item = { id: number; state: State };
type NamedPalette = { name: string; about: string; colors: Palette };

const MAX_BYTES = 512;
const DEVICE_HOLD_SECONDS = 10;
const DEFAULT_DIFFUSION = 0.35;

const GAMUT: Gamut =
  window.matchMedia("(color-gamut: p3)").matches && CSS.supports("color", "color(display-p3 1 0 0)") ? "p3" : "srgb";

async function api<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, body === undefined ? undefined : { method: "POST", body: JSON.stringify(body) });
  const data = await response.json();
  if (!response.ok) throw new Error(data.error);
  return data as T;
}

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
const bytes = (text: string) => new TextEncoder().encode(text).length;

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
  const [lit, setLit] = useState<Palette | null>(null);
  const [rendering, setRendering] = useState<Rendering | null>(null);
  const [replays, setReplays] = useState(0);
  const [scratch, setScratch] = useState("#FF0044 #FF8800 #FFFF00 #00FF66 #00CCFF #004CFF #8800FF #FF00CC\nroll 2s linear\nrepeat");
  const [trueColor, setTrueColor] = useState(true);
  const [model, setModel] = useState<Model>(DEFAULT_MODEL);
  const [diffusion, setDiffusion] = useState(DEFAULT_DIFFUSION);
  const [message, setMessage] = useState<{ text: string; bad: boolean } | null>(null);
  // The state to announce on the next render; set by queue edits and replays, consumed once.
  const announce = useRef<State | null>(null);
  const lastAnnounced = useRef<State | null>(null);
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
    const timers: number[] = [];
    const scene: Scene = { ...config, states: items.map((entry) => entry.state), announce: announce.current };
    announce.current = null;
    api<ServiceRendering>("/api/render", scene)
      .then((service) => {
        if (stale) return;
        const result = design.adapt(scene, service);
        setLit(service.lit);
        setRendering(result);
        setMessage(null);
        // Write each program when the one before it ends, as the service would.
        let at = 0;
        for (const step of result.steps) {
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
  }, [engine, config, items, design, replays]);

  function changeQueue(next: Item[], entered: State | null) {
    announce.current = entered;
    if (entered) lastAnnounced.current = entered;
    setItems(next);
  }

  function replay() {
    announce.current = lastAnnounced.current;
    setReplays(replays + 1);
  }

  async function showOnDevice(programs: Rendering | string) {
    const play = (program: string) => api("/api/play", { program, seconds: DEVICE_HOLD_SECONDS });
    try {
      if (typeof programs === "string") {
        await play(programs);
      } else {
        for (const step of programs.steps) {
          await play(step.program);
          await wait(step.ms);
        }
        await play(programs.steady);
      }
      setMessage({ text: `Showing on the device for ${DEVICE_HOLD_SECONDS} seconds, then the real queue returns.`, bad: false });
    } catch (error) {
      fail(error);
    }
  }

  async function saveToService() {
    try {
      await api("/api/config", config);
      setMessage({ text: "Saved. The service now uses these colors, brightness, die balance and front light.", bad: false });
    } catch (error) {
      fail(error);
    }
  }

  if (!engine || !config) return <p className="loading">{message?.text ?? "Loading…"}</p>;

  const setColor = (state: State, color: string) => setConfig({ ...config, colors: { ...config.colors, [state]: color } });
  const setBalance = (die: number, value: number) =>
    setConfig({ ...config, balance: config.balance.map((old, index) => (index === die ? value : old)) as Config["balance"] });
  // The three dies side by side, each scaled so that all look as bright as the dimmest one at full value.
  const dimmest = Math.min(...config.balance);
  const dies = ["#FF0000", "#00FF00", "#0000FF"].map((die, index) => scaled(die, dimmest / config.balance[index]!));
  const diesProgram = `${dies[0]} ${dies[0]} #000000 ${dies[1]} ${dies[1]} #000000 ${dies[2]} ${dies[2]}`;

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
          <div className="row">
            <button disabled={!lastAnnounced.current} onClick={replay}>Replay the last change</button>
            {rendering && <button onClick={() => showOnDevice(rendering)}>Show on device</button>}
          </div>
          {rendering?.steps.map((step, index) => (
            <Program
              key={index}
              title={`On the change${rendering.steps.length > 1 ? `, part ${index + 1}` : ""} · ${(step.ms / 1000).toFixed(2)} s`}
              text={step.program}
            />
          ))}
          {rendering && <Program title="Then, held" text={rendering.steady} />}
          <h2>Scratch program</h2>
          <textarea value={scratch} rows={5} spellCheck={false} onChange={(event) => setScratch(event.target.value)} />
          <div className="row">
            <span className={bytes(scratch) > MAX_BYTES ? "count bad" : "count"}>{bytes(scratch)} / {MAX_BYTES} bytes</span>
            <button onClick={() => run(scratch) && setMessage(null)}>Run here</button>
            <button onClick={() => showOnDevice(scratch)}>Show on device</button>
          </div>
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
                  <td><code>{lit?.[state]}</code></td>
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
            <button onClick={() => showOnDevice(diesProgram)}>Show dies on device</button>
          </div>
          <button onClick={saveToService}>Save to the service</button>
        </section>

        <section>
          <h2>Color look</h2>
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
          <button disabled={!trueColor} onClick={() => { setModel(DEFAULT_MODEL); setDiffusion(DEFAULT_DIFFUSION); }}>Reset</button>
          <p className="note">
            {trueColor
              ? "True color models the Pro's WS2816 LEDs: light linear in the value, near single-wavelength dies, white-balanced by the firmware. Response 1 is the hardware's linear response. The model comes from the datasheet, so tune it against the device."
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
