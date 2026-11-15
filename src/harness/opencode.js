// agent-leds: tells the agent-leds service what OpenCode's database does not
// record. `agent-leds install` writes this file and `agent-leds uninstall`
// removes it.
import { spawnSync } from "node:child_process";

const AGENT_LEDS = "__AGENT_LEDS__";

export const AgentLeds = async ({ directory }) => {
  /** The sessions that are busy, so each run is told once and not at every step. */
  const busy = new Set();

  // The hook runs with no shell between, so its parent is this process.
  const send = (payload) => {
    try {
      spawnSync(AGENT_LEDS, ["hook", "opencode"], {
        input: JSON.stringify(payload),
        stdio: ["pipe", "ignore", "ignore"],
        timeout: 2000,
      });
    } catch {}
  };

  return {
    event: async ({ event }) => {
      const p = event.properties ?? {};
      if (!p.sessionID) return;
      switch (event.type) {
        case "session.status":
          // A retry is the same run going on.
          if (p.status?.type === "idle") busy.delete(p.sessionID);
          else if (p.status?.type === "busy" && !busy.has(p.sessionID)) {
            busy.add(p.sessionID);
            send({ event: "busy", session: p.sessionID, pid: process.pid, directory: directory ?? "" });
          }
          return;
        case "permission.asked":
        case "permission.replied":
          send({ event: event.type, session: p.sessionID });
          return;
        case "session.error":
          // Only an error that ends the run. Before the session goes busy
          // it is a file OpenCode could not read, and the run goes on, or a
          // prompt that wrote nothing; a context overflow is compacted, or
          // else written on the message.
          if (!busy.has(p.sessionID) || p.error?.name === "ContextOverflowError") return;
          send({ event: event.type, session: p.sessionID, error: p.error?.name ?? "Error" });
          return;
      }
    },
  };
};
