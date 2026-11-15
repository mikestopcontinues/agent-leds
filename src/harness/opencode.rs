//! OpenCode: it runs plugins rather than hook commands, so a plugin of
//! ours in `~/.config/opencode/plugins/` passes the events its database does
//! not record to the hook.

use super::Plugin;

pub const PLUGIN: Plugin = Plugin {
    path: ".config/opencode/plugins/agent-leds.js",
    source: include_str!("opencode.js"),
};

/// The events the plugin passes on, by the name it gives each: `busy` as a
/// session goes busy, with the pid of the OpenCode process, and three of
/// OpenCode's own.
pub const HOOKS: &[&str] = &[
    "busy",
    "permission.asked",
    "permission.replied",
    "session.error",
];

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use serde_json::{Value, json};

    use super::*;

    /// What the plugin passes on of OpenCode's events, run by Node with a
    /// stand-in for the binary that writes down each payload with the pid
    /// of the process that ran it, and the pid of the plugin's process.
    fn passed(events: Value) -> (Vec<(i32, Value)>, i32) {
        let folder = tempfile::tempdir().unwrap();
        let out = folder.path().join("out");
        let exe = folder.path().join("agent-leds");
        fs::write(
            &exe,
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = 'hook opencode' ] || exit 1\nprintf '%s ' \"$PPID\" >> '{}'\ncat >> '{0}'\necho >> '{0}'\n",
                out.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = folder.path().join("agent-leds.mjs");
        fs::write(&plugin, PLUGIN.text(&exe)).unwrap();
        let run = format!(
            "const {{ AgentLeds }} = await import({plugin});\n\
             const hooks = await AgentLeds({{ directory: '/Users/me/Code/app' }});\n\
             for (const event of {events}) await hooks.event({{ event }});\n\
             console.log(process.pid);",
            plugin = json!(plugin.to_string_lossy()),
        );
        let node = Command::new("node")
            .args(["--input-type=module", "-e", &run])
            .output()
            .expect("node runs the plugin");
        assert!(
            node.status.success(),
            "{}",
            String::from_utf8_lossy(&node.stderr)
        );
        let pid = String::from_utf8(node.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let text = fs::read_to_string(&out).unwrap_or_default();
        let payloads = text
            .lines()
            .map(|line| {
                let (parent, payload) = line.split_once(' ').unwrap();
                (
                    parent.parse().unwrap(),
                    serde_json::from_str(payload).unwrap(),
                )
            })
            .collect();
        (payloads, pid)
    }

    fn status(session: &str, kind: &str) -> Value {
        json!({ "type": "session.status", "properties": { "sessionID": session, "status": { "type": kind } } })
    }

    /// OpenCode's events in a run, as the probe saw them on the event stream:
    /// busy twice at each step, a permission, an abort.
    #[test]
    fn the_plugin_passes_on_a_run_once_with_its_own_pid_and_what_the_database_does_not_record() {
        let session = "ses_e23f20dc0ffe0K5UEiYZ4lxHfx";
        let (payloads, pid) = passed(json!([
            status(session, "busy"),
            status(session, "busy"),
            { "type": "message.updated", "properties": { "info": { "id": "msg_1", "sessionID": session, "role": "assistant" } } },
            { "type": "permission.asked", "properties": { "id": "per_1dc10dc8f001i7QPGQfw1Ith8O", "sessionID": session, "permission": "bash", "patterns": ["sleep 30"] } },
            { "type": "permission.replied", "properties": { "sessionID": session, "requestID": "per_1dc10dc8f001i7QPGQfw1Ith8O", "reply": "once" } },
            status(session, "busy"),
            { "type": "session.error", "properties": { "sessionID": session, "error": { "name": "MessageAbortedError", "data": { "message": "Aborted" } } } },
            status(session, "idle"),
            { "type": "session.idle", "properties": { "sessionID": session } },
            // A retry is the same run; idle ends it.
            status(session, "busy"),
            { "type": "session.status", "properties": { "sessionID": session, "status": { "type": "retry", "attempt": 1 } } },
            status(session, "busy"),
            status(session, "idle"),
            { "type": "session.error", "properties": { "error": { "name": "UnknownError" } } },
            { "type": "question.asked", "properties": { "id": "que_1", "sessionID": session } },
            { "type": "session.deleted", "properties": { "info": { "id": session } } },
        ]));
        // The plugin runs the hook itself, with no shell between.
        assert!(payloads.iter().all(|(parent, _)| *parent == pid));
        let payloads: Vec<Value> = payloads.into_iter().map(|(_, payload)| payload).collect();
        let busy = json!({ "event": "busy", "session": session, "pid": pid, "directory": "/Users/me/Code/app" });
        assert_eq!(
            payloads,
            [
                busy.clone(),
                json!({ "event": "permission.asked", "session": session }),
                json!({ "event": "permission.replied", "session": session }),
                json!({ "event": "session.error", "session": session, "error": "MessageAbortedError" }),
                busy,
            ]
        );
        let mut events: Vec<&str> = payloads
            .iter()
            .map(|payload| payload["event"].as_str().unwrap())
            .collect();
        events.sort();
        events.dedup();
        let mut hooks = HOOKS.to_vec();
        hooks.sort();
        assert_eq!(events, hooks);
    }

    #[test]
    fn the_plugin_passes_on_a_subagent_s_events_under_its_own_session() {
        let child = "ses_e23ed2339ffeTL0ywE5nig2JRH";
        let (payloads, _) = passed(json!([
            status(child, "busy"),
            { "type": "permission.asked", "properties": { "id": "per_1dc12e08e001fRVLcMJg4fWfpR", "sessionID": child, "permission": "bash" } },
        ]));
        let sessions: Vec<&Value> = payloads
            .iter()
            .map(|(_, payload)| &payload["session"])
            .collect();
        assert_eq!(sessions, [child, child]);
    }
}
