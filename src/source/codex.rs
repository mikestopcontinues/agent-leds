//! Codex, from its own files. A process that runs a thread holds a lock on
//! `~/.codex/thread-writer-locks/<thread>.lock` for as long as it runs it,
//! and appends the thread's rollout,
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`, a record at
//! a time: when each turn starts and how it ends, what it asks, and the
//! subagents it starts. Nothing is written while an approval waits, so the
//! `PermissionRequest` hook says that.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Codex 0.153.2, and the desktop app's rollouts up to 0.162.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::tail::{self, Lines, MAX_LINE, Tail};
use super::{Event, Hook, Observation, Seed, Source, Start, Told, Unread};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness};
use crate::queue::TurnKey;

const fn file(signal: Signal, record: &'static str, cite: &'static str) -> Declared {
    Declared {
        signal,
        channel: Channel::File,
        evidence: Evidence::Observed,
        record,
        cite,
    }
}

/// What Codex's files and its one hook tell, all of it seen in the probe's
/// runs or in the rollouts Codex had written before them.
pub const SIGNALS: &[Declared] = &[
    file(
        Signal::Started,
        "a thread's lock held, with a rollout whose session_meta is not a subagent's: neither its source nor its thread_source names one",
        "probe-codex runA, runB, runD; desktop rollouts 0.162",
    ),
    file(
        Signal::Prompted,
        "event_msg task_started, keyed by its turn_id",
        "probe-codex runA, runB, runC",
    ),
    file(
        Signal::QuestionAsked,
        "a function_call to request_user_input, which blocks; not request_user_input_async, which does not",
        "probe-codex runA PROMPT2; desktop rollouts 0.162",
    ),
    file(
        Signal::Answered,
        "the output for the pending question's call_id; or, after the approval hook or a daemon start inside a turn, the next call output, completed item that is not a subagent's, or task_complete",
        "probe-codex runA PROMPT2, DENY-ESC; codex2 (approved)",
    ),
    file(
        Signal::TurnEnded,
        "event_msg task_complete",
        "probe-codex runA, runB",
    ),
    file(
        Signal::TurnEndedAsking,
        "task_complete whose last_agent_message's last line ends in a question mark, or that holds a <proposed_plan> block, which no rollout has shown yet",
        "probe-codex runA PROMPT6",
    ),
    file(
        Signal::TurnFailed,
        "task_complete with an error",
        "probe-codex runC; 119 such records in earlier rollouts",
    ),
    file(
        Signal::Interrupted,
        "event_msg turn_aborted for any reason but replaced, which a new turn follows; a cancelled approval writes it too",
        "probe-codex runA CTRLC-INTERRUPT, DENY-ESC; codex2",
    ),
    file(
        Signal::Backgrounded,
        "task_complete while a subagent the turn started, or gave a followup_task, has not completed",
        "probe-codex runA PROMPT5; desktop rollout 2026-10-13",
    ),
    file(
        Signal::BackgroundDone,
        "the last such subagent's SubAgentActivity completed or interrupted, after the turn ended",
        "probe-codex runA PROMPT5; desktop rollout 2026-10-13",
    ),
    file(
        Signal::Ended,
        "the thread's lock deleted, or taken again by another process; or left unlocked, as a killed process leaves it",
        "probe-codex runA, runB (kill -9), runC",
    ),
    Declared {
        signal: Signal::PermissionAsked,
        channel: Channel::Hook,
        evidence: Evidence::Observed,
        record: "PermissionRequest naming the turn the rollout has open",
        cite: "0.5.0 hooks during probe-codex runA PROMPT4 and codex2",
    },
];

/// The hook Codex runs when an approval waits, which no file records.
const PERMISSION_REQUEST: &str = "PermissionRequest";

/// The first record of a rollout, saying whose thread it is.
#[derive(Deserialize)]
struct Meta {
    #[serde(rename = "type")]
    kind: String,
    payload: MetaPayload,
}

#[derive(Deserialize)]
struct MetaPayload {
    #[serde(default)]
    cwd: String,
    /// `cli`, `exec` or `vscode` for a thread someone started; an object
    /// naming a subagent for Codex's own, `{"subagent": "memory_consolidation"}`
    /// for the one that consolidates its memories.
    source: Option<Value>,
    /// `user`, `subagent`, or `agent_created_thread` for a thread the desktop
    /// app shows that an agent opened; older rollouts leave it out.
    thread_source: Option<String>,
}

impl Meta {
    /// Whether the thread is one the user sees: not a subagent, whose
    /// parent's rollout tells of it, nor an agent Codex runs for itself.
    fn users(&self) -> bool {
        let payload = &self.payload;
        let subagent = payload
            .source
            .as_ref()
            .is_some_and(|source| source.get("subagent").is_some());
        self.kind == "session_meta"
            && !subagent
            && payload.thread_source.as_deref() != Some("subagent")
    }
}

/// One rollout record, with only the fields that carry state.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    #[serde(default)]
    payload: Payload,
}

#[derive(Default, Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    turn_id: Option<String>,
    last_agent_message: Option<String>,
    /// Why a turn was aborted: `interrupted`, or `replaced` by a new turn.
    reason: Option<String>,
    error: Option<IgnoredAny>,
    /// A function call's tool, with or without its namespace.
    name: Option<String>,
    call_id: Option<String>,
    item: Option<Item>,
}

/// An item an `item_completed` record completes.
#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type", default)]
    kind: String,
    /// A SubAgentActivity's call: the parent's for `started` and
    /// `interacted`.
    id: Option<String>,
    /// What a SubAgentActivity says of the subagent: started, interacted,
    /// completed or interrupted.
    #[serde(rename = "kind")]
    activity: Option<String>,
    agent_thread_id: Option<String>,
}

impl Record {
    fn read(line: &[u8]) -> Option<Record> {
        serde_json::from_slice(line).ok()
    }

    /// Whether the record was written before a process took the thread's
    /// lock at `start`, by a process that ran the thread before it.
    fn before(&self, start: i64) -> bool {
        written(self.timestamp.as_deref()) < start
    }

    fn is(&self, kind: &str, payload: &str) -> bool {
        self.kind == kind && self.payload.kind.as_deref() == Some(payload)
    }

    fn starts_turn(&self) -> bool {
        self.is("event_msg", "task_started")
    }

    fn ends_turn(&self) -> bool {
        self.is("event_msg", "task_complete") || self.is("event_msg", "turn_aborted")
    }

    /// Whether only a running turn writes the record: the model's output,
    /// a tool's call or result, or an item it completed.
    fn is_work(&self) -> bool {
        self.kind == "response_item" || self.is("event_msg", "item_completed")
    }

    /// The call this record makes to `tool`, if it is one.
    fn calls(&self, tool: &str) -> Option<&str> {
        let name = self.payload.name.as_deref()?.rsplit('.').next()?;
        let calls = self.is("response_item", "function_call") && name == tool;
        calls.then_some(self.payload.call_id.as_deref()?)
    }

    /// The call a blocking question to the user makes, if this is one.
    fn question(&self) -> Option<&str> {
        self.calls("request_user_input")
    }

    /// The call that gives a subagent a task and starts its turn, if this
    /// is one. A `send_message` to a subagent starts none.
    fn followup(&self) -> Option<&str> {
        self.calls("followup_task")
    }

    /// The call a tool's output answers, if this is an output.
    fn output(&self) -> Option<&str> {
        let output = self.kind == "response_item"
            && self
                .payload
                .kind
                .as_deref()
                .is_some_and(|kind| kind.ends_with("_call_output"));
        output.then_some(self.payload.call_id.as_deref()?)
    }

    /// What a SubAgentActivity record says, with the subagent it is about
    /// and the call that made it.
    fn subagent(&self) -> Option<(&str, &str, Option<&str>)> {
        let item = self.payload.item.as_ref()?;
        if !self.is("event_msg", "item_completed") || item.kind != "SubAgentActivity" {
            return None;
        }
        let activity = item.activity.as_deref()?;
        Some((
            activity,
            item.agent_thread_id.as_deref()?,
            item.id.as_deref(),
        ))
    }

    /// Whether the record completes an item of the turn's own, which only a
    /// turn that goes on writes.
    fn completes_item(&self) -> bool {
        self.is("event_msg", "item_completed") && self.subagent().is_none()
    }
}

/// Whether a final message is a plan put to the user for review.
fn proposes_plan(message: &str) -> bool {
    let mut lines = message.lines().map(str::trim);
    lines.any(|line| line == "<proposed_plan>") && lines.any(|line| line == "</proposed_plan>")
}

/// What the files have said of one thread the user runs.
struct Thread {
    /// When the lock was last taken: a process taking the thread writes it.
    taken: SystemTime,
    /// The same, as a Unix second, to set beside the records' times.
    start: i64,
    rollout: PathBuf,
    tail: Tail,
    /// Whether the last read of the rollout stopped at its budget.
    behind: bool,
    /// Whether a turn started and has not ended.
    open: bool,
    turn: Option<TurnKey>,
    /// The call of a question the turn waits on.
    question: Option<String>,
    /// Whether an approval may wait, which nothing is written for: the
    /// hook said one does, or the daemon started inside the turn.
    approval: bool,
    /// The subagents the turn started, or gave a follow-up task, that have
    /// not finished. A turn's start clears them: one that never says it
    /// finished keeps only the turn it worked for in the background.
    children: HashSet<String>,
    /// The turn's follow-up task calls whose subagent is not known yet.
    followups: HashSet<String>,
    /// Whether the last turn ended with subagents still at work.
    background: bool,
    told: Told,
}

/// Codex's threads, from its locks and rollouts.
pub struct Codex {
    locks_dir: PathBuf,
    sessions_dir: PathBuf,
    threads: HashMap<String, Thread>,
    /// Threads that are not the user's, by their rollout, never read again.
    others: HashSet<String>,
}

/// What a changed path is to the source: a thread's lock, or its rollout.
enum Changed {
    Lock(String),
    Rollout(String, PathBuf),
}

impl Codex {
    pub fn new(home: &Path) -> Codex {
        Codex {
            locks_dir: home.join(".codex/thread-writer-locks"),
            sessions_dir: home.join(".codex/sessions"),
            threads: HashMap::new(),
            others: HashSet::new(),
        }
    }

    fn lock(&self, id: &str) -> PathBuf {
        self.locks_dir.join(format!("{id}.lock"))
    }

    fn changed_path(&self, path: &Path) -> Option<Changed> {
        let name = path.file_name()?.to_str()?;
        if path.parent() == Some(&self.locks_dir) {
            let id = name.strip_suffix(".lock")?;
            return Some(Changed::Lock(id.to_owned()));
        }
        if !path.starts_with(&self.sessions_dir) {
            return None;
        }
        let id = rollout_id(name)?;
        Some(Changed::Rollout(id.to_owned(), path.to_owned()))
    }

    /// The thread every lock file names.
    fn locked_ids(&self) -> Vec<String> {
        let entries = fs::read_dir(&self.locks_dir)
            .into_iter()
            .flatten()
            .flatten();
        let names = entries.filter_map(|entry| entry.file_name().into_string().ok());
        names
            .filter_map(|name| Some(name.strip_suffix(".lock")?.to_owned()))
            .filter(|id| !id.starts_with('.'))
            .collect()
    }

    /// A thread's rollout, found by its name in the date folders, which are
    /// listed and never computed: the folder is the local date the thread
    /// began, and a resume appends to it there.
    fn find(&self, id: &str) -> Option<PathBuf> {
        let suffix = format!("-{id}.jsonl");
        let mut folders = vec![(self.sessions_dir.clone(), 0)];
        while let Some((folder, depth)) = folders.pop() {
            for entry in fs::read_dir(&folder).into_iter().flatten().flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if depth < 3 && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    folders.push((path, depth + 1));
                } else if name.starts_with("rollout-") && name.ends_with(&suffix) {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Looks at a thread's lock again: a process that took it, quit, or
    /// handed it to another. A rollout the caller knows of is `found`.
    /// True when it starts following the thread, whose rollout is then to
    /// be read back; the rollout of a running thread that cannot be read
    /// is not followed, and is looked at again at its next change.
    fn look(
        &mut self,
        k: &dyn Kernel,
        id: &str,
        found: Option<&Path>,
        out: &mut Vec<Observation>,
    ) -> Result<bool, Unread> {
        let lock = self.lock(id);
        let taken = fs::metadata(&lock).and_then(|metadata| metadata.modified());
        if let Some(thread) = self.threads.get(id) {
            match &taken {
                // The process that ran it quit.
                Err(_) => {}
                // Still the process that took it. A killed process leaves
                // its lock unlocked, which the sweep finds.
                Ok(taken) if *taken == thread.taken => return Ok(false),
                // Taken by another process, as a resume after a crash does:
                // the one before is gone.
                Ok(_) => {}
            }
            self.threads.remove(id);
            out.push(Observation::new(
                Harness::Codex,
                id,
                Channel::File,
                Event::one(Signal::Ended),
                None,
            ));
        }
        let Ok(taken) = taken else {
            self.others.remove(id);
            return Ok(false);
        };
        if self.others.contains(id) {
            return Ok(false);
        }
        let liveness = Liveness::Lock { path: lock };
        // Left behind by a process that was killed.
        if !liveness::alive(k, &liveness) {
            return Ok(false);
        }
        // A thread opened in the TUI has no rollout until its first prompt.
        let Some(rollout) = found.map(Path::to_owned).or_else(|| self.find(id)) else {
            return Ok(false);
        };
        // A first line caught half written is read again at the next event.
        let Some(meta) = meta(&rollout).map_err(|error| Unread::new(&rollout, error))? else {
            return Ok(false);
        };
        if !meta.users() {
            self.others.insert(id.to_owned());
            return Ok(false);
        }
        let start = taken
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs() as i64);
        // At the end until the rollout is read back.
        let tail = Tail::at_end(&rollout).map_err(|error| Unread::new(&rollout, error))?;
        out.push(Observation::new(
            Harness::Codex,
            id,
            Channel::File,
            Event::Started(Start {
                cwd: meta.payload.cwd,
                liveness,
            }),
            None,
        ));
        self.threads.insert(
            id.to_owned(),
            Thread {
                taken,
                start,
                rollout,
                tail,
                behind: false,
                open: false,
                turn: None,
                question: None,
                approval: false,
                children: HashSet::new(),
                followups: HashSet::new(),
                background: false,
                told: Told::default(),
            },
        );
        Ok(true)
    }

    /// Follows a thread: its lock, then what its rollout says.
    fn follow(
        &mut self,
        k: &dyn Kernel,
        id: &str,
        found: Option<&Path>,
        out: &mut Vec<Observation>,
    ) {
        match self.look(k, id, found, out) {
            Ok(true) => {
                if let Err(unread) = self.read_back(id, out) {
                    eprintln!(
                        "agent-leds: reading {}: {}",
                        unread.path.display(),
                        unread.error
                    );
                }
            }
            Ok(false) => {}
            // Looked at again at its next change.
            Err(_) => return,
        }
        self.read(id, out);
    }

    /// Reads what was appended to a thread's rollout.
    fn read(&mut self, id: &str, out: &mut Vec<Observation>) {
        let Some(thread) = self.threads.get_mut(id) else {
            return;
        };
        let lines = match thread.tail.read(&thread.rollout) {
            Ok(Lines::Read { lines, more }) => {
                thread.behind = more;
                lines
            }
            Ok(Lines::Replaced) => {
                if let Ok(tail) = Tail::at_end(&thread.rollout) {
                    thread.tail = tail;
                }
                thread.behind = false;
                return;
            }
            Err(_) => {
                thread.behind = false;
                return;
            }
        };
        for line in lines {
            if let Some(record) = Record::read(&line) {
                says(id, thread, &record, out);
            }
        }
    }

    /// Reads the rollout of a thread `look` started following back from its
    /// end, to where its last turn began or as far as `tail_back` reads, and
    /// tells what that says: at most 16 MiB of a resumed thread's rollout,
    /// which can be gigabytes long. What was written before
    /// the lock was taken is history of a process before this one, as after
    /// a resume or a crash, and says nothing.
    fn read_back(&mut self, id: &str, out: &mut Vec<Observation>) -> Result<(), Unread> {
        let thread = self.threads.get_mut(id).expect("started");
        let start = thread.start;
        // Back to the last turn's start, or to what an earlier process wrote.
        let read = tail::tail_back(&thread.rollout, |line| {
            Record::read(line).is_some_and(|record| record.starts_turn() || record.before(start))
        });
        let lines = match read {
            Ok((lines, tail)) => {
                thread.tail = tail;
                lines
            }
            // The thread is still followed, from what it writes next.
            Err(error) => return Err(Unread::new(&thread.rollout, error)),
        };
        let read = |line: &Vec<u8>| Record::read(line).filter(|record| !record.before(start));
        let records: Vec<Record> = lines.iter().filter_map(read).collect();
        // A turn's work with neither its start nor its end: the turn began
        // further back than `tail_back` reads.
        let bounded = |record: &Record| record.starts_turn() || record.ends_turn();
        if !records.iter().any(bounded) && records.iter().any(Record::is_work) {
            thread.open = true;
            tell(id, thread, Channel::File, [Signal::Prompted], out);
        }
        for record in &records {
            says(id, thread, record, out);
        }
        Ok(())
    }
}

/// The thread a rollout's file name ends with: `rollout-<time>-<thread>.jsonl`.
/// A compressed or archived rollout is never appended, and never read.
fn rollout_id(name: &str) -> Option<&str> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.split('-').count() == 5).then_some(id)
}

/// A rollout's first record, or None if it is not all written yet.
fn meta(path: &Path) -> io::Result<Option<Meta>> {
    let mut line = Vec::new();
    let file = File::open(path)?;
    BufReader::new(file.take(MAX_LINE as u64)).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Ok(None);
    }
    // A whole first line that is not one is a format it does not know.
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(io::Error::other)
}

/// The Unix second of a record's time, "2026-11-14T21:58:34.284Z"; a record
/// without one is as old as can be.
fn written(timestamp: Option<&str>) -> i64 {
    timestamp.and_then(liveness::iso_second).unwrap_or(i64::MIN)
}

/// What one rollout record says. Each detector says only its own fact; the
/// table's order decides what they come to.
fn says(id: &str, thread: &mut Thread, record: &Record, out: &mut Vec<Observation>) {
    let payload = &record.payload;
    // A turn whose start was not read is named by its other records.
    if thread.open && thread.turn.is_none() {
        thread.turn = payload.turn_id.clone().map(TurnKey);
    }
    if record.starts_turn() {
        // Also after a crash left a turn open: the new one is what runs.
        thread.open = true;
        thread.turn = payload.turn_id.clone().map(TurnKey);
        thread.question = None;
        thread.approval = false;
        thread.children.clear();
        thread.followups.clear();
        thread.background = false;
        tell(id, thread, Channel::File, [Signal::Prompted], out);
    } else if record.is("event_msg", "task_complete") {
        // No approval outlasts its turn, which would otherwise hold the
        // wait against an end told with work running on.
        if thread.approval {
            thread.approval = false;
            tell(id, thread, Channel::File, [Signal::Answered], out);
        }
        let mut end = BTreeSet::from([Signal::TurnEnded]);
        let message = payload.last_agent_message.as_deref().unwrap_or_default();
        if ends_on_question(message) || proposes_plan(message) {
            end.insert(Signal::TurnEndedAsking);
        }
        if payload.error.is_some() {
            end.insert(Signal::TurnFailed);
        }
        if !thread.children.is_empty() {
            end.insert(Signal::Backgrounded);
        }
        thread.background = !thread.children.is_empty();
        close(thread, payload);
        tell(id, thread, Channel::File, end, out);
    } else if record.is("event_msg", "turn_aborted") {
        // A turn replaced by another is followed by that turn's start,
        // which is all there is to tell.
        thread.background = false;
        close(thread, payload);
        if payload.reason.as_deref() != Some("replaced") {
            tell(id, thread, Channel::File, [Signal::Interrupted], out);
        }
    } else if let Some(call) = record.followup() {
        thread.followups.insert(call.to_owned());
    } else if let Some((activity, child, call)) = record.subagent() {
        match activity {
            "started" => {
                thread.children.insert(child.to_owned());
            }
            // A follow-up task starts the subagent's turn; a message does not.
            "interacted" if call.is_some_and(|call| thread.followups.remove(call)) => {
                thread.children.insert(child.to_owned());
            }
            "completed" | "interrupted" => {
                thread.children.remove(child);
                if thread.children.is_empty() && thread.background {
                    thread.background = false;
                    tell(id, thread, Channel::File, [Signal::BackgroundDone], out);
                }
            }
            _ => {}
        }
    } else if let Some(call) = record.question() {
        thread.question = Some(call.to_owned());
        tell(id, thread, Channel::File, [Signal::QuestionAsked], out);
    } else if let Some(call) = record.output()
        && thread.question.as_deref() == Some(call)
    {
        thread.question = None;
        tell(id, thread, Channel::File, [Signal::Answered], out);
    } else if thread.approval && (record.output().is_some() || record.completes_item()) {
        // Nothing is written while an approval waits, so the first record
        // after it says it was answered.
        thread.approval = false;
        tell(id, thread, Channel::File, [Signal::Answered], out);
    }
}

/// Closes the turn a record ends.
fn close(thread: &mut Thread, payload: &Payload) {
    thread.open = false;
    if let Some(turn) = &payload.turn_id {
        thread.turn = Some(TurnKey(turn.clone()));
    }
    thread.question = None;
    thread.approval = false;
}

/// Tells what one record or the hook says.
fn tell(
    id: &str,
    thread: &mut Thread,
    channel: Channel,
    signals: impl IntoIterator<Item = Signal>,
    out: &mut Vec<Observation>,
) {
    out.push(
        thread
            .told
            .observe(Harness::Codex, id, channel, signals, thread.turn.clone()),
    );
}

impl Source for Codex {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.locks_dir.clone(), self.sessions_dir.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Seed> {
        let mut seeded = Vec::new();
        let mut unread = Vec::new();
        for id in self.locked_ids() {
            let mut started = Vec::new();
            match self.look(k, &id, None, &mut started) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    unread.push(error);
                    continue;
                }
            }
            // Only where the rollout leaves the thread is told.
            if let Err(error) = self.read_back(&id, &mut Vec::new()) {
                unread.push(error);
            }
            let thread = self.threads.get_mut(&id).expect("followed");
            // Nothing is written while an approval waits, so one the hook
            // told before the daemon started may wait still: the first
            // record after says none does.
            thread.approval = thread.open && thread.question.is_none();
            let turn = thread.turn.clone();
            for o in &mut started {
                o.turn = turn.clone();
            }
            seeded.extend(started);
            seeded.extend(self.threads[&id].told.current().into_iter().map(|signal| {
                Observation::new(
                    Harness::Codex,
                    &id,
                    Channel::File,
                    Event::one(signal),
                    turn.clone(),
                )
            }));
        }
        for o in &mut seeded {
            o.quiet = true;
        }
        Ok(Seed {
            observations: seeded,
            unread,
        })
    }

    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        // Each thread once, with the rollout a path names.
        let mut changed: Vec<(String, Option<PathBuf>)> = Vec::new();
        let mut add = |id: String, rollout: Option<PathBuf>| match changed
            .iter_mut()
            .find(|(known, _)| *known == id)
        {
            Some((_, known)) => *known = known.take().or(rollout),
            None => changed.push((id, rollout)),
        };
        match paths {
            Some(paths) => {
                for path in paths {
                    match self.changed_path(path) {
                        Some(Changed::Lock(id)) => add(id, None),
                        // A subagent's rollout, written as busily as the
                        // thread it works for, is not looked at again.
                        Some(Changed::Rollout(id, _)) if self.others.contains(&id) => {}
                        Some(Changed::Rollout(id, path)) => add(id, Some(path)),
                        None => {}
                    }
                }
            }
            None => {
                let tracked: Vec<String> = self.threads.keys().cloned().collect();
                for id in self.locked_ids().into_iter().chain(tracked) {
                    add(id, None);
                }
            }
        }
        let mut out = Vec::new();
        for (id, rollout) in changed {
            self.follow(k, &id, rollout.as_deref(), &mut out);
        }
        out
    }

    /// An approval wait, kept only when it is about the turn the rollout
    /// has open once it has been read to its end: a hook that arrives after
    /// its turn ended says nothing.
    fn hook(&mut self, k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let payload = &hook.payload;
        if payload["hook_event_name"] != PERMISSION_REQUEST {
            return Vec::new();
        }
        let (Some(id), Some(turn)) = (payload["session_id"].as_str(), payload["turn_id"].as_str())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        self.follow(k, id, None, &mut out);
        if let Some(thread) = self.threads.get_mut(id)
            && thread.open
            && thread.turn.as_ref().is_some_and(|open| open.0 == turn)
        {
            thread.approval = true;
            tell(
                id,
                thread,
                Channel::Hook,
                [Signal::PermissionAsked],
                &mut out,
            );
        }
        out
    }

    fn behind(&self) -> bool {
        self.threads.values().any(|thread| thread.behind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::Phase;
    use crate::queue::Key;
    use crate::source::fixture::{self, Replay};

    /// The rollout of the thread most fixtures follow.
    const ROLLOUT: &str = ".codex/sessions/2026/11/14/rollout-2026-11-14T15-57-54-01a1dc11-270f-7323-9536-360e91c12739.jsonl";

    fn replay(name: &str) -> Replay {
        fixture::replay(Harness::Codex, name)
    }

    /// The phase of each session the replay left, by id.
    fn phases(replay: &Replay) -> Vec<(String, Phase)> {
        let phases = replay.phases().into_iter();
        phases.map(|(key, phase)| (key.id, phase)).collect()
    }

    /// The phase the replay left its only session in.
    fn phase(replay: &Replay) -> Phase {
        let phases = phases(replay);
        assert_eq!(phases.len(), 1, "{phases:?}");
        phases[0].1
    }

    /// What every observation of the replay told, in order.
    fn told(replay: &Replay) -> Vec<BTreeSet<Signal>> {
        let told = replay.yielded.iter();
        told.map(|(o, _)| o.event.signals()).collect()
    }

    /// Seeds from the real home, reading it and changing nothing, and prints
    /// the phase of each thread Codex's files show running.
    #[test]
    #[ignore = "reads the real ~/.codex"]
    fn seeds_from_the_real_home() {
        let home = std::env::var_os("HOME").expect("a home");
        let mut codex = Codex::new(Path::new(&home));
        let mut queue = crate::queue::Queue::default();
        for o in fixture::printed(codex.seed(&liveness::System, &[]).unwrap()) {
            assert!(o.quiet);
            queue.apply(&o, 0);
        }
        println!("{} running threads", queue.sessions().len());
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
        for name in fixture::names(Harness::Codex) {
            replay(&name);
        }
    }

    #[test]
    fn a_daemon_started_after_any_fixture_seeds_the_phases_it_reached() {
        for name in fixture::names(Harness::Codex) {
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
    fn a_forked_childs_history_burst_is_never_read() {
        let replay = replay("a_forked_childs_history_burst_is_never_read");
        let ids: Vec<String> = phases(&replay).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["01a1dc11-270f-7323-9536-360e91c12739"]);
    }

    #[test]
    fn an_unlocked_lock_after_kill_9_ends_the_session() {
        let replay = replay("an_unlocked_lock_after_kill_9_ends_the_session");
        assert!(replay.phases().is_empty());
        assert!(!fixture::seed(&replay).tracked());
    }

    #[test]
    fn a_completed_subagent_after_the_turn_ends_the_background() {
        let replay = replay("a_completed_subagent_after_the_turn_ends_the_background");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_subagent_that_never_completes_holds_only_its_own_turn() {
        let replay = replay("a_subagent_that_never_completes_holds_only_its_own_turn");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_subagent_given_a_followup_task_keeps_the_turn_in_the_background() {
        let replay = replay("a_subagent_given_a_followup_task_keeps_the_turn_in_the_background");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_resume_seen_with_its_first_prompt_shows_working() {
        let replay = replay("a_resume_seen_with_its_first_prompt_shows_working");
        assert_eq!(phase(&replay), Phase::Working);
    }

    #[test]
    fn a_resume_reads_none_of_the_history_before_its_lock() {
        let replay = replay("a_resumed_rollout_does_not_replay");
        let home = replay.home.path();
        let rollout = home.join(ROLLOUT);
        let lock =
            home.join(".codex/thread-writer-locks/01a1dc11-270f-7323-9536-360e91c12739.lock");
        // Output an earlier process's turn wrote, more than one read takes.
        let output = format!(
            "{{\"timestamp\": \"2026-11-14T21:58:38.000Z\", \"type\": \"response_item\", \"payload\": {{\"type\": \"function_call_output\", \"call_id\": \"call_1\", \"output\": \"{}\"}}}}\n",
            "x".repeat(1 << 20)
        );
        let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
        for _ in 0..20 {
            std::io::Write::write_all(&mut file, output.as_bytes()).unwrap();
        }
        let mut source = replay.source();
        let told = source.changed(&replay.kernel, Some(&[lock]));
        assert!(matches!(
            told[..],
            [Observation {
                event: Event::Started(_),
                ..
            }]
        ));
        assert!(!source.behind());
    }

    #[test]
    fn a_permission_hook_for_a_closed_turn_is_dropped() {
        let replay = replay("a_permission_hook_for_a_closed_turn_is_dropped");
        assert_eq!(phase(&replay), Phase::Idle);
        assert!(
            told(&replay)
                .iter()
                .all(|told| !told.contains(&Signal::PermissionAsked))
        );
    }

    #[test]
    fn the_first_record_after_a_hook_permission_answers_it() {
        let replay = replay("the_first_record_after_a_hook_permission_answers_it");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_turn_that_ends_after_a_late_approval_hook_lets_the_wait_go() {
        let replay = replay("a_turn_that_ends_after_a_late_approval_hook_lets_the_wait_go");
        assert_eq!(phase(&replay), Phase::Background);
    }

    #[test]
    fn a_crashed_open_turn_does_not_show_working_on_resume() {
        let replay = replay("a_crashed_open_turn_does_not_show_working_on_resume");
        assert_eq!(phase(&replay), Phase::Idle);
    }

    #[test]
    fn a_cancelled_approval_is_an_interrupt() {
        assert_eq!(phase(&replay("permission_asked.hook")), Phase::Idle);
    }

    #[test]
    fn a_thread_an_agent_created_is_followed() {
        let replay = replay("a_thread_an_agent_created_is_followed");
        assert_eq!(phase(&replay), Phase::Idle);
    }

    #[test]
    fn a_turn_too_long_to_read_back_to_its_start_still_seeds_as_working() {
        let replay = replay("started.file");
        let rollout = replay.home.path().join(ROLLOUT);
        let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
        // Output the turn's commands wrote, more than a seed reads back.
        let output = format!(
            "{{\"timestamp\": \"2026-11-14T21:58:40.000Z\", \"type\": \"response_item\", \"payload\": {{\"type\": \"function_call_output\", \"call_id\": \"call_1\", \"output\": \"{}\"}}}}\n",
            "x".repeat(1 << 20)
        );
        for _ in 0..20 {
            std::io::Write::write_all(&mut file, output.as_bytes()).unwrap();
        }
        let phases: Vec<Phase> = fixture::seed(&replay)
            .sessions()
            .iter()
            .map(|session| session.phase)
            .collect();
        assert_eq!(phases, [Phase::Working]);
    }

    #[test]
    fn an_async_question_does_not_block() {
        let replay = replay("an_async_question_does_not_block");
        assert_eq!(phase(&replay), Phase::Working);
        assert!(
            told(&replay)
                .iter()
                .all(|told| !told.contains(&Signal::QuestionAsked))
        );
    }

    #[test]
    fn a_memory_consolidation_thread_is_never_started() {
        let replay = replay("a_memory_consolidation_thread_is_never_started");
        assert!(replay.yielded.is_empty());
    }

    #[test]
    fn a_resumed_rollout_does_not_replay() {
        let replay = replay("a_resumed_rollout_does_not_replay");
        assert_eq!(
            told(&replay),
            [Signal::Started, Signal::Prompted, Signal::TurnEnded]
                .map(|signal| BTreeSet::from([signal]))
        );
    }

    #[test]
    fn a_turn_that_ends_on_a_plan_asks_for_its_review() {
        let plan = "<proposed_plan>\n### Test plan\n\n1. Run `sleep 5`.\n</proposed_plan>";
        assert!(proposes_plan(plan));
        assert!(!proposes_plan("Plans go in a `<proposed_plan>` block."));
    }

    #[test]
    fn a_rollout_names_its_thread_at_the_end_of_its_file_name() {
        let id = "01a1dc11-270f-7323-9536-360e91c12739";
        let name = format!("rollout-2026-11-14T15-57-54-{id}.jsonl");
        assert_eq!(rollout_id(&name), Some(id));
        assert_eq!(rollout_id(&format!("{name}.zst")), None);
        assert_eq!(rollout_id("rollout-short.jsonl"), None);
        assert_eq!(rollout_id(&format!("history-{id}.jsonl")), None);
    }

    #[test]
    fn a_rollout_left_partly_read_stays_behind_until_it_is_read_to_its_end() {
        let replay = replay("turn_ended.file");
        let home = replay.home.path();
        let rollout = home.join(ROLLOUT);
        let lock =
            home.join(".codex/thread-writer-locks/01a1dc11-270f-7323-9536-360e91c12739.lock");
        let mut source = replay.source();
        source.seed(&replay.kernel, &[]).unwrap();
        // An image the model was shown, as huge lines.
        let line = format!(
            "{{\"type\": \"response_item\", \"payload\": {{\"type\": \"message\", \"image\": \"{}\"}}}}\n",
            "x".repeat(1 << 20)
        );
        let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
        for _ in 0..9 {
            std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
        }
        source.changed(&replay.kernel, Some(&[rollout]));
        assert!(source.behind());
        // The lock changing first leaves what is unread to read.
        source.changed(&replay.kernel, Some(&[lock]));
        assert!(!source.behind());
    }
}
