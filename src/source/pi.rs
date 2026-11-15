//! Pi, from its own files and the events our extension passes on. Each
//! session is a file, `~/.pi/agent/sessions/--<cwd>--/<time>_<session>.jsonl`,
//! appended an entry at a time as each message ends: when a turn starts and
//! how it ends. No file says which process runs a session, when a dialog
//! holds a run up, or whether an error is final, since Pi retries some; the
//! extension tells those, and where the session's file is.
//!
//! What the files and the extension say at each moment was taken from Pi
//! 0.84.4 and 1.5.2. Files nested deeper than a cwd's folder are
//! pi-subagents children, which run without a UI, so the extension never
//! names them and they are never read.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::mem;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use super::tail::{self, Tail};
use super::{Event, Hook, Observation, Seed, Source, Start, Strange, Told, Unread};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{self, Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, StartCheck};
use crate::queue::TurnKey;

/// What Pi's files and our extension tell. The probe's `ext*` runs ran this
/// extension, recording how many lines the session's file held as each hook
/// ran; the others ran Pi alone.
pub const SIGNALS: &[Declared] = &[
    Declared::new(
        Signal::Started,
        Channel::Hook,
        Evidence::Observed,
        "session_start or agent_start from a Pi with a UI, naming its own pid, which is the hook's parent, and its session file",
        "probe-pi ext084, ext152, extfork, extdir, extresume",
    ),
    Declared::new(
        Signal::Prompted,
        Channel::File,
        Evidence::Observed,
        "a message entry with role user, keyed by its entry id; or an assistant entry after the run ended, as when a background subagent's report wakes the agent, keyed by the custom_message entry of customType subagent-notify before it",
        "probe-pi ext152, rpc1, rpc8, rpcsa, extsa",
    ),
    Declared::new(
        Signal::Prompted,
        Channel::Hook,
        Evidence::Observed,
        "agent_start, at every prompt and retry; 0.84.4 writes a new session's first prompt to no file until the reply ends",
        "probe-pi ext084, tq1",
    ),
    Declared::new(
        Signal::QuestionAsked,
        Channel::Hook,
        Evidence::Observed,
        "ui_prompt_start during a run: an extension's dialog holds it up, and nothing is written while it is open",
        "probe-pi ext084, ext152, rpc1, tui1",
    ),
    Declared::new(
        Signal::Answered,
        Channel::File,
        Evidence::Inferred,
        "the first message entry after a dialog the extension reported, such as the tool result its answer gave, when the file is read before ui_prompt_end is handled; Pi itself runs ui_prompt_end first",
        "probe-pi ext084, ext152, where each ui_prompt_end ran before the tool result was written",
    ),
    Declared::new(
        Signal::Answered,
        Channel::Hook,
        Evidence::Observed,
        "ui_prompt_end",
        "probe-pi ext084, ext152",
    ),
    Declared::new(
        Signal::TurnEnded,
        Channel::File,
        Evidence::Observed,
        "an assistant message entry with stopReason stop or length and no toolCall block",
        "probe-pi ext084, rpc1, tui1",
    ),
    Declared::new(
        Signal::TurnEndedAsking,
        Channel::File,
        Evidence::Observed,
        "that entry, when the last line of its joined text blocks ends in a question mark",
        "probe-pi ext084, rpc1, tui1",
    ),
    Declared::new(
        Signal::TurnFailed,
        Channel::Hook,
        Evidence::Observed,
        "agent_settled whose last assistant message has stopReason error; an error entry in the file may be retried, and settles nothing",
        "probe-pi ext084, ext152 fail400, fail500always",
    ),
    Declared::new(
        Signal::Interrupted,
        Channel::File,
        Evidence::Observed,
        "an assistant message entry with stopReason aborted, or error with errorMessage exactly Operation aborted, Request was aborted or This operation was aborted",
        "probe-pi ext084, ext152, rpc1, tui1",
    ),
    Declared::new(
        Signal::Interrupted,
        Channel::Hook,
        Evidence::Observed,
        "agent_settled with aborted: the run's abort signal, taken at agent_start, was aborted, as at a stop mid-stream and 0.84.4's stop during a tool written with stop_reason error; or 1.5.2's agent_settled says aborted, as at a stop during a retry's wait",
        "gaps-pi sig084, sig152",
    ),
    Declared::new(
        Signal::Ended,
        Channel::Hook,
        Evidence::Observed,
        "session_shutdown other than for a reload, as at quit and /new, or the Pi process gone; no file records an exit; inferred: another session's session_start or agent_start from the same Pi process, which runs one session at a time, as after a /new whose session_shutdown was lost",
        "probe-pi ext084, ext152, extfork, tq1–tq6, rpc5–rpc7; inferred from gaps-pi sig084 with its session_shutdown removed",
    ),
];

/// The texts Pi gives an assistant message that the user stopped: in the
/// terminal interface, over RPC, and during a tool.
const ABORTS: [&str; 3] = [
    "Operation aborted",
    "Request was aborted",
    "This operation was aborted",
];

/// One entry of a session file, with only the fields that carry state. The
/// first line, of type `session`, is the file's header.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    /// An ISO time to the millisecond, as JavaScript writes it.
    timestamp: Option<String>,
    /// The header's: the folder the session works in.
    cwd: Option<String>,
    /// A custom_message's kind, such as `subagent-notify`.
    custom_type: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    role: String,
    content: Option<Content>,
    stop_reason: Option<String>,
    error_message: Option<String>,
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

/// The kinds of entry Pi was seen to write, in the sessions of 0.84 and
/// 1.5.2 on the author's Mac and the fixtures.
const KINDS: &[&str] = &[
    "cache_warm",
    "compaction",
    "context_edit",
    "custom",
    "custom_message",
    "label",
    "message",
    "model_change",
    "session",
    "session_info",
    "thinking_level_change",
];

impl Entry {
    /// An entry Pi wrote whole. A line a crash cut short, which Pi ends with
    /// a newline when it loads the file again, is none.
    fn read(line: &[u8]) -> Option<Entry> {
        serde_json::from_slice(line).ok()
    }

    /// The entry on a line, counted in `strange` if it is none or of a kind
    /// Pi was not seen to write.
    fn counted(line: &[u8], strange: &mut Strange) -> Option<Entry> {
        let entry = strange.parsed(Entry::read(line))?;
        strange.kind(&entry.kind, KINDS);
        Some(entry)
    }

    /// What starts a turn: a prompt.
    fn is_prompt(&self) -> bool {
        self.kind == "message" && self.message.as_ref().is_some_and(|m| m.role == "user")
    }

    /// A background subagent's report. It wakes the agent only when sent to,
    /// which the entry does not say: one sent without waking it, as when a
    /// resumed session takes in reports finished while Pi was closed, is
    /// written just the same.
    fn is_report(&self) -> bool {
        self.kind == "custom_message" && self.custom_type.as_deref() == Some("subagent-notify")
    }

    /// Whether the entry is part of the conversation: a message, or a
    /// custom message an extension adds to it. Compactions, labels, model
    /// changes and 1.5.2's cache_warm and context_edit are not.
    fn is_message(&self) -> bool {
        matches!(self.kind.as_str(), "message" | "custom_message")
    }

    fn assistant(&self) -> Option<&Message> {
        let message = self.message.as_ref()?;
        (self.kind == "message" && message.role == "assistant").then_some(message)
    }
}

impl Message {
    /// Whether the user stopped the run this message was part of. On 0.84.4
    /// a stop during a tool is written as an error with the abort's text; a
    /// provider's own error that mentions an abort is none.
    fn is_interrupt(&self) -> bool {
        match self.stop_reason.as_deref() {
            Some("aborted") => true,
            Some("error") => self
                .error_message
                .as_deref()
                .is_some_and(|error| ABORTS.contains(&error)),
            _ => false,
        }
    }

    /// The final text of a turn, if the message ends one: it stopped by
    /// itself and calls no tool.
    fn end(&self) -> Option<String> {
        if !matches!(self.stop_reason.as_deref(), Some("stop" | "length")) {
            return None;
        }
        match &self.content {
            None => Some(String::new()),
            Some(Content::Text(text)) => Some(text.clone()),
            Some(Content::Blocks(blocks)) => {
                if blocks.iter().any(|block| block.kind == "toolCall") {
                    return None;
                }
                let texts: Vec<&str> = blocks
                    .iter()
                    .filter(|block| block.kind == "text")
                    .filter_map(|block| block.text.as_deref())
                    .collect();
                Some(texts.join("\n"))
            }
        }
    }
}

/// A session file's first line, if it is written whole.
fn header(path: &Path) -> io::Result<Option<Entry>> {
    let mut line = Vec::new();
    BufReader::new(File::open(path)?).read_until(b'\n', &mut line)?;
    if line.last() != Some(&b'\n') {
        return Ok(None);
    }
    Ok(Entry::read(&line).filter(|entry| entry.kind == "session"))
}

/// Whether a tail of a session file reaches back to a prompt, where the
/// turn the file ends on began.
fn holds_prompt(line: &[u8]) -> bool {
    Entry::read(line).is_some_and(|entry| entry.is_prompt())
}

/// What the extension passes on, by Pi's own name for the event.
#[derive(Deserialize)]
struct Payload {
    event: String,
    session: String,
    /// The Pi process's own pid, at session_start and agent_start.
    pid: Option<i32>,
    cwd: Option<String>,
    /// The session's file, which may not be written yet, at session_start
    /// and agent_start. None for a session Pi keeps no file of.
    file: Option<PathBuf>,
    /// Whether the user stopped the run, at agent_settled.
    #[serde(default)]
    aborted: bool,
    /// How the last assistant message ended, at agent_settled.
    stop_reason: Option<String>,
}

/// What the files and the extension have said of one session.
struct Session {
    /// The process that runs it.
    liveness: Liveness,
    /// When that process started, as a Unix second. What an earlier process
    /// wrote to the file is history.
    start: i64,
    /// None for a session a seed found no file of, until it is found.
    file: Option<PathBuf>,
    /// Opened at the file's end when the session started with its file
    /// there, as on a resume, a fork or a clone; at its start if the file
    /// came after.
    tail: Option<Tail>,
    /// When the file's header says it was made. A fork copies entries from
    /// the session it forks, each as old as it was there.
    made: Option<String>,
    cwd: String,
    /// Whether the last read of the file stopped at its budget.
    behind: bool,
    turn: Option<TurnKey>,
    /// Whether the file says the last run ended, by a turn end or a stop.
    ended: bool,
    /// A background subagent's report written since the last run ended,
    /// which is the turn of a run that comes after it.
    woke: Option<TurnKey>,
    /// Whether a dialog may hold the run up, which nothing is written for:
    /// the extension said one does, or the daemon started inside the run.
    dialog: bool,
    told: Told,
}

impl Session {
    fn new(liveness: Liveness, file: Option<PathBuf>, cwd: String) -> Session {
        let start = match liveness {
            Liveness::Process {
                start: StartCheck::ExactUs(us),
                ..
            } => (us / 1_000_000) as i64,
            _ => i64::MIN,
        };
        Session {
            liveness,
            start,
            file,
            tail: None,
            made: None,
            cwd,
            behind: false,
            turn: None,
            ended: true,
            woke: None,
            dialog: false,
            told: Told::default(),
        }
    }

    /// What one message entry says, as the signals of each fact in order:
    /// a run that no prompt began, then the entry's own.
    fn facts(&mut self, entry: &Entry) -> Vec<BTreeSet<Signal>> {
        let mut facts = Vec::new();
        if entry.is_prompt() {
            self.turn = entry.id.clone().map(TurnKey);
            self.ended = false;
            self.woke = None;
            facts.push(BTreeSet::from([Signal::Prompted]));
        }
        if entry.is_report() && self.ended {
            self.woke = entry.id.clone().map(TurnKey);
        }
        if let Some(message) = entry.assistant() {
            // A reply after the run ended: a report woke the agent, and Pi
            // ran it with no prompt.
            if self.ended {
                if let Some(turn) = self.woke.take() {
                    self.turn = Some(turn);
                }
                facts.push(BTreeSet::from([Signal::Prompted]));
            }
            let mut own = BTreeSet::new();
            if message.is_interrupt() {
                own.insert(Signal::Interrupted);
            }
            if let Some(text) = message.end() {
                own.insert(Signal::TurnEnded);
                if ends_on_question(&text) {
                    own.insert(Signal::TurnEndedAsking);
                }
            }
            self.ended = !own.is_empty();
            if self.ended {
                facts.push(own);
            }
        }
        facts
    }

    /// Whether this session's process did not write the entry: an earlier
    /// process did, or it was copied from the session this one forks.
    fn history(&self, entry: &Entry) -> bool {
        let Some(written) = entry.timestamp.as_deref() else {
            return false;
        };
        let earlier = liveness::iso_second(written).is_some_and(|second| second < self.start);
        // Pi writes every time in one format, so the texts sort as the times do.
        let copied = self.made.as_deref().is_some_and(|made| written < made);
        earlier || copied
    }
}

/// Pi's sessions, from the extension's events and the files they name.
pub struct Pi {
    sessions_dir: PathBuf,
    sessions: HashMap<String, Session>,
    /// What its reads could not make sense of, since it was last asked.
    strange: Strange,
}

impl Pi {
    pub fn new(home: &Path) -> Pi {
        Pi {
            sessions_dir: home.join(".pi/agent/sessions"),
            sessions: HashMap::new(),
            strange: Strange::default(),
        }
    }

    /// The file of a session in a cwd's folder, found by the id its name
    /// ends with. A session in a folder of its own, set by sessionDir or
    /// --session-dir, is found only when the extension names its file.
    fn find(&self, id: &str) -> Option<PathBuf> {
        let name = format!("_{id}.jsonl");
        fs::read_dir(&self.sessions_dir)
            .ok()?
            .flatten()
            .filter_map(|folder| fs::read_dir(folder.path()).ok())
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|file| file.ends_with(&name))
                    && path.is_file()
            })
    }

    /// Reads what was appended to a session's file, looking for the file
    /// first if a seed found none.
    fn read(&mut self, id: &str, out: &mut Vec<Observation>) {
        if self
            .sessions
            .get(id)
            .is_some_and(|session| session.file.is_none())
        {
            let file = self.find(id);
            self.sessions.get_mut(id).expect("tracked").file = file;
        }
        let Some(session) = self.sessions.get_mut(id) else {
            return;
        };
        let Some(file) = session.file.clone() else {
            return;
        };
        if session.tail.is_none() {
            // Written since the session started, so all of it is the session's.
            let Ok(tail) = Tail::at_start(&file) else {
                return;
            };
            session.tail = Some(tail);
        }
        // Pi rewrites a file only to migrate it from an older version, as it
        // loads it, which is history: a file replaced is followed from its end.
        let read = tail::follow(
            &mut session.tail,
            &file,
            &mut session.behind,
            &mut self.strange,
        );
        let Ok(lines) = read else {
            return;
        };
        for line in lines {
            if let Some(entry) = Entry::counted(&line, &mut self.strange) {
                self.entry(id, &entry, out);
            }
        }
    }

    /// What one entry says, each fact a signal of its own.
    fn entry(&mut self, id: &str, entry: &Entry, out: &mut Vec<Observation>) {
        let session = self.sessions.get_mut(id).expect("tracked");
        if entry.kind == "session" {
            session.made = entry.timestamp.clone();
            return;
        }
        if !entry.is_message() {
            return;
        }
        // What this session's process did not write tells nothing, but
        // says which turn the file is on.
        if session.history(entry) {
            session.facts(entry);
            return;
        }
        // Nothing is written while a dialog is open, so whatever comes next
        // comes after it.
        if session.dialog {
            session.dialog = false;
            tell(id, session, Channel::File, [Signal::Answered], out);
        }
        for facts in session.facts(entry) {
            tell(id, session, Channel::File, facts, out);
        }
    }

    /// Follows the session a session_start or agent_start names, from the
    /// Pi process the hook runs under, if that is the process the payload
    /// names: the extension runs the hook with no shell between.
    fn bind(&mut self, payload: &Payload, hook: &Hook, out: &mut Vec<Observation>) {
        let (Some(pid), Some(parent)) = (payload.pid, hook.ancestry.first()) else {
            return;
        };
        // A session Pi keeps no file of says nothing of its turns.
        let Some(file) = payload.file.clone() else {
            return;
        };
        if parent.pid != pid {
            return;
        }
        let liveness = Liveness::Process {
            pid,
            start: StartCheck::ExactUs(parent.started_us),
        };
        let cwd = payload.cwd.clone().unwrap_or_default();
        let id = &payload.session;
        // The same process going on with it.
        let known = self.sessions.get(id).is_some_and(|session| {
            session.liveness == liveness && session.file.as_ref() == Some(&file)
        });
        if !known {
            // A Pi process runs one session at a time, so any other it ran
            // has ended, though its session_shutdown never came.
            let mut left: Vec<String> = self
                .sessions
                .iter()
                .filter(|(other, session)| *other != id && session.liveness == liveness)
                .map(|(other, _)| other.clone())
                .collect();
            left.sort();
            for other in left {
                let mut session = self.sessions.remove(&other).expect("tracked");
                tell(&other, &mut session, Channel::Hook, [Signal::Ended], out);
            }
            let mut session = Session::new(liveness.clone(), Some(file.clone()), cwd.clone());
            // What is already written, as a resumed, forked or cloned file
            // holds, is history: it says only which turn the session is on,
            // as a seed reads it, though this process may have written it
            // before it went to another session and back.
            if let Ok((lines, tail)) = tail::tail_back(&file, &mut self.strange, holds_prompt) {
                let strange = &mut self.strange;
                let entries = lines
                    .iter()
                    .filter_map(|line| Entry::counted(line, strange));
                for entry in entries.filter(Entry::is_message) {
                    session.facts(&entry);
                }
                session.tail = Some(tail);
            }
            session.made = header(&file)
                .ok()
                .flatten()
                .and_then(|header| header.timestamp);
            self.sessions.insert(id.clone(), session);
        }
        let turn = self.sessions[id].turn.clone();
        out.push(Observation::new(
            Harness::Pi,
            id,
            Channel::Hook,
            Event::Started(Start { cwd, liveness }),
            turn,
        ));
    }

    /// Where the tail of a session's file leaves it, back to where its last
    /// turn began.
    fn seed_file(&mut self, id: &str, path: &Path) -> io::Result<()> {
        if let Some(header) = header(path)? {
            let session = self.sessions.get_mut(id).expect("seeded");
            session.made = header.timestamp;
            session.cwd = header.cwd.unwrap_or_default();
        }
        let (lines, tail) = tail::tail_back(path, &mut self.strange, holds_prompt)?;
        let strange = &mut self.strange;
        let entries: Vec<Entry> = lines
            .iter()
            .filter_map(|line| Entry::counted(line, strange))
            .collect();
        self.sessions.get_mut(id).expect("seeded").tail = Some(tail);
        let mut history = Vec::new();
        // A tail that holds no prompt starts in a run, which its first reply
        // tells as a run no prompt began.
        for entry in &entries {
            self.entry(id, entry, &mut history);
        }
        Ok(())
    }
}

/// Tells what one entry or one event says.
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
            .observe(Harness::Pi, id, channel, signals, session.turn.clone()),
    );
}

/// The channel a signal is told through when a session's state is rebuilt:
/// its file, if the file can tell it.
fn channel(signal: Signal) -> Channel {
    if harness::declares(Harness::Pi, signal, Channel::File) {
        Channel::File
    } else {
        Channel::Hook
    }
}

impl Source for Pi {
    fn harness(&self) -> Harness {
        Harness::Pi
    }

    /// The default folder, and the folder of any session file the extension
    /// named outside it.
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.sessions_dir.clone()];
        for session in self.sessions.values() {
            if let Some(folder) = session.file.as_deref().and_then(Path::parent)
                && !folder.starts_with(&self.sessions_dir)
                && !roots.iter().any(|root| root == folder)
            {
                roots.push(folder.to_owned());
            }
        }
        roots
    }

    /// Each session the extension started whose process still runs, from
    /// the tail of its file. No file says which sessions run. One whose file
    /// is not there yet, as 0.84.4 writes a new session's file only as its
    /// first reply ends, is read from the file's start once it is.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Seed> {
        let mut seeded = Vec::new();
        let mut unread = Vec::new();
        for (id, liveness) in bound {
            if !liveness::alive(k, liveness) {
                continue;
            }
            let file = self.find(id);
            let session = Session::new(liveness.clone(), file.clone(), String::new());
            self.sessions.insert(id.clone(), session);
            if let Some(file) = file
                && let Err(error) = self.seed_file(id, &file)
            {
                // The session is still followed, from what it writes next.
                unread.push(Unread::new(&file, error));
                let session = self.sessions.get_mut(id).expect("seeded");
                session.tail = Tail::at_end(&file).ok();
            }
            let session = self.sessions.get_mut(id).expect("seeded");
            // Nothing is written while a dialog is open, so one the extension
            // told before the daemon started may be open still: the first
            // entry after says it is not.
            session.dialog = !session.ended;
            let started = Observation::new(
                Harness::Pi,
                id,
                Channel::Hook,
                Event::Started(Start {
                    cwd: session.cwd.clone(),
                    liveness: liveness.clone(),
                }),
                None,
            );
            let session = &self.sessions[id];
            seeded.extend(
                session
                    .told
                    .seeded(vec![started], session.turn.clone(), channel),
            );
        }
        Ok(Seed {
            observations: seeded,
            unread,
        })
    }

    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        // A session whose process is gone is the sweep's to end; its file
        // says nothing more of it.
        self.sessions
            .retain(|_, session| liveness::alive(k, &session.liveness));
        // A session whose file is not found yet looks for it at each change.
        let mut ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                let file = session.file.as_ref();
                paths.is_none_or(|paths| file.is_none_or(|file| paths.contains(file)))
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        let mut out = Vec::new();
        for id in ids {
            self.read(&id, &mut out);
        }
        out
    }

    fn hook(&mut self, _k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let Ok(payload) = Payload::deserialize(&hook.payload) else {
            return Vec::new();
        };
        let id = payload.session.clone();
        let mut out = Vec::new();
        // What Pi wrote before it ran the hook comes first.
        loop {
            self.read(&id, &mut out);
            if !self.sessions.get(&id).is_some_and(|session| session.behind) {
                break;
            }
        }
        if matches!(payload.event.as_str(), "session_start" | "agent_start") {
            self.bind(&payload, hook, &mut out);
        }
        let session = self.sessions.get_mut(&id);
        let mut facts = BTreeSet::new();
        match payload.event.as_str() {
            "agent_start" => {
                facts.insert(Signal::Prompted);
            }
            "ui_prompt_start" => {
                facts.insert(Signal::QuestionAsked);
            }
            "ui_prompt_end" => {
                facts.insert(Signal::Answered);
            }
            // Pi will not go on by itself after this, so an error it ended on
            // is final. A stop that 0.84.4 wrote as an error is a stop too:
            // the table ranks it first.
            "agent_settled" => {
                if payload.stop_reason.as_deref() == Some("error") {
                    facts.insert(Signal::TurnFailed);
                }
                if payload.aborted {
                    facts.insert(Signal::Interrupted);
                }
            }
            "session_shutdown" => {
                facts.insert(Signal::Ended);
            }
            _ => {}
        }
        if facts.is_empty() {
            return out;
        }
        match session {
            Some(session) => {
                session.dialog = facts.contains(&Signal::QuestionAsked);
                tell(&id, session, Channel::Hook, facts, &mut out);
            }
            None => out.push(Observation::new(
                Harness::Pi,
                &id,
                Channel::Hook,
                Event::Signals(facts),
                None,
            )),
        }
        if payload.event == "session_shutdown" {
            self.sessions.remove(&id);
        }
        out
    }

    fn behind(&self) -> Vec<PathBuf> {
        let behind = self.sessions.values().filter(|session| session.behind);
        let mut paths: Vec<PathBuf> = behind.filter_map(|s| s.file.clone()).collect();
        paths.sort();
        paths
    }

    fn strange(&mut self) -> Strange {
        mem::take(&mut self.strange)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;
    use crate::fsm::Phase;
    use crate::liveness::Proc;
    use crate::queue::Key;
    use crate::source::fixture::{self, Replay};

    fn replay(name: &str) -> Replay {
        fixture::replay(Harness::Pi, name)
    }

    /// The phase the replay left each session in, by id.
    fn phases(replay: &Replay) -> Vec<(String, Phase)> {
        let phases = replay.phases().into_iter();
        phases.map(|(key, phase)| (key.id, phase)).collect()
    }

    /// The signals one session was told through its file, in order.
    fn from_file(replay: &Replay, id: &str) -> Vec<BTreeSet<Signal>> {
        replay
            .yielded
            .iter()
            .filter(|(o, _)| o.key.id == id && o.channel == Channel::File)
            .map(|(o, _)| o.event.signals())
            .collect()
    }

    fn told(signals: &[Signal]) -> BTreeSet<Signal> {
        signals.iter().copied().collect()
    }

    /// Seeds from the real home, reading it and changing nothing, and prints
    /// where each session file one level under a cwd's folder leaves its
    /// session, as if a running Pi had named it. No file says which run.
    #[test]
    #[ignore = "reads the real ~/.pi"]
    fn seeds_from_the_real_home() {
        let home = std::env::var_os("HOME").expect("a home");
        let mut pi = Pi::new(Path::new(&home));
        let mut files = Vec::new();
        for folder in fs::read_dir(&pi.sessions_dir)
            .into_iter()
            .flatten()
            .flatten()
        {
            for file in fs::read_dir(folder.path()).into_iter().flatten().flatten() {
                let path = file.path();
                if path.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                    files.push(path);
                }
            }
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        // This process, alive since before now, so that nothing in the files
        // is history.
        let running = Liveness::Process {
            pid: std::process::id() as i32,
            start: StartCheck::NotAfterMs(now.as_millis() as u64),
        };
        let bound: Vec<(String, Liveness)> = files
            .iter()
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?;
                Some((stem.rsplit_once('_')?.1.to_owned(), running.clone()))
            })
            .collect();
        let mut queue = crate::queue::Queue::default();
        for o in fixture::printed(pi.seed(&liveness::System, &bound).unwrap()) {
            assert!(o.quiet);
            queue.apply(&o, 0);
        }
        println!("{} session files", files.len());
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
        for name in fixture::names(Harness::Pi) {
            replay(&name);
        }
    }

    #[test]
    /// The phases are the replay's to check, which knows the moments a file
    /// cannot tell, such as a dialog open or a final error.
    fn a_daemon_started_after_any_fixture_follows_its_sessions_on_their_turns() {
        for name in fixture::names(Harness::Pi) {
            let replay = replay(&name);
            let where_ = |queue: &crate::queue::Queue| -> Vec<(Key, Option<TurnKey>)> {
                let mut sessions: Vec<_> = queue
                    .sessions()
                    .iter()
                    .map(|session| (session.key.clone(), session.turn.clone()))
                    .collect();
                sessions.sort_by(|a, b| a.0.cmp(&b.0));
                sessions
            };
            assert_eq!(
                where_(&fixture::restart(&replay)),
                where_(&replay.queue),
                "{name}"
            );
        }
    }

    #[test]
    fn a_fork_file_is_not_replayed() {
        let replay = replay("a_fork_file_is_not_replayed");
        // The fork and the clone each tell only the turn run in them, not
        // the turns they copied.
        for id in [
            "01a1df28-931e-7b0a-8d32-8f9ee911e9b5",
            "01a1df28-99d0-7c70-98e3-92446957da82",
        ] {
            assert_eq!(
                from_file(&replay, id),
                [told(&[Signal::Prompted]), told(&[Signal::TurnEnded])],
                "{id}"
            );
        }
        assert!(replay.phases().is_empty());
    }

    #[test]
    fn an_unparseable_recovered_line_is_skipped() {
        let replay = replay("an_unparseable_recovered_line_is_skipped");
        assert_eq!(
            phases(&replay),
            [(
                "01a1df27-cb1f-7a4a-a17c-e6183f04fd77".to_owned(),
                Phase::Stopped
            )]
        );
    }

    #[test]
    fn a_nested_child_is_never_read() {
        let replay = replay("a_nested_child_is_never_read");
        let parent = "01a1df28-c927-7d3a-a0e0-2f2e51d4ab67";
        assert!(replay.yielded.iter().all(|(o, _)| o.key.id == parent));
        // The background child's report wakes the parent.
        let prompts = from_file(&replay, parent)
            .into_iter()
            .filter(|signals| signals.contains(&Signal::Prompted))
            .count();
        assert_eq!(prompts, 3);
    }

    #[test]
    fn a_session_resumed_in_its_own_process_starts_where_it_was() {
        let replay = replay("a_session_resumed_in_its_own_process_starts_where_it_was");
        // On the turn it ran before it went to another session, which a
        // daemon started after the resume does not show again.
        let resumed = replay
            .yielded
            .iter()
            .rfind(|(o, _)| {
                o.key.id == "01a1df40-5fd9-7ebd-8458-13e9dd839625"
                    && matches!(o.event, Event::Started(_))
            })
            .unwrap();
        assert_eq!(resumed.0.turn, Some(TurnKey("d4a87a05".into())));
    }

    #[test]
    fn a_subagent_report_that_wakes_no_run_starts_no_turn() {
        let replay = replay("a_subagent_report_that_wakes_no_run_starts_no_turn");
        assert_eq!(phases(&replay)[0].1, Phase::Stopped);
    }

    #[test]
    fn a_provider_error_containing_aborted_is_not_an_interrupt() {
        let replay = replay("a_provider_error_containing_aborted_is_not_an_interrupt");
        let interrupted = replay
            .yielded
            .iter()
            .any(|(o, _)| o.event.signals().contains(&Signal::Interrupted));
        assert!(!interrupted);
        assert_eq!(phases(&replay)[0].1, Phase::Failed);
    }

    #[test]
    fn an_error_entry_is_not_final_until_agent_settled() {
        let replay = replay("an_error_entry_is_not_final_until_agent_settled");
        let failed: Vec<(Channel, usize)> = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::TurnFailed))
            .map(|(o, step)| (o.channel, *step))
            .collect();
        let last = replay.yielded.last().unwrap().1;
        assert_eq!(failed, [(Channel::Hook, last)]);
        assert_eq!(phases(&replay)[0].1, Phase::Failed);
    }

    #[test]
    fn a_stop_during_a_tool_that_0_84_4_writes_as_an_error_is_an_interrupt() {
        assert_eq!(phases(&replay("interrupted.hook"))[0].1, Phase::Idle);
    }

    #[test]
    fn a_stop_while_a_retry_waits_is_an_interrupt_on_1_5_2() {
        let replay = replay("a_stop_while_a_retry_waits_is_an_interrupt_on_1_5_2");
        assert_eq!(phases(&replay)[0].1, Phase::Idle);
    }

    #[test]
    fn a_new_session_in_the_same_process_ends_the_last() {
        let replay = replay("a_new_session_in_the_same_process_ends_the_last");
        assert_eq!(
            phases(&replay),
            [(
                "01a1dfd1-8dff-7a73-9986-e82c359e2eea".to_owned(),
                Phase::Stopped
            )]
        );
    }

    #[test]
    fn a_session_whose_process_exits_ends() {
        assert!(
            replay("a_session_whose_process_exits_ends")
                .phases()
                .is_empty()
        );
    }

    /// A hook as our extension runs it, straight under the Pi process.
    fn start(pid: i32, file: Option<PathBuf>) -> Hook {
        Hook {
            harness: Harness::Pi,
            payload: json!({
                "event": "session_start",
                "session": "01a1df28-aef0-7086-ae19-601efb920365",
                "pid": pid,
                "cwd": "/Users/me/Code/app",
                "file": file,
            }),
            ancestry: vec![Proc {
                pid: 79128,
                started_us: 1_794_744_583_000_000,
            }],
        }
    }

    #[test]
    fn a_custom_session_dir_is_followed() {
        let replay = replay("a_custom_session_dir_is_followed");
        assert!(replay.phases().is_empty());
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("pi-sessions");
        let mut pi = Pi::new(home.path());
        let file =
            folder.join("2026-11-15T12-22-27-824Z_01a1df28-aef0-7086-ae19-601efb920365.jsonl");
        pi.hook(&liveness::System, &start(79128, Some(file)));
        assert_eq!(pi.roots(), [pi.sessions_dir.clone(), folder]);
    }

    #[test]
    fn a_session_seeded_before_its_file_is_written_is_read_once_it_is() {
        let home = tempfile::tempdir().unwrap();
        let mut pi = Pi::new(home.path());
        let id = "01a1df28-8760-7cc5-95aa-023c234bc57e";
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let running = Liveness::Process {
            pid: std::process::id() as i32,
            start: StartCheck::NotAfterMs(now.as_millis() as u64),
        };
        // agent_start bound it, and the daemon restarted before Pi 0.84.4
        // wrote the file, as its first reply ended.
        let seeded = pi.seed(&liveness::System, &[(id.into(), running)]).unwrap();
        assert!(seeded.unread.is_empty());
        assert_eq!(seeded.observations.len(), 1);
        let file = pi
            .sessions_dir
            .join("--Users-me-Code-app--")
            .join(format!("2026-11-15T12-22-17-696Z_{id}.jsonl"));
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let entries = [
            json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-11-15T12:22:17.696Z", "cwd": "/Users/me/Code/app"}),
            json!({"type": "message", "id": "f83585eb", "timestamp": "2026-11-15T12:22:19.050Z", "message": {"role": "user", "content": [{"type": "text", "text": "Reply with just the word ready."}]}}),
            json!({"type": "message", "id": "fbdad335", "timestamp": "2026-11-15T12:22:19.069Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "ready"}], "stopReason": "stop"}}),
        ];
        let text: String = entries.iter().map(|entry| format!("{entry}\n")).collect();
        fs::write(&file, text).unwrap();
        let read = pi.changed(&liveness::System, Some(&[file]));
        let signals: Vec<_> = read.iter().map(|o| o.event.signals()).collect();
        assert_eq!(
            signals,
            [told(&[Signal::Prompted]), told(&[Signal::TurnEnded])]
        );
    }

    #[test]
    fn a_hook_from_another_process_than_it_names_starts_nothing() {
        let home = tempfile::tempdir().unwrap();
        let mut pi = Pi::new(home.path());
        let file = home.path().join(".pi/agent/sessions/--a--/x.jsonl");
        let told = pi.hook(&liveness::System, &start(1, Some(file)));
        assert!(told.is_empty());
        assert!(pi.sessions.is_empty());
    }

    #[test]
    fn a_session_with_no_file_is_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let mut pi = Pi::new(home.path());
        let told = pi.hook(&liveness::System, &start(79128, None));
        assert!(told.is_empty());
    }

    #[test]
    fn a_file_that_cannot_be_read_leaves_nothing_behind() {
        fixture::a_read_that_fails_leaves_nothing_behind(
            &replay("turn_ended.file"),
            ".pi/agent/sessions/--Users-me-Code-app--/2026-11-15T12-22-17-696Z_01a1df28-8760-7cc5-95aa-023c234bc57e.jsonl",
        );
    }
}
