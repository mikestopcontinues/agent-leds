//! Claude Code: its hooks live in `~/.claude/settings.json`.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::{Event, Harness, edit_hooks, register, text, unregister};

/// The hook events the daemon needs, in the order a turn produces them.
const EVENTS: [&str; 10] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "Elicitation",
    "ElicitationResult",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

pub fn event(payload: &Value) -> Option<Event> {
    let mut event = Event::read(payload)?;
    event.asks |= text(payload, "tool_name") == Some("AskUserQuestion");
    event.unfinished = event.name == "Stop" && has_work_running(payload);
    // An aborted tool means the user interrupted the turn.
    if event.name == "PostToolUseFailure" && payload.get("is_interrupt") == Some(&Value::Bool(true))
    {
        event.name = "Interrupt".into();
    }
    Some(event)
}

/// Whether a turn ended with a subagent or a workflow still running. Claude
/// Code starts a new turn when one finishes. It lists monitors and shell
/// commands as background tasks too, but those can run for as long as the
/// session does, and a session held as working for one would never show
/// that it had stopped.
fn has_work_running(payload: &Value) -> bool {
    let tasks = payload.get("background_tasks").and_then(Value::as_array);
    tasks.into_iter().flatten().any(|task| {
        matches!(text(task, "type"), Some("subagent" | "workflow"))
            && text(task, "status") == Some("running")
    })
}

const SETTINGS: &str = ".claude/settings.json";

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    edit_hooks(&home.join(SETTINGS), |hooks| {
        for event in EVENTS {
            let mut handler =
                json!({ "type": "command", "command": Harness::Claude.command(exe), "timeout": 5 });
            if event == "PermissionRequest" {
                // An async hook cannot answer the permission prompt, so observing never approves anything.
                handler["async"] = json!(true);
            }
            register(hooks, Harness::Claude, event, handler)?;
        }
        Ok(())
    })
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(SETTINGS), |hooks| {
        unregister(hooks, Harness::Claude);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Home, update};
    use super::*;
    use crate::state::{State, Update};

    fn event(name: &str, extra: Value) -> Option<Update> {
        let mut event =
            json!({ "session_id": "s1", "cwd": "/Users/me/Code/app", "hook_event_name": name });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        update(Harness::Claude, event)
    }

    #[test]
    fn maps_a_turn() {
        let none = json!({});
        assert_eq!(
            event("UserPromptSubmit", none.clone()),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            event("PreToolUse", json!({ "tool_name": "Bash" })),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            event("PreToolUse", json!({ "tool_name": "AskUserQuestion" })),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(
            event("PermissionRequest", none.clone()),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(
            event("PostToolUse", none.clone()),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            event("StopFailure", json!({ "error": "rate_limit" })),
            Some(Update::Enter(State::Error))
        );
        assert_eq!(event("SessionEnd", none.clone()), Some(Update::Leave));
        assert_eq!(event("SessionStart", none), None);
    }

    #[test]
    fn stop_is_a_question_only_when_the_last_line_asks() {
        let stop = |message: &str| event("Stop", json!({ "last_assistant_message": message }));
        assert_eq!(
            stop("Done. All tests pass."),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(
            stop("I found two options.\n\n**Which should I use?**\n"),
            Some(Update::Asked)
        );
        assert_eq!(
            event("Stop", json!({})),
            Some(Update::Enter(State::Stopped))
        );
    }

    #[test]
    fn a_turn_that_leaves_work_running_has_not_stopped() {
        let stop = |message: &str, tasks: Value| {
            event(
                "Stop",
                json!({ "last_assistant_message": message, "background_tasks": tasks }),
            )
        };
        let task = |kind: &str, status: &str| json!({ "id": "t1", "type": kind, "status": status });
        let working = Some(Update::Enter(State::Working));
        let stopped = Some(Update::Enter(State::Stopped));
        assert_eq!(
            stop(
                "The review is running.",
                json!([task("subagent", "running")])
            ),
            working
        );
        assert_eq!(
            stop(
                "Started.",
                json!([task("monitor", "running"), task("workflow", "running")])
            ),
            working
        );
        // A question is put to the user whatever else is going on.
        assert_eq!(
            stop("Which one?", json!([task("workflow", "running")])),
            Some(Update::Asked)
        );
        // A monitor or a shell command may never finish.
        assert_eq!(
            stop(
                "Done.",
                json!([task("monitor", "running"), task("shell", "running")])
            ),
            stopped
        );
        assert_eq!(
            stop("Done.", json!([task("subagent", "completed")])),
            stopped
        );
        assert_eq!(stop("Done.", json!([])), stopped);
    }

    #[test]
    fn interrupts_and_subagents() {
        assert_eq!(
            event("PostToolUseFailure", json!({ "is_interrupt": true })),
            Some(Update::Leave)
        );
        assert_eq!(
            event("PostToolUseFailure", json!({ "error": "exit 1" })),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            event(
                "PreToolUse",
                json!({ "agent_id": "a1", "tool_name": "AskUserQuestion" })
            ),
            Some(Update::Background)
        );
    }

    #[test]
    fn installing_twice_changes_nothing_and_uninstalling_leaves_the_rest() {
        let theirs = json!({
            "model": "opus",
            "hooks": { "Stop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "python hook_entry.py --provider claude" }] }] }
        });
        let home = Home::new("claude");
        home.write(SETTINGS, &theirs.to_string());

        install(&home, Path::new("/bin/leds")).unwrap();
        let installed = home.json(SETTINGS);
        assert_eq!(installed["model"], theirs["model"]);
        assert_eq!(installed["hooks"]["Stop"][0], theirs["hooks"]["Stop"][0]);
        assert_eq!(
            installed["hooks"]["Stop"][1]["hooks"][0]["command"],
            "/bin/leds hook claude"
        );
        assert_eq!(
            installed["hooks"]["PermissionRequest"][0]["hooks"][0]["async"],
            true
        );
        install(&home, Path::new("/bin/leds")).unwrap();
        assert_eq!(home.json(SETTINGS), installed);

        uninstall(&home).unwrap();
        assert_eq!(home.json(SETTINGS), theirs);
    }
}
