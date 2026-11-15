//! Junie: its hooks live in `~/.junie/config.json`, in the shape Claude
//! Code's have. The daemon reads its files instead, so an install registers
//! none there and removes any an older version registered, with the file the
//! older hook kept between events. None may ever be registered for
//! PermissionRequest or PreToolUse: Junie reads a hook that succeeds there as
//! an answer, approving the request or allowing the tool.

use std::path::Path;

use anyhow::Result;

use super::{Harness, commands, edit_hooks, read_hooks, unregister};
use crate::files;

pub const CONFIG: &str = ".junie/config.json";

/// Where an older version's hook kept the session the last prompt named.
const SESSION: &str = ".local/state/agent-leds/junie-session.json";

/// The hook events the daemon needs: none, since its files say it all.
pub const HOOKS: [&str; 0] = [];

/// Removes every hook an older version registered.
pub fn install(home: &Path, _exe: &Path) -> Result<()> {
    uninstall(home)
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    Ok(commands(&read_hooks(&home.join(CONFIG))?, true))
}

pub fn uninstall(home: &Path) -> Result<()> {
    edit_hooks(&home.join(CONFIG), |hooks| {
        unregister(hooks, Harness::Junie, true);
        Ok(())
    })?;
    files::remove(&home.join(SESSION))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::Home;
    use super::*;

    /// The hooks 0.5.0 registered, beside one of someone else's.
    fn registered_by_0_5_0() -> serde_json::Value {
        let ours = json!([{ "hooks": [{ "type": "command", "command": "/opt/homebrew/bin/agent-leds hook junie", "timeout": 5 }] }]);
        let mut hooks = serde_json::Map::new();
        for event in [
            "UserPromptSubmit",
            "PostToolUse",
            "Stop",
            "StopFailure",
            "SessionEnd",
        ] {
            hooks.insert(event.into(), ours.clone());
        }
        hooks.insert(
            "PreToolUse".into(),
            json!([{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "python guard.py" }] }]),
        );
        // A handler of someone else's in a group of ours.
        hooks["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "type": "command", "command": "say done" }));
        json!({ "model": "sonnet", "hooks": hooks })
    }

    #[test]
    fn installing_over_0_5_0_removes_every_hook_of_ours_and_its_session_file() {
        let home = Home::new();
        home.write(CONFIG, &registered_by_0_5_0().to_string());
        home.write(
            SESSION,
            r#"{"session_id": "j1", "cwd": "/Users/me/Code/app"}"#,
        );
        let exe = Path::new("/bin/agent-leds");
        assert!(Harness::Junie.check(&home, &home.folders(), exe).is_err());

        install(&home, exe).unwrap();
        let theirs = json!({
            "model": "sonnet",
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }],
                "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "python guard.py" }] }],
            }
        });
        assert_eq!(home.json(CONFIG), theirs);
        assert!(!home.join(SESSION).exists());
        Harness::Junie.check(&home, &home.folders(), exe).unwrap();
        install(&home, exe).unwrap();
        assert_eq!(home.json(CONFIG), theirs);
        uninstall(&home).unwrap();
        assert_eq!(home.json(CONFIG), theirs);
    }

    #[test]
    fn a_hook_of_ours_left_registered_fails_the_check() {
        let home = Home::new();
        home.write(
            CONFIG,
            &json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "/bin/agent-leds hook junie" }] }] } }).to_string(),
        );
        let error = Harness::Junie
            .check(&home, &home.folders(), Path::new("/bin/agent-leds"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not use is registered for Stop"),
            "{error}"
        );
    }

    #[test]
    fn a_home_where_junie_has_no_config_is_left_without_one() {
        let home = Home::new();
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert!(!home.join(CONFIG).exists());
        Harness::Junie
            .check(&home, &home.folders(), Path::new("/bin/agent-leds"))
            .unwrap();
    }
}
