//! Pi: it runs extensions rather than hook commands, so an extension of
//! ours in `~/.pi/agent/extensions/` passes its events to the hook.

use serde::Deserialize;
use serde_json::Value;

use super::{Event, Hook, Plugin, ends_on_question, hooked, or_empty};
use crate::coverage::{Declared, Evidence};
use crate::fsm::Signal;

pub const PLUGIN: Plugin = Plugin {
    path: ".pi/agent/extensions/agent-leds.ts",
    source: include_str!("pi.ts"),
};

/// What Pi's hooks tell, until its own files are read.
pub const SIGNALS: &[Declared] = &[
    hooked(
        Signal::Started,
        Evidence::Observed,
        "any event the extension passes on, from the Pi process that runs the hook",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::Prompted,
        Evidence::Observed,
        "agent_start or ui_prompt_end",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::QuestionAsked,
        Evidence::Observed,
        "ui_prompt_start: a dialog holds the run up",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::TurnEnded,
        Evidence::Observed,
        "agent_settled",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::TurnEndedAsking,
        Evidence::Observed,
        "agent_settled whose last assistant message asks",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::TurnFailed,
        Evidence::Inferred,
        "agent_settled with stop_reason error",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::Interrupted,
        Evidence::Observed,
        "agent_settled after an abort",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
    hooked(
        Signal::Ended,
        Evidence::Observed,
        "session_shutdown, or the Pi process gone",
        "0.5.0 extension, probe-pi rpc1, tui1",
    ),
];

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
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::Harness;
    use super::super::tests::signal;
    use super::*;
    use crate::fsm::Signal;

    fn event(name: &str, extra: Value) -> Option<Signal> {
        let mut event = json!({ "event": name, "session": "019a-pi", "cwd": "/Users/me/Code/app" });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        signal(Harness::Pi, event)
    }

    #[test]
    fn maps_a_run() {
        let none = json!({});
        assert_eq!(event("agent_start", none.clone()), Some(Signal::Prompted));
        assert_eq!(
            event("ui_prompt_start", none.clone()),
            Some(Signal::QuestionAsked)
        );
        assert_eq!(event("ui_prompt_end", none.clone()), Some(Signal::Prompted));
        let settled = |fields: Value| event("agent_settled", fields);
        assert_eq!(
            settled(json!({ "aborted": false, "stop_reason": "stop", "message": "Done." })),
            Some(Signal::TurnEnded)
        );
        assert_eq!(
            settled(
                json!({ "aborted": false, "stop_reason": "stop", "message": "Shall I push it?" })
            ),
            Some(Signal::TurnEndedAsking)
        );
        assert_eq!(
            settled(json!({ "aborted": false, "stop_reason": "error", "message": "" })),
            Some(Signal::TurnFailed)
        );
        assert_eq!(
            settled(json!({ "aborted": true, "stop_reason": "aborted", "message": "" })),
            Some(Signal::Interrupted)
        );
        assert_eq!(
            settled(json!({ "stop_reason": "aborted", "message": "Lighthouses stand." })),
            Some(Signal::Interrupted)
        );
        assert_eq!(event("session_shutdown", none.clone()), Some(Signal::Ended));
        assert_eq!(event("turn_end", none), None);
    }
}
