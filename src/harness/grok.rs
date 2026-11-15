//! Grok: its hooks live in files of their own in the `hooks` folder of
//! Grok's folder, `~/.grok` or `$GROK_HOME`. The daemon reads Grok's files,
//! and needs one hook, for a tool's approval card, which nothing in them
//! tells from an approval Grok gave itself. While any hook of the user's own
//! is registered, Grok asks once to trust each folder but the home folder
//! itself before it runs any of them.
//!
//! Grok also runs the hooks in `~/.claude/settings.json`, unless its config
//! sets `[compat.claude] hooks = false`, so a hook registered for Claude Code
//! fires inside every Grok session too, under Grok's session id.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::{Harness, claude, commands, read_hooks};
use crate::files;
use crate::source::grok::PERMISSION_PROMPT;

/// Our hooks' own file in Grok's folder, written whole. Grok reads every
/// JSON file in its `hooks` folder.
pub const FILE: &str = "hooks/agent-leds.json";

/// The hook events the daemon needs: `Notification`, only for
/// [`PERMISSION_PROMPT`].
pub const HOOKS: [&str; 1] = ["Notification"];

/// Grok's own config, in its folder.
const CONFIG: &str = "config.toml";

/// Writes our hooks' file whole in Grok's `folder`, in place of whatever an
/// older version wrote.
pub fn install(folder: &Path, exe: &Path) -> Result<()> {
    let handler = json!({ "type": "command", "command": Harness::Grok.command(exe), "timeout": 5 });
    let hooks =
        json!({ "hooks": { HOOKS[0]: [{ "matcher": PERMISSION_PROMPT, "hooks": [handler] }] } });
    files::replace(
        &folder.join(FILE),
        &(serde_json::to_string_pretty(&hooks)? + "\n"),
    )
}

/// The hooks in every file of the `hooks` folder of Grok's `folder`, since
/// Grok runs them all.
pub fn hooks(folder: &Path) -> Result<Vec<(String, String)>> {
    let folder = folder.join(FILE);
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

pub fn uninstall(folder: &Path) -> Result<()> {
    files::remove(&folder.join(FILE))
}

/// What is wrong when hooks an older agent-leds registered with Claude Code
/// are still in `~/.claude/settings.json` and Grok, with its `folder`, runs
/// that file's hooks.
pub fn claude_hooks(home: &Path, folder: &Path) -> Result<()> {
    if !reads_claude_hooks(folder)? {
        return Ok(());
    }
    let mut events: Vec<String> = claude::hooks(home)?
        .into_iter()
        .filter(|(_, command)| Harness::Claude.owns(command))
        .map(|(event, _)| event)
        .collect();
    events.dedup();
    if !events.is_empty() {
        bail!(
            "Grok also runs the Claude Code hook of agent-leds that ~/{} registers for {}, \
             each under Grok's session id; run `agent-leds install`",
            claude::SETTINGS,
            events.join(", ")
        );
    }
    Ok(())
}

/// Whether the config in Grok's `folder` leaves `compat.claude.hooks` on,
/// as it is by default.
fn reads_claude_hooks(folder: &Path) -> Result<bool> {
    let path = folder.join(CONFIG);
    let Some(text) = files::read_optional(&path)? else {
        return Ok(true);
    };
    let config: toml::Table =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let hooks = config
        .get("compat")
        .and_then(|compat| compat.get("claude"))
        .and_then(|claude| claude.get("hooks"))
        .and_then(toml::Value::as_bool);
    Ok(hooks.unwrap_or(true))
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::super::tests::Home;
    use super::*;
    use crate::source::Folders;

    /// Grok's folder where `GROK_HOME` puts it, outside `~/.grok`.
    fn elsewhere(home: &Home) -> Folders {
        Folders {
            grok: home.join("work/grok"),
            ..home.folders()
        }
    }

    #[test]
    fn installing_over_0_5_0_leaves_only_the_permission_prompt_and_the_rest() {
        let home = Home::new();
        let folders = elsewhere(&home);
        let grok = &folders.grok;
        home.write("work/grok/hooks/sidepulse.json", "{}");
        let ours = json!([{ "hooks": [{ "type": "command", "command": "/opt/homebrew/bin/agent-leds hook grok", "timeout": 5 }] }]);
        let mut hooks = Map::new();
        for event in [
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "Stop",
            "StopFailure",
            "StopCancelled",
            "SessionEnd",
        ] {
            hooks.insert(event.into(), ours.clone());
        }
        hooks.insert(
            "Notification".into(),
            json!([{ "matcher": "permission_prompt", "hooks": ours[0]["hooks"] }]),
        );
        home.write(
            "work/grok/hooks/agent-leds.json",
            &json!({ "hooks": hooks }).to_string(),
        );
        let exe = Path::new("/opt/homebrew/bin/agent-leds");
        let leftover = Harness::Grok
            .check(&home, &folders, exe)
            .unwrap_err()
            .to_string();
        assert!(
            leftover.starts_with("a hook this version does not use is registered"),
            "{leftover}"
        );
        install(grok, exe).unwrap();
        assert_eq!(
            home.json("work/grok/hooks/agent-leds.json"),
            json!({ "hooks": { "Notification": [{ "matcher": "permission_prompt", "hooks": ours[0]["hooks"] }] } })
        );
        assert!(grok.join("hooks/sidepulse.json").exists());
        assert!(!home.join(".grok").exists());
        Harness::Grok.check(&home, &folders, exe).unwrap();
        let moved = Harness::Grok.check(&home, &folders, Path::new("/usr/local/bin/agent-leds"));
        let moved = moved.unwrap_err().to_string();
        assert!(moved.contains("not this binary"), "{moved}");
        // A hook in ~/.grok is not one Grok reads.
        assert_eq!(
            Harness::Grok
                .check(&home, &home.folders(), exe)
                .unwrap_err()
                .to_string(),
            "not registered; run `agent-leds install`"
        );
        uninstall(grok).unwrap();
        assert!(!grok.join(FILE).exists());
        assert!(grok.join("hooks/sidepulse.json").exists());
        assert_eq!(
            Harness::Grok
                .check(&home, &folders, exe)
                .unwrap_err()
                .to_string(),
            "not registered; run `agent-leds install`"
        );
    }

    #[test]
    fn claude_hooks_of_ours_fire_inside_grok_unless_its_config_turns_them_off() {
        let home = Home::new();
        let folders = elsewhere(&home);
        let exe = Path::new("/opt/homebrew/bin/agent-leds");
        home.write("work/grok/config.toml", "[cli]\nauto_update = true\n");
        install(&folders.grok, exe).unwrap();
        Harness::Grok.check(&home, &folders, exe).unwrap();
        let handler =
            json!({ "type": "command", "command": "/opt/homebrew/bin/agent-leds hook claude" });
        home.write(
            claude::SETTINGS,
            &json!({ "hooks": {
                "Stop": [{ "hooks": [handler] }],
                "StopFailure": [{ "hooks": [handler, { "type": "command", "command": "afplay done.aiff" }] }],
                "Notification": [{ "hooks": [{ "type": "command", "command": "afplay ping.aiff" }] }],
            } })
            .to_string(),
        );
        assert_eq!(
            Harness::Grok
                .check(&home, &folders, exe)
                .unwrap_err()
                .to_string(),
            "Grok also runs the Claude Code hook of agent-leds that ~/.claude/settings.json \
             registers for Stop, StopFailure, each under Grok's session id; run `agent-leds \
             install`"
        );
        home.write("work/grok/config.toml", "[compat.claude]\nhooks = false\n");
        Harness::Grok.check(&home, &folders, exe).unwrap();
        // Installing removes them from Claude Code's settings.
        home.write("work/grok/config.toml", "");
        let used = |harness| super::super::in_use(harness, &home, &folders);
        super::super::install(&home, &folders, exe, used);
        Harness::Grok.check(&home, &folders, exe).unwrap();
        // The user's own handlers stay, the one beside ours included.
        let left = home.json(claude::SETTINGS)["hooks"].clone();
        assert_eq!(
            left,
            json!({
                "StopFailure": [{ "hooks": [{ "type": "command", "command": "afplay done.aiff" }] }],
                "Notification": [{ "hooks": [{ "type": "command", "command": "afplay ping.aiff" }] }],
            })
        );
    }
}
