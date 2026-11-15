//! Grok: its hooks live in a file of their own in `~/.grok/hooks/`. The
//! daemon reads Grok's files, and keeps one hook, for a turn that ends on an
//! API error, which no file was seen to record.

use std::path::Path;

use anyhow::Result;
use serde_json::{Map, json};

use super::{Harness, commands, read_hooks};
use crate::files;

/// Grok reads every JSON file in the folder, so this one holds nothing but
/// what an install wrote.
pub const FILE: &str = ".grok/hooks/agent-leds.json";

/// The hook events the daemon needs: a turn's end on an API error.
pub const HOOKS: [&str; 1] = ["StopFailure"];

/// Writes the file whole, so a hook an older version registered goes.
pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let mut hooks = Map::new();
    for event in HOOKS {
        let handler =
            json!({ "type": "command", "command": Harness::Grok.command(exe), "timeout": 5 });
        hooks.insert(event.to_owned(), json!([{ "hooks": [handler] }]));
    }
    let hooks = json!({ "hooks": hooks });
    files::replace(
        &home.join(FILE),
        &(serde_json::to_string_pretty(&hooks)? + "\n"),
    )
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
    use super::super::tests::Home;
    use super::*;

    #[test]
    fn installing_writes_a_file_of_its_own_and_uninstalling_removes_only_that() {
        let home = Home::new();
        home.write(".grok/hooks/sidepulse.json", "{}");
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(FILE);
        assert_eq!(
            installed["hooks"]["StopFailure"][0]["hooks"][0]["command"],
            "/bin/agent-leds hook grok"
        );
        assert_eq!(installed["hooks"].as_object().unwrap().len(), HOOKS.len());
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert_eq!(home.json(FILE), installed);

        uninstall(&home).unwrap();
        assert!(!home.join(FILE).exists());
        assert!(home.join(".grok/hooks/sidepulse.json").exists());
        uninstall(&home).unwrap();
    }

    #[test]
    fn installing_over_0_5_0_keeps_only_stop_failure() {
        let home = Home::new();
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
        let events: Vec<String> = home.json(FILE)["hooks"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(events, ["StopFailure"]);
        Harness::Grok.check(&home, exe).unwrap();
    }
}
