//! The agent harnesses the daemon follows: registering our hooks with each,
//! and what each declares it tells. Each harness module knows where the
//! harness keeps its hooks.

mod claude;
mod codex;
mod cursor;
mod grok;
mod junie;
mod opencode;
mod pi;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::coverage::Declared;
use crate::files;
use crate::fsm::{Channel, Signal};
use crate::launchd;
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

    /// The events an install registers the hook for, and the only ones. For
    /// OpenCode, the events its plugin passes on, by the names it gives them.
    pub fn events(self) -> Vec<&'static str> {
        match self {
            Harness::Claude => claude::HOOKS.to_vec(),
            Harness::Codex => codex::HOOKS.to_vec(),
            Harness::Grok => grok::HOOKS.to_vec(),
            Harness::Cursor => cursor::HOOKS.to_vec(),
            Harness::Junie => junie::HOOKS.to_vec(),
            Harness::OpenCode => opencode::HOOKS.to_vec(),
            // An extension of ours passes these on, and `check` looks at it.
            Harness::Pi => pi::HOOKS.to_vec(),
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
            Harness::OpenCode => return opencode::check(home, exe),
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
        if self == Harness::Grok
            && let Err(error) = grok::claude_hooks(home)
        {
            problems.push(format!("{error:#}"));
        }
        if !problems.is_empty() {
            bail!("{}", problems.join("; "));
        }
        Ok(())
    }

    /// What `agent-leds doctor` says of a harness whose hooks are right,
    /// after what its files show.
    pub fn registered(self) -> &'static str {
        match self {
            Harness::Claude | Harness::Grok | Harness::Junie => "no hooks",
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
        Harness::Codex => source::codex::SIGNALS,
        Harness::Grok => source::grok::SIGNALS,
        Harness::Cursor => source::cursor::SIGNALS,
        Harness::Junie => source::junie::SIGNALS,
        Harness::OpenCode => source::opencode::SIGNALS,
        Harness::Pi => source::pi::SIGNALS,
    }
}

/// Whether a harness tells `signal` through `channel`.
pub fn declares(harness: Harness, signal: Signal, channel: Channel) -> bool {
    signals(harness)
        .iter()
        .any(|declared| declared.signal == signal && declared.channel == channel)
}

/// Whether the harness has its folder in `home`.
pub fn at_home(home: &Path, harness: Harness) -> bool {
    home.join(harness.folder()).is_dir()
}

/// Claude Code's and Junie's folders as the service finds them, from
/// launchd's environment.
pub fn service_folders(home: &Path) -> (PathBuf, PathBuf) {
    let claude = launchd::getenv("CLAUDE_CONFIG_DIR");
    let junie = launchd::getenv("JUNIE_HOME");
    (
        source::claude::folder(claude.as_deref(), home),
        source::junie::folder(junie.as_deref()),
    )
}

/// Whether a harness is in use: Claude Code's and Junie's folders where the
/// service, given `claude` and `junie`, or this shell finds them, the
/// others' in `home`.
pub fn in_use(harness: Harness, home: &Path, claude: &Path, junie: &Path) -> bool {
    match harness {
        Harness::Claude => claude.is_dir() || source::claude::config_dir(home).is_dir(),
        Harness::Junie => junie.is_dir() || source::junie::home().is_dir(),
        _ => at_home(home, harness),
    }
}

/// Registers the hook with every harness `used` says is in use, replacing
/// any earlier registration. One harness failing does not stop the rest.
pub fn install(
    home: &Path,
    exe: &Path,
    used: impl Fn(Harness) -> bool,
) -> Vec<(Harness, Result<()>)> {
    Harness::ALL
        .into_iter()
        .filter(|&harness| used(harness))
        .map(|harness| (harness, harness.install(home, exe)))
        .collect()
}

/// Checks the hooks registered with every harness `used` says is in use
/// against what an install from `exe` would write.
pub fn check(
    home: &Path,
    exe: &Path,
    used: impl Fn(Harness) -> bool,
) -> Vec<(Harness, Result<()>)> {
    Harness::ALL
        .into_iter()
        .filter(|&harness| used(harness))
        .map(|harness| (harness, harness.check(home, exe)))
        .collect()
}

/// Removes the hook from every harness and leaves everything else in place.
pub fn uninstall(home: &Path) -> Vec<(Harness, Result<()>)> {
    let all = Harness::ALL.into_iter();
    all.map(|harness| (harness, harness.uninstall(home)))
        .collect()
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
/// `hooks` object. Everything else in the file stays as it is, a `hooks`
/// that `edit` leaves empty goes only if it was not empty already, and a
/// file that `edit` leaves unchanged is not written.
fn edit_hooks(path: &Path, edit: impl FnOnce(&mut Map<String, Value>) -> Result<()>) -> Result<()> {
    let text = files::read_optional(path)?.unwrap_or_else(|| "{}".into());
    let before: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let mut settings = before.clone();
    let Some(root) = settings.as_object_mut() else {
        bail!("{} is not a JSON object", path.display())
    };
    let empty = root
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(Map::is_empty);
    let Some(hooks) = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
    else {
        bail!("hooks in {} is not an object", path.display())
    };
    edit(hooks)?;
    if hooks.is_empty() && !empty {
        root.shift_remove("hooks");
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

/// Removes the handlers an install of `harness` added to a `hooks` object,
/// laid out as `commands` reads it, with the groups and events that
/// removing them left with none, and says whether there were any. Another
/// tool's handler in a group of ours stays, with the group.
fn unregister(hooks: &mut Map<String, Value>, harness: Harness, grouped: bool) -> bool {
    let ours = |handler: &Value| {
        handler["command"]
            .as_str()
            .is_some_and(|command| harness.owns(command))
    };
    // Drops what `ours` matches from a list, and says whether that left it
    // empty where it was not.
    let prune = |list: &mut Vec<Value>, removed: &mut bool| {
        let before = list.len();
        list.retain(|entry| !ours(entry));
        *removed |= list.len() < before;
        list.len() < before && list.is_empty()
    };
    let mut removed = false;
    hooks.retain(|_, entries| {
        let Some(entries) = entries.as_array_mut() else {
            return true;
        };
        if !grouped {
            return !prune(entries, &mut removed);
        }
        let before = entries.len();
        entries.retain_mut(
            |group| match group.get_mut("hooks").and_then(Value::as_array_mut) {
                Some(handlers) => !prune(handlers, &mut removed),
                None => true,
            },
        );
        entries.len() == before || !entries.is_empty()
    });
    removed
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

    /// Claude Code and Junie are in use where their folders are, which may
    /// be outside the home folder; the others, in it.
    #[test]
    fn a_harness_is_in_use_where_its_folder_is() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let (claude, junie) = (home.join("work/claude"), home.join("work/junie"));
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(&junie).unwrap();
        assert!(in_use(Harness::Claude, home, &claude, &junie));
        assert!(in_use(Harness::Junie, home, &claude, &junie));
        assert!(!in_use(Harness::Codex, home, &claude, &junie));
        std::fs::create_dir(home.join(".codex")).unwrap();
        assert!(in_use(Harness::Codex, home, &claude, &junie));
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
    fn only_harnesses_that_have_been_used_are_registered() {
        let home = Home::new();
        let used = |harness| at_home(&home, harness);
        assert!(install(&home, Path::new("/bin/agent-leds"), used).is_empty());
        fs::create_dir(home.join(".cursor")).unwrap();
        let installed = install(&home, Path::new("/bin/agent-leds"), used);
        assert!(matches!(installed[..], [(Harness::Cursor, Ok(()))]));
        assert!(home.join(cursor::FILE).exists());
    }

    #[test]
    fn a_settings_file_holding_nothing_of_ours_is_not_rewritten() {
        let home = Home::new();
        let path = home.join("settings.json");
        let theirs = [
            "{\n    \"model\": \"opus\",\n    \"hooks\": {}\n}\n",
            r#"{"hooks":{"Stop":[]},"x":1}"#,
        ];
        for text in theirs {
            home.write("settings.json", text);
            edit_hooks(&path, |hooks| {
                unregister(hooks, Harness::Claude, true);
                Ok(())
            })
            .unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
        // An event and a `hooks` that held only ours go with them, and the
        // keys after it keep their order.
        home.write(
            "settings.json",
            r#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"command":"/bin/agent-leds hook claude"}]}]},"permissions":{},"theme":"dark"}"#,
        );
        edit_hooks(&path, |hooks| {
            unregister(hooks, Harness::Claude, true);
            Ok(())
        })
        .unwrap();
        let left = home.json("settings.json");
        let keys: Vec<&String> = left.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["model", "permissions", "theme"]);
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
            Harness::Cursor
                .check(&home, Path::new(exe))
                .map_err(|error| error.to_string())
        };
        assert!(
            check("/bin/agent-leds")
                .unwrap_err()
                .starts_with("not registered")
        );
        cursor::install(&home, exe).unwrap();
        check("/bin/agent-leds").unwrap();
        // The binary moved, or a second one is being run.
        assert!(
            check("/opt/agent-leds")
                .unwrap_err()
                .starts_with("registered for /bin/agent-leds, not this binary")
        );
        let kept = cursor::HOOKS[0];
        edit_hooks(&home.join(cursor::FILE), |hooks| {
            let entries = hooks.remove(kept).unwrap();
            // An older version also registered preToolUse.
            hooks.insert("preToolUse".into(), entries);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            check("/bin/agent-leds").unwrap_err(),
            format!(
                "no hook for {kept}; run `agent-leds install`; a hook this version does not use is \
                 registered for preToolUse; run `agent-leds install`"
            )
        );
        cursor::install(&home, exe).unwrap();
        check("/bin/agent-leds").unwrap();
        assert!(home.json(cursor::FILE)["hooks"].get("preToolUse").is_none());
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
