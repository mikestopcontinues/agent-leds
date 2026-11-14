//! Grok: its hooks live in a file of their own in `~/.grok/hooks/`.

use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Event, Harness, Hook, commands, ends_on_question, hooked, or_empty, read_hooks};
use crate::coverage::{Declared, Evidence};
use crate::files;
use crate::fsm::Signal;

/// Grok reads every JSON file in the folder, so this one holds nothing but
/// what an install wrote.
pub const HOOKS: &str = ".grok/hooks/agent-leds.json";

/// The hook events the daemon needs, in the order a turn produces them,
/// each with what it must match. Grok names the kind of a notification
/// there.
pub const EVENTS: [(&str, Option<&str>); 9] = [
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

/// What Grok's hooks tell, until its own files are read.
pub const SIGNALS: &[Declared] = &[
    hooked(
        Signal::Started,
        Evidence::Observed,
        "any hook but a subagent's, run under the process named grok-native",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::Prompted,
        Evidence::Observed,
        "user_prompt_submit, pre_tool_use, post_tool_use or post_tool_use_failure",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::PermissionAsked,
        Evidence::Observed,
        "notification of a permission_prompt",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::QuestionAsked,
        Evidence::Observed,
        "pre_tool_use for ask_user_question",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::TurnEnded,
        Evidence::Observed,
        "stop with reason end_turn, or stop_cancelled that Grok gave up on itself",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::TurnEndedAsking,
        Evidence::Observed,
        "stop with reason end_turn whose last message asks",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::TurnFailed,
        Evidence::Inferred,
        "stop_failure; no terminal API error was produced in the probe",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::Interrupted,
        Evidence::Observed,
        "stop_cancelled by the user",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
    hooked(
        Signal::Ended,
        Evidence::Observed,
        "session_end, or the grok-native process gone",
        "0.5.0 hooks, probe-grok runs a–l",
    ),
];

/// A hook payload as Grok spells it: its fields in camelCase and its
/// event names in snake_case.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    hook_event_name: String,
    session_id: String,
    #[serde(default, deserialize_with = "or_empty")]
    cwd: String,
    /// Set when a subagent sent the event.
    subagent_type: Option<String>,
    notification_type: Option<String>,
    /// Why a turn stopped.
    reason: Option<String>,
    cancelled_by: Option<String>,
    tool_name: Option<String>,
    #[serde(default, deserialize_with = "or_empty")]
    last_assistant_message: String,
}

pub fn event(payload: &Value) -> Option<Event> {
    let payload = Payload::deserialize(payload).ok()?;
    // A subagent runs as a session of its own, which says what kind it is
    // and not which session started it, so its events cannot be put to the
    // session they belong to.
    if payload.subagent_type.is_some() {
        return None;
    }
    let is = |field: &Option<String>, value: &str| field.as_deref() == Some(value);
    let hook = match payload.hook_event_name.as_str() {
        "user_prompt_submit" => Hook::UserPromptSubmit,
        "pre_tool_use" => Hook::PreToolUse,
        // Grok has no event for a permission request. It sends this
        // notification while its prompt is waiting.
        "notification" if is(&payload.notification_type, "permission_prompt") => {
            Hook::PermissionRequest
        }
        "post_tool_use" => Hook::PostToolUse,
        "post_tool_use_failure" => Hook::PostToolUseFailure,
        // Stop fires again when the session closes, which says nothing about a turn.
        "stop" if is(&payload.reason, "end_turn") => Hook::Stop,
        "stop_failure" => Hook::StopFailure,
        // The user was there to cancel the turn. One that Grok gave up on by
        // itself is left for them to find.
        "stop_cancelled" if is(&payload.cancelled_by, "user") => Hook::Interrupt,
        "stop_cancelled" => Hook::Stop,
        "session_end" => Hook::SessionEnd,
        _ => return None,
    };
    Some(Event {
        hook,
        asks: match hook {
            Hook::PreToolUse => is(&payload.tool_name, "ask_user_question"),
            _ => ends_on_question(&payload.last_assistant_message),
        },
        session: payload.session_id,
        cwd: payload.cwd,
        subagent: false,
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
    let hooks = json!({ "hooks": hooks });
    files::replace(
        &home.join(HOOKS),
        &(serde_json::to_string_pretty(&hooks)? + "\n"),
    )
}

/// The hooks in every file of the folder, since Grok runs them all.
pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    let folder = home.join(HOOKS);
    let folder = folder.parent().expect("the file is in a folder");
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            found.extend(commands(&read_hooks(&path)?, true));
        }
    }
    Ok(found)
}

pub fn uninstall(home: &Path) -> Result<()> {
    files::remove(&home.join(HOOKS))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Home, signal};
    use super::*;
    use crate::fsm::Signal;

    /// A payload with the fields Grok sends on every event.
    fn event(name: &str, extra: Value) -> Option<Signal> {
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
        signal(Harness::Grok, event)
    }

    #[test]
    fn maps_a_turn() {
        let working = Some(Signal::Prompted);
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
            Some(Signal::QuestionAsked)
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
            Some(Signal::TurnFailed)
        );
        assert_eq!(
            event("session_end", json!({ "reason": "exit" })),
            Some(Signal::Ended)
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
        assert_eq!(stop("end_turn", "Done."), Some(Signal::TurnEnded));
        assert_eq!(
            stop("end_turn", "Shall I push it?"),
            Some(Signal::TurnEndedAsking)
        );
        assert_eq!(stop("channel_closed", "Done."), None);
        assert_eq!(stop("shutdown", "Done."), None);
    }

    #[test]
    fn a_waiting_permission_prompt_is_a_question_and_other_notifications_are_not() {
        let notification = |kind: &str| event("notification", json!({ "notificationType": kind }));
        assert_eq!(
            notification("permission_prompt"),
            Some(Signal::PermissionAsked)
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
        assert_eq!(
            cancelled("user_interrupt", "user"),
            Some(Signal::Interrupted)
        );
        assert_eq!(
            cancelled("permission_rejected", "user"),
            Some(Signal::Interrupted)
        );
        assert_eq!(cancelled("max_turns", "runtime"), Some(Signal::TurnEnded));
    }

    #[test]
    fn a_subagent_is_not_followed() {
        // A subagent's events carry its own session and nothing that names
        // the session that started it.
        let explore = json!({ "subagentType": "explore", "reason": "end_turn" });
        assert_eq!(event("stop", explore.clone()), None);
        assert_eq!(event("post_tool_use", explore), None);
    }

    #[test]
    fn payloads_grok_sends_to_the_hooks_of_other_harnesses_are_not_theirs() {
        // As Grok 1.0.13 sent it to the hooks in ~/.claude/settings.json: in
        // both spellings, with its own event names.
        let stop = json!({
            "hookEventName": "stop", "hook_event_name": "stop",
            "sessionId": "s1", "session_id": "s1",
            "reason": "end_turn", "lastAssistantMessage": "ok", "backgroundTasks": [],
        });
        assert_eq!(signal(Harness::Codex, stop), None);
    }

    #[test]
    fn installing_writes_a_file_of_its_own_and_uninstalling_removes_only_that() {
        let home = Home::new();
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
