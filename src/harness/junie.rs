//! Junie: its hooks live in `~/.junie/config.json`, in the shape Claude
//! Code's have.

use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::{Event, Harness, edit_hooks, register, unregister};

const CONFIG: &str = ".junie/config.json";

/// The hook events the daemon needs, in the order a turn produces them.
/// Two are left out because Junie reads a hook that succeeds as an answer:
/// on PermissionRequest it approves the request, and on PreToolUse its
/// documentation counts a hook that says nothing as allowing the tool.
const EVENTS: [&str; 5] = [
    "UserPromptSubmit",
    "PostToolUse",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// Junie names the session only when a prompt is submitted. The hook keeps
/// that in `state`, and takes the events that name no session to belong to
/// the session that submitted a prompt last.
pub fn event(payload: &Value, state: &Path) -> Option<Event> {
    let last = state.join("junie-session.json");
    if payload.get("session_id").is_some() {
        let session = json!({ "session_id": payload["session_id"], "cwd": payload["cwd"] });
        let _ = fs::write(last, session.to_string());
        return Event::read(payload);
    }
    let session: Value = serde_json::from_slice(&fs::read(last).ok()?).ok()?;
    let mut payload = payload.clone();
    payload
        .as_object_mut()?
        .extend(session.as_object()?.clone());
    Event::read(&payload)
}

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    edit_hooks(&home.join(CONFIG), |hooks| {
        for event in EVENTS {
            let handler =
                json!({ "type": "command", "command": Harness::Junie.command(exe), "timeout": 5 });
            register(hooks, Harness::Junie, event, handler)?;
        }
        Ok(())
    })
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(CONFIG), |hooks| {
        unregister(hooks, Harness::Junie);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::Home;
    use super::*;
    use crate::state::{State, Update};

    /// The session and what the event means for it.
    fn update(payload: Value, state: &Path) -> Option<(String, Update)> {
        let event = event(&payload, state)?;
        Some((event.session.clone(), event.update()?))
    }

    #[test]
    fn events_that_name_no_session_belong_to_the_one_that_prompted_last() {
        let state = Home::new("junie-session");
        let of = |session: &str, update| Some((session.to_owned(), update));
        // Nothing can be said about a turn before any prompt was seen.
        let stop = json!({ "hook_event_name": "Stop", "stop_hook_active": false, "last_assistant_message": "Done." });
        assert_eq!(update(stop.clone(), &state), None);

        let prompt = |session: &str| {
            json!({
                "hook_event_name": "UserPromptSubmit", "session_id": session,
                "cwd": "/Users/me/Code/app", "project_path": "/Users/me/Code/app", "prompt": "fix it"
            })
        };
        assert_eq!(
            update(prompt("j1"), &state),
            of("j1", Update::Enter(State::Working))
        );
        let tool = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": { "command": "pytest" }, "tool_response": "ok", "duration_ms": 12
        });
        assert_eq!(
            update(tool, &state),
            of("j1", Update::Enter(State::Working))
        );
        assert_eq!(
            update(stop.clone(), &state),
            of("j1", Update::Enter(State::Stopped))
        );
        let asks =
            json!({ "hook_event_name": "Stop", "last_assistant_message": "Shall I push it?" });
        assert_eq!(update(asks, &state), of("j1", Update::Asked));
        let failed = json!({ "hook_event_name": "StopFailure", "error": "rate_limit", "error_details": "429 Too Many Requests" });
        assert_eq!(
            update(failed, &state),
            of("j1", Update::Enter(State::Error))
        );

        update(prompt("j2"), &state);
        let ended = json!({ "hook_event_name": "SessionEnd", "reason": "prompt_input_exit" });
        assert_eq!(update(ended, &state), of("j2", Update::Leave));
    }

    #[test]
    fn a_session_is_labelled_by_the_folder_its_prompt_named() {
        let state = Home::new("junie-label");
        let prompt = json!({ "hook_event_name": "UserPromptSubmit", "session_id": "j1", "cwd": "/Users/me/Code/app" });
        event(&prompt, &state);
        let stop = event(&json!({ "hook_event_name": "Stop" }), &state).unwrap();
        assert_eq!(stop.cwd, "/Users/me/Code/app");
    }

    #[test]
    fn installing_twice_changes_nothing_and_uninstalling_leaves_the_rest() {
        let theirs = json!({
            "model": "sonnet",
            "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "python guard.py" }] }] }
        });
        let home = Home::new("junie");
        home.write(CONFIG, &theirs.to_string());

        install(&home, Path::new("/bin/leds")).unwrap();
        let installed = home.json(CONFIG);
        assert_eq!(installed["model"], "sonnet");
        assert_eq!(
            installed["hooks"]["PreToolUse"],
            theirs["hooks"]["PreToolUse"]
        );
        assert_eq!(
            installed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/bin/leds hook junie"
        );
        // A hook on either of these would decide for the user.
        assert!(installed["hooks"]["PermissionRequest"].is_null());
        assert_eq!(
            installed["hooks"]["PreToolUse"].as_array().unwrap().len(),
            1
        );
        install(&home, Path::new("/bin/leds")).unwrap();
        assert_eq!(home.json(CONFIG), installed);

        uninstall(&home).unwrap();
        assert_eq!(home.json(CONFIG), theirs);
    }
}
