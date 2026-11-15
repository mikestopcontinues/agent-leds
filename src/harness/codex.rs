//! Codex: its hooks live in `~/.codex/config.toml`, and it runs only the
//! hooks whose hash that file says to trust. Its sessions are read from its
//! files, `source::codex`; the one hook says what no file records.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};

use super::Harness;
use crate::files;

pub const CONFIG: &str = ".codex/config.toml";

/// The hook events the daemon needs: only an approval that waits, which
/// Codex writes nowhere until it is answered.
pub const HOOKS: [&str; 1] = ["PermissionRequest"];

/// How long Codex gets to say which hooks it sees.
const ANSWER: Duration = Duration::from_secs(15);

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let config = home.join(CONFIG);
    let command = Harness::Codex.command(exe);
    let path = config.to_string_lossy().into_owned();
    edit(&config, |config| register(config, &path, &command))?;
    let hashes = hashes(home).context(
        "the hooks are in config.toml, but Codex will not run them until it is told to trust \
         them; run `agent-leds install` again once `codex` works",
    )?;
    edit(&config, |config| trust(config, &hashes))
}

pub fn uninstall(home: &Path) -> Result<()> {
    let config = home.join(CONFIG);
    let mut removed = false;
    edit(&config, |document| {
        removed = unregister(document, &config.to_string_lossy(), &[]);
        Ok(())
    })?;
    // Nothing is left of a config the install created.
    if removed && read(&config)?.to_string().trim().is_empty() {
        files::remove(&config)?;
    }
    Ok(())
}

/// Rewrites the config through `edit`, leaving every line `edit` does not
/// touch as it is. A config that `edit` leaves unchanged is not written.
fn edit(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let mut config = read(path)?;
    let before = config.to_string();
    edit(&mut config)?;
    let after = config.to_string();
    if after == before {
        return Ok(());
    }
    files::replace(path, &after)
}

/// A table that prints only through the tables inside it, so that removing
/// those leaves no empty header behind.
fn implicit() -> Item {
    let mut table = Table::new();
    table.set_implicit(true);
    Item::Table(table)
}

fn hooks_table(config: &mut DocumentMut) -> Result<&mut Table> {
    let hooks = config.entry("hooks").or_insert_with(implicit);
    hooks.as_table_mut().context("hooks is not a table")
}

/// The commands of a group's handlers, which are written inline or as tables.
fn group_commands(group: &Table) -> Vec<&str> {
    match group.get("hooks") {
        Some(Item::Value(handlers)) => handlers
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|handler| handler.as_inline_table()?.get("command")?.as_str())
            .collect(),
        Some(Item::ArrayOfTables(handlers)) => handlers
            .iter()
            .filter_map(|handler| handler.get("command")?.as_str())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a group of handlers is the one an install wrote.
fn is_ours(group: &Table) -> bool {
    group_commands(group)
        .into_iter()
        .any(|command| Harness::Codex.owns(command))
}

/// The config, parsed.
fn read(path: &Path) -> Result<DocumentMut> {
    let text = files::read_optional(path)?.unwrap_or_default();
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

pub fn hooks(home: &Path) -> Result<Vec<(String, String)>> {
    let config = read(&home.join(CONFIG))?;
    let Some(hooks) = config.get("hooks").and_then(Item::as_table) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for (event, groups) in hooks.iter().filter(|(event, _)| *event != "state") {
        for group in groups.as_array_of_tables().into_iter().flatten() {
            for command in group_commands(group) {
                found.push((event.to_owned(), command.to_owned()));
            }
        }
    }
    Ok(found)
}

/// Whether the config records Codex's trust in each hook as it is now.
pub fn trusted(home: &Path) -> Result<()> {
    let hashes = hashes(home).context("Codex could not say which hooks it runs")?;
    let config = read(&home.join(CONFIG))?;
    let state = config
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(Item::as_table);
    let untrusted = hashes.iter().filter(|(key, hash)| {
        let trusted = state.and_then(|state| state.get(key)?.get("trusted_hash")?.as_str());
        trusted != Some(hash.as_str())
    });
    let untrusted = untrusted.count();
    if untrusted > 0 {
        bail!(
            "Codex will not run {untrusted} of the hooks until it trusts them; run `agent-leds install`"
        );
    }
    Ok(())
}

/// Registers `command` for every event in HOOKS and for no other, in the
/// config at `path`. Codex trusts a hook by its place among the groups of
/// its event, so a group that is there already keeps its place, and a new
/// one goes after the others; a group of ours under any other event, as an
/// older version registered, goes with its trust records.
fn register(config: &mut DocumentMut, path: &str, command: &str) -> Result<()> {
    unregister(config, path, &HOOKS);
    let hooks = hooks_table(config)?;
    for event in HOOKS {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        let Some(groups) = groups.as_array_of_tables_mut() else {
            bail!("hooks.{event} is not a list of tables")
        };
        let mut handler = InlineTable::new();
        handler.insert("type", "command".into());
        handler.insert("command", command.into());
        handler.insert("timeout", 5.into());
        let handlers = value(Array::from_iter([handler]));
        let ours = groups.iter().position(is_ours);
        let ours = ours.unwrap_or_else(|| {
            let mut group = Table::new();
            // At the end of the file, clear of what other tools manage.
            group.set_position(Some(isize::MAX));
            groups.push(group);
            groups.len() - 1
        });
        groups.get_mut(ours).expect("the group is there")["hooks"] = handlers;
    }
    Ok(())
}

/// Records the hash Codex gave for each hook, which is what makes it run them.
fn trust(config: &mut DocumentMut, hashes: &[(String, String)]) -> Result<()> {
    let hooks = hooks_table(config)?;
    let state = hooks.entry("state").or_insert_with(implicit);
    let state = state.as_table_mut().context("hooks.state is not a table")?;
    for (key, hash) in hashes {
        let hook = state
            .entry(key)
            .or_insert_with(|| Item::Table(Table::new()));
        hook["trusted_hash"] = value(hash);
    }
    Ok(())
}

/// Removes the groups an install added under every event but those in
/// `keep`, the events left with no group, and what was recorded about
/// trusting them, and says whether there were any. Codex keys that record
/// by a group's place among its event's groups, so the groups after a
/// removed one take their records with them as they move up. A table left
/// empty goes only if it has no header of its own, and so prints nothing.
fn unregister(config: &mut DocumentMut, path: &str, keep: &[&str]) -> bool {
    let Some(hooks) = config.get_mut("hooks").and_then(Item::as_table_mut) else {
        return false;
    };
    let mut removed = false;
    let events: Vec<String> = hooks
        .iter()
        .map(|(event, _)| event.to_owned())
        .filter(|event| event != "state" && !keep.contains(&event.as_str()))
        .collect();
    let mut moves = Vec::new();
    for event in &events {
        let Some(groups) = hooks.get_mut(event).and_then(Item::as_array_of_tables_mut) else {
            continue;
        };
        let ours: Vec<bool> = groups.iter().map(is_ours).collect();
        removed |= ours.contains(&true);
        groups.retain(|group| !is_ours(group));
        if groups.is_empty() {
            hooks.remove(event);
        }
        let mut place = 0;
        for (index, ours) in ours.into_iter().enumerate() {
            moves.push((snake_case(event), index, (!ours).then_some(place)));
            place += usize::from(!ours);
        }
    }
    if let Some(state) = hooks.get_mut("state").and_then(Item::as_table_mut) {
        move_records(state, path, &moves);
    }
    let unheaded = |table: &Table| table.is_empty() && table.is_implicit();
    if hooks
        .get("state")
        .and_then(Item::as_table)
        .is_some_and(unheaded)
    {
        hooks.remove("state");
    }
    if unheaded(hooks) {
        config.remove("hooks");
    }
    removed
}

/// Moves each group's trust records from its old place to its new one, and
/// drops the records of groups that are gone. A record's key is
/// `<config path>:<event>:<group>:<handler>`.
fn move_records(state: &mut Table, path: &str, moves: &[(String, usize, Option<usize>)]) {
    let keys: Vec<String> = state.iter().map(|(key, _)| key.to_owned()).collect();
    let mut moved = Vec::new();
    for key in keys {
        let Some((event, group, handler)) = key
            .strip_prefix(path)
            .and_then(|rest| rest.strip_prefix(':'))
            .and_then(|rest| {
                let mut parts = rest.splitn(3, ':');
                Some((
                    parts.next()?,
                    parts.next()?.parse::<usize>().ok()?,
                    parts.next()?,
                ))
            })
        else {
            continue;
        };
        let Some((_, _, to)) = moves
            .iter()
            .find(|(name, from, _)| name == event && *from == group)
        else {
            continue;
        };
        let handler = handler.to_owned();
        let event = event.to_owned();
        if let Some(record) = state.remove(&key)
            && let Some(to) = to
        {
            moved.push((format!("{path}:{event}:{to}:{handler}"), record));
        }
    }
    for (key, record) in moved {
        state.insert(&key, record);
    }
}

/// An event's name as Codex spells it in the key of a hook: `pre_tool_use`.
fn snake_case(event: &str) -> String {
    let mut snake = String::new();
    for letter in event.chars() {
        if letter.is_uppercase() && !snake.is_empty() {
            snake.push('_');
        }
        snake.extend(letter.to_lowercase());
    }
    snake
}

/// The key and current hash of each hook an install registered, as Codex
/// itself reports them. Only Codex knows how it hashes a hook.
fn hashes(home: &Path) -> Result<Vec<(String, String)>> {
    let mut codex = Command::new("codex")
        .args(["app-server", "--stdio"])
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting `codex app-server`")?;
    let listed = list_hooks(&mut codex, home);
    let _ = codex.kill();
    let _ = codex.wait();
    let listed = listed?;
    let hashes: Vec<(String, String)> = listed["data"][0]["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|hook| {
            hook["command"]
                .as_str()
                .is_some_and(|command| Harness::Codex.owns(command))
        })
        .filter_map(|hook| {
            Some((
                hook["key"].as_str()?.to_owned(),
                hook["currentHash"].as_str()?.to_owned(),
            ))
        })
        .collect();
    if hashes.len() != HOOKS.len() {
        bail!("Codex lists {} of the {} hooks", hashes.len(), HOOKS.len());
    }
    Ok(hashes)
}

/// Asks a Codex app server, over its JSON-RPC on stdio, which hooks apply in `home`.
fn list_hooks(codex: &mut Child, home: &Path) -> Result<Value> {
    let mut requests = codex.stdin.take().context("no pipe to Codex")?;
    let replies = BufReader::new(codex.stdout.take().context("no pipe from Codex")?);
    let (lines, inbox) = mpsc::channel();
    thread::spawn(move || {
        for line in replies.lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + ANSWER;
    let mut call = |id: u64, method: &str, params: Value| -> Result<Value> {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(requests, "{request}")?;
        loop {
            let line = inbox
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .with_context(|| format!("Codex did not answer {method}"))?;
            let Ok(mut reply) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if reply["id"] != id {
                continue;
            }
            if let Some(error) = reply.get("error") {
                bail!("Codex refused {method}: {error}");
            }
            return Ok(reply["result"].take());
        }
    };
    let client = json!({ "name": "agent-leds", "version": env!("CARGO_PKG_VERSION") });
    call(1, "initialize", json!({ "clientInfo": client }))?;
    call(2, "hooks/list", json!({ "cwds": [home] }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::tests::Home;
    use super::*;

    /// A config with other tools' hooks and their trusted hashes, as
    /// SidePulse and an approval tool write them.
    const THEIRS: &str = r#"model = "gpt-6.1-sol"

[features]
memories = true

hooks = true
[projects."/Users/me"]
trust_level = "trusted"

[hooks.state]

# >>> agent-monitor hooks >>>
# Provider-neutral status collection. Do not edit inside this block.
[[hooks.Stop]]
matcher = "*"
[[hooks.Stop.hooks]]
type = "command"
command = '''python hook_entry.py --provider codex ; true'''

[[hooks.PermissionRequest]]
[[hooks.PermissionRequest.hooks]]
type = "command"
command = '''python hook_entry.py --provider codex --approvals ; true'''

# <<< agent-monitor hooks <<<

[hooks.state."/Users/me/.codex/config.toml:stop:0:0"]
trusted_hash = "sha256:271070a9"

[hooks.state."/Users/me/.codex/config.toml:permission_request:0:0"]
trusted_hash = "sha256:3f9a"
"#;

    const PATH: &str = "/Users/me/.codex/config.toml";

    /// Registers the hook and trusts it at `place` among the
    /// PermissionRequest groups.
    fn install(config: &Path, place: usize) {
        let hashes = [(
            format!("{PATH}:permission_request:{place}:0"),
            "sha256:0d1f".to_owned(),
        )];
        edit(config, |config| {
            register(config, PATH, "/bin/agent-leds hook codex")
        })
        .unwrap();
        edit(config, |config| trust(config, &hashes)).unwrap();
    }

    fn uninstall(config: &Path) {
        edit(config, |config| {
            unregister(config, PATH, &[]);
            Ok(())
        })
        .unwrap();
    }

    fn parse(config: &Path) -> toml::Value {
        toml::from_str(&fs::read_to_string(config).unwrap()).unwrap()
    }

    /// The trusted hash recorded for a hook's `<event>:<group>:<handler>`.
    fn hash(config: &Path, key: &str) -> Option<String> {
        let parsed = parse(config);
        let record = parsed["hooks"]["state"].get(format!("{PATH}:{key}"))?;
        Some(record["trusted_hash"].as_str()?.to_owned())
    }

    #[test]
    fn installing_adds_to_a_config_and_uninstalling_restores_it() {
        let home = Home::new();
        home.write(CONFIG, THEIRS);
        let config = home.join(CONFIG);
        install(&config, 1);
        let installed = fs::read_to_string(&config).unwrap();
        // Every line that was there still is, in the same order.
        let mut added = installed.lines();
        for line in THEIRS.lines() {
            assert!(added.any(|kept| kept == line), "lost {line:?}");
        }
        let parsed = parse(&config);
        let asked = parsed["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(asked.len(), 2);
        assert_eq!(
            asked[1]["hooks"][0]["command"].as_str(),
            Some("/bin/agent-leds hook codex")
        );
        assert_eq!(
            hash(&config, "permission_request:1:0").as_deref(),
            Some("sha256:0d1f")
        );
        assert_eq!(parsed["hooks"]["Stop"].as_array().unwrap().len(), 1);

        install(&config, 1);
        assert_eq!(fs::read_to_string(&config).unwrap(), installed);

        uninstall(&config);
        assert_eq!(fs::read_to_string(&config).unwrap(), THEIRS);
    }

    #[test]
    fn a_config_with_no_hooks_is_left_as_it_was() {
        let home = Home::new();
        let bare = "model = \"gpt-6.1-sol\"\n";
        home.write(CONFIG, bare);
        let config = home.join(CONFIG);
        install(&config, 0);
        uninstall(&config);
        assert_eq!(fs::read_to_string(&config).unwrap(), bare);
    }

    #[test]
    fn hooks_registered_after_ours_stay_trusted_when_ours_go() {
        let home = Home::new();
        home.write(CONFIG, THEIRS);
        let config = home.join(CONFIG);
        install(&config, 1);
        // Another tool adds a group after ours, and Codex trusts it there.
        let later = r#"
[[hooks.PermissionRequest]]
[[hooks.PermissionRequest.hooks]]
type = "command"
command = "later-tool"

[hooks.state."/Users/me/.codex/config.toml:permission_request:2:0"]
trusted_hash = "sha256:7777"
"#;
        let installed = fs::read_to_string(&config).unwrap();
        fs::write(&config, installed + later).unwrap();
        uninstall(&config);
        let parsed = parse(&config);
        let asked = parsed["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(asked[1]["hooks"][0]["command"].as_str(), Some("later-tool"));
        let hash = |key| hash(&config, key);
        assert_eq!(
            hash("permission_request:0:0").as_deref(),
            Some("sha256:3f9a")
        );
        assert_eq!(
            hash("permission_request:1:0").as_deref(),
            Some("sha256:7777")
        );
        assert_eq!(hash("permission_request:2:0"), None);
        assert_eq!(hash("stop:0:0").as_deref(), Some("sha256:271070a9"));
    }

    /// What 0.5.0 registered: a group of ours for each event it followed,
    /// after the other tools' groups, each trusted where it sat, and then a
    /// tool that registered after it.
    fn registered_by_0_5_0() -> String {
        let mut config = THEIRS.to_owned();
        let events = [
            ("UserPromptSubmit", 0),
            ("PreToolUse", 0),
            ("PermissionRequest", 1),
            ("PostToolUse", 0),
            ("Stop", 1),
            ("Interrupt", 0),
            ("SessionEnd", 0),
        ];
        for (event, place) in events {
            config += &format!(
                "\n[[hooks.{event}]]\n[[hooks.{event}.hooks]]\ntype = \"command\"\n\
                 command = \"/opt/homebrew/bin/agent-leds hook codex\"\ntimeout = 5\n\n\
                 [hooks.state.\"{PATH}:{}:{place}:0\"]\ntrusted_hash = \"sha256:{event}\"\n",
                snake_case(event)
            );
        }
        config += &format!(
            "\n[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"later-tool\"\n\n\
             [hooks.state.\"{PATH}:stop:2:0\"]\ntrusted_hash = \"sha256:7777\"\n"
        );
        config
    }

    #[test]
    fn installing_over_0_5_0_keeps_only_the_permission_hook_in_its_place_and_trusted() {
        let home = Home::new();
        home.write(CONFIG, &registered_by_0_5_0());
        let config = home.join(CONFIG);
        let exe = Path::new("/opt/homebrew/bin/agent-leds");
        let error = Harness::Codex.check(&home, exe).unwrap_err().to_string();
        assert!(
            error.contains(
                "does not use is registered for Stop, UserPromptSubmit, PreToolUse, PostToolUse, \
                 Interrupt, SessionEnd"
            ),
            "{error}"
        );
        let command = Harness::Codex.command(exe);
        edit(&config, |config| register(config, PATH, &command)).unwrap();
        let ours: Vec<String> = hooks(&home)
            .unwrap()
            .into_iter()
            .filter(|(_, command)| Harness::Codex.owns(command))
            .map(|(event, _)| event)
            .collect();
        assert_eq!(ours, HOOKS);
        // Kept where Codex trusted it, so it runs without being trusted again.
        let parsed = parse(&config);
        let asked = parsed["hooks"]["PermissionRequest"].as_array().unwrap();
        assert_eq!(
            asked[1]["hooks"][0]["command"].as_str(),
            Some(command.as_str())
        );
        let hash = |key| hash(&config, key);
        assert_eq!(
            hash("permission_request:1:0").as_deref(),
            Some("sha256:PermissionRequest")
        );
        // The others went with their records, and the tool after ours in
        // Stop took its place with its record.
        for key in [
            "user_prompt_submit:0:0",
            "pre_tool_use:0:0",
            "interrupt:0:0",
        ] {
            assert_eq!(hash(key), None, "{key}");
        }
        assert_eq!(hash("stop:0:0").as_deref(), Some("sha256:271070a9"));
        assert_eq!(hash("stop:1:0").as_deref(), Some("sha256:7777"));
        assert_eq!(hash("stop:2:0"), None);
        assert!(parsed["hooks"].get("SessionEnd").is_none());
    }

    #[test]
    fn installing_removes_a_hook_of_ours_under_an_event_no_longer_used() {
        let home = Home::new();
        home.write(CONFIG, THEIRS);
        let config = home.join(CONFIG);
        let leftover = r#"
[[hooks.Notification]]
[[hooks.Notification.hooks]]
type = "command"
command = "/opt/homebrew/bin/agent-leds hook codex"

[hooks.state."/Users/me/.codex/config.toml:notification:0:0"]
trusted_hash = "sha256:5151"
"#;
        fs::write(&config, THEIRS.to_owned() + leftover).unwrap();
        assert!(
            Harness::Codex
                .check(&home, Path::new("/opt/homebrew/bin/agent-leds"))
                .unwrap_err()
                .to_string()
                .contains("does not use is registered for Notification")
        );
        install(&config, 1);
        assert!(parse(&config)["hooks"].get("Notification").is_none());
        assert_eq!(hash(&config, "notification:0:0"), None);
        uninstall(&config);
        assert_eq!(fs::read_to_string(&config).unwrap(), THEIRS);
    }

    #[test]
    fn spells_an_event_as_codex_keys_it() {
        assert_eq!(snake_case("PermissionRequest"), "permission_request");
        assert_eq!(snake_case("Stop"), "stop");
    }

    #[test]
    fn the_hooks_registered_are_listed_by_event() {
        let home = Home::new();
        let command = "/bin/agent-leds hook codex";
        edit(&home.join(CONFIG), |config| register(config, PATH, command)).unwrap();
        let listed = hooks(&home).unwrap();
        let events: Vec<&str> = listed.iter().map(|(event, _)| event.as_str()).collect();
        assert_eq!(events, HOOKS);
        assert!(listed.iter().all(|(_, hook)| hook == command));
    }

    #[test]
    fn uninstalling_removes_the_config_an_install_created() {
        let home = Home::new();
        fs::create_dir(home.join(".codex")).unwrap();
        let config = home.join(CONFIG);
        let path = config.to_string_lossy().into_owned();
        let hashes = [(
            format!("{path}:permission_request:0:0"),
            "sha256:0d1f".to_owned(),
        )];
        edit(&config, |config| {
            register(config, &path, "/bin/agent-leds hook codex")
        })
        .unwrap();
        edit(&config, |config| trust(config, &hashes)).unwrap();
        super::uninstall(&home).unwrap();
        assert!(!home.join(CONFIG).exists());
    }

    #[test]
    fn an_empty_hooks_table_of_the_users_is_left_as_it_was() {
        let home = Home::new();
        let headed = "model = \"gpt-6.1-sol\"\n\n[hooks]\n";
        home.write(CONFIG, headed);
        let config = home.join(CONFIG);
        uninstall(&config);
        assert_eq!(fs::read_to_string(&config).unwrap(), headed);
        install(&config, 0);
        uninstall(&config);
        assert_eq!(fs::read_to_string(&config).unwrap(), headed);
    }
}
