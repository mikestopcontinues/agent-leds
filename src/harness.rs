//! The agent harnesses that report to the daemon: what their hook events
//! mean, and registering the hook with each.
//!
//! The harnesses took their hook vocabulary from Claude Code, so one mapping
//! serves them all. Each harness module turns its own payloads into that
//! vocabulary and knows where the harness keeps its hooks.

mod claude;
mod codex;
mod grok;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::daemon::Request;
use crate::state::{State, Update};

/// An agent harness whose sessions the queue shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    /// The default, because a queue saved before sessions recorded their
    /// harness holds only Claude Code's.
    #[default]
    Claude,
    Codex,
    Grok,
}

impl Harness {
    pub const ALL: [Harness; 3] = [Harness::Claude, Harness::Codex, Harness::Grok];

    /// What the harness is called on the command line and in the queue.
    fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
        }
    }

    /// What the harness is called in a sentence.
    pub fn title(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Grok => "Grok",
        }
    }

    /// The harness's folder in the home directory, which exists once the
    /// harness has been used.
    fn folder(self) -> &'static str {
        match self {
            Harness::Claude => ".claude",
            Harness::Codex => ".codex",
            Harness::Grok => ".grok",
        }
    }

    /// The command the harness runs on each hook event.
    fn command(self, exe: &Path) -> String {
        format!("{} hook {}", exe.display(), self.name())
    }

    /// Whether a hook command is one this harness's install wrote.
    fn owns(self, command: &Value) -> bool {
        let ours = format!("leds hook {}", self.name());
        command
            .as_str()
            .is_some_and(|command| command.ends_with(&ours))
    }

    fn event(self, payload: &Value) -> Option<Event> {
        match self {
            Harness::Claude => claude::event(payload),
            Harness::Codex => codex::event(payload),
            Harness::Grok => grok::event(payload),
        }
    }

    fn install(self, home: &Path, exe: &Path) -> Result<()> {
        match self {
            Harness::Claude => claude::install(home, exe),
            Harness::Codex => codex::install(home, exe),
            Harness::Grok => grok::install(home, exe),
        }
    }

    fn uninstall(self, home: &Path) -> Result<()> {
        match self {
            Harness::Claude => claude::uninstall(home),
            Harness::Codex => codex::uninstall(home),
            Harness::Grok => grok::uninstall(home),
        }
    }
}

/// Registers the hook with every harness that has a folder in `home`,
/// replacing any earlier registration. One harness failing does not stop the
/// rest.
pub fn install(home: &Path, exe: &Path) -> Vec<(Harness, Result<()>)> {
    let used = |harness: &Harness| home.join(harness.folder()).is_dir();
    let used = Harness::ALL.into_iter().filter(used);
    used.map(|harness| (harness, harness.install(home, exe)))
        .collect()
}

/// Removes the hook from every harness and leaves everything else in place.
pub fn uninstall(home: &Path) -> Vec<(Harness, Result<()>)> {
    let all = Harness::ALL.into_iter();
    all.map(|harness| (harness, harness.uninstall(home)))
        .collect()
}

/// One hook event in the vocabulary the harnesses share.
struct Event {
    /// The event as Claude Code names it, or `Interrupt`, which is Codex's
    /// name for the user cutting a turn short.
    name: String,
    session: String,
    /// The folder the session works in.
    cwd: String,
    /// Whether the event puts a question to the user: it runs a tool that
    /// asks, or it ends the turn on a message that does.
    asks: bool,
    /// Whether something other than the main agent sent the event.
    subagent: bool,
}

impl Event {
    /// Reads a payload that spells its fields the way Claude Code does.
    fn read(payload: &Value) -> Option<Event> {
        Some(Event {
            name: text(payload, "hook_event_name")?.to_owned(),
            session: text(payload, "session_id")?.to_owned(),
            cwd: text(payload, "cwd").unwrap_or_default().to_owned(),
            asks: ends_on_question(text(payload, "last_assistant_message").unwrap_or_default()),
            subagent: payload.get("agent_id").is_some(),
        })
    }

    /// What the event means for its session.
    fn update(&self) -> Option<Update> {
        if self.subagent {
            // Only the main agent's events say what the session needs. A
            // subagent's say that work is going on, which matters once the
            // main agent has stopped and left a subagent running in the
            // background.
            return Some(Update::Background);
        }
        Some(match self.name.as_str() {
            "UserPromptSubmit" | "PostToolUse" | "PostToolUseFailure" | "ElicitationResult" => {
                Update::Enter(State::Working)
            }
            "PreToolUse" if self.asks => Update::Enter(State::Question),
            "PreToolUse" => Update::Enter(State::Working),
            "PermissionRequest" | "Elicitation" => Update::Enter(State::Question),
            "Stop" if self.asks => Update::Asked,
            "Stop" => Update::Enter(State::Stopped),
            "StopFailure" => Update::Enter(State::Error),
            "SessionEnd" | "Interrupt" => Update::Leave,
            _ => return None,
        })
    }
}

/// Translates one hook payload from a harness into a report for the daemon.
pub fn report(harness: Harness, payload: &Value) -> Option<Request> {
    let event = harness.event(payload)?;
    let update = event.update()?;
    let label = Path::new(&event.cwd)
        .file_name()
        .map_or(event.cwd.as_str().into(), |name| name.to_string_lossy())
        .into_owned();
    Some(Request::Report {
        harness,
        session: event.session,
        label,
        update,
    })
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// Whether a final message ends by asking the user something.
fn ends_on_question(message: &str) -> bool {
    let last = message
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    last.trim_end_matches(|c: char| c.is_whitespace() || "*_`\"')".contains(c))
        .ends_with('?')
}

/// Rewrites a JSON settings file through `edit`, which is given the file's
/// `hooks` object. Everything else in the file stays as it is, and a file
/// that `edit` leaves unchanged is not written.
fn edit_hooks(path: &Path, edit: impl FnOnce(&mut Map<String, Value>) -> Result<()>) -> Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let before: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut settings = before.clone();
    let Some(root) = settings.as_object_mut() else {
        bail!("{} is not a JSON object", path.display())
    };
    let Some(hooks) = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        bail!("hooks in {} is not an object", path.display())
    };
    edit(hooks)?;
    if hooks.is_empty() {
        root.remove("hooks");
    }
    if settings == before {
        return Ok(());
    }
    replace(path, &(serde_json::to_string_pretty(&settings)? + "\n"))
}

/// Replaces a file's text in one step, so a harness never reads half a
/// file. The file stays as private as it was: a config can hold secrets.
fn replace(path: &Path, text: &str) -> Result<()> {
    let mut staged = path.as_os_str().to_owned();
    staged.push(".leds-tmp");
    fs::write(&staged, text)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&staged, metadata.permissions())?;
    }
    fs::rename(&staged, path).with_context(|| format!("writing {}", path.display()))
}

/// Registers `handler` for `event` in a `hooks` object of the shape Claude
/// Code gave the harnesses: each event lists groups, and each group lists
/// handlers. The group an earlier install added is replaced.
fn register(
    hooks: &mut Map<String, Value>,
    harness: Harness,
    event: &str,
    handler: Value,
) -> Result<()> {
    let groups = hooks.entry(event).or_insert_with(|| json!([]));
    let Some(groups) = groups.as_array_mut() else {
        bail!("hooks.{event} is not a list")
    };
    groups.retain(|group| !is_ours(harness, group));
    groups.push(json!({ "hooks": [handler] }));
    Ok(())
}

/// Removes the groups an install added to a `hooks` object, and the events
/// that are left with none.
fn unregister(hooks: &mut Map<String, Value>, harness: Harness) {
    for groups in hooks.values_mut() {
        if let Some(groups) = groups.as_array_mut() {
            groups.retain(|group| !is_ours(harness, group));
        }
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|groups| !groups.is_empty()));
}

fn is_ours(harness: Harness, group: &Value) -> bool {
    group["hooks"].as_array().is_some_and(|handlers| {
        handlers
            .iter()
            .any(|handler| harness.owns(&handler["command"]))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// An empty home directory for one test, removed when the test ends.
    pub struct Home(PathBuf);

    impl Home {
        pub fn new(test: &str) -> Home {
            let home = std::env::temp_dir().join(format!("leds-{test}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&home);
            fs::create_dir_all(&home).unwrap();
            Home(home)
        }

        /// Writes a file in the home directory, with the folders it sits in.
        pub fn write(&self, file: &str, text: &str) {
            let path = self.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        pub fn json(&self, file: &str) -> Value {
            serde_json::from_str(&fs::read_to_string(self.join(file)).unwrap()).unwrap()
        }
    }

    impl std::ops::Deref for Home {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub fn update(harness: Harness, payload: Value) -> Option<Update> {
        match report(harness, &payload)? {
            Request::Report { update, .. } => Some(update),
            _ => None,
        }
    }

    #[test]
    fn a_message_asks_only_when_its_last_line_does() {
        assert!(!ends_on_question("Done. All tests pass."));
        assert!(!ends_on_question(
            "Why did it fail? The cache was stale.\n\nFixed."
        ));
        assert!(ends_on_question(
            "I found two options.\n\n**Which should I use?**\n"
        ));
        assert!(!ends_on_question(""));
    }

    #[test]
    fn labels_a_session_by_its_folder() {
        let stop =
            json!({ "session_id": "s1", "cwd": "/Users/me/Code/app", "hook_event_name": "Stop" });
        let Some(Request::Report {
            harness,
            session,
            label,
            ..
        }) = report(Harness::Claude, &stop)
        else {
            panic!()
        };
        assert_eq!(harness, Harness::Claude);
        assert_eq!((session.as_str(), label.as_str()), ("s1", "app"));
    }

    #[test]
    fn only_harnesses_that_have_been_used_are_registered() {
        let home = Home::new("used");
        assert!(install(&home, Path::new("/bin/leds")).is_empty());
        fs::create_dir(home.join(".claude")).unwrap();
        let installed = install(&home, Path::new("/bin/leds"));
        assert!(matches!(installed[..], [(Harness::Claude, Ok(()))]));
        assert!(home.join(".claude/settings.json").exists());
    }
}
