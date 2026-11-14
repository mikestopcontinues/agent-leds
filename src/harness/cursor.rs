//! Cursor: its hooks live in `~/.cursor/hooks.json`, under event names of
//! its own.

use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Event, Harness, Hook, commands, edit_hooks, read_hooks, register, unregister};
use crate::files;

pub const HOOKS: &str = ".cursor/hooks.json";

/// The hook events the daemon needs, in the order a turn produces them.
/// Cursor expects the hooks that run before a tool to say whether it may
/// run, and this hook must not answer that, so none of those is here.
pub const EVENTS: [&str; 5] = [
    "beforeSubmitPrompt",
    "postToolUse",
    "postToolUseFailure",
    "stop",
    "sessionEnd",
];

/// A hook payload as Cursor spells it.
#[derive(Deserialize)]
struct Payload {
    hook_event_name: String,
    /// How a turn ended.
    status: Option<String>,
    /// Whether a failed tool was cut short by the user.
    #[serde(default)]
    is_interrupt: bool,
    /// Cursor calls the session a conversation everywhere but at its end.
    conversation_id: Option<String>,
    session_id: Option<String>,
    #[serde(default)]
    workspace_roots: Vec<String>,
}

pub fn event(payload: &Value) -> Option<Event> {
    let payload = Payload::deserialize(payload).ok()?;
    let hook = match payload.hook_event_name.as_str() {
        "beforeSubmitPrompt" => Hook::UserPromptSubmit,
        "postToolUse" => Hook::PostToolUse,
        // An aborted tool means the user interrupted the turn.
        "postToolUseFailure" if payload.is_interrupt => Hook::Interrupt,
        "postToolUseFailure" => Hook::PostToolUseFailure,
        // Cursor has one event for the end of a turn and says how it ended.
        "stop" => match payload.status.as_deref()? {
            "completed" => Hook::Stop,
            "aborted" => Hook::Interrupt,
            "error" => Hook::StopFailure,
            _ => return None,
        },
        "sessionEnd" => Hook::SessionEnd,
        _ => return None,
    };
    Some(Event {
        hook,
        session: payload.conversation_id.or(payload.session_id)?,
        cwd: payload
            .workspace_roots
            .into_iter()
            .next()
            .unwrap_or_default(),
        // Cursor does not pass the agent's last message to a hook.
        asks: false,
        subagent: false,
        unfinished: false,
    })
}

fn is_ours(entry: &Value) -> bool {
    entry["command"]
        .as_str()
        .is_some_and(|command| Harness::Cursor.owns(command))
}

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let file = home.join(HOOKS);
    if !file.exists() {
        files::replace(&file, "{ \"version\": 1 }\n")?;
    }
    let entry = json!({ "command": Harness::Cursor.command(exe), "timeout": 5 });
    edit_hooks(&file, |hooks| {
        for event in EVENTS {
            register(hooks, event, entry.clone(), is_ours)?;
        }
        Ok(())
    })
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    Ok(commands(&read_hooks(&home.join(HOOKS))?, false))
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(HOOKS), |hooks| {
        unregister(hooks, is_ours);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::tests::{Home, update};
    use super::*;
    use crate::state::{State, Update};

    /// A payload with the fields Cursor sends on every event of a turn.
    fn event(name: &str, extra: Value) -> Option<Update> {
        let mut event = json!({
            "conversation_id": "5b1f3c1e-7d0a-4a55-9a34-0c7f5f1f2b10",
            "session_id": "5b1f3c1e-7d0a-4a55-9a34-0c7f5f1f2b10",
            "generation_id": "0f6c1c2b-3a44-4e21-8d2b-5d2f4c0e9a11",
            "model": "default",
            "hook_event_name": name,
            "cursor_version": "2026.05.28-a70ca7c",
            "workspace_roots": ["/Users/me/Code/app"],
            "user_email": null,
            "transcript_path": null,
        });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        update(Harness::Cursor, event)
    }

    #[test]
    fn maps_a_turn() {
        let working = Some(Update::Enter(State::Working));
        assert_eq!(
            event("beforeSubmitPrompt", json!({ "prompt": "fix it" })),
            working
        );
        assert_eq!(
            event("postToolUse", json!({ "tool_name": "Shell" })),
            working
        );
        assert_eq!(
            event(
                "postToolUseFailure",
                json!({ "tool_name": "Shell", "failure_type": "error", "is_interrupt": false })
            ),
            working
        );
        assert_eq!(
            event(
                "postToolUseFailure",
                json!({ "tool_name": "Shell", "failure_type": "error", "is_interrupt": true })
            ),
            Some(Update::Leave)
        );
        assert_eq!(
            event("sessionEnd", json!({ "reason": "user_close" })),
            Some(Update::Leave)
        );
        assert_eq!(event("preToolUse", json!({ "tool_name": "Shell" })), None);
    }

    #[test]
    fn a_stop_says_how_the_turn_ended() {
        let stop = |status: &str| event("stop", json!({ "status": status, "loop_count": 0 }));
        assert_eq!(stop("completed"), Some(Update::Enter(State::Stopped)));
        assert_eq!(stop("aborted"), Some(Update::Leave));
        assert_eq!(stop("error"), Some(Update::Enter(State::Error)));
    }

    #[test]
    fn a_payload_cursor_sends_to_the_hooks_of_claude_code_is_not_claude_codes() {
        let stop = json!({
            "conversation_id": "c1", "session_id": "c1", "hook_event_name": "stop",
            "status": "completed", "workspace_roots": ["/Users/me/Code/app"]
        });
        assert_eq!(update(Harness::Claude, stop.clone()), None);
        assert_eq!(
            update(Harness::Cursor, stop),
            Some(Update::Enter(State::Stopped))
        );
    }

    #[test]
    fn installing_twice_changes_nothing_and_uninstalling_leaves_the_rest() {
        let theirs = json!({
            "version": 1,
            "hooks": {
                "stop": [{ "command": "python hook_entry.py --provider cursor --event stop ; true" }],
                "preToolUse": [{ "command": "python guard.py" }]
            }
        });
        let home = Home::new();
        home.write(HOOKS, &theirs.to_string());

        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(HOOKS);
        assert_eq!(installed["version"], 1);
        assert_eq!(
            installed["hooks"]["preToolUse"],
            theirs["hooks"]["preToolUse"]
        );
        assert_eq!(installed["hooks"]["stop"][0], theirs["hooks"]["stop"][0]);
        assert_eq!(
            installed["hooks"]["stop"][1],
            json!({ "command": "/bin/agent-leds hook cursor", "timeout": 5 })
        );
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert_eq!(home.json(HOOKS), installed);

        uninstall(&home).unwrap();
        assert_eq!(home.json(HOOKS), theirs);
    }

    #[test]
    fn a_first_hooks_file_says_which_version_it_is() {
        let home = Home::new();
        fs::create_dir(home.join(".cursor")).unwrap();
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(HOOKS);
        assert_eq!(installed["version"], 1);
        assert_eq!(installed["hooks"].as_object().unwrap().len(), EVENTS.len());
    }
}
