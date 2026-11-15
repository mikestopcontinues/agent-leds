//! Codex, from its own files. A process that runs a thread holds a lock on
//! `~/.codex/thread-writer-locks/<thread>.lock` for as long as it runs it,
//! and appends the thread's rollout,
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<thread>.jsonl`, a record at
//! a time: when each turn starts and how it ends, what it asks, and the
//! subagents it starts. Nothing is written while an approval waits, so the
//! `PermissionRequest` hook says that. Two of its databases say what a turn
//! end leads to: `goals_1.sqlite`, whether Codex goes on with the thread's
//! goal by itself or the goal waits on the user, and `queue_1.sqlite`,
//! whether a prompt the user queued starts next. A turn nobody typed, an
//! automation's heartbeat or a message from another thread, ends quietly
//! unless it has something for the user. The desktop app's global state
//! says which threads its sidebar shows unread, and so when the user looked.
//!
//! The files are undocumented. What they say at each moment of a session
//! was taken from Codex 0.153.2, and the desktop app's rollouts and
//! databases up to 0.162.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use super::tail::{self, MAX_LINE, Tail};
use super::{Event, Hook, Observation, Seed, Source, Start, Strange, Told, Unread};
use crate::coverage::{Declared, Evidence};
use crate::fsm::{self, Channel, Phase, Signal};
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
        untold: None,
    }
}

/// What Codex's files and its one hook tell, all of it but the queue and a
/// goal's moves after its turn seen in the probe's runs or in the rollouts
/// Codex had written before them.
pub const SIGNALS: &[Declared] = &[
    file(
        Signal::Started,
        "a thread's lock held, with a rollout whose session_meta is not a subagent's: neither its source nor its thread_source names one",
        "probe-codex runA, runB, runD; desktop rollouts 0.162",
    ),
    file(
        Signal::Prompted,
        "event_msg task_started, keyed by its turn_id, of any turn but those that tell Woke",
        "probe-codex runA, runB, runC",
    ),
    file(
        Signal::Woke,
        "task_started whose turn_trigger is automation_heartbeat_scheduled or app_tool_send_message, or capacity_retry_automatic after such a turn the user wrote nothing in",
        "desktop rollouts 0.162, 2026-10-12 to 2026-10-19: 211 heartbeat, 394 message and 5 retry turns",
    ),
    file(
        Signal::Retried,
        "task_started whose turn_trigger is capacity_retry_automatic after a turn that told Woke, which the user wrote nothing in",
        "desktop rollout 0.162, 2026-10-13T12:29: a message from another thread that failed at capacity, and the app's retry 11 s later",
    ),
    Declared::new(
        Signal::Prompted,
        Channel::File,
        Evidence::Inferred,
        "task_complete without an error while queue_1.sqlite holds a prompt queued for the thread, which Codex starts next",
        "the queue from Codex 0.162's schema and a desktop rollout's queued turn, 2026-10-11, its row unseen",
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
        "task_complete whose last_agent_message's last line ends in a question mark, or that holds a <proposed_plan> block, which no rollout has shown yet; or while an item of a request_user_input_async call has no answer since the last prompt the user typed, asked in the turn itself when nobody typed it, or the thread's goal in goals_1.sqlite went blocked, usage_limited or budget_limited at or after the turn's started_at",
        "probe-codex runA PROMPT6; desktop rollouts 0.162 and goals_1.sqlite, 2026-10-14 and 2026-10-15",
    ),
    file(
        Signal::TurnEndedQuietly,
        "task_complete of a turn that started with Woke and holds no user message but what Codex wraps around a turn, whose last_agent_message's last <heartbeat> block does not decide NOTIFY, and, for a message, whose goal in goals_1.sqlite was not complete or paused at or after the turn's started_at",
        "desktop rollouts 0.162, 2026-10-13: a heartbeat deciding DONT_NOTIFY",
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
        "task_complete while a subagent the turn started, or gave a followup_task, has not completed, or while the thread's goal in goals_1.sqlite is active with no continuation deferral, which Codex goes on with by itself, unless the last_agent_message decides NOTIFY",
        "probe-codex runA PROMPT5; desktop rollouts 2026-10-13 and 2026-10-15",
    ),
    file(
        Signal::BackgroundDone,
        "after the turn ended, the last such subagent's SubAgentActivity completed or interrupted, with no active goal left",
        "probe-codex runA PROMPT5; desktop rollout 2026-10-13",
    ),
    Declared::new(
        Signal::BackgroundDone,
        Channel::File,
        Evidence::Inferred,
        "after the turn ended, the thread's goal in goals_1.sqlite no longer active, or deferred, with no such subagent left",
        "the goal from Codex 0.162's statements on goals_1.sqlite, unseen",
    ),
    file(
        Signal::Ended,
        "the thread's lock deleted, or taken again by another process; or left unlocked, as a killed process leaves it",
        "probe-codex runA, runB (kill -9), runC",
    ),
    Declared::new(
        Signal::Looked,
        Channel::File,
        Evidence::Inferred,
        "a thread's id leaving a list under electron-thread-read-state-v1's unreadByIdentity in .codex-global-state.json, which the desktop app keeps by account and host, while the list stays; the file read whole, unchanged across the read",
        "the desktop app's bundle (app.asar, 2026-10-16) and the global state it left on 2026-10-18; no change seen",
    )
    .untold("a thread the desktop app does not show, as one a terminal runs"),
    Declared {
        signal: Signal::PermissionAsked,
        channel: Channel::Hook,
        evidence: Evidence::Observed,
        record: "PermissionRequest naming the turn the rollout has open",
        cite: "0.5.0 hooks during probe-codex runA PROMPT4 and codex2",
        untold: None,
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
    /// The async question items a user message answers.
    #[serde(skip)]
    answers: Vec<(String, usize)>,
    /// Whether a user message holds text the user wrote.
    #[serde(skip)]
    own: bool,
}

#[derive(Default, Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    turn_id: Option<String>,
    /// What started a turn, which the desktop app and Codex 0.163 write.
    turn_attribution: Option<Attribution>,
    last_agent_message: Option<String>,
    /// The Unix second a turn started, which its task_complete also says.
    started_at: Option<i64>,
    /// Why a turn was aborted: `interrupted`, or `replaced` by a new turn.
    reason: Option<String>,
    error: Option<IgnoredAny>,
    /// A function call's tool, with or without its namespace.
    name: Option<String>,
    call_id: Option<String>,
    /// A function call's arguments, as JSON in a string.
    arguments: Option<String>,
    /// A message's author: `user`, `assistant` or `developer`.
    role: Option<String>,
    item: Option<Item>,
}

#[derive(Default, Deserialize)]
struct Attribution {
    /// `composer`, `queue` or `remote_android` for a prompt the user typed;
    /// `goal`, `automation_heartbeat_scheduled`, `app_tool_send_message`,
    /// `send_user_message_async_question` or `capacity_retry_automatic` for
    /// a turn Codex started; `exec` for `codex exec`.
    turn_trigger: Option<String>,
}

/// The triggers of a turn the user started by typing a prompt: in the
/// composer, queued while a turn ran, or from the phone.
const TYPED: &[&str] = &["composer", "queue", "remote_android"];

/// The trigger of the desktop app's retry of a turn that failed at capacity.
const RETRY: &str = "capacity_retry_automatic";

/// What started a turn, as far as how it ends goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Origin {
    /// The user, or anything that is not one of the others: a typed prompt,
    /// an answer to an async question, a goal, `codex exec`, or a turn
    /// whose start names no trigger. It ends as a typed turn does.
    #[default]
    User,
    /// An automation's heartbeat (`automation_heartbeat_scheduled`).
    Heartbeat,
    /// A message another thread sent with `send_message_to_thread`
    /// (`app_tool_send_message`).
    Message,
}

impl Origin {
    /// The origin of a turn started by `trigger`, after a turn of `last`:
    /// the desktop app's retry after a capacity error is the turn it
    /// retries.
    fn of(trigger: Option<&str>, last: Origin) -> Origin {
        match trigger {
            Some("automation_heartbeat_scheduled") => Origin::Heartbeat,
            Some("app_tool_send_message") => Origin::Message,
            Some(RETRY) => last,
            _ => Origin::User,
        }
    }
}

/// What Codex writes as a user message around a turn, which the user did
/// not write, by how its text begins: the project's instructions, its own
/// context, the environment, a page the desktop app opened with it, a
/// heartbeat's prompt, plugins it suggests, and the answers to an async
/// question. These are every kind of user message the rollouts on the
/// author's Mac hold; any other text is the user's own.
const WRAPPERS: &[&str] = &[
    "# AGENTS.md",
    "<codex_internal_context",
    "<environment_context",
    "<external_codex_apps_open_page",
    "<heartbeat",
    "<recommended_plugins",
    "<send_user_message_question_reply",
];

/// The desktop app's question, which does not stop the turn.
const ASYNC_QUESTION: &str = "request_user_input_async";

/// The block a user message answers async question items in.
const ANSWER_OPEN: &str = "<send_user_message_question_reply>";
const ANSWER_CLOSE: &str = "</send_user_message_question_reply>";

/// The texts of a user message's content.
fn texts(line: &[u8]) -> Vec<String> {
    #[derive(Deserialize)]
    struct Message {
        payload: Content,
    }
    #[derive(Deserialize)]
    struct Content {
        content: Vec<Text>,
    }
    #[derive(Deserialize)]
    struct Text {
        text: Option<String>,
    }
    let Ok(message) = serde_json::from_slice::<Message>(line) else {
        return Vec::new();
    };
    let texts = message.payload.content.into_iter();
    texts.filter_map(|text| text.text).collect()
}

/// Whether a user message's texts hold one the user wrote, not only what
/// Codex wraps around a turn.
fn own(texts: &[String]) -> bool {
    texts
        .iter()
        .map(|text| text.trim_start())
        .any(|text| !text.is_empty() && !WRAPPERS.iter().any(|wrapper| text.starts_with(wrapper)))
}

/// The question items a user message answers, as the desktop app writes an
/// answer to `request_user_input_async`: a block of JSON naming each item as
/// a JSON array in a string, `[{"questionItemId":
/// "[\"request_user_input_async\",\"call_…\",0]", "answer": …}]`.
fn answers(texts: &[String]) -> Vec<(String, usize)> {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(rename = "questionItemId")]
        item: String,
    }
    let mut answered = Vec::new();
    for text in texts {
        let block = text
            .split_once(ANSWER_OPEN)
            .and_then(|(_, rest)| rest.split_once(ANSWER_CLOSE));
        let Some((block, _)) = block else {
            continue;
        };
        let Ok(items) = serde_json::from_str::<Vec<Answer>>(block.trim()) else {
            continue;
        };
        for Answer { item } in items {
            if let Ok((tool, call, index)) = serde_json::from_str::<(String, String, usize)>(&item)
                && tool == ASYNC_QUESTION
            {
                answered.push((call, index));
            }
        }
    }
    answered
}

/// How many question items an async question's arguments hold, each
/// answered on its own; one when they cannot be read.
fn items(arguments: Option<&str>) -> usize {
    #[derive(Deserialize)]
    struct Arguments {
        #[serde(default)]
        questions: Vec<IgnoredAny>,
    }
    let arguments = arguments.and_then(|text| serde_json::from_str::<Arguments>(text).ok());
    arguments.map_or(1, |arguments| arguments.questions.len().max(1))
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

/// The kinds of rollout record Codex was seen to write, in the rollouts of
/// 0.153 to 0.163 on the author's Mac, and those of an `event_msg` and a
/// `response_item`'s payload.
const KINDS: &[&str] = &[
    "compacted",
    "event_msg",
    "inter_agent_communication_metadata",
    "response_item",
    "session_meta",
    "token_usage_record",
    "turn_context",
    "world_state",
];
const EVENTS: &[&str] = &[
    "item_completed",
    "task_complete",
    "task_started",
    "thread_goal_updated",
    "thread_settings_applied",
    "token_count",
    "turn_aborted",
];
const ITEMS: &[&str] = &[
    "agent_message",
    "custom_tool_call",
    "custom_tool_call_output",
    "function_call",
    "function_call_output",
    "message",
    "reasoning",
];

impl Record {
    /// The record on a line, counted in `strange` if it is none, or of a
    /// kind or with a payload Codex was not seen to write.
    fn counted(line: &[u8], strange: &mut Strange) -> Option<Record> {
        let record = strange.parsed(Record::read(line))?;
        strange.kind(&record.kind, KINDS);
        let payloads = match record.kind.as_str() {
            "event_msg" => EVENTS,
            "response_item" => ITEMS,
            _ => return Some(record),
        };
        strange.kind(record.payload.kind.as_deref().unwrap_or_default(), payloads);
        Some(record)
    }

    fn read(line: &[u8]) -> Option<Record> {
        let mut record: Record = serde_json::from_slice(line).ok()?;
        if record.is("response_item", "message") && record.payload.role.as_deref() == Some("user") {
            let texts = texts(line);
            record.answers = answers(&texts);
            record.own = own(&texts);
        }
        Some(record)
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

    /// What started the turn, if the record starts one.
    fn trigger(&self) -> Option<&str> {
        let trigger = self.payload.turn_attribution.as_ref();
        trigger.and_then(|attribution| attribution.turn_trigger.as_deref())
    }

    /// Whether the record starts a turn the user typed the prompt of.
    fn starts_typed_turn(&self) -> bool {
        self.starts_turn()
            && self
                .trigger()
                .is_some_and(|trigger| TYPED.contains(&trigger))
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

/// Whether a final message decides that the user should be told of the
/// turn, as the desktop app reads it: the `<decision>` of the last
/// `<heartbeat>…</heartbeat>` block in it, ignoring case, is NOTIFY. The app
/// also reads a heartbeat's message that is only `DONT_NOTIFY`, or that in
/// a CDATA section, as not to, which is never NOTIFY either; and it takes a
/// heartbeat that completed with no message, or with no decision, as it
/// takes any turn without one.
fn notifies(message: &str) -> bool {
    let text = message.trim().to_ascii_lowercase();
    let (open, close) = ("<heartbeat>", "</heartbeat>");
    let mut last = None;
    let mut from = 0;
    while let Some(start) = text[from..].find(open).map(|at| from + at) {
        let Some(end) = text[start + open.len()..].find(close) else {
            break;
        };
        let end = start + open.len() + end + close.len();
        last = Some(&text[start..end]);
        from = end;
    }
    let Some(block) = last else {
        return false;
    };
    // The first `<decision>` that holds one, with whitespace around it.
    for (at, tag) in block.match_indices("<decision>") {
        let rest = block[at + tag.len()..].trim_start();
        for (decision, notify) in [("notify", true), ("dont_notify", false)] {
            if let Some(rest) = rest.strip_prefix(decision)
                && rest.trim_start().starts_with("</decision>")
            {
                return notify;
            }
        }
    }
    false
}

/// Whether a final message is a plan put to the user for review.
fn proposes_plan(message: &str) -> bool {
    let mut lines = message.lines().map(str::trim);
    lines.any(|line| line == "<proposed_plan>") && lines.any(|line| line == "</proposed_plan>")
}

/// Where a thread's goal stands in `goals_1.sqlite`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Goal {
    /// None, or one paused, complete, or active with its continuation
    /// deferred: nothing goes on by itself.
    #[default]
    Quiet,
    /// Active with no deferral: Codex starts the goal's next turn itself.
    Active,
    /// Blocked, or stopped at the usage or the token budget limit, since the
    /// turn began: it waits on the user. Codex leaves a goal so while the
    /// user goes on in the thread, and a goal that stopped before the turn
    /// is no news at its end.
    Waiting,
    /// Complete or paused since the turn began: nothing goes on by itself,
    /// and a message's turn that saw it stop does not end quietly.
    Left,
}

/// Codex's databases, each opened read-only for one read and closed again.
struct Databases {
    goals: PathBuf,
    queue: PathBuf,
}

const GOAL: &str = "select status, updated_at_ms, exists(select 1 from thread_goal_continuation_deferrals \
     where thread_goal_continuation_deferrals.thread_id = thread_goals.thread_id) \
     from thread_goals where thread_id = ?1";
const QUEUED: &str = "select exists(select 1 from queued_items where thread_id = ?1)";

impl Databases {
    fn new(codex: &Path) -> Databases {
        Databases {
            goals: codex.join("goals_1.sqlite"),
            queue: codex.join("queue_1.sqlite"),
        }
    }

    /// Whether a change to `path` can be a commit to the goals.
    fn goals_changed(&self, path: &Path) -> bool {
        let wal = wal(&self.goals);
        path == self.goals || path == wal
    }

    /// The goal of `thread` for a turn that began at the Unix second
    /// `since`; Quiet when the database cannot be read, which is said.
    fn goal(&self, thread: &str, since: Option<i64>) -> Goal {
        let read = query(&self.goals, GOAL, thread, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, bool>(2)?,
            ))
        });
        // The second a turn began holds its start, so a change in it counts.
        let lately = |updated: i64| since.is_some_and(|since| updated >= since * 1000);
        match read {
            Ok(Some((status, updated, deferred))) => match status.as_str() {
                "active" if !deferred => Goal::Active,
                "blocked" | "usage_limited" | "budget_limited" if lately(updated) => Goal::Waiting,
                "complete" | "paused" if lately(updated) => Goal::Left,
                _ => Goal::Quiet,
            },
            Ok(None) => Goal::Quiet,
            Err(error) => {
                eprintln!("agent-leds: reading {}: {error}", self.goals.display());
                Goal::Quiet
            }
        }
    }

    /// Whether the user queued a prompt for `thread`; not when the database
    /// cannot be read, which is said.
    fn queued(&self, thread: &str) -> bool {
        match query(&self.queue, QUEUED, thread, |row| row.get(0)) {
            Ok(queued) => queued.unwrap_or(false),
            Err(error) => {
                eprintln!("agent-leds: reading {}: {error}", self.queue.display());
                false
            }
        }
    }
}

/// The threads the desktop app's sidebar shows unread, in each list it
/// keeps, by account and host.
type Marks = BTreeMap<(String, String), BTreeSet<String>>;

/// The marks in the desktop app's global state, `.codex-global-state.json`:
/// `"electron-thread-read-state-v1": {"unreadByIdentity": {<account>:
/// {<host>: [<thread>, …]}}}`. The app marks a thread unread as a turn that
/// wants the user ends, and takes the mark off once the thread is shown in
/// a focused window. None when the file is not all JSON, as half written.
fn marks(bytes: &[u8]) -> Option<Marks> {
    #[derive(Deserialize)]
    struct State {
        #[serde(rename = "electron-thread-read-state-v1")]
        read: Option<ReadState>,
    }
    #[derive(Deserialize)]
    struct ReadState {
        #[serde(rename = "unreadByIdentity", default)]
        unread: HashMap<String, HashMap<String, Vec<String>>>,
    }
    let state: State = serde_json::from_slice(bytes).ok()?;
    let unread = state.read.map(|read| read.unread).unwrap_or_default();
    let lists = unread.into_iter().flat_map(|(account, hosts)| {
        let lists = hosts.into_iter();
        lists.map(move |(host, ids)| ((account.clone(), host), ids.into_iter().collect()))
    });
    Some(lists.collect())
}

/// The threads whose mark came off between two reads of the marks: those
/// the user looked at. Only a mark taken off a list that is still there
/// counts, so a list that goes, as when the user signs in to another
/// account, marks nothing read; and a mark there all along says nothing,
/// as the app leaves hundreds of them.
fn looked(before: &Marks, after: &Marks) -> BTreeSet<String> {
    let lists = before
        .iter()
        .filter_map(|(list, ids)| Some((ids, after.get(list)?)));
    lists
        .flat_map(|(before, after)| before.difference(after).cloned())
        .collect()
}

/// The write-ahead log of the database at `path`.
fn wal(path: &Path) -> PathBuf {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    PathBuf::from(wal)
}

/// One row of `sql`, about `thread`, from the database at `path` as it is
/// now; None when there is no database or no row. Codex keeps its databases
/// in WAL mode: a read-only connection reads what is committed to the log,
/// and takes no lock a writer waits on. A database no process has open has
/// no log, and a read-only connection cannot open it then, but its file
/// holds every commit, so it is read as immutable, also when the last
/// process closes it between the look and the read.
fn query<T>(
    path: &Path,
    sql: &str,
    thread: &str,
    row: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if wal(path).exists() {
        let read = Connection::open_with_flags(path, flags).and_then(|db| {
            db.busy_timeout(Duration::from_millis(250))?;
            db.query_row(sql, [thread], &row).optional()
        });
        if read.is_ok() || wal(path).exists() {
            return read;
        }
    }
    let path = path.to_string_lossy();
    let path = path
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let uri = format!("file:{path}?immutable=1");
    let db = Connection::open_with_flags(uri, flags | OpenFlags::SQLITE_OPEN_URI)?;
    db.query_row(sql, [thread], row).optional()
}

/// How a thread's last turn ended, from its task_complete until another
/// turn starts or it is interrupted.
struct Rest {
    /// What the rollout says: TurnEnded, and TurnEndedAsking or TurnFailed.
    told: BTreeSet<Signal>,
    /// The Unix second the turn began, as its task_complete says.
    started: Option<i64>,
    /// The goal, which changes after the turn ended.
    goal: Goal,
    /// What started the turn, the user if they wrote in it.
    origin: Origin,
    /// Whether its last message decides that the user is to be told.
    notify: bool,
}

impl Rest {
    /// Whether the turn has nothing for the user, as the author chose: a heartbeat
    /// unless it decides to notify, whether it decides not to or decides
    /// nothing; a message unless it decides to notify or its goal stopped.
    /// A failure, a question or work going on is told with it, and wins.
    fn quiet(&self) -> bool {
        match self.origin {
            Origin::User => false,
            Origin::Heartbeat => !self.notify,
            Origin::Message => !self.notify && self.goal != Goal::Left,
        }
    }
}

/// The phase a turn that ended on `signals` comes to.
fn rests(signals: &BTreeSet<Signal>) -> Phase {
    fsm::next(Phase::Working, signals).map_or(Phase::Working, |rule| rule.to.phase(Phase::Working))
}

/// What the files have said of one thread the user runs.
struct Thread {
    /// When the lock was last taken: a process taking the thread writes it.
    taken: SystemTime,
    /// The same, as a Unix second, to set beside the records' times.
    start: i64,
    rollout: PathBuf,
    tail: Option<Tail>,
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
    /// The async question items, by call and index, that have no answer
    /// since the last turn the user typed a prompt for.
    asks: BTreeSet<(String, usize)>,
    /// Of those, the ones the last turn asked, with the attempt a retry
    /// goes on from: all a turn nobody typed asks at its end.
    asked: BTreeSet<(String, usize)>,
    /// How the last turn ended, while no other has started.
    rest: Option<Rest>,
    /// What started the turn, and the last one once it ended.
    origin: Origin,
    /// Whether the user wrote a message of their own in the turn.
    own: bool,
    told: Told,
}

impl Thread {
    /// What the last turn's end comes to now, if it is the last thing the
    /// thread did: the rollout's end, with the work that goes on by itself
    /// and the goal that waits on the user.
    fn end(&self) -> Option<BTreeSet<Signal>> {
        let rest = self.rest.as_ref()?;
        let mut end = rest.told.clone();
        // A turn that decides to notify ends, though its goal goes on.
        if !self.children.is_empty() || (rest.goal == Goal::Active && !rest.notify) {
            end.insert(Signal::Backgrounded);
        }
        if rest.goal == Goal::Waiting {
            end.insert(Signal::TurnEndedAsking);
        }
        if rest.quiet() {
            end.insert(Signal::TurnEndedQuietly);
        }
        Some(end)
    }
}

/// Codex's threads, from its locks, rollouts and databases.
pub struct Codex {
    /// `~/.codex`, which holds the rest.
    dir: PathBuf,
    locks_dir: PathBuf,
    sessions_dir: PathBuf,
    databases: Databases,
    /// The desktop app's global state, `.codex-global-state.json`.
    state: PathBuf,
    /// Its marks as last read, which only a change to counts against.
    marks: Option<Marks>,
    threads: HashMap<String, Thread>,
    /// Threads that are not the user's, by their rollout, never read again.
    others: HashSet<String>,
    /// What its reads could not make sense of, since it was last asked.
    strange: Strange,
}

/// What a changed path is to the source: a thread's lock, its rollout, the
/// goals, or the desktop app's global state.
enum Changed {
    Lock(String),
    Rollout(String, PathBuf),
    Goals,
    State,
}

impl Codex {
    pub fn new(home: &Path) -> Codex {
        let dir = home.join(".codex");
        Codex {
            locks_dir: dir.join("thread-writer-locks"),
            sessions_dir: dir.join("sessions"),
            databases: Databases::new(&dir),
            state: dir.join(".codex-global-state.json"),
            marks: None,
            dir,
            threads: HashMap::new(),
            others: HashSet::new(),
            strange: Strange::default(),
        }
    }

    fn lock(&self, id: &str) -> PathBuf {
        self.locks_dir.join(format!("{id}.lock"))
    }

    fn changed_path(&self, path: &Path) -> Option<Changed> {
        if self.databases.goals_changed(path) {
            return Some(Changed::Goals);
        }
        if path == self.state {
            return Some(Changed::State);
        }
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
                tail: Some(tail),
                behind: false,
                open: false,
                turn: None,
                question: None,
                approval: false,
                children: HashSet::new(),
                followups: HashSet::new(),
                asks: BTreeSet::new(),
                asked: BTreeSet::new(),
                rest: None,
                origin: Origin::User,
                own: false,
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
        let read = tail::follow(
            &mut thread.tail,
            &thread.rollout,
            &mut thread.behind,
            &mut self.strange,
        );
        let Ok(lines) = read else {
            return;
        };
        for line in lines {
            if let Some(record) = Record::counted(&line, &mut self.strange) {
                says(id, thread, &self.databases, &record, out);
            }
        }
    }

    /// Tells what a change to the goals makes of each thread whose last
    /// turn has ended.
    fn regoal(&mut self, out: &mut Vec<Observation>) {
        for (id, thread) in &mut self.threads {
            let Some(rest) = &thread.rest else {
                continue;
            };
            let goal = self.databases.goal(id, rest.started);
            if goal != rest.goal {
                retell(id, thread, out, |thread| {
                    thread.rest.as_mut().expect("rested").goal = goal;
                });
            }
        }
    }

    /// The desktop app's marks as they are now: none when it has no global
    /// state, and None when the file changed while it was read, is half
    /// written, or cannot be read, which is said.
    fn read_marks(&self) -> Option<Marks> {
        match tail::stable(&self.state) {
            Ok(Some(bytes)) => marks(&bytes),
            Ok(None) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(Marks::new()),
            Err(error) => {
                eprintln!("agent-leds: reading {}: {error}", self.state.display());
                None
            }
        }
    }

    /// Tells that the user looked at each thread followed whose mark the
    /// desktop app took off since its marks were last read, and keeps them
    /// to read the next change against. A read that fails keeps the last.
    fn remark(&mut self, out: &mut Vec<Observation>) {
        let Some(marks) = self.read_marks() else {
            return;
        };
        if let Some(before) = &self.marks {
            for id in looked(before, &marks) {
                if let Some(thread) = self.threads.get_mut(&id) {
                    tell(&id, thread, Channel::File, [Signal::Looked], out);
                }
            }
        }
        self.marks = Some(marks);
    }

    /// Reads the rollout of a thread `look` started following back from its
    /// end, to where the last turn the user typed a prompt for began, which
    /// leaves no async question open from before it, or as far as
    /// `tail_back` reads, and tells what that says: at most 16 MiB of a
    /// resumed thread's rollout, which can be gigabytes long. What was
    /// written before the lock was taken is history of a process before this
    /// one, as after a resume or a crash, and says nothing.
    fn read_back(&mut self, id: &str, out: &mut Vec<Observation>) -> Result<(), Unread> {
        let thread = self.threads.get_mut(id).expect("started");
        let start = thread.start;
        // Back to the last typed turn's start, or to what an earlier
        // process wrote.
        let read = tail::tail_back(&thread.rollout, &mut self.strange, |line| {
            Record::read(line)
                .is_some_and(|record| record.starts_typed_turn() || record.before(start))
        });
        let lines = match read {
            Ok((lines, tail)) => {
                thread.tail = Some(tail);
                lines
            }
            // The thread is still followed, from what it writes next.
            Err(error) => return Err(Unread::new(&thread.rollout, error)),
        };
        let strange = &mut self.strange;
        let read =
            |line: &Vec<u8>| Record::counted(line, strange).filter(|record| !record.before(start));
        let records: Vec<Record> = lines.iter().filter_map(read).collect();
        // A turn's work with neither its start nor its end: the turn began
        // further back than `tail_back` reads.
        let bounded = |record: &Record| record.starts_turn() || record.ends_turn();
        if !records.iter().any(bounded) && records.iter().any(Record::is_work) {
            thread.open = true;
            tell(id, thread, Channel::File, [Signal::Prompted], out);
        }
        for record in &records {
            says(id, thread, &self.databases, record, out);
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
fn says(
    id: &str,
    thread: &mut Thread,
    databases: &Databases,
    record: &Record,
    out: &mut Vec<Observation>,
) {
    let payload = &record.payload;
    // A turn whose start was not read is named by its other records.
    if thread.open && thread.turn.is_none() {
        thread.turn = payload.turn_id.clone().map(TurnKey);
    }
    // A turn the user wrote in is theirs, whatever started it.
    if record.own && thread.open {
        thread.own = true;
    }
    if record.starts_turn() {
        // Also after a crash left a turn open: the new one is what runs.
        thread.open = true;
        thread.turn = payload.turn_id.clone().map(TurnKey);
        thread.question = None;
        thread.approval = false;
        thread.children.clear();
        thread.followups.clear();
        thread.rest = None;
        // A prompt the user typed moves on from the questions before it;
        // a turn Codex started does not.
        if record.starts_typed_turn() {
            thread.asks.clear();
        }
        if record.trigger() != Some(RETRY) {
            thread.asked.clear();
        }
        thread.origin = Origin::of(record.trigger(), thread.origin);
        thread.own = false;
        let started: &[Signal] = match thread.origin {
            Origin::User => &[Signal::Prompted],
            _ if record.trigger() == Some(RETRY) => &[Signal::Woke, Signal::Retried],
            Origin::Heartbeat | Origin::Message => &[Signal::Woke],
        };
        tell(id, thread, Channel::File, started.iter().copied(), out);
    } else if record.is("event_msg", "task_complete") {
        // No approval outlasts its turn, which would otherwise hold the
        // wait against an end told with work running on.
        if thread.approval {
            thread.approval = false;
            tell(id, thread, Channel::File, [Signal::Answered], out);
        }
        close(thread, payload);
        let mut told = BTreeSet::from([Signal::TurnEnded]);
        let message = payload.last_agent_message.as_deref().unwrap_or_default();
        // A turn nobody typed asks again only what it asked itself, so a
        // question left open from before ends it quietly, unread or read.
        let asks = match thread.origin {
            Origin::User => &thread.asks,
            Origin::Heartbeat | Origin::Message => &thread.asked,
        };
        if ends_on_question(message) || proposes_plan(message) || !asks.is_empty() {
            told.insert(Signal::TurnEndedAsking);
        }
        let failed = payload.error.is_some();
        if failed {
            told.insert(Signal::TurnFailed);
        }
        thread.rest = Some(Rest {
            told,
            started: payload.started_at,
            goal: databases.goal(id, payload.started_at),
            origin: thread.origin,
            notify: notifies(message),
        });
        // The prompt the user queued starts next, so the session works on;
        // whether Codex starts it after a failure is not known, and the
        // failure is told. The end stays, for what changes before it starts.
        if !failed && databases.queued(id) {
            tell(id, thread, Channel::File, [Signal::Prompted], out);
            return;
        }
        let end = thread.end().expect("rested");
        tell(id, thread, Channel::File, end, out);
    } else if record.is("event_msg", "turn_aborted") {
        // A turn replaced by another is followed by that turn's start,
        // which is all there is to tell.
        close(thread, payload);
        thread.rest = None;
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
            "completed" | "interrupted" => retell(id, thread, out, |thread| {
                thread.children.remove(child);
            }),
            _ => {}
        }
    } else if let Some(call) = record.calls(ASYNC_QUESTION) {
        let items = (0..items(payload.arguments.as_deref())).map(|index| (call.to_owned(), index));
        thread.asks.extend(items.clone());
        thread.asked.extend(items);
    } else if !record.answers.is_empty() {
        for answer in &record.answers {
            thread.asks.remove(answer);
            thread.asked.remove(answer);
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
    if thread.own {
        thread.origin = Origin::User;
    }
    if let Some(turn) = &payload.turn_id {
        thread.turn = Some(TurnKey(turn.clone()));
    }
    thread.question = None;
    thread.approval = false;
}

/// Makes `change` to a thread, and tells what it makes of the last turn's
/// end when no other turn has started: work going on by itself, or that
/// work finished, or a goal that waits on the user. Any other move waits
/// for the next turn.
fn retell(
    id: &str,
    thread: &mut Thread,
    out: &mut Vec<Observation>,
    change: impl FnOnce(&mut Thread),
) {
    let before = thread.end();
    change(thread);
    let (Some(before), Some(after)) = (before, thread.end()) else {
        return;
    };
    let (from, to) = (rests(&before), rests(&after));
    let signal = match to {
        _ if from == to => return,
        Phase::Asked => Signal::TurnEndedAsking,
        Phase::Background => Signal::Backgrounded,
        Phase::Stopped if from == Phase::Background => Signal::BackgroundDone,
        _ => return,
    };
    tell(id, thread, Channel::File, [signal], out);
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

    /// `~/.codex` itself, for the goals database beside the locks and
    /// rollouts, and none of the worktrees and builds Codex keeps under it.
    fn shallow_roots(&self) -> Vec<PathBuf> {
        vec![self.dir.clone()]
    }

    fn seed(&mut self, k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Seed> {
        // A mark there already says nothing: only one taken off counts.
        self.marks = self.read_marks();
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
            seeded.extend(thread.told.seeded(started, turn, |_| Channel::File));
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
        let mut goals = paths.is_none();
        let mut state = paths.is_none();
        match paths {
            Some(paths) => {
                for path in paths {
                    match self.changed_path(path) {
                        Some(Changed::Lock(id)) => add(id, None),
                        // A subagent's rollout, written as busily as the
                        // thread it works for, is not looked at again.
                        Some(Changed::Rollout(id, _)) if self.others.contains(&id) => {}
                        Some(Changed::Rollout(id, path)) => add(id, Some(path)),
                        Some(Changed::Goals) => goals = true,
                        Some(Changed::State) => state = true,
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
        // After the rollouts, whose turn ends read the goals as they are,
        // and which a look may come after.
        if goals {
            self.regoal(&mut out);
        }
        if state {
            self.remark(&mut out);
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

    fn behind(&self) -> Vec<PathBuf> {
        let behind = self.threads.values().filter(|thread| thread.behind);
        let mut paths: Vec<PathBuf> = behind.map(|thread| thread.rollout.clone()).collect();
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
        let marked = codex
            .marks
            .as_ref()
            .map(|marks| marks.values().map(BTreeSet::len).sum::<usize>());
        println!("{marked:?} threads the desktop app marks unread");
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

    /// The files alone say where each session stands, but for a look: only
    /// a mark coming off says that, so the files show the light unread, and
    /// the queue the daemon saved keeps it read.
    #[test]
    fn a_daemon_started_after_any_fixture_seeds_the_phases_it_reached() {
        let unread = |phase| match phase {
            Phase::StoppedRead => Phase::Stopped,
            Phase::AskedRead => Phase::Asked,
            phase => phase,
        };
        for name in fixture::names(Harness::Codex) {
            let replay = replay(&name);
            let where_ = |queue: &crate::queue::Queue,
                          read: &dyn Fn(Phase) -> Phase|
             -> Vec<(Key, Phase, Option<TurnKey>)> {
                let mut sessions: Vec<_> = queue
                    .sessions()
                    .iter()
                    .map(|session| {
                        (
                            session.key.clone(),
                            read(session.phase),
                            session.turn.clone(),
                        )
                    })
                    .collect();
                sessions.sort_by(|a, b| a.0.cmp(&b.0));
                sessions
            };
            assert_eq!(
                where_(&fixture::seed(&replay), &|phase| phase),
                where_(&replay.queue, &unread),
                "{name}"
            );
            assert_eq!(
                where_(&fixture::restart(&replay), &|phase| phase),
                where_(&replay.queue, &|phase| phase),
                "{name}"
            );
        }
    }

    #[test]
    fn a_thread_shown_in_the_desktop_app_is_read() {
        let replay = replay("looked.file");
        assert_eq!(phase(&replay), Phase::StoppedRead);
    }

    #[test]
    fn a_look_while_a_message_turn_runs_reads_the_stop_it_goes_back_to() {
        let replay = replay("a_look_while_a_message_turn_runs_reads_the_stop_it_goes_back_to");
        assert_eq!(phase(&replay), Phase::StoppedRead);
    }

    #[test]
    fn only_a_mark_taken_off_a_list_still_kept_is_a_look() {
        let list = |account: &str, host: &str, ids: &[&str]| {
            let ids = ids.iter().map(|id| id.to_string()).collect();
            ((account.to_owned(), host.to_owned()), ids)
        };
        let before = Marks::from([
            list("a", "local", &["t", "u"]),
            list("a", "durable", &["v"]),
        ]);
        let after = Marks::from([list("a", "local", &["u", "w"])]);
        assert_eq!(looked(&before, &after), BTreeSet::from(["t".to_owned()]));
        // Another account's lists, which a sign-in swaps for these.
        let other = Marks::from([list("b", "local", &[])]);
        assert!(looked(&before, &other).is_empty());
        assert!(looked(&Marks::new(), &after).is_empty());
    }

    #[test]
    fn the_marks_are_read_from_the_desktop_apps_global_state() {
        let state = br#"{"other": {"x": 1}, "electron-thread-read-state-v1": {"version": 1, "unreadByIdentity": {"a": {"local:1": ["t"], "durable:2": []}}, "legacyMigration": {}}}"#;
        let read = marks(state).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(
            read[&("a".to_owned(), "local:1".to_owned())],
            BTreeSet::from(["t".to_owned()])
        );
        assert_eq!(marks(br#"{"other": 1}"#), Some(Marks::new()));
        assert_eq!(marks(br#"{"electron-thread-read-state-v1": {"#), None);
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
        assert!(source.behind().is_empty());
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
    fn a_goal_goes_on_by_itself_until_it_is_blocked() {
        let replay = replay("a_goal_goes_on_by_itself_until_it_is_blocked");
        assert_eq!(phase(&replay), Phase::Asked);
    }

    #[test]
    fn a_goal_blocked_before_a_turn_does_not_ask_at_its_end() {
        let replay = replay("a_goal_blocked_before_a_turn_does_not_ask_at_its_end");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_goal_that_moves_after_its_turn_ended_moves_the_light() {
        let replay = replay("background_done.file.inferred");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn an_async_question_unanswered_at_the_turn_end_asks() {
        let replay = replay("an_async_question_unanswered_at_the_turn_end_asks");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_typed_prompt_lets_an_unanswered_async_question_go() {
        let replay = replay("a_typed_prompt_lets_an_unanswered_async_question_go");
        assert_eq!(phase(&replay), Phase::Stopped);
    }

    #[test]
    fn a_prompt_queued_when_the_turn_ends_keeps_the_session_working() {
        let replay = replay("prompted.file.inferred");
        assert_eq!(phase(&replay), Phase::Working);
        assert!(
            told(&replay)
                .iter()
                .all(|told| !told.contains(&Signal::TurnEnded))
        );
    }

    /// A failed turn is told even with a prompt queued, since whether Codex
    /// starts the prompt after it is not known.
    #[test]
    fn a_turn_that_fails_with_a_prompt_queued_fails() {
        let replay = replay("a_turn_that_fails_with_a_prompt_queued_fails");
        assert_eq!(phase(&replay), Phase::Failed);
    }

    /// A goals database as Codex leaves it, with `status` for the thread.
    fn goals(dir: &Path, status: &str) -> rusqlite::Connection {
        let db = rusqlite::Connection::open(dir.join("goals_1.sqlite")).unwrap();
        db.pragma_update(None, "journal_mode", "wal").unwrap();
        db.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        let schema = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex/goals.sql");
        db.execute_batch(&fs::read_to_string(schema).unwrap())
            .unwrap();
        db.execute(
            "insert into thread_goals (thread_id, goal_id, objective, status, created_at_ms, updated_at_ms) values ('t', 'g', 'o', ?1, 0, 0)",
            [status],
        )
        .unwrap();
        db
    }

    #[test]
    fn a_goal_committed_to_the_log_and_not_checkpointed_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let writer = goals(dir.path(), "active");
        let databases = Databases::new(dir.path());
        assert_eq!(databases.goal("t", Some(0)), Goal::Active);
        writer
            .execute("update thread_goals set status = 'blocked'", [])
            .unwrap();
        assert!(fs::metadata(wal(&databases.goals)).unwrap().len() > 0);
        assert_eq!(databases.goal("t", Some(0)), Goal::Waiting);
        // A reader takes no lock the writer waits on.
        writer
            .execute(
                "insert into thread_goal_continuation_deferrals values ('t')",
                [],
            )
            .unwrap();
        assert_eq!(databases.goal("u", Some(0)), Goal::Quiet);
    }

    #[test]
    fn a_goals_database_no_process_has_open_is_read_from_its_file() {
        let dir = tempfile::tempdir().unwrap();
        drop(goals(dir.path(), "usage_limited"));
        let databases = Databases::new(dir.path());
        // As a Codex that links its own SQLite leaves it on quitting.
        for file in [wal(&databases.goals), dir.path().join("goals_1.sqlite-shm")] {
            let _ = fs::remove_file(file);
        }
        assert_eq!(databases.goal("t", Some(0)), Goal::Waiting);
        assert!(!wal(&databases.goals).exists());
        assert!(!databases.queued("t"));
    }

    #[test]
    fn a_goal_that_stopped_before_the_turn_began_does_not_wait_on_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let writer = goals(dir.path(), "blocked");
        writer
            .execute("update thread_goals set updated_at_ms = 41999", [])
            .unwrap();
        let databases = Databases::new(dir.path());
        assert_eq!(databases.goal("t", Some(42)), Goal::Quiet);
        assert_eq!(databases.goal("t", None), Goal::Quiet);
        // Within the second the turn began, or after it.
        writer
            .execute("update thread_goals set updated_at_ms = 42000", [])
            .unwrap();
        assert_eq!(databases.goal("t", Some(42)), Goal::Waiting);
        // An active goal goes on whenever it was set so.
        writer
            .execute("update thread_goals set status = 'active'", [])
            .unwrap();
        assert_eq!(databases.goal("t", Some(43)), Goal::Active);
    }

    #[test]
    fn an_active_goal_whose_continuation_is_deferred_goes_on_by_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let writer = goals(dir.path(), "active");
        writer
            .execute(
                "insert into thread_goal_continuation_deferrals values ('t')",
                [],
            )
            .unwrap();
        assert_eq!(Databases::new(dir.path()).goal("t", Some(0)), Goal::Quiet);
    }

    #[test]
    fn an_answer_names_each_async_question_item_it_answers() {
        let text = "<send_user_message_question_reply>\n[{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call_e2\\\",0]\",\"answer\":\"a\"},{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call_e2\\\",1]\",\"answer\":\"b\"}]\n</send_user_message_question_reply>\n";
        let line = serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}
        });
        let record = Record::read(line.to_string().as_bytes()).unwrap();
        assert_eq!(
            record.answers,
            [("call_e2".to_owned(), 0), ("call_e2".to_owned(), 1)]
        );
        assert_eq!(items(Some("{\"questions\":[{},{}]}")), 2);
        assert_eq!(items(Some("not json")), 1);
    }

    #[test]
    fn a_turn_nobody_typed_that_decides_to_notify_stops() {
        assert_eq!(phase(&replay("woke.file")), Phase::Stopped);
    }

    #[test]
    fn turns_nobody_typed_that_have_nothing_for_the_user_leave_the_session_dark() {
        let replay = replay("turn_ended_quietly.file");
        assert_eq!(phase(&replay), Phase::Idle);
        assert!(
            replay
                .yielded
                .iter()
                .all(|(o, _)| !o.event.signals().contains(&Signal::Prompted))
        );
    }

    /// The typed turn's stop is announced, and so is the end of the message
    /// the user wrote into; the quiet end goes back to that stop, unread.
    #[test]
    fn a_message_turn_the_user_wrote_in_ends_as_their_own_do() {
        let replay = replay("a_message_turn_the_user_wrote_in_ends_as_their_own_do");
        assert_eq!(phase(&replay), Phase::Stopped);
        let mut queue = crate::queue::Queue::default();
        let changes: Vec<crate::queue::Change> = replay
            .yielded
            .iter()
            .map(|(o, step)| queue.apply(o, *step as u64))
            .collect();
        use crate::queue::Change::*;
        use crate::state::State;
        assert_eq!(
            changes,
            [
                Saved,
                Settle,
                Announce(State::Stopped),
                Settle,
                Announce(State::Stopped),
                Settle,
                Settle,
            ]
        );
    }

    #[test]
    fn a_heartbeat_that_fails_shows_the_error() {
        let replay = replay("a_heartbeat_that_fails_shows_the_error");
        assert_eq!(phase(&replay), Phase::Failed);
    }

    /// The error is announced; the retry, which ends with nothing for the
    /// user, goes back past it to the dark session the failed turn found.
    #[test]
    fn a_retry_after_a_capacity_error_is_the_turn_it_retries() {
        let replay = replay("retried.file");
        assert_eq!(phase(&replay), Phase::Idle);
        let mut queue = crate::queue::Queue::default();
        let changes: Vec<crate::queue::Change> = replay
            .yielded
            .iter()
            .map(|(o, step)| queue.apply(o, *step as u64))
            .collect();
        use crate::queue::Change::*;
        use crate::state::State;
        assert_eq!(
            changes,
            [Saved, Settle, Announce(State::Error), Settle, Settle]
        );
    }

    #[test]
    fn a_goal_turn_that_decides_to_notify_ends() {
        let replay = replay("a_goal_turn_that_decides_to_notify_ends");
        assert_eq!(phase(&replay), Phase::Background);
    }

    #[test]
    fn the_last_heartbeat_block_decides_as_the_desktop_app_reads_it() {
        let block = |decision: &str| {
            format!(
                "<heartbeat>\n  <automation_id>a</automation_id>\n  <decision>{decision}</decision>\n  <message>m</message>\n</heartbeat>"
            )
        };
        assert!(notifies(&block("NOTIFY")));
        assert!(notifies(&format!("Text before.\n\n{}", block("notify"))));
        assert!(notifies(&block("\n NOTIFY \n")));
        assert!(!notifies(&block("DONT_NOTIFY")));
        assert!(!notifies(&format!(
            "{}\n{}",
            block("NOTIFY"),
            block("DONT_NOTIFY")
        )));
        assert!(notifies(&format!(
            "{}\n{}",
            block("DONT_NOTIFY"),
            block("NOTIFY")
        )));
        // A decision outside a block, or no block, decides nothing.
        assert!(!notifies("<decision>NOTIFY</decision>"));
        assert!(!notifies("<heartbeat><decision>NOTIFY</decision>"));
        assert!(!notifies(
            "<heartbeat><decision>MAYBE</decision></heartbeat>"
        ));
        // The forms only a heartbeat's message is read in say not to.
        assert!(!notifies("DONT_NOTIFY"));
        assert!(!notifies("<![CDATA[ DONT_NOTIFY ]]>"));
        assert!(!notifies(""));
    }

    #[test]
    fn only_text_the_user_wrote_makes_a_turn_theirs() {
        let own = |texts: &[&str]| own(&texts.iter().map(|t| t.to_string()).collect::<Vec<_>>());
        assert!(own(&["Is the deploy done?"]));
        assert!(own(&[
            "<external_codex_apps_open_page>p</external_codex_apps_open_page>",
            "Why?"
        ]));
        for wrapper in WRAPPERS {
            assert!(!own(&[&format!("  {wrapper} rest")]), "{wrapper}");
        }
        assert!(!own(&["", "  \n"]));
    }

    #[test]
    fn a_message_turn_ends_quietly_unless_it_notifies_or_its_goal_stops() {
        let rest = |origin, notify, goal| Rest {
            told: BTreeSet::from([Signal::TurnEnded]),
            started: Some(0),
            goal,
            origin,
            notify,
        };
        assert!(rest(Origin::Message, false, Goal::Quiet).quiet());
        assert!(rest(Origin::Message, false, Goal::Waiting).quiet());
        assert!(!rest(Origin::Message, false, Goal::Left).quiet());
        assert!(!rest(Origin::Message, true, Goal::Quiet).quiet());
        assert!(rest(Origin::Heartbeat, false, Goal::Left).quiet());
        assert!(!rest(Origin::Heartbeat, true, Goal::Quiet).quiet());
        assert!(!rest(Origin::User, false, Goal::Quiet).quiet());
    }

    #[test]
    fn a_goal_completed_or_paused_since_the_turn_began_has_left() {
        let dir = tempfile::tempdir().unwrap();
        let writer = goals(dir.path(), "complete");
        writer
            .execute("update thread_goals set updated_at_ms = 42000", [])
            .unwrap();
        let databases = Databases::new(dir.path());
        assert_eq!(databases.goal("t", Some(42)), Goal::Left);
        assert_eq!(databases.goal("t", Some(43)), Goal::Quiet);
        writer
            .execute("update thread_goals set status = 'paused'", [])
            .unwrap();
        assert_eq!(databases.goal("t", Some(42)), Goal::Left);
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
        assert!(!source.behind().is_empty());
        // The lock changing first leaves what is unread to read.
        source.changed(&replay.kernel, Some(&[lock]));
        assert!(source.behind().is_empty());
    }
}
