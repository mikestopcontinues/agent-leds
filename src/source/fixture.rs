//! Replays a fixture, a session as a harness wrote it, against the
//! harness's source on a home directory of its own.
//!
//! A fixture is `tests/fixtures/<harness>/<name>.jsonl`. Its first line says
//! where it comes from, `{"cite": "probe-claude cc1", "evidence": "observed"}`,
//! and each line after is one step:
//!
//! - `{"append": "<path under home>", "line": ...}` adds a line to a file,
//!   `"times": <n>` times over if it says so;
//! - `{"write": "<path>", "text": ...}` rewrites a file in place;
//! - `{"delete": "<path>"}`;
//! - `{"hook": <payload>, "ancestry": [<proc>, ...]}` delivers a hook;
//! - `{"alive": <pid>, "started_us": <n>}` and `{"exit": <pid>}` start and
//!   end a process, and `{"lock": "<path>"}` and `{"unlock": "<path>"}` take
//!   and drop a lock, for the fake kernel the source and the sweep ask;
//! - `{"expect": [["<signal>", "<channel>"], ...]}` says what every step
//!   since the last `expect` yielded, in order.
//!
//! A line or text that is not a string is written as compact JSON. After
//! each step the replay also holds the source to three properties: a rescan
//! yields nothing, `current()` rebuilds the phase the observations reached,
//! and reading the file steps two at a time ends in the same phases.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use super::{Hook, Observation, Source, all};
use crate::coverage::Evidence;
use crate::fsm::{self, Channel, Phase, Signal};
use crate::harness::{self, Harness};
use crate::liveness::{self, Kernel, Proc};
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
        line: Value,
        times: Option<usize>,
    },
    Write {
        write: String,
        text: Value,
    },
    Delete {
        delete: String,
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
    },
    Unlock {
        unlock: String,
    },
    Expect {
        expect: Vec<(Signal, Channel)>,
    },
}

impl Step {
    /// The file a step writes, if it is a file step.
    fn file(&self) -> Option<&str> {
        match self {
            Step::Append { append: path, .. }
            | Step::Write { write: path, .. }
            | Step::Delete { delete: path } => Some(path),
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
        all(home, &home.join(".local/state/agent-leds"))
            .into_iter()
            .find(|source| source.harness() == self.harness)
            .expect("every harness has a source")
    }

    /// The phase of each session the queue tracks.
    pub fn phases(&self) -> BTreeMap<Key, Phase> {
        self.queue
            .sessions()
            .iter()
            .map(|session| (session.key.clone(), session.phase))
            .collect()
    }

    /// Makes a file step's change on disk.
    fn write(&self, step: &Step) -> PathBuf {
        let text = |value: &Value| match value {
            Value::String(text) => text.clone(),
            value => value.to_string(),
        };
        let path = self.home.path().join(step.file().expect("a file step"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        match step {
            Step::Append { line, times, .. } => {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                let line = text(line);
                for _ in 0..times.unwrap_or(1) {
                    writeln!(file, "{line}").unwrap();
                }
            }
            Step::Write { text: value, .. } => fs::write(&path, text(value)).unwrap(),
            Step::Delete { .. } => fs::remove_file(&path).unwrap(),
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
            Step::Lock { lock } => {
                self.kernel.locks.insert(home.join(lock));
            }
            Step::Unlock { unlock } => {
                self.kernel.locks.remove(&home.join(unlock));
            }
            _ => unreachable!(),
        }
    }

    /// Applies what one step yielded, after checking each is declared.
    fn take(&mut self, observations: Vec<Observation>, step: usize) -> Vec<(Signal, Channel)> {
        let mut took = Vec::new();
        for o in observations {
            let signal = o.event.signal();
            assert!(
                harness::declares(self.harness, signal, o.channel),
                "{}: step {step} yielded {signal:?} on {:?}, which {} does not declare",
                self.name,
                o.channel,
                self.harness.title()
            );
            self.queue.apply(&o, step as u64);
            took.push((signal, o.channel));
            self.yielded.push((o, step));
        }
        took
    }

    /// The sweep the daemon runs, after a process ends or a lock goes.
    fn sweep(&self) -> Vec<Observation> {
        self.queue
            .sweep(|liveness| liveness::alive(&self.kernel, liveness))
    }
}

/// What a source reads of files that changed, and then of whatever its
/// budget left unread, as the daemon reads it.
fn changed(source: &mut dyn Source, k: &dyn Kernel, paths: &[PathBuf]) -> Vec<Observation> {
    let mut observations = source.changed(k, Some(paths));
    while source.behind() {
        observations.extend(source.changed(k, None));
    }
    observations
}

/// Replays a fixture, checking its expectations and the three properties.
pub fn replay(harness: Harness, name: &str) -> Replay {
    let fixture = load(harness, name);
    let mut replay = Replay::new(harness, name);
    let mut source = replay.source();
    let seeded = source.seed(&replay.kernel, &[]).unwrap();
    let mut since = replay.take(seeded, 0);
    for (index, step) in fixture.steps.iter().enumerate() {
        let number = index + 1;
        let observations = match step {
            Step::Expect { expect } => {
                assert_eq!(&since, expect, "{name}: what led to step {number}");
                since.clear();
                continue;
            }
            Step::Hook { hook, ancestry } => source.hook(
                &replay.kernel,
                &Hook {
                    harness,
                    payload: hook.clone(),
                    ancestry: ancestry.clone(),
                },
            ),
            step if step.file().is_some() => {
                let path = replay.write(step);
                changed(source.as_mut(), &replay.kernel, &[path])
            }
            step => {
                replay.kernel(step);
                replay.sweep()
            }
        };
        since.extend(replay.take(observations, number));
        let again = source.changed(&replay.kernel, None);
        assert!(
            again.is_empty(),
            "{name}: a rescan after step {number} yielded {:?}",
            again.iter().map(|o| o.event.signal()).collect::<Vec<_>>()
        );
        for (key, phase) in replay.phases() {
            let rebuilt = source
                .current(&key.id)
                .into_iter()
                .fold(Phase::Idle, |phase, (signal, _)| {
                    fsm::next(phase, signal).unwrap_or(phase)
                });
            assert_eq!(
                rebuilt, phase,
                "{name}: after step {number}, current() of {} rebuilds another phase",
                key.id
            );
        }
    }
    assert!(
        since.is_empty(),
        "{name}: the last steps yielded {since:?} with no expect after them"
    );
    let coalesced = coalesce(harness, name, &fixture);
    assert_eq!(
        coalesced,
        replay.phases(),
        "{name}: reading the file steps two at a time ends elsewhere"
    );
    replay
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
                let observations = changed(source.as_mut(), &replay.kernel, &pending);
                replay.take(observations, number);
                pending.clear();
            }
            continue;
        }
        if !pending.is_empty() {
            let observations = changed(source.as_mut(), &replay.kernel, &pending);
            replay.take(observations, number);
            pending.clear();
        }
        let observations = match step {
            Step::Hook { hook, ancestry } => source.hook(
                &replay.kernel,
                &Hook {
                    harness,
                    payload: hook.clone(),
                    ancestry: ancestry.clone(),
                },
            ),
            step => {
                replay.kernel(step);
                replay.sweep()
            }
        };
        replay.take(observations, number);
    }
    if !pending.is_empty() {
        let observations = changed(source.as_mut(), &replay.kernel, &pending);
        replay.take(observations, fixture.steps.len());
    }
    replay.phases()
}

/// The queue a fresh source seeds from what a replay left on disk, as a
/// daemon starting then would.
pub fn seed(replay: &Replay) -> Queue {
    let mut source = replay.source();
    let mut queue = Queue::default();
    for o in source.seed(&replay.kernel, &[]).unwrap() {
        assert!(o.quiet, "{}: a seed announced", replay.name);
        queue.apply(&o, 0);
    }
    queue
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
                    replay
                        .yielded
                        .iter()
                        .any(|(o, _)| o.event.signal() == declared.signal
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

    #[test]
    fn every_harness_declares_how_its_sessions_start_and_end() {
        for harness in Harness::ALL {
            for signal in [Signal::Started, Signal::Ended] {
                assert!(
                    harness::signals(harness)
                        .iter()
                        .any(|declared| declared.signal == signal),
                    "{} declares no {signal:?}",
                    harness.title()
                );
            }
        }
    }
}
