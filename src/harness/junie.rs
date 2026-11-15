//! Junie: its hooks live in `~/.junie/config.json`, in the shape Claude
//! Code's have.

use std::borrow::Cow;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    Event, Harness, Payload, commands, edit_hooks, hooked, owns_group, read_hooks, register_groups,
    unregister,
};
use crate::coverage::{Declared, Evidence};
use crate::files;
use crate::fsm::Signal;

pub const CONFIG: &str = ".junie/config.json";

/// The hook events the daemon needs, in the order a turn produces them.
/// Two are left out because Junie reads a hook that succeeds as an answer:
/// on PermissionRequest it approves the request, and on PreToolUse its
/// documentation counts a hook that says nothing as allowing the tool.
pub const HOOKS: [&str; 5] = [
    "UserPromptSubmit",
    "PostToolUse",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// What Junie's hooks tell, until its own files are read.
pub const SIGNALS: &[Declared] = &[
    hooked(
        Signal::Started,
        Evidence::Inferred,
        "any hook of the session the last prompt named, run under the process named junie",
        "0.5.0 hooks; Junie was not installed for the probe",
    ),
    hooked(
        Signal::Prompted,
        Evidence::Inferred,
        "UserPromptSubmit or PostToolUse",
        "0.5.0 hooks; Junie was not installed for the probe",
    ),
    hooked(
        Signal::TurnEnded,
        Evidence::Inferred,
        "Stop",
        "0.5.0 hooks; Junie was not installed for the probe",
    ),
    hooked(
        Signal::TurnEndedAsking,
        Evidence::Inferred,
        "Stop whose last message asks",
        "0.5.0 hooks; Junie was not installed for the probe",
    ),
    hooked(
        Signal::TurnFailed,
        Evidence::Inferred,
        "StopFailure",
        "0.5.0 hooks; Junie was not installed for the probe",
    ),
    hooked(
        Signal::Ended,
        Evidence::Inferred,
        "SessionEnd, or the junie process gone",
        "0.5.0 hooks; Junie was not installed for the probe",
    ),
];

pub fn event(payload: &Value) -> Option<Event> {
    Event::read(&Payload::read(payload)?)
}

/// The session a prompt named, as the hook keeps it between events.
#[derive(Serialize, Deserialize)]
struct Session {
    session_id: String,
    #[serde(default)]
    cwd: String,
}

/// Junie names the session only when a prompt is submitted. The hook keeps
/// that in `state`, and takes the events that name no session to belong to
/// the session that submitted a prompt last: this is `payload` with that
/// session in it.
pub fn with_session<'a>(payload: &'a Value, state: &Path) -> Option<Cow<'a, Value>> {
    let last = state.join("junie-session.json");
    if let Ok(session) = Session::deserialize(payload) {
        // Written whole, since the next event's hook may be reading it already.
        let _ = files::replace(&last, &serde_json::to_string(&session).ok()?);
        return Some(Cow::Borrowed(payload));
    }
    let session: Session = serde_json::from_str(&files::read_optional(&last).ok()??).ok()?;
    let mut payload = payload.clone();
    let fields = payload.as_object_mut()?;
    fields.insert("session_id".into(), session.session_id.into());
    fields.insert("cwd".into(), session.cwd.into());
    Some(Cow::Owned(payload))
}

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let command = Harness::Junie.command(exe);
    edit_hooks(&home.join(CONFIG), |hooks| {
        register_groups(
            hooks,
            Harness::Junie,
            &HOOKS,
            |_| json!({ "type": "command", "command": command, "timeout": 5 }),
        )
    })
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    Ok(commands(&read_hooks(&home.join(CONFIG))?, true))
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(CONFIG), |hooks| {
        unregister(hooks, |group| owns_group(Harness::Junie, group));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::Home;
    use super::*;
    use crate::fsm::Signal;

    #[test]
    fn every_event_registered_is_one_the_daemon_follows() {
        for event in HOOKS {
            assert!(super::super::Hook::named(event).is_some(), "{event}");
        }
    }

    /// The session and what the event means for it, as the daemon reads it.
    fn update(payload: Value, state: &Path) -> Option<(String, Vec<Signal>)> {
        let (session, _, signals) = super::super::interim(Harness::Junie, &payload, state)?;
        Some((session, signals.into_iter().collect()))
    }

    #[test]
    fn events_that_name_no_session_belong_to_the_one_that_prompted_last() {
        let state = Home::new();
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
            of("j1", vec![Signal::Prompted])
        );
        let tool = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": { "command": "pytest" }, "tool_response": "ok", "duration_ms": 12
        });
        assert_eq!(update(tool, &state), of("j1", vec![Signal::Prompted]));
        assert_eq!(
            update(stop.clone(), &state),
            of("j1", vec![Signal::TurnEnded])
        );
        let asks =
            json!({ "hook_event_name": "Stop", "last_assistant_message": "Shall I push it?" });
        assert_eq!(
            update(asks, &state),
            of("j1", vec![Signal::TurnEnded, Signal::TurnEndedAsking])
        );
        let failed = json!({ "hook_event_name": "StopFailure", "error": "rate_limit", "error_details": "429 Too Many Requests" });
        assert_eq!(update(failed, &state), of("j1", vec![Signal::TurnFailed]));

        update(prompt("j2"), &state);
        let ended = json!({ "hook_event_name": "SessionEnd", "reason": "prompt_input_exit" });
        assert_eq!(update(ended, &state), of("j2", vec![Signal::Ended]));
    }

    #[test]
    fn a_session_is_labelled_by_the_folder_its_prompt_named() {
        let state = Home::new();
        let prompt = json!({ "hook_event_name": "UserPromptSubmit", "session_id": "j1", "cwd": "/Users/me/Code/app" });
        with_session(&prompt, &state);
        let stop = json!({ "hook_event_name": "Stop" });
        let stop = event(&with_session(&stop, &state).unwrap()).unwrap();
        assert_eq!(stop.cwd, "/Users/me/Code/app");
    }

    #[test]
    fn installing_twice_changes_nothing_and_uninstalling_leaves_the_rest() {
        let theirs = json!({
            "model": "sonnet",
            "hooks": { "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "python guard.py" }] }] }
        });
        let home = Home::new();
        home.write(CONFIG, &theirs.to_string());

        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(CONFIG);
        assert_eq!(installed["model"], "sonnet");
        assert_eq!(
            installed["hooks"]["PreToolUse"],
            theirs["hooks"]["PreToolUse"]
        );
        assert_eq!(
            installed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/bin/agent-leds hook junie"
        );
        // A hook on either of these would decide for the user.
        assert!(installed["hooks"]["PermissionRequest"].is_null());
        assert_eq!(
            installed["hooks"]["PreToolUse"].as_array().unwrap().len(),
            1
        );
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert_eq!(home.json(CONFIG), installed);

        uninstall(&home).unwrap();
        assert_eq!(home.json(CONFIG), theirs);
    }
}
