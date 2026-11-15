//! Replays a fixture, a session as a harness wrote it, against the
//! harness's source on a home directory of its own.
//!
//! A fixture is `tests/fixtures/<harness>/<name>.jsonl`. Its first line says
//! where it comes from, `{"cite": "probe-claude cc1", "evidence": "observed"}`,
//! and each line after is one step:
//!
//! - `{"append": "<path under home>", "line": ...}` adds a line to a file,
//!   `"times": <n>` times over if it says so, and `"lines": [...]` in place
//!   of `"line"` adds several in one write, as a burst the watcher reports
//!   once;
//! - `{"write": "<path>", "text": ...}` rewrites a file in place;
//! - `{"delete": "<path>"}`;
//! - `{"sql": "<path>", "run": "<statements>"}` runs SQL on a database in
//!   WAL mode, as another process committing to it, and `"from": "<file>"`
//!   in place of `run` runs a file in the harness's fixture folder, such as
//!   the schema of the harness's database;
//! - `{"hook": <payload>, "ancestry": [<proc>, ...]}` delivers a hook, with
//!   each string in the payload that starts `~/` taken as a path under home;
//! - `{"lock": "<path>", "at": "<ISO 8601 UTC time>"}` is a process opening
//!   a file, creating it if it is not there, and holding a lock on it: the
//!   file's modification time is `at`, or now, and the fake kernel says it
//!   is locked;
//! - `{"alive": <pid>, "started_us": <n>}` and `{"exit": <pid>}` start and
//!   end a process, and `{"unlock": "<path>"}` drops a lock as a killed
//!   process does, for the fake kernel the source and the sweep ask;
//! - `{"expect": [["<signal>", "<channel>"], ...]}` says what every step
//!   since the last `expect` yielded, in order, one observation each; an
//!   observation of several signals is `[["<signal>", ...], "<channel>"]`.
//!   With `"restart":
//!   "<phase>"` that a daemon started after one of those steps shows that
//!   phase instead: a moment its files cannot tell from another, which a
//!   daemon started any time before may show otherwise too.
//!
//! A line or text that is not a string is written as compact JSON. After
//! each step the replay also holds the source to four properties: a rescan
//! yields nothing, a daemon started then shows the phases the observations
//! reached, that daemon goes on through the steps after to show what the
//! replay showed after each, and reading the file steps two at a time ends
//! in the same phases.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::Value;

use super::{Hook, Observation, Source, all};
use crate::coverage::Evidence;
use crate::fsm::{self, Channel, Phase, Signal};
use crate::harness::{self, Harness};
use crate::liveness::{self, Kernel, Liveness, Proc};
use crate::queue::{Key, Queue};

/// Processes and locks as a test says they are.
#[derive(Default)]
pub struct FakeKernel {
    starts: HashMap<i32, u64>,
    locks: HashSet<PathBuf>,
}

impl Kernel for FakeKernel {
    fn started_us(&self, pid: i32) -> Option<u64> {
        self.starts.get(&pid).copied()
    }

    fn locked(&self, path: &Path) -> io::Result<bool> {
        fs::metadata(path)?;
        Ok(self.locks.contains(path))
    }
}

/// Where a fixture comes from, and its steps.
pub struct Fixture {
    pub cite: String,
    pub evidence: Evidence,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Header {
    cite: String,
    evidence: Evidence,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Step {
    Append {
        append: String,
        line: Option<Value>,
        #[serde(default)]
        lines: Vec<Value>,
        times: Option<usize>,
    },
    Write {
        write: String,
        text: Value,
    },
    Delete {
        delete: String,
    },
    Sql {
        sql: String,
        run: Option<String>,
        from: Option<String>,
    },
    Hook {
        hook: Value,
        #[serde(default)]
        ancestry: Vec<Proc>,
    },
    Alive {
        alive: i32,
        started_us: u64,
    },
    Exit {
        exit: i32,
    },
    Lock {
        lock: String,
        at: Option<String>,
    },
    Unlock {
        unlock: String,
    },
    Expect {
        expect: Vec<(Told, Channel)>,
        restart: Option<Phase>,
    },
}

/// What one observation an `expect` names told: one signal, or several.
#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Told {
    One(Signal),
    Several(BTreeSet<Signal>),
}

impl Told {
    fn signals(&self) -> BTreeSet<Signal> {
        match self {
            Told::One(signal) => BTreeSet::from([*signal]),
            Told::Several(signals) => signals.clone(),
        }
    }
}

impl Step {
    /// The file a step writes, if it is a file step.
    fn file(&self) -> Option<&str> {
        match self {
            Step::Append { append: path, .. }
            | Step::Write { write: path, .. }
            | Step::Delete { delete: path }
            | Step::Lock { lock: path, .. }
            | Step::Sql { sql: path, .. } => Some(path),
            _ => None,
        }
    }
}

/// The folder a harness's fixtures are in.
fn folder(harness: Harness) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(harness.name())
}

/// The names of every fixture of a harness.
pub fn names(harness: Harness) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(folder(harness))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some(name.strip_suffix(".jsonl")?.to_owned())
        })
        .collect();
    names.sort();
    names
}

pub fn load(harness: Harness, name: &str) -> Fixture {
    let path = folder(harness).join(format!("{name}.jsonl"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header: Header = serde_json::from_str(lines.next().expect("a header"))
        .unwrap_or_else(|e| panic!("{name}: the header: {e}"));
    let steps = lines
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{name}: step {}: {e}: {line}", index + 1))
        })
        .collect();
    Fixture {
        cite: header.cite,
        evidence: header.evidence,
        steps,
    }
}

/// A home directory with a harness's source on it, and what it has yielded.
pub struct Replay {
    pub harness: Harness,
    pub name: String,
    /// Each observation with the step that yielded it, counted from 1.
    pub yielded: Vec<(Observation, usize)>,
    pub queue: Queue,
    pub home: tempfile::TempDir,
    pub kernel: FakeKernel,
}

impl Replay {
    fn new(harness: Harness, name: &str) -> Replay {
        Replay {
            harness,
            name: name.to_owned(),
            yielded: Vec::new(),
            queue: Queue::default(),
            home: tempfile::tempdir().unwrap(),
            kernel: FakeKernel::default(),
        }
    }

    /// The harness's source, as the daemon makes it, on this home.
    pub fn source(&self) -> Box<dyn Source> {
        let home = self.home.path();
        all(home, &home.join(".junie"))
            .into_iter()
            .find(|source| source.harness() == self.harness)
            .expect("every harness has a source")
    }

    /// The phase of each session the queue tracks.
    pub fn phases(&self) -> BTreeMap<Key, Phase> {
        phases(&self.queue)
    }

    /// A hook step as the harness delivers it, naming paths in this home.
    fn hook(&self, payload: &Value, ancestry: &[Proc]) -> Hook {
        Hook {
            harness: self.harness,
            payload: at_home(payload, self.home.path()),
            ancestry: ancestry.to_vec(),
        }
    }

    /// Makes a file step's change on disk, and to the fake kernel for a lock.
    fn write(&mut self, step: &Step) -> PathBuf {
        let text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            value => value.to_string(),
        };
        let path = self.home.path().join(step.file().expect("a file step"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        match step {
            Step::Append {
                line, lines, times, ..
            } => {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                let mut burst = String::new();
                for line in line.iter().chain(lines) {
                    burst.push_str(&text(line));
                    burst.push('\n');
                }
                file.write_all(burst.repeat(times.unwrap_or(1)).as_bytes())
                    .unwrap();
            }
            Step::Write { text: value, .. } => fs::write(&path, text(value)).unwrap(),
            Step::Delete { .. } => fs::remove_file(&path).unwrap(),
            Step::Lock { at, .. } => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                let at = at.as_deref().map_or_else(SystemTime::now, |at| {
                    let second = liveness::iso_second(at).expect("an ISO 8601 time");
                    UNIX_EPOCH + Duration::from_secs(second as u64)
                });
                file.set_modified(at).unwrap();
                self.kernel.locks.insert(path.clone());
            }
            Step::Sql { run, from, .. } => {
                let statements = match (run, from) {
                    (Some(run), None) => run.clone(),
                    (None, Some(from)) => fs::read_to_string(folder(self.harness).join(from))
                        .unwrap_or_else(|e| panic!("{}: {from}: {e}", self.name)),
                    _ => panic!("{}: a sql step has run or from, not both", self.name),
                };
                let db = rusqlite::Connection::open(&path).unwrap();
                db.pragma_update(None, "journal_mode", "wal").unwrap();
                db.execute_batch(&statements)
                    .unwrap_or_else(|e| panic!("{}: {e}: {statements}", self.name));
            }
            _ => unreachable!(),
        }
        path
    }

    /// Changes the fake kernel, which only the sweep notices.
    fn kernel(&mut self, step: &Step) {
        let home = self.home.path();
        match step {
            Step::Alive { alive, started_us } => {
                self.kernel.starts.insert(*alive, *started_us);
            }
            Step::Exit { exit } => {
                self.kernel.starts.remove(exit);
            }
            Step::Unlock { unlock } => {
                self.kernel.locks.remove(&home.join(unlock));
            }
            _ => unreachable!(),
        }
    }

    /// Applies what one step yielded, after checking that every signal is
    /// declared and that none told together overlap.
    fn take(
        &mut self,
        observations: Vec<Observation>,
        step: usize,
    ) -> Vec<(BTreeSet<Signal>, Channel)> {
        let mut took = Vec::new();
        for o in observations {
            let signals = o.event.signals();
            for &signal in &signals {
                assert!(
                    harness::declares(self.harness, signal, o.channel),
                    "{}: step {step} yielded {signal:?} on {:?}, which {} does not declare",
                    self.name,
                    o.channel,
                    self.harness.title()
                );
            }
            let from = self.queue.phase(&o.key);
            assert!(
                !fsm::overlaps(from, &signals),
                "{}: step {step} yielded {signals:?} from {from:?}, which overlap",
                self.name
            );
            self.queue.apply(&o, step as u64);
            took.push((signals, o.channel));
            self.yielded.push((o, step));
        }
        took
    }

    /// The sweep the daemon runs, after a process ends or a lock goes.
    fn sweep(&self) -> Vec<Observation> {
        self.queue
            .sweep(|liveness| liveness::alive(&self.kernel, liveness))
    }

    /// Makes a step that is not an expect, and says what the source tells
    /// of it.
    fn step(&mut self, source: &mut dyn Source, step: &Step) -> Vec<Observation> {
        match step {
            Step::Expect { .. } => unreachable!("an expect makes no change"),
            Step::Hook { hook, ancestry } => source.hook(&self.kernel, &self.hook(hook, ancestry)),
            step if step.file().is_some() => {
                let path = self.write(step);
                changed(source, &self.kernel, Some(&[path]))
            }
            step => {
                self.kernel(step);
                self.sweep()
            }
        }
    }
}

/// What a source reads of files that changed, and then of whatever its
/// budget left unread, as the daemon reads it.
fn changed(source: &mut dyn Source, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
    let mut observations = source.changed(k, paths);
    while source.behind() {
        observations.extend(source.changed(k, None));
    }
    observations
}

/// Replays a fixture, checking its expectations and the three properties.
pub fn replay(harness: Harness, name: &str) -> Replay {
    let fixture = load(harness, name);
    // Harnesses name their fixtures alike, so every message says whose.
    let name = &format!("{}/{name}", harness.name());
    let mut replay = Replay::new(harness, name);
    let mut source = replay.source();
    let seeded = source.seed(&replay.kernel, &[]).unwrap();
    let mut since = replay.take(seeded, 0);
    // The steps since the last expect after which a restart shows another
    // phase, with the phase it shows.
    let mut differs: Vec<(usize, Phase)> = Vec::new();
    // The phases after each step that is not an expect.
    let mut shown: Vec<(usize, BTreeMap<Key, Phase>)> = Vec::new();
    for (index, step) in fixture.steps.iter().enumerate() {
        let number = index + 1;
        let observations = match step {
            Step::Expect { expect, restart } => {
                let expect: Vec<(BTreeSet<Signal>, Channel)> = expect
                    .iter()
                    .map(|(told, channel)| (told.signals(), *channel))
                    .collect();
                assert_eq!(since, expect, "{name}: what led to step {number}");
                for (after, shown) in &differs {
                    assert_eq!(
                        Some(*shown),
                        *restart,
                        "{name}: a daemon started after step {after} shows {shown:?}"
                    );
                }
                assert!(
                    restart.is_none() || !differs.is_empty(),
                    "{name}: step {number} says a restart shows {restart:?}, and none does"
                );
                since.clear();
                differs.clear();
                continue;
            }
            step => replay.step(source.as_mut(), step),
        };
        since.extend(replay.take(observations, number));
        let again = source.changed(&replay.kernel, None);
        assert!(
            again.is_empty(),
            "{name}: a rescan after step {number} yielded {:?}",
            again.iter().map(|o| o.event.signals()).collect::<Vec<_>>()
        );
        let restarted = phases(&restart(&replay));
        let live = replay.phases();
        shown.push((number, live.clone()));
        assert!(
            restarted.keys().eq(live.keys()),
            "{name}: a daemon started after step {number} follows {restarted:?}, not {live:?}"
        );
        for (key, phase) in restarted {
            if phase != live[&key] {
                differs.push((number, phase));
            }
        }
    }
    assert!(
        since.is_empty(),
        "{name}: the last steps yielded {since:?} with no expect after them"
    );
    assert!(
        differs.is_empty(),
        "{name}: a daemon started after the last steps shows {differs:?}, with no expect after them"
    );
    let coalesced = coalesce(harness, name, &fixture);
    assert_eq!(
        coalesced,
        replay.phases(),
        "{name}: reading the file steps two at a time ends elsewhere"
    );
    for &(after, _) in &shown {
        resume(harness, name, &fixture, after, &shown);
    }
    replay
}

/// Replays a fixture with a daemon restarted after step `after`, which must
/// show after each later step what the replay showed there, unless that
/// step's expect says a restart shows another phase: whatever a hook told
/// that the files cannot, the restarted source can still take on from there.
fn resume(
    harness: Harness,
    name: &str,
    fixture: &Fixture,
    after: usize,
    shown: &[(usize, BTreeMap<Key, Phase>)],
) {
    let mut replay = Replay::new(harness, name);
    let mut source = replay.source();
    let seeded = source.seed(&replay.kernel, &[]).unwrap();
    replay.take(seeded, 0);
    for (number, live) in shown {
        let step = &fixture.steps[number - 1];
        let observations = replay.step(source.as_mut(), step);
        replay.take(observations, *number);
        if *number == after {
            (source, replay.queue) = restarted(&replay);
        } else if *number > after && !untold(fixture, *number) {
            assert_eq!(
                replay.phases(),
                *live,
                "{name}: a daemon started after step {after} shows this after step {number}"
            );
        }
    }
}

/// Whether the expect after a step says a restart shows another phase.
fn untold(fixture: &Fixture, number: usize) -> bool {
    let mut after = fixture.steps[number..].iter();
    let restart = after.find_map(|step| match step {
        Step::Expect { restart, .. } => Some(restart.is_some()),
        _ => None,
    });
    restart.unwrap_or(false)
}

/// A payload with each string that starts `~/` made a path under `home`.
fn at_home(payload: &Value, home: &Path) -> Value {
    match payload {
        Value::String(text) => match text.strip_prefix("~/") {
            Some(path) => Value::from(home.join(path).to_string_lossy()),
            None => payload.clone(),
        },
        Value::Array(items) => items.iter().map(|item| at_home(item, home)).collect(),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(name, value)| (name.clone(), at_home(value, home)))
                .collect(),
        ),
        _ => payload.clone(),
    }
}

/// The phases a fixture ends in when its file steps are read two at a time,
/// as FSEvents may coalesce them.
fn coalesce(harness: Harness, name: &str, fixture: &Fixture) -> BTreeMap<Key, Phase> {
    let mut replay = Replay::new(harness, name);
    let mut source = replay.source();
    let seeded = source.seed(&replay.kernel, &[]).unwrap();
    replay.take(seeded, 0);
    let mut pending: Vec<PathBuf> = Vec::new();
    let steps = fixture
        .steps
        .iter()
        .filter(|step| !matches!(step, Step::Expect { .. }));
    for (index, step) in steps.enumerate() {
        let number = index + 1;
        if step.file().is_some() {
            pending.push(replay.write(step));
            if pending.len() == 2 {
                let observations = changed(source.as_mut(), &replay.kernel, Some(&pending));
                replay.take(observations, number);
                pending.clear();
            }
            continue;
        }
        if !pending.is_empty() {
            let observations = changed(source.as_mut(), &replay.kernel, Some(&pending));
            replay.take(observations, number);
            pending.clear();
        }
        let observations = match step {
            Step::Hook { hook, ancestry } => {
                source.hook(&replay.kernel, &replay.hook(hook, ancestry))
            }
            step => {
                replay.kernel(step);
                replay.sweep()
            }
        };
        replay.take(observations, number);
    }
    if !pending.is_empty() {
        let observations = changed(source.as_mut(), &replay.kernel, Some(&pending));
        replay.take(observations, fixture.steps.len());
    }
    replay.phases()
}

/// A fresh source seeded from what a replay left on disk, as a daemon
/// starting then would, given the hook-bound sessions in `bound`, with the
/// queue its seed makes.
fn seeded(replay: &Replay, bound: &[(String, Liveness)]) -> (Box<dyn Source>, Queue) {
    let mut source = replay.source();
    let mut queue = Queue::default();
    for o in source.seed(&replay.kernel, bound).unwrap() {
        assert!(o.quiet, "{}: a seed announced", replay.name);
        queue.apply(&o, 0);
    }
    (source, queue)
}

/// The queue a fresh source seeds from what a replay left on disk alone.
pub fn seed(replay: &Replay) -> Queue {
    seeded(replay, &[]).1
}

/// A daemon starting now: a fresh source seeded with the hook-bound
/// sessions the replay's queue holds, and the queue it shows, that seed
/// restored against the replay's queue as saved.
fn restarted(replay: &Replay) -> (Box<dyn Source>, Queue) {
    let saved: Queue =
        serde_json::from_value(serde_json::to_value(&replay.queue).unwrap()).unwrap();
    let bound: Vec<(String, Liveness)> = saved
        .bound()
        .into_iter()
        .filter(|(key, _)| key.harness == replay.harness)
        .map(|(key, liveness)| (key.id, liveness))
        .collect();
    let (source, fresh) = seeded(replay, &bound);
    (source, Queue::restore(saved, fresh))
}

/// Holds a daemon started after a replay to a read of `file` that stops at
/// its budget and then fails for good: the source is not left behind, so it
/// is not read again and again until the file can be read.
pub fn a_read_that_fails_leaves_nothing_behind(replay: &Replay, file: &str) {
    let file = replay.home.path().join(file);
    let (mut source, _) = restarted(replay);
    let line = format!("{{\"pad\": \"{}\"}}\n", "x".repeat(1 << 20));
    let mut log = OpenOptions::new().append(true).open(&file).unwrap();
    log.write_all(line.repeat(9).as_bytes()).unwrap();
    source.changed(&replay.kernel, Some(std::slice::from_ref(&file)));
    assert!(source.behind(), "{}: read all at once", replay.name);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    source.changed(&replay.kernel, Some(std::slice::from_ref(&file)));
    let behind = source.behind();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!behind, "{}: behind on a file it cannot read", replay.name);
}

/// The queue a daemon starting now would show.
pub fn restart(replay: &Replay) -> Queue {
    restarted(replay).1
}

/// The phase of each session a queue tracks.
fn phases(queue: &Queue) -> BTreeMap<Key, Phase> {
    queue
        .sessions()
        .iter()
        .map(|session| (session.key.clone(), session.phase))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coverage::Declared;

    /// The name a fixture that demonstrates a declaration has:
    /// `<signal>.<channel>`, as the fixtures spell them.
    fn demonstrating(declared: &Declared) -> String {
        let signal = serde_json::to_value(declared.signal).unwrap();
        let channel = serde_json::to_value(declared.channel).unwrap();
        format!("{}.{}", signal.as_str().unwrap(), channel.as_str().unwrap())
    }

    #[test]
    fn every_declared_row_is_demonstrated_by_a_fixture_of_its_evidence() {
        for harness in Harness::ALL {
            for declared in harness::signals(harness) {
                let name = demonstrating(declared);
                assert!(
                    names(harness).contains(&name),
                    "{}: no fixture {name} demonstrates {:?} on {:?}",
                    harness.title(),
                    declared.signal,
                    declared.channel
                );
                let fixture = load(harness, &name);
                assert_eq!(
                    fixture.evidence,
                    declared.evidence,
                    "{}: {name} ({}) is not of the evidence its row declares",
                    harness.title(),
                    fixture.cite
                );
                let replay = replay(harness, &name);
                assert!(
                    replay.yielded.iter().any(|(o, _)| o
                        .event
                        .signals()
                        .contains(&declared.signal)
                        && o.channel == declared.channel),
                    "{}: {name} never yields what it demonstrates",
                    harness.title()
                );
            }
        }
    }

    /// The replay fails on any observation its harness does not declare.
    #[test]
    fn every_fixture_yields_only_what_its_harness_declares() {
        for harness in Harness::ALL {
            let names = names(harness);
            assert!(!names.is_empty(), "{} has no fixtures", harness.title());
            for name in names {
                assert!(
                    !load(harness, &name).cite.is_empty(),
                    "{name} cites nothing"
                );
                replay(harness, &name);
            }
        }
    }

    #[test]
    fn a_signal_told_on_two_channels_is_idempotent() {
        for harness in Harness::ALL {
            for declared in harness::signals(harness) {
                let channels: HashSet<Channel> = harness::signals(harness)
                    .iter()
                    .filter(|other| other.signal == declared.signal)
                    .map(|other| other.channel)
                    .collect();
                assert!(
                    channels.len() == 1 || fsm::idempotent(declared.signal),
                    "{}: {:?} comes on {channels:?} and is not idempotent",
                    harness.title(),
                    declared.signal
                );
            }
        }
    }

    /// A sweep ends a session through the channel that started it, so Ended
    /// must come on that channel too, or a session whose process is gone
    /// keeps its light.
    #[test]
    fn every_harness_declares_how_its_sessions_start_and_end() {
        for harness in Harness::ALL {
            let started: Vec<Channel> = harness::signals(harness)
                .iter()
                .filter(|declared| declared.signal == Signal::Started)
                .map(|declared| declared.channel)
                .collect();
            assert!(
                !started.is_empty(),
                "{} declares no Started",
                harness.title()
            );
            for channel in started {
                assert!(
                    harness::declares(harness, Signal::Ended, channel),
                    "{} starts sessions on {channel:?} and does not end them there",
                    harness.title()
                );
            }
        }
    }

    /// The hook event a fixture's hook step stands for, as an install
    /// registers it.
    fn registered(harness: Harness, payload: &Value) -> Option<String> {
        let named = |field: &str| payload.get(field)?.as_str().map(str::to_owned);
        match harness {
            // Grok names its events in snake case, and is registered for them
            // in Pascal case.
            Harness::Grok => Some(
                named("hookEventName")?
                    .split('_')
                    .map(|word| word[..1].to_uppercase() + &word[1..])
                    .collect(),
            ),
            // OpenCode's plugin names the event it passes on, and Pi's
            // extension passes on Pi's own name for it.
            Harness::OpenCode | Harness::Pi => named("event"),
            _ => named("hook_event_name"),
        }
    }

    #[test]
    fn install_registers_exactly_the_hooks_the_source_reads() {
        for harness in Harness::ALL {
            let events = harness.events();
            // Claude Code has no hooks.
            if events.is_empty() {
                continue;
            }
            let mut heard = HashSet::new();
            for name in names(harness) {
                let fixture = load(harness, &name);
                let replay = replay(harness, &name);
                for (o, step) in &replay.yielded {
                    let Step::Hook { hook, .. } = &fixture.steps[step - 1] else {
                        continue;
                    };
                    let event = registered(harness, hook).expect("a hook names its event");
                    assert!(
                        events.contains(&event.as_str()),
                        "{}: {name} yields {:?} from {event}, which install does not register",
                        harness.title(),
                        o.event.signals()
                    );
                    heard.insert(event);
                }
            }
            for event in events {
                assert!(
                    heard.contains(event),
                    "{}: install registers {event}, and no fixture's {event} yields anything",
                    harness.title()
                );
            }
        }
    }
}
