// agent-leds: tells the agent-leds service what OpenCode's database does not
// record. `agent-leds install` writes this file and `agent-leds uninstall`
// removes it. OpenCode 1.x runs `server`, and 2.x runs `setup`.
import { spawnSync } from "node:child_process";

const AGENT_LEDS = "__AGENT_LEDS__";

// The hook runs with no shell between, so its parent is this process: the
// one that runs the session, a 2.x service included.
const send = (payload) => {
  try {
    spawnSync(AGENT_LEDS, ["hook", "opencode"], {
      input: JSON.stringify(payload),
      stdio: ["pipe", "ignore", "ignore"],
      timeout: 2000,
    });
  } catch {}
};

const busy = (session, directory) => ({ event: "busy", session, pid: process.pid, directory: directory ?? "" });

// A request asked names itself by `id`, and a reply names it by `requestID`.
const permission = (type, session, data) =>
  send({ event: type, session, permission: type === "permission.asked" ? data.id : data.requestID });

const server = async ({ directory }) => {
  /** The sessions that are busy, so each run is told once and not at every step. */
  const running = new Set();
  return {
    event: async ({ event }) => {
      const p = event.properties ?? {};
      if (!p.sessionID) return;
      switch (event.type) {
        case "session.status":
          // A retry is the same run going on.
          if (p.status?.type === "idle") running.delete(p.sessionID);
          else if (p.status?.type === "busy" && !running.has(p.sessionID)) {
            running.add(p.sessionID);
            send(busy(p.sessionID, directory));
          }
          return;
        case "permission.asked":
        case "permission.replied":
          permission(event.type, p.sessionID, p);
          return;
        case "session.error":
          // Only an error that ends the run. Before the session goes busy
          // it is a file OpenCode could not read, and the run goes on, or a
          // prompt that wrote nothing; a context overflow is compacted, or
          // else written on the message.
          if (!running.has(p.sessionID) || p.error?.name === "ContextOverflowError") return;
          send({ event: event.type, session: p.sessionID, error: p.error?.name ?? "Error" });
          return;
      }
    },
  };
};

// 2.x writes how each run ends; only the run's start, with this process, and
// its permissions are not written. A run the service resumes as it starts
// again, or one under way as this file loads, begins with a step rather
// than a start, so the first a process hears of a session says it runs.
const TYPES = ["session.execution.started", "session.step.started", "permission.asked", "permission.replied"];

const setup = async (api) => {
  const directory = api.location?.directory;
  /** The sessions this process has told of. */
  const told = new Set();
  (async () => {
    for await (const event of api.event.subscribe({ types: TYPES })) {
      const session = event.data?.sessionID;
      if (!session) continue;
      const first = !told.has(session) && event.type !== "permission.replied";
      if (first) told.add(session);
      // A run of 2.x's, which its rows say the end of.
      if (event.type === "session.execution.started" || first) send({ ...busy(session, directory), run: true });
      if (event.type.startsWith("permission.")) permission(event.type, session, event.data);
    }
  })().catch(() => {});
};

export default { id: "agent-leds", server, setup };
