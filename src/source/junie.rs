//! Junie, from its own files. Each process writes a sidecar,
//! `processes/<pid>-<session>-<hash>.json`, naming the session it takes and
//! when it took it, and never removes it: one process can take session after
//! session, so the sidecar with the newest `startedAt` for a pid names the one
//! it runs. Each session appends its events to
//! `sessions/<session>/events.jsonl`, which its first prompt creates: the
//! prompt, the task it starts, the waits on the user, a failure or a cancel,
//! the result, and the task's end. No hook is needed.
//!
//! Junie keeps its folder at `$JUNIE_HOME`, or at `.junie` in the account's
//! home directory, which it takes from the system and never from `$HOME`.
//!
//! Junie was not installed for the probe. What its files say was taken from
//! captures of Junie CLI 26.8.24 that others published, Irrlicht's recorded
//! sessions above all, so every signal is inferred. An events log has reached
//! hundreds of gigabytes, as Junie writes terminal output into it, so it is
//! only ever read from its end or from where it began after its session did.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{CStr, OsStr};
use std::fs;
use std::io;
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Deserialize;
use serde::de::IgnoredAny;

use super::tail::{self, Lines, Tail};
use super::{Event, Hook, Observation, Seed, Source, Start, Told, Unread};
use crate::config;
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, StartCheck};
use crate::queue::TurnKey;

const fn file(signal: Signal, record: &'static str, cite: &'static str) -> Declared {
    Declared {
        signal,
        channel: Channel::File,
        evidence: Evidence::Inferred,
        record,
        cite,
    }
}

/// What Junie's files tell, all of it from others' captures.
pub const SIGNALS: &[Declared] = &[
    file(
        Signal::Started,
        "a sidecar in processes/ that is the newest for its pid, whose process started no later than its startedAt; or a CurrentDirectoryUpdatedEvent naming a new folder",
        "probe-junie session_start; Irrlicht pid.go",
    ),
    file(
        Signal::Prompted,
        "a UserPromptEvent, or a TaskStartedEvent, keyed by its taskId",
        "probe-junie prompt_submitted; Irrlicht real-lifecycle-session",
    ),
    file(
        Signal::PermissionAsked,
        "a SessionA2uxEvent in state INPUT_REQUIRED whose block carries an approvalRequest and is not COMPLETED, once per stepId",
        "probe-junie permission_pending; Irrlicht real-terminal-approval",
    ),
    file(
        Signal::QuestionAsked,
        "a SessionA2uxEvent in state INPUT_REQUIRED whose block is an AskAsyncRequestUpdatedEvent or ChoiceRequestUpdatedEvent and is not COMPLETED, once per stepId",
        "probe-junie question_pending; Irrlicht real-lifecycle-session",
    ),
    file(
        Signal::Answered,
        "the last waiting step written again as COMPLETED or in a state other than INPUT_REQUIRED, or a UserAsyncResponseEvent or UserResponseEvent while a step waits",
        "probe-junie permission_resolved, question_answered; Irrlicht real-terminal-approval, real-lifecycle-session",
    ),
    file(
        Signal::TurnEnded,
        "TaskState, in any state, after a task started",
        "probe-junie turn_completed; Irrlicht real-lifecycle-session, parser.go",
    ),
    file(
        Signal::TurnEndedAsking,
        "TaskState after a task started whose last ResultBlockUpdatedEvent result ends its last line in a question mark",
        "probe-junie turn_completed; Irrlicht real-lifecycle-session",
    ),
    file(
        Signal::TurnFailed,
        "TaskState after a task started, when it is FAILED, which only Irrlicht's parser reads, or the task had an AgentFailureEvent or an AgentTaskFailedEvent",
        "probe-junie turn_error; Irrlicht real-failed-session",
    ),
    file(
        Signal::Interrupted,
        "TaskState after a task started that had a CancelAgentEvent",
        "probe-junie interrupted; Irrlicht parser_test.go",
    ),
    file(
        Signal::Ended,
        "a newer sidecar for the pid naming another session, the sidecar deleted, or its process gone",
        "probe-junie session_start notes, quit/crash; Irrlicht pid.go",
    ),
];

/// Junie's folder, as this process's environment gives it to `folder`.
pub fn home() -> PathBuf {
    folder(std::env::var_os("JUNIE_HOME").as_deref())
}

/// Junie's folder given `$JUNIE_HOME`: that, or `.junie` in the account's
/// home directory, which Junie takes from the system and never from `$HOME`.
pub fn folder(junie_home: Option<&OsStr>) -> PathBuf {
    match junie_home {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => account_home().join(".junie"),
    }
}

/// The home directory the system records for the user the daemon runs as,
/// or `$HOME` for an account it has no record of.
fn account_home() -> PathBuf {
    // SAFETY: passwd is plain data, for which all zeroes is a valid value.
    let mut record: libc::passwd = unsafe { mem::zeroed() };
    let mut buffer = vec![0 as libc::c_char; 16 << 10];
    let mut found = ptr::null_mut();
    // SAFETY: the record and the buffer outlive the call, which writes only
    // into them and `found`, and the buffer's length is the one given.
    let status = unsafe {
        libc::getpwuid_r(
            libc::getuid(),
            &mut record,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || record.pw_dir.is_null() {
        return config::home();
    }
    // SAFETY: pw_dir points to a NUL-terminated string in the buffer.
    let dir = unsafe { CStr::from_ptr(record.pw_dir) };
    PathBuf::from(OsStr::from_bytes(dir.to_bytes()))
}

/// A sidecar as Junie writes it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar {
    pid: i32,
    session_id: String,
    #[serde(default)]
    project_path: String,
    /// When the process took the session, in Unix milliseconds.
    started_at: u64,
}

impl Sidecar {
    fn liveness(&self) -> Liveness {
        Liveness::Process {
            pid: self.pid,
            start: StartCheck::NotAfterMs(self.started_at),
        }
    }
}

/// One line of an events log, with only the fields that carry state.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    kind: String,
    task_id: Option<String>,
    /// A TaskState's state.
    state: Option<String>,
    /// What a SessionA2uxEvent wraps.
    event: Option<Envelope>,
    /// When it was written, in Unix milliseconds. A block's goes backwards,
    /// but a prompt's or a task start's is the time of its writing.
    timestamp_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    /// The session's state, stamped on every block update.
    state: Option<String>,
    agent_event: Option<Block>,
}

/// A block of the agent's work, written again each time its status changes,
/// and once more as COMPLETED when its task ends.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Block {
    kind: String,
    step_id: Option<String>,
    status: Option<String>,
    /// Set while a tool waits for the user's approval.
    approval_request: Option<IgnoredAny>,
    /// A ResultBlockUpdatedEvent's final text.
    result: Option<String>,
    /// A CurrentDirectoryUpdatedEvent's folder, often empty.
    current_directory: Option<String>,
}

impl Block {
    /// Whether the update is stamped with the session waiting on the user and
    /// the step is not COMPLETED. A step that waited is answered by an update
    /// that is not open: one COMPLETED, as in the replay of a task's blocks at
    /// its end, or stamped with another state.
    fn open(&self, state: Option<&str>) -> bool {
        state == Some("INPUT_REQUIRED") && self.status.as_deref() != Some("COMPLETED")
    }

    /// What an open step waits on the user for: an approval, a question, or
    /// nothing.
    fn waits(&self, state: Option<&str>) -> BTreeSet<Signal> {
        let mut waits = BTreeSet::new();
        if !self.open(state) {
            return waits;
        }
        if self.approval_request.is_some() {
            waits.insert(Signal::PermissionAsked);
        }
        if matches!(
            self.kind.as_str(),
            "AskAsyncRequestUpdatedEvent" | "ChoiceRequestUpdatedEvent"
        ) {
            waits.insert(Signal::QuestionAsked);
        }
        waits
    }
}

impl Record {
    fn read(line: &[u8]) -> Option<Record> {
        serde_json::from_slice(line).ok()
    }

    fn starts_turn(&self) -> bool {
        matches!(self.kind.as_str(), "UserPromptEvent" | "TaskStartedEvent")
    }

    /// Whether the record was written before `start`, in Unix milliseconds.
    fn before(&self, start: u64) -> bool {
        self.timestamp_ms.is_some_and(|written| written < start)
    }

    /// The session state and block of a SessionA2uxEvent.
    fn block(&self) -> Option<(Option<&str>, &Block)> {
        let envelope = self.event.as_ref()?;
        Some((envelope.state.as_deref(), envelope.agent_event.as_ref()?))
    }

    /// Whether the record says a task is under way or waits on the user.
    fn works(&self) -> bool {
        self.block()
            .is_some_and(|(state, _)| matches!(state, Some("IN_PROGRESS" | "INPUT_REQUIRED")))
    }
}

/// What the task under way has said of itself, which its end is told with.
#[derive(Default)]
struct Task {
    failed: bool,
    cancelled: bool,
    result: String,
}

/// What the files have said of one session a process runs.
struct Session {
    /// The sidecar of the process that runs it.
    sidecar: PathBuf,
    liveness: Liveness,
    cwd: String,
    log: Option<Tail>,
    /// Whether the last read of the log stopped at its budget.
    behind: bool,
    /// The task under way, from its start to its TaskState.
    task: Option<Task>,
    turn: Option<TurnKey>,
    /// The steps waiting on the user, by stepId.
    waiting: BTreeSet<String>,
    told: Told,
}

/// Junie's sessions, from its sidecars and events logs.
pub struct Junie {
    processes_dir: PathBuf,
    sessions_dir: PathBuf,
    /// Every sidecar read, by its path.
    sidecars: HashMap<PathBuf, Sidecar>,
    sessions: HashMap<String, Session>,
}

impl Junie {
    /// The source of Junie's folder, `root`.
    pub fn new(root: &Path) -> Junie {
        Junie {
            processes_dir: root.join("processes"),
            sessions_dir: root.join("sessions"),
            sidecars: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    fn is_sidecar(&self, path: &Path) -> bool {
        path.parent() == Some(&self.processes_dir)
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
    }

    /// The session an events log belongs to. The logs of a session's tasks
    /// and subagents are further down, and are never read.
    fn log_of(&self, path: &Path) -> Option<String> {
        let folder = path.parent()?;
        if path.file_name()? != "events.jsonl" || folder.parent() != Some(&self.sessions_dir) {
            return None;
        }
        Some(folder.file_name()?.to_str()?.to_owned())
    }

    fn log(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(id).join("events.jsonl")
    }

    fn sidecar_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.processes_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| self.is_sidecar(path))
            .collect();
        for known in self.sidecars.keys() {
            if !files.contains(known) {
                files.push(known.clone());
            }
        }
        files
    }

    /// Reads one sidecar again. One that cannot be read or is half written
    /// keeps what it said before; the write that finishes it is another event.
    fn read_sidecar(&mut self, path: &Path) -> Result<(), Unread> {
        match fs::read(path) {
            Ok(bytes) => {
                let sidecar = serde_json::from_slice(&bytes).map_err(|e| Unread::new(path, e))?;
                self.sidecars.insert(path.to_owned(), sidecar);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.sidecars.remove(path);
            }
            Err(error) => return Err(Unread::new(path, error)),
        }
        Ok(())
    }

    /// The session each process runs, by the newest sidecar for its pid,
    /// with that sidecar: the newest of them when several processes name one
    /// session. Ties go to the greater path, so every read agrees.
    fn running(&self) -> Vec<(String, PathBuf, Sidecar)> {
        let newer = |(path, sidecar): (&PathBuf, &Sidecar), than: (&PathBuf, &Sidecar)| {
            (sidecar.started_at, path) > (than.1.started_at, than.0)
        };
        let mut owners: HashMap<i32, (&PathBuf, &Sidecar)> = HashMap::new();
        for entry in &self.sidecars {
            let (_, sidecar) = entry;
            match owners.get(&sidecar.pid) {
                Some(&owner) if !newer(entry, owner) => {}
                _ => {
                    owners.insert(sidecar.pid, entry);
                }
            }
        }
        let mut sessions: HashMap<&str, (&PathBuf, &Sidecar)> = HashMap::new();
        for owner in owners.into_values() {
            match sessions.get(owner.1.session_id.as_str()) {
                Some(&other) if !newer(owner, other) => {}
                _ => {
                    sessions.insert(&owner.1.session_id, owner);
                }
            }
        }
        let mut running: Vec<(String, PathBuf, Sidecar)> = sessions
            .into_iter()
            .map(|(id, (path, sidecar))| (id.to_owned(), path.clone(), sidecar.clone()))
            .collect();
        running.sort_by(|a, b| a.0.cmp(&b.0));
        running
    }

    /// Follows the sidecars as they are now: ends each session no process
    /// runs any more, or that another process took, and starts each one a
    /// live process took.
    fn sync(&mut self, k: &dyn Kernel, out: &mut Vec<Observation>) {
        let running = self.running();
        let mut lost: Vec<String> = self
            .sessions
            .iter()
            .filter(|(id, session)| {
                !running.iter().any(|(other, path, sidecar)| {
                    other == *id
                        && *path == session.sidecar
                        && sidecar.liveness() == session.liveness
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        lost.sort();
        for id in &lost {
            self.sessions.remove(id);
        }
        // A sidecar a process that is gone left behind starts nothing.
        let taken: Vec<(String, PathBuf, Sidecar)> = running
            .into_iter()
            .filter(|(id, _, sidecar)| {
                !self.sessions.contains_key(id) && liveness::alive(k, &sidecar.liveness())
            })
            .collect();
        for id in lost {
            if !taken.iter().any(|(other, _, _)| *other == id) {
                out.push(Observation::new(
                    Harness::Junie,
                    &id,
                    Channel::File,
                    Event::one(Signal::Ended),
                    None,
                ));
            }
        }
        for (id, path, sidecar) in taken {
            if let Err(unread) = self.start(&id, path, &sidecar, false, out) {
                eprintln!(
                    "agent-leds: reading {}: {}",
                    unread.path.display(),
                    unread.error
                );
            }
        }
    }

    /// Starts following a session its process took. When `seeding`, its log
    /// is read back to where its last task began, and where that leaves the
    /// session is told after its start. Otherwise the session begins idle: of
    /// a log begun before the process took the session, as for a resume, only
    /// what the process wrote counts; a log the process began is read from
    /// its first line. A log that cannot be read is followed from what is
    /// written to it next.
    fn start(
        &mut self,
        id: &str,
        sidecar_path: PathBuf,
        sidecar: &Sidecar,
        seeding: bool,
        out: &mut Vec<Observation>,
    ) -> Result<(), Unread> {
        self.sessions.insert(
            id.to_owned(),
            Session {
                sidecar: sidecar_path,
                liveness: sidecar.liveness(),
                cwd: sidecar.project_path.clone(),
                log: None,
                behind: false,
                task: None,
                turn: None,
                waiting: BTreeSet::new(),
                told: Told::default(),
            },
        );
        let log = self.log(id);
        let created = created_ms(&log);
        let mut unread = None;
        if seeding
            && created.is_ok()
            && let Err(error) = self.seed_log(id, &log)
        {
            unread = Some(Unread::new(&log, error));
            let session = self.sessions.get_mut(id).expect("just started");
            session.log = Tail::at_end(&log).ok();
        }
        let session = &self.sessions[id];
        out.push(Observation::new(
            Harness::Junie,
            id,
            Channel::File,
            Event::Started(Start {
                cwd: session.cwd.clone(),
                liveness: session.liveness.clone(),
            }),
            session.turn.clone(),
        ));
        if seeding {
            for signal in session.told.current() {
                out.push(Observation::new(
                    Harness::Junie,
                    id,
                    Channel::File,
                    Event::one(signal),
                    session.turn.clone(),
                ));
            }
        } else if created.is_ok_and(|created| created < sidecar.started_at)
            && let Err(error) = self.resume_log(id, &log, sidecar.started_at, out)
        {
            unread = Some(Unread::new(&log, error));
            let session = self.sessions.get_mut(id).expect("just started");
            session.log = Tail::at_end(&log).ok();
        }
        unread.map_or(Ok(()), Err)
    }

    /// Opens the log of a session a process resumed, at its end, and tells
    /// what the process wrote to it already: from the first task it began,
    /// which was written after the process took the session at `start`.
    fn resume_log(
        &mut self,
        id: &str,
        path: &Path,
        start: u64,
        out: &mut Vec<Observation>,
    ) -> io::Result<()> {
        let (lines, tail) = tail::tail_back(path, |line| {
            Record::read(line).is_some_and(|record| record.before(start))
        })?;
        self.sessions.get_mut(id).expect("started").log = Some(tail);
        let records: Vec<Record> = lines
            .iter()
            .filter_map(|line| Record::read(line))
            .skip_while(|record| !record.starts_turn() || record.before(start))
            .collect();
        for record in &records {
            self.record(id, record, out);
        }
        Ok(())
    }

    /// Where the tail of a session's log leaves it: read back to where its
    /// last task began.
    fn seed_log(&mut self, id: &str, path: &Path) -> io::Result<()> {
        let (lines, tail) = tail::tail_back(path, |line| {
            Record::read(line).is_some_and(|record| record.starts_turn())
        })?;
        let records: Vec<Record> = lines.iter().filter_map(|line| Record::read(line)).collect();
        let session = self.sessions.get_mut(id).expect("started");
        session.log = Some(tail);
        let mut history = Vec::new();
        // A task's work without its start: the task began further back.
        if !records.iter().any(Record::starts_turn) && records.iter().any(Record::works) {
            session.task = Some(Task::default());
            tell(id, session, [Signal::Prompted], &mut history);
        }
        for record in &records {
            self.record(id, record, &mut history);
        }
        Ok(())
    }

    /// Reads what was appended to a session's log, from its first line if
    /// it was begun after the session was started.
    fn read_log(&mut self, id: &str, out: &mut Vec<Observation>) {
        let path = self.log(id);
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if session.log.is_none() {
            // Not written yet: the session's first prompt creates it.
            let Ok(tail) = Tail::at_start(&path) else {
                return;
            };
            session.log = Some(tail);
        }
        let tail = session.log.as_mut().expect("opened");
        let lines = match tail.read(&path) {
            Ok(Lines::Read { lines, more }) => {
                session.behind = more;
                lines
            }
            Ok(Lines::Replaced) => {
                session.log = Tail::at_end(&path).ok();
                session.behind = false;
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                session.log = None;
                session.behind = false;
                return;
            }
            // Read again at its next change, not over and over now.
            Err(_) => {
                session.behind = false;
                return;
            }
        };
        for line in lines {
            if let Some(record) = Record::read(&line) {
                self.record(id, &record, out);
            }
        }
    }

    /// What one line of the log says. A task's failure, cancel and result
    /// are told with its end, which Junie writes after each of them.
    fn record(&mut self, id: &str, record: &Record, out: &mut Vec<Observation>) {
        let session = self.sessions.get_mut(id).expect("tracked");
        match record.kind.as_str() {
            "UserPromptEvent" => {
                session.task = Some(Task::default());
                session.turn = None;
                session.waiting.clear();
                tell(id, session, [Signal::Prompted], out);
            }
            // After its prompt, or alone when a task is resumed.
            "TaskStartedEvent" => {
                session.task.get_or_insert_with(Task::default);
                session.turn = record.task_id.clone().map(TurnKey);
                tell(id, session, [Signal::Prompted], out);
            }
            "AgentTaskFailedEvent" => {
                if let Some(task) = &mut session.task {
                    task.failed = true;
                }
            }
            "CancelAgentEvent" => {
                if let Some(task) = &mut session.task {
                    task.cancelled = true;
                }
            }
            // An answer names no step: whatever waited is answered.
            "UserAsyncResponseEvent" | "UserResponseEvent" => {
                if !session.waiting.is_empty() {
                    session.waiting.clear();
                    tell(id, session, [Signal::Answered], out);
                }
            }
            // Junie writes one TaskState for each task, whatever its state.
            "TaskState" => {
                // Only a task that started ends.
                let Some(task) = session.task.take() else {
                    return;
                };
                session.waiting.clear();
                let mut end = BTreeSet::from([Signal::TurnEnded]);
                if record.state.as_deref() == Some("FAILED") || task.failed {
                    end.insert(Signal::TurnFailed);
                }
                if task.cancelled {
                    end.insert(Signal::Interrupted);
                }
                if ends_on_question(&task.result) {
                    end.insert(Signal::TurnEndedAsking);
                }
                tell(id, session, end, out);
            }
            "SessionA2uxEvent" => update(id, session, record, out),
            _ => {}
        }
    }
}

/// What one block update in a SessionA2uxEvent says.
fn update(id: &str, session: &mut Session, record: &Record, out: &mut Vec<Observation>) {
    let Some((state, block)) = record.block() else {
        return;
    };
    // A task whose start was not read is named by its other records.
    if session.task.is_some() && session.turn.is_none() {
        session.turn = record.task_id.clone().map(TurnKey);
    }
    match block.kind.as_str() {
        "AgentFailureEvent" => {
            if let Some(task) = &mut session.task {
                task.failed = true;
            }
        }
        "ResultBlockUpdatedEvent" => {
            if let (Some(task), Some(result)) = (&mut session.task, &block.result) {
                task.result = result.clone();
            }
        }
        "CurrentDirectoryUpdatedEvent" => {
            let folder = block.current_directory.as_deref().unwrap_or_default();
            if !folder.is_empty() && folder != session.cwd {
                session.cwd = folder.to_owned();
                out.push(Observation::new(
                    Harness::Junie,
                    id,
                    Channel::File,
                    Event::Started(Start {
                        cwd: session.cwd.clone(),
                        liveness: session.liveness.clone(),
                    }),
                    session.turn.clone(),
                ));
            }
        }
        _ => {}
    }
    let Some(step) = &block.step_id else {
        return;
    };
    let waits = block.waits(state);
    if !waits.is_empty() {
        if session.waiting.insert(step.clone()) {
            tell(id, session, waits, out);
        }
    } else if !block.open(state) && session.waiting.remove(step) && session.waiting.is_empty() {
        tell(id, session, [Signal::Answered], out);
    }
}

/// When a file was created, in Unix milliseconds.
fn created_ms(path: &Path) -> io::Result<u64> {
    let created = fs::metadata(path)?.created()?;
    Ok(created
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64))
}

/// Tells what one line of the log says.
fn tell(
    id: &str,
    session: &mut Session,
    signals: impl IntoIterator<Item = Signal>,
    out: &mut Vec<Observation>,
) {
    out.push(session.told.observe(
        Harness::Junie,
        id,
        Channel::File,
        signals,
        session.turn.clone(),
    ));
}

impl Source for Junie {
    fn harness(&self) -> Harness {
        Harness::Junie
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.processes_dir.clone(), self.sessions_dir.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Seed> {
        let mut unread = Vec::new();
        for path in self.sidecar_files() {
            unread.extend(self.read_sidecar(&path).err());
        }
        let mut seeded = Vec::new();
        for (id, path, sidecar) in self.running() {
            if liveness::alive(k, &sidecar.liveness()) {
                unread.extend(self.start(&id, path, &sidecar, true, &mut seeded).err());
            }
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
        let mut out = Vec::new();
        let sidecars: Vec<PathBuf> = match paths {
            Some(paths) => paths
                .iter()
                .filter(|path| self.is_sidecar(path))
                .cloned()
                .collect(),
            None => self.sidecar_files(),
        };
        for path in &sidecars {
            // Read again at its next change.
            let _ = self.read_sidecar(path);
        }
        if paths.is_none() || !sidecars.is_empty() {
            self.sync(k, &mut out);
        }
        let mut logs: Vec<String> = match paths {
            Some(paths) => paths.iter().filter_map(|path| self.log_of(path)).collect(),
            None => {
                let mut ids: Vec<String> = self.sessions.keys().cloned().collect();
                ids.sort();
                ids
            }
        };
        // A session started just now reads what its log holds of it.
        for o in &out {
            if let Event::Started(_) = o.event
                && !logs.contains(&o.key.id)
            {
                logs.push(o.key.id.clone());
            }
        }
        for id in logs {
            self.read_log(&id, &mut out);
        }
        out
    }

    fn hook(&mut self, _k: &dyn Kernel, _hook: &Hook) -> Vec<Observation> {
        Vec::new()
    }

    fn behind(&self) -> bool {
        self.sessions.values().any(|session| session.behind)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::fsm::Phase;
    use crate::queue::Key;
    use crate::source::fixture::{self, Replay};

    fn replay(name: &str) -> Replay {
        fixture::replay(Harness::Junie, name)
    }

    /// The phase the replay left its only session in.
    fn phase(replay: &Replay) -> Phase {
        let phases = replay.phases();
        assert_eq!(phases.len(), 1, "{phases:?}");
        phases.into_values().next().unwrap()
    }

    /// Seeds from the real Junie folder, reading it and changing nothing,
    /// and prints the phase of each session its files show running.
    #[test]
    #[ignore = "reads the real ~/.junie"]
    fn seeds_from_the_real_home() {
        let root = home();
        println!("Junie's folder: {}", root.display());
        let mut junie = Junie::new(&root);
        let mut queue = crate::queue::Queue::default();
        for o in fixture::printed(junie.seed(&liveness::System, &[]).unwrap()) {
            assert!(o.quiet);
            queue.apply(&o, 0);
        }
        println!(
            "{} sidecars, {} running sessions",
            junie.sidecars.len(),
            queue.sessions().len()
        );
        for session in queue.sessions() {
            println!(
                "{:<32}  {:<11}  {:<20}  turn {}",
                session.key.id,
                format!("{:?}", session.phase),
                session.label,
                session.turn.as_ref().map_or("-", |turn| turn.0.as_str())
            );
        }
    }

    /// Prints Junie's folder as this process finds it, for the test below,
    /// which runs it under another environment.
    #[test]
    #[ignore = "run by the_folder_is_junie_home_or_the_account_s_never_home"]
    fn prints_the_folder() {
        println!("folder={}", home().display());
    }

    #[test]
    fn the_folder_is_junie_home_or_the_account_s_never_home() {
        let folder = |junie_home: Option<&str>| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "source::junie::tests::prints_the_folder",
                    "--ignored",
                    "--nocapture",
                ])
                .env("HOME", "/nonexistent");
            match junie_home {
                Some(junie_home) => command.env("JUNIE_HOME", junie_home),
                None => command.env_remove("JUNIE_HOME"),
            };
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            assert!(output.status.success(), "{stdout}");
            let line = stdout.lines().find_map(|line| line.strip_prefix("folder="));
            PathBuf::from(line.unwrap_or_else(|| panic!("{stdout}")))
        };
        assert_eq!(folder(None), account_home().join(".junie"));
        assert!(!folder(None).starts_with("/nonexistent"));
        assert_eq!(folder(Some("/tmp/junie")), Path::new("/tmp/junie"));
    }

    #[test]
    fn every_fixture_replays_as_it_expects() {
        for name in fixture::names(Harness::Junie) {
            replay(&name);
        }
    }

    #[test]
    fn a_daemon_started_after_any_fixture_seeds_the_phases_and_turns_it_reached() {
        for name in fixture::names(Harness::Junie) {
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
    fn completed_after_a_failure_is_an_error() {
        assert_eq!(
            phase(&replay("completed_after_a_failure_is_an_error")),
            Phase::Failed
        );
    }

    #[test]
    fn the_completed_replay_does_not_reopen_a_question() {
        let replay = replay("the_completed_replay_does_not_reopen_a_question");
        assert_eq!(phase(&replay), Phase::Stopped);
        let waits = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::QuestionAsked))
            .count();
        assert_eq!(waits, 1);
    }

    #[test]
    fn a_newer_sidecar_for_the_pid_ends_the_older_session() {
        let replay = replay("a_newer_sidecar_for_the_pid_ends_the_older_session");
        let ids: Vec<String> = replay.phases().into_keys().map(|key| key.id).collect();
        assert_eq!(ids, ["session-260830-194120-17p0"]);
    }

    #[test]
    fn a_task_s_turn_is_named_by_its_task_id() {
        let replay = replay("turn_ended.file");
        let session = &replay.queue.sessions()[0];
        assert_eq!(
            session.turn,
            Some(TurnKey("task-260824-174155-3ua6".to_owned()))
        );
    }

    #[test]
    fn a_process_that_is_gone_is_not_seeded() {
        let replay = replay("ended.file");
        assert!(replay.phases().is_empty());
        // The sidecar stays behind; a daemon starting now ignores it.
        assert!(!fixture::seed(&replay).tracked());
    }

    #[test]
    fn a_resumed_session_starts_idle_whatever_its_log_left() {
        let replay = replay("a_resumed_session_starts_idle_whatever_its_log_left");
        let signals: Vec<BTreeSet<Signal>> = replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signals())
            .collect();
        // The killed process's wait is not told again for the one resuming.
        assert_eq!(
            signals,
            [
                Signal::Started,
                Signal::Prompted,
                Signal::Prompted,
                Signal::PermissionAsked,
                Signal::Ended,
                Signal::Started,
                Signal::Prompted
            ]
            .map(|signal| BTreeSet::from([signal]))
        );
        assert_eq!(phase(&replay), Phase::Working);
    }

    #[test]
    fn a_task_whose_start_is_beyond_the_seed_s_window_seeds_working() {
        let replay = replay("prompted.file");
        let log = replay
            .home
            .path()
            .join(".junie/sessions/session-260824-174155-sm5c/events.jsonl");
        let line = serde_json::json!({
            "kind": "SessionA2uxEvent",
            "event": {
                "state": "IN_PROGRESS",
                "agentEvent": {
                    "kind": "TerminalBlockUpdatedEvent",
                    "stepId": "3e0adc81-026b-488e-a22a-2389704716ce",
                    "status": "IN_PROGRESS",
                    "output": "x".repeat(1 << 20)
                }
            },
            "taskId": "task-260824-174337-1avu"
        });
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        for _ in 0..17 {
            std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes()).unwrap();
        }
        let seeded = fixture::seed(&replay);
        let session = &seeded.sessions()[0];
        assert_eq!(session.phase, Phase::Working);
        assert_eq!(
            session.turn,
            Some(TurnKey("task-260824-174337-1avu".to_owned()))
        );
    }

    #[test]
    fn a_log_left_partly_read_stays_behind_until_it_is_read_to_its_end() {
        let replay = replay("turn_ended.file");
        let home = replay.home.path();
        let log = home.join(".junie/sessions/session-260824-174155-sm5c/events.jsonl");
        let mut source = replay.source();
        source.seed(&replay.kernel, &[]).unwrap();
        let line = format!(
            "{{\"kind\": \"SessionA2uxEvent\", \"output\": \"{}\"}}\n",
            "x".repeat(1 << 20)
        );
        let mut file = fs::OpenOptions::new().append(true).open(&log).unwrap();
        for _ in 0..9 {
            std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
        }
        source.changed(&replay.kernel, Some(&[log]));
        assert!(source.behind());
        source.changed(&replay.kernel, None);
        assert!(!source.behind());
    }

    #[test]
    fn a_file_that_cannot_be_read_leaves_nothing_behind() {
        fixture::a_read_that_fails_leaves_nothing_behind(
            &replay("turn_ended.file"),
            ".junie/sessions/session-260824-174155-sm5c/events.jsonl",
        );
    }
}
