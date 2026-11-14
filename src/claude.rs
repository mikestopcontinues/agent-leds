//! Claude Code: what its hook events mean, and registering the hook.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

use crate::config::home;
use crate::daemon::Request;
use crate::state::{State, Update};

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

/// Translates one hook payload into a report for the daemon.
pub fn report(event: &Value) -> Option<Request> {
    let text = |key: &str| event.get(key).and_then(Value::as_str);
    let update = if event.get("agent_id").is_some() {
        // Only the main agent's events say what the session needs. A
        // subagent's say that work is going on, which matters once the main
        // agent has stopped and left a subagent running in the background.
        Update::Background
    } else {
        match text("hook_event_name")? {
            "UserPromptSubmit" | "PostToolUse" | "ElicitationResult" => {
                Update::Enter(State::Working)
            }
            "PreToolUse" if text("tool_name") == Some("AskUserQuestion") => {
                Update::Enter(State::Question)
            }
            "PreToolUse" => Update::Enter(State::Working),
            // An aborted tool means the user interrupted the turn.
            "PostToolUseFailure" if event.get("is_interrupt") == Some(&Value::Bool(true)) => {
                Update::Leave
            }
            "PostToolUseFailure" => Update::Enter(State::Working),
            "PermissionRequest" | "Elicitation" => Update::Enter(State::Question),
            "Stop" if asks(text("last_assistant_message").unwrap_or_default()) => Update::Asked,
            "Stop" => Update::Enter(State::Stopped),
            "StopFailure" => Update::Enter(State::Error),
            "SessionEnd" => Update::Leave,
            _ => return None,
        }
    };
    let cwd = text("cwd").unwrap_or_default();
    Some(Request::Report {
        session: text("session_id")?.to_owned(),
        label: Path::new(cwd)
            .file_name()
            .map_or(cwd.into(), |name| name.to_string_lossy())
            .into_owned(),
        update,
    })
}

/// Whether a final message ends by asking the user something.
fn asks(message: &str) -> bool {
    let last = message
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    last.trim_end_matches(|c: char| c.is_whitespace() || "*_`\"')".contains(c))
        .ends_with('?')
}

fn settings_file() -> PathBuf {
    home().join(".claude/settings.json")
}

fn is_ours(group: &Value) -> bool {
    group["hooks"].as_array().is_some_and(|handlers| {
        handlers.iter().any(|handler| {
            handler["command"]
                .as_str()
                .is_some_and(|command| command.ends_with("leds hook claude"))
        })
    })
}

/// Registers `exe hook claude` for every event in EVENTS, replacing any earlier registration.
pub fn install(exe: &Path) -> Result<()> {
    edit_settings(|hooks| {
        for event in EVENTS {
            let mut handler = json!({ "type": "command", "command": format!("{} hook claude", exe.display()), "timeout": 5 });
            if event == "PermissionRequest" {
                // An async hook cannot answer the permission prompt, so observing never approves anything.
                handler["async"] = json!(true);
            }
            let groups = hooks.entry(event).or_insert_with(|| json!([]));
            let Some(groups) = groups.as_array_mut() else {
                bail!("hooks.{event} is not a list")
            };
            groups.retain(|group| !is_ours(group));
            groups.push(json!({ "hooks": [handler] }));
        }
        Ok(())
    })
}

pub fn uninstall() -> Result<()> {
    edit_settings(|hooks| {
        for groups in hooks.values_mut() {
            if let Some(groups) = groups.as_array_mut() {
                groups.retain(|group| !is_ours(group));
            }
        }
        hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
        Ok(())
    })
}

fn edit_settings(edit: impl FnOnce(&mut Map<String, Value>) -> Result<()>) -> Result<()> {
    let path = settings_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut settings: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let Some(root) = settings.as_object_mut() else {
        bail!("{} is not a JSON object", path.display())
    };
    let Some(hooks) = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        bail!("hooks in {} is not an object", path.display())
    };
    edit(hooks)?;
    if hooks.is_empty() {
        root.remove("hooks");
    }
    // Write beside the file and rename, so Claude Code never reads half a file.
    let staged = path.with_extension("json.leds-tmp");
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&staged, serde_json::to_string_pretty(&settings)? + "\n")?;
    fs::rename(&staged, &path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(event: Value) -> Option<Update> {
        match report(&event)? {
            Request::Report { update, .. } => Some(update),
            _ => None,
        }
    }

    fn event(name: &str, extra: Value) -> Value {
        let mut event =
            json!({ "session_id": "s1", "cwd": "/Users/me/Code/app", "hook_event_name": name });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        event
    }

    #[test]
    fn maps_a_turn() {
        let none = json!({});
        assert_eq!(
            update(event("UserPromptSubmit", none.clone())),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            update(event("PreToolUse", json!({ "tool_name": "Bash" }))),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            update(event(
                "PreToolUse",
                json!({ "tool_name": "AskUserQuestion" })
            )),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(
            update(event("PermissionRequest", none.clone())),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(
            update(event("PostToolUse", none.clone())),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            update(event("StopFailure", json!({ "error": "rate_limit" }))),
            Some(Update::Enter(State::Error))
        );
        assert_eq!(
            update(event("SessionEnd", none.clone())),
            Some(Update::Leave)
        );
        assert_eq!(update(event("SessionStart", none)), None);
    }

    #[test]
    fn stop_is_a_question_only_when_the_last_line_asks() {
        let stop =
            |message: &str| update(event("Stop", json!({ "last_assistant_message": message })));
        assert_eq!(
            stop("Done. All tests pass."),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(
            stop("Why did it fail? The cache was stale.\n\nFixed."),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(
            stop("I found two options.\n\n**Which should I use?**\n"),
            Some(Update::Asked)
        );
        assert_eq!(
            update(event("Stop", json!({}))),
            Some(Update::Enter(State::Stopped))
        );
    }

    #[test]
    fn interrupts_and_subagents() {
        assert_eq!(
            update(event("PostToolUseFailure", json!({ "is_interrupt": true }))),
            Some(Update::Leave)
        );
        assert_eq!(
            update(event("PostToolUseFailure", json!({ "error": "exit 1" }))),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            update(event(
                "PreToolUse",
                json!({ "agent_id": "a1", "tool_name": "AskUserQuestion" })
            )),
            Some(Update::Background)
        );
    }

    #[test]
    fn labels_a_session_by_its_folder() {
        let Some(Request::Report { session, label, .. }) = report(&event("Stop", json!({}))) else {
            panic!()
        };
        assert_eq!((session.as_str(), label.as_str()), ("s1", "app"));
    }

    #[test]
    fn recognizes_only_its_own_hook_groups() {
        assert!(is_ours(
            &json!({ "hooks": [{ "type": "command", "command": "/Users/me/.cargo/bin/leds hook claude" }] })
        ));
        assert!(!is_ours(
            &json!({ "matcher": "*", "hooks": [{ "type": "command", "command": "python hook_entry.py --provider claude" }] })
        ));
    }
}
