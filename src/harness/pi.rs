//! Pi: it runs extensions rather than hook commands, so an extension of
//! ours in `~/.pi/agent/extensions/` passes the events its files cannot tell
//! to the hook.

use super::Plugin;

pub const PLUGIN: Plugin = Plugin {
    path: ".pi/agent/extensions/agent-leds.ts",
    source: include_str!("pi.ts"),
};

/// The events the extension passes on, and the only ones.
pub const HOOKS: [&str; 6] = [
    "session_start",
    "agent_start",
    "ui_prompt_start",
    "ui_prompt_end",
    "agent_settled",
    "session_shutdown",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_passes_on_exactly_the_events_it_declares() {
        let mut heard: Vec<&str> = PLUGIN
            .source
            .split("pi.on(\"")
            .skip(1)
            .filter_map(|rest| rest.split_once('"').map(|(event, _)| event))
            .collect();
        heard.sort();
        let mut declared = HOOKS.to_vec();
        declared.sort();
        assert_eq!(heard, declared);
    }
}
