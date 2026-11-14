//! Pi: it runs extensions rather than hook commands, so an extension of
//! ours in `~/.pi/agent/extensions/` passes its events to the hook.

use serde::Deserialize;
use serde_json::Value;

use super::{Event, Hook, Plugin, ends_on_question, or_empty};

pub const PLUGIN: Plugin = Plugin {
    path: ".pi/agent/extensions/agent-leds.ts",
    source: include_str!("pi.ts"),
};

/// An event as the extension passes it on, by Pi's own name for it. The
/// extension passes none from a Pi without a UI, such as a subagent's.
#[derive(Deserialize)]
struct Payload {
    event: String,
    session: String,
    #[serde(default, deserialize_with = "or_empty")]
    cwd: String,
    /// Whether the run was stopped, as Escape does, at `agent_settled`.
    #[serde(default)]
    aborted: bool,
    /// Why the last assistant message ended, at `agent_settled`.
    stop_reason: Option<String>,
    /// The text of the last assistant message, at `agent_settled`.
    #[serde(default, deserialize_with = "or_empty")]
    message: String,
}

pub fn event(payload: &Value) -> Option<Event> {
    let payload = Payload::deserialize(payload).ok()?;
    let hook = match payload.event.as_str() {
        "agent_start" => Hook::UserPromptSubmit,
        // A dialog that holds up the run, such as an extension's confirmation.
        "ui_prompt_start" => Hook::Elicitation,
        "ui_prompt_end" => Hook::ElicitationResult,
        // Pi will not go on by itself after this. Before 1.0 it said that
        // the user stopped the run only by how the last message ended.
        "agent_settled" if payload.aborted || payload.stop_reason.as_deref() == Some("aborted") => {
            Hook::Interrupt
        }
        "agent_settled" if payload.stop_reason.as_deref() == Some("error") => Hook::StopFailure,
        "agent_settled" => Hook::Stop,
        "session_shutdown" => Hook::SessionEnd,
        _ => return None,
    };
    Some(Event {
        hook,
        asks: hook == Hook::Stop && ends_on_question(&payload.message),
        session: payload.session,
        cwd: payload.cwd,
        subagent: false,
        unfinished: false,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::Harness;
    use super::super::tests::update;
    use super::*;
    use crate::state::{State, Update};

    fn event(name: &str, extra: Value) -> Option<Update> {
        let mut event = json!({ "event": name, "session": "019a-pi", "cwd": "/Users/me/Code/app" });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        update(Harness::Pi, event)
    }

    #[test]
    fn maps_a_run() {
        let none = json!({});
        assert_eq!(
            event("agent_start", none.clone()),
            Some(Update::Enter(State::Working))
        );
        assert_eq!(
            event("ui_prompt_start", none.clone()),
            Some(Update::Enter(State::Question))
        );
        assert_eq!(
            event("ui_prompt_end", none.clone()),
            Some(Update::Enter(State::Working))
        );
        let settled = |fields: Value| event("agent_settled", fields);
        assert_eq!(
            settled(json!({ "aborted": false, "stop_reason": "stop", "message": "Done." })),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(
            settled(
                json!({ "aborted": false, "stop_reason": "stop", "message": "Shall I push it?" })
            ),
            Some(Update::Asked)
        );
        assert_eq!(
            settled(json!({ "aborted": false, "stop_reason": "error", "message": "" })),
            Some(Update::Enter(State::Error))
        );
        assert_eq!(
            settled(json!({ "aborted": true, "stop_reason": "aborted", "message": "" })),
            Some(Update::Leave)
        );
        assert_eq!(
            settled(json!({ "stop_reason": "aborted", "message": "Lighthouses stand." })),
            Some(Update::Leave)
        );
        assert_eq!(event("session_shutdown", none.clone()), Some(Update::Leave));
        assert_eq!(event("turn_end", none), None);
    }
}
