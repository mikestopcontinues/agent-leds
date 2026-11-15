//! OpenCode: it runs plugins rather than hook commands, so a plugin of
//! ours in `~/.config/opencode/plugins/` passes its events to the hook.

use serde::Deserialize;
use serde_json::Value;

use super::{Event, Hook, Plugin, ends_on_question, hooked, or_empty};
use crate::coverage::{Declared, Evidence};
use crate::fsm::Signal;

pub const PLUGIN: Plugin = Plugin {
    path: ".config/opencode/plugins/agent-leds.js",
    source: include_str!("opencode.js"),
};

/// What OpenCode's hooks tell, until its own files are read.
pub const SIGNALS: &[Declared] = &[
    hooked(
        Signal::Started,
        Evidence::Observed,
        "any event the plugin passes on, from the OpenCode process that runs the hook",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::Prompted,
        Evidence::Observed,
        "session.status going busy, permission.replied, question.replied or question.rejected",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::PermissionAsked,
        Evidence::Observed,
        "permission.asked",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::QuestionAsked,
        Evidence::Observed,
        "question.asked",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::TurnEnded,
        Evidence::Observed,
        "session.idle, however the run ended",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::TurnEndedAsking,
        Evidence::Observed,
        "session.idle whose last assistant message asks",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::TurnFailed,
        Evidence::Observed,
        "session.idle after an error",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::Interrupted,
        Evidence::Observed,
        "session.idle after MessageAbortedError",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::Backgrounded,
        Evidence::Observed,
        "any event of a subagent's session, put to its parent",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
    hooked(
        Signal::Ended,
        Evidence::Observed,
        "session.deleted, or the OpenCode process gone",
        "0.5.0 plugin, probe-opencode T2–T17",
    ),
];

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
        "session.idle" => Hook::Stop,
        "session.deleted" => Hook::SessionEnd,
        _ => return None,
    };
    Some(Event {
        hook,
        asks: hook == Hook::Stop && ends_on_question(&payload.message),
        // The user stopped the run, which OpenCode tells as an error too.
        interrupted: hook == Hook::Stop && payload.error.as_deref() == Some("MessageAbortedError"),
        failed: hook == Hook::Stop && payload.error.is_some(),
        session: payload.session,
        cwd: payload.directory,
        subagent: payload.subagent,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::super::tests::signals;
    use super::super::{Harness, interim};
    use super::*;
    use crate::fsm::Signal;

    fn event(name: &str, extra: Value) -> Option<Vec<Signal>> {
        let mut event = json!({ "event": name, "session": "ses_1", "directory": "/Users/me/Code/app", "subagent": false });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        signals(Harness::OpenCode, event)
    }

    #[test]
    fn maps_a_run() {
        let none = json!({});
        let working = Some(vec![Signal::Prompted]);
        let question = Some(vec![Signal::QuestionAsked]);
        assert_eq!(event("session.busy", none.clone()), working);
        assert_eq!(
            event("permission.asked", none.clone()),
            Some(vec![Signal::PermissionAsked])
        );
        assert_eq!(event("permission.replied", none.clone()), working);
        assert_eq!(event("question.asked", none.clone()), question);
        assert_eq!(event("question.rejected", none.clone()), working);
        let idle = |error: Value, message: &str| {
            event(
                "session.idle",
                json!({ "error": error, "message": message }),
            )
        };
        assert_eq!(idle(Value::Null, "done"), Some(vec![Signal::TurnEnded]));
        assert_eq!(
            idle(Value::Null, "Red or blue?"),
            Some(vec![Signal::TurnEnded, Signal::TurnEndedAsking])
        );
        assert_eq!(
            idle(json!("MessageAbortedError"), ""),
            Some(vec![
                Signal::TurnEnded,
                Signal::TurnFailed,
                Signal::Interrupted
            ])
        );
        assert_eq!(
            idle(json!("ProviderAuthError"), ""),
            Some(vec![Signal::TurnEnded, Signal::TurnFailed])
        );
        assert_eq!(
            event("session.deleted", none.clone()),
            Some(vec![Signal::Ended])
        );
        assert_eq!(event("session.compacted", none), None);
    }

    #[test]
    fn a_subagent_s_events_say_its_parent_is_at_work() {
        let idle = json!({
            "event": "session.idle", "session": "ses_parent", "directory": "/Users/me/Code/app",
            "subagent": true, "error": null, "message": "done"
        });
        assert_eq!(
            interim(Harness::OpenCode, &idle),
            Some((
                "ses_parent".into(),
                "/Users/me/Code/app".into(),
                BTreeSet::from([Signal::Backgrounded])
            ))
        );
    }
}
