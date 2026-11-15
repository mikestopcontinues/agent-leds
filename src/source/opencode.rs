//! OpenCode, from its database and our plugin. Every OpenCode process, the
//! terminal interface, `opencode serve`, `opencode run` and the desktop app
//! alike, writes its sessions to one SQLite database in WAL mode,
//! `~/.local/share/opencode/opencode.db`.
//!
//! OpenCode 1.x writes a row per session, message and part, rewritten as
//! they change, and since 1.18 an append-only log of events written in the
//! same commit. 2.x keeps its sessions in tables of its own, `session_v2`
//! and `session_message`, and writes on each session's row when a run
//! started and when it went idle, how, and when the user last saw it idle.
//! Its first start copies 1.x's sessions into them and leaves 1.x's tables
//! in place, and a 1.x process run on that database afterwards, the desktop
//! app or Zed's agent, goes on writing 1.x's tables. Each session is read
//! from the tables that hold its latest run.
//!
//! The database records no process and no permission request, and 1.x no
//! error that comes before a turn's loop starts. The plugin of ours tells
//! those: a session going busy, with the pid of the process that runs it; a
//! permission asked and replied; and, from 1.x, an error.
//!
//! The database is undocumented. What it holds at each moment of a session
//! was taken from OpenCode 1.19.5, 1.18.25's `run`, and 2.3.0.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::mem;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;

use super::{Event, Hook, Observation, Seed, Source, Start, Strange, Told, Unread};
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
        untold: None,
    }
}

const fn hook(signal: Signal, record: &'static str, cite: &'static str) -> Declared {
    Declared {
        signal,
        channel: Channel::Hook,
        evidence: Evidence::Observed,
        record,
        cite,
        untold: None,
    }
}

/// What OpenCode's database and our plugin tell, all of it but the plugin's
/// error seen in the probe's runs.
pub const SIGNALS: &[Declared] = &[
    hook(
        Signal::Started,
        "the plugin's busy, as a session goes busy (1.x) or starts a run (2.x), or as 2.x first steps or asks in a process, as a run the service resumes does, from the OpenCode process whose pid it names: 2.x's background service, or the server a terminal interface runs; a subagent's starts the session it works for when that is not followed",
        "probe-opencode T3–T16 (session.status busy, in serve and the TUI); gaps oc8 (2.3.0 session.execution.started, in the service, in `--standalone` and in `run`; session.step.started first after the service restarted), oc9 (one plugin file whose `server` 1.18.25 runs and `setup` 2.3.0 runs), oc10 (the shipped plugin's busy from 1.18.25 `run`, on a new database and on oc9's migrated one)",
    ),
    hook(
        Signal::Prompted,
        "the plugin's busy",
        "probe-opencode T3–T16 (session.status busy, in serve and the TUI); gaps oc8 (2.3.0 session.execution.started), oc10 (1.18.25 `run`)",
    ),
    file(
        Signal::Prompted,
        "1.x: the session's latest message a user message, or an assistant message answering one, other than the turn last read; 2.x: a run started after the session last went idle, other than the run last read",
        "probe-opencode T2–T16; gaps oc8 (2.3.0 time_busy)",
    ),
    hook(
        Signal::PermissionAsked,
        "permission.asked, for the session or a subagent's session under it, which 2.x does not write either",
        "probe-opencode T3, T4, T7, T8, T12; gaps oc8 (2.3.0, a subagent's included)",
    ),
    file(
        Signal::QuestionAsked,
        "a question tool part running in the turn (1.x) or in the latest assistant message (2.x), in the session or a subagent's session under it",
        "probe-opencode T5, T6, T16; gaps oc8 (2.3.0 question tool)",
    ),
    hook(
        Signal::Answered,
        "permission.replied, for the session or a subagent's session under it",
        "probe-opencode T3, T7, T12; gaps oc8 (2.3.0)",
    ),
    file(
        Signal::Answered,
        "no question tool part running in the turn any more, while it runs",
        "probe-opencode T5; gaps oc8 (2.3.0 question answered)",
    ),
    file(
        Signal::TurnEnded,
        "1.x: the turn's latest assistant message completed, unless it is a step that finished on tool-calls or unknown with no error and no tool the user refused or dismissed; 2.x: the session went idle after its latest run started",
        "probe-opencode T2–T10, T15, T16; gaps oc8 (2.3.0 time_idle)",
    ),
    file(
        Signal::TurnEndedAsking,
        "the last text part, ending on a question mark, of that message, not synthetic (1.x), or of the run's last assistant message (2.x)",
        "probe-opencode T2; gaps oc8 (2.3.0 run)",
    ),
    file(
        Signal::TurnFailed,
        "1.x: that message carrying an error, a MessageAbortedError included; 2.x: the run's outcome failed, an error before its first step included",
        "probe-opencode T7–T9; gaps oc8 (2.3.0, an unknown model)",
    ),
    // No run of the probe caught the plugin's error for an unknown model.
    Declared {
        evidence: Evidence::Inferred,
        ..hook(
            Signal::TurnFailed,
            "1.x's session.error, for the session itself, while it is busy, other than a ContextOverflowError",
            "OpenCode 1.19.5's SessionPrompt.run, which sets the session busy before getModel publishes session.error UnknownError; probe-opencode T11 (a user message and no reply)",
        )
    },
    file(
        Signal::Interrupted,
        "1.x: that message's error a MessageAbortedError, or the message with neither finish nor error; 2.x: the run's outcome interrupted",
        "probe-opencode T7, T8, T10; gaps oc8 (2.3.0 escape during a shell)",
    ),
    hook(
        Signal::Interrupted,
        "1.x's session.error with a MessageAbortedError",
        "probe-opencode T7, T8",
    ),
    file(
        Signal::Backgrounded,
        "2.x: the run ended while a job of the session's is left, a shell moved to the background or a subagent, at work or not yet reported to it",
        "gaps oc8 (2.3.0 ctrl+b on a shell, a background shell, a task subagent)",
    ),
    file(
        Signal::Looked,
        "2.x: the session seen idle since it last went idle, as the terminal interface writes once it shows the session while its terminal has focus",
        "gaps oc8 (2.3.0 session.viewed)",
    )
    .untold("a 1.x session, whose rows record no viewing"),
    file(
        Signal::Ended,
        "the session's row deleted or archived",
        "probe-opencode T13; gaps oc8 (2.3.0 session delete)",
    ),
    hook(
        Signal::Ended,
        "the process the plugin's busy named gone",
        "probe-opencode T14, T17; gaps oc8 (2.3.0 terminal interface quit, service stopped)",
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

/// The `data` of a 1.x message row, with only the fields that carry state.
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

/// The `data` of a 1.x text part.
#[derive(Deserialize)]
struct Text {
    #[serde(default)]
    text: String,
    /// Set on text OpenCode wrote itself.
    #[serde(default)]
    synthetic: bool,
}

/// Which of OpenCode's two sets of tables a session is read from.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Schema {
    /// `session`, `message` and `part`, as 1.x writes them.
    V1,
    /// `session_v2` and `session_message`, as 2.x writes them.
    V2,
}

/// Where the database leaves a session's turn: in 1.x, the turn of one user
/// message; in 2.x, one run.
#[derive(Clone, Debug, PartialEq)]
enum Level {
    /// No turn yet.
    None,
    /// The turn `turn` runs, held up by a question when a question tool
    /// waits on the user in it.
    Open { turn: String, question: bool },
    /// The turn `turn` ended, as `end` names the ending, which tells
    /// `signals`. A subagent the turn left at work may still wait on a
    /// question, and 2.x writes when the user saw the session idle.
    Ended {
        turn: String,
        end: String,
        signals: BTreeSet<Signal>,
        question: bool,
        viewed: bool,
    },
}

impl Level {
    fn turn(&self) -> Option<TurnKey> {
        match self {
            Level::None => None,
            Level::Open { turn, .. } | Level::Ended { turn, .. } => Some(TurnKey(turn.clone())),
        }
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
            Level::Ended { signals, .. } => signals.clone(),
        }
    }

    /// Whether a question holds the turn up, in it or after it.
    fn question(&self) -> bool {
        match self {
            Level::None => false,
            Level::Open { question, .. } | Level::Ended { question, .. } => *question,
        }
    }

    fn viewed(&self) -> bool {
        matches!(self, Level::Ended { viewed: true, .. })
    }

    /// What the level says, record by record: how the turn stands; after an
    /// end, which lets go of any question, one a subagent still asks; and
    /// that the user saw it.
    fn told(&self) -> Vec<BTreeSet<Signal>> {
        let mut told = vec![self.signals()];
        if matches!(self, Level::Ended { .. }) && self.question() {
            told.push(BTreeSet::from([Signal::QuestionAsked]));
        }
        if self.viewed() {
            told.push(BTreeSet::from([Signal::Looked]));
        }
        told
    }
}

/// A session a busy from the plugin named, followed in the database.
struct Session {
    /// The OpenCode process that runs it.
    liveness: Liveness,
    /// Whether its row has been read: one never read is not yet written,
    /// not deleted.
    known: bool,
    /// While it holds, so does the level.
    stamp: Stamp,
    level: Level,
}

/// A session's row.
struct Row {
    parent: Option<String>,
    directory: String,
    archived: bool,
}

/// The sets of tables a database holds.
#[derive(Clone, Copy)]
struct Tables {
    v1: bool,
    v2: bool,
}

/// The database, opened read-only.
struct Database {
    connection: Connection,
    /// The file it was opened as, and its schema's version then: a database
    /// replaced in its place, or given other tables, as 2.x's first start
    /// gives 1.x's, is opened again.
    ino: u64,
    schema: i64,
    tables: Tables,
    /// `PRAGMA data_version` at the last read: it changes with every commit
    /// another connection makes.
    version: Option<i64>,
}

/// The statements the source runs, each checked against the schema when the
/// database is opened, so a schema without the columns read fails at once.
/// First 1.x's.
const ROW: &str = "select parent_id, directory, time_archived from session where id = ?1";
/// When the session's latest message was written, if it has a row.
const LAST: &str = "select (select max(time_created) from message where session_id = ?1) from session where id = ?1";
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
const V1: [&str; 7] = [ROW, LAST, STAMP, LATEST, REFUSED, TEXTS, QUESTION];

/// Then 2.x's. An assistant message's parts are in its row's `content`, or
/// in `session_part` with the experimental `part_rows`.
const ROW2: &str = "select parent_id, directory, time_archived from session_v2 where id = ?1";
/// When the session's latest run started, if it has a row.
const BUSY2: &str = "select time_busy from session_v2 where id = ?1";
/// Each session of the tree with its latest event's seq, which 2.x keeps
/// only in `event_sequence`, and each job the session left: a job's row
/// changes and goes with no event of its own.
const STAMP2: &str = "with recursive tree(id) as \
     (select ?1 union select session_v2.id from session_v2 join tree on session_v2.parent_id = tree.id) \
     select tree.id, (select seq from event_sequence where aggregate_id = tree.id) from tree \
     union all select 'job ' || notification_id, time_updated from job where session_id = ?1 order by 1";
const RUN2: &str = "select time_busy, time_idle, idle_outcome, time_viewed, \
     exists(select 1 from job where session_id = ?1) from session_v2 where id = ?1";
/// The text parts of the last assistant message written since `?2`.
const TEXTS2: &str = "with last as (select id, data from session_message where session_id = ?1 \
     and type = 'assistant' and time_created >= ?2 order by seq desc limit 1) \
     select key, json_extract(value, '$.text') from last, json_each(last.data, '$.content') \
     where json_extract(value, '$.type') = 'text' \
     union all select ordinal, json_extract(session_part.data, '$.text') from last \
     join session_part on session_part.message_id = last.id where session_part.type = 'text' order by 1";
/// A question tool running in the latest assistant message of the session
/// or of a subagent's session under it.
const QUESTION2: &str = "with recursive tree(id) as \
     (select ?1 union select session_v2.id from session_v2 join tree on session_v2.parent_id = tree.id), \
     latest(id, data) as (select id, data from session_message where type = 'assistant' \
     and seq = (select max(seq) from session_message as m where m.session_id = session_message.session_id \
     and m.type = 'assistant') and session_id in (select id from tree)), \
     tools(data) as (select value from latest, json_each(latest.data, '$.content') \
     union all select session_part.data from latest join session_part on session_part.message_id = latest.id) \
     select exists(select 1 from tools where json_extract(data, '$.type') = 'tool' \
     and json_extract(data, '$.name') = 'question' and json_extract(data, '$.state.status') = 'running')";
const V2: [&str; 6] = [ROW2, BUSY2, STAMP2, RUN2, TEXTS2, QUESTION2];

impl Database {
    fn open(path: &Path, ino: u64) -> Result<Database> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let connection = Connection::open_with_flags(path, flags)
            .with_context(|| format!("opening {}", path.display()))?;
        connection.busy_timeout(Duration::from_millis(250))?;
        let schema = schema_version(&connection)?;
        let has = |table: &str| -> Result<bool> {
            Ok(connection.query_row(
                "select exists(select 1 from sqlite_master where type = 'table' and name = ?1)",
                [table],
                |row| row.get(0),
            )?)
        };
        let tables = Tables {
            v1: has("session")?,
            v2: has("session_v2")?,
        };
        let unread = || {
            format!(
                "{} is not a database this version reads (OpenCode 1.18 to 2.3)",
                path.display()
            )
        };
        if !tables.v1 && !tables.v2 {
            anyhow::bail!(unread());
        }
        let v1 = V1.iter().filter(|_| tables.v1);
        let v2 = V2.iter().filter(|_| tables.v2);
        for statement in v1.chain(v2) {
            connection.prepare_cached(statement).with_context(unread)?;
        }
        Ok(Database {
            connection,
            ino,
            schema,
            tables,
            version: None,
        })
    }
}

fn schema_version(db: &Connection) -> Result<i64> {
    Ok(db.query_row("pragma schema_version", [], |row| row.get(0))?)
}

/// The tables that hold the session's latest run, or None if it is in
/// neither: 2.x's once it has a run later than 1.x's latest message.
fn schema(db: &Connection, tables: Tables, id: &str) -> Result<Option<Schema>> {
    let query = |statement: &str| -> Result<Option<Option<i64>>> {
        Ok(db
            .prepare_cached(statement)?
            .query_row([id], |row| row.get(0))
            .optional()?)
    };
    let v1 = if tables.v1 { query(LAST)? } else { None };
    let v2 = if tables.v2 { query(BUSY2)? } else { None };
    Ok(match (v1, v2) {
        (None, None) => None,
        (Some(_), None) => Some(Schema::V1),
        (None, Some(_)) => Some(Schema::V2),
        (Some(last), Some(busy)) => {
            let later = busy.is_some_and(|busy| last.is_none_or(|last| busy > last));
            Some(if later { Schema::V2 } else { Schema::V1 })
        }
    })
}

/// The session's row, or None if it is not in the database.
fn row(db: &Connection, schema: Schema, id: &str) -> Result<Option<Row>> {
    let statement = match schema {
        Schema::V1 => ROW,
        Schema::V2 => ROW2,
    };
    let row = db
        .prepare_cached(statement)?
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

/// A session and its subagents' sessions, each with its last event's seq,
/// and in 2.x the jobs it left with when each last changed.
type Stamp = Vec<(String, Option<i64>)>;

fn stamp(db: &Connection, schema: Schema, id: &str) -> Result<Stamp> {
    let statement = match schema {
        Schema::V1 => STAMP,
        Schema::V2 => STAMP2,
    };
    let mut statement = db.prepare_cached(statement)?;
    let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The roles and finishes of a 1.x message, and the outcomes of a 2.x run,
/// that OpenCode was seen to write, in the sessions of 1.18.25, 1.19.5 and
/// 2.3.0 the fixtures were taken from.
const ROLES: &[&str] = &["assistant", "user"];
const FINISHES: &[&str] = &["error", "stop", "tool-calls", "unknown"];
const OUTCOMES: &[&str] = &["failed", "interrupted", "succeeded"];

/// Where the session's rows leave its turn, counting in `strange` what
/// they say that OpenCode was not seen to write.
fn level(db: &Connection, schema: Schema, id: &str, strange: &mut Strange) -> Result<Level> {
    match schema {
        Schema::V1 => level1(db, id, strange),
        Schema::V2 => level2(db, id, strange),
    }
}

/// Where the session's 1.x messages leave its turn.
fn level1(db: &Connection, id: &str, strange: &mut Strange) -> Result<Level> {
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
    strange.kind(&data.role, ROLES);
    if let Some(finish) = &data.finish {
        strange.kind(finish, FINISHES);
    }
    if data.role == "user" {
        return Ok(Level::Open {
            turn: message,
            question: false,
        });
    }
    let turn = data
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
        return Ok(Level::Open { turn, question });
    }
    let mut signals = BTreeSet::from([Signal::TurnEnded]);
    if ends_on_question(&text1(db, &message)?) {
        signals.insert(Signal::TurnEndedAsking);
    }
    if let Some(error) = &data.error {
        signals.insert(Signal::TurnFailed);
        if error.name.as_deref() == Some("MessageAbortedError") {
            signals.insert(Signal::Interrupted);
        }
    }
    // Stopped while a request was retried: no finish, and no error. A step
    // that overflowed the context and is compacted automatically looks the
    // same until the compaction's message is written just after.
    if data.finish.is_none() && data.error.is_none() {
        signals.insert(Signal::Interrupted);
    }
    Ok(Level::Ended {
        turn,
        end: message,
        signals,
        question: false,
        viewed: false,
    })
}

/// The 1.x message's last text part that OpenCode did not write itself.
fn text1(db: &Connection, message: &str) -> Result<String> {
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

/// Where the session's 2.x row leaves its latest run. A run starts by
/// writing `time_busy` and ends by writing `time_idle`, never earlier, and
/// how it ended; a run cut short by OpenCode shutting down writes no end,
/// and is resumed when it starts again.
fn level2(db: &Connection, id: &str, strange: &mut Strange) -> Result<Level> {
    let (busy, idle, outcome, viewed, jobs) = db.prepare_cached(RUN2)?.query_row([id], |row| {
        Ok((
            row.get::<_, Option<i64>>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, bool>(4)?,
        ))
    })?;
    let Some(busy) = busy else {
        return Ok(Level::None);
    };
    let turn = busy.to_string();
    let question = db
        .prepare_cached(QUESTION2)?
        .query_row([id], |row| row.get(0))?;
    let Some(idle) = idle.filter(|&idle| idle >= busy) else {
        return Ok(Level::Open { turn, question });
    };
    if let Some(outcome) = &outcome {
        strange.kind(outcome, OUTCOMES);
    }
    let mut signals = BTreeSet::from([Signal::TurnEnded]);
    match outcome.as_deref() {
        Some("failed") => signals.insert(Signal::TurnFailed),
        Some("interrupted") => signals.insert(Signal::Interrupted),
        _ => false,
    };
    if jobs {
        signals.insert(Signal::Backgrounded);
    }
    if ends_on_question(&text2(db, id, busy)?) {
        signals.insert(Signal::TurnEndedAsking);
    }
    Ok(Level::Ended {
        turn,
        end: idle.to_string(),
        signals,
        question,
        viewed: viewed.is_some_and(|viewed| viewed >= idle),
    })
}

/// The last text part of the last assistant message the run since `busy`
/// wrote.
fn text2(db: &Connection, id: &str, busy: i64) -> Result<String> {
    let mut statement = db.prepare_cached(TEXTS2)?;
    let texts = statement.query_map(rusqlite::params![id, busy], |row| {
        row.get::<_, Option<String>>(1)
    })?;
    let mut last = String::new();
    for text in texts {
        last = text?.unwrap_or_default();
    }
    Ok(last)
}

/// The session a subagent's session works for, followed up to the session
/// no other started; the session itself when it is not a subagent's, or its
/// row is not there.
fn root(db: &Connection, tables: Tables, id: &str) -> Result<String> {
    let mut id = id.to_owned();
    // Subagents start subagents, though never this deep.
    for _ in 0..16 {
        let Some(schema) = schema(db, tables, &id)? else {
            break;
        };
        match row(db, schema, &id)?.and_then(|row| row.parent) {
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
    /// By id, so they are read in the same order every time.
    sessions: BTreeMap<String, Session>,
    /// What its reads could not make sense of, since it was last asked.
    strange: Strange,
}

impl OpenCode {
    pub fn new(home: &Path) -> OpenCode {
        let folder = home.join(".local/share/opencode");
        OpenCode {
            path: folder.join("opencode.db"),
            folder,
            db: None,
            sessions: BTreeMap::new(),
            strange: Strange::default(),
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
    fn open(&mut self) -> Result<Option<&Database>> {
        let ino = match fs::metadata(&self.path) {
            Ok(metadata) => metadata.ino(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.db = None;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let stale = match &self.db {
            None => true,
            Some(db) => db.ino != ino || schema_version(&db.connection)? != db.schema,
        };
        if stale {
            self.db = Some(Database::open(&self.path, ino)?);
        }
        Ok(self.db.as_ref())
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
        // One snapshot for every session, held only for the reads.
        let tx = db.connection.unchecked_transaction()?;
        let mut ended = Vec::new();
        let mut read = true;
        for (id, session) in &mut self.sessions {
            match read_session(&tx, db.tables, id, session, &mut self.strange, out) {
                Ok(true) => {}
                Ok(false) => ended.push(id.clone()),
                // The others are read all the same, and this one is read
                // again at the next change.
                Err(error) => {
                    eprintln!("agent-leds: reading OpenCode session {id}: {error:#}");
                    read = false;
                }
            }
        }
        tx.commit()?;
        if read {
            db.version = Some(version);
        }
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
            Ok(Some(db)) => root(&db.connection, db.tables, id).unwrap_or_else(|_| id.to_owned()),
            _ => id.to_owned(),
        }
    }

    /// The tables that hold the session's latest run, as the database says.
    fn schema(&mut self, id: &str) -> Option<Schema> {
        let db = self.open().ok().flatten()?;
        schema(&db.connection, db.tables, id).ok().flatten()
    }

    /// Where the database leaves a session now, if it has the session.
    fn look(&mut self, id: &str) -> Result<Option<(Row, Stamp, Level)>> {
        if self.open()?.is_none() {
            return Ok(None);
        }
        let db = self.db.as_ref().expect("opened");
        let strange = &mut self.strange;
        let tx = db.connection.unchecked_transaction()?;
        let read = match schema(&tx, db.tables, id)? {
            Some(schema) => match row(&tx, schema, id)? {
                Some(row) => Some((
                    row,
                    stamp(&tx, schema, id)?,
                    level(&tx, schema, id, strange)?,
                )),
                None => None,
            },
            None => None,
        };
        tx.commit()?;
        Ok(read)
    }

    /// Starts following a session from where the database leaves it now, or
    /// goes on following it if the same process still runs it. Says where it
    /// works: the database's folder for it, else `directory`. A session the
    /// database cannot be read for is followed from nothing, and read again
    /// at the next change, and is said with where it works.
    fn follow(
        &mut self,
        id: &str,
        liveness: Liveness,
        directory: String,
    ) -> (String, Option<Unread>) {
        let (read, unread) = match self.look(id) {
            Ok(read) => (read, None),
            Err(error) => (
                None,
                Some(Unread::new(&self.path, format!("session {id}: {error:#}"))),
            ),
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
            return (cwd, unread);
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
        (cwd, unread)
    }

    /// Follows a session a busy named, as `follow` does, and says where it
    /// works.
    fn bind(&mut self, id: &str, liveness: Liveness, directory: String) -> String {
        let (cwd, unread) = self.follow(id, liveness, directory);
        if let Some(unread) = unread {
            eprintln!(
                "agent-leds: reading {}: {}",
                unread.path.display(),
                unread.error
            );
        }
        cwd
    }

    /// Where the database leaves a session just followed, as the signals
    /// that take it there from Idle.
    fn current(&self, id: &str) -> Vec<Observation> {
        let session = &self.sessions[id];
        let mut told = Told::default();
        for signals in session.level.told() {
            told.tell(&signals);
        }
        // The shortest signals do not say what the user saw.
        let mut signals = told.current();
        if session.level.viewed() {
            signals.push(Signal::Looked);
        }
        let turn = session.level.turn();
        signals
            .into_iter()
            .map(|signal| {
                Observation::new(
                    Harness::OpenCode,
                    id,
                    Channel::File,
                    Event::one(signal),
                    turn.clone(),
                )
            })
            .collect()
    }

    fn turn(&self, id: &str) -> Option<TurnKey> {
        self.sessions
            .get(id)
            .and_then(|session| session.level.turn())
    }
}

/// Reads one followed session again, if its rows changed. False if it is
/// gone from the database or archived.
fn read_session(
    db: &Connection,
    tables: Tables,
    id: &str,
    session: &mut Session,
    strange: &mut Strange,
    out: &mut Vec<Observation>,
) -> Result<bool> {
    let schema = schema(db, tables, id)?;
    match schema
        .map(|schema| row(db, schema, id))
        .transpose()?
        .flatten()
    {
        None if !session.known => return Ok(true),
        None => return Ok(false),
        Some(row) if row.archived => return Ok(false),
        Some(_) => session.known = true,
    }
    let schema = schema.expect("a row");
    let stamp = stamp(db, schema, id)?;
    if stamp == session.stamp {
        return Ok(true);
    }
    let level = level(db, schema, id, strange)?;
    moved(id, &session.level, &level, out);
    session.stamp = stamp;
    session.level = level;
    Ok(true)
}

/// Tells `signals` of a session the database leaves at `level`. An answer
/// to a background subagent lets go of the wait that held off how the run
/// ended, which is then told again.
fn tell(
    id: &str,
    level: &Level,
    channel: Channel,
    signals: BTreeSet<Signal>,
    out: &mut Vec<Observation>,
) {
    let answered = signals.contains(&Signal::Answered);
    out.push(Observation::new(
        Harness::OpenCode,
        id,
        channel,
        Event::Signals(signals),
        level.turn(),
    ));
    if answered && level.signals().contains(&Signal::Backgrounded) {
        for signals in level.told() {
            out.push(Observation::new(
                Harness::OpenCode,
                id,
                Channel::File,
                Event::Signals(signals),
                level.turn(),
            ));
        }
    }
}

/// What a session's level moving from `from` to `to` tells.
fn moved(id: &str, from: &Level, to: &Level, out: &mut Vec<Observation>) {
    let mut say = |signals: BTreeSet<Signal>| tell(id, to, Channel::File, signals, out);
    let wait = match (from.question(), to.question()) {
        (false, true) => Some(Signal::QuestionAsked),
        (true, false) => Some(Signal::Answered),
        _ => None,
    };
    let new = to.turn().is_some() && to.turn() != from.turn();
    match (from, to) {
        (_, Level::None) => {}
        // A turn starts, or one that looked ended goes on.
        (_, Level::Open { .. }) if new || matches!(from, Level::Ended { .. }) => say(to.signals()),
        (_, Level::Open { .. }) => {
            if let Some(wait) = wait {
                say(BTreeSet::from([wait]));
            }
        }
        // The same end, written again, tells only what changed since.
        (
            Level::Ended {
                end: before,
                signals: was,
                ..
            },
            Level::Ended { end, signals, .. },
        ) if !new && before == end && was == signals => {
            if let Some(wait) = wait {
                say(BTreeSet::from([wait]));
            }
            if !from.viewed() && to.viewed() {
                say(BTreeSet::from([Signal::Looked]));
            }
        }
        // Each end is told once, however often its rows are written again.
        (_, Level::Ended { .. }) => {
            if new {
                say(BTreeSet::from([Signal::Prompted]));
            }
            for signals in to.told() {
                say(signals);
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
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Seed> {
        let mut seeded = Vec::new();
        let mut unread = Vec::new();
        // A database that cannot be read is said so even with nothing to seed.
        self.open()?;
        for (id, liveness) in bound {
            if !liveness::alive(k, liveness) {
                continue;
            }
            // Gone from the database, or archived; a row that cannot be read
            // is followed all the same.
            let gone = |db: &Database| -> Result<bool> {
                Ok(match schema(&db.connection, db.tables, id)? {
                    None => true,
                    Some(schema) => row(&db.connection, schema, id)?.is_none_or(|row| row.archived),
                })
            };
            if let Some(db) = self.open()?
                && gone(db).unwrap_or(false)
            {
                continue;
            }
            let (cwd, error) = self.follow(id, liveness.clone(), String::new());
            unread.extend(error);
            seeded.push(Observation::new(
                Harness::OpenCode,
                id,
                Channel::Hook,
                Event::Started(Start {
                    cwd,
                    liveness: liveness.clone(),
                }),
                self.turn(id),
            ));
            // What only the plugin told, a permission or an error before the
            // turn's loop, is not in the database.
            seeded.extend(self.current(id));
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
        Ok(Seed {
            observations: seeded,
            unread,
        })
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
            "busy" => {
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
                // A subagent at work says its session's process runs: a
                // session not followed, as when the service resumes a
                // subagent once it starts again, is followed from where the
                // database leaves it.
                if !own {
                    if !self.sessions.contains_key(&id) {
                        let cwd = self.bind(&id, liveness.clone(), directory);
                        out.push(Observation::new(
                            Harness::OpenCode,
                            &id,
                            Channel::Hook,
                            Event::Started(Start { cwd, liveness }),
                            self.turn(&id),
                        ));
                        let mut current = self.current(&id);
                        for o in &mut current {
                            o.quiet = true;
                        }
                        out.extend(current);
                    }
                    return out;
                }
                let followed = self
                    .sessions
                    .get(&id)
                    .is_some_and(|session| session.liveness == liveness);
                let cwd = self.bind(&id, liveness.clone(), directory);
                out.push(Observation::new(
                    Harness::OpenCode,
                    &id,
                    Channel::Hook,
                    Event::Started(Start { cwd, liveness }),
                    self.turn(&id),
                ));
                // 2.x writes a run's start before the plugin hears of it, so
                // a run that failed at once may have ended by now: as read
                // already, or, for a session not followed yet, told here.
                // 1.x's busy can come before the turn's prompt is written.
                let level = self.sessions[&id].level.clone();
                if let Level::Ended { turn, .. } = &level
                    && self.schema(&id) == Some(Schema::V2)
                {
                    if !followed {
                        let open = Level::Open {
                            turn: turn.clone(),
                            question: false,
                        };
                        let prompted = BTreeSet::from([Signal::Prompted]);
                        tell(&id, &open, Channel::Hook, prompted, &mut out);
                        moved(&id, &open, &level, &mut out);
                    }
                    return out;
                }
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
        let level = self.sessions.get(&id).map(|session| &session.level);
        tell(
            &id,
            level.unwrap_or(&Level::None),
            Channel::Hook,
            signals,
            &mut out,
        );
        out
    }

    fn behind(&self) -> bool {
        false
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
        let seeded = fixture::printed(opencode.seed(&liveness::System, &[]).unwrap());
        assert!(seeded.is_empty(), "no session is bound to a process");
        let db = opencode.open().unwrap().expect("a database");
        let table = if db.tables.v2 {
            "session_v2"
        } else {
            "session"
        };
        let mut roots = db
            .connection
            .prepare(&format!("select id, coalesce(title, '') from {table} where parent_id is null and time_archived is null order by time_updated desc"))
            .unwrap();
        let roots: Vec<(String, String)> = roots
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        println!("{} sessions", roots.len());
        for (id, title) in roots {
            let schema = schema(&db.connection, db.tables, &id).unwrap().unwrap();
            let level = level(&db.connection, schema, &id, &mut Strange::default()).unwrap();
            println!("{id}  {title:<40.40}  {schema:?} {level:?}");
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
    fn a_session_whose_reply_cannot_be_read_holds_up_no_other() {
        let replay = replay("a_session_whose_reply_cannot_be_read_holds_up_no_other");
        let phases: Vec<Phase> = replay.phases().into_values().collect();
        assert_eq!(phases, [Phase::Stopped, Phase::Stopped]);
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

    /// 2.x writes when the user saw a run's end, after a question and after
    /// a wake for a background shell alike.
    #[test]
    fn a_2x_run_s_end_the_user_saw_is_read() {
        for name in [
            "a_2x_question_holds_the_run_until_answered",
            "backgrounded.file",
        ] {
            assert_eq!(phase(&replay(name)), Phase::StoppedRead, "{name}");
        }
    }

    /// The phase each step of the replay left its session in, by the step.
    fn steps(replay: &Replay) -> BTreeMap<usize, Phase> {
        let mut phase = Phase::Gone;
        let mut held = Phase::Idle;
        let mut steps = BTreeMap::new();
        for (o, step) in &replay.yielded {
            if let Some(rule) = crate::fsm::next(phase, &o.event.signals()) {
                let to = rule.to.phase(held);
                if to == Phase::Blocked && phase != Phase::Blocked {
                    held = phase;
                }
                phase = to;
            }
            steps.insert(*step, phase);
        }
        steps
    }

    /// The subagent's permission held the session up after its run ended,
    /// so the answer leaves it working in the background, as the files say.
    #[test]
    fn an_answer_to_a_background_subagent_leaves_its_session_in_the_background() {
        let replay = replay("a_background_subagent_s_permission_holds_the_session");
        let steps = steps(&replay);
        let replied: Vec<Phase> = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::Answered))
            .map(|(_, step)| steps[step])
            .collect();
        assert_eq!(replied, [Phase::Background, Phase::Background]);
    }

    /// The service started again resumes a background subagent with no
    /// start of its own: the subagent's busy follows its session again, which
    /// its permission then holds up, and the answer leaves in the background.
    #[test]
    fn a_subagent_the_service_resumes_holds_its_session() {
        let replay = replay("a_subagent_the_service_resumes_holds_its_session");
        let steps = steps(&replay);
        let after = |signal: Signal| {
            let (_, step) = replay
                .yielded
                .iter()
                .rfind(|(o, _)| o.event.signals().contains(&signal))
                .unwrap();
            steps[step]
        };
        assert_eq!(after(Signal::PermissionAsked), Phase::Blocked);
        assert_eq!(after(Signal::Answered), Phase::Background);
    }

    #[test]
    fn a_2x_run_that_failed_before_the_plugin_heard_of_it_fails() {
        let replay = replay("a_2x_run_that_failed_before_the_plugin_heard_of_it_fails");
        assert_eq!(phase(&replay), Phase::Failed);
    }

    #[test]
    fn a_2x_run_the_user_interrupts_ends_interrupted() {
        let replay = replay("a_2x_run_the_user_interrupts_ends_interrupted");
        assert_eq!(phase(&replay), Phase::Idle);
    }

    #[test]
    fn a_deleted_2x_session_ends() {
        assert!(replay("a_deleted_2x_session_ends").phases().is_empty());
    }

    /// Each version's run ends once, read from the tables it wrote.
    #[test]
    fn a_migrated_session_is_read_from_the_tables_of_its_latest_run() {
        let replay = replay("a_migrated_session_is_read_from_the_tables_of_its_latest_run");
        let ends = replay
            .yielded
            .iter()
            .filter(|(o, _)| o.event.signals().contains(&Signal::TurnEnded))
            .count();
        assert_eq!(ends, 2);
        assert_eq!(phase(&replay), Phase::Stopped);
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
            format!("{error:#}")
                .contains("is not a database this version reads (OpenCode 1.18 to 2.3)"),
            "{error:#}"
        );
    }

    /// 2.x's own database has none of 1.x's tables, and a database 2.x
    /// migrated has both.
    #[test]
    fn a_database_of_either_version_or_both_is_read() {
        let schemas = |names: &[&str]| {
            let home = tempfile::tempdir().unwrap();
            let folder = home.path().join(".local/share/opencode");
            fs::create_dir_all(&folder).unwrap();
            let db = Connection::open(folder.join("opencode.db")).unwrap();
            for name in names {
                let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/opencode")
                    .join(name);
                db.execute_batch(&fs::read_to_string(path).unwrap())
                    .unwrap();
            }
            let seeded = OpenCode::new(home.path()).seed(&liveness::System, &[]);
            seeded
                .map(|seed| seed.observations.len())
                .map_err(|error| format!("{error:#}"))
        };
        assert_eq!(schemas(&["schema.sql"]), Ok(0));
        assert_eq!(schemas(&["schema-2.sql"]), Ok(0));
        assert_eq!(schemas(&["schema.sql", "schema-2.sql"]), Ok(0));
        let empty = schemas(&[]).unwrap_err();
        assert!(empty.ends_with("(OpenCode 1.18 to 2.3)"), "{empty}");
    }
}
