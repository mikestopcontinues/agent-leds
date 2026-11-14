//! Grok: its hooks live in a file of their own in `~/.grok/hooks/`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::{Event, Harness, ends_on_question, replace, text};

/// Grok reads every JSON file in the folder, so this one holds nothing but
/// what an install wrote.
const HOOKS: &str = ".grok/hooks/agent-leds.json";

/// The hook events the daemon needs, in the order a turn produces them,
/// each with what it must match. Grok names the kind of a notification
/// there.
const EVENTS: [(&str, Option<&str>); 9] = [
    ("UserPromptSubmit", None),
    ("PreToolUse", None),
    ("Notification", Some("permission_prompt")),
    ("PostToolUse", None),
    ("PostToolUseFailure", None),
    ("Stop", None),
    ("StopFailure", None),
    ("StopCancelled", None),
    ("SessionEnd", None),
];

/// Grok spells its payloads in camelCase and its event names in snake_case.
pub fn event(payload: &Value) -> Option<Event> {
    let text = |key| text(payload, key);
    let name = match text("hookEventName")? {
        "user_prompt_submit" => "UserPromptSubmit",
        "pre_tool_use" => "PreToolUse",
        // Grok has no event for a permission request. It sends this
        // notification while its prompt is waiting.
        "notification" if text("notificationType") == Some("permission_prompt") => {
            "PermissionRequest"
        }
        "post_tool_use" => "PostToolUse",
        "post_tool_use_failure" => "PostToolUseFailure",
        // Stop fires again when the session closes, which says nothing about a turn.
        "stop" if text("reason") == Some("end_turn") => "Stop",
        "stop_failure" => "StopFailure",
        // The user was there to cancel the turn. One that Grok gave up on by
        // itself is left for them to find.
        "stop_cancelled" if text("cancelledBy") == Some("user") => "Interrupt",
        "stop_cancelled" => "Stop",
        "session_end" => "SessionEnd",
        _ => return None,
    };
    Some(Event {
        name: name.to_owned(),
        session: text("sessionId")?.to_owned(),
        cwd: text("cwd").unwrap_or_default().to_owned(),
        asks: match name {
            "PreToolUse" => text("toolName") == Some("ask_user_question"),
            _ => ends_on_question(text("lastAssistantMessage").unwrap_or_default()),
        },
        // A subagent runs as a session of its own and says what kind it is.
        subagent: payload.get("subagentType").is_some(),
        unfinished: false,
    })
}

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let mut hooks = Map::new();
    for (event, matcher) in EVENTS {
        let handler =
            json!({ "type": "command", "command": Harness::Grok.command(exe), "timeout": 5 });
        let mut group = json!({ "hooks": [handler] });
        if let Some(matcher) = matcher {
            group["matcher"] = json!(matcher);
        }
        hooks.insert(event.to_owned(), json!([group]));
    }
    let file = home.join(HOOKS);
    fs::create_dir_all(file.parent().expect("the file is in a folder"))?;
    let hooks = json!({ "hooks": hooks });
    replace(&file, &(serde_json::to_string_pretty(&hooks)? + "\n"))
}

pub fn uninstall(home: &Path) -> Result<()> {
    let file = home.join(HOOKS);
    match fs::remove_file(&file) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", file.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Home, update};
    use super::*;
    use crate::state::{State, Update};

    /// A payload with the fields Grok sends on every event.
    fn event(name: &str, extra: Value) -> Option<Update> {
        let mut event = json!({
            "hookEventName": name,
            "sessionId": "019f0a3c-6f0e-7a51-8d2b-5d2f4c0e9a11",
            "cwd": "/Users/me/Code/app",
            "workspaceRoot": "/Users/me/Code/app",
            "permissionMode": "default",
            "promptId": "p1",
            "timestamp": "2026-11-13T22:40:00Z",
        });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        update(Harness::Grok, event)
    }

    #[test]
    fn maps_a_turn() {
        let working = Some(Update::Enter(State::Working));
        assert_eq!(
            event("user_prompt_submit", json!({ "prompt": "fix it" })),
            working
        );
        assert_eq!(
            event(
                "pre_tool_use",
                json!({ "toolName": "run_terminal_command" })
            ),
            working
        );
        assert_eq!(
            event("pre_tool_use", json!({ "toolName": "ask_user_question" })),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(
            event("post_tool_use", json!({ "toolName": "read_file" })),
            working
        );
        assert_eq!(
            event("post_tool_use_failure", json!({ "toolName": "read_file" })),
            working
        );
        assert_eq!(
            event("stop_failure", json!({ "error": "rate_limit" })),
            Some(Update::Enter(State::Error))
        );
        assert_eq!(
            event("session_end", json!({ "reason": "exit" })),
            Some(Update::Leave)
        );
        assert_eq!(event("session_start", json!({ "source": "startup" })), None);
    }

    #[test]
    fn only_the_stop_that_ends_a_turn_counts() {
        let stop = |reason: &str, message: &str| {
            event(
                "stop",
                json!({ "reason": reason, "lastAssistantMessage": message }),
            )
        };
        assert_eq!(
            stop("end_turn", "Done."),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(stop("end_turn", "Shall I push it?"), Some(Update::Asked));
        assert_eq!(stop("channel_closed", "Done."), None);
        assert_eq!(stop("shutdown", "Done."), None);
    }

    #[test]
    fn a_waiting_permission_prompt_is_a_question_and_other_notifications_are_not() {
        let notification = |kind: &str| event("notification", json!({ "notificationType": kind }));
        assert_eq!(
            notification("permission_prompt"),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(notification("idle_prompt"), None);
    }

    #[test]
    fn a_turn_the_user_cancels_leaves_and_one_grok_gives_up_on_stops() {
        let cancelled = |reason: &str, by: &str| {
            event(
                "stop_cancelled",
                json!({ "reason": reason, "cancelledBy": by }),
            )
        };
        assert_eq!(cancelled("user_interrupt", "user"), Some(Update::Leave));
        assert_eq!(
            cancelled("permission_rejected", "user"),
            Some(Update::Leave)
        );
        assert_eq!(
            cancelled("max_turns", "runtime"),
            Some(Update::Enter(State::Stopped))
        );
    }

    #[test]
    fn a_subagent_only_says_that_work_is_going_on() {
        let explore = json!({ "subagentType": "explore", "reason": "end_turn" });
        assert_eq!(event("stop", explore.clone()), Some(Update::Background));
        assert_eq!(event("session_end", explore), Some(Update::Background));
    }

    #[test]
    fn payloads_grok_sends_to_the_hooks_of_other_harnesses_are_not_theirs() {
        let stop = json!({ "hookEventName": "stop", "sessionId": "s1", "reason": "end_turn" });
        assert_eq!(update(Harness::Claude, stop.clone()), None);
        assert_eq!(update(Harness::Codex, stop), None);
    }

    #[test]
    fn installing_writes_a_file_of_its_own_and_uninstalling_removes_only_that() {
        let home = Home::new("grok");
        home.write(".grok/hooks/sidepulse.json", "{}");
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(HOOKS);
        assert_eq!(
            installed["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/bin/agent-leds hook grok"
        );
        assert_eq!(
            installed["hooks"]["Notification"][0]["matcher"],
            "permission_prompt"
        );
        assert_eq!(installed["hooks"].as_object().unwrap().len(), EVENTS.len());
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert_eq!(home.json(HOOKS), installed);

        uninstall(&home).unwrap();
        assert!(!home.join(HOOKS).exists());
        assert!(home.join(".grok/hooks/sidepulse.json").exists());
        uninstall(&home).unwrap();
    }
}
