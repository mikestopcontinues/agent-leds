//! The agent harnesses the daemon follows: registering our hooks with each,
//! what each declares it tells, and, until a harness's own files are read,
//! what its hook events mean.
//!
//! The harnesses took their hook vocabulary from Claude Code, so one mapping
//! serves them all. Each harness module turns its own payloads into that
//! vocabulary and knows where the harness keeps its hooks.

mod claude;
mod codex;
mod cursor;
mod grok;
mod junie;
mod opencode;
mod pi;

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::coverage::{Declared, Evidence};
use crate::files;
use crate::fsm::{Channel, Signal};
use crate::source;

/// An agent harness whose sessions the queue shows.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
    Grok,
    Cursor,
    Junie,
    #[value(name = "opencode")]
    OpenCode,
    Pi,
}

impl Harness {
    pub const ALL: [Harness; 7] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Grok,
        Harness::Cursor,
        Harness::Junie,
        Harness::OpenCode,
        Harness::Pi,
    ];

    /// What the harness is called on the command line and in the queue.
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
            Harness::Cursor => "cursor",
            Harness::Junie => "junie",
            Harness::OpenCode => "opencode",
            Harness::Pi => "pi",
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
            Harness::OpenCode => "OpenCode",
            Harness::Pi => "Pi",
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
            Harness::OpenCode => ".config/opencode",
            Harness::Pi => ".pi",
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
            // Read by sources of their own.
            Harness::Claude | Harness::Cursor => None,
            Harness::Codex => codex::event(payload),
            Harness::Grok => grok::event(payload),
            Harness::Junie => junie::event(payload),
            Harness::OpenCode => opencode::event(payload),
            Harness::Pi => pi::event(payload),
        }
    }

    /// The events an install registers the hook for, and the only ones.
    pub fn events(self) -> Vec<&'static str> {
        match self {
            Harness::Claude => claude::HOOKS.to_vec(),
            Harness::Codex => codex::HOOKS.to_vec(),
            Harness::Grok => grok::HOOKS.map(|(event, _)| event).to_vec(),
            Harness::Cursor => cursor::HOOKS.to_vec(),
            Harness::Junie => junie::HOOKS.to_vec(),
            // These load a plugin of ours, which `check` looks at instead.
            Harness::OpenCode | Harness::Pi => Vec::new(),
        }
    }

    /// Every hook command registered with the harness, with its event.
    fn hooks(self, home: &Path) -> Result<Vec<(String, String)>> {
        match self {
            Harness::Claude => claude::hooks(home),
            Harness::Codex => codex::hooks(home),
            Harness::Grok => grok::hooks(home),
            Harness::Cursor => cursor::hooks(home),
            Harness::Junie => junie::hooks(home),
            Harness::OpenCode | Harness::Pi => Ok(Vec::new()),
        }
    }

    /// What is wrong with the hooks registered with the harness: whether
    /// our hooks are exactly the ones an install from `exe` writes, for
    /// exactly the events it writes them for, with nothing of SidePulse's
    /// own software beside them.
    fn check(self, home: &Path, exe: &Path) -> Result<()> {
        match self {
            Harness::OpenCode => return opencode::PLUGIN.check(home, exe),
            Harness::Pi => return pi::PLUGIN.check(home, exe),
            _ => {}
        }
        let hooks = self.hooks(home)?;
        let ours: Vec<&(String, String)> =
            hooks.iter().filter(|(_, hook)| self.owns(hook)).collect();
        let mut problems = Vec::new();
        let events = self.events();
        let missing: Vec<&str> = events
            .iter()
            .copied()
            .filter(|event| !ours.iter().any(|(registered, _)| registered == event))
            .collect();
        let mut leftover: Vec<&str> = ours
            .iter()
            .map(|(event, _)| event.as_str())
            .filter(|event| !events.contains(event))
            .collect();
        leftover.dedup();
        if ours.is_empty() && !events.is_empty() {
            problems.push("not registered; run `agent-leds install`".to_owned());
        } else if !missing.is_empty() {
            problems.push(format!(
                "no hook for {}; run `agent-leds install`",
                missing.join(", ")
            ));
        }
        if !leftover.is_empty() {
            problems.push(format!(
                "a hook this version does not use is registered for {}; run `agent-leds install`",
                leftover.join(", ")
            ));
        }
        let command = self.command(exe);
        let mut others: Vec<String> = ours
            .iter()
            .filter(|(_, hook)| *hook != command)
            .filter_map(|(_, hook)| shlex::split(hook)?.into_iter().next())
            .collect();
        others.sort();
        others.dedup();
        if !others.is_empty() {
            problems.push(format!(
                "registered for {}, not this binary; run `agent-leds install`",
                others.join(" and ")
            ));
        }
        let mut theirs: Vec<&str> = hooks
            .iter()
            .filter(|(_, hook)| hook.to_lowercase().contains("sidepulse"))
            .map(|(event, _)| event.as_str())
            .collect();
        theirs.dedup();
        if !theirs.is_empty() {
            problems.push(format!(
                "SidePulse's own hook is registered for {}; remove it from ~/{}",
                theirs.join(", "),
                self.file()
            ));
        }
        if self == Harness::Codex
            && problems.is_empty()
            && let Err(error) = codex::trusted(home)
        {
            problems.push(format!("{error:#}"));
        }
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        Ok(())
    }

    /// What `agent-leds doctor` says of a harness whose hooks are right.
    pub fn registered(self) -> &'static str {
        match self {
            Harness::Claude => "read from its own files, with no hooks",
            Harness::OpenCode | Harness::Pi => "plugin installed",
            _ => "hooks registered",
        }
    }

    /// The file in the home directory that holds the harness's hooks.
    fn file(self) -> &'static str {
        match self {
            Harness::Claude => claude::SETTINGS,
            Harness::Codex => codex::CONFIG,
            Harness::Grok => grok::FILE,
            Harness::Cursor => cursor::FILE,
            Harness::Junie => junie::CONFIG,
            Harness::OpenCode => opencode::PLUGIN.path,
            Harness::Pi => pi::PLUGIN.path,
        }
    }

    fn install(self, home: &Path, exe: &Path) -> Result<()> {
        match self {
            Harness::Claude => claude::install(home, exe),
            Harness::Codex => codex::install(home, exe),
            Harness::Grok => grok::install(home, exe),
            Harness::Cursor => cursor::install(home, exe),
            Harness::Junie => junie::install(home, exe),
            Harness::OpenCode => opencode::PLUGIN.install(home, exe),
            Harness::Pi => pi::PLUGIN.install(home, exe),
        }
    }

    fn uninstall(self, home: &Path) -> Result<()> {
        match self {
            Harness::Claude => claude::uninstall(home),
            Harness::Codex => codex::uninstall(home),
            Harness::Grok => grok::uninstall(home),
            Harness::Cursor => cursor::uninstall(home),
            Harness::Junie => junie::uninstall(home),
            Harness::OpenCode => opencode::PLUGIN.uninstall(home),
            Harness::Pi => pi::PLUGIN.uninstall(home),
        }
    }
}

/// What a harness's files and hooks tell the daemon, which `coverage`
/// crosses with the table.
pub fn signals(harness: Harness) -> &'static [Declared] {
    match harness {
        Harness::Claude => source::claude::SIGNALS,
        Harness::Codex => codex::SIGNALS,
        Harness::Grok => grok::SIGNALS,
        Harness::Cursor => source::cursor::SIGNALS,
        Harness::Junie => junie::SIGNALS,
        Harness::OpenCode => opencode::SIGNALS,
        Harness::Pi => pi::SIGNALS,
    }
}

/// Whether a harness tells `signal` through `channel`.
pub fn declares(harness: Harness, signal: Signal, channel: Channel) -> bool {
    signals(harness)
        .iter()
        .any(|declared| declared.signal == signal && declared.channel == channel)
}

/// The harnesses that have a folder in `home`.
fn used(home: &Path) -> impl Iterator<Item = Harness> {
    Harness::ALL
        .into_iter()
        .filter(|harness| home.join(harness.folder()).is_dir())
}

/// Registers the hook with every harness in use, replacing any earlier
/// registration. One harness failing does not stop the rest.
pub fn install(home: &Path, exe: &Path) -> Vec<(Harness, Result<()>)> {
    used(home)
        .map(|harness| (harness, harness.install(home, exe)))
        .collect()
}

/// Checks the hooks registered with every harness in use against what an
/// install from `exe` would write.
pub fn check(home: &Path, exe: &Path) -> Vec<(Harness, Result<()>)> {
    used(home)
        .map(|harness| (harness, harness.check(home, exe)))
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
    /// Whether the event ends the turn and says the user cut it short.
    interrupted: bool,
    /// Whether the event ends the turn and says it ended on an error.
    failed: bool,
    /// Whether something other than the main agent sent the event.
    subagent: bool,
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
    /// Codex sends null for a session it keeps no record of.
    #[serde(default, deserialize_with = "present")]
    transcript_path: Option<Option<String>>,
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
            interrupted: false,
            failed: false,
            subagent: payload.agent_id.is_some(),
        })
    }

    /// What the event says of its session, each fact a signal of the table,
    /// whose order decides what they come to.
    fn signals(&self) -> BTreeSet<Signal> {
        if self.subagent {
            // Only the main agent's events say what the session needs. A
            // subagent's say that work is going on, which matters once the
            // main agent has stopped and left a subagent running in the
            // background.
            return BTreeSet::from([Signal::Backgrounded]);
        }
        let mut signals = BTreeSet::from([match self.hook {
            Hook::UserPromptSubmit
            | Hook::PreToolUse
            | Hook::PostToolUse
            | Hook::PostToolUseFailure
            | Hook::ElicitationResult => Signal::Prompted,
            Hook::PermissionRequest => Signal::PermissionAsked,
            Hook::Elicitation => Signal::QuestionAsked,
            Hook::Stop => Signal::TurnEnded,
            Hook::StopFailure => Signal::TurnFailed,
            Hook::Interrupt => Signal::Interrupted,
            Hook::SessionEnd => Signal::Ended,
        }]);
        // A tool that asks the user runs, or the turn ends on a message that asks.
        match self.hook {
            Hook::PreToolUse if self.asks => {
                signals.insert(Signal::QuestionAsked);
            }
            Hook::Stop if self.asks => {
                signals.insert(Signal::TurnEndedAsking);
            }
            _ => {}
        }
        if self.interrupted {
            signals.insert(Signal::Interrupted);
        }
        if self.failed {
            signals.insert(Signal::TurnFailed);
        }
        signals
    }
}

/// Translates one hook payload while the harness's own files are not read:
/// the session it names, the folder that session works in, and what the
/// event means. Junie names the session only when a prompt is submitted, so
/// the session its last prompt named is kept in `state` between events.
pub fn interim(
    harness: Harness,
    payload: &Value,
    state: &Path,
) -> Option<(String, String, BTreeSet<Signal>)> {
    let payload = match harness {
        Harness::Junie => junie::with_session(payload, state)?,
        _ => Cow::Borrowed(payload),
    };
    let event = harness.event(&payload)?;
    let signals = event.signals();
    Some((event.session, event.cwd, signals))
}

/// A signal one of the harnesses whose files are not read yet tells through
/// its hooks.
const fn hooked(
    signal: Signal,
    evidence: Evidence,
    record: &'static str,
    cite: &'static str,
) -> Declared {
    Declared {
        signal,
        channel: Channel::Hook,
        evidence,
        record,
        cite,
    }
}

/// Whether a final message ends by asking the user something.
pub fn ends_on_question(message: &str) -> bool {
    let last = message
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    last.trim_end_matches(|c: char| c.is_whitespace() || "*_`\"')".contains(c))
        .ends_with('?')
}

/// A plugin a harness loads by itself from a file of its own, in place of
/// hook commands. The plugin passes the harness's events to the hook, at
/// the binary whose path an install writes into it.
pub struct Plugin {
    /// Where the harness looks for it, from the home directory.
    pub path: &'static str,
    source: &'static str,
}

impl Plugin {
    /// The plugin, set to run `exe`.
    fn text(&self, exe: &Path) -> String {
        let exe = serde_json::to_string(&exe.to_string_lossy()).expect("a string is JSON");
        self.source.replacen("\"__AGENT_LEDS__\"", &exe, 1)
    }

    fn install(&self, home: &Path, exe: &Path) -> Result<()> {
        files::replace(&home.join(self.path), &self.text(exe))
    }

    fn uninstall(&self, home: &Path) -> Result<()> {
        files::remove(&home.join(self.path))
    }

    /// Whether the plugin is the one an install from `exe` writes.
    fn check(&self, home: &Path, exe: &Path) -> Result<()> {
        match files::read_optional(&home.join(self.path))? {
            None => bail!("not registered; run `agent-leds install`"),
            Some(text) if text == self.text(exe) => Ok(()),
            Some(_) => bail!(
                "~/{} is from another agent-leds binary or version; run `agent-leds install`",
                self.path
            ),
        }
    }
}

/// The `hooks` object of a JSON settings file, empty when the file is not there.
fn read_hooks(path: &Path) -> Result<Map<String, Value>> {
    let Some(text) = files::read_optional(path)? else {
        return Ok(Map::new());
    };
    let settings: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    match settings.get("hooks") {
        None => Ok(Map::new()),
        Some(Value::Object(hooks)) => Ok(hooks.clone()),
        Some(_) => bail!("hooks in {} is not an object", path.display()),
    }
}

/// Every command in a `hooks` object, with its event. Hooks in groups list
/// groups of handlers under each event, as Claude Code's do; the others
/// list the handlers themselves.
fn commands(hooks: &Map<String, Value>, grouped: bool) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for (event, entries) in hooks {
        for entry in entries.as_array().into_iter().flatten() {
            let handlers = if grouped {
                entry["hooks"].as_array().map_or(&[][..], Vec::as_slice)
            } else {
                std::slice::from_ref(entry)
            };
            for handler in handlers {
                if let Some(command) = handler["command"].as_str() {
                    found.push((event.clone(), command.to_owned()));
                }
            }
        }
    }
    found
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

/// Registers `handler` for each of `events` and for no other, in the shape
/// Claude Code gave the harnesses: each event lists groups, and each group
/// lists handlers. A hook of ours under any other event, as an older version
/// registered, goes.
fn register_groups(
    hooks: &mut Map<String, Value>,
    harness: Harness,
    events: &[&str],
    handler: impl Fn(&str) -> Value,
) -> Result<()> {
    unregister(hooks, |group| owns_group(harness, group));
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

    /// What a payload says, as the daemon reads it.
    pub fn signals(harness: Harness, payload: Value) -> Option<Vec<Signal>> {
        Some(harness.event(&payload)?.signals().into_iter().collect())
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
        fs::create_dir(home.join(".junie")).unwrap();
        let installed = install(&home, Path::new("/bin/agent-leds"));
        assert!(matches!(installed[..], [(Harness::Junie, Ok(()))]));
        assert!(home.join(junie::CONFIG).exists());
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

    #[test]
    fn a_check_says_what_an_install_would_change() {
        let home = Home::new();
        let exe = Path::new("/bin/agent-leds");
        let check = |exe: &str| {
            Harness::Junie
                .check(&home, Path::new(exe))
                .map_err(|error| error.to_string())
        };
        assert!(
            check("/bin/agent-leds")
                .unwrap_err()
                .starts_with("not registered")
        );
        junie::install(&home, exe).unwrap();
        check("/bin/agent-leds").unwrap();
        // The binary moved, or a second one is being run.
        assert!(
            check("/opt/agent-leds")
                .unwrap_err()
                .starts_with("registered for /bin/agent-leds, not this binary")
        );
        edit_hooks(&home.join(junie::CONFIG), |hooks| {
            let stop = hooks.remove("Stop").unwrap();
            // 0.5.0 also registered PreToolUse.
            hooks.insert("PreToolUse".into(), stop);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            check("/bin/agent-leds").unwrap_err(),
            "no hook for Stop; run `agent-leds install`; a hook this version does not use is \
             registered for PreToolUse; run `agent-leds install`"
        );
        junie::install(&home, exe).unwrap();
        check("/bin/agent-leds").unwrap();
        assert!(
            home.json(junie::CONFIG)["hooks"]
                .get("PreToolUse")
                .is_none()
        );
    }

    #[test]
    fn a_check_finds_sidepulse_s_own_hooks() {
        let home = Home::new();
        let exe = Path::new("/bin/agent-leds");
        home.write(
            cursor::FILE,
            r#"{ "version": 1, "hooks": { "stop": [{ "command": "~/.local/share/sidepulse/venv/bin/python -m sidepulse.hook_entry --provider cursor" }] } }"#,
        );
        cursor::install(&home, exe).unwrap();
        let error = Harness::Cursor.check(&home, exe).unwrap_err().to_string();
        assert_eq!(
            error,
            "SidePulse's own hook is registered for stop; remove it from ~/.cursor/hooks.json"
        );
    }

    #[test]
    fn a_plugin_runs_the_binary_that_installed_it() {
        let home = Home::new();
        let exe = Path::new("/Users/me/My \"Tools\"/agent-leds");
        assert!(pi::PLUGIN.check(&home, exe).is_err());
        pi::PLUGIN.install(&home, exe).unwrap();
        let text = fs::read_to_string(home.join(pi::PLUGIN.path)).unwrap();
        assert!(text.contains(r#"const AGENT_LEDS = "/Users/me/My \"Tools\"/agent-leds";"#));
        pi::PLUGIN.check(&home, exe).unwrap();
        let moved = pi::PLUGIN.check(&home, Path::new("/opt/agent-leds"));
        assert!(
            moved
                .unwrap_err()
                .to_string()
                .contains("another agent-leds binary")
        );
        pi::PLUGIN.uninstall(&home).unwrap();
        assert!(!home.join(pi::PLUGIN.path).exists());
        // Both plugins hold the place the path goes.
        assert!(opencode::PLUGIN.text(exe).contains("My \\\"Tools"));
    }
}
