//! Claude Code, from its own files. Each running process keeps a session
//! file, `~/.claude/sessions/<pid>.json`, rewritten whole with the session it
//! runs and whether it is busy, idle or waiting on the user. The session's
//! transcript, `~/.claude/projects/<slug>/<session>.jsonl`, is appended a
//! record at a time. The session file says when the turn waits; the
//! transcript says when it starts and how it ends. No hook is needed.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Claude Code 2.1.325 and the desktop app's 2.1.321.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::tail::{self, Lines, Tail};
use super::{Event, Hook, Observation, Seed, Source, Start, Told, Unread};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, StartCheck};
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

/// What Claude Code's files tell, all of it seen in the probe's runs.
pub const SIGNALS: &[Declared] = &[
    file(
        Signal::Started,
        "a session file naming a new pid or session, whose process started at its procStart; not a spare, a bg session, or a run of claude -p or the Agent SDK (entrypoint sdk-*)",
        "probe-claude cc0–cc11; gaps/h1",
    ),
    file(
        Signal::Prompted,
        "a transcript user record, not a tool result, with a promptSource (typed, sdk or a task notification) or an origin (a skill's slash command), keyed by its promptId; or the model writing again after its turn closed, as after a Stop hook that blocks or a compaction",
        "probe-claude cc1, cc3; desktop transcripts 2.1.317, 2.1.321",
    ),
    file(
        Signal::PermissionAsked,
        "the session file going to status waiting, for anything but input needed; or a prompt read while the session file says so",
        "probe-claude cc1, cc4, cc10",
    ),
    file(
        Signal::QuestionAsked,
        "the session file going to status waiting for input needed; or a prompt read while the session file says so",
        "probe-claude cc2, cc9",
    ),
    file(
        Signal::Answered,
        "the session file going from waiting to busy while the turn is open, or after it closed when a subagent it left is let go on; or from waiting to idle while the turn is open, as a denial does before its interrupt",
        "probe-claude cc1, cc2",
    ),
    file(
        Signal::TurnEnded,
        "an end_turn record with text, or a tool result marked claude/endTurn; or waiting to idle after the turn closed while waiting",
        "probe-claude cc1, cc4; desktop transcripts 2.1.299–2.1.321",
    ),
    file(
        Signal::TurnEndedAsking,
        "an end_turn record whose text's last line ends in a question mark",
        "probe-claude cc1, cc2",
    ),
    file(
        Signal::TurnFailed,
        "an assistant record with isApiErrorMessage",
        "probe-claude cc4, cc5",
    ),
    file(
        Signal::Interrupted,
        "a user text starting [Request interrupted by user",
        "probe-claude cc1–cc4",
    ),
    file(
        Signal::Backgrounded,
        "an end_turn record while the session file is busy with delegated work, or waiting to busy after the turn closed",
        "probe-claude cc3, cc4, cc5",
    ),
    file(
        Signal::BackgroundDone,
        "the session file going from busy to idle after a backgrounded turn",
        "probe-claude cc3",
    ),
    file(
        Signal::Ended,
        "the session file deleted or naming another session (/clear), or its process gone",
        "probe-claude cc1, cc2, cc10",
    ),
];

/// What a session file says the process is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Status {
    /// Not said yet: the file's first write.
    Starting,
    /// Idle, or only a shell command running in the background.
    Idle,
    /// Busy, which includes work a turn delegated and left running.
    Busy,
    /// Waiting on the user, for the wait signal given.
    Waiting(Signal),
}

/// A session file as Claude Code writes it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Process {
    pid: i32,
    session_id: String,
    #[serde(default)]
    cwd: String,
    /// When the process started, as `ps` prints lstart, in UTC.
    proc_start: Option<String>,
    /// `interactive`, or `bg` for the desktop app's own sessions.
    kind: Option<String>,
    /// What started the process: `cli` for a terminal, `claude-desktop`, or
    /// `sdk-cli`, `sdk-ts` or `sdk-py` for `claude -p` and the Agent SDK.
    entrypoint: Option<String>,
    /// A process the desktop app keeps warm for a session not begun yet.
    #[serde(default)]
    spare: bool,
    status: Option<String>,
    waiting_for: Option<String>,
}

impl Process {
    fn status(&self) -> Status {
        match (self.status.as_deref(), self.waiting_for.as_deref()) {
            (None, _) => Status::Starting,
            (Some("busy"), _) => Status::Busy,
            (Some("waiting"), Some("input needed")) => Status::Waiting(Signal::QuestionAsked),
            (Some("waiting"), _) => Status::Waiting(Signal::PermissionAsked),
            _ => Status::Idle,
        }
    }
}

/// One transcript record, with only the fields that carry state.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    /// Set on a prompt: typed, sdk, or system for a task notification.
    prompt_source: Option<String>,
    /// Set on what the user sent, a slash command that runs a skill too.
    origin: Option<IgnoredAny>,
    prompt_id: Option<String>,
    timestamp: Option<String>,
    #[serde(default)]
    is_api_error_message: bool,
    /// Set on a subagent's records, which carry the parent's session id.
    #[serde(default)]
    is_sidechain: bool,
    message: Option<Message>,
    /// What the desktop app's tool said of its result: `claude/endTurn` in
    /// its `_meta` when the tool ends the turn.
    mcp_meta: Option<Value>,
}

#[derive(Deserialize)]
struct Message {
    /// `<synthetic>` for a message Claude Code wrote itself.
    model: Option<String>,
    stop_reason: Option<String>,
    content: Content,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

impl Content {
    fn blocks(&self) -> &[Block] {
        match self {
            Content::Text(_) => &[],
            Content::Blocks(blocks) => blocks,
        }
    }
}

impl Record {
    fn read(line: &[u8]) -> Option<Record> {
        let record: Record = serde_json::from_slice(line).ok()?;
        (!record.is_sidechain).then_some(record)
    }

    /// The record's text blocks, or its text.
    fn texts(&self) -> Vec<&str> {
        match self.message.as_ref().map(|message| &message.content) {
            Some(Content::Text(text)) => vec![text],
            Some(Content::Blocks(blocks)) => blocks
                .iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text.as_deref())
                .collect(),
            None => Vec::new(),
        }
    }

    fn is_tool_result(&self) -> bool {
        self.message.as_ref().is_some_and(|message| {
            let blocks = message.content.blocks();
            blocks.iter().any(|block| block.kind == "tool_result")
        })
    }

    /// What starts a turn: a prompt, a task notification, or a slash command
    /// that runs a skill. A command Claude Code runs itself, such as
    /// `/model`, is recorded with neither field.
    fn is_prompt(&self) -> bool {
        self.kind == "user"
            && (self.prompt_source.is_some() || self.origin.is_some())
            && !self.is_tool_result()
    }

    fn is_interrupt(&self) -> bool {
        self.kind == "user"
            && !self.is_prompt()
            && self
                .texts()
                .first()
                .is_some_and(|text| text.starts_with("[Request interrupted by user"))
    }

    /// The final message of a turn that ended, if this record ends one.
    /// One message is written as a record per block, and only the text
    /// block's record ends the turn. In the desktop app a tool can end the
    /// turn instead, with no message after it.
    fn end(&self) -> Option<&str> {
        let message = self.message.as_ref()?;
        let ends =
            |meta: &Value| meta.pointer("/_meta/claude~1endTurn") == Some(&Value::Bool(true));
        if self.kind == "user" && self.mcp_meta.as_ref().is_some_and(ends) {
            return Some("");
        }
        if self.kind != "assistant" || message.stop_reason.as_deref() != Some("end_turn") {
            return None;
        }
        self.texts().last().copied()
    }

    /// Whether the model wrote the record, which only a running turn does.
    fn is_model(&self) -> bool {
        self.kind == "assistant"
            && self
                .message
                .as_ref()
                .is_some_and(|message| message.model.as_deref() != Some("<synthetic>"))
    }

    fn is_error(&self) -> bool {
        self.kind == "assistant" && self.is_api_error_message
    }

    /// Whether the record was written before a process that started at
    /// `start`: an earlier process wrote it, or it was copied.
    fn before(&self, start: i64) -> bool {
        let written = self.timestamp.as_deref().and_then(liveness::iso_second);
        written.is_some_and(|written| written < start)
    }
}

/// A running process, by its session file.
struct Proc {
    session: String,
    pid: i32,
    start: i64,
}

/// What the files have said of one session.
struct Session {
    /// When the process that runs it started, as a Unix second.
    start: i64,
    /// The session file of the process that runs it. A file another process
    /// left naming the same session, as a killed one does before a resume,
    /// says nothing of it.
    owner: PathBuf,
    status: Status,
    transcript: Option<(PathBuf, Tail)>,
    /// Whether the last read of the transcript stopped at its budget.
    behind: bool,
    /// Whether a turn started and has not ended.
    open: bool,
    turn: Option<TurnKey>,
    /// How a turn that closed while the process waited on the user ended,
    /// told once the wait is over.
    closed_waiting: Option<BTreeSet<Signal>>,
    /// Whether the last turn ended with delegated work still running.
    background: bool,
    told: Told,
}

/// Claude Code's sessions, from its session files and transcripts.
pub struct Claude {
    sessions_dir: PathBuf,
    projects_dir: PathBuf,
    procs: HashMap<PathBuf, Proc>,
    sessions: HashMap<String, Session>,
}

impl Claude {
    pub fn new(home: &Path) -> Claude {
        Claude {
            sessions_dir: home.join(".claude/sessions"),
            projects_dir: home.join(".claude/projects"),
            procs: HashMap::new(),
            sessions: HashMap::new(),
        }
    }

    fn is_session_file(&self, path: &Path) -> bool {
        path.parent() == Some(&self.sessions_dir)
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
    }

    /// The session a transcript belongs to. A subagent's transcript is a
    /// level further down, and is never read.
    fn transcript_of(&self, path: &Path) -> Option<String> {
        let folder = path.parent()?;
        if folder.parent() != Some(&self.projects_dir)
            || path
                .extension()
                .is_none_or(|extension| extension != "jsonl")
        {
            return None;
        }
        Some(path.file_stem()?.to_str()?.to_owned())
    }

    /// The session's transcript, found by its name in any project folder:
    /// the folder's name is the cwd cut short, and is never computed. A
    /// session moved to another folder leaves a copy in the old one, which
    /// is written no more, so the one written last is the transcript.
    fn find(&self, session: &str) -> Option<PathBuf> {
        let name = format!("{session}.jsonl");
        fs::read_dir(&self.projects_dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path().join(&name))
            .filter_map(|path| {
                let metadata = fs::metadata(&path).ok()?;
                metadata
                    .is_file()
                    .then_some((metadata.modified().ok()?, path))
            })
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, path)| path)
    }

    fn session_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = fs::read_dir(&self.sessions_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| self.is_session_file(path))
            .collect();
        for known in self.procs.keys() {
            if !files.contains(known) {
                files.push(known.clone());
            }
        }
        files
    }

    /// A session file as it is now, if it names a process to follow.
    fn process(path: &Path) -> io::Result<Option<(Process, i64)>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // A write caught half done is read again at the event that finishes it.
        let process: Process = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        // A run nobody watches, `claude -p` or a program on the Agent SDK,
        // is not shown.
        let unwatched = process
            .entrypoint
            .as_deref()
            .is_some_and(|entrypoint| entrypoint.starts_with("sdk-"));
        if process.spare || process.kind.as_deref() == Some("bg") || unwatched {
            return Ok(None);
        }
        // A start it cannot read is a format it does not know.
        let Some(start) = process.proc_start.as_deref().and_then(liveness::lstart) else {
            let error = format!("procStart {:?} is not a start time", process.proc_start);
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        };
        Ok(Some((process, start)))
    }

    /// Reads one session file again: a process that started or changed
    /// session, one that ended, or a new status, which `edges` collects to
    /// tell after the transcripts are read. One that cannot be read or is
    /// half written keeps what it said before.
    fn session_file(
        &mut self,
        k: &dyn Kernel,
        path: &Path,
        out: &mut Vec<Observation>,
        edges: &mut Vec<(String, Status, Status)>,
    ) -> io::Result<()> {
        let read = Claude::process(path)?;
        let known = self.procs.get(path);
        if let (Some(known), Some((process, start))) = (known, &read)
            && known.session == process.session_id
            && known.pid == process.pid
            && known.start == *start
        {
            match self.sessions.get_mut(&known.session) {
                Some(session) if session.owner != path => return Ok(()),
                Some(session) => {
                    let status = process.status();
                    if session.status != status {
                        edges.push((known.session.clone(), session.status, status));
                        session.status = status;
                    }
                    return Ok(());
                }
                // Ended with the file of another process that ran it, while
                // this one still does: started again below.
                None => {}
            }
        }
        // Gone, or another session in the process (/clear), or another process.
        if let Some(gone) = self.procs.remove(path) {
            self.end(k, &gone.session, path, out);
        }
        let Some((process, start)) = read else {
            return Ok(());
        };
        let liveness = Liveness::Process {
            pid: process.pid,
            start: StartCheck::Second(start),
        };
        // A file left behind by a process that was killed.
        if !liveness::alive(k, &liveness) {
            return Ok(());
        }
        let status = process.status();
        self.procs.insert(
            path.to_owned(),
            Proc {
                session: process.session_id.clone(),
                pid: process.pid,
                start,
            },
        );
        let id = process.session_id;
        out.push(Observation::new(
            Harness::Claude,
            &id,
            Channel::File,
            Event::Started(Start {
                cwd: process.cwd,
                liveness,
            }),
            None,
        ));
        let transcript = self
            .find(&id)
            .and_then(|path| Some((path.clone(), open(&path, start).ok()?)));
        self.sessions.insert(
            id.clone(),
            Session {
                start,
                owner: path.to_owned(),
                status: Status::Starting,
                transcript,
                behind: false,
                open: false,
                turn: None,
                closed_waiting: None,
                background: false,
                told: Told::default(),
            },
        );
        if status != Status::Starting {
            edges.push((id.clone(), Status::Starting, status));
            self.sessions.get_mut(&id).expect("just added").status = status;
        }
        Ok(())
    }

    /// Ends the session the file at `path` named, if that file's process
    /// ran it and no other running process does.
    fn end(&mut self, k: &dyn Kernel, id: &str, path: &Path, out: &mut Vec<Observation>) {
        if !self
            .sessions
            .get(id)
            .is_some_and(|session| session.owner == path)
        {
            return;
        }
        let other = self.procs.iter().find_map(|(other, proc)| {
            let liveness = Liveness::Process {
                pid: proc.pid,
                start: StartCheck::Second(proc.start),
            };
            let alive = proc.session == id && liveness::alive(k, &liveness);
            alive.then(|| (other.clone(), proc.start, liveness))
        });
        let Some((other, start, liveness)) = other else {
            self.sessions.remove(id);
            out.push(Observation::new(
                Harness::Claude,
                id,
                Channel::File,
                Event::one(Signal::Ended),
                None,
            ));
            return;
        };
        // The same session resumed in another process, which goes on with it.
        let status = Claude::process(&other)
            .ok()
            .flatten()
            .map_or(Status::Starting, |(process, _)| process.status());
        let session = self.sessions.get_mut(id).expect("owned");
        session.start = start;
        session.owner = other;
        session.status = status;
        out.push(Observation::new(
            Harness::Claude,
            id,
            Channel::File,
            Event::Started(Start {
                cwd: String::new(),
                liveness,
            }),
            None,
        ));
    }

    /// Reads what was appended to a session's transcript.
    fn transcript(&mut self, id: &str, path: &Path, out: &mut Vec<Observation>) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        if let Some((known, tail)) = &mut session.transcript
            && known != path
        {
            // The session moved to another folder: its transcript was copied
            // there and is written on there. Another file of the same name
            // that holds less than was read is not that copy.
            if !tail.moved(path).unwrap_or(false) {
                return;
            }
            *known = path.to_owned();
        }
        if session.transcript.is_none() {
            let Ok(tail) = open(path, session.start) else {
                return;
            };
            session.transcript = Some((path.to_owned(), tail));
        }
        let (_, tail) = session.transcript.as_mut().expect("opened");
        let lines = match tail.read(path) {
            Ok(Lines::Read { lines, more }) => {
                session.behind = more;
                lines
            }
            Ok(Lines::Replaced) => {
                session.transcript = Tail::at_end(path).ok().map(|tail| (path.to_owned(), tail));
                session.behind = false;
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                session.transcript = None;
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
            self.line(id, &line, out);
        }
    }

    fn line(&mut self, id: &str, line: &[u8], out: &mut Vec<Observation>) {
        if let Some(record) = Record::read(line) {
            self.record(id, &record, out);
        }
    }

    /// What one transcript record says, read against the session file as it
    /// is now. Only what the session's process wrote counts.
    fn record(&mut self, id: &str, record: &Record, out: &mut Vec<Observation>) {
        let session = self.sessions.get_mut(id).expect("tracked");
        if record.before(session.start) {
            return;
        }
        // The model writing after its turn closed goes on with that turn, as
        // after a Stop hook that blocks or a compaction: what ends it is
        // still to come. That is told before what the record itself says,
        // which can be the end.
        let goes_on = !session.open && record.is_model();
        if record.is_prompt() || goes_on {
            session.open = true;
            if record.is_prompt() {
                session.turn = record.prompt_id.clone().map(TurnKey);
            }
            session.closed_waiting = None;
            session.background = false;
            // A prompt written after the session file said it waits, as a
            // permission asked at once is, is told with the wait.
            let mut started = BTreeSet::from([Signal::Prompted]);
            if let Status::Waiting(wait) = session.status {
                started.insert(wait);
            }
            tell(id, session, started, out);
        }
        // A turn whose start was not read is named by its other records.
        if session.open && session.turn.is_none() {
            session.turn = record.prompt_id.clone().map(TurnKey);
        }
        // Only a turn that is open ends.
        if !session.open {
            return;
        }
        let mut end = BTreeSet::new();
        if record.is_interrupt() {
            end.insert(Signal::Interrupted);
        }
        if record.is_error() {
            end.insert(Signal::TurnFailed);
        }
        if let Some(text) = record.end() {
            end.insert(Signal::TurnEnded);
            if ends_on_question(text) {
                end.insert(Signal::TurnEndedAsking);
            }
            // Work the turn delegated runs on.
            if session.status == Status::Busy {
                end.insert(Signal::Backgrounded);
            }
        }
        if end.is_empty() {
            return;
        }
        session.open = false;
        session.background = end.contains(&Signal::Backgrounded);
        match session.status {
            // A subagent's request holds the session up after its turn: how
            // the turn ended is told once the wait is over.
            Status::Waiting(_) => session.closed_waiting = Some(end),
            _ => tell(id, session, end, out),
        }
    }

    /// Where the tail of a session's transcript leaves it, read against the
    /// session file as it is now: back to where its last turn began, or to
    /// what an earlier process wrote.
    fn seed_transcript(&mut self, id: &str, path: &Path) -> io::Result<()> {
        let start = self.sessions[id].start;
        let anchors = |record: &Record| record.is_prompt() || record.before(start);
        let (lines, tail) = tail::tail_back(path, |line| {
            Record::read(line).is_some_and(|record| anchors(&record))
        })?;
        let records: Vec<Record> = lines.iter().filter_map(|line| Record::read(line)).collect();
        let session = self.sessions.get_mut(id).expect("started");
        session.transcript = Some((path.to_owned(), tail));
        let mut history = Vec::new();
        // A turn's work without its start: the turn began further back.
        let work = |record: &Record| record.is_model() || record.is_tool_result();
        if !records.iter().any(anchors) && records.iter().any(work) {
            session.open = true;
            tell(id, session, [Signal::Prompted], &mut history);
        }
        for record in &records {
            self.record(id, record, &mut history);
        }
        Ok(())
    }

    /// What a change of the session file's status says, once the
    /// transcript has been read up to it.
    fn edge(&mut self, id: &str, from: Status, to: Status, out: &mut Vec<Observation>) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        match (from, to) {
            (_, Status::Waiting(wait)) if from != to => tell(id, session, [wait], out),
            (Status::Waiting(_), Status::Busy) if session.open => {
                tell(id, session, [Signal::Answered], out)
            }
            // Denied or dismissed: the turn's end, an interrupt, follows.
            (Status::Waiting(_), Status::Idle | Status::Starting) if session.open => {
                tell(id, session, [Signal::Answered], out)
            }
            // A subagent was let go on after the turn ended: the wait is
            // over, then how the turn ended is told, with the work going on.
            (Status::Waiting(_), Status::Busy) => {
                let mut end = session.closed_waiting.take().unwrap_or_default();
                end.insert(Signal::Backgrounded);
                session.background = true;
                tell(id, session, [Signal::Answered], out);
                tell(id, session, end, out);
            }
            (Status::Waiting(_), Status::Idle | Status::Starting) if !session.open => {
                let end = session.closed_waiting.take();
                let end = end.or(session
                    .background
                    .then(|| BTreeSet::from([Signal::TurnEnded])));
                session.background = false;
                if let Some(end) = end {
                    tell(id, session, end, out);
                }
            }
            (Status::Busy, Status::Idle | Status::Starting)
                if !session.open && session.background =>
            {
                session.background = false;
                tell(id, session, [Signal::BackgroundDone], out);
            }
            _ => {}
        }
    }
}

/// A session's transcript, opened at its start if the session's process,
/// which started at `start`, created it, as for a new session or after
/// /clear; at its end if not, as for a resumed one, whose transcript so far is
/// history.
fn open(path: &Path, start: i64) -> io::Result<Tail> {
    let created = fs::metadata(path)?.created()?;
    let created = created
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if created as i64 >= start {
        Tail::at_start(path)
    } else {
        Tail::at_end(path)
    }
}

/// Tells what one record or one change of the session file says.
fn tell(
    id: &str,
    session: &mut Session,
    signals: impl IntoIterator<Item = Signal>,
    out: &mut Vec<Observation>,
) {
    out.push(session.told.observe(
        Harness::Claude,
        id,
        Channel::File,
        signals,
        session.turn.clone(),
    ));
}

impl Source for Claude {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.sessions_dir.clone(), self.projects_dir.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Seed> {
        let mut seeded = Vec::new();
        let mut unread = Vec::new();
        for path in self.session_files() {
            let mut started = Vec::new();
            let mut edges = Vec::new();
            if let Err(error) = self.session_file(k, &path, &mut started, &mut edges) {
                unread.push(Unread::new(&path, error));
                continue;
            }
            let Some(Event::Started(_)) = started.first().map(|o| &o.event) else {
                continue;
            };
            let id = started[0].key.id.clone();
            if let Some(transcript) = self.find(&id)
                && let Err(error) = self.seed_transcript(&id, &transcript)
            {
                // The session is still followed, from what it writes next.
                unread.push(Unread::new(&transcript, error));
                let session = self.sessions.get_mut(&id).expect("just started");
                session.transcript = Tail::at_end(&transcript)
                    .ok()
                    .map(|tail| (transcript, tail));
            }
            for (id, from, to) in edges {
                self.edge(&id, from, to, &mut Vec::new());
            }
            let turn = self.sessions[&id].turn.clone();
            let current = self.sessions[&id].told.current();
            for o in &mut started {
                o.turn = turn.clone();
            }
            seeded.extend(started);
            seeded.extend(current.into_iter().map(|signal| {
                Observation::new(
                    Harness::Claude,
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
        let mut out = Vec::new();
        let mut edges = Vec::new();
        let files = match paths {
            Some(paths) => paths
                .iter()
                .filter(|path| self.is_session_file(path))
                .cloned()
                .collect(),
            None => self.session_files(),
        };
        for path in files {
            // Read again at its next change.
            let _ = self.session_file(k, &path, &mut out, &mut edges);
        }
        // The transcripts are read against the session files as they are
        // now, and the status changes told after them.
        let mut transcripts: Vec<(String, PathBuf)> = match paths {
            Some(paths) => paths
                .iter()
                .filter_map(|path| Some((self.transcript_of(path)?, path.clone())))
                .collect(),
            None => self
                .sessions
                .iter()
                .filter_map(|(id, session)| {
                    let path = match &session.transcript {
                        Some((path, _)) => path.clone(),
                        None => self.find(id)?,
                    };
                    Some((id.clone(), path))
                })
                .collect(),
        };
        // A session started just now reads what its transcript holds of it.
        for o in &out {
            if let Event::Started(_) = o.event
                && let Some(session) = self.sessions.get(&o.key.id)
                && let Some((path, _)) = &session.transcript
                && !transcripts.iter().any(|(_, known)| known == path)
            {
                transcripts.push((o.key.id.clone(), path.clone()));
            }
        }
        for (id, path) in transcripts {
            self.transcript(&id, &path, &mut out);
        }
        for (id, from, to) in edges {
            self.edge(&id, from, to, &mut out);
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
    use super::*;
    use crate::fsm::Phase;
    use crate::queue::Key;
    use crate::source::fixture::{self, Replay};

    fn replay(name: &str) -> Replay {
        fixture::replay(Harness::Claude, name)
    }

    /// The phase the replay left its only session in.
    fn phase(replay: &Replay) -> Phase {
        let phases = replay.phases();
        assert_eq!(phases.len(), 1, "{phases:?}");
        phases.into_values().next().unwrap()
    }

    /// Seeds from the real home, reading it and changing nothing, and prints
    /// the phase of each session Claude Code's files show running.
    #[test]
    #[ignore = "reads the real ~/.claude"]
    fn seeds_from_the_real_home() {
        let home = std::env::var_os("HOME").expect("a home");
        let mut claude = Claude::new(Path::new(&home));
        let mut queue = crate::queue::Queue::default();
        for o in fixture::printed(claude.seed(&liveness::System, &[]).unwrap()) {
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
        for name in fixture::names(Harness::Claude) {
            replay(&name);
        }
    }

    #[test]
    fn a_daemon_started_after_any_fixture_seeds_the_phases_it_reached() {
        for name in fixture::names(Harness::Claude) {
            let replay = replay(&name);
            // The turn too, which a saved read mark or dismissal is kept by.
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
    fn a_busy_write_before_the_pid_file_is_deleted_never_shows_working() {
        let replay = replay("a_busy_write_before_the_pid_file_is_deleted_never_shows_working");
        let signals: Vec<BTreeSet<Signal>> = replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signals())
            .collect();
        assert!(
            signals
                .iter()
                .all(|told| !told.contains(&Signal::Backgrounded)),
            "{signals:?}"
        );
        assert!(replay.phases().is_empty());
    }

    #[test]
    fn a_status_idle_before_end_turn_still_gives_stopped() {
        let replay = replay("a_status_idle_before_end_turn_still_gives_stopped");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_subagent_permission_after_the_turn_ended_shows_question_then_working() {
        let replay =
            replay("a_subagent_permission_after_the_turn_ended_shows_question_then_working");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn clear_ends_the_old_id_and_starts_the_new_one() {
        let replay = replay("clear_ends_the_old_id_and_starts_the_new_one");
        let phases = replay.phases();
        let ids: Vec<&str> = phases.keys().map(|key| key.id.as_str()).collect();
        assert_eq!(ids, ["281fd1a9-8d2c-4ebf-87ae-7bdc3f6bbeac"]);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_spare_is_never_started() {
        let replay = replay("a_spare_is_never_started");
        let ids: Vec<String> = replay.phases().into_keys().map(|key| key.id).collect();
        assert_eq!(ids, ["0d9a0d3c-5b1e-4e0b-9a51-6f1f2f3e4d5c"]);
    }

    #[test]
    fn a_print_or_sdk_run_is_never_started() {
        assert!(
            replay("a_print_or_sdk_run_is_never_started")
                .phases()
                .is_empty()
        );
    }

    #[test]
    fn a_desktop_permission_prompt_shows_a_question() {
        let replay = replay("a_desktop_permission_prompt_shows_a_question");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_resumed_transcript_does_not_replay() {
        let replay = replay("a_resumed_transcript_does_not_replay");
        let signals: Vec<BTreeSet<Signal>> = replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signals())
            .collect();
        assert_eq!(
            signals,
            [Signal::Started, Signal::Prompted, Signal::TurnEnded]
                .map(|signal| BTreeSet::from([signal]))
        );
    }

    #[test]
    fn a_denied_permission_is_an_interrupt() {
        assert_eq!(
            phase(&replay("a_denied_permission_is_an_interrupt")),
            Phase::Idle
        );
    }

    #[test]
    fn a_killed_process_s_file_never_speaks_for_the_session_resumed_after_it() {
        let replay =
            replay("a_killed_process_s_file_never_speaks_for_the_session_resumed_after_it");
        assert_eq!(phase(&replay), Phase::Working);
    }

    #[test]
    fn a_session_two_processes_run_goes_on_when_one_quits() {
        let replay = replay("a_session_two_processes_run_goes_on_when_one_quits");
        assert_eq!(phase(&replay), Phase::Idle);
        let session = &replay.queue.sessions()[0];
        assert_eq!(
            session.liveness,
            Liveness::Process {
                pid: 90859,
                start: StartCheck::Second(1_794_693_357)
            }
        );
    }

    #[test]
    fn a_slash_command_starts_a_turn() {
        assert_eq!(
            phase(&replay("a_slash_command_starts_a_turn")),
            Phase::Stopped
        );
    }

    #[test]
    fn a_stop_hook_that_blocks_keeps_the_turn_going() {
        assert_eq!(
            phase(&replay("a_stop_hook_that_blocks_keeps_the_turn_going")),
            Phase::Asked
        );
    }

    #[test]
    fn a_turn_compacted_after_an_error_goes_on() {
        assert_eq!(
            phase(&replay("a_turn_compacted_after_an_error_goes_on")),
            Phase::Stopped
        );
    }

    #[test]
    fn the_desktop_s_end_turn_tool_ends_the_turn() {
        assert_eq!(
            phase(&replay("the_desktop_s_end_turn_tool_ends_the_turn")),
            Phase::Stopped
        );
    }

    #[test]
    fn a_transcript_that_cannot_be_read_back_still_leaves_its_session_followed() {
        use std::os::unix::fs::PermissionsExt;
        let replay = replay("turn_ended.file");
        let transcript = replay
            .home
            .path()
            .join(".claude/projects/-Users-me-Code-app/e8f843a7-1f54-44e3-b14d-99f53e9cec7a.jsonl");
        let unreadable = fs::Permissions::from_mode(0o000);
        fs::set_permissions(&transcript, unreadable).unwrap();
        let seeded = fixture::seed(&replay);
        let unread = fixture::unread(&replay);
        fs::set_permissions(&transcript, fs::Permissions::from_mode(0o600)).unwrap();
        let phases: Vec<Phase> = seeded.sessions().iter().map(|s| s.phase).collect();
        assert_eq!(phases, [Phase::Idle]);
        // The seed says what it could not read.
        assert_eq!(unread, [transcript]);
    }

    #[test]
    fn a_session_file_the_seed_cannot_make_sense_of_is_said() {
        let replay = replay("turn_ended.file");
        let session_file = replay.home.path().join(".claude/sessions/90859.json");
        let text = fs::read_to_string(&session_file).unwrap();
        // A start time in a format it does not know, and a file that is not JSON.
        for changed in [text.replace("procStart", "processStart"), "{\"pid\"".into()] {
            crate::files::replace(&session_file, &changed).unwrap();
            assert!(!fixture::seed(&replay).tracked());
            assert_eq!(
                fixture::unread(&replay),
                std::slice::from_ref(&session_file)
            );
        }
        crate::files::replace(&session_file, &text).unwrap();
        assert!(fixture::unread(&replay).is_empty());
    }

    #[test]
    fn a_transcript_left_partly_read_stays_behind_until_it_is_read_to_its_end() {
        let replay = replay("turn_ended.file");
        let home = replay.home.path();
        let transcript = home
            .join(".claude/projects/-Users-me-Code-app/e8f843a7-1f54-44e3-b14d-99f53e9cec7a.jsonl");
        let session_file = home.join(".claude/sessions/90859.json");
        let mut source = replay.source();
        source.seed(&replay.kernel, &[]).unwrap();
        let line = format!(
            "{{\"type\": \"progress\", \"data\": \"{}\"}}\n",
            "x".repeat(1 << 20)
        );
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        for _ in 0..9 {
            std::io::Write::write_all(&mut file, line.as_bytes()).unwrap();
        }
        source.changed(&replay.kernel, Some(&[transcript]));
        assert!(source.behind());
        // Another file changing first leaves what is unread to read.
        source.changed(&replay.kernel, Some(&[session_file]));
        assert!(source.behind());
        source.changed(&replay.kernel, None);
        assert!(!source.behind());
    }

    /// The phase and turn the replay left its only session in.
    fn turn(replay: &Replay) -> (Phase, Option<String>) {
        let session = &replay.queue.sessions()[0];
        (session.phase, session.turn.clone().map(|turn| turn.0))
    }

    #[test]
    fn a_session_moved_to_another_folder_is_followed_there() {
        let replay = replay("a_session_moved_to_another_folder_is_followed_there");
        let moved = Some("546cf2fb-5ed6-4413-82c3-4a52553b6beb".to_owned());
        assert_eq!(turn(&replay), (Phase::Stopped, moved.clone()));
        let seeded = fixture::seed(&replay);
        let session = &seeded.sessions()[0];
        assert_eq!(session.turn.clone().map(|turn| turn.0), moved);
    }

    #[test]
    fn a_prompt_written_before_its_session_file_is_read_still_starts_the_turn() {
        let replay =
            replay("a_prompt_written_before_its_session_file_is_read_still_starts_the_turn");
        let prompted = Some("7258eeab-0b1e-4b0e-8a51-6f1f2f3e4d5c".to_owned());
        assert_eq!(turn(&replay), (Phase::Stopped, prompted));
    }

    #[test]
    fn a_process_that_is_gone_is_not_seeded() {
        let replay = replay("ended.file");
        assert!(replay.phases().is_empty());
        // The killed process left its file; a daemon starting now ignores it.
        assert!(!fixture::seed(&replay).tracked());
    }

    #[test]
    fn a_file_that_cannot_be_read_leaves_nothing_behind() {
        fixture::a_read_that_fails_leaves_nothing_behind(
            &replay("turn_ended.file"),
            ".claude/projects/-Users-me-Code-app/e8f843a7-1f54-44e3-b14d-99f53e9cec7a.jsonl",
        );
    }
}
