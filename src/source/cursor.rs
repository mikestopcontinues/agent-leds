//! Cursor's CLI, `cursor-agent`, from its own files and one hook. Each chat's
//! transcript, `~/.cursor/projects/<slug>/agent-transcripts/<chat>/<chat>.jsonl`,
//! is written at the checkpoints of a turn: appended to, or truncated and
//! rewritten whole in place, as at the first checkpoint of every turn. So it
//! is read whole, and a read counts only once it is finished and new. Each
//! running process, the CLI and its node worker alike, marks itself in
//! `~/.local/share/cursor-agent/versions/<version>/.running/<pid>`, a file
//! that names no chat. The `beforeSubmitPrompt` hook binds the chat to the
//! CLI's process it runs under, and says that the turn started, which a
//! text-only turn writes nothing of until it ends. The `stop` hook says
//! that the user interrupted the turn: an interrupt before the turn's first
//! checkpoint, after a turn that was interrupted too, leaves the transcript
//! as it was.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Cursor 2026.11.13.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use super::{Event, Hook, Observation, Source, Start, Told, tail};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{Channel, Signal};
use crate::harness::{Harness, ends_on_question};
use crate::liveness::{self, Kernel, Liveness, Proc, StartCheck};
use crate::queue::{Key, TurnKey};

const fn declared(
    signal: Signal,
    channel: Channel,
    evidence: Evidence,
    record: &'static str,
    cite: &'static str,
) -> Declared {
    Declared {
        signal,
        channel,
        evidence,
        record,
        cite,
    }
}

/// What Cursor's files and its one hook tell.
pub const SIGNALS: &[Declared] = &[
    declared(
        Signal::Started,
        Channel::Hook,
        Evidence::Observed,
        "beforeSubmitPrompt, run under a process Cursor marked running in .running with the second it started, the outermost of the marked processes in a row, so the CLI and not its node worker",
        "probe-cursor runs A–I (markers, lsof); the bundle's install-in-use-marker; 0.5.0 hooks",
    ),
    declared(
        Signal::Prompted,
        Channel::Hook,
        Evidence::Observed,
        "beforeSubmitPrompt, for a chat bound to its process; the only word of a text-only turn until it ends",
        "probe-cursor run F; 0.5.0 hooks",
    ),
    declared(
        Signal::Prompted,
        Channel::File,
        Evidence::Observed,
        "a finished transcript whose last record is a user or assistant record, after the turn's end or with more user records than before, as after a background job",
        "probe-cursor runs A, B, D, G, H",
    ),
    declared(
        Signal::TurnEnded,
        Channel::File,
        Evidence::Observed,
        "a finished transcript whose last record is turn_ended, once for each count of user records and status",
        "probe-cursor runs A–H",
    ),
    declared(
        Signal::TurnEndedAsking,
        Channel::File,
        Evidence::Observed,
        "turn_ended after an assistant text whose last line ends in a question mark; a reasoning model's reasoning follows its reply in that text, so its last line is tested instead",
        "probe-cursor runs A, B",
    ),
    declared(
        Signal::TurnFailed,
        Channel::File,
        Evidence::Inferred,
        "turn_ended with status error, from the bundle's code; not reproduced",
        "probe-cursor turn_error (bundle 2986/7777.index.js)",
    ),
    declared(
        Signal::Interrupted,
        Channel::Hook,
        Evidence::Inferred,
        "stop with status aborted, for a chat bound to its process; Cursor runs it on every interrupt, from the bundle's code",
        "probe-cursor run B, turn 3 (no write); bundle 2986.index.js, the abort path",
    ),
    declared(
        Signal::Interrupted,
        Channel::File,
        Evidence::Observed,
        "turn_ended with status aborted, including the previous turn's end rewritten to aborted",
        "probe-cursor runs B, H",
    ),
    declared(
        Signal::Ended,
        Channel::File,
        Evidence::Observed,
        "the .running marker of the process the chat is bound to deleted, as a quit does",
        "probe-cursor runs A, C, D, G, I",
    ),
    declared(
        Signal::Ended,
        Channel::Hook,
        Evidence::Observed,
        "the process the hook bound the chat to gone, as after kill -9, which leaves its marker",
        "probe-cursor runs B, H",
    ),
];

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

impl Level {
    fn read(bytes: &[u8]) -> Level {
        let mut level = Level {
            users: 0,
            last: None,
            text: String::new(),
        };
        let records = bytes.split(|&byte| byte == b'\n');
        for record in records.filter_map(|line| serde_json::from_slice::<Record>(line).ok()) {
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

/// Whether a whole read of a transcript is one to take: finished, and not
/// a part of the read taken last. A rewrite truncates the file in place and
/// writes it again from the start, so a read caught in the middle is the
/// start of the new file and, since every rewrite keeps the history, of the
/// last one taken too.
fn finished(bytes: &[u8], taken: Option<&[u8]>) -> bool {
    !bytes.is_empty()
        && bytes.ends_with(b"\n")
        && !taken.is_some_and(|taken| taken.starts_with(bytes))
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
    taken: Option<Vec<u8>>,
    level: Option<Level>,
    told: Told,
}

impl Session {
    fn new(pid: i32, start: i64, marker: PathBuf) -> Session {
        Session {
            pid,
            start,
            marker,
            taken: None,
            level: None,
            told: Told::default(),
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
}

impl Cursor {
    pub fn new(home: &Path) -> Cursor {
        Cursor {
            projects: home.join(".cursor/projects"),
            versions: home.join(".local/share/cursor-agent/versions"),
            chats: home.join(".cursor/chats"),
            sessions: BTreeMap::new(),
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

    /// The folder the chat works in, as Cursor's record of it says.
    fn folder(&self, id: &str) -> String {
        #[derive(Deserialize)]
        struct Meta {
            cwd: String,
        }
        let meta = fs::read_dir(&self.chats)
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|bucket| {
                let text = fs::read(bucket.path().join(id).join("meta.json")).ok()?;
                serde_json::from_slice::<Meta>(&text).ok()
            });
        meta.map(|meta| meta.cwd).unwrap_or_default()
    }

    /// Reads the chat's transcript whole and tells what it says that the
    /// last read taken did not.
    fn read(&mut self, id: &str, out: &mut Vec<Observation>) {
        let Some(path) = self.find(id) else {
            return;
        };
        // Changing while it was read, or gone: read again at the next change.
        let Ok(Some(bytes)) = tail::stable(&path) else {
            return;
        };
        let session = self.sessions.get_mut(id).expect("bound");
        if !finished(&bytes, session.taken.as_deref()) {
            return;
        }
        let level = Level::read(&bytes);
        let signals = level.since(session.level.as_ref());
        session.taken = Some(bytes);
        session.level = Some(level);
        if !signals.is_empty() {
            tell(id, session, Channel::File, signals, out);
        }
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
            out.push(observation(
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
            self.read(&id, &mut out);
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
                    self.read(&id, &mut Vec::new());
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
                out.push(observation(
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

    /// An interrupt of a bound chat. One before the turn's first checkpoint
    /// rewrites the transcript to end the previous turn as aborted, which
    /// says nothing new when it already ended so.
    fn interrupt(&mut self, id: &str) -> Vec<Observation> {
        let mut out = Vec::new();
        if !self.sessions.contains_key(id) {
            return out;
        }
        self.read(id, &mut out);
        let session = self.sessions.get_mut(id).expect("bound");
        let signals = BTreeSet::from([Signal::Interrupted]);
        tell(id, session, Channel::Hook, signals, &mut out);
        out
    }
}

fn observation(id: &str, channel: Channel, event: Event, turn: Option<TurnKey>) -> Observation {
    Observation {
        key: Key {
            harness: Harness::Cursor,
            id: id.to_owned(),
        },
        channel,
        event,
        turn,
        quiet: false,
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
    session.told.tell(&signals);
    let turn = match channel {
        // The hook comes before the turn's records, so it cannot name it.
        Channel::Hook => None,
        _ => session.turn(),
    };
    out.push(observation(id, channel, Event::Signals(signals), turn));
}

impl Source for Cursor {
    fn harness(&self) -> Harness {
        Harness::Cursor
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.projects.clone(), self.versions.clone()]
    }

    /// Each chat a hook bound before the daemon started, whose process still
    /// runs, as its transcript leaves it. A chat no hook bound names no
    /// process, so it shows from its next prompt.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Vec<Observation>> {
        let mut seeded = Vec::new();
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
            self.sessions
                .insert(id.clone(), Session::new(pid, start, marker));
            self.read(id, &mut Vec::new());
            let turn = self.sessions[id].turn();
            let start = Start {
                cwd: self.folder(id),
                liveness: liveness.clone(),
            };
            seeded.push(observation(id, Channel::Hook, Event::Started(start), turn));
            seeded.extend(
                self.current(id)
                    .into_iter()
                    .map(|(signal, turn)| observation(id, Channel::File, Event::one(signal), turn)),
            );
        }
        for o in &mut seeded {
            o.quiet = true;
        }
        Ok(seeded)
    }

    fn changed(&mut self, _k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        let (mut chats, mut markers): (Vec<String>, Vec<PathBuf>) = match paths {
            Some(paths) => (
                paths.iter().filter_map(|path| self.chat_of(path)).collect(),
                paths
                    .iter()
                    .filter(|path| self.is_marker(path))
                    .cloned()
                    .collect(),
            ),
            None => (
                self.sessions.keys().cloned().collect(),
                self.sessions.values().map(|s| s.marker.clone()).collect(),
            ),
        };
        chats.sort();
        chats.dedup();
        markers.sort();
        markers.dedup();
        let mut out = Vec::new();
        // The last checkpoint is written before the process unlinks its marker.
        for id in chats {
            if self.sessions.contains_key(&id) {
                self.read(&id, &mut out);
            }
        }
        for marker in markers {
            self.gone(&marker, &mut out);
        }
        out
    }

    /// A prompt or an interrupt, each after catching up on the chat's
    /// transcript.
    fn hook(&mut self, _k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let Ok(payload) = Payload::deserialize(&hook.payload) else {
            return Vec::new();
        };
        match payload.hook_event_name.as_str() {
            "beforeSubmitPrompt" => self.prompt(payload, &hook.ancestry),
            // A turn that completes or fails says so in its transcript.
            "stop" if payload.status.as_deref() == Some("aborted") => {
                self.interrupt(&payload.conversation_id)
            }
            _ => Vec::new(),
        }
    }

    fn current(&self, id: &str) -> Vec<(Signal, Option<TurnKey>)> {
        let Some(session) = self.sessions.get(id) else {
            return Vec::new();
        };
        let current = session.told.current().into_iter();
        current.map(|signal| (signal, session.turn())).collect()
    }

    /// Never: a transcript is read whole.
    fn behind(&self) -> bool {
        false
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
                        let level = Level::read(&bytes);
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
        let saved = crate::config::state_dir().join("queue.json");
        let bound: Vec<(String, Liveness)> = fs::read_to_string(saved)
            .ok()
            .and_then(|text| serde_json::from_str::<crate::queue::Queue>(&text).ok())
            .map(|queue| {
                let bound = queue.bound().into_iter();
                bound
                    .filter(|(key, _)| key.harness == Harness::Cursor)
                    .map(|(key, liveness)| (key.id, liveness))
                    .collect()
            })
            .unwrap_or_default();
        println!("{} chats the saved queue binds", bound.len());
        let mut queue = crate::queue::Queue::default();
        for o in cursor.seed(&liveness::System, &bound).unwrap() {
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
        let replay = replay("interrupted.hook");
        assert_eq!(
            phases(&replay),
            [(
                "b6f5f883-1823-4bc2-a601-1cc761c083a1".to_owned(),
                Phase::Idle
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
        let taken = b"{\"role\":\"user\"}\n{\"type\":\"turn_ended\"}\n";
        assert!(finished(taken, None));
        assert!(!finished(b"", None));
        assert!(!finished(b"{\"role\":\"us", None));
        assert!(!finished(b"{\"role\":\"user\"}\n", Some(taken)));
        assert!(!finished(taken, Some(taken)));
        let appended = [&taken[..], b"{\"role\":\"user\"}\n"].concat();
        assert!(finished(&appended, Some(taken)));
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
        // The hook runs under the worker 89038, the child of the CLI 88985.
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
