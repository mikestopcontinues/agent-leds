//! Reading what the harnesses write about their sessions, and what their
//! hooks tell.
//!
//! Each harness has one source. A source reads the harness's own files as
//! they change, and translates the hooks that remain, into observations of
//! its sessions. It never sees the queue: it reports everything it reads,
//! and the table drops what is about a session nobody tracks.

pub mod claude;
pub mod codex;
pub mod cursor;
#[cfg(test)]
pub mod fixture;
pub mod grok;
pub mod junie;
pub mod opencode;
pub mod pi;
pub mod tail;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::daemon::Input;
use crate::fsm::{self, Channel, Phase, Signal};
use crate::harness::{self, Harness};
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

impl Observation {
    /// What a source tells of the session `id` of `harness`.
    pub fn new(
        harness: Harness,
        id: &str,
        channel: Channel,
        event: Event,
        turn: Option<TurnKey>,
    ) -> Observation {
        Observation {
            key: Key {
                harness,
                id: id.to_owned(),
            },
            channel,
            event,
            turn,
            quiet: false,
        }
    }

    /// The signals in it that its harness does not declare through its
    /// channel. The daemon's own are never declared, and never undeclared.
    pub fn undeclared(&self) -> Vec<Signal> {
        if self.channel == Channel::Daemon {
            return Vec::new();
        }
        let harness = self.key.harness;
        self.event
            .signals()
            .into_iter()
            .filter(|&signal| !harness::declares(harness, signal, self.channel))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    /// A process of the harness runs the session.
    Started(Start),
    /// What one record says, each signal a fact of its own; the table's
    /// order decides what they come to.
    Signals(BTreeSet<Signal>),
}

impl Event {
    /// One signal, told alone.
    pub fn one(signal: Signal) -> Event {
        Event::Signals(BTreeSet::from([signal]))
    }

    /// The signals the event stands for in the table.
    pub fn signals(&self) -> BTreeSet<Signal> {
        match self {
            Event::Started(_) => BTreeSet::from([Signal::Started]),
            Event::Signals(signals) => signals.clone(),
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

/// What a seed read: where each running session's files leave it, and
/// each file it found but could not read or make sense of.
#[derive(Debug, Default)]
pub struct Seed {
    pub observations: Vec<Observation>,
    pub unread: Vec<Unread>,
}

/// A file a source found but could not read or make sense of, and why. A
/// file read as it changes may be caught half written, and is read again at
/// its next change; at a seed it is not, so the seed says so.
#[derive(Debug)]
pub struct Unread {
    pub path: PathBuf,
    pub error: String,
}

impl Unread {
    pub fn new(path: &Path, error: impl std::fmt::Display) -> Unread {
        Unread {
            path: path.to_owned(),
            error: format!("{error:#}"),
        }
    }
}

/// What a source's reads could not make sense of: lines that are not
/// records it can read, lines too long to read at all, and records of kinds
/// its harness was not seen to write, by kind. A harness that writes
/// otherwise after an update shows up here first.
#[derive(Debug, Default, PartialEq)]
pub struct Strange {
    pub unparsed: usize,
    pub oversized: usize,
    pub kinds: BTreeMap<String, usize>,
}

impl Strange {
    /// A line read as `record`, counted if it is none.
    pub fn parsed<R>(&mut self, record: Option<R>) -> Option<R> {
        if record.is_none() {
            self.unparsed += 1;
        }
        record
    }

    /// A record of `kind`, counted if it is none of those `known`.
    pub fn kind(&mut self, kind: &str, known: &[&str]) {
        if !known.contains(&kind) {
            *self.kinds.entry(kind.to_owned()).or_default() += 1;
        }
    }

    /// What was counted, as `harness`'s source says it, if anything.
    pub fn said(&self, harness: Harness) -> Option<String> {
        let said: Vec<String> = self
            .lines()
            .into_iter()
            .chain(self.unknown(harness))
            .collect();
        (!said.is_empty()).then(|| said.join("; "))
    }

    /// The lines that are not records, and those too long to read, if
    /// any. A crash or a rewrite leaves a line cut short, and the longest
    /// lines are images and terminal output, so they are no sign of a new
    /// format by themselves.
    pub fn lines(&self) -> Option<String> {
        let unparsed = match self.unparsed {
            0 => None,
            1 => Some("1 line is not a record it reads".to_owned()),
            n => Some(format!("{n} lines are not records it reads")),
        };
        let oversized = match self.oversized {
            0 => None,
            1 => Some("1 line is too long to read".to_owned()),
            n => Some(format!("{n} lines are too long to read")),
        };
        let said: Vec<String> = unparsed.into_iter().chain(oversized).collect();
        (!said.is_empty()).then(|| said.join("; "))
    }

    /// The records of kinds `harness` was not seen to write, if any.
    pub fn unknown(&self, harness: Harness) -> Option<String> {
        if self.kinds.is_empty() {
            return None;
        }
        let kinds: Vec<String> = self
            .kinds
            .iter()
            .map(|(kind, n)| format!("{kind} ({n})"))
            .collect();
        Some(format!(
            "records of kinds {} was not seen to write: {}",
            harness.title(),
            kinds.join(", ")
        ))
    }
}

/// What one harness's files and hooks say about its sessions.
pub trait Source {
    fn harness(&self) -> Harness;
    /// The folders whose changes this source reads, recursively. Asked
    /// again after each hook and device check, for folders that appear later.
    fn roots(&self) -> Vec<PathBuf>;
    /// The folders whose own files it reads changes of, and not those in
    /// their subfolders, as `roots` are asked.
    fn shallow_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    /// At daemon start: for each session its files show running, and each
    /// hook-bound session in `bound` that is still alive, a quiet Started
    /// and then the signals `Told::current` gives of where its files leave
    /// it, leaving every reader at the end of its file; and each file of a
    /// running session it could not read or make sense of.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Seed>;
    /// What changed under `paths`, or under every root when None, as after
    /// dropped events.
    fn changed(&mut self, k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation>;
    /// Catches up on the session's files, then translates the hook.
    fn hook(&mut self, k: &dyn Kernel, hook: &Hook) -> Vec<Observation>;
    /// The files whose last read stopped at its budget and left bytes to
    /// read, which `changed` given them reads on from there, and nothing
    /// else, in sorted order.
    fn behind(&self) -> Vec<PathBuf>;
    /// What its reads since it was last asked could not make sense of.
    fn strange(&mut self) -> Strange;
}

/// Where a source last left a session, by the signals it told: kept so the
/// source can say how the session stands without telling it all again.
#[derive(Clone, Copy, Debug)]
pub struct Told {
    phase: Phase,
    /// The wait it last told, which Blocked came from.
    wait: Signal,
    /// While blocked, the phase it was blocked from.
    held: Phase,
}

/// A session just started: Idle.
impl Default for Told {
    fn default() -> Told {
        Told {
            phase: Phase::Idle,
            wait: Signal::PermissionAsked,
            held: Phase::Idle,
        }
    }
}

impl Told {
    pub fn tell(&mut self, signals: &BTreeSet<Signal>) {
        let Some(rule) = fsm::next(self.phase, signals) else {
            return;
        };
        let to = rule.to.phase(self.held);
        if to == Phase::Blocked && self.phase != Phase::Blocked {
            self.held = self.phase;
        }
        self.phase = to;
        if matches!(rule.on, Signal::PermissionAsked | Signal::QuestionAsked) {
            self.wait = rule.on;
        }
    }

    /// Tells what one record or hook says of the session `id` of
    /// `harness`, on `turn`.
    pub fn observe(
        &mut self,
        harness: Harness,
        id: &str,
        channel: Channel,
        signals: impl IntoIterator<Item = Signal>,
        turn: Option<TurnKey>,
    ) -> Observation {
        let signals: BTreeSet<Signal> = signals.into_iter().collect();
        self.tell(&signals);
        Observation::new(harness, id, channel, Event::Signals(signals), turn)
    }

    /// The shortest signals from Idle to the same phase, each told on its
    /// own: for Blocked, by way of the phase it was blocked from.
    pub fn current(&self) -> Vec<Signal> {
        match self.phase {
            Phase::Gone | Phase::Idle => Vec::new(),
            Phase::Working => vec![Signal::Prompted],
            Phase::Background => vec![Signal::Backgrounded],
            Phase::Blocked => {
                let held = Told {
                    phase: self.held,
                    ..*self
                };
                let mut path = held.current();
                path.push(self.wait);
                path
            }
            Phase::Asked | Phase::AskedRead => vec![Signal::TurnEndedAsking],
            Phase::Stopped | Phase::StoppedRead => vec![Signal::TurnEnded],
            Phase::Failed => vec![Signal::TurnFailed],
        }
    }
}

/// Every harness's source, reading the harnesses' files in `home`, and
/// Claude Code's and Junie's in their own folders, `claude` and `junie`,
/// which need not be in `home`.
pub fn all(home: &Path, claude: &Path, junie: &Path) -> Vec<Box<dyn Source>> {
    vec![
        Box::new(claude::Claude::new(claude)),
        Box::new(codex::Codex::new(home)),
        Box::new(grok::Grok::new(home)),
        Box::new(cursor::Cursor::new(home)),
        Box::new(junie::Junie::new(junie)),
        Box::new(opencode::OpenCode::new(home)),
        Box::new(pi::Pi::new(home)),
    ]
}

/// The source of `harness`, made as `all` makes it.
pub fn of(harness: Harness, home: &Path, claude: &Path, junie: &Path) -> Box<dyn Source> {
    all(home, claude, junie)
        .into_iter()
        .find(|source| source.harness() == harness)
        .expect("every harness has a source")
}

/// Seeds one source as the daemon does at start, handing it the hook-bound
/// sessions in `bound` that are its harness's.
pub fn seed(source: &mut dyn Source, k: &dyn Kernel, bound: &[(Key, Liveness)]) -> Result<Seed> {
    let harness = source.harness();
    let bound: Vec<(String, Liveness)> = bound
        .iter()
        .filter(|(key, _)| key.harness == harness)
        .map(|(key, liveness)| (key.id.clone(), liveness.clone()))
        .collect();
    source.seed(k, &bound)
}

/// What the daemon asks the sources to read.
pub enum Read<'a> {
    /// Daemon start, with the hook-bound sessions saved in the queue.
    Seed(&'a [(Key, Liveness)]),
    /// Files the watcher says changed, and whatever a budgeted read left
    /// unread, each tail read once.
    Paths(&'a [PathBuf]),
    /// Everything, because the watcher dropped events.
    All,
    Hook(&'a Hook),
    /// Whatever a budgeted read left unread.
    Behind,
    /// Roots that have appeared since they were last looked for, and then
    /// everything, as for `All`, if one has.
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
    /// The record kinds of each harness already logged as unknown.
    logged: HashSet<(Harness, String)>,
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
            logged: HashSet::new(),
        };
        sources.watch();
        Ok(sources)
    }

    /// What the sources say after `what`, and logs what they could not
    /// make sense of.
    pub fn read(&mut self, what: Read) -> Vec<Observation> {
        let read = self.gather(what);
        for said in self.strange() {
            eprintln!("agent-leds: {said}");
        }
        read
    }

    /// What each source's reads since it was last asked could not make
    /// sense of, to log: lines that are not records each time, and a kind
    /// it does not know the first time.
    fn strange(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        for source in &mut self.sources {
            let harness = source.harness();
            let mut strange = source.strange();
            strange
                .kinds
                .retain(|kind, _| self.logged.insert((harness, kind.clone())));
            said.extend(
                strange
                    .said(harness)
                    .map(|strange| format!("{}: {strange}", harness.title())),
            );
        }
        said
    }

    fn gather(&mut self, what: Read) -> Vec<Observation> {
        let kernel = &self.kernel;
        match what {
            Read::Seed(bound) => {
                let mut seeded = Vec::new();
                for source in &mut self.sources {
                    let title = source.harness().title();
                    match seed(source.as_mut(), kernel, bound) {
                        Ok(seed) => {
                            for unread in seed.unread {
                                eprintln!(
                                    "agent-leds: {title}: reading {}: {}",
                                    unread.path.display(),
                                    unread.error
                                );
                            }
                            seeded.extend(seed.observations);
                        }
                        Err(error) => eprintln!("agent-leds: {title}: {error:#}"),
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
                    // The paths come sorted and once each.
                    if let Some((canonical, root, index)) = root {
                        routed[*index]
                            .push(root.join(path.strip_prefix(canonical).expect("a prefix")));
                    }
                }
                let mut changed = Vec::new();
                for (source, mut paths) in self.sources.iter_mut().zip(routed) {
                    // What a read left behind is read on with what changed,
                    // each once.
                    for behind in source.behind() {
                        if !paths.contains(&behind) {
                            paths.push(behind);
                        }
                    }
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
                told.extend(self.gather(Read::Roots));
                told
            }
            Read::Behind => {
                let mut changed = Vec::new();
                for source in &mut self.sources {
                    let behind = source.behind();
                    if !behind.is_empty() {
                        changed.extend(source.changed(kernel, Some(&behind)));
                    }
                }
                changed
            }
            // Watching a new root restarts the watcher's stream for every
            // root, which drops what changed under any of them meanwhile.
            Read::Roots if self.watch() => self.gather(Read::All),
            Read::Roots => Vec::new(),
        }
    }

    /// Whether any source left bytes unread.
    pub fn behind(&self) -> bool {
        self.sources
            .iter()
            .any(|source| !source.behind().is_empty())
    }

    /// Starts watching each root that exists and is not watched yet, and
    /// says whether any was.
    fn watch(&mut self) -> bool {
        let mut gained = false;
        for (index, source) in self.sources.iter().enumerate() {
            let deep = source
                .roots()
                .into_iter()
                .map(|root| (root, RecursiveMode::Recursive));
            let shallow = source.shallow_roots().into_iter();
            let roots = deep.chain(shallow.map(|root| (root, RecursiveMode::NonRecursive)));
            for (root, mode) in roots {
                if self.watched.iter().any(|(_, watched, _)| *watched == root) {
                    continue;
                }
                let Ok(canonical) = root.canonicalize() else {
                    continue;
                };
                if let Err(error) = self.watcher.watch(&canonical, mode) {
                    eprintln!("agent-leds: watching {}: {error}", root.display());
                    continue;
                }
                self.watched.push((canonical, root, index));
                gained = true;
            }
        }
        gained
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::tail::{self, Tail};
    use super::*;
    use crate::files;

    /// What the watched source was handed: each path it was told changed,
    /// what each file held then, and every line read from the log.
    #[derive(Default)]
    struct Seen {
        paths: Vec<PathBuf>,
        texts: HashMap<PathBuf, String>,
        lines: usize,
        /// How often it was told to read everything.
        rescans: usize,
        /// What it is to say it could not make sense of.
        strange: Strange,
    }

    /// A source of one folder that reads whatever it is told changed, in
    /// its subfolders too unless it is `shallow`.
    struct Folder {
        root: PathBuf,
        shallow: bool,
        seen: Arc<Mutex<Seen>>,
        log: Option<Tail>,
        /// A file it says a read left behind.
        behind: Option<PathBuf>,
    }

    impl Source for Folder {
        fn harness(&self) -> Harness {
            Harness::Claude
        }

        fn roots(&self) -> Vec<PathBuf> {
            if self.shallow {
                Vec::new()
            } else {
                vec![self.root.clone()]
            }
        }

        fn shallow_roots(&self) -> Vec<PathBuf> {
            if self.shallow {
                vec![self.root.clone()]
            } else {
                Vec::new()
            }
        }

        fn seed(&mut self, _k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Seed> {
            Ok(Seed::default())
        }

        fn changed(&mut self, _k: &dyn Kernel, paths: Option<&[PathBuf]>) -> Vec<Observation> {
            let mut seen = self.seen.lock().unwrap();
            seen.rescans += usize::from(paths.is_none());
            for path in paths.unwrap_or_default() {
                seen.paths.push(path.clone());
                if path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    if self.log.is_none() {
                        self.log = Tail::at_start(path).ok();
                    }
                    let (mut behind, mut strange) = (false, Strange::default());
                    if let Ok(lines) = tail::follow(&mut self.log, path, &mut behind, &mut strange)
                    {
                        seen.lines += lines.len();
                    }
                } else if let Ok(text) = std::fs::read_to_string(path) {
                    seen.texts.insert(path.clone(), text);
                }
            }
            Vec::new()
        }

        fn hook(&mut self, _k: &dyn Kernel, _hook: &Hook) -> Vec<Observation> {
            Vec::new()
        }

        fn behind(&self) -> Vec<PathBuf> {
            self.behind.iter().cloned().collect()
        }

        fn strange(&mut self) -> Strange {
            std::mem::take(&mut self.seen.lock().unwrap().strange)
        }
    }

    /// Feeds the watcher's inputs to the sources until `done` holds of what
    /// the source has seen, or ten seconds pass.
    fn until(
        sources: &mut Sources,
        inputs: &mpsc::Receiver<Input>,
        seen: &Mutex<Seen>,
        done: impl Fn(&Seen) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if done(&seen.lock().unwrap()) {
                return true;
            }
            match inputs.recv_timeout(Duration::from_millis(200)) {
                Ok(Input::Files(paths)) => drop(sources.read(Read::Paths(&paths))),
                Ok(Input::Rescan) => drop(sources.read(Read::All)),
                _ => {}
            }
        }
        done(&seen.lock().unwrap())
    }

    /// FSEvents itself, on a folder in the temporary directory, which macOS
    /// spells `/var/folders/...` and FSEvents `/private/var/folders/...`.
    #[test]
    fn the_watcher_routes_real_file_system_events_to_the_source_that_reads_them() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().to_owned();
        assert_ne!(
            root,
            root.canonicalize().unwrap(),
            "a root FSEvents respells"
        );
        let seen = Arc::new(Mutex::new(Seen::default()));
        let source = Folder {
            root: root.clone(),
            shallow: false,
            seen: seen.clone(),
            log: None,
            behind: None,
        };
        let (sender, inputs) = mpsc::channel();
        let mut sources = Sources::start(vec![Box::new(source)], sender).unwrap();
        assert_eq!(sources.watched.len(), 1);

        // Written whole, through a temporary file renamed onto it.
        let status = root.join("1.json");
        files::replace(&status, "idle").unwrap();
        assert!(until(&mut sources, &inputs, &seen, |seen| {
            seen.texts.get(&status).map(String::as_str) == Some("idle")
        }));
        files::replace(&status, "busy").unwrap();
        assert!(until(&mut sources, &inputs, &seen, |seen| {
            seen.texts.get(&status).map(String::as_str) == Some("busy")
        }));
        // Every path the source was handed is in its root as it spells it.
        assert!(
            seen.lock()
                .unwrap()
                .paths
                .iter()
                .all(|path| path.starts_with(&root))
        );

        // Created and appended to in one burst, faster than FSEvents reports.
        let log = root.join("log.jsonl");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        for line in 0..200 {
            writeln!(file, "{{\"line\": {line}}}").unwrap();
        }
        drop(file);
        assert!(until(&mut sources, &inputs, &seen, |seen| seen.lines == 200));
    }

    /// A shallow root's own files are read, and nothing in its subfolders,
    /// however busy, is handed to the source.
    #[test]
    fn a_shallow_root_reads_only_its_own_files() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().to_owned();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let source = Folder {
            root: root.clone(),
            shallow: true,
            seen: seen.clone(),
            log: None,
            behind: None,
        };
        let (sender, inputs) = mpsc::channel();
        let mut sources = Sources::start(vec![Box::new(source)], sender).unwrap();
        let build = root.join("worktrees/app/target");
        std::fs::create_dir_all(&build).unwrap();
        for n in 0..50 {
            std::fs::write(build.join(format!("{n}.o")), "x").unwrap();
        }
        let goals = root.join("goals_1.sqlite-wal");
        std::fs::write(&goals, "wal").unwrap();
        assert!(until(&mut sources, &inputs, &seen, |seen| {
            seen.texts.contains_key(&goals)
        }));
        let seen = seen.lock().unwrap();
        assert!(seen.paths.iter().all(|path| path.parent() == Some(&root)));
    }

    /// A kind a source does not know is logged the first time it is read,
    /// and lines that are not records each time.
    #[test]
    fn what_a_live_read_cannot_make_sense_of_is_logged() {
        let folder = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let source = Folder {
            root: folder.path().to_owned(),
            shallow: false,
            seen: seen.clone(),
            log: None,
            behind: None,
        };
        let (sender, _inputs) = mpsc::channel();
        let mut sources = Sources::start(vec![Box::new(source)], sender).unwrap();
        let count = |unparsed: usize| {
            let mut seen = seen.lock().unwrap();
            seen.strange.unparsed += unparsed;
            seen.strange.kind("turn-summary", &[]);
        };
        count(0);
        assert_eq!(
            sources.strange(),
            ["Claude Code: records of kinds Claude Code was not seen to write: turn-summary (1)"]
        );
        count(1);
        assert_eq!(
            sources.strange(),
            ["Claude Code: 1 line is not a record it reads"]
        );
        assert!(sources.strange().is_empty());
    }

    #[test]
    fn a_root_that_appears_has_every_source_read_again() {
        let folder = tempfile::tempdir().unwrap();
        let (early, late) = (folder.path().join("early"), folder.path().join("late"));
        std::fs::create_dir(&early).unwrap();
        let folders = [early, late.clone()].map(|root| Folder {
            root,
            shallow: false,
            seen: Arc::new(Mutex::new(Seen::default())),
            log: None,
            behind: None,
        });
        let seen: Vec<Arc<Mutex<Seen>>> = folders.iter().map(|f| f.seen.clone()).collect();
        let (sender, _inputs) = mpsc::channel();
        let boxed = folders.map(|f| Box::new(f) as Box<dyn Source>);
        let mut sources = Sources::start(boxed.into(), sender).unwrap();
        let rescans = || {
            seen.iter()
                .map(|s| s.lock().unwrap().rescans)
                .collect::<Vec<_>>()
        };
        sources.read(Read::Roots);
        assert_eq!(rescans(), [0, 0]);
        std::fs::create_dir(&late).unwrap();
        sources.read(Read::Roots);
        assert_eq!(rescans(), [1, 1]);
    }

    #[test]
    fn a_source_left_behind_reads_on_with_what_changed_and_nothing_else() {
        let folder = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let behind = folder.path().join("log.jsonl");
        let source = Folder {
            root: folder.path().to_owned(),
            shallow: false,
            seen: seen.clone(),
            log: None,
            behind: Some(behind.clone()),
        };
        let (sender, _inputs) = mpsc::channel();
        let mut sources = Sources::start(vec![Box::new(source)], sender).unwrap();
        let changed = folder.path().canonicalize().unwrap().join("1.json");
        sources.read(Read::Paths(&[changed]));
        sources.read(Read::Behind);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.rescans, 0);
        assert_eq!(
            seen.paths,
            [folder.path().join("1.json"), behind.clone(), behind]
        );
    }
}
