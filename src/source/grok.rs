//! Grok, from its own files. `~/.grok/active_sessions.json` lists each
//! running session with its process's pid and the microsecond that process
//! started, rewritten whole on every change. Each session keeps a folder,
//! `~/.grok/sessions/<cwd folder>/<session>/`, with two logs appended a
//! record at a time: `events.jsonl` says when a turn starts, waits and ends,
//! and `updates.jsonl` holds what the agent wrote and the subagents it left
//! running.
//! The events own every boundary; the updates are read to their end when a
//! turn ends, for its final text. Beside them `plan_mode.json`,
//! replaced whole at each change, says whether a plan waits on the user's
//! approval. No hook is needed.
//!
//! Every tool writes a permission request, and nothing in the request tells
//! one Grok approves by itself from one that waits on the user, so a tool
//! approval wait is not told and shows as working.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Grok [`VERSION`], and [`drift`] says when Grok has moved on.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::tail::{self, Tail};
use super::{Event, Hook, Observation, Seed, Source, Start, Strange, Told, Unread};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, StartCheck};
use crate::queue::TurnKey;

/// The Grok release the fixtures were recorded from.
pub const VERSION: &str = "1.0.77";

/// The `schema_version` that release writes on each `turn_started`.
const SCHEMA: &str = "1.0";

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
        untold: None,
    }
}

/// What Grok's files tell.
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
        Signal::PermissionAsked,
        Evidence::Observed,
        "plan_mode.json awaiting_plan_approval true while a turn is open, as exit_plan_mode's plan card waits on the user",
        "gaps p3",
    )
    .untold("a tool's approval card"),
    file(
        Signal::QuestionAsked,
        Evidence::Observed,
        "events.jsonl tool_started for ask_user_question",
        "probe-grok question_pending, grok5",
    ),
    file(
        Signal::Answered,
        Evidence::Observed,
        "events.jsonl tool_completed for ask_user_question, or plan_mode.json awaiting_plan_approval false again while the turn goes on",
        "probe-grok question_pending, grok5; gaps p3",
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
        Signal::TurnFailed,
        Evidence::Observed,
        "events.jsonl turn_ended with outcome error, written as a turn ends on an API error, before Grok runs StopFailure for it; whether a refusal or a max_output_tokens end writes it is unverified, and a request that cannot connect retries without end",
        "gaps p2; probe-grok runs c, k",
    ),
    file(
        Signal::Backgrounded,
        Evidence::Observed,
        "a turn_ended while updates.jsonl has a subagent_spawned with no subagent_finished",
        "probe-grok subagent, run e",
    )
    .untold("a background command left running, which wakes the session with a turn of its own when it finishes; nothing at its start says whether it ever will, as a dev server never does"),
    file(
        Signal::BackgroundDone,
        Evidence::Inferred,
        "updates.jsonl subagent_finished with will_wake false, leaving no subagent running; will_wake false was seen only for subagents that finished inside the turn, and the one that finished after its turn ended had will_wake true and started a turn, so this may never fire",
        "probe-grok subagent, run e; session 01a1db59-934f",
    ),
    file(
        Signal::Interrupted,
        Evidence::Observed,
        "a cancelled turn_ended triggered by ctrl_c, the stop button, the dashboard or send_now (the user's next prompt, whose turn_started follows at once), or cancelled as permission_rejected or permission_cancelled",
        "probe-grok interrupted, runs a, h; gaps grok-x a",
    ),
    file(
        Signal::Ended,
        Evidence::Observed,
        "the session's row gone from active_sessions.json, as /quit leaves it even mid-turn, or its process gone",
        "probe-grok quit/crash, runs a, b, c",
    ),
];

/// The gestures that cancel a turn for the user, as the pager names them:
/// `send_now` cancels it to run the user's next prompt at once. A cancel
/// Grok makes itself, such as `shutdown`, is not one.
const USER_TRIGGERS: [&str; 4] = ["ctrl_c", "mouse", "dashboard_stop", "send_now"];

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

/// The kinds of `events.jsonl` and `updates.jsonl` record Grok was seen to
/// write, in the sessions on the author's Mac and the fixtures.
const EVENTS: &[&str] = &[
    "first_token",
    "loop_started",
    "mcp_config_resolved",
    "mcp_init_completed",
    "mcp_server_connected",
    "mcp_server_starting",
    "permission_requested",
    "permission_resolved",
    "phase_changed",
    "tool_completed",
    "tool_started",
    "turn_ended",
    "turn_started",
];
const UPDATES: &[&str] = &[
    "agent_message_chunk",
    "agent_thought_chunk",
    "background_tasks",
    "current_mode_update",
    "hook_execution",
    "retry_state",
    "rewind_marker",
    "subagent_finished",
    "subagent_spawned",
    "task_backgrounded",
    "task_completed",
    "tool_call",
    "tool_call_update",
    "turn_completed",
    "user_message_chunk",
];

/// One `events.jsonl` record, with only the fields that carry state.
#[derive(Deserialize)]
struct Record {
    ts: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    turn_number: Option<u64>,
    schema_version: Option<String>,
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

    fn starts_turn(line: &[u8]) -> bool {
        Record::read(line).is_some_and(|record| record.kind == "turn_started")
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

    /// A message the user sent the agent. A `!` shell command is written as
    /// one too, with the command in its `_meta`, but it is no turn.
    fn is_prompt(&self) -> bool {
        let change = &self.params.update;
        let shell = change
            .content
            .as_ref()
            .and_then(|content| content.get("_meta"))
            .and_then(|meta| meta.get("bash_command"))
            .is_some();
        change.kind == "user_message_chunk" && !shell
    }
}

/// `plan_mode.json`, with only the field that carries state.
#[derive(Deserialize)]
struct PlanMode {
    awaiting_plan_approval: bool,
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

/// A file of a session's folder that the source reads.
#[derive(Clone, Copy, PartialEq)]
enum File {
    Log(Log),
    Plan,
}

impl File {
    /// In the order they are read: the updates before the events, so a turn
    /// end has its text, and the plan after them, against the turn as the
    /// events leave it.
    const ALL: [File; 3] = [File::Log(Log::Updates), File::Log(Log::Events), File::Plan];

    fn name(self) -> &'static str {
        match self {
            File::Log(log) => log.name(),
            File::Plan => "plan_mode.json",
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
    /// Whether a plan waits on the user's approval.
    plan: bool,
    /// The subagents started and not finished.
    subagents: HashSet<String>,
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
}

/// Grok's sessions, from its registry of running sessions and their logs.
pub struct Grok {
    root: PathBuf,
    registry: PathBuf,
    sessions_dir: PathBuf,
    sessions: HashMap<String, Session>,
    /// What its reads could not make sense of, since it was last asked.
    strange: Strange,
}

impl Grok {
    pub fn new(home: &Path) -> Grok {
        let root = home.join(".grok");
        Grok {
            registry: root.join("active_sessions.json"),
            sessions_dir: root.join("sessions"),
            root,
            sessions: HashMap::new(),
            strange: Strange::default(),
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
            // Read again at its next change.
            let _ = self.start(k, row, out);
        }
        gone
    }

    /// Starts following the session a row names, if its process runs, and
    /// reads what that process has written of it so far, saying which of
    /// its files it could not read.
    fn start(&mut self, k: &dyn Kernel, row: &Row, out: &mut Vec<Observation>) -> Vec<Unread> {
        let liveness = Liveness::Process {
            pid: row.pid,
            start: StartCheck::ExactUs(row.pid_start),
        };
        // A row left behind by a process that was killed.
        if !liveness::alive(k, &liveness) {
            return Vec::new();
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
                plan: false,
                subagents: HashSet::new(),
                text: String::new(),
                told: Told::default(),
            },
        );
        File::ALL
            .into_iter()
            .filter_map(|file| self.read(&id, file, out).err())
            .collect()
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

    /// The session and file a path is, if it is one. A subagent's folder is
    /// a sibling of its parent's, under an id no row names, so it is never
    /// read.
    fn file_of(&self, path: &Path) -> Option<(String, File)> {
        let relative = path.strip_prefix(&self.sessions_dir).ok()?;
        let parts: Vec<&str> = relative.iter().filter_map(|part| part.to_str()).collect();
        let [_, id, name] = parts[..] else {
            return None;
        };
        let file = File::ALL.into_iter().find(|file| file.name() == name)?;
        Some((id.to_owned(), file))
    }

    /// Reads what changed in one of a session's files. A file that cannot be
    /// read is read again at its next change.
    fn read(&mut self, id: &str, file: File, out: &mut Vec<Observation>) -> Result<(), Unread> {
        match file {
            File::Log(log) => self.tail(id, log, out),
            File::Plan => self.plan(id, out),
        }
    }

    /// Reads what was appended to one of a session's logs. A log read for
    /// the first time is read back to where the session's last turn began,
    /// only as far as its own process wrote.
    fn tail(&mut self, id: &str, log: Log, out: &mut Vec<Observation>) -> Result<(), Unread> {
        let Some(session) = self.sessions.get_mut(id) else {
            return Ok(());
        };
        let Some(dir) = &session.dir else {
            return Ok(());
        };
        let path = dir.join(log.name());
        let start = session.start;
        let reader = session.reader(log);
        let lines = match &mut reader.tail {
            None => {
                let anchors = |line: &[u8]| match log {
                    Log::Events => {
                        Record::starts_turn(line)
                            || Record::read(line).is_some_and(|record| record.before(start))
                    }
                    Log::Updates => Update::read(line)
                        .is_some_and(|update| update.is_prompt() || update.before(start)),
                };
                let (lines, tail) = match tail::tail_back(&path, &mut self.strange, anchors) {
                    Ok(read) => read,
                    // Not written yet.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                    Err(error) => return Err(Unread::new(&path, error)),
                };
                reader.tail = Some(tail);
                lines
            }
            Some(_) => {
                let read = tail::follow(
                    &mut reader.tail,
                    &path,
                    &mut reader.behind,
                    &mut self.strange,
                );
                read.map_err(|error| Unread::new(&path, error))?
            }
        };
        for line in lines {
            match log {
                Log::Events => self.event(id, &line, out),
                Log::Updates => self.update(id, &line, out),
            }
        }
        Ok(())
    }

    /// Reads whether a plan waits on the user's approval, which counts only
    /// while a turn is open: the events are read first, so the turn is known
    /// as it stands whatever order the changes came in.
    fn plan(&mut self, id: &str, out: &mut Vec<Observation>) -> Result<(), Unread> {
        // What cannot be read now is read at its next change.
        let _ = self.tail(id, Log::Events, out);
        let Some(session) = self.sessions.get_mut(id) else {
            return Ok(());
        };
        let Some(dir) = &session.dir else {
            return Ok(());
        };
        let path = dir.join(File::Plan.name());
        let awaiting = match fs::read(&path) {
            Ok(bytes) => {
                let plan: PlanMode =
                    serde_json::from_slice(&bytes).map_err(|error| Unread::new(&path, error))?;
                plan.awaiting_plan_approval
            }
            // Never in plan mode.
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(Unread::new(&path, error)),
        };
        let waits = awaiting && session.open.is_some();
        if waits != session.plan {
            session.plan = waits;
            let signal = if waits {
                Signal::PermissionAsked
            } else {
                Signal::Answered
            };
            tell(id, session, [signal], out);
        }
        Ok(())
    }

    /// What one events record says.
    fn event(&mut self, id: &str, line: &[u8], out: &mut Vec<Observation>) {
        let Some(record) = self.strange.parsed(Record::read(line)) else {
            return;
        };
        self.strange.kind(&record.kind, EVENTS);
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
                session.plan = false;
                tell(id, session, [Signal::Prompted], out);
            }
            _ if session.open.is_none() => {}
            "tool_started" if tool == Some(QUESTION_TOOL) => {
                session.question = true;
                tell(id, session, [Signal::QuestionAsked], out);
            }
            "tool_completed" if tool == Some(QUESTION_TOOL) && session.question => {
                session.question = false;
                tell(id, session, [Signal::Answered], out);
            }
            "turn_ended" => self.end(id, &record, out),
            _ => {}
        }
    }

    /// The end of the open turn: the updates are read to their end first,
    /// for the turn's final text and the subagents it leaves running.
    fn end(&mut self, id: &str, record: &Record, out: &mut Vec<Observation>) {
        loop {
            // What cannot be read now is read at its next change.
            let _ = self.tail(id, Log::Updates, out);
            if !self.sessions[id].updates.behind {
                break;
            }
        }
        let session = self.sessions.get_mut(id).expect("tracked");
        session.open = None;
        session.question = false;
        session.plan = false;
        let mut end = BTreeSet::from([Signal::TurnEnded]);
        match record.outcome.as_deref() {
            Some("completed") if ends_on_question(&session.text) => {
                end.insert(Signal::TurnEndedAsking);
            }
            Some("cancelled") if record.by_user() => {
                end.insert(Signal::Interrupted);
            }
            Some("error") => {
                end.insert(Signal::TurnFailed);
            }
            _ => {}
        }
        if !session.subagents.is_empty() {
            end.insert(Signal::Backgrounded);
        }
        tell(id, session, end, out);
    }

    /// What one updates record says.
    fn update(&mut self, id: &str, line: &[u8], out: &mut Vec<Observation>) {
        let Some(update) = self.strange.parsed(Update::read(line)) else {
            return;
        };
        self.strange.kind(&update.params.update.kind, UPDATES);
        let session = self.sessions.get_mut(id).expect("tracked");
        if update.before(session.start) {
            return;
        }
        if update.is_prompt() {
            session.text.clear();
        }
        let change = update.params.update;
        match change.kind.as_str() {
            "agent_message_chunk" => {
                let text = change
                    .content
                    .as_ref()
                    .and_then(|content| content.get("text"));
                session.text = text.and_then(Value::as_str).unwrap_or_default().to_owned();
            }
            "subagent_spawned" => session.subagents.extend(change.child_session_id),
            "subagent_finished" => {
                if let Some(child) = change.child_session_id {
                    session.subagents.remove(&child);
                }
                // A subagent that wakes its parent starts a turn instead.
                if session.subagents.is_empty() && !change.will_wake {
                    tell(id, session, [Signal::BackgroundDone], out);
                }
            }
            _ => {}
        }
    }
}

/// Tells what one record says.
fn tell(
    id: &str,
    session: &mut Session,
    signals: impl IntoIterator<Item = Signal>,
    out: &mut Vec<Observation>,
) {
    out.push(session.told.observe(
        Harness::Grok,
        id,
        Channel::File,
        signals,
        session.turn.clone(),
    ));
}

/// What says Grok may write its files otherwise than the fixtures pin:
/// `version`, what `grok --version` printed, names another release, or the
/// newest session's last turn names another schema. Grok updates itself by
/// default, so a new release arrives unannounced.
pub fn drift(home: &Path, version: Option<&str>) -> Option<String> {
    let mut problems = Vec::new();
    // "grok 1.0.77 (50ca579fa996) [stable]"
    let release = version.and_then(|printed| printed.split_whitespace().nth(1));
    if let Some(release) = release
        && release != VERSION
    {
        problems.push(format!(
            "grok is {release}, and its files were read as {VERSION} writes them"
        ));
    }
    if let Some(schema) = newest_schema(&home.join(".grok/sessions"))
        && schema != SCHEMA
    {
        problems.push(format!(
            "the newest session's turn_started has schema_version {schema}, and {VERSION} writes {SCHEMA}"
        ));
    }
    (!problems.is_empty())
        .then(|| format!("{}; its format may have changed", problems.join(", and ")))
}

/// The `schema_version` of the last turn started in the events log written
/// last, or `none` when that turn names none.
fn newest_schema(sessions: &Path) -> Option<String> {
    let modified = |path: &Path| fs::metadata(path).and_then(|m| m.modified()).ok();
    let newest: (SystemTime, PathBuf) = fs::read_dir(sessions)
        .ok()?
        .flatten()
        .filter_map(|cwd| fs::read_dir(cwd.path()).ok())
        .flatten()
        .flatten()
        .map(|session| session.path().join(Log::Events.name()))
        .filter_map(|log| Some((modified(&log)?, log)))
        .max()?;
    let strange = &mut Strange::default();
    let (lines, _) = tail::tail_back(&newest.1, strange, Record::starts_turn).ok()?;
    let started = lines
        .iter()
        .find_map(|line| Record::read(line).filter(|record| record.kind == "turn_started"))?;
    Some(started.schema_version.unwrap_or_else(|| "none".to_owned()))
}

impl Source for Grok {
    fn harness(&self) -> Harness {
        Harness::Grok
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Seed> {
        let rows = self.rows()?;
        let mut ids: Vec<&String> = rows.keys().collect();
        ids.sort();
        let mut seeded = Vec::new();
        let mut unread = Vec::new();
        for id in ids {
            let mut started = Vec::new();
            unread.extend(self.start(k, &rows[id], &mut started));
            // Only where the logs leave the session is told, after its start.
            started.truncate(1);
            let Some(session) = self.sessions.get(id) else {
                continue;
            };
            let turn = session.turn.clone();
            seeded.extend(session.told.seeded(started, turn, |_| Channel::File));
        }
        Ok(Seed {
            observations: seeded,
            unread,
        })
    }

    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        let mut out = Vec::new();
        let registry = paths.is_none_or(|paths| paths.contains(&self.registry));
        let gone = if registry {
            self.registry(k, &mut out)
        } else {
            Vec::new()
        };
        let files: Vec<(String, File)> = match paths {
            Some(paths) => paths.iter().filter_map(|path| self.file_of(path)).collect(),
            None => {
                let mut ids: Vec<&String> = self.sessions.keys().collect();
                ids.sort();
                ids.into_iter()
                    .flat_map(|id| File::ALL.map(|file| (id.clone(), file)))
                    .collect()
            }
        };
        for (id, file) in files {
            let found = match self.sessions.get(&id) {
                Some(session) if session.dir.is_none() => self.find(&id),
                _ => None,
            };
            if let Some(dir) = found {
                self.sessions.get_mut(&id).expect("tracked").dir = Some(dir);
            }
            // Read again at its next change.
            let _ = self.read(&id, file, &mut out);
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

    fn hook(&mut self, _k: &dyn Kernel, _hook: &Hook) -> Vec<Observation> {
        Vec::new()
    }

    fn behind(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for session in self.sessions.values() {
            let Some(dir) = &session.dir else {
                continue;
            };
            for (log, reader) in [
                (Log::Events, &session.events),
                (Log::Updates, &session.updates),
            ] {
                if reader.behind {
                    paths.push(dir.join(log.name()));
                }
            }
        }
        paths.sort();
        paths
    }

    fn strange(&mut self) -> Strange {
        mem::take(&mut self.strange)
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
        for o in fixture::printed(grok.seed(&liveness::System, &[]).unwrap()) {
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
    fn a_plan_awaiting_approval_blocks_until_the_user_decides() {
        let replay = replay("permission_asked.file");
        let told = told(&replay);
        let asked = told
            .iter()
            .position(|signals| signals.contains(&Signal::PermissionAsked))
            .expect("the plan card is told");
        assert_eq!(told[asked + 1], BTreeSet::from([Signal::Answered]));
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_plan_sent_back_and_shown_again_blocks_again() {
        let replay = replay("a_plan_sent_back_and_shown_again_blocks_again");
        let asked = told(&replay)
            .iter()
            .filter(|signals| signals.contains(&Signal::PermissionAsked))
            .count();
        assert_eq!(asked, 2);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_turn_that_ends_on_an_api_error_fails() {
        let replay = replay("turn_failed.file");
        assert_eq!(phase(&replay), Phase::Failed);
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

    /// A command left running is no work the turn goes on with: a server
    /// may never exit. When one finishes, Grok wakes the session with a turn
    /// of its own.
    #[test]
    fn a_background_command_left_running_ends_its_turn_stopped() {
        let replay = replay("a_background_command_left_running_ends_its_turn_stopped");
        let told = told(&replay);
        assert!(
            told.iter()
                .all(|told| !told.contains(&Signal::Backgrounded))
        );
        assert!(
            told.iter()
                .all(|told| !told.contains(&Signal::BackgroundDone))
        );
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_shell_command_after_a_question_leaves_it_asking() {
        let replay = replay("a_shell_command_after_a_question_leaves_it_asking");
        assert_eq!(phase(&replay), Phase::Asked);
        let seeded = fixture::seed(&replay);
        assert_eq!(seeded.sessions()[0].phase, Phase::Asked);
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

    #[test]
    fn a_release_or_schema_other_than_the_fixtures_pin_is_drift() {
        let replay = replay("turn_ended.file");
        let home = replay.home.path();
        let pinned = format!("grok {VERSION} (50ca579fa996) [stable]\n");
        assert_eq!(drift(home, Some(&pinned)), None);
        assert_eq!(drift(home, None), None);
        assert_eq!(
            drift(home, Some("grok 1.0.80 (0123456789ab) [stable]")).unwrap(),
            format!(
                "grok is 1.0.80, and its files were read as {VERSION} writes them; its format may \
                 have changed"
            )
        );
        let log = home.join(
            ".grok/sessions/%2FUsers%2Fme%2FCode%2Fapp/01a1dc0d-63f4-7eb0-95d3-175af42f88aa/events.jsonl",
        );
        let text = fs::read_to_string(&log).unwrap();
        fs::write(
            &log,
            text.replace("\"schema_version\":\"1.0\"", "\"schema_version\":\"2.0\""),
        )
        .unwrap();
        assert_eq!(
            drift(home, Some(&pinned)).unwrap(),
            format!(
                "the newest session's turn_started has schema_version 2.0, and {VERSION} writes \
                 {SCHEMA}; its format may have changed"
            )
        );
    }
}
