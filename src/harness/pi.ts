// agent-leds: tells the agent-leds service what each Pi session needs.
// `agent-leds install` writes this file and `agent-leds uninstall` removes it.
import { spawnSync } from "node:child_process";

const AGENT_LEDS = "__AGENT_LEDS__";

export default function (pi: any) {
  /** Whether a run is going on, which is when a prompt to the user holds it up. */
  let running = false;

  const send = (ctx: any, event: string, fields: object = {}) => {
    // Without a UI no one is there to need anything: print and JSON modes,
    // and the subagents that run in them.
    if (!ctx.hasUI) return;
    const payload = { event, session: ctx.sessionManager.getSessionId(), cwd: ctx.cwd, ...fields };
    try {
      spawnSync(AGENT_LEDS, ["hook", "pi"], {
        input: JSON.stringify(payload),
        stdio: ["pipe", "ignore", "ignore"],
        timeout: 2000,
      });
    } catch {}
  };

  pi.on("agent_start", (_event: any, ctx: any) => {
    running = true;
    send(ctx, "agent_start");
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
    const text = (last?.content ?? [])
      .filter((part: any) => part.type === "text")
      .map((part: any) => part.text)
      .join("\n");
    send(ctx, "agent_settled", {
      aborted: event.aborted === true,
      stop_reason: last?.stopReason ?? null,
      message: text,
    });
  });
  pi.on("session_shutdown", (event: any, ctx: any) => {
    // A reload keeps the session; anything else leaves it.
    if (event.reason !== "reload") send(ctx, "session_shutdown");
  });
}
