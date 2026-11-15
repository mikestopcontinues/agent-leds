//! Grok: its hooks live in files of their own in `~/.grok/hooks/`. The
//! daemon reads Grok's files instead, so an install registers none and
//! removes the file an older version wrote.

use std::path::Path;

use anyhow::Result;

use super::{commands, read_hooks};
use crate::files;

/// The file an older version wrote. Grok reads every JSON file in the
/// folder.
pub const FILE: &str = ".grok/hooks/agent-leds.json";

/// The hook events the daemon needs: none, since its files say it all.
pub const HOOKS: [&str; 0] = [];

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

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::super::Harness;
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
}
