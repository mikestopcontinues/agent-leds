//! Cursor's CLI, `cursor-agent`, from its own files and one hook. Each chat's
//! transcript, `~/.cursor/projects/<slug>/agent-transcripts/<chat>/<chat>.jsonl`,
//! is written at the checkpoints of a turn: appended to, or truncated and
//! rewritten whole in place, as at the first checkpoint of every turn. So it
//! is read whole, and a read counts only once it is finished and new. Each
//! running process, the CLI and its node worker alike, marks itself in
//! `~/.local/share/cursor-agent/versions/<version>/.running/<pid>`, a file
//! that names no chat. The process holds the store of the chat it is on,
//! `~/.cursor/chats/<bucket>/<chat>/store.db`, open, and lets go of it at
//! `/new` or on resuming another chat, which deletes the store's
//! write-ahead log. The `beforeSubmitPrompt` hook binds the chat to the
//! CLI's process it runs under, and says that the turn started, which a
//! text-only turn writes nothing of until it ends. The `stop` hook says
//! that the turn was interrupted or failed, which can leave the transcript
//! as it was: an interrupt before the turn's first checkpoint after a turn
//! that was interrupted too, and a failure after one the same.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Cursor 2026.11.13.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use super::{Event, Hook, Observation, Seed, Source, Start, Strange, Told, Unread, tail};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, Proc, StartCheck};
use crate::queue::TurnKey;

/// What Cursor's files and its one hook tell.
pub const SIGNALS: &[Declared] = &[
    Declared::new(
        Signal::Started,
        Channel::Hook,
        Evidence::Observed,
        "beforeSubmitPrompt, run under a process Cursor marked running in .running with the second it started, the outermost of the marked processes in a row, so the CLI and not its node worker",
        "probe-cursor runs A–I (markers, lsof); the bundle's install-in-use-marker; 0.5.0 hooks",
    ),
    Declared::new(
        Signal::Prompted,
        Channel::Hook,
        Evidence::Observed,
        "beforeSubmitPrompt, for a chat bound to its process; the only word of a text-only turn until it ends",
        "probe-cursor run F; 0.5.0 hooks",
    ),
    Declared::new(
        Signal::Prompted,
        Channel::File,
        Evidence::Observed,
        "a finished transcript whose last record is a user or assistant record, after the turn's end or with more user records than before, as after a background job",
        "probe-cursor runs A, B, D, G, H",
    ),
    Declared::new(
        Signal::TurnEnded,
        Channel::File,
        Evidence::Observed,
        "a finished transcript whose last record is turn_ended, once for each count of user records and status",
        "probe-cursor runs A–H",
    ),
    Declared::new(
        Signal::TurnEndedAsking,
        Channel::File,
        Evidence::Observed,
        "turn_ended after an assistant text whose last line ends in a question mark; a reasoning model's reasoning follows its reply in that text, so its last line is tested instead",
        "probe-cursor runs A, B",
    ),
    Declared::new(
        Signal::TurnFailed,
        Channel::File,
        Evidence::Observed,
        "turn_ended with status error, the transcript rewritten whole as it was before the turn with the error after",
        "gaps cursor fail run (interactive) and -p runs on a model the plan does not include",
    ),
    Declared::new(
        Signal::TurnFailed,
        Channel::Hook,
        Evidence::Observed,
        "stop with status error, for a bound chat whose transcript ends in turn_ended error: a failure after one the same rewrites the same bytes; after an interrupt the transcript ends aborted, so it says nothing",
        "gaps cursor fail run, turns 1 and 2; gaps int-run turn 1; bundle 2986.index.js, the catch path",
    ),
    Declared::new(
        Signal::Interrupted,
        Channel::Hook,
        Evidence::Observed,
        "stop with status aborted, for a chat bound to its process; that it runs on an interrupt that writes nothing is from the bundle's code",
        "gaps int-run turn 1 (err-hooks.log); probe-cursor run B, turn 3 (no write); bundle 2986.index.js, the abort path",
    ),
    Declared::new(
        Signal::Interrupted,
        Channel::File,
        Evidence::Observed,
        "turn_ended with status aborted, including the previous turn's end rewritten to aborted",
        "probe-cursor runs B, H",
    ),
    Declared::new(
        Signal::Ended,
        Channel::File,
        Evidence::Observed,
        "the .running marker of the process the chat is bound to deleted, as a quit does; or the chat's store.db-wal, once seen or once the daemon starts to find store.db made, gone while the process runs on, as /new and resuming another chat leave it",
        "probe-cursor runs A, C, D, G, I; gaps cursor pty and /new runs",
    ),
    Declared::new(
        Signal::Ended,
        Channel::Hook,
        Evidence::Observed,
        "the process the hook bound the chat to gone, as after kill -9, which leaves its marker",
        "probe-cursor runs B, H",
    ),
];

/// A chat's store.
const DB: &str = "store.db";

/// The write-ahead log of a chat's store, which SQLite keeps while a
/// connection holds the store open.
const WAL: &str = "store.db-wal";

/// A hook payload, with the fields the daemon reads.
#[derive(Deserialize)]
struct Payload {
    hook_event_name: String,
    conversation_id: String,
    /// How the turn a stop hook ends ended: completed, aborted or error.
    status: Option<String>,
    #[serde(default)]
    workspace_roots: Vec<String>,
}

/// One transcript record, with only the fields that carry state.
#[derive(Deserialize)]
struct Record {
    role: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    /// How a turn ended: success, aborted or error.
    status: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
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
    /// The record's last text, or nothing. Cursor writes a reply's
    /// reasoning after it, in the same block and with a blank line between,
    /// so a reply with reasoning cannot be told from its reasoning.
    fn text(&self) -> String {
        match self.message.as_ref().map(|message| &message.content) {
            Some(Content::Text(text)) => text.clone(),
            Some(Content::Blocks(blocks)) => blocks
                .iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text.clone())
                .next_back()
                .unwrap_or_default(),
            None => String::new(),
        }
    }
}

/// How a transcript ends.
#[derive(Clone, Debug, PartialEq)]
enum Last {
    /// On a user or assistant record: a turn runs.
    Running,
    /// On a turn_ended record, with its status.
    Ended(String),
}

/// What a finished transcript says, as a whole.
#[derive(Clone, Debug, PartialEq)]
struct Level {
    users: usize,
    last: Option<Last>,
    /// The last assistant record's text, in the turn the last user record began.
    text: String,
}

/// The kinds of transcript record Cursor was seen to write: by role, or by
/// type for a record with none.
const KINDS: &[&str] = &["assistant", "metadata", "turn_ended", "user"];

impl Level {
    /// What a whole transcript says, counting in `strange` each record of a
    /// kind Cursor was not seen to write, and each line that is not a record
    /// and is not among those `counted` already: every rewrite of the
    /// transcript writes its history again.
    fn read(bytes: &[u8], strange: &mut Strange, counted: &mut HashSet<u64>) -> Level {
        let mut level = Level {
            users: 0,
            last: None,
            text: String::new(),
        };
        let lines = bytes
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty());
        for line in lines {
            let Ok(record) = serde_json::from_slice::<Record>(line) else {
                let mut hasher = DefaultHasher::new();
                line.hash(&mut hasher);
                if counted.insert(hasher.finish()) {
                    strange.unparsed += 1;
                }
                continue;
            };
            strange.kind(
                record
                    .role
                    .as_deref()
                    .or(record.kind.as_deref())
                    .unwrap_or_default(),
                KINDS,
            );
            match (record.role.as_deref(), record.kind.as_deref()) {
                (Some("user"), _) => {
                    level.users += 1;
                    level.last = Some(Last::Running);
                    level.text.clear();
                }
                (Some("assistant"), _) => {
                    level.last = Some(Last::Running);
                    level.text = record.text();
                }
                (_, Some("turn_ended")) => {
                    level.last = Some(Last::Ended(record.status.unwrap_or_default()));
                }
                _ => {}
            }
        }
        level
    }

    /// What is true of the transcript now that was not of `before`, each a
    /// fact of its own.
    fn since(&self, before: Option<&Level>) -> BTreeSet<Signal> {
        let mut told = BTreeSet::new();
        match &self.last {
            None => {}
            Some(Last::Running) => {
                let new = before.is_none_or(|before| {
                    before.last != Some(Last::Running) || self.users > before.users
                });
                if new {
                    told.insert(Signal::Prompted);
                }
            }
            Some(Last::Ended(status)) => {
                let new = before
                    .is_none_or(|before| (before.users, &before.last) != (self.users, &self.last));
                if new {
                    told.insert(Signal::TurnEnded);
                    if ends_on_question(&self.text) {
                        told.insert(Signal::TurnEndedAsking);
                    }
                    match status.as_str() {
                        "aborted" => {
                            told.insert(Signal::Interrupted);
                        }
                        "error" => {
                            told.insert(Signal::TurnFailed);
                        }
                        _ => {}
                    }
                }
            }
        }
        told
    }
}

/// Where each line of a read of a transcript ends, with a hash of the read
/// up to there: enough to tell a later read that is the start of this one,
/// or the whole of it, without keeping its bytes.
#[derive(Debug, PartialEq)]
struct Taken(Vec<(usize, u64)>);

impl Taken {
    fn of(bytes: &[u8]) -> Taken {
        let mut hasher = DefaultHasher::new();
        let mut end = 0;
        let mut ends = Vec::new();
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            hasher.write(line);
            end += line.len();
            ends.push((end, hasher.clone().finish()));
        }
        Taken(ends)
    }

    /// Whether `read` is the start of this read, or the whole of it.
    fn starts_with(&self, read: &Taken) -> bool {
        read.0.last().is_some_and(|last| {
            let at = self.0.binary_search_by_key(&last.0, |&(end, _)| end);
            at.is_ok_and(|at| self.0[at] == *last)
        })
    }
}

/// A whole read of a transcript, if it is one to take: finished, and not a
/// part of the read taken last. A rewrite truncates the file in place and
/// writes it again from the start, so a read caught in the middle is the
/// start of the new file and, since every rewrite keeps the history, of the
/// last one taken too.
fn take(bytes: &[u8], taken: Option<&Taken>) -> Option<Taken> {
    if bytes.is_empty() || !bytes.ends_with(b"\n") {
        return None;
    }
    let read = Taken::of(bytes);
    (!taken.is_some_and(|taken| taken.starts_with(&read))).then_some(read)
}

/// The second the process started, as the marker at `path` says:
/// `<name>|<ps lstart>`, where the name is what `ps -c` calls the process.
/// That is the name the CLI was run by, `cursor-agent` or `agent`, and
/// `node` for its worker, so it says nothing of which is which. The first
/// write, `<name>|`, has no start yet.
fn marked(path: &Path) -> Option<i64> {
    let text = fs::read_to_string(path).ok()?;
    let (_, start) = text.trim().split_once('|')?;
    liveness::lstart(start)
}

/// What the files and the hook have said of one chat.
struct Session {
    pid: i32,
    /// When the process started, as a Unix second.
    start: i64,
    /// The process's `.running` marker.
    marker: PathBuf,
    /// The last read of the transcript taken, whole.
    taken: Option<Taken>,
    level: Option<Level>,
    /// The lines of the transcript that are not records, counted already.
    unparsed: HashSet<u64>,
    told: Told,
    /// Whether the process has held the chat's store open: seen so, or made
    /// before the daemon started.
    held: bool,
}

impl Session {
    fn new(pid: i32, start: i64, marker: PathBuf) -> Session {
        Session {
            pid,
            start,
            marker,
            taken: None,
            level: None,
            unparsed: HashSet::new(),
            told: Told::default(),
            held: false,
        }
    }

    /// A turn is named by the count of user records up to it.
    fn turn(&self) -> Option<TurnKey> {
        let level = self.level.as_ref()?;
        Some(TurnKey(level.users.to_string()))
    }

    fn liveness(&self) -> Liveness {
        Liveness::Process {
            pid: self.pid,
            start: StartCheck::Second(self.start),
        }
    }
}

/// Cursor's chats, from their transcripts, the processes' markers and the
/// hook that binds the one to the other.
pub struct Cursor {
    projects: PathBuf,
    versions: PathBuf,
    chats: PathBuf,
    sessions: BTreeMap<String, Session>,
    /// What its reads could not make sense of, since it was last asked.
    strange: Strange,
}

impl Cursor {
    pub fn new(home: &Path) -> Cursor {
        Cursor {
            projects: home.join(".cursor/projects"),
            versions: home.join(".local/share/cursor-agent/versions"),
            chats: home.join(".cursor/chats"),
            sessions: BTreeMap::new(),
            strange: Strange::default(),
        }
    }

    /// The chat a transcript belongs to. A subagent's transcript is in a
    /// folder of the parent's, and is never read.
    fn chat_of(&self, path: &Path) -> Option<String> {
        let id = path.file_stem()?.to_str()?;
        let folder = path.parent()?;
        let transcripts = folder.parent()?;
        let ours = path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && folder.file_name()?.to_str()? == id
            && transcripts.file_name()? == "agent-transcripts"
            && transcripts.parent()?.parent()? == self.projects;
        ours.then(|| id.to_owned())
    }

    fn is_marker(&self, path: &Path) -> bool {
        let Some(running) = path.parent() else {
            return false;
        };
        running.file_name().is_some_and(|name| name == ".running")
            && running
                .parent()
                .and_then(Path::parent)
                .is_some_and(|versions| versions == self.versions)
    }

    /// The chat's transcript, found by its name in any project folder: the
    /// folder's name is the cwd cut short, and is never computed.
    fn find(&self, id: &str) -> Option<PathBuf> {
        let name = Path::new("agent-transcripts")
            .join(id)
            .join(format!("{id}.jsonl"));
        fs::read_dir(&self.projects)
            .ok()?
            .flatten()
            .map(|entry| entry.path().join(&name))
            .find(|path| path.is_file())
    }

    /// The marker Cursor wrote for the process with this pid and start, in
    /// any installed version.
    fn marker(&self, pid: i32, start: i64) -> Option<PathBuf> {
        fs::read_dir(&self.versions)
            .ok()?
            .flatten()
            .map(|version| version.path().join(".running").join(pid.to_string()))
            .find(|marker| marked(marker) == Some(start))
    }

    /// The CLI's process the hook runs under, with when it started and its
    /// marker: of the hook's ancestors that Cursor marked, the nearest whose
    /// parent it did not. The CLI's node worker is marked too, and is the
    /// CLI's child; the CLI holds the chat, and unlinks its marker when it
    /// quits, which the worker never does.
    fn process(&self, ancestry: &[Proc]) -> Option<(i32, i64, PathBuf)> {
        let mut found = None;
        for proc in ancestry {
            let start = (proc.started_us / 1_000_000) as i64;
            match self.marker(proc.pid, start) {
                Some(marker) => found = Some((proc.pid, start, marker)),
                None if found.is_some() => break,
                None => {}
            }
        }
        found
    }

    /// The folder Cursor keeps the chat's store and record in,
    /// `~/.cursor/chats/<bucket>/<chat>`, found by its name in any bucket:
    /// the bucket is a hash of the cwd, and is never computed.
    fn store(&self, id: &str) -> Option<PathBuf> {
        fs::read_dir(&self.chats)
            .ok()?
            .flatten()
            .map(|bucket| bucket.path().join(id))
            .find(|store| store.is_dir())
    }

    /// The chat whose store's write-ahead log is at `path`.
    fn wal_of(&self, path: &Path) -> Option<String> {
        let store = path.parent()?;
        if path.file_name()? != WAL || store.parent()?.parent()? != self.chats {
            return None;
        }
        Some(store.file_name()?.to_str()?.to_owned())
    }

    /// The folder the chat works in, as Cursor's record of it says.
    fn folder(&self, id: &str) -> String {
        #[derive(Deserialize)]
        struct Meta {
            cwd: String,
        }
        let meta = self.store(id).and_then(|store| {
            let text = fs::read(store.join("meta.json")).ok()?;
            serde_json::from_slice::<Meta>(&text).ok()
        });
        meta.map(|meta| meta.cwd).unwrap_or_default()
    }

    /// Ends the chat if its process let go of the chat's store, as `/new`
    /// and resuming another chat do while the process runs on. The process
    /// holds the store of the chat it is on open from the chat's first
    /// prompt, and SQLite deletes the store's write-ahead log when its last
    /// connection closes. A log not seen yet says nothing: the prompt's hook
    /// comes before the store is made. A store made before the daemon
    /// started was held, so its log gone says so too.
    fn let_go(&mut self, id: &str, out: &mut Vec<Observation>) {
        if !self.sessions.contains_key(id) {
            return;
        }
        let open = self
            .store(id)
            .is_some_and(|store| store.join(WAL).is_file());
        let session = self.sessions.get_mut(id).expect("bound");
        if open {
            session.held = true;
        } else if session.held {
            self.sessions.remove(id);
            out.push(Observation::new(
                Harness::Cursor,
                id,
                Channel::File,
                Event::one(Signal::Ended),
                None,
            ));
        }
    }

    /// Reads the chat's transcript whole and tells what it says that the
    /// last read taken did not. One changing while it was read, gone, or
    /// that cannot be read is read again at its next change.
    fn read(&mut self, id: &str, out: &mut Vec<Observation>) -> Result<(), Unread> {
        let Some(path) = self.find(id) else {
            return Ok(());
        };
        let bytes = match tail::stable(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(Unread::new(&path, error)),
        };
        let session = self.sessions.get_mut(id).expect("bound");
        let Some(taken) = take(&bytes, session.taken.as_ref()) else {
            return Ok(());
        };
        let level = Level::read(&bytes, &mut self.strange, &mut session.unparsed);
        let signals = level.since(session.level.as_ref());
        session.taken = Some(taken);
        session.level = Some(level);
        if !signals.is_empty() {
            tell(id, session, Channel::File, signals, out);
        }
        Ok(())
    }

    /// Ends every chat bound to the process whose marker is at `path`, if
    /// the marker no longer says that process runs.
    fn gone(&mut self, path: &Path, out: &mut Vec<Observation>) {
        let now = marked(path);
        let ended: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.marker == path && now != Some(session.start))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ended {
            self.sessions.remove(&id);
            out.push(Observation::new(
                Harness::Cursor,
                &id,
                Channel::File,
                Event::one(Signal::Ended),
                None,
            ));
        }
    }

    /// A prompt: binds the chat to the process the hook runs under, and
    /// says the turn started. A chat the hook sees first holds only
    /// history, as a resumed one does.
    fn prompt(&mut self, payload: Payload, ancestry: &[Proc]) -> Vec<Observation> {
        let id = payload.conversation_id;
        let mut out = Vec::new();
        let known = self.sessions.contains_key(&id);
        if known {
            let _ = self.read(&id, &mut out);
        }
        match self.process(ancestry) {
            Some((pid, start, marker)) => {
                let session = self
                    .sessions
                    .entry(id.clone())
                    .or_insert_with(|| Session::new(pid, start, marker.clone()));
                // Resumed in another process.
                (session.pid, session.start, session.marker) = (pid, start, marker);
                if !known {
                    let _ = self.read(&id, &mut Vec::new());
                }
                let session = &self.sessions[&id];
                let start = Start {
                    cwd: payload
                        .workspace_roots
                        .into_iter()
                        .next()
                        .unwrap_or_default(),
                    liveness: session.liveness(),
                };
                out.push(Observation::new(
                    Harness::Cursor,
                    &id,
                    Channel::Hook,
                    Event::Started(start),
                    session.turn(),
                ));
            }
            // No process of the CLI to follow the chat by.
            None if !known => return out,
            None => {}
        }
        let session = self.sessions.get_mut(&id).expect("bound");
        tell(
            &id,
            session,
            Channel::Hook,
            BTreeSet::from([Signal::Prompted]),
            &mut out,
        );
        out
    }

    /// The end of a bound chat's turn, with how it ended, after catching up
    /// on its transcript. An interrupt before the turn's first checkpoint
    /// rewrites the transcript to end the previous turn as aborted, which
    /// says nothing new when it already ended so. A failure rewrites it to
    /// end in turn_ended error, the same bytes when the turn before failed
    /// the same way, so the hook says it when the transcript ends so. Cursor
    /// runs the hook with status error after every interrupt too, once the
    /// transcript ends aborted, which says nothing.
    fn stop(&mut self, id: &str, status: &str) -> Vec<Observation> {
        let mut out = Vec::new();
        if !self.sessions.contains_key(id) {
            return out;
        }
        let _ = self.read(id, &mut out);
        let session = self.sessions.get_mut(id).expect("bound");
        let failed = session
            .level
            .as_ref()
            .is_some_and(|level| level.last == Some(Last::Ended("error".to_owned())));
        let signal = match status {
            "aborted" => Signal::Interrupted,
            "error" if failed => Signal::TurnFailed,
            _ => return out,
        };
        tell(
            id,
            session,
            Channel::Hook,
            BTreeSet::from([signal]),
            &mut out,
        );
        out
    }
}

/// Tells what one read or one hook says.
fn tell(
    id: &str,
    session: &mut Session,
    channel: Channel,
    signals: BTreeSet<Signal>,
    out: &mut Vec<Observation>,
) {
    let turn = match channel {
        // The hook comes before the turn's records, so it cannot name it.
        Channel::Hook => None,
        _ => session.turn(),
    };
    out.push(
        session
            .told
            .observe(Harness::Cursor, id, channel, signals, turn),
    );
}

impl Source for Cursor {
    fn harness(&self) -> Harness {
        Harness::Cursor
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![
            self.projects.clone(),
            self.versions.clone(),
            self.chats.clone(),
        ]
    }

    /// Each chat a hook bound before the daemon started, whose process still
    /// runs and still holds the chat's store, if the store was made, as its
    /// transcript leaves it. A chat no hook bound names no process, so it
    /// shows from its next prompt.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Seed> {
        let mut seeded = Vec::new();
        let mut unread = Vec::new();
        for (id, liveness) in bound {
            let &Liveness::Process {
                pid,
                start: StartCheck::Second(start),
            } = liveness
            else {
                continue;
            };
            if !liveness::alive(k, liveness) {
                continue;
            }
            let Some(marker) = self.marker(pid, start) else {
                continue;
            };
            let mut session = Session::new(pid, start, marker);
            // The process made the store at the chat's first prompt, or
            // opened it on resuming the chat, before the prompt.
            session.held = self.store(id).is_some_and(|store| store.join(DB).is_file());
            self.sessions.insert(id.clone(), session);
            let mut ended = Vec::new();
            self.let_go(id, &mut ended);
            if !ended.is_empty() {
                continue;
            }
            // The chat is still followed, from its next read.
            if let Err(error) = self.read(id, &mut Vec::new()) {
                unread.push(error);
            }
            let turn = self.sessions[id].turn();
            let start = Start {
                cwd: self.folder(id),
                liveness: liveness.clone(),
            };
            seeded.push(Observation::new(
                Harness::Cursor,
                id,
                Channel::Hook,
                Event::Started(start),
                turn.clone(),
            ));
            seeded.extend(self.sessions[id].told.current().into_iter().map(|signal| {
                Observation::new(
                    Harness::Cursor,
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
        Ok(Seed {
            observations: seeded,
            unread,
        })
    }

    fn changed(&mut self, _k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        let (mut chats, mut stores, mut markers): (Vec<String>, Vec<String>, Vec<PathBuf>) =
            match paths {
                Some(paths) => (
                    paths.iter().filter_map(|path| self.chat_of(path)).collect(),
                    paths.iter().filter_map(|path| self.wal_of(path)).collect(),
                    paths
                        .iter()
                        .filter(|path| self.is_marker(path))
                        .cloned()
                        .collect(),
                ),
                None => (
                    self.sessions.keys().cloned().collect(),
                    self.sessions.keys().cloned().collect(),
                    self.sessions.values().map(|s| s.marker.clone()).collect(),
                ),
            };
        chats.sort();
        chats.dedup();
        stores.sort();
        stores.dedup();
        markers.sort();
        markers.dedup();
        let mut out = Vec::new();
        // The last checkpoint is written before the process lets go of the
        // chat's store or unlinks its marker.
        for id in chats {
            if self.sessions.contains_key(&id) {
                let _ = self.read(&id, &mut out);
            }
        }
        for id in stores {
            self.let_go(&id, &mut out);
        }
        for marker in markers {
            self.gone(&marker, &mut out);
        }
        out
    }

    /// A prompt or a turn's end, each after catching up on the chat's
    /// transcript.
    fn hook(&mut self, _k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let Ok(payload) = Payload::deserialize(&hook.payload) else {
            return Vec::new();
        };
        match (payload.hook_event_name.as_str(), payload.status.as_deref()) {
            ("beforeSubmitPrompt", _) => self.prompt(payload, &hook.ancestry),
            // A turn that completes says so in its transcript.
            ("stop", Some(status)) => self.stop(&payload.conversation_id, status),
            _ => Vec::new(),
        }
    }

    /// Never: a transcript is read whole.
    fn behind(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn strange(&mut self) -> Strange {
        mem::take(&mut self.strange)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::Phase;
    use crate::source::fixture::{self, Replay};

    fn replay(name: &str) -> Replay {
        fixture::replay(Harness::Cursor, name)
    }

    /// The phase of each session the replay left, by chat.
    fn phases(replay: &Replay) -> Vec<(String, Phase)> {
        let phases = replay.phases().into_iter();
        phases.map(|(key, phase)| (key.id, phase)).collect()
    }

    /// Reads the real home, changing nothing, and prints what Cursor's files
    /// show: each process marker, each chat's transcript, and the sessions a
    /// daemon starting now would seed from the queue it saved.
    #[test]
    #[ignore = "reads the real ~/.cursor and ~/.local/share/cursor-agent"]
    fn seeds_from_the_real_home() {
        let home = std::env::var_os("HOME").expect("a home");
        let mut cursor = Cursor::new(Path::new(&home));
        for version in fs::read_dir(&cursor.versions)
            .into_iter()
            .flatten()
            .flatten()
        {
            for marker in fs::read_dir(version.path().join(".running"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let text = fs::read_to_string(marker.path()).unwrap_or_default();
                let pid: i32 = marker.file_name().to_string_lossy().parse().unwrap_or(0);
                let running = marked(&marker.path()).is_some_and(|start| {
                    liveness::alive(
                        &liveness::System,
                        &Liveness::Process {
                            pid,
                            start: StartCheck::Second(start),
                        },
                    )
                });
                println!(
                    "marker {pid:>6}  {:<40}  {}",
                    text.trim(),
                    if running { "running" } else { "left behind" }
                );
            }
        }
        let mut transcripts = 0;
        for project in fs::read_dir(&cursor.projects)
            .into_iter()
            .flatten()
            .flatten()
        {
            for chat in fs::read_dir(project.path().join("agent-transcripts"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let id = chat.file_name().to_string_lossy().into_owned();
                let Some(path) = cursor.find(&id) else {
                    continue;
                };
                transcripts += 1;
                match tail::stable(&path) {
                    Ok(Some(bytes)) => {
                        let mut counted = HashSet::new();
                        let level = Level::read(&bytes, &mut Strange::default(), &mut counted);
                        println!(
                            "chat {id}  {:>3} user records  {:?}  asks {}",
                            level.users,
                            level.last,
                            ends_on_question(&level.text)
                        );
                    }
                    other => println!("chat {id}  unreadable: {other:?}"),
                }
            }
        }
        println!("{transcripts} transcripts");
        let bound = crate::daemon::saved_queue().map_or_else(
            |error| {
                println!("ignoring the saved queue: {error:#}");
                Vec::new()
            },
            |saved| saved.bound(),
        );
        let ours = bound
            .iter()
            .filter(|(key, _)| key.harness == Harness::Cursor);
        println!("{} chats the saved queue binds", ours.count());
        let mut queue = crate::queue::Queue::default();
        for o in
            fixture::printed(super::super::seed(&mut cursor, &liveness::System, &bound).unwrap())
        {
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
        for name in fixture::names(Harness::Cursor) {
            replay(&name);
        }
    }

    #[test]
    fn a_torn_rewrite_prefix_is_skipped() {
        let replay = replay("a_torn_rewrite_prefix_is_skipped");
        assert_eq!(
            phases(&replay),
            [(
                "052e7079-8ae4-44f9-a205-4b80fb790165".to_owned(),
                Phase::Asked
            )]
        );
    }

    #[test]
    fn a_rewrite_with_the_same_last_line_says_nothing() {
        let replay = replay("a_rewrite_with_the_same_last_line_says_nothing");
        let told: Vec<BTreeSet<Signal>> = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.channel == Channel::File)
            .map(|(o, _)| o.event.signals())
            .collect();
        assert_eq!(
            told,
            [Signal::Prompted, Signal::TurnEnded].map(|signal| BTreeSet::from([signal]))
        );
    }

    #[test]
    fn an_interrupt_rewriting_the_previous_success_is_an_interrupt() {
        let replay = replay("an_interrupt_rewriting_the_previous_success_is_an_interrupt");
        assert_eq!(
            phases(&replay),
            [(
                "b6f5f883-1823-4bc2-a601-1cc761c083a1".to_owned(),
                Phase::Idle
            )]
        );
    }

    #[test]
    fn an_interrupt_that_writes_nothing_is_told_by_the_stop_hook() {
        let replay = replay("an_interrupt_that_writes_nothing_is_told_by_the_stop_hook");
        assert_eq!(
            phases(&replay),
            [(
                "b6f5f883-1823-4bc2-a601-1cc761c083a1".to_owned(),
                Phase::Idle
            )]
        );
    }

    #[test]
    fn the_stop_hook_s_error_after_an_interrupt_says_nothing() {
        let replay = replay("interrupted.hook");
        assert_eq!(
            phases(&replay),
            [(
                "ec732734-8c9d-4cc5-bc25-474e3015b279".to_owned(),
                Phase::Idle
            )]
        );
    }

    #[test]
    fn a_second_failure_like_the_first_is_told_by_the_stop_hook() {
        let replay = replay("turn_failed.hook");
        assert_eq!(
            phases(&replay),
            [(
                "0903e018-043a-45e2-a954-1dd888361027".to_owned(),
                Phase::Failed
            )]
        );
    }

    #[test]
    fn a_reply_with_reasoning_is_judged_by_the_reasoning() {
        let replay = replay("a_reply_with_reasoning_is_judged_by_the_reasoning");
        assert_eq!(
            phases(&replay),
            [(
                "052e7079-8ae4-44f9-a205-4b80fb790165".to_owned(),
                Phase::Stopped
            )]
        );
    }

    #[test]
    fn the_marker_unlinked_ends_every_chat_of_its_pid() {
        let replay = replay("the_marker_unlinked_ends_every_chat_of_its_pid");
        assert!(replay.phases().is_empty());
        let ended = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::Ended))
            .count();
        assert_eq!(ended, 2);
    }

    #[test]
    fn a_new_chat_in_the_process_ends_the_old_one() {
        let replay = replay("a_new_chat_in_the_process_ends_the_old_one");
        assert!(replay.phases().is_empty());
        let ended: Vec<Channel> = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::Ended))
            .map(|(o, _)| o.channel)
            .collect();
        assert_eq!(ended, [Channel::File]);
    }

    #[test]
    fn a_chat_left_while_the_daemon_was_stopped_is_not_followed() {
        let replay = replay("a_chat_whose_store_is_held");
        assert_eq!(fixture::restart(&replay).sessions().len(), 1);
        // /new while the daemon was stopped: the store stays, its log goes.
        let store = replay.home.path().join(
            ".cursor/chats/8c06ea56610da84475132a9aaefbb90e/01f0f07f-a15b-4a43-a037-ec9e03b971b0",
        );
        fs::remove_file(store.join(WAL)).unwrap();
        assert!(store.join(DB).is_file());
        assert!(fixture::restart(&replay).sessions().is_empty());
    }

    #[test]
    fn a_hook_without_a_marked_ancestor_starts_nothing() {
        let replay = replay("a_hook_without_a_marked_ancestor_starts_nothing");
        assert!(replay.yielded.is_empty());
    }

    #[test]
    fn a_subagent_transcript_is_never_read() {
        let replay = replay("a_subagent_transcript_is_never_read");
        assert_eq!(
            phases(&replay),
            [(
                "b6f5f883-1823-4bc2-a601-1cc761c083a1".to_owned(),
                Phase::Stopped
            )]
        );
    }

    #[test]
    fn a_resumed_transcript_does_not_replay() {
        let replay = replay("a_resumed_transcript_does_not_replay");
        let told: Vec<BTreeSet<Signal>> = replay
            .yielded
            .iter()
            .map(|(o, _)| o.event.signals())
            .collect();
        assert_eq!(
            told,
            [
                Signal::Started,
                Signal::Prompted,
                Signal::Prompted,
                Signal::TurnEnded
            ]
            .map(|signal| BTreeSet::from([signal]))
        );
    }

    #[test]
    fn a_restart_names_the_chat_by_its_folder() {
        let replay = replay("turn_ended.file");
        let home = replay.home.path();
        let meta =
            home.join(".cursor/chats/0a1b2c3d/052e7079-8ae4-44f9-a205-4b80fb790165/meta.json");
        fs::create_dir_all(meta.parent().unwrap()).unwrap();
        fs::write(&meta, r#"{"schemaVersion":1,"cwd":"/Users/me/Code/app"}"#).unwrap();
        let restarted = fixture::restart(&replay);
        let labels: Vec<&str> = restarted
            .sessions()
            .iter()
            .map(|session| session.label.as_str())
            .collect();
        assert_eq!(labels, ["app"]);
    }

    #[test]
    fn only_a_finished_read_that_is_not_part_of_the_last_one_is_taken() {
        let bytes = b"{\"role\":\"user\"}\n{\"type\":\"turn_ended\"}\n";
        let taken = take(bytes, None).unwrap();
        assert_eq!(taken, Taken::of(bytes));
        assert!(take(b"", None).is_none());
        assert!(take(b"{\"role\":\"us", None).is_none());
        assert!(take(b"{\"role\":\"user\"}\n", Some(&taken)).is_none());
        assert!(take(bytes, Some(&taken)).is_none());
        let appended = [&bytes[..], b"{\"role\":\"user\"}\n"].concat();
        assert!(take(&appended, Some(&taken)).is_some());
        // A read as long as a start of the last one, but other bytes.
        assert!(take(b"{\"role\":\"asst\"}\n", Some(&taken)).is_some());
    }

    #[test]
    fn a_line_that_is_not_a_record_is_counted_once_however_often_it_is_read() {
        let bytes = b"{\"role\":\"user\"}\n{\"role\n{\"type\":\"turn_ended\"}\n";
        let (mut strange, mut counted) = (Strange::default(), HashSet::new());
        let level = Level::read(bytes, &mut strange, &mut counted);
        assert_eq!(level.users, 1);
        Level::read(bytes, &mut strange, &mut counted);
        assert_eq!(strange.unparsed, 1);
    }

    #[test]
    fn a_marker_names_its_start_whatever_the_process_is_called() {
        let folder = tempfile::tempdir().unwrap();
        let marker = folder.path().join("88985");
        for name in ["cursor-agent", "agent", "node"] {
            fs::write(&marker, format!("{name}|Sat Nov 14 21:54:52 2026")).unwrap();
            assert_eq!(marked(&marker), Some(1_794_693_292), "{name}");
        }
        // Written before the process has looked up its own start.
        fs::write(&marker, "node|").unwrap();
        assert_eq!(marked(&marker), None);
    }

    /// The process the replay's only session is bound to.
    fn bound(replay: &Replay) -> i32 {
        let sessions = replay.queue.sessions();
        assert_eq!(sessions.len(), 1);
        match sessions[0].liveness {
            Liveness::Process { pid, .. } => pid,
            ref other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_hook_under_the_worker_binds_the_chat_to_the_cli() {
        let replay = replay("a_hook_under_the_worker_binds_the_chat_to_the_cli");
        // The hook runs under the worker 89038, the child of the CLI 88985.
        assert_eq!(bound(&replay), 88985);
    }

    #[test]
    fn a_hook_under_the_cli_binds_the_chat_to_it() {
        assert_eq!(bound(&replay("started.hook")), 88985);
    }

    #[test]
    fn a_cli_run_as_agent_is_followed_and_ends_when_it_quits() {
        let replay = replay("a_cli_run_as_agent_is_followed_and_ends_when_it_quits");
        assert!(replay.phases().is_empty());
        let ended = replay
            .yielded
            .last()
            .map(|(o, _)| (o.event.signals(), o.channel));
        assert_eq!(
            ended,
            Some((BTreeSet::from([Signal::Ended]), Channel::File))
        );
    }
}
