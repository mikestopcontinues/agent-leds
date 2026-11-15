//! Grok, from its own files. `~/.grok/active_sessions.json` lists each
//! running session with its process's pid and the microsecond that process
//! started, rewritten whole on every change. Each session keeps a folder,
//! `~/.grok/sessions/<cwd folder>/<session>/`, with two logs appended a
//! record at a time: `events.jsonl` says when a turn starts, waits and ends,
//! and `updates.jsonl` holds what the agent wrote and the subagents it left
//! running. The events own every boundary; the updates are read to their end
//! when a turn ends, for its final text. One hook remains, StopFailure, for a
//! turn that ends on an API error, which no file was seen to record.
//!
//! Every tool writes a permission request, and nothing in the request tells
//! one Grok approves by itself from one that waits on the user, so an
//! approval wait is not told and shows as working.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Grok 1.0.77.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::tail::{self, Lines, Tail};
use super::{Event, Hook, Observation, Source, Start, Told};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, StartCheck};
use crate::queue::TurnKey;

const fn file(
    signal: Signal,
    evidence: Evidence,
    record: &'static str,
    cite: &'static str,
) -> Declared {
    Declared {
        signal,
        channel: Channel::File,
        evidence,
        record,
        cite,
    }
}

/// What Grok's files and its one hook tell.
pub const SIGNALS: &[Declared] = &[
    file(
        Signal::Started,
        Evidence::Observed,
        "a new row in active_sessions.json whose process started at its pid_start, to the microsecond; a headless run and a subagent get none",
        "probe-grok session_start, runs a, j",
    ),
    file(
        Signal::Prompted,
        Evidence::Observed,
        "events.jsonl turn_started, keyed by its turn_number, including the turn a session starts by itself when a background subagent finishes",
        "probe-grok prompt_submitted, runs a, e",
    ),
    file(
        Signal::QuestionAsked,
        Evidence::Observed,
        "events.jsonl tool_started for ask_user_question",
        "probe-grok question_pending, grok5",
    ),
    file(
        Signal::Answered,
        Evidence::Observed,
        "events.jsonl tool_completed for ask_user_question",
        "probe-grok question_pending, grok5",
    ),
    file(
        Signal::TurnEnded,
        Evidence::Observed,
        "events.jsonl turn_ended, completed or cancelled",
        "probe-grok turn_completed, runs a, e, h",
    ),
    file(
        Signal::TurnEndedAsking,
        Evidence::Observed,
        "a completed turn_ended whose final text, the last agent_message_chunk in updates.jsonl after the latest user_message_chunk, has a last line ending in a question mark",
        "probe-grok run a",
    ),
    file(
        Signal::Backgrounded,
        Evidence::Observed,
        "a turn_ended while updates.jsonl has a subagent_spawned with no subagent_finished",
        "probe-grok subagent, run e",
    ),
    file(
        Signal::BackgroundDone,
        Evidence::Inferred,
        "updates.jsonl subagent_finished with will_wake false, leaving no subagent running; will_wake false was seen only for subagents that finished inside the turn, and the one that finished after its turn ended had will_wake true and started a turn, so this may never fire",
        "probe-grok subagent, run e; session 01a1db59-934f",
    ),
    file(
        Signal::Interrupted,
        Evidence::Observed,
        "a cancelled turn_ended triggered by ctrl_c, the stop button or the dashboard, or cancelled as permission_rejected or permission_cancelled",
        "probe-grok interrupted, runs a, h",
    ),
    file(
        Signal::Ended,
        Evidence::Observed,
        "the session's row gone from active_sessions.json, as /quit leaves it even mid-turn, or its process gone",
        "probe-grok quit/crash, runs a, b, c",
    ),
    Declared {
        signal: Signal::TurnFailed,
        channel: Channel::Hook,
        evidence: Evidence::Inferred,
        record: "stop_failure whose promptId is the newest turn's in updates.jsonl, or one never seen while that turn has named none; Grok can report a turn after the next has started, and that report tells nothing; no terminal API error was produced in the probe, whose refused connections retried without end",
        cite: "probe-grok turn_error, runs c, k; Grok's hook docs",
    },
];

/// The gestures that cancel a turn for the user, as the pager names them.
/// A cancel Grok makes itself, such as `shutdown`, is not one.
const USER_TRIGGERS: [&str; 3] = ["ctrl_c", "mouse", "dashboard_stop"];

/// The cancellations that are the user turning a tool call down.
const USER_CATEGORIES: [&str; 2] = ["permission_rejected", "permission_cancelled"];

const QUESTION_TOOL: &str = "ask_user_question";

/// A row of `active_sessions.json`.
#[derive(Deserialize)]
struct Row {
    session_id: String,
    pid: i32,
    /// When the process started, in Unix microseconds.
    pid_start: u64,
    #[serde(default)]
    cwd: String,
}

/// One `events.jsonl` record, with only the fields that carry state.
#[derive(Deserialize)]
struct Record {
    ts: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    turn_number: Option<u64>,
    tool_name: Option<String>,
    outcome: Option<String>,
    cancellation_category: Option<String>,
    cancellation_context: Option<Cancellation>,
}

#[derive(Deserialize)]
struct Cancellation {
    trigger: Option<String>,
}

impl Record {
    fn read(line: &[u8]) -> Option<Record> {
        serde_json::from_slice(line).ok()
    }

    /// Whether an earlier process wrote the record, before the one that
    /// started at `start` (a Unix second) resumed the session.
    fn before(&self, start: i64) -> bool {
        let written = self.ts.as_deref().and_then(liveness::iso_second);
        written.is_some_and(|written| written < start)
    }

    /// Whether the user cut the turn this record ends short.
    fn by_user(&self) -> bool {
        let trigger = self.cancellation_context.as_ref();
        let trigger = trigger.and_then(|context| context.trigger.as_deref());
        trigger.is_some_and(|trigger| USER_TRIGGERS.contains(&trigger))
            || self
                .cancellation_category
                .as_deref()
                .is_some_and(|category| USER_CATEGORIES.contains(&category))
    }
}

/// One `updates.jsonl` record, with only the fields that carry state.
#[derive(Deserialize)]
struct Update {
    /// When it was written, as a Unix second.
    timestamp: Option<i64>,
    params: Params,
}

#[derive(Deserialize)]
struct Params {
    update: Change,
    #[serde(rename = "_meta")]
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Meta {
    /// The prompt whose turn wrote the record: set on what the agent writes
    /// and the tools it calls, never on the prompt itself.
    #[serde(rename = "promptId")]
    prompt_id: Option<String>,
}

#[derive(Deserialize)]
struct Change {
    #[serde(rename = "sessionUpdate")]
    kind: String,
    content: Option<Value>,
    child_session_id: Option<String>,
    #[serde(default)]
    will_wake: bool,
}

impl Update {
    fn read(line: &[u8]) -> Option<Update> {
        serde_json::from_slice(line).ok()
    }

    fn before(&self, start: i64) -> bool {
        self.timestamp.is_some_and(|written| written < start)
    }

    fn is_prompt(&self) -> bool {
        self.params.update.kind == "user_message_chunk"
    }

    fn prompt_id(&self) -> Option<&str> {
        self.params.meta.as_ref()?.prompt_id.as_deref()
    }
}

/// A StopFailure hook as Grok sends it, its fields in camelCase.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    hook_event_name: String,
    session_id: String,
    /// The prompt whose turn failed.
    prompt_id: Option<String>,
    /// Set when a subagent's turn failed, which is no session of ours.
    subagent_type: Option<String>,
}

/// The two logs of a session.
#[derive(Clone, Copy, PartialEq)]
enum Log {
    Events,
    Updates,
}

impl Log {
    fn name(self) -> &'static str {
        match self {
            Log::Events => "events.jsonl",
            Log::Updates => "updates.jsonl",
        }
    }
}

/// How far one log has been read.
#[derive(Default)]
struct Reader {
    tail: Option<Tail>,
    /// Whether the last read stopped at its budget.
    behind: bool,
}

/// What the files have said of one session.
struct Session {
    /// The process that runs it, by its pid and start in microseconds.
    process: (i32, u64),
    /// That start as a Unix second, to set beside the logs' stamps.
    start: i64,
    /// The session's folder, once it is found.
    dir: Option<PathBuf>,
    events: Reader,
    updates: Reader,
    /// The turn that started and has not ended.
    open: Option<u64>,
    turn: Option<TurnKey>,
    /// Whether an ask_user_question waits on the user.
    question: bool,
    /// The turn the StopFailure hook ended, with the prompt the hook named:
    /// that turn's end in the file says nothing more, unless a later prompt's
    /// records show the hook was about an earlier turn.
    failed: Option<(u64, Option<String>)>,
    /// The prompt of the newest turn in updates.jsonl, once a record of
    /// that turn names it.
    prompt: Option<String>,
    /// The prompts of the turns before it.
    prompts: HashSet<String>,
    /// The subagents spawned and not finished.
    children: HashSet<String>,
    /// The last thing the agent wrote since the latest prompt.
    text: String,
    told: Told,
}

impl Session {
    fn reader(&mut self, log: Log) -> &mut Reader {
        match log {
            Log::Events => &mut self.events,
            Log::Updates => &mut self.updates,
        }
    }

    /// Whether a report names the newest turn: no prompt at all, the newest
    /// one, or one never seen while the newest turn has not named its own,
    /// as a turn that fails before the agent writes anything never does.
    fn newest(&self, prompt: Option<&str>) -> bool {
        match (prompt, self.prompt.as_deref()) {
            (None, _) => true,
            (Some(prompt), Some(newest)) => prompt == newest,
            (Some(prompt), None) => !self.prompts.contains(prompt),
        }
    }
}

/// Grok's sessions, from its registry of running sessions and their logs.
pub struct Grok {
    root: PathBuf,
    registry: PathBuf,
    sessions_dir: PathBuf,
    sessions: HashMap<String, Session>,
}

impl Grok {
    pub fn new(home: &Path) -> Grok {
        let root = home.join(".grok");
        Grok {
            registry: root.join("active_sessions.json"),
            sessions_dir: root.join("sessions"),
            root,
            sessions: HashMap::new(),
        }
    }

    /// The rows of the registry, by session. A session listed twice is run
    /// by the process its last row names.
    fn rows(&self) -> Result<HashMap<String, Row>> {
        let bytes = match fs::read(&self.registry) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(HashMap::new()),
            Err(error) => return Err(error.into()),
        };
        let rows: Vec<Row> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", self.registry.display()))?;
        Ok(rows
            .into_iter()
            .map(|row| (row.session_id.clone(), row))
            .collect())
    }

    /// Reads the registry again: starts the session of each new row whose
    /// process runs, starts again one handed to another process, and says
    /// which sessions lost their row, to end once their logs are read.
    fn registry(&mut self, k: &dyn Kernel, out: &mut Vec<Observation>) -> Vec<String> {
        // Unreadable: keep what it said before.
        let Ok(rows) = self.rows() else {
            return Vec::new();
        };
        let mut gone: Vec<String> = self
            .sessions
            .keys()
            .filter(|id| !rows.contains_key(*id))
            .cloned()
            .collect();
        gone.sort();
        let mut ids: Vec<&String> = rows.keys().collect();
        ids.sort();
        for id in ids {
            let row = &rows[id];
            let process = (row.pid, row.pid_start);
            match self.sessions.get(id) {
                Some(session) if session.process == process => continue,
                // Resumed in another process: the old one's turn is over.
                Some(_) => {
                    self.sessions.remove(id);
                    out.push(Observation::new(
                        Harness::Grok,
                        id,
                        Channel::File,
                        Event::one(Signal::Ended),
                        None,
                    ));
                }
                None => {}
            }
            self.start(k, row, out);
        }
        gone
    }

    /// Starts following the session a row names, if its process runs, and
    /// reads what that process has written of it so far.
    fn start(&mut self, k: &dyn Kernel, row: &Row, out: &mut Vec<Observation>) {
        let liveness = Liveness::Process {
            pid: row.pid,
            start: StartCheck::ExactUs(row.pid_start),
        };
        // A row left behind by a process that was killed.
        if !liveness::alive(k, &liveness) {
            return;
        }
        let id = row.session_id.clone();
        out.push(Observation::new(
            Harness::Grok,
            &id,
            Channel::File,
            Event::Started(Start {
                cwd: row.cwd.clone(),
                liveness,
            }),
            None,
        ));
        let dir = self.find(&id);
        self.sessions.insert(
            id.clone(),
            Session {
                process: (row.pid, row.pid_start),
                start: (row.pid_start / 1_000_000) as i64,
                dir,
                events: Reader::default(),
                updates: Reader::default(),
                open: None,
                turn: None,
                question: false,
                failed: None,
                prompt: None,
                prompts: HashSet::new(),
                children: HashSet::new(),
                text: String::new(),
                told: Told::default(),
            },
        );
        // The updates first, so a turn end in the events has its text.
        self.read(&id, Log::Updates, out);
        self.read(&id, Log::Events, out);
    }

    /// The session's folder, found by its name under any cwd folder: that
    /// folder's name is the cwd encoded or cut short, and is never computed.
    fn find(&self, id: &str) -> Option<PathBuf> {
        fs::read_dir(&self.sessions_dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path().join(id))
            .find(|dir| dir.is_dir())
    }

    /// The session and log a path is, if it is one. A subagent's folder is a
    /// sibling of its parent's, under an id no row names, so it is never read.
    fn log_of(&self, path: &Path) -> Option<(String, Log)> {
        let relative = path.strip_prefix(&self.sessions_dir).ok()?;
        let parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
        let [_, id, name] = parts[..] else {
            return None;
        };
        let log = [Log::Events, Log::Updates]
            .into_iter()
            .find(|log| log.name() == name)?;
        Some((id.to_owned(), log))
    }

    /// Reads what was appended to one of a session's logs. A log read for
    /// the first time is read back to where the session's last turn began,
    /// counting only what its own process wrote.
    fn read(&mut self, id: &str, log: Log, out: &mut Vec<Observation>) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(dir) = &session.dir else {
            return;
        };
        let path = dir.join(log.name());
        let start = session.start;
        let reader = session.reader(log);
        let lines = match &mut reader.tail {
            None => {
                let anchors = |line: &Vec<u8>| match log {
                    Log::Events => Record::read(line).is_some_and(|record| {
                        record.kind == "turn_started" || record.before(start)
                    }),
                    Log::Updates => Update::read(line)
                        .is_some_and(|update| update.is_prompt() || update.before(start)),
                };
                let Ok((lines, tail)) = tail::tail_back(&path, |lines| lines.iter().any(anchors))
                else {
                    return;
                };
                reader.tail = Some(tail);
                lines
            }
            Some(tail) => match tail.read(&path) {
                Ok(Lines::Read { lines, more }) => {
                    reader.behind = more;
                    lines
                }
                Ok(Lines::Replaced) => {
                    reader.tail = Tail::at_end(&path).ok();
                    reader.behind = false;
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    reader.tail = None;
                    reader.behind = false;
                    return;
                }
                // Read again at its next change, not over and over now.
                Err(_) => {
                    reader.behind = false;
                    return;
                }
            },
        };
        for line in lines {
            match log {
                Log::Events => self.event(id, &line, out),
                Log::Updates => self.update(id, &line, out),
            }
        }
    }

    /// What one events record says.
    fn event(&mut self, id: &str, line: &[u8], out: &mut Vec<Observation>) {
        let Some(record) = Record::read(line) else {
            return;
        };
        let session = self.sessions.get_mut(id).expect("tracked");
        if record.before(session.start) {
            return;
        }
        let tool = record.tool_name.as_deref();
        match record.kind.as_str() {
            "turn_started" => {
                session.open = record.turn_number;
                session.turn = record.turn_number.map(|n| TurnKey(n.to_string()));
                session.question = false;
                session.failed = None;
                tell(id, session, Channel::File, [Signal::Prompted], out);
            }
            _ if session.open.is_none() => {}
            "tool_started" if tool == Some(QUESTION_TOOL) => {
                session.question = true;
                tell(id, session, Channel::File, [Signal::QuestionAsked], out);
            }
            "tool_completed" if tool == Some(QUESTION_TOOL) && session.question => {
                session.question = false;
                tell(id, session, Channel::File, [Signal::Answered], out);
            }
            "turn_ended" => self.end(id, &record, out),
            _ => {}
        }
    }

    /// The end of the open turn: the updates are read to their end first,
    /// for the turn's final text and the subagents it leaves running.
    fn end(&mut self, id: &str, record: &Record, out: &mut Vec<Observation>) {
        loop {
            self.read(id, Log::Updates, out);
            if !self.sessions[id].updates.behind {
                break;
            }
        }
        let session = self.sessions.get_mut(id).expect("tracked");
        let turn = session.open.take();
        session.question = false;
        // The StopFailure hook told how this turn ended.
        if let (Some(turn), Some((failed, prompt))) = (turn, &session.failed)
            && turn == *failed
            && session.newest(prompt.as_deref())
        {
            return;
        }
        let mut end = BTreeSet::from([Signal::TurnEnded]);
        match record.outcome.as_deref() {
            Some("completed") if ends_on_question(&session.text) => {
                end.insert(Signal::TurnEndedAsking);
            }
            Some("cancelled") if record.by_user() => {
                end.insert(Signal::Interrupted);
            }
            _ => {}
        }
        if !session.children.is_empty() {
            end.insert(Signal::Backgrounded);
        }
        tell(id, session, Channel::File, end, out);
    }

    /// What one updates record says.
    fn update(&mut self, id: &str, line: &[u8], out: &mut Vec<Observation>) {
        let Some(update) = Update::read(line) else {
            return;
        };
        let session = self.sessions.get_mut(id).expect("tracked");
        if update.before(session.start) {
            return;
        }
        // The record's prompt is the newest turn's from now on.
        if let Some(named) = update.prompt_id()
            && session.prompt.as_deref() != Some(named)
        {
            session
                .prompts
                .extend(session.prompt.replace(named.to_owned()));
            session.prompts.remove(named);
        }
        let change = update.params.update;
        let text = || {
            let text = change
                .content
                .as_ref()
                .and_then(|content| content.get("text"));
            text.and_then(Value::as_str).unwrap_or_default().to_owned()
        };
        match change.kind.as_str() {
            "user_message_chunk" => {
                session.text.clear();
                session.prompts.extend(session.prompt.take());
            }
            "agent_message_chunk" => session.text = text(),
            "subagent_spawned" => {
                session.children.extend(change.child_session_id.clone());
            }
            "subagent_finished" => {
                if let Some(child) = &change.child_session_id {
                    session.children.remove(child);
                }
                // A subagent that wakes its parent starts a turn instead.
                if session.children.is_empty() && !change.will_wake {
                    tell(id, session, Channel::File, [Signal::BackgroundDone], out);
                }
            }
            _ => {}
        }
    }
}

/// Tells what one record or the hook says.
fn tell(
    id: &str,
    session: &mut Session,
    channel: Channel,
    signals: impl IntoIterator<Item = Signal>,
    out: &mut Vec<Observation>,
) {
    out.push(
        session
            .told
            .observe(Harness::Grok, id, channel, signals, session.turn.clone()),
    );
}

impl Source for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Vec<Observation>> {
        let rows = self.rows()?;
        let mut ids: Vec<&String> = rows.keys().collect();
        ids.sort();
        let mut seeded = Vec::new();
        for id in ids {
            let mut started = Vec::new();
            self.start(k, &rows[id], &mut started);
            let Some(mut started) = started.into_iter().next() else {
                continue;
            };
            let turn = self.sessions[id].turn.clone();
            started.turn = turn.clone();
            seeded.push(started);
            seeded.extend(self.sessions[id].told.current().into_iter().map(|signal| {
                Observation::new(
                    Harness::Grok,
                    id,
                    Channel::File,
                    Event::one(signal),
                    turn.clone(),
                )
            }));
        }
        for o in &mut seeded {
            o.quiet = true;
        }
        Ok(seeded)
    }

    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        let mut out = Vec::new();
        let registry = paths.is_none_or(|paths| paths.contains(&self.registry));
        let gone = if registry {
            self.registry(k, &mut out)
        } else {
            Vec::new()
        };
        let logs: Vec<(String, Log)> = match paths {
            Some(paths) => paths.iter().filter_map(|path| self.log_of(path)).collect(),
            None => {
                let mut ids: Vec<&String> = self.sessions.keys().collect();
                ids.sort();
                ids.into_iter()
                    .flat_map(|id| [(id.clone(), Log::Updates), (id.clone(), Log::Events)])
                    .collect()
            }
        };
        for (id, log) in logs {
            let found = match self.sessions.get(&id) {
                Some(session) if session.dir.is_none() => self.find(&id),
                _ => None,
            };
            if let Some(dir) = found {
                self.sessions.get_mut(&id).expect("tracked").dir = Some(dir);
            }
            self.read(&id, log, &mut out);
        }
        for id in gone {
            self.sessions.remove(&id);
            out.push(Observation::new(
                Harness::Grok,
                &id,
                Channel::File,
                Event::one(Signal::Ended),
                None,
            ));
        }
        out
    }

    fn hook(&mut self, k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let mut out = self.changed(k, None);
        let Ok(payload) = Payload::deserialize(&hook.payload) else {
            return out;
        };
        if payload.hook_event_name != "stop_failure" || payload.subagent_type.is_some() {
            return out;
        }
        let id = payload.session_id;
        let Some(session) = self.sessions.get_mut(&id) else {
            return out;
        };
        // Grok may report a turn after the next one has started.
        if !session.newest(payload.prompt_id.as_deref()) {
            return out;
        }
        session.failed = session.open.map(|turn| (turn, payload.prompt_id));
        tell(&id, session, Channel::Hook, [Signal::TurnFailed], &mut out);
        out
    }

    fn behind(&self) -> bool {
        let behind = |session: &Session| session.events.behind || session.updates.behind;
        self.sessions.values().any(behind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::Phase;
    use crate::queue::Key;
    use crate::source::fixture::{self, Replay};

    fn replay(name: &str) -> Replay {
        fixture::replay(Harness::Grok, name)
    }

    /// The phase the replay left its only session in.
    fn phase(replay: &Replay) -> Phase {
        let phases = replay.phases();
        assert_eq!(phases.len(), 1, "{phases:?}");
        phases.into_values().next().unwrap()
    }

    /// Every set of signals the replay yielded, in order.
    fn told(replay: &Replay) -> Vec<BTreeSet<Signal>> {
        replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signals())
            .collect()
    }

    /// Seeds from the real home, reading it and changing nothing, and prints
    /// the phase of each session Grok's registry shows running.
    #[test]
    #[ignore = "reads the real ~/.grok"]
    fn seeds_from_the_real_home() {
        let home = std::env::var_os("HOME").expect("a home");
        let mut grok = Grok::new(Path::new(&home));
        let mut queue = crate::queue::Queue::default();
        for o in grok.seed(&liveness::System, &[]).unwrap() {
            assert!(o.quiet);
            queue.apply(&o, 0);
        }
        println!("{} running sessions", queue.sessions().len());
        for session in queue.sessions() {
            println!(
                "{:<36}  {:<11}  {:<20}  turn {}",
                session.key.id,
                format!("{:?}", session.phase),
                session.label,
                session.turn.as_ref().map_or("-", |turn| turn.0.as_str())
            );
        }
    }

    #[test]
    fn every_fixture_replays_as_it_expects() {
        for name in fixture::names(Harness::Grok) {
            replay(&name);
        }
    }

    #[test]
    fn a_daemon_started_after_any_fixture_seeds_the_phases_it_reached() {
        for name in fixture::names(Harness::Grok) {
            let replay = replay(&name);
            let where_ = |queue: &crate::queue::Queue| -> Vec<(Key, Phase, Option<TurnKey>)> {
                let mut sessions: Vec<_> = queue
                    .sessions()
                    .iter()
                    .map(|session| (session.key.clone(), session.phase, session.turn.clone()))
                    .collect();
                sessions.sort_by(|a, b| a.0.cmp(&b.0));
                sessions
            };
            // A failure only the hook told is not in the files.
            if replay
                .phases()
                .values()
                .any(|phase| *phase == Phase::Failed)
            {
                continue;
            }
            assert_eq!(
                where_(&fixture::seed(&replay)),
                where_(&replay.queue),
                "{name}"
            );
        }
    }

    #[test]
    fn an_auto_approved_permission_is_not_a_question() {
        let replay = replay("an_auto_approved_permission_is_not_a_question");
        let told = told(&replay);
        assert!(
            told.iter()
                .all(|signals| !signals.contains(&Signal::PermissionAsked)),
            "{told:?}"
        );
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_subagent_finished_inside_its_turn_leaves_it_working() {
        let replay = replay("a_subagent_finished_inside_its_turn_leaves_it_working");
        let (finished, step) = replay
            .yielded
            .iter()
            .find(|(o, _)| o.event.signals().contains(&Signal::BackgroundDone))
            .expect("the finish is told");
        let mut queue = crate::queue::Queue::default();
        for (o, at) in &replay.yielded {
            if at <= step {
                queue.apply(o, 0);
            }
        }
        assert_eq!(queue.phase(&finished.key), Phase::Working);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_fast_turn_in_one_batch_ends_stopped() {
        let replay = replay("a_fast_turn_in_one_batch_ends_stopped");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn quit_mid_turn_ends_the_session() {
        let replay = replay("quit_mid_turn_ends_the_session");
        assert!(replay.phases().is_empty());
        assert_eq!(told(&replay).last(), Some(&BTreeSet::from([Signal::Ended])));
    }

    #[test]
    fn a_turn_end_after_stop_failure_stays_failed() {
        let replay = replay("a_turn_end_after_stop_failure_stays_failed");
        assert_eq!(phase(&replay), Phase::Failed);
    }

    #[test]
    fn a_late_stop_failure_leaves_the_next_turn_working() {
        let replay = replay("a_late_stop_failure_leaves_the_next_turn_working");
        let told = told(&replay);
        assert!(
            told.iter()
                .all(|signals| !signals.contains(&Signal::TurnFailed)),
            "{told:?}"
        );
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_late_stop_failure_does_not_swallow_the_next_turn_s_end() {
        let replay = replay("a_late_stop_failure_does_not_swallow_the_next_turn_s_end");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_stop_failure_after_its_turn_ended_fails_it() {
        let replay = replay("a_stop_failure_after_its_turn_ended_fails_it");
        assert_eq!(phase(&replay), Phase::Failed);
    }

    #[test]
    fn a_child_directory_is_never_started() {
        let replay = replay("a_child_directory_is_never_started");
        let ids: Vec<String> = replay.phases().into_keys().map(|key| key.id).collect();
        assert_eq!(ids, ["01a1dc12-ef70-7af2-9d6c-b78c992d3ee4"]);
    }

    #[test]
    fn a_resumed_session_does_not_replay_what_its_earlier_process_wrote() {
        let replay = replay("turn_ended.file");
        assert_eq!(
            told(&replay),
            [Signal::Started, Signal::Prompted, Signal::TurnEnded]
                .map(|signal| BTreeSet::from([signal]))
        );
        let session = &replay.queue.sessions()[0];
        assert_eq!(session.turn, Some(TurnKey("2".to_owned())));
    }

    #[test]
    fn a_process_that_is_gone_is_not_seeded() {
        let replay = replay("a_session_whose_process_exits_ends");
        assert!(replay.phases().is_empty());
        assert!(!fixture::seed(&replay).tracked());
    }

    #[test]
    fn a_file_that_cannot_be_read_leaves_nothing_behind() {
        fixture::a_read_that_fails_leaves_nothing_behind(
            &replay("turn_ended.file"),
            ".grok/sessions/%2FUsers%2Fme%2FCode%2Fapp/01a1dc0d-63f4-7eb0-95d3-175af42f88aa/events.jsonl",
        );
    }
}
