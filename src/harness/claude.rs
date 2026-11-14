//! Claude Code: its hooks live in `~/.claude/settings.json`.

use std::path::Path;

use anyhow::Result;
use serde_json::json;

use super::{Harness, commands, edit_hooks, owns_group, read_hooks, register_groups, unregister};

/// The hook events the daemon needs, in the order a turn produces them.
pub const EVENTS: [&str; 10] = [
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

pub const SETTINGS: &str = ".claude/settings.json";

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let command = Harness::Claude.command(exe);
    edit_hooks(&home.join(SETTINGS), |hooks| {
        register_groups(hooks, Harness::Claude, &EVENTS, |event| {
            let mut handler = json!({ "type": "command", "command": command, "timeout": 5 });
            if event == "PermissionRequest" {
                // An async hook cannot answer the permission prompt, so observing never approves anything.
                handler["async"] = json!(true);
            }
            handler
        })
    })
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    Ok(commands(&read_hooks(&home.join(SETTINGS))?, true))
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(SETTINGS), |hooks| {
        unregister(hooks, |group| owns_group(Harness::Claude, group));
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::Home;
    use super::*;

    #[test]
    fn every_event_registered_is_one_the_daemon_follows() {
        for event in EVENTS {
            assert!(super::super::Hook::named(event).is_some(), "{event}");
        }
    }

    #[test]
    fn installing_twice_changes_nothing_and_uninstalling_leaves_the_rest() {
        let theirs = json!({
            "model": "opus",
            "hooks": { "Stop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "python hook_entry.py --provider claude" }] }] }
        });
        let home = Home::new();
        home.write(SETTINGS, &theirs.to_string());

        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(SETTINGS);
        assert_eq!(installed["model"], theirs["model"]);
        assert_eq!(installed["hooks"]["Stop"][0], theirs["hooks"]["Stop"][0]);
        assert_eq!(
            installed["hooks"]["Stop"][1]["hooks"][0]["command"],
            "/bin/agent-leds hook claude"
        );
        assert_eq!(
            installed["hooks"]["PermissionRequest"][0]["hooks"][0]["async"],
            true
        );
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert_eq!(home.json(SETTINGS), installed);

        uninstall(&home).unwrap();
        assert_eq!(home.json(SETTINGS), theirs);
    }
}
