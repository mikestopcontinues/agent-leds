//! OpenCode: it runs plugins rather than hook commands, so a plugin of
//! ours in `~/.config/opencode/plugins/` passes its events to the hook.

use serde::Deserialize;
use serde_json::Value;

use super::{Event, Hook, Plugin, ends_on_question, or_empty};

pub const PLUGIN: Plugin = Plugin {
    path: ".config/opencode/plugins/agent-leds.js",
    source: include_str!("opencode.js"),
};

/// An event as the plugin passes it on: OpenCode's own name for it, and
/// the session it belongs to. A subagent's event names the session that
/// started the subagent.
#[derive(Deserialize)]
struct Payload {
    event: String,
    session: String,
    #[serde(default, deserialize_with = "or_empty")]
    directory: String,
    #[serde(default)]
    subagent: bool,
    /// The name of the error the run ended on, at `session.idle`.
    error: Option<String>,
    /// The text of the last assistant message, at `session.idle`.
    #[serde(default, deserialize_with = "or_empty")]
    message: String,
}

pub fn event(payload: &Value) -> Option<Event> {
    let payload = Payload::deserialize(payload).ok()?;
    let hook = match payload.event.as_str() {
        // The plugin reports a session going busy once per run.
        "session.busy" => Hook::UserPromptSubmit,
        "permission.asked" => Hook::PermissionRequest,
        "question.asked" => Hook::Elicitation,
        "permission.replied" | "question.replied" | "question.rejected" => Hook::ElicitationResult,
        // A run ends idle however it ended; the error says how.
        "session.idle" => match payload.error.as_deref() {
            None => Hook::Stop,
            // The user stopped the run.
            Some("MessageAbortedError") => Hook::Interrupt,
            Some(_) => Hook::StopFailure,
        },
        "session.deleted" => Hook::SessionEnd,
        _ => return None,
    };
    Some(Event {
        hook,
        asks: hook == Hook::Stop && ends_on_question(&payload.message),
        session: payload.session,
        cwd: payload.directory,
        subagent: payload.subagent,
        unfinished: false,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::update;
    use super::super::{Harness, Report, report};
    use super::*;
    use crate::state::{State, Update};

    fn event(name: &str, extra: Value) -> Option<Update> {
        let mut event = json!({ "event": name, "session": "ses_1", "directory": "/Users/me/Code/app", "subagent": false });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        update(Harness::OpenCode, event)
    }

    #[test]
    fn maps_a_run() {
        let none = json!({});
        let working = Some(Update::Enter(State::Working));
        let question = Some(Update::Enter(State::Question));
        assert_eq!(event("session.busy", none.clone()), working);
        assert_eq!(event("permission.asked", none.clone()), question);
        assert_eq!(event("permission.replied", none.clone()), working);
        assert_eq!(event("question.asked", none.clone()), question);
        assert_eq!(event("question.rejected", none.clone()), working);
        let idle = |error: Value, message: &str| {
            event(
                "session.idle",
                json!({ "error": error, "message": message }),
            )
        };
        assert_eq!(
            idle(Value::Null, "done"),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(idle(Value::Null, "Red or blue?"), Some(Update::Asked));
        assert_eq!(idle(json!("MessageAbortedError"), ""), Some(Update::Leave));
        assert_eq!(
            idle(json!("ProviderAuthError"), ""),
            Some(Update::Enter(State::Error))
        );
        assert_eq!(event("session.deleted", none.clone()), Some(Update::Leave));
        assert_eq!(event("session.compacted", none), None);
    }

    #[test]
    fn a_subagent_s_events_say_its_parent_is_at_work() {
        let idle = json!({
            "event": "session.idle", "session": "ses_parent", "directory": "/Users/me/Code/app",
            "subagent": true, "error": null, "message": "done"
        });
        assert_eq!(
            report(Harness::OpenCode, &idle),
            Some(Report {
                harness: Harness::OpenCode,
                session: "ses_parent".into(),
                label: "app".into(),
                update: Update::Background,
            })
        );
    }
}
