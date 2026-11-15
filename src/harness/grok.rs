//! Grok: its hooks live in files of their own in `~/.grok/hooks/`. The
//! daemon reads Grok's files instead, so an install registers none and
//! removes the file an older version wrote.
//!
//! Grok also runs the hooks in `~/.claude/settings.json`, unless its config
//! sets `[compat.claude] hooks = false`, so a hook registered for Claude Code
//! fires inside every Grok session too, under Grok's session id.

use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Harness, claude, commands, read_hooks};
use crate::files;

/// The file an older version wrote. Grok reads every JSON file in the
/// folder.
pub const FILE: &str = ".grok/hooks/agent-leds.json";

/// The hook events the daemon needs: none, since its files say it all.
pub const HOOKS: [&str; 0] = [];

const CONFIG: &str = ".grok/config.toml";

/// Removes the file an older version wrote.
pub fn install(home: &Path, _exe: &Path) -> Result<()> {
    uninstall(home)
}

/// The hooks in every file of the folder, since Grok runs them all.
pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    let folder = home.join(FILE);
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
    files::remove(&home.join(FILE))
}

/// What is wrong when hooks an older agent-leds registered with Claude Code
/// are still in `~/.claude/settings.json` and Grok runs that file's hooks.
pub fn claude_hooks(home: &Path) -> Result<()> {
    if !reads_claude_hooks(home)? {
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

/// Whether Grok's config leaves `compat.claude.hooks` on, as it is by default.
fn reads_claude_hooks(home: &Path) -> Result<bool> {
    let path = home.join(CONFIG);
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

    #[test]
    fn installing_over_0_5_0_removes_its_file_and_leaves_the_rest() {
        let home = Home::new();
        home.write(".grok/hooks/sidepulse.json", "{}");
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
        home.write(FILE, &json!({ "hooks": hooks }).to_string());
        let exe = Path::new("/opt/homebrew/bin/agent-leds");
        let leftover = Harness::Grok.check(&home, exe).unwrap_err().to_string();
        assert!(
            leftover.starts_with("a hook this version does not use is registered"),
            "{leftover}"
        );
        install(&home, exe).unwrap();
        assert!(!home.join(FILE).exists());
        assert!(home.join(".grok/hooks/sidepulse.json").exists());
        Harness::Grok.check(&home, exe).unwrap();
        install(&home, exe).unwrap();
        uninstall(&home).unwrap();
    }

    #[test]
    fn claude_hooks_of_ours_fire_inside_grok_unless_its_config_turns_them_off() {
        let home = Home::new();
        let exe = Path::new("/opt/homebrew/bin/agent-leds");
        home.write(CONFIG, "[cli]\nauto_update = true\n");
        Harness::Grok.check(&home, exe).unwrap();
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
            Harness::Grok.check(&home, exe).unwrap_err().to_string(),
            "Grok also runs the Claude Code hook of agent-leds that ~/.claude/settings.json \
             registers for Stop, StopFailure, each under Grok's session id; run `agent-leds \
             install`"
        );
        home.write(CONFIG, "[compat.claude]\nhooks = false\n");
        Harness::Grok.check(&home, exe).unwrap();
        // Installing removes them from Claude Code's settings.
        home.write(CONFIG, "");
        super::super::install(&home, exe);
        Harness::Grok.check(&home, exe).unwrap();
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
