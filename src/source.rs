//! Reading what the harnesses write about their sessions, and what their
//! hooks tell.
//!
//! Each harness has one source. A source reads the harness's own files as
//! they change, and translates the hooks that remain, into observations of
//! its sessions. It never sees the queue: it reports everything it reads,
//! and the table drops what is about a session nobody tracks.

pub mod claude;
#[cfg(test)]
pub mod fixture;
// Read by the harness sources, which come in the next layer.
#[allow(dead_code)]
pub mod tail;

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::daemon::Input;
use crate::fsm::{self, Channel, Phase, Signal};
use crate::harness::Harness;
use crate::liveness::{Kernel, Liveness, Proc, System};
use crate::queue::{Key, TurnKey};

/// One thing a harness said about a session, whichever way it came.
#[derive(Clone, Debug)]
pub struct Observation {
    pub key: Key,
    pub channel: Channel,
    pub event: Event,
    /// The harness's name for the turn it is about, when it gives one.
    pub turn: Option<TurnKey>,
    /// Startup seeding: applied, never announced.
    pub quiet: bool,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A process of the harness runs the session.
    Started(Start),
    Signal(Signal),
}

impl Event {
    /// The signal the event stands for in the table.
    pub fn signal(&self) -> Signal {
        match self {
            Event::Started(_) => Signal::Started,
            Event::Signal(signal) => *signal,
        }
    }
}

/// What a session's start says: where it works and how to tell it still runs.
#[derive(Clone, Debug)]
pub struct Start {
    pub cwd: String,
    pub liveness: Liveness,
}

/// A hook as the command line delivers it: the payload the harness gave it,
/// and the hook process's own ancestors, nearest first.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hook {
    pub harness: Harness,
    pub payload: Value,
    pub ancestry: Vec<Proc>,
}

/// What one harness's files and hooks say about its sessions.
pub trait Source {
    fn harness(&self) -> Harness;
    /// The folders whose changes this source reads, recursively. Asked
    /// again after each hook and device check, for folders that appear later.
    fn roots(&self) -> Vec<PathBuf>;
    /// At daemon start: for each session its files show running, and each
    /// hook-bound session in `bound` that is still alive, a quiet Started
    /// and then `current()`, leaving every reader at the end of its file.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Vec<Observation>>;
    /// What changed under `paths`, or under every root when None, as after
    /// dropped events.
    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation>;
    /// Catches up on the session's files, then translates the hook.
    fn hook(&mut self, k: &dyn Kernel, hook: &Hook) -> Vec<Observation>;
    /// The signals that take the session from Idle to where it stands now.
    fn current(&self, id: &str) -> Vec<(Signal, Option<TurnKey>)>;
    /// Whether a read stopped at its budget and left bytes to read.
    fn behind(&self) -> bool;
}

/// Where a source last left a session, by the signals it told: kept so the
/// source can say how the session stands without telling it all again.
#[derive(Clone, Copy, Debug)]
pub struct Told {
    phase: Phase,
    /// The wait it last told, which Blocked came from.
    wait: Signal,
}

/// A session just started: Idle.
impl Default for Told {
    fn default() -> Told {
        Told {
            phase: Phase::Idle,
            wait: Signal::PermissionAsked,
        }
    }
}

impl Told {
    pub fn tell(&mut self, signal: Signal) {
        if let Some(to) = fsm::next(self.phase, signal) {
            self.phase = to;
        }
        if matches!(signal, Signal::PermissionAsked | Signal::QuestionAsked) {
            self.wait = signal;
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The shortest signals from Idle to the same phase.
    pub fn current(&self) -> Vec<Signal> {
        match self.phase {
            Phase::Gone | Phase::Idle => Vec::new(),
            Phase::Working => vec![Signal::Prompted],
            Phase::Background => vec![Signal::Backgrounded],
            Phase::Blocked => vec![self.wait],
            Phase::Asked | Phase::AskedRead => vec![Signal::TurnEndedAsking],
            Phase::Stopped | Phase::StoppedRead => vec![Signal::TurnEnded],
            Phase::Failed => vec![Signal::TurnFailed],
        }
    }
}

/// Every harness's source, reading the harnesses' files in `home`.
pub fn all(home: &Path, _state: &Path) -> Vec<Box<dyn Source>> {
    vec![Box::new(claude::Claude::new(home))]
}

/// What the daemon asks the sources to read.
pub enum Read<'a> {
    /// Daemon start, with the hook-bound sessions saved in the queue.
    Seed(&'a [(Key, Liveness)]),
    /// Files the watcher says changed.
    Paths(&'a [PathBuf]),
    /// Everything, because the watcher dropped events.
    All,
    Hook(&'a Hook),
    /// Whatever a budgeted read left unread.
    Behind,
    /// Roots that have appeared since they were last looked for.
    Roots,
}

/// The sources and the watcher that feeds them.
pub struct Sources {
    sources: Vec<Box<dyn Source>>,
    /// Each watched root as the file system spells it (FSEvents reports
    /// `/private/tmp`, not `/tmp`), with the root as its source spells it
    /// and the source's index.
    watched: Vec<(PathBuf, PathBuf, usize)>,
    watcher: notify::RecommendedWatcher,
    kernel: System,
}

impl Sources {
    /// Starts watching every root that exists. Seeding comes after, so a
    /// change in between is read from where the seed left the file.
    pub fn start(sources: Vec<Box<dyn Source>>, inputs: Sender<Input>) -> Result<Sources> {
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let _ = inputs.send(match event {
                Ok(event) if !event.need_rescan() => Input::Files(event.paths),
                // FSEvents dropped events, or the watcher failed.
                _ => Input::Rescan,
            });
        })?;
        let mut sources = Sources {
            sources,
            watched: Vec::new(),
            watcher,
            kernel: System,
        };
        sources.watch();
        Ok(sources)
    }

    pub fn read(&mut self, what: Read) -> Vec<Observation> {
        let kernel = &self.kernel;
        match what {
            Read::Seed(bound) => {
                let mut seeded = Vec::new();
                for source in &mut self.sources {
                    let harness = source.harness();
                    let bound: Vec<(String, Liveness)> = bound
                        .iter()
                        .filter(|(key, _)| key.harness == harness)
                        .map(|(key, liveness)| (key.id.clone(), liveness.clone()))
                        .collect();
                    match source.seed(kernel, &bound) {
                        Ok(observations) => seeded.extend(observations),
                        Err(error) => eprintln!("agent-leds: {}: {error:#}", harness.title()),
                    }
                }
                seeded
            }
            Read::Paths(paths) => {
                let mut routed: Vec<Vec<PathBuf>> = vec![Vec::new(); self.sources.len()];
                for path in paths {
                    let root = self
                        .watched
                        .iter()
                        .filter(|(canonical, _, _)| path.starts_with(canonical))
                        .max_by_key(|(canonical, _, _)| canonical.as_os_str().len());
                    if let Some((canonical, root, index)) = root {
                        let path = root.join(path.strip_prefix(canonical).expect("a prefix"));
                        if !routed[*index].contains(&path) {
                            routed[*index].push(path);
                        }
                    }
                }
                let mut changed = Vec::new();
                for (source, paths) in self.sources.iter_mut().zip(routed) {
                    if !paths.is_empty() {
                        changed.extend(source.changed(kernel, Some(&paths)));
                    }
                }
                changed
            }
            Read::All => self
                .sources
                .iter_mut()
                .flat_map(|source| source.changed(kernel, None))
                .collect(),
            Read::Hook(hook) => {
                let mut told = Vec::new();
                for source in &mut self.sources {
                    if source.harness() == hook.harness {
                        told.extend(source.hook(kernel, hook));
                    }
                }
                // A hook can name a folder that was not there before.
                told.extend(self.read(Read::Roots));
                told
            }
            Read::Behind => self
                .sources
                .iter_mut()
                .filter(|source| source.behind())
                .flat_map(|source| source.changed(kernel, None))
                .collect(),
            Read::Roots => {
                let mut found = Vec::new();
                for index in self.watch() {
                    found.extend(self.sources[index].changed(&self.kernel, None));
                }
                found
            }
        }
    }

    /// Whether any source left bytes unread.
    pub fn behind(&self) -> bool {
        self.sources.iter().any(|source| source.behind())
    }

    /// Starts watching each root that exists and is not watched yet, and
    /// says which sources gained one.
    fn watch(&mut self) -> Vec<usize> {
        let mut gained = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            for root in source.roots() {
                if self.watched.iter().any(|(_, watched, _)| *watched == root) {
                    continue;
                }
                let Ok(canonical) = root.canonicalize() else {
                    continue;
                };
                if let Err(error) = self.watcher.watch(&canonical, RecursiveMode::Recursive) {
                    eprintln!("agent-leds: watching {}: {error}", root.display());
                    continue;
                }
                self.watched.push((canonical, root, index));
                if !gained.contains(&index) {
                    gained.push(index);
                }
            }
        }
        gained
    }
}
