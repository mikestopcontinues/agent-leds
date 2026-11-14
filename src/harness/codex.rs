//! Codex: its hooks live in `~/.codex/config.toml`, and it runs only the
//! hooks whose hash that file says to trust.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};

use super::{Event, Harness, Hook, Payload};
use crate::files;

const CONFIG: &str = ".codex/config.toml";

/// The hook events the daemon needs, in the order a turn produces them.
const EVENTS: [&str; 7] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "Interrupt",
    "SessionEnd",
];

/// How long Codex gets to say which hooks it sees.
const ANSWER: Duration = Duration::from_secs(15);

pub fn event(payload: &Value) -> Option<Event> {
    // Codex runs agents of its own as sessions it keeps no record of, such
    // as the one that consolidates its memories. Those are not the user's.
    let payload = Payload::read(payload)?;
    if payload.transcript_path == Some(None) {
        return None;
    }
    let mut event = Event::read(&payload)?;
    // Codex names a tool with or without its namespace.
    let tool = payload
        .tool_name
        .as_deref()
        .and_then(|tool| tool.rsplit('.').next());
    let asks_user = matches!(
        tool,
        Some("request_user_input" | "request_user_input_async")
    );
    match event.hook {
        Hook::PreToolUse => event.asks = asks_user,
        // The async tool returns at once: that acknowledges the question and
        // does not answer it.
        Hook::PostToolUse if tool == Some("request_user_input_async") => return None,
        // A turn that ends on a plan waits for the user to accept it.
        Hook::Stop => event.asks |= proposes_plan(&payload.last_assistant_message),
        _ => {}
    }
    Some(event)
}

/// Whether a final message is a plan put to the user for review.
fn proposes_plan(message: &str) -> bool {
    let mut lines = message.lines().map(str::trim);
    lines.any(|line| line == "<proposed_plan>") && lines.any(|line| line == "</proposed_plan>")
}

pub fn install(home: &Path, exe: &Path) -> Result<()> {
    let config = home.join(CONFIG);
    let command = Harness::Codex.command(exe);
    edit(&config, |config| register(config, &command))?;
    let hashes = hashes(home).context(
        "the hooks are in config.toml, but Codex will not run them until it is told to trust \
         them; run `agent-leds install` again once `codex` works",
    )?;
    edit(&config, |config| trust(config, &hashes))
}

pub fn uninstall(home: &Path) -> Result<()> {
    let config = home.join(CONFIG);
    edit(&config, |document| {
        unregister(document, &config.to_string_lossy());
        Ok(())
    })
}

/// Rewrites the config through `edit`, leaving every line `edit` does not
/// touch as it is. A config that `edit` leaves unchanged is not written.
fn edit(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let before = files::read_optional(path)?.unwrap_or_default();
    let mut config: DocumentMut = before
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
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

fn hooks(config: &mut DocumentMut) -> Result<&mut Table> {
    let hooks = config.entry("hooks").or_insert_with(implicit);
    hooks.as_table_mut().context("hooks is not a table")
}

/// Whether a group of handlers is the one an install wrote.
fn is_ours(group: &Table) -> bool {
    let owns = |command: Option<&str>| command.is_some_and(|command| Harness::Codex.owns(command));
    match group.get("hooks") {
        Some(Item::Value(handlers)) => handlers.as_array().is_some_and(|handlers| {
            handlers.iter().any(|handler| {
                let handler = handler.as_inline_table();
                owns(handler.and_then(|handler| handler.get("command")?.as_str()))
            })
        }),
        Some(Item::ArrayOfTables(handlers)) => handlers
            .iter()
            .any(|handler| owns(handler.get("command").and_then(Item::as_str))),
        _ => false,
    }
}

/// Registers `command` for every event. Codex trusts a hook by its place
/// among the groups of its event, so a group that is there already keeps
/// its place, and a new one goes after the others.
fn register(config: &mut DocumentMut, command: &str) -> Result<()> {
    let hooks = hooks(config)?;
    for event in EVENTS {
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
    let hooks = hooks(config)?;
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

/// Removes the groups an install added, the events left with no group, and
/// what was recorded about trusting them. Codex keys that record by a
/// group's place among its event's groups, so the groups after a removed one
/// take their records with them as they move up.
fn unregister(config: &mut DocumentMut, path: &str) {
    let Some(hooks) = config.get_mut("hooks").and_then(Item::as_table_mut) else {
        return;
    };
    let mut moves = Vec::new();
    for event in EVENTS {
        let Some(groups) = hooks.get_mut(event).and_then(Item::as_array_of_tables_mut) else {
            continue;
        };
        let ours: Vec<bool> = groups.iter().map(is_ours).collect();
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
    if hooks.is_empty() {
        config.remove("hooks");
    }
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
    if hashes.len() != EVENTS.len() {
        bail!("Codex lists {} of the {} hooks", hashes.len(), EVENTS.len());
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

    use super::super::tests::{Home, update};
    use super::*;
    use crate::state::{State, Update};

    /// A payload with the fields Codex sends on every event of a turn.
    fn event(name: &str, extra: Value) -> Option<Update> {
        let mut event = json!({
            "session_id": "019f0a3c-6f0e-7a51-8d2b-5d2f4c0e9a11",
            "turn_id": "019f0a3c-7b1d-7c03-9d1e-0f6c1c2b3a44",
            "transcript_path": "/Users/me/.codex/sessions/2026/11/13/rollout-2026-11-13T22-34-08-019f0a3c-6f0e-7a51-8d2b-5d2f4c0e9a11.jsonl",
            "cwd": "/Users/me/Code/app",
            "hook_event_name": name,
            "model": "gpt-6.1-sol",
            "permission_mode": "bypassPermissions",
        });
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        update(Harness::Codex, event)
    }

    #[test]
    fn maps_a_turn() {
        let working = Some(Update::Enter(State::Working));
        assert_eq!(
            event("UserPromptSubmit", json!({ "prompt": "fix it" })),
            working
        );
        assert_eq!(event("PreToolUse", json!({ "tool_name": "Bash" })), working);
        assert_eq!(
            event("PermissionRequest", json!({ "tool_name": "Bash" })),
            Some(Update::Enter(State::Question))
        );
        // A tool that fails still reports PostToolUse: Codex deals with it.
        assert_eq!(
            event(
                "PostToolUse",
                json!({ "tool_name": "Bash", "tool_response": "exit code: 1" })
            ),
            working
        );
        assert_eq!(
            event(
                "Stop",
                json!({ "last_assistant_message": "Done.", "stop_hook_active": false })
            ),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(
            event(
                "Stop",
                json!({ "last_assistant_message": "Shall I push it?" })
            ),
            Some(Update::Asked)
        );
        assert_eq!(
            event("Stop", json!({ "last_assistant_message": null })),
            Some(Update::Enter(State::Stopped))
        );
        assert_eq!(event("Interrupt", json!({})), Some(Update::Leave));
        assert_eq!(
            event("SessionEnd", json!({ "reason": "other" })),
            Some(Update::Leave)
        );
        assert_eq!(event("SessionStart", json!({ "source": "startup" })), None);
        assert_eq!(
            event(
                "PreToolUse",
                json!({ "agent_id": "a1", "agent_type": "worker", "tool_name": "Bash" })
            ),
            Some(Update::Background)
        );
    }

    #[test]
    fn a_session_codex_keeps_no_record_of_is_its_own_and_is_ignored() {
        let memories = json!({ "transcript_path": null, "cwd": "/Users/me/.codex/memories" });
        assert_eq!(event("UserPromptSubmit", memories.clone()), None);
        assert_eq!(event("Interrupt", memories), None);
    }

    #[test]
    fn a_tool_that_asks_the_user_is_a_question() {
        let question = Some(Update::Enter(State::Question));
        for tool in ["request_user_input", "functions.request_user_input_async"] {
            assert_eq!(event("PreToolUse", json!({ "tool_name": tool })), question);
        }
        // The answer to the blocking tool is its result.
        assert_eq!(
            event(
                "PostToolUse",
                json!({ "tool_name": "request_user_input", "tool_response": { "answers": {} } })
            ),
            Some(Update::Enter(State::Working))
        );
        // The async tool only says the question was put.
        assert_eq!(
            event(
                "PostToolUse",
                json!({ "tool_name": "request_user_input_async", "tool_response": "{\"accepted\":true}" })
            ),
            None
        );
    }

    #[test]
    fn a_turn_that_ends_on_a_plan_asks_for_its_review() {
        let plan = "<proposed_plan>\n### Test plan\n\n1. Run `sleep 5`.\n</proposed_plan>";
        assert_eq!(
            event("Stop", json!({ "last_assistant_message": plan })),
            Some(Update::Asked)
        );
        assert_eq!(
            event(
                "Stop",
                json!({ "last_assistant_message": "Plans go in a `<proposed_plan>` block." })
            ),
            Some(Update::Enter(State::Stopped))
        );
    }

    /// A config with another tool's hooks and its trusted hashes, as SidePulse writes them.
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

# <<< agent-monitor hooks <<<

[hooks.state."/Users/me/.codex/config.toml:stop:0:0"]
trusted_hash = "sha256:271070a9"
"#;

    const PATH: &str = "/Users/me/.codex/config.toml";

    /// Registers the hooks and trusts the Stop hook at `place` among the Stop groups.
    fn install(config: &Path, place: usize) {
        let hashes = [
            (format!("{PATH}:stop:{place}:0"), "sha256:0d1f".to_owned()),
            (format!("{PATH}:interrupt:0:0"), "sha256:9c2e".to_owned()),
        ];
        edit(config, |config| {
            register(config, "/bin/agent-leds hook codex")
        })
        .unwrap();
        edit(config, |config| trust(config, &hashes)).unwrap();
    }

    fn uninstall(config: &Path) {
        edit(config, |config| {
            unregister(config, PATH);
            Ok(())
        })
        .unwrap();
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
        let parsed: toml::Value = toml::from_str(&installed).unwrap();
        let stop = parsed["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(
            stop[1]["hooks"][0]["command"].as_str(),
            Some("/bin/agent-leds hook codex")
        );
        assert_eq!(
            parsed["hooks"]["state"][format!("{PATH}:stop:1:0").as_str()]["trusted_hash"].as_str(),
            Some("sha256:0d1f")
        );
        for event in EVENTS {
            assert!(
                parsed["hooks"][event].is_array(),
                "{event} is not registered"
            );
        }

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
        // Another tool adds a Stop group after ours, and Codex trusts it there.
        let later = r#"
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "later-tool"

[hooks.state."/Users/me/.codex/config.toml:stop:2:0"]
trusted_hash = "sha256:7777"
"#;
        let installed = fs::read_to_string(&config).unwrap();
        fs::write(&config, installed + later).unwrap();
        uninstall(&config);
        let parsed: toml::Value = toml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        let stop = parsed["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop[1]["hooks"][0]["command"].as_str(), Some("later-tool"));
        let state = parsed["hooks"]["state"].as_table().unwrap();
        let hash = |key: &str| state.get(&format!("{PATH}:{key}"))?["trusted_hash"].as_str();
        assert_eq!(hash("stop:0:0"), Some("sha256:271070a9"));
        assert_eq!(hash("stop:1:0"), Some("sha256:7777"));
        assert_eq!(hash("stop:2:0"), None);
        assert_eq!(hash("interrupt:0:0"), None);
    }

    #[test]
    fn every_event_registered_is_one_the_daemon_follows() {
        for event in EVENTS {
            assert!(Hook::named(event).is_some(), "{event}");
        }
    }

    #[test]
    fn spells_an_event_as_codex_keys_it() {
        assert_eq!(snake_case("PreToolUse"), "pre_tool_use");
        assert_eq!(snake_case("Stop"), "stop");
    }
}
