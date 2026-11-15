//! OpenCode: it runs plugins rather than hook commands, so a plugin of
//! ours in `~/.config/opencode/plugins/` passes the events its database does
//! not record to the hook. One file serves both versions: its default export
//! holds the `server` that OpenCode 1.x runs and the `setup` that 2.x runs.
//! 2.x loads it again when the file changes, a background service included.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};

use super::Plugin;
use crate::liveness;

pub const PLUGIN: Plugin = Plugin {
    path: ".config/opencode/plugins/agent-leds.js",
    source: include_str!("opencode.js"),
};

/// The log OpenCode 2.x writes, which says when it loads a plugin and when
/// it could not; 1.x logs elsewhere, and neither.
const LOG: &str = ".local/share/opencode/log/opencode.log";

/// Whether the plugin is the one an install from `exe` writes, and, if 2.x
/// tried to load it since it was written, whether it could: 2.x goes on
/// without a plugin it cannot load, and only its log says so.
pub fn check(home: &Path, exe: &Path) -> Result<()> {
    PLUGIN.check(home, exe)?;
    let log = home.join(LOG);
    let text = match fs::read(&log) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", log.display())),
    };
    let plugin = home.join(PLUGIN.path);
    let written = fs::metadata(&plugin)?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_secs();
    if let Some((time, cause)) = failure(&text, &plugin.to_string_lossy())
        && liveness::iso_second(&time).is_some_and(|second| second >= written as i64)
    {
        bail!(
            "OpenCode could not load ~/{} at {time}: {cause}",
            PLUGIN.path
        );
    }
    Ok(())
}

/// When and why the log's last load of the plugin at `path` failed, if it
/// did. 2.x logs `loading plugin` as it starts to load one, and `failed to
/// load plugin` after, as logfmt lines.
fn failure(log: &str, path: &str) -> Option<(String, String)> {
    let named = |key: &str, line: &str| {
        line.contains(&format!(" {key}={path} ")) || line.contains(&format!(" {key}={path:?} "))
    };
    let line = log.lines().rev().find(|line| {
        (line.contains(" msg=\"loading plugin\" ") && named("id", line))
            || (line.contains(" message=\"failed to load plugin\" ") && named("target", line))
    })?;
    if !line.contains(" message=\"failed to load plugin\" ") {
        return None;
    }
    let time = line.strip_prefix("timestamp=")?.split(' ').next()?;
    Some((time.to_owned(), quoted(line, "cause").unwrap_or_default()))
}

/// A logfmt line's quoted value for `key`, unescaped, its whitespace
/// collapsed.
fn quoted(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!(" {key}=\""))? + key.len() + 3;
    let mut value = String::new();
    let mut chars = line[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(value.split_whitespace().collect::<Vec<_>>().join(" ")),
            '\\' => match chars.next()? {
                'n' | 't' => value.push(' '),
                escaped => value.push(escaped),
            },
            c => value.push(c),
        }
    }
    None
}

/// The events the plugin passes on, by the name it gives each: `busy` as a
/// session goes busy (1.x) or starts a run (2.x), with the pid of the
/// OpenCode process, and three of OpenCode's own, `session.error` only from
/// 1.x.
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

    /// What the plugin passes on of OpenCode 1.x's events, which 1.x hands
    /// to `server`'s `event`.
    fn passed(events: Value) -> (Vec<(i32, Value)>, i32) {
        let (payloads, pid) = run(&format!(
            "const hooks = await plugin.server({{ directory: '/Users/me/Code/app' }});\n\
             for (const event of {events}) await hooks.event({{ event }});\n\
             console.log(process.pid);"
        ));
        (payloads, serde_json::from_value(pid).unwrap())
    }

    /// What the plugin passes on of OpenCode 2.x's events, which 2.x
    /// streams to `setup` from `event.subscribe`, and the types it asks for.
    fn streamed(events: Value) -> (Vec<(i32, Value)>, Value) {
        run(&format!(
            "let types;\n\
             const subscribe = (options) => {{\n\
               types = options.types;\n\
               return (async function* () {{ for (const event of {events}) yield event; }})();\n\
             }};\n\
             await plugin.setup({{ location: {{ directory: '/Users/me/Code/app' }}, event: {{ subscribe }} }});\n\
             await new Promise((done) => setTimeout(done, 0));\n\
             console.log(JSON.stringify(types));"
        ))
    }

    /// Runs `script` in Node with the plugin imported as `plugin`, and a
    /// stand-in for the binary that writes down each payload with the pid of
    /// the process that ran it. Says those, and the JSON the script printed.
    fn run(script: &str) -> (Vec<(i32, Value)>, Value) {
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
            "const {{ default: plugin }} = await import({plugin});\n{script}",
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
        let printed = serde_json::from_slice(&node.stdout).unwrap();
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
        (payloads, printed)
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

    /// The errors OpenCode 1.19.5 publishes and goes on from, among those
    /// that end a run.
    #[test]
    fn the_plugin_passes_on_only_an_error_that_ends_the_run() {
        let session = "ses_e23ee5772ffeEVRcj47R02tvbh";
        let error = |name: &str, message: &str| json!({ "type": "session.error", "properties": { "sessionID": session, "error": { "name": name, "data": { "message": message } } } });
        let (payloads, pid) = passed(json!([
            // A file the prompt names cannot be read; the run goes on.
            error("UnknownError", "ENOENT: no such file or directory"),
            status(session, "busy"),
            // The context overflows, and the session is compacted.
            error("ContextOverflowError", "prompt is too long"),
            status(session, "busy"),
            error("UnknownError", "Model not found: opencode/no-such-model."),
            status(session, "idle"),
            // The prompt's request fails after the run has ended.
            error("UnknownError", "Model not found: opencode/no-such-model."),
        ]));
        let payloads: Vec<Value> = payloads.into_iter().map(|(_, payload)| payload).collect();
        assert_eq!(
            payloads,
            [
                json!({ "event": "busy", "session": session, "pid": pid, "directory": "/Users/me/Code/app" }),
                json!({ "event": "session.error", "session": session, "error": "UnknownError" }),
            ]
        );
    }

    /// OpenCode 2.3.0's events in a run, as the probe's plugin saw them in
    /// gaps oc8: the run's start, a permission asked and replied, the end
    /// that the database writes, and a subagent's permission.
    #[test]
    fn the_plugin_passes_on_a_2x_run_s_start_with_its_own_pid_and_its_permissions() {
        let session = "ses_e202ce171ffeQ3yI7wCxTO328I";
        let child = "ses_e202904f4ffegKoZPcGV7jVtmD";
        let event = |kind: &str, data: Value| json!({ "id": "evt_1dfd31eb9001", "created": 1794756517563u64, "type": kind, "data": data });
        let (payloads, types) = streamed(json!([
            event("session.execution.started", json!({ "sessionID": session })),
            event(
                "session.tool.called",
                json!({ "sessionID": session, "assistantMessageID": "msg_1dfd31eb90019d6FKj8BuWHElr", "id": "call_01a1dfd3221e73ddbbc3c91b", "input": { "command": "sleep 2 && echo hi" }, "executed": false })
            ),
            event(
                "permission.asked",
                json!({ "id": "per_1dfd3228e001weupwRT1frii2n", "sessionID": session, "action": "shell", "resources": ["sleep 2", "echo hi"], "save": ["sleep *", "echo *"], "source": { "type": "tool", "messageID": "msg_1dfd31eb90019d6FKj8BuWHElr", "id": "call_01a1dfd3221e73ddbbc3c91b" } })
            ),
            event(
                "permission.replied",
                json!({ "sessionID": session, "requestID": "per_1dfd3228e001weupwRT1frii2n", "reply": "once" })
            ),
            event(
                "session.execution.succeeded",
                json!({ "sessionID": session })
            ),
            event("session.execution.started", json!({ "sessionID": child })),
            event(
                "permission.asked",
                json!({ "id": "per_1dfd7541c0015QcrFxQOtvhQPa", "sessionID": child, "action": "shell", "resources": ["ls -la"] })
            ),
            event("models-dev.refreshed", json!({})),
        ]));
        let pid = payloads[0].0;
        assert!(payloads.iter().all(|(parent, _)| *parent == pid));
        let payloads: Vec<Value> = payloads.into_iter().map(|(_, payload)| payload).collect();
        assert_eq!(
            payloads,
            [
                json!({ "event": "busy", "session": session, "pid": pid, "directory": "/Users/me/Code/app" }),
                json!({ "event": "permission.asked", "session": session }),
                json!({ "event": "permission.replied", "session": session }),
                json!({ "event": "busy", "session": child, "pid": pid, "directory": "/Users/me/Code/app" }),
                json!({ "event": "permission.asked", "session": child }),
            ]
        );
        assert_eq!(
            types,
            json!([
                "session.execution.started",
                "permission.asked",
                "permission.replied"
            ])
        );
    }

    /// OpenCode 2.3.0's log as it loads a plugin and fails to (gaps oc2, a
    /// plugin with no default export), for the plugin at `plugin`.
    fn logged(plugin: &Path) -> (String, String) {
        let plugin = plugin.display();
        let loading = format!(
            "timestamp=2026-11-15T14:25:51.600Z level=INFO run=3a0bf765 msg=\"loading plugin\" id={plugin} entrypoint=file://{plugin} http.span=23 role=server"
        );
        let failed = format!(
            "timestamp=2026-11-15T14:25:51.602Z level=WARN run=3a0bf765 message=\"failed to load plugin\" target={plugin} ref=err_cfe571ca cause=\"Cause([Fail(PluginModule.LoadError: Plugin must export a default definition with an id and an effect or setup function. (cause: SchemaError(Missing key\\n  at [\\\"default\\\"])))])\" http.span=25 role=server"
        );
        (loading, failed)
    }

    #[test]
    fn doctor_says_when_opencode_could_not_load_the_plugin_since_it_was_written() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let exe = Path::new("/usr/local/bin/agent-leds");
        let plugin = home.join(PLUGIN.path);
        let log = home.join(LOG);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        PLUGIN.install(home, exe).unwrap();
        let at = |time: &str| {
            let second = liveness::iso_second(time).unwrap() as u64;
            let file = fs::File::options().write(true).open(&plugin).unwrap();
            file.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(second))
                .unwrap();
        };
        at("2026-11-15T14:00:00Z");
        // No log: no 2.x has run.
        check(home, exe).unwrap();
        let (loading, failed) = logged(&plugin);
        let (other, _) = logged(&home.join(".config/opencode/plugins/other.js"));
        fs::write(&log, format!("{loading}\n{failed}\n{other}\n")).unwrap();
        let error = format!("{:#}", check(home, exe).unwrap_err());
        assert_eq!(
            error,
            "OpenCode could not load ~/.config/opencode/plugins/agent-leds.js at 2026-11-15T14:25:51.602Z: \
             Cause([Fail(PluginModule.LoadError: Plugin must export a default definition with an id and an effect or setup function. \
             (cause: SchemaError(Missing key at [\"default\"])))])"
        );
        // Loaded again since.
        fs::write(&log, format!("{failed}\n{loading}\n")).unwrap();
        check(home, exe).unwrap();
        // A failure before the plugin was written again is not this one's.
        fs::write(&log, format!("{loading}\n{failed}\n")).unwrap();
        at("2026-11-15T15:00:00Z");
        check(home, exe).unwrap();
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
