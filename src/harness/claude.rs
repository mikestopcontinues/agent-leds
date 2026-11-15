//! Claude Code: its hooks live in `~/.claude/settings.json`. The daemon reads
//! its files instead, so an install registers none there and removes any an
//! older version registered.

use std::path::Path;

use anyhow::Result;

use super::{Harness, commands, edit_hooks, read_hooks, unregister};

/// The hook events the daemon needs: none, since its files say it all.
pub const HOOKS: [&str; 0] = [];

pub const SETTINGS: &str = ".claude/settings.json";

/// Removes every hook an older version registered.
pub fn install(home: &Path, _exe: &Path) -> Result<()> {
    uninstall(home)
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    Ok(commands(&read_hooks(&home.join(SETTINGS))?, true))
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(SETTINGS), |hooks| {
        unregister(hooks, Harness::Claude, true);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::Home;
    use super::*;

    /// The hooks 0.5.0 registered, beside one of someone else's.
    fn registered_by_0_5_0() -> serde_json::Value {
        let ours = |extra: serde_json::Value| {
            let mut handler = json!({ "type": "command", "command": "/opt/homebrew/bin/agent-leds hook claude", "timeout": 5 });
            handler
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            json!({ "hooks": [handler] })
        };
        let mut hooks = serde_json::Map::new();
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "Elicitation",
            "ElicitationResult",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ] {
            hooks.insert(event.into(), json!([ours(json!({}))]));
        }
        hooks.insert(
            "PermissionRequest".into(),
            json!([ours(json!({ "async": true }))]),
        );
        hooks.get_mut("Stop").unwrap().as_array_mut().unwrap().insert(
            0,
            json!({ "matcher": "*", "hooks": [{ "type": "command", "command": "python hook_entry.py --provider claude" }] }),
        );
        // The user, or `/hooks`, added a handler to a group of ours.
        hooks["StopFailure"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "command", "command": "afplay done.aiff" }));
        json!({ "model": "opus", "hooks": hooks, "theme": "dark" })
    }

    #[test]
    fn installing_over_0_5_0_removes_every_hook_of_ours_and_leaves_the_rest() {
        let home = Home::new();
        home.write(SETTINGS, &registered_by_0_5_0().to_string());
        let exe = Path::new("/bin/agent-leds");
        assert!(Harness::Claude.check(&home, exe).is_err());

        install(&home, exe).unwrap();
        let theirs = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "python hook_entry.py --provider claude" }] }],
                "StopFailure": [{ "hooks": [{ "type": "command", "command": "afplay done.aiff" }] }],
            },
            "theme": "dark"
        });
        assert_eq!(home.json(SETTINGS), theirs);
        Harness::Claude.check(&home, exe).unwrap();
        install(&home, exe).unwrap();
        assert_eq!(home.json(SETTINGS), theirs);
        uninstall(&home).unwrap();
        assert_eq!(home.json(SETTINGS), theirs);
    }

    #[test]
    fn a_hook_of_ours_left_registered_fails_the_check() {
        let home = Home::new();
        home.write(
            SETTINGS,
            &json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "/bin/agent-leds hook claude" }] }] } }).to_string(),
        );
        let error = Harness::Claude
            .check(&home, Path::new("/bin/agent-leds"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not use is registered for Stop"),
            "{error}"
        );
    }
}
