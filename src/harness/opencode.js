// agent-leds: tells the agent-leds service what each OpenCode session needs.
// `agent-leds install` writes this file and `agent-leds uninstall` removes it.
import { spawnSync } from "node:child_process";

const AGENT_LEDS = "__AGENT_LEDS__";

export const AgentLeds = async () => {
  /** Each session's parent and folder, from the events that describe it. */
  const sessions = new Map();
  /** Whether each session is busy, so a run is reported once and not on every step. */
  const busy = new Map();
  /** Each session's last error. */
  const errors = new Map();
  /** Each session's newest assistant message, and that message's text. A
   * message stopped by an abort can still update after the next begins. */
  const newest = new Map();
  const texts = new Map();

  const send = (event, sessionID, fields = {}) => {
    const session = sessions.get(sessionID) ?? {};
    const parent = session.parentID ? sessions.get(session.parentID) ?? {} : session;
    // A subagent runs in a session of its own, for the session that started
    // it: its work is that session's work, and its questions hold that
    // session up. Its runs ending say nothing about that session.
    const subagent = Boolean(session.parentID);
    if (subagent && (event === "session.idle" || event === "session.deleted")) return;
    const payload = {
      event,
      session: session.parentID ?? sessionID,
      subagent: subagent && event === "session.busy",
      directory: parent.directory ?? session.directory ?? "",
      ...fields,
    };
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
      switch (event.type) {
        case "session.created":
        case "session.updated":
          if (p.info?.id) sessions.set(p.info.id, { parentID: p.info.parentID, directory: p.info.directory });
          return;
        case "message.updated":
          if (p.info?.role === "assistant" && !texts.has(p.info.id)) {
            texts.delete(newest.get(p.info.sessionID));
            newest.set(p.info.sessionID, p.info.id);
            texts.set(p.info.id, "");
          }
          return;
        case "message.part.updated":
          if (p.part?.type === "text" && texts.has(p.part.messageID)) texts.set(p.part.messageID, p.part.text ?? "");
          return;
        case "session.status":
          if (p.status?.type === "busy" && !busy.get(p.sessionID)) {
            busy.set(p.sessionID, true);
            errors.delete(p.sessionID);
            send("session.busy", p.sessionID);
          }
          return;
        case "session.error":
          if (p.sessionID) errors.set(p.sessionID, p.error?.name ?? "Error");
          return;
        case "session.idle":
          busy.set(p.sessionID, false);
          send("session.idle", p.sessionID, {
            error: errors.get(p.sessionID) ?? null,
            message: texts.get(newest.get(p.sessionID)) ?? "",
          });
          return;
        case "permission.asked":
        case "permission.replied":
        case "question.asked":
        case "question.replied":
        case "question.rejected":
        case "session.deleted":
          send(event.type, p.sessionID);
          return;
      }
    },
  };
};
