//! The agent harnesses that report to the daemon: what their hook events
//! mean, and registering the hook with each.
//!
//! The harnesses took their hook vocabulary from Claude Code, so one mapping
//! serves them all. Each harness module turns its own payloads into that
//! vocabulary and knows where the harness keeps its hooks.

mod claude;
mod codex;
mod cursor;
mod grok;
mod junie;

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::state_dir;
use crate::files;
use crate::state::{State, Update};

/// An agent harness whose sessions the queue shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
    Grok,
    Cursor,
    Junie,
}

impl Harness {
    pub const ALL: [Harness; 5] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Grok,
        Harness::Cursor,
        Harness::Junie,
    ];

    /// What the harness is called on the command line and in the queue.
    fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
            Harness::Cursor => "cursor",
            Harness::Junie => "junie",
        }
    }

    /// What the harness is called in a sentence.
    pub fn title(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
            Harness::Grok => "Grok",
            Harness::Cursor => "Cursor",
            Harness::Junie => "Junie",
        }
    }

    /// The harness's folder in the home directory, which exists once the
    /// harness has been used.
    fn folder(self) -> &'static str {
        match self {
            Harness::Claude => ".claude",
            Harness::Codex => ".codex",
            Harness::Grok => ".grok",
            Harness::Cursor => ".cursor",
            Harness::Junie => ".junie",
        }
    }

    /// The command the harness runs on each hook event, through a shell.
    fn command(self, exe: &Path) -> String {
        let exe = exe.to_string_lossy();
        shlex::try_join([exe.as_ref(), "hook", self.name()]).expect("a path holds no NUL")
    }

    /// Whether a hook command is one an install of this harness wrote,
    /// wherever the binary was then.
    fn owns(self, command: &str) -> bool {
        match shlex::split(command).as_deref() {
            Some([exe, hook, name]) => {
                Path::new(exe).file_name() == Some(env!("CARGO_BIN_NAME").as_ref())
                    && hook == "hook"
                    && name == self.name()
            }
            _ => false,
        }
    }

    fn event(self, payload: &Value) -> Option<Event> {
        match self {
            Harness::Claude => claude::event(payload),
            Harness::Codex => codex::event(payload),
            Harness::Grok => grok::event(payload),
            Harness::Cursor => cursor::event(payload),
            Harness::Junie => junie::event(payload),
        }
    }

    fn install(self, home: &Path, exe: &Path) -> Result<()> {
        match self {
            Harness::Claude => claude::install(home, exe),
            Harness::Codex => codex::install(home, exe),
            Harness::Grok => grok::install(home, exe),
            Harness::Cursor => cursor::install(home, exe),
            Harness::Junie => junie::install(home, exe),
        }
    }

    fn uninstall(self, home: &Path) -> Result<()> {
        match self {
            Harness::Claude => claude::uninstall(home),
            Harness::Codex => codex::uninstall(home),
            Harness::Grok => grok::uninstall(home),
            Harness::Cursor => cursor::uninstall(home),
            Harness::Junie => junie::uninstall(home),
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

/// A hook event as Claude Code names it, which is the vocabulary the
/// harnesses share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
enum Hook {
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    PostToolUseFailure,
    Elicitation,
    ElicitationResult,
    Stop,
    StopFailure,
    SessionEnd,
    /// The user cut the turn short. Codex has an event of this name; the
    /// other harnesses say so in other ways.
    Interrupt,
}

impl Hook {
    /// The event a name stands for, if it is one the daemon follows.
    fn named(name: &str) -> Option<Hook> {
        serde_json::from_value(Value::from(name)).ok()
    }
}

/// One hook event in the vocabulary the harnesses share.
struct Event {
    hook: Hook,
    session: String,
    /// The folder the session works in.
    cwd: String,
    /// Whether the event puts a question to the user: it runs a tool that
    /// asks, or it ends the turn on a message that does.
    asks: bool,
    /// Whether something other than the main agent sent the event.
    subagent: bool,
    /// Whether the turn ended with work of the session's still running in
    /// the background, which wakes the session when it finishes.
    unfinished: bool,
}

/// A hook payload as Claude Code spells it, which Codex and Junie follow.
/// Each harness reads the fields it sends.
#[derive(Deserialize)]
struct Payload {
    hook_event_name: String,
    /// Junie names the session only when a prompt is submitted.
    session_id: Option<String>,
    #[serde(default, deserialize_with = "or_empty")]
    cwd: String,
    #[serde(default, deserialize_with = "or_empty")]
    last_assistant_message: String,
    /// Set when a subagent sent the event.
    agent_id: Option<String>,
    tool_name: Option<String>,
    /// Whether a failed tool was cut short by the user.
    #[serde(default)]
    is_interrupt: bool,
    #[serde(default)]
    background_tasks: Vec<Task>,
    /// Codex sends null for a session it keeps no record of.
    #[serde(default, deserialize_with = "present")]
    transcript_path: Option<Option<String>>,
}

/// Work a Claude Code session runs in the background.
#[derive(Deserialize)]
struct Task {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    status: String,
}

/// Reads text that may be null as empty.
fn or_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Option::deserialize(deserializer).map(Option::unwrap_or_default)
}

/// Reads a field that may be null, so that a null tells apart from a field left out.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer).map(Some)
}

impl Payload {
    fn read(payload: &Value) -> Option<Payload> {
        Payload::deserialize(payload).ok()
    }
}

impl Event {
    /// The event a payload spelled the way Claude Code spells them stands for.
    fn read(payload: &Payload) -> Option<Event> {
        Some(Event {
            hook: Hook::named(&payload.hook_event_name)?,
            session: payload.session_id.clone()?,
            cwd: payload.cwd.clone(),
            asks: ends_on_question(&payload.last_assistant_message),
            subagent: payload.agent_id.is_some(),
            unfinished: false,
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
        Some(match self.hook {
            Hook::UserPromptSubmit
            | Hook::PostToolUse
            | Hook::PostToolUseFailure
            | Hook::ElicitationResult => Update::Enter(State::Working),
            Hook::PreToolUse if self.asks => Update::Enter(State::Question),
            Hook::PreToolUse => Update::Enter(State::Working),
            Hook::PermissionRequest | Hook::Elicitation => Update::Enter(State::Question),
            Hook::Stop if self.asks => Update::Asked,
            // The session goes on by itself, so there is nothing to come back to yet.
            Hook::Stop if self.unfinished => Update::Enter(State::Working),
            Hook::Stop => Update::Enter(State::Stopped),
            Hook::StopFailure => Update::Enter(State::Error),
            Hook::SessionEnd | Hook::Interrupt => Update::Leave,
        })
    }
}

/// What one hook event says about a session, as the hook tells the daemon.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub harness: Harness,
    pub session: String,
    /// What the queue calls the session: the name of its folder.
    pub label: String,
    pub update: Update,
}

/// Translates one hook payload from a harness into a report for the daemon,
/// as the hook does. Junie names the session only when a prompt is
/// submitted, so the hook keeps that between events.
pub fn hook(harness: Harness, payload: &Value) -> Option<Report> {
    if harness == Harness::Junie {
        return report(harness, &*junie::with_session(payload, &state_dir())?);
    }
    report(harness, payload)
}

/// Translates one hook payload from a harness into a report for the daemon.
fn report(harness: Harness, payload: &Value) -> Option<Report> {
    let event = harness.event(payload)?;
    let update = event.update()?;
    let label = Path::new(&event.cwd)
        .file_name()
        .map_or(event.cwd.as_str().into(), |name| name.to_string_lossy())
        .into_owned();
    Some(Report {
        harness,
        session: event.session,
        label,
        update,
    })
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
    let text = files::read_optional(path)?.unwrap_or_else(|| "{}".into());
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
    files::replace(path, &(serde_json::to_string_pretty(&settings)? + "\n"))
}

/// Adds `entry` to the list `hooks` holds for `event`, in place of the entry
/// an earlier install added there.
fn register(
    hooks: &mut Map<String, Value>,
    event: &str,
    entry: Value,
    ours: impl Fn(&Value) -> bool,
) -> Result<()> {
    let entries = hooks.entry(event).or_insert_with(|| json!([]));
    let Some(entries) = entries.as_array_mut() else {
        bail!("hooks.{event} is not a list")
    };
    entries.retain(|entry| !ours(entry));
    entries.push(entry);
    Ok(())
}

/// Removes the entries an install added to a `hooks` object, and the events
/// left with none.
fn unregister(hooks: &mut Map<String, Value>, ours: impl Fn(&Value) -> bool) {
    for entries in hooks.values_mut() {
        if let Some(entries) = entries.as_array_mut() {
            entries.retain(|entry| !ours(entry));
        }
    }
    hooks.retain(|_, entries| entries.as_array().is_none_or(|entries| !entries.is_empty()));
}

/// Registers `handler` for each of `events`, in the shape Claude Code gave
/// the harnesses: each event lists groups, and each group lists handlers.
fn register_groups(
    hooks: &mut Map<String, Value>,
    harness: Harness,
    events: &[&str],
    handler: impl Fn(&str) -> Value,
) -> Result<()> {
    for event in events {
        let group = json!({ "hooks": [handler(event)] });
        register(hooks, event, group, |group| owns_group(harness, group))?;
    }
    Ok(())
}

/// Whether a group of handlers holds one an install of `harness` added.
fn owns_group(harness: Harness, group: &Value) -> bool {
    group["hooks"].as_array().is_some_and(|handlers| {
        handlers.iter().any(|handler| {
            handler["command"]
                .as_str()
                .is_some_and(|command| harness.owns(command))
        })
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// An empty home directory for one test, removed when the test ends.
    pub struct Home(tempfile::TempDir);

    impl Home {
        pub fn new() -> Home {
            Home(tempfile::tempdir().unwrap())
        }

        /// Writes a file in the home directory, with the folders it sits in.
        pub fn write(&self, file: &str, text: &str) {
            files::replace(&self.join(file), text).unwrap();
        }

        pub fn json(&self, file: &str) -> Value {
            serde_json::from_str(&fs::read_to_string(self.join(file)).unwrap()).unwrap()
        }
    }

    impl std::ops::Deref for Home {
        type Target = Path;

        fn deref(&self) -> &Path {
            self.0.path()
        }
    }

    pub fn update(harness: Harness, payload: Value) -> Option<Update> {
        report(harness, &payload).map(|report| report.update)
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
        let report = report(Harness::Claude, &stop).unwrap();
        assert_eq!(
            report,
            Report {
                harness: Harness::Claude,
                session: "s1".into(),
                label: "app".into(),
                update: Update::Enter(State::Stopped),
            }
        );
    }

    #[test]
    fn a_hook_command_survives_a_path_with_spaces() {
        let command = Harness::Claude.command(Path::new("/Users/me/My Tools/agent-leds"));
        assert_eq!(command, "'/Users/me/My Tools/agent-leds' hook claude");
        assert!(Harness::Claude.owns(&command));
        assert!(Harness::Claude.owns("/opt/homebrew/bin/agent-leds hook claude"));
        assert!(!Harness::Codex.owns(&command));
        assert!(!Harness::Claude.owns("/usr/local/bin/other hook claude"));
        assert!(!Harness::Claude.owns("agent-leds hook claude --verbose"));
        assert!(!Harness::Claude.owns("'unclosed"));
    }

    #[test]
    fn only_events_the_daemon_follows_are_named() {
        assert_eq!(Hook::named("Stop"), Some(Hook::Stop));
        assert_eq!(Hook::named("Interrupt"), Some(Hook::Interrupt));
        // Grok runs the hooks in ~/.claude/settings.json with its own payloads,
        // whose events are spelled in snake_case.
        assert_eq!(Hook::named("stop"), None);
        assert_eq!(Hook::named("Notification"), None);
    }

    #[test]
    fn only_harnesses_that_have_been_used_are_registered() {
        let home = Home::new();
        assert!(install(&home, Path::new("/bin/agent-leds")).is_empty());
        fs::create_dir(home.join(".claude")).unwrap();
        let installed = install(&home, Path::new("/bin/agent-leds"));
        assert!(matches!(installed[..], [(Harness::Claude, Ok(()))]));
        assert!(home.join(".claude/settings.json").exists());
    }

    #[test]
    fn a_settings_file_that_is_not_an_object_is_left_alone() {
        let home = Home::new();
        home.write("settings.json", "[1]");
        let error = edit_hooks(&home.join("settings.json"), |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("not a JSON object"));
        home.write("settings.json", r#"{"hooks": 3}"#);
        assert!(edit_hooks(&home.join("settings.json"), |_| Ok(())).is_err());
        assert_eq!(
            fs::read_to_string(home.join("settings.json")).unwrap(),
            r#"{"hooks": 3}"#
        );
    }
}
