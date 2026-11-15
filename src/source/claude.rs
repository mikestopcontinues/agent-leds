//! Claude Code, from its own files. Each running process keeps a session
//! file, `~/.claude/sessions/<pid>.json`, rewritten whole with the session it
//! runs and whether it is busy, idle or waiting on the user. The session's
//! transcript, `~/.claude/projects/<slug>/<session>.jsonl`, is appended a
//! record at a time. The session file says when the turn waits; the
//! transcript says when it starts and how it ends. No hook is needed.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Claude Code 2.1.325 and the desktop app's 2.1.321.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use super::tail::{self, Lines, Tail};
use super::{Event, Hook, Observation, Source, Start, Told};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, StartCheck};
use crate::queue::{Key, TurnKey};

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
        "a session file naming a new pid or session, whose process started at its procStart; not a spare or a bg session",
        "probe-claude cc0–cc11",
    ),
    file(
        Signal::Prompted,
        "a transcript user record with a promptSource (typed, sdk or a task notification), keyed by its promptId",
        "probe-claude cc1, cc3",
    ),
    file(
        Signal::PermissionAsked,
        "the session file going to status waiting, for anything but input needed",
        "probe-claude cc1, cc4, cc10",
    ),
    file(
        Signal::QuestionAsked,
        "the session file going to status waiting for input needed",
        "probe-claude cc2, cc9",
    ),
    file(
        Signal::Answered,
        "the session file going from waiting to busy: while the turn is open, or after it closed, when a subagent it left is let go on",
        "probe-claude cc1, cc2",
    ),
    file(
        Signal::TurnEnded,
        "an end_turn record with text while the session file is idle; or waiting to idle after the turn closed while waiting",
        "probe-claude cc1, cc4",
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
        "an end_turn record while the session file stays busy with delegated work, or waiting to busy after the turn closed",
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
    prompt_id: Option<String>,
    #[serde(default)]
    is_api_error_message: bool,
    /// Set on a subagent's records, which carry the parent's session id.
    #[serde(default)]
    is_sidechain: bool,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
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

    fn is_prompt(&self) -> bool {
        self.kind == "user" && self.prompt_source.is_some()
    }

    fn is_interrupt(&self) -> bool {
        self.kind == "user"
            && self.prompt_source.is_none()
            && self
                .texts()
                .first()
                .is_some_and(|text| text.starts_with("[Request interrupted by user"))
    }

    /// The final message of a turn that ended, if this record ends one.
    /// One message is written as a record per block, and only the text
    /// block's record ends the turn.
    fn end(&self) -> Option<&str> {
        let message = self.message.as_ref()?;
        if self.kind != "assistant" || message.stop_reason.as_deref() != Some("end_turn") {
            return None;
        }
        self.texts().last().copied()
    }

    fn is_error(&self) -> bool {
        self.kind == "assistant" && self.is_api_error_message
    }

    /// Whether the record starts or ends a turn, so a tail that holds one
    /// says where the session stands.
    fn anchors(&self) -> bool {
        self.is_prompt() || self.is_interrupt() || self.end().is_some() || self.is_error()
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
    status: Status,
    transcript: Option<(PathBuf, Tail)>,
    /// Whether a turn started and has not ended.
    open: bool,
    turn: Option<TurnKey>,
    /// The end of a turn that closed while the process waited on the user,
    /// told once the wait is over.
    closed_waiting: Option<Signal>,
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
    behind: bool,
}

impl Claude {
    pub fn new(home: &Path) -> Claude {
        Claude {
            sessions_dir: home.join(".claude/sessions"),
            projects_dir: home.join(".claude/projects"),
            procs: HashMap::new(),
            sessions: HashMap::new(),
            behind: false,
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
    /// the folder's name is the cwd cut short, and is never computed.
    fn find(&self, session: &str) -> Option<PathBuf> {
        let name = format!("{session}.jsonl");
        fs::read_dir(&self.projects_dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path().join(&name))
            .find(|path| path.is_file())
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
        if process.spare || process.kind.as_deref() == Some("bg") {
            return Ok(None);
        }
        let Some(start) = process.proc_start.as_deref().and_then(liveness::lstart) else {
            return Ok(None);
        };
        Ok(Some((process, start)))
    }

    /// Reads one session file again: a process that started or changed
    /// session, one that ended, or a new status, which `edges` collects to
    /// tell after the transcripts are read.
    fn session_file(
        &mut self,
        k: &dyn Kernel,
        path: &Path,
        out: &mut Vec<Observation>,
        edges: &mut Vec<(String, Status, Status)>,
    ) {
        let read = match Claude::process(path) {
            Ok(read) => read,
            // Unreadable or half written: keep what it said before.
            Err(_) => return,
        };
        let known = self.procs.get(path);
        if let (Some(known), Some((process, start))) = (known, &read)
            && known.session == process.session_id
            && known.pid == process.pid
            && known.start == *start
        {
            let status = process.status();
            let session = self.sessions.get_mut(&known.session).expect("tracked");
            if session.status != status {
                edges.push((known.session.clone(), session.status, status));
                session.status = status;
            }
            return;
        }
        // Gone, or another session in the process (/clear), or another process.
        if let Some(gone) = self.procs.remove(path) {
            self.end(&gone.session, out);
        }
        let Some((process, start)) = read else {
            return;
        };
        let liveness = Liveness::Process {
            pid: process.pid,
            start: StartCheck::Second(start),
        };
        // A file left behind by a process that was killed.
        if !liveness::alive(k, &liveness) {
            return;
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
        out.push(observation(
            &id,
            Event::Started(Start {
                cwd: process.cwd,
                liveness,
            }),
            None,
        ));
        // Its transcript so far is history.
        let transcript = self
            .find(&id)
            .and_then(|path| Some((path.clone(), Tail::at_end(&path).ok()?)));
        self.sessions.insert(
            id.clone(),
            Session {
                status: Status::Starting,
                transcript,
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
    }

    fn end(&mut self, id: &str, out: &mut Vec<Observation>) {
        if self.sessions.remove(id).is_some() {
            out.push(observation(id, Event::Signal(Signal::Ended), None));
        }
    }

    /// Reads what was appended to a session's transcript.
    fn transcript(&mut self, id: &str, path: &Path, out: &mut Vec<Observation>) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        // Another file of the same name; the first one found is followed.
        if session
            .transcript
            .as_ref()
            .is_some_and(|(known, _)| known != path)
        {
            return;
        }
        if session.transcript.is_none() {
            // Created after the session started, so all of it is new.
            let Ok(tail) = Tail::at_start(path) else {
                return;
            };
            session.transcript = Some((path.to_owned(), tail));
        }
        let (_, tail) = session.transcript.as_mut().expect("opened");
        let lines = match tail.read(path) {
            Ok(Lines::Read { lines, more }) => {
                self.behind |= more;
                lines
            }
            Ok(Lines::Replaced) => {
                session.transcript = Tail::at_end(path).ok().map(|tail| (path.to_owned(), tail));
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                session.transcript = None;
                return;
            }
            Err(_) => return,
        };
        for line in lines {
            self.line(id, &line, out);
        }
    }

    /// What one transcript record says, read against the session file as it
    /// is now.
    fn line(&mut self, id: &str, line: &[u8], out: &mut Vec<Observation>) {
        let Some(record) = Record::read(line) else {
            return;
        };
        let session = self.sessions.get_mut(id).expect("tracked");
        if record.is_prompt() {
            session.open = true;
            session.turn = record.prompt_id.clone().map(TurnKey);
            session.closed_waiting = None;
            session.background = false;
            // A prompt written after the session file said it waits, as a
            // permission asked at once is, is still waiting.
            let signal = match session.status {
                Status::Waiting(wait) => wait,
                _ => Signal::Prompted,
            };
            tell(id, session, signal, out);
        } else if record.is_interrupt() {
            if session.open {
                session.open = false;
                tell(id, session, Signal::Interrupted, out);
            }
        } else if record.is_error() {
            if session.open {
                session.open = false;
                tell(id, session, Signal::TurnFailed, out);
            }
        } else if let Some(text) = record.end()
            && session.open
        {
            session.open = false;
            let end = if ends_on_question(text) {
                Signal::TurnEndedAsking
            } else {
                Signal::TurnEnded
            };
            match session.status {
                // A subagent's request holds the session up after its turn.
                Status::Waiting(_) => session.closed_waiting = Some(end),
                Status::Busy if end == Signal::TurnEnded => {
                    session.background = true;
                    tell(id, session, Signal::Backgrounded, out);
                }
                _ => tell(id, session, end, out),
            }
        }
    }

    /// What a change of the session file's status says, once the
    /// transcript has been read up to it.
    fn edge(&mut self, id: &str, from: Status, to: Status, out: &mut Vec<Observation>) {
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        match (from, to) {
            (_, Status::Waiting(wait)) if from != to => tell(id, session, wait, out),
            (Status::Waiting(_), Status::Busy) if session.open => {
                tell(id, session, Signal::Answered, out)
            }
            // A subagent was let go on after the turn ended.
            (Status::Waiting(_), Status::Busy) => {
                session.closed_waiting = None;
                session.background = true;
                tell(id, session, Signal::Answered, out);
                tell(id, session, Signal::Backgrounded, out);
            }
            (Status::Waiting(_), Status::Idle | Status::Starting) if !session.open => {
                let end = session.closed_waiting.take();
                let end = end.or(session.background.then_some(Signal::TurnEnded));
                session.background = false;
                if let Some(end) = end {
                    tell(id, session, end, out);
                }
            }
            (Status::Busy, Status::Idle | Status::Starting)
                if !session.open && session.background =>
            {
                session.background = false;
                tell(id, session, Signal::BackgroundDone, out);
            }
            _ => {}
        }
    }
}

fn observation(id: &str, event: Event, turn: Option<TurnKey>) -> Observation {
    Observation {
        key: Key {
            harness: Harness::Claude,
            id: id.to_owned(),
        },
        channel: Channel::File,
        event,
        turn,
        quiet: false,
    }
}

fn tell(id: &str, session: &mut Session, signal: Signal, out: &mut Vec<Observation>) {
    session.told.tell(signal);
    out.push(observation(id, Event::Signal(signal), session.turn.clone()));
}

impl Source for Claude {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.sessions_dir.clone(), self.projects_dir.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Vec<Observation>> {
        let mut seeded = Vec::new();
        for path in self.session_files() {
            let mut started = Vec::new();
            let mut edges = Vec::new();
            self.session_file(k, &path, &mut started, &mut edges);
            let Some(Event::Started(_)) = started.first().map(|o| &o.event) else {
                continue;
            };
            let id = started[0].key.id.clone();
            // Where the transcript's tail leaves the session, read against
            // the session file as it is now.
            if let Some(transcript) = self.find(&id) {
                let (lines, tail) = tail::tail_back(&transcript, |lines| {
                    lines
                        .iter()
                        .any(|line| Record::read(line).is_some_and(|record| record.anchors()))
                })?;
                let session = self.sessions.get_mut(&id).expect("just started");
                session.transcript = Some((transcript, tail));
                let mut history = Vec::new();
                for line in lines {
                    self.line(&id, &line, &mut history);
                }
            }
            for (id, from, to) in edges {
                self.edge(&id, from, to, &mut Vec::new());
            }
            let turn = self.sessions[&id].turn.clone();
            let current = self.current(&id);
            seeded.extend(started);
            seeded.extend(
                current
                    .into_iter()
                    .map(|(signal, _)| observation(&id, Event::Signal(signal), turn.clone())),
            );
        }
        for o in &mut seeded {
            o.quiet = true;
        }
        Ok(seeded)
    }

    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        self.behind = false;
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
            self.session_file(k, &path, &mut out, &mut edges);
        }
        // The transcripts are read against the session files as they are
        // now, and the status changes told after them.
        let transcripts: Vec<(String, PathBuf)> = match paths {
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

    fn current(&self, id: &str) -> Vec<(Signal, Option<TurnKey>)> {
        let Some(session) = self.sessions.get(id) else {
            return Vec::new();
        };
        let current = session.told.current().into_iter();
        current
            .map(|signal| (signal, session.turn.clone()))
            .collect()
    }

    fn behind(&self) -> bool {
        self.behind
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::Phase;
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
        for o in claude.seed(&liveness::System, &[]).unwrap() {
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
            let seeded = fixture::seed(&replay);
            let phases: Vec<(Key, Phase)> = seeded
                .sessions()
                .iter()
                .map(|session| (session.key.clone(), session.phase))
                .collect();
            let replayed: Vec<(Key, Phase)> = replay.phases().into_iter().collect();
            assert_eq!(phases, replayed, "{name}");
        }
    }

    #[test]
    fn a_busy_write_before_the_pid_file_is_deleted_never_shows_working() {
        let replay = replay("a_busy_write_before_the_pid_file_is_deleted_never_shows_working");
        let signals: Vec<Signal> = replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signal())
            .collect();
        assert!(!signals.contains(&Signal::Backgrounded), "{signals:?}");
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
    fn a_resumed_transcript_does_not_replay() {
        let replay = replay("a_resumed_transcript_does_not_replay");
        let signals: Vec<Signal> = replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signal())
            .collect();
        assert_eq!(
            signals,
            [Signal::Started, Signal::Prompted, Signal::TurnEnded]
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
    fn a_process_that_is_gone_is_not_seeded() {
        let replay = replay("ended.file");
        assert!(replay.phases().is_empty());
        // The killed process left its file; a daemon starting now ignores it.
        assert!(!fixture::seed(&replay).tracked());
    }
}
