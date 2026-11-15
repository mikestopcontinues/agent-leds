//! Cursor: its hooks live in `~/.cursor/hooks.json`, under event names of
//! its own. The daemon reads its files, and needs two hooks.

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use super::{Harness, commands, edit_hooks, read_hooks, register, unregister};
use crate::files;

pub const FILE: &str = ".cursor/hooks.json";

/// What an install writes to a hooks file that is not there, to add to.
const CREATED: &str = "{ \"version\": 1 }\n";

/// The hook events the daemon needs. A prompt given as an argument or a
/// text-only turn writes nothing until the turn ends, and no file names
/// the process a chat runs in, so the prompt's hook says both. An
/// interrupt before the turn's first checkpoint, or a failure after one the
/// same, can leave the transcript as it was, so the stop hook says it.
pub const HOOKS: [&str; 2] = ["beforeSubmitPrompt", "stop"];

fn is_ours(entry: &Value) -> bool {
    entry["command"]
        .as_str()
        .is_some_and(|command| Harness::Cursor.owns(command))
}

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let file = home.join(FILE);
    if !file.exists() {
        files::replace(&file, CREATED)?;
    }
    let entry = json!({ "command": Harness::Cursor.command(exe), "timeout": 5 });
    edit_hooks(&file, |hooks| {
        // Only these events, whatever an older version registered.
        unregister(hooks, Harness::Cursor, false);
        for event in HOOKS {
            register(hooks, event, entry.clone(), is_ours)?;
        }
        Ok(())
    })
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    Ok(commands(&read_hooks(&home.join(FILE))?, false))
}

pub fn uninstall(home: &Path) -> Result<()> {
    let file = home.join(FILE);
    let mut removed = false;
    edit_hooks(&file, |hooks| {
        removed = unregister(hooks, Harness::Cursor, false);
        Ok(())
    })?;
    // Nothing is left of a file the install created.
    let left = files::read_optional(&file)?;
    let created: Value = serde_json::from_str(CREATED)?;
    if removed
        && left.is_some_and(|text| serde_json::from_str::<Value>(&text).ok() == Some(created))
    {
        files::remove(&file)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::tests::Home;
    use super::*;

    #[test]
    fn installing_twice_changes_nothing_and_uninstalling_leaves_the_rest() {
        let theirs = json!({
            "version": 1,
            "hooks": {
                "stop": [{ "command": "python hook_entry.py --provider cursor --event stop ; true" }],
                "preToolUse": [{ "command": "python guard.py" }]
            }
        });
        let home = Home::new();
        home.write(FILE, &theirs.to_string());

        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(FILE);
        assert_eq!(installed["version"], 1);
        assert_eq!(
            installed["hooks"]["preToolUse"],
            theirs["hooks"]["preToolUse"]
        );
        let ours = json!({ "command": "/bin/agent-leds hook cursor", "timeout": 5 });
        let mut stop = theirs["hooks"]["stop"].as_array().unwrap().clone();
        stop.push(ours.clone());
        assert_eq!(installed["hooks"]["stop"], json!(stop));
        assert_eq!(installed["hooks"]["beforeSubmitPrompt"], json!([ours]));
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        assert_eq!(home.json(FILE), installed);

        uninstall(&home).unwrap();
        assert_eq!(home.json(FILE), theirs);
    }

    #[test]
    fn a_first_hooks_file_says_which_version_it_is() {
        let home = Home::new();
        fs::create_dir(home.join(".cursor")).unwrap();
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        let installed = home.json(FILE);
        assert_eq!(installed["version"], 1);
        assert_eq!(installed["hooks"].as_object().unwrap().len(), HOOKS.len());
    }

    #[test]
    fn installing_over_0_5_0_leaves_only_the_prompt_and_stop_hooks() {
        let ours = json!([{ "command": "/opt/homebrew/bin/agent-leds hook cursor", "timeout": 5 }]);
        let mut hooks = serde_json::Map::new();
        for event in [
            "beforeSubmitPrompt",
            "postToolUse",
            "postToolUseFailure",
            "stop",
            "sessionEnd",
        ] {
            hooks.insert(event.into(), ours.clone());
        }
        let home = Home::new();
        home.write(FILE, &json!({ "version": 1, "hooks": hooks }).to_string());
        assert!(
            Harness::Cursor
                .check(&home, Path::new("/opt/homebrew/bin/agent-leds"))
                .unwrap_err()
                .to_string()
                .contains("a hook this version does not use")
        );
        install(&home, Path::new("/opt/homebrew/bin/agent-leds")).unwrap();
        assert_eq!(
            home.json(FILE),
            json!({ "version": 1, "hooks": { "beforeSubmitPrompt": ours, "stop": ours } })
        );
        Harness::Cursor
            .check(&home, Path::new("/opt/homebrew/bin/agent-leds"))
            .unwrap();
    }

    #[test]
    fn uninstalling_removes_the_file_an_install_created() {
        let home = Home::new();
        fs::create_dir(home.join(".cursor")).unwrap();
        install(&home, Path::new("/bin/agent-leds")).unwrap();
        uninstall(&home).unwrap();
        assert!(!home.join(FILE).exists());
        // One the user made is theirs, though it holds no more.
        home.write(FILE, CREATED);
        uninstall(&home).unwrap();
        assert_eq!(fs::read_to_string(home.join(FILE)).unwrap(), CREATED);
    }
}
