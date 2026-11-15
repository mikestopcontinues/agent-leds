// agent-leds: tells the agent-leds service what Pi's session files cannot.
// `agent-leds install` writes this file and `agent-leds uninstall` removes it.
//
// Pi writes each session to a file of its own, which the service reads. What
// no file records is told here: which process runs the session and where its
// file is, when a dialog holds a run up, and how a run settled, which a file
// cannot say while Pi may still retry.
import { spawnSync } from "node:child_process";

const AGENT_LEDS = "__AGENT_LEDS__";

export default function (pi: any) {
  /** Whether a run is going on, which is when a prompt to the user holds it up. */
  let running = false;
  /** What Pi aborts when the user stops the run, as the run began. */
  let signal: AbortSignal | undefined;

  const send = (ctx: any, event: string, fields: object = {}) => {
    // Without a UI no one is there to need anything: print and JSON modes,
    // and the subagents that run in them.
    if (!ctx.hasUI) return;
    const payload = { event, session: ctx.sessionManager.getSessionId(), ...fields };
    try {
      spawnSync(AGENT_LEDS, ["hook", "pi"], {
        input: JSON.stringify(payload),
        stdio: ["pipe", "ignore", "ignore"],
        timeout: 2000,
      });
    } catch {}
  };

  /** The process that runs the session, the folder it works in, and the file it writes. */
  const where = (ctx: any) => ({
    pid: process.pid,
    cwd: ctx.cwd,
    file: ctx.sessionManager.getSessionFile() ?? null,
  });

  pi.on("session_start", (_event: any, ctx: any) => {
    send(ctx, "session_start", where(ctx));
  });
  pi.on("agent_start", (_event: any, ctx: any) => {
    running = true;
    signal = ctx.signal;
    send(ctx, "agent_start", where(ctx));
  });
  pi.on("ui_prompt_start", (_event: any, ctx: any) => {
    if (running) send(ctx, "ui_prompt_start");
  });
  pi.on("ui_prompt_end", (_event: any, ctx: any) => {
    if (running) send(ctx, "ui_prompt_end");
  });
  pi.on("agent_settled", (event: any, ctx: any) => {
    running = false;
    const last = [...ctx.sessionManager.getBranch()]
      .reverse()
      .find((entry: any) => entry.type === "message" && entry.message?.role === "assistant")?.message;
    send(ctx, "agent_settled", {
      // The signal says so on every version; 1.5.2's own field also covers
      // a stop during a retry's wait, which aborts no run.
      aborted: signal?.aborted === true || event.aborted === true,
      stop_reason: last?.stopReason ?? null,
    });
    signal = undefined;
  });
  pi.on("session_shutdown", (event: any, ctx: any) => {
    // A reload keeps the session; anything else leaves it.
    if (event.reason !== "reload") send(ctx, "session_shutdown");
  });
}
