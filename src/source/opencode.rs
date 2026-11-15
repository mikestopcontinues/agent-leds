//! OpenCode, from its database and our plugin. Every OpenCode process, the
//! terminal interface, `opencode serve`, `opencode run` and the desktop app
//! alike, writes its sessions to one SQLite database in WAL mode,
//! `~/.local/share/opencode/opencode.db`: a row per session, message and
//! part, rewritten as they change, and since 1.18 an append-only log of
//! events written in the same commit. The database says when a turn starts,
//! when it waits on a question and how it ends.
//!
//! It records no process, no permission request, and no error that comes
//! before a turn's loop starts. The plugin of ours tells those: a session
//! going busy, with the pid of the process that runs it; a permission asked
//! and replied; and an error.
//!
//! The database is undocumented. What it holds at each moment of a session
//! was taken from OpenCode 1.19.5, and from 1.18.25's `run`.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;

use super::{Event, Hook, Observation, Source, Start, Told};
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

const fn hook(signal: Signal, record: &'static str, cite: &'static str) -> Declared {
    Declared {
        signal,
        channel: Channel::Hook,
        evidence: Evidence::Observed,
        record,
        cite,
    }
}

/// What OpenCode's database and our plugin tell, all of it but the plugin's
/// error seen in the probe's runs.
pub const SIGNALS: &[Declared] = &[
    hook(
        Signal::Started,
        "the plugin's busy, as a session that is not a subagent's goes busy, from the OpenCode process whose pid it names",
        "probe-opencode T3–T16 (session.status busy, in serve and the TUI)",
    ),
    hook(
        Signal::Prompted,
        "the plugin's busy",
        "probe-opencode T3–T16 (session.status busy, in serve and the TUI)",
    ),
    file(
        Signal::Prompted,
        "the session's latest message a user message, or an assistant message answering one, other than the turn last read",
        "probe-opencode T2–T16",
    ),
    hook(
        Signal::PermissionAsked,
        "permission.asked, for the session or a subagent's session under it",
        "probe-opencode T3, T4, T7, T8, T12",
    ),
    file(
        Signal::QuestionAsked,
        "a question tool part running in the turn, in the session or a subagent's session under it",
        "probe-opencode T5, T6, T16",
    ),
    hook(
        Signal::Answered,
        "permission.replied, for the session or a subagent's session under it",
        "probe-opencode T3, T7, T12",
    ),
    file(
        Signal::Answered,
        "no question tool part running in the turn any more, while it runs",
        "probe-opencode T5",
    ),
    file(
        Signal::TurnEnded,
        "the turn's latest assistant message completed, unless it is a step that finished on tool-calls or unknown with no error and no tool the user refused or dismissed",
        "probe-opencode T2–T10, T15, T16",
    ),
    file(
        Signal::TurnEndedAsking,
        "that message's last text part, not synthetic, ending on a question mark",
        "probe-opencode T2",
    ),
    file(
        Signal::TurnFailed,
        "that message carrying an error, a MessageAbortedError included",
        "probe-opencode T7–T9",
    ),
    // No run of the probe caught the plugin's error for an unknown model.
    Declared {
        evidence: Evidence::Inferred,
        ..hook(
            Signal::TurnFailed,
            "session.error, for the session itself, while it is busy, other than a ContextOverflowError",
            "OpenCode 1.19.5's SessionPrompt.run, which sets the session busy before getModel publishes session.error UnknownError; probe-opencode T11 (a user message and no reply)",
        )
    },
    file(
        Signal::Interrupted,
        "that message's error a MessageAbortedError, or the message with neither finish nor error",
        "probe-opencode T7, T8, T10",
    ),
    hook(
        Signal::Interrupted,
        "session.error with a MessageAbortedError",
        "probe-opencode T7, T8",
    ),
    file(
        Signal::Ended,
        "the session's row deleted or archived",
        "probe-opencode T13",
    ),
    hook(
        Signal::Ended,
        "the process the plugin's busy named gone",
        "probe-opencode T14, T17",
    ),
];

/// What the plugin sends.
#[derive(Deserialize)]
struct Payload {
    event: String,
    /// OpenCode's own id of the session the event is about, a subagent's
    /// session included.
    session: String,
    /// The OpenCode process the plugin runs in, at `busy`.
    pid: Option<i32>,
    /// The folder OpenCode was started in, at `busy`.
    directory: Option<String>,
    /// The name of the error, at `session.error`, which the plugin sends
    /// only for an error that ends the run.
    error: Option<String>,
}

/// The `data` of a message row, with only the fields that carry state.
#[derive(Deserialize)]
struct Message {
    role: String,
    /// The user message an assistant message answers.
    #[serde(rename = "parentID")]
    parent: Option<String>,
    #[serde(default)]
    time: Times,
    finish: Option<String>,
    error: Option<Error>,
}

#[derive(Default, Deserialize)]
struct Times {
    completed: Option<Value>,
}

#[derive(Deserialize)]
struct Error {
    name: Option<String>,
}

/// The `data` of a text part.
#[derive(Deserialize)]
struct Text {
    #[serde(default)]
    text: String,
    /// Set on text OpenCode wrote itself.
    #[serde(default)]
    synthetic: bool,
}

/// Where the database leaves a session's turn.
#[derive(Clone, Debug, PartialEq)]
enum Level {
    /// No message yet.
    None,
    /// A turn runs for the user message `user`, held up by a question when
    /// a question tool waits on the user in it.
    Open { user: String, question: bool },
    /// The turn for the user message `user` ended in the assistant message
    /// `assistant`, which tells `end`.
    Ended {
        user: String,
        assistant: String,
        end: BTreeSet<Signal>,
    },
}

impl Level {
    /// The user message that started the turn.
    fn user(&self) -> Option<&str> {
        match self {
            Level::None => None,
            Level::Open { user, .. } | Level::Ended { user, .. } => Some(user),
        }
    }

    fn turn(&self) -> Option<TurnKey> {
        self.user().map(|user| TurnKey(user.to_owned()))
    }

    /// What the level says, told all at once.
    fn signals(&self) -> BTreeSet<Signal> {
        match self {
            Level::None => BTreeSet::new(),
            Level::Open { question, .. } => {
                let mut signals = BTreeSet::from([Signal::Prompted]);
                if *question {
                    signals.insert(Signal::QuestionAsked);
                }
                signals
            }
            Level::Ended { end, .. } => end.clone(),
        }
    }
}

/// A session a busy from the plugin named, followed in the database.
struct Session {
    /// The OpenCode process that runs it.
    liveness: Liveness,
    /// Whether its row has been read: one never read is not yet written,
    /// not deleted.
    known: bool,
    /// The session and its subagents' sessions, each with its last event's
    /// seq. While they hold, so does the level.
    stamp: Vec<(String, Option<i64>)>,
    level: Level,
}

/// A session's row.
struct Row {
    parent: Option<String>,
    directory: String,
    archived: bool,
}

/// The database, opened read-only.
struct Database {
    connection: Connection,
    /// The file it was opened as: a database replaced in its place is
    /// opened again.
    ino: u64,
    /// `PRAGMA data_version` at the last read: it changes with every commit
    /// another connection makes.
    version: Option<i64>,
}

/// The statements the source runs, each checked against the schema when the
/// database is opened, so a schema without the columns read fails at once.
const ROW: &str = "select parent_id, directory, time_archived from session where id = ?1";
const STAMP: &str = "with recursive tree(id) as \
     (select ?1 union select session.id from session join tree on session.parent_id = tree.id) \
     select tree.id, (select max(seq) from event where aggregate_id = tree.id) from tree order by tree.id";
const LATEST: &str = "select id, time_created, data from message where session_id = ?1 \
     order by time_created desc, id desc limit 1";
/// A tool part failed for a permission the user refused or a question they
/// dismissed, the only tool errors that stop OpenCode's loop. The texts are
/// the messages of its PermissionRejectedError and QuestionRejectedError.
const REFUSED: &str = "select exists(select 1 from part where message_id = ?1 \
     and json_extract(data, '$.type') = 'tool' and json_extract(data, '$.state.status') = 'error' \
     and json_extract(data, '$.state.error') in \
     ('The user rejected permission to use this specific tool call.', 'The user dismissed this question'))";
const TEXTS: &str = "select data from part where message_id = ?1 \
     and json_extract(data, '$.type') = 'text' order by id";
const QUESTION: &str = "with recursive tree(id) as \
     (select ?1 union select session.id from session join tree on session.parent_id = tree.id) \
     select exists(select 1 from part where session_id in (select id from tree) and time_created >= ?2 \
     and json_extract(data, '$.type') = 'tool' and json_extract(data, '$.tool') = 'question' \
     and json_extract(data, '$.state.status') = 'running')";

impl Database {
    fn open(path: &Path, ino: u64) -> Result<Database> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(path, flags)
            .with_context(|| format!("opening {}", path.display()))?;
        connection.busy_timeout(Duration::from_millis(250))?;
        for statement in [ROW, STAMP, LATEST, REFUSED, TEXTS, QUESTION] {
            connection.prepare_cached(statement).with_context(|| {
                format!(
                    "{} is not a database this version reads (OpenCode 1.18 or later)",
                    path.display()
                )
            })?;
        }
        Ok(Database {
            connection,
            ino,
            version: None,
        })
    }
}

/// The session's row, or None if it is not in the database.
fn row(db: &Connection, id: &str) -> Result<Option<Row>> {
    let mut statement = db.prepare_cached(ROW)?;
    let row = statement
        .query_row([id], |row| {
            Ok(Row {
                parent: row.get(0)?,
                directory: row.get(1)?,
                archived: row.get::<_, Option<i64>>(2)?.is_some(),
            })
        })
        .optional()?;
    Ok(row)
}

/// The session and its subagents' sessions, each with its last event's seq.
fn stamp(db: &Connection, id: &str) -> Result<Vec<(String, Option<i64>)>> {
    let mut statement = db.prepare_cached(STAMP)?;
    let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Where the session's messages leave its turn.
fn level(db: &Connection, id: &str) -> Result<Level> {
    let latest = db
        .prepare_cached(LATEST)?
        .query_row([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()?;
    let Some((message, created, data)) = latest else {
        return Ok(Level::None);
    };
    let data: Message =
        serde_json::from_str(&data).with_context(|| format!("reading message {message}"))?;
    if data.role == "user" {
        return Ok(Level::Open {
            user: message,
            question: false,
        });
    }
    let user = data
        .parent
        .ok_or_else(|| anyhow!("message {message} answers no user message"))?;
    let completed = data.time.completed.is_some_and(|time| !time.is_null());
    // OpenCode's loop goes on after a step that finished on its tool calls or
    // for no known reason, and writes the next step's message only after it
    // has read the session's messages again, so this step may be the latest
    // for a while. Only an error, or a tool the user refused or dismissed,
    // stops the loop there.
    let goes_on = matches!(data.finish.as_deref(), Some("tool-calls" | "unknown"));
    let ended = completed
        && (!goes_on
            || data.error.is_some()
            || db
                .prepare_cached(REFUSED)?
                .query_row([&message], |row| row.get(0))?);
    if !ended {
        // Only the latest step and the subagents it started can be held up.
        let question = db
            .prepare_cached(QUESTION)?
            .query_row(rusqlite::params![id, created], |row| row.get(0))?;
        return Ok(Level::Open { user, question });
    }
    let mut end = BTreeSet::from([Signal::TurnEnded]);
    if ends_on_question(&text(db, &message)?) {
        end.insert(Signal::TurnEndedAsking);
    }
    if let Some(error) = &data.error {
        end.insert(Signal::TurnFailed);
        if error.name.as_deref() == Some("MessageAbortedError") {
            end.insert(Signal::Interrupted);
        }
    }
    // Stopped while a request was retried: no finish, and no error. A step
    // that overflowed the context and is compacted automatically looks the
    // same until the compaction's message is written just after.
    if data.finish.is_none() && data.error.is_none() {
        end.insert(Signal::Interrupted);
    }
    Ok(Level::Ended {
        user,
        assistant: message,
        end,
    })
}

/// The message's last text part that OpenCode did not write itself.
fn text(db: &Connection, message: &str) -> Result<String> {
    let mut statement = db.prepare_cached(TEXTS)?;
    let texts = statement.query_map([message], |row| row.get::<_, String>(0))?;
    let mut last = String::new();
    for data in texts {
        let text: Text = serde_json::from_str(&data?)?;
        if !text.synthetic {
            last = text.text;
        }
    }
    Ok(last)
}

/// The session a subagent's session works for, followed up to the session
/// no other started; the session itself when it is not a subagent's, or its
/// row is not there.
fn root(db: &Connection, id: &str) -> Result<String> {
    let mut id = id.to_owned();
    // Subagents start subagents, though never this deep.
    for _ in 0..16 {
        match row(db, &id)?.and_then(|row| row.parent) {
            Some(parent) => id = parent,
            None => break,
        }
    }
    Ok(id)
}

/// OpenCode's sessions, from its database and the plugin.
pub struct OpenCode {
    folder: PathBuf,
    path: PathBuf,
    db: Option<Database>,
    sessions: HashMap<String, Session>,
}

impl OpenCode {
    pub fn new(home: &Path) -> OpenCode {
        let folder = home.join(".local/share/opencode");
        OpenCode {
            path: folder.join("opencode.db"),
            folder,
            db: None,
            sessions: HashMap::new(),
        }
    }

    /// Whether a change to `path` can be a commit: one to the database, or
    /// to its write-ahead log.
    fn commits(&self, path: &Path) -> bool {
        path.parent() == Some(&self.folder)
            && path
                .file_name()
                .is_some_and(|name| name == "opencode.db" || name == "opencode.db-wal")
    }

    /// The database, opened if it is there and was not opened as it is.
    fn open(&mut self) -> Result<Option<&Connection>> {
        let ino = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.ino(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.db = None;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if self.db.as_ref().is_none_or(|db| db.ino != ino) {
            self.db = Some(Database::open(&self.path, ino)?);
        }
        Ok(self.db.as_ref().map(|db| &db.connection))
    }

    /// What the database says of every session followed, if another process
    /// committed since the last read. A session whose process is gone is
    /// let go, and the sweep ends it.
    fn read(&mut self, k: &dyn Kernel, out: &mut Vec<Observation>) {
        self.sessions
            .retain(|_, session| liveness::alive(k, &session.liveness));
        if let Err(error) = self.read_sessions(out) {
            eprintln!("agent-leds: reading {}: {error:#}", self.path.display());
        }
    }

    fn read_sessions(&mut self, out: &mut Vec<Observation>) -> Result<()> {
        if self.sessions.is_empty() || self.open()?.is_none() {
            return Ok(());
        }
        let db = self.db.as_mut().expect("opened");
        let version: i64 = db
            .connection
            .query_row("pragma data_version", [], |row| row.get(0))?;
        if db.version == Some(version) {
            return Ok(());
        }
        db.version = Some(version);
        // One snapshot for every session, held only for the reads.
        let tx = db.connection.unchecked_transaction()?;
        let mut ended = Vec::new();
        for (id, session) in &mut self.sessions {
            match row(&tx, id)? {
                None if !session.known => continue,
                None => {
                    ended.push(id.clone());
                    continue;
                }
                Some(row) if row.archived => {
                    ended.push(id.clone());
                    continue;
                }
                Some(_) => session.known = true,
            }
            let stamp = stamp(&tx, id)?;
            if stamp == session.stamp {
                continue;
            }
            let level = level(&tx, id)?;
            moved(id, &session.level, &level, out);
            session.stamp = stamp;
            session.level = level;
        }
        tx.commit()?;
        for id in ended {
            self.sessions.remove(&id);
            out.push(Observation::new(
                Harness::OpenCode,
                &id,
                Channel::File,
                Event::one(Signal::Ended),
                None,
            ));
        }
        Ok(())
    }

    /// The session a subagent's session works for, as the database says.
    fn root(&mut self, id: &str) -> String {
        match self.open() {
            Ok(Some(db)) => root(db, id).unwrap_or_else(|_| id.to_owned()),
            _ => id.to_owned(),
        }
    }

    /// Starts following a session from where the database leaves it now, or
    /// goes on following it if the same process still runs it. Says where it
    /// works: the database's folder for it, else `directory`.
    fn follow(&mut self, id: &str, liveness: Liveness, directory: String) -> Result<String> {
        let read = match self.open()? {
            Some(db) => {
                let tx = db.unchecked_transaction()?;
                let read = match row(&tx, id)? {
                    Some(row) => Some((row, stamp(&tx, id)?, level(&tx, id)?)),
                    None => None,
                };
                tx.commit()?;
                read
            }
            None => None,
        };
        let cwd = match &read {
            Some((row, _, _)) => row.directory.clone(),
            None => directory,
        };
        if self
            .sessions
            .get(id)
            .is_some_and(|session| session.liveness == liveness)
        {
            return Ok(cwd);
        }
        let (known, stamp, level) = match read {
            Some((_, stamp, level)) => (true, stamp, level),
            None => (false, Vec::new(), Level::None),
        };
        self.sessions.insert(
            id.to_owned(),
            Session {
                liveness,
                known,
                stamp,
                level,
            },
        );
        Ok(cwd)
    }

    fn turn(&self, id: &str) -> Option<TurnKey> {
        self.sessions
            .get(id)
            .and_then(|session| session.level.turn())
    }
}

/// What a session's level moving from `from` to `to` tells.
fn moved(id: &str, from: &Level, to: &Level, out: &mut Vec<Observation>) {
    let turn = to.turn();
    let tell = |signals: BTreeSet<Signal>, out: &mut Vec<Observation>| {
        out.push(Observation::new(
            Harness::OpenCode,
            id,
            Channel::File,
            Event::Signals(signals),
            turn.clone(),
        ));
    };
    let new = to.user().is_some() && to.user() != from.user();
    match (from, to) {
        (_, Level::None) => {}
        // A turn starts, or one that looked ended goes on.
        (_, Level::Open { .. }) if new || matches!(from, Level::Ended { .. }) => {
            tell(to.signals(), out)
        }
        (
            Level::Open {
                question: before, ..
            },
            Level::Open { question, .. },
        ) => match (before, question) {
            (false, true) => tell(BTreeSet::from([Signal::QuestionAsked]), out),
            (true, false) => tell(BTreeSet::from([Signal::Answered]), out),
            _ => {}
        },
        (_, Level::Open { .. }) => {}
        // Each end is told once, however often its rows are written again.
        (_, Level::Ended { end, .. }) => {
            if new {
                tell(BTreeSet::from([Signal::Prompted]), out);
            }
            if from != to {
                tell(end.clone(), out);
            }
        }
    }
}

impl Source for OpenCode {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn roots(&self) -> Vec<PathBuf> {
        vec![self.folder.clone()]
    }

    /// Each session a busy bound to a process that still runs, from where
    /// the database leaves it. The database names no process, so it starts
    /// no session by itself.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Vec<Observation>> {
        let mut seeded = Vec::new();
        // A database that cannot be read is said so even with nothing to seed.
        self.open()?;
        for (id, liveness) in bound {
            if !liveness::alive(k, liveness) {
                continue;
            }
            if let Some(db) = self.open()?
                && row(db, id)?.is_none_or(|row| row.archived)
            {
                continue;
            }
            let cwd = self.follow(id, liveness.clone(), String::new())?;
            let turn = self.turn(id);
            seeded.push(Observation::new(
                Harness::OpenCode,
                id,
                Channel::Hook,
                Event::Started(Start {
                    cwd,
                    liveness: liveness.clone(),
                }),
                turn.clone(),
            ));
            // What only the plugin told, a permission or an error before the
            // turn's loop, is not in the database.
            let mut told = Told::default();
            told.tell(&self.sessions[id].level.signals());
            for signal in told.current() {
                seeded.push(Observation::new(
                    Harness::OpenCode,
                    id,
                    Channel::File,
                    Event::one(signal),
                    turn.clone(),
                ));
            }
        }
        if let Some(db) = &mut self.db {
            db.version = db
                .connection
                .query_row("pragma data_version", [], |row| row.get(0))
                .ok();
        }
        for o in &mut seeded {
            o.quiet = true;
        }
        Ok(seeded)
    }

    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
        let mut out = Vec::new();
        if paths.is_none_or(|paths| paths.iter().any(|path| self.commits(path))) {
            self.read(k, &mut out);
        }
        out
    }

    fn hook(&mut self, k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let mut out = Vec::new();
        self.read(k, &mut out);
        let Ok(payload) = Payload::deserialize(&hook.payload) else {
            return out;
        };
        // A subagent's session is held up for the session it works for, and
        // its going busy or failing says nothing of that session's turn.
        let id = self.root(&payload.session);
        let own = id == payload.session;
        let signals = match payload.event.as_str() {
            "busy" if own => {
                // The plugin runs the hook itself, so the process it names is
                // the hook's parent, whose start tells it from a later one.
                let liveness = hook
                    .ancestry
                    .iter()
                    .find(|proc| Some(proc.pid) == payload.pid)
                    .map(|proc| Liveness::Process {
                        pid: proc.pid,
                        start: StartCheck::ExactUs(proc.started_us),
                    });
                let Some(liveness) = liveness else {
                    return out;
                };
                let directory = payload.directory.unwrap_or_default();
                let cwd = match self.follow(&id, liveness.clone(), directory.clone()) {
                    Ok(cwd) => cwd,
                    Err(error) => {
                        eprintln!("agent-leds: reading {}: {error:#}", self.path.display());
                        directory
                    }
                };
                out.push(Observation::new(
                    Harness::OpenCode,
                    &id,
                    Channel::Hook,
                    Event::Started(Start { cwd, liveness }),
                    self.turn(&id),
                ));
                BTreeSet::from([Signal::Prompted])
            }
            "permission.asked" => BTreeSet::from([Signal::PermissionAsked]),
            "permission.replied" => BTreeSet::from([Signal::Answered]),
            "session.error" if own => {
                let mut signals = BTreeSet::from([Signal::TurnFailed]);
                if payload.error.as_deref() == Some("MessageAbortedError") {
                    signals.insert(Signal::Interrupted);
                }
                signals
            }
            _ => return out,
        };
        out.push(Observation::new(
            Harness::OpenCode,
            &id,
            Channel::Hook,
            Event::Signals(signals),
            self.turn(&id),
        ));
        out
    }

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
        fixture::replay(Harness::OpenCode, name)
    }

    /// The phase the replay left its only session in.
    fn phase(replay: &Replay) -> Phase {
        let phases = replay.phases();
        assert_eq!(phases.len(), 1, "{phases:?}");
        phases.into_values().next().unwrap()
    }

    /// Opens the real database read-only and prints what it holds of each
    /// root session that a turn left open, from where the database leaves
    /// it. Nothing binds a process to them, so none is seeded.
    #[test]
    #[ignore = "reads the real ~/.local/share/opencode"]
    fn seeds_from_the_real_home() {
        let home = std::env::var_os("HOME").expect("a home");
        let mut opencode = OpenCode::new(Path::new(&home));
        let seeded = opencode.seed(&liveness::System, &[]).unwrap();
        assert!(seeded.is_empty(), "no session is bound to a process");
        let db = opencode.open().unwrap().expect("a database");
        let mut roots = db
            .prepare("select id, title from session where parent_id is null and time_archived is null order by time_updated desc")
            .unwrap();
        let roots: Vec<(String, String)> = roots
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        println!("{} sessions", roots.len());
        for (id, title) in roots {
            let level = level(db, &id).unwrap();
            println!("{id}  {title:<40.40}  {level:?}");
        }
    }

    #[test]
    fn every_fixture_replays_as_it_expects() {
        for name in fixture::names(Harness::OpenCode) {
            replay(&name);
        }
    }

    #[test]
    fn a_deleted_session_ends() {
        assert!(replay("a_deleted_session_ends").phases().is_empty());
    }

    #[test]
    fn reused_rowids_after_a_delete_are_not_skipped() {
        assert_eq!(
            phase(&replay("reused_rowids_after_a_delete_are_not_skipped")),
            Phase::Asked
        );
    }

    #[test]
    fn a_childs_question_holds_the_parent() {
        let replay = replay("a_childs_question_holds_the_parent");
        let parent = "ses_e23f20dc0ffe0K5UEiYZ4lxHfx";
        assert!(replay.yielded.iter().all(|(o, _)| o.key.id == parent));
        let blocked = replay
            .yielded
            .iter()
            .any(|(o, _)| o.event.signals() == BTreeSet::from([Signal::QuestionAsked]));
        assert!(blocked);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_childs_permission_holds_the_parent() {
        let replay = replay("a_childs_permission_holds_the_parent");
        let parent = "ses_e23f20dc0ffe0K5UEiYZ4lxHfx";
        assert!(replay.yielded.iter().all(|(o, _)| o.key.id == parent));
        assert_eq!(phase(&replay), Phase::Working);
    }

    #[test]
    fn finish_tool_calls_with_a_rejected_tool_and_nothing_after_is_a_stop() {
        assert_eq!(
            phase(&replay(
                "finish_tool_calls_with_a_rejected_tool_and_nothing_after_is_a_stop"
            )),
            Phase::Stopped
        );
    }

    #[test]
    fn a_step_whose_tool_failed_goes_on_to_the_next() {
        let replay = replay("a_step_whose_tool_failed_goes_on_to_the_next");
        let ends = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::TurnEnded))
            .count();
        assert_eq!(ends, 1);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_step_that_finished_unknown_goes_on_to_the_next() {
        let replay = replay("a_step_that_finished_unknown_goes_on_to_the_next");
        let ends = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::TurnEnded))
            .count();
        assert_eq!(ends, 1);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn the_auto_title_update_does_not_re_announce() {
        let replay = replay("the_auto_title_update_does_not_re_announce");
        let ends = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::TurnEnded))
            .count();
        assert_eq!(ends, 1);
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_crashed_turn_left_running_is_not_working_once_the_session_runs_again() {
        let replay = replay("turn_ended.file");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_stop_told_as_an_error_is_an_interrupt() {
        assert_eq!(phase(&replay("interrupted.hook")), Phase::Idle);
    }

    #[test]
    fn a_busy_from_a_process_not_among_the_hook_s_ancestors_starts_nothing() {
        let replay = replay("started.hook");
        let mut source = replay.source();
        let hook = Hook {
            harness: Harness::OpenCode,
            payload: serde_json::json!({ "event": "busy", "session": "ses_e23f20dc0ffe0K5UEiYZ4lxHfx", "pid": 1, "directory": "/" }),
            ancestry: Vec::new(),
        };
        assert!(source.hook(&replay.kernel, &hook).is_empty());
    }

    #[test]
    fn a_database_older_than_the_event_log_fails_the_seed() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join(".local/share/opencode");
        fs::create_dir_all(&folder).unwrap();
        let db = Connection::open(folder.join("opencode.db")).unwrap();
        db.execute_batch("create table session (id text primary key, parent_id text, directory text, time_archived integer)")
            .unwrap();
        let error = OpenCode::new(home.path())
            .seed(&liveness::System, &[])
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("OpenCode 1.18 or later"),
            "{error:#}"
        );
    }
}
