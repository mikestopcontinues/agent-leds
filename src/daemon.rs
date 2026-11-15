//! The service: owns the queue and the card, follows the harnesses' files
//! and hooks through their sources, and answers the command line. What the
//! lights play is the player's call; everything the daemon reads or changes
//! outside itself goes through a `World`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, PALETTES, home, state_dir};
use crate::desktop::Desktop;
use crate::device;
use crate::eject;
use crate::files;
use crate::fsm::{self, Channel, Signal};
use crate::harness::{self, Harness};
use crate::ipc::{self, Overlap, Request, Status, View};
use crate::liveness::{self, Liveness};
use crate::player::Player;
use crate::queue::{Change, Expiry, Queue};
use crate::render::{self, Step};
use crate::source::{self, Observation, Read, Sources};
use crate::state::State;

fn queue_path() -> PathBuf {
    state_dir().join("queue.json")
}

/// The queue as last saved, so a restart keeps what is unread and in progress.
fn saved_queue() -> Queue {
    let saved = match files::read_optional(&queue_path()) {
        Ok(Some(saved)) => saved,
        Ok(None) => return Queue::default(),
        Err(error) => {
            eprintln!("agent-leds: ignoring the saved queue: {error:#}");
            return Queue::default();
        }
    };
    serde_json::from_str(&saved).unwrap_or_else(|error| {
        eprintln!("agent-leds: ignoring the saved queue: {error}");
        Queue::default()
    })
}

/// How often to read the device's uptime, which also keeps the SD reader awake.
const DEVICE_CHECK: Duration = Duration::from_secs(15);
/// How often to look for whether the user has read a session, while one is
/// unread, and whether the processes of the tracked sessions still run.
const LOOK: Duration = Duration::from_secs(3);
/// How long a check that falls in the middle of an animation waits before trying again.
const ANIMATION_CHECK: Duration = Duration::from_secs(1);
/// How long a shown scene holds the queue it leaves before the change plays.
const SHOW_LEAD_MS: u64 = 400;

/// What the daemon needs from outside itself: the card, the clocks, the
/// desktop app, the harnesses' sources and processes, and somewhere to keep
/// its queue and settings.
trait World {
    /// Where the card is mounted, if it is.
    fn card(&self) -> Option<PathBuf>;
    fn write(&mut self, program: &str) -> io::Result<()>;
    fn uptime_ms(&mut self) -> io::Result<u64>;
    /// The sessions the user has looked at since `since`, with when.
    fn looked_at(&mut self, since: u64, now: u64) -> HashMap<String, u64>;
    /// The wall clock, in Unix milliseconds.
    fn now_ms(&self) -> u64;
    /// The monotonic clock that times the lights.
    fn instant(&self) -> Instant;
    fn save_queue(&mut self, queue: &Queue);
    fn save_config(&mut self, config: &Config) -> Result<()>;
    /// Whether the binary the daemon was started from has been replaced,
    /// as an upgrade does.
    fn replaced(&self) -> bool;
    /// What the harnesses' sources read.
    fn read(&mut self, what: Read) -> Vec<Observation>;
    /// Whether the process behind a session still runs.
    fn alive(&mut self, liveness: &Liveness) -> bool;
    /// Whether a source left bytes unread at its read budget.
    fn behind(&self) -> bool;
}

/// The Mac the daemon runs on.
struct System {
    desktop: Desktop,
    exe: PathBuf,
    /// The file `exe` led to when the daemon started.
    started_from: Option<Binary>,
    sources: Sources,
}

/// Which file a path leads to, and when it was last written.
#[derive(PartialEq)]
struct Binary {
    device: u64,
    inode: u64,
    modified: SystemTime,
}

impl Binary {
    /// The file at `path`, after any symlinks, such as the one Homebrew
    /// points at each version in turn.
    fn at(path: &Path) -> Option<Binary> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Binary {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: metadata.modified().ok()?,
        })
    }
}

impl World for System {
    fn card(&self) -> Option<PathBuf> {
        device::volume()
    }

    fn write(&mut self, program: &str) -> io::Result<()> {
        device::write(program)
    }

    fn uptime_ms(&mut self) -> io::Result<u64> {
        device::uptime_ms()
    }

    fn looked_at(&mut self, since: u64, now: u64) -> HashMap<String, u64> {
        self.desktop.looked_at(since, now)
    }

    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }

    fn save_queue(&mut self, queue: &Queue) {
        let saved = serde_json::to_string(queue).expect("a queue is plain data");
        if let Err(error) = files::replace(&queue_path(), &saved) {
            eprintln!("agent-leds: could not save the queue: {error:#}");
        }
    }

    fn save_config(&mut self, config: &Config) -> Result<()> {
        config.save()
    }

    fn replaced(&self) -> bool {
        // A binary that is gone, as during an uninstall, has no new version
        // to restart into.
        let now = Binary::at(&self.exe);
        now.is_some() && now != self.started_from
    }

    fn read(&mut self, what: Read) -> Vec<Observation> {
        self.sources.read(what)
    }

    fn alive(&mut self, liveness: &Liveness) -> bool {
        liveness::alive(&liveness::System, liveness)
    }

    fn behind(&self) -> bool {
        self.sources.behind()
    }
}

struct Daemon<W: World> {
    world: W,
    config: Config,
    queue: Queue,
    player: Player,
    /// The device's last uptime, or None while it is missing or needs a rewrite.
    uptime: Option<u64>,
    next_check: Instant,
    /// When to look next for whether the user has read a session, and
    /// whether the tracked sessions' processes still run.
    next_look: Instant,
    /// The binary was replaced, so the daemon should exit and let launchd
    /// start the new one.
    replaced: bool,
    /// How many observations were dropped because their harness does not
    /// declare them.
    undeclared: u64,
    /// The undeclared kinds already logged, each logged once.
    logged: HashSet<(Harness, Signal, Channel)>,
    /// How often each harness told each set of signals that overlap, which
    /// the table resolved by its order. Each set is logged the first time.
    overlaps: BTreeMap<(Harness, BTreeSet<Signal>), u64>,
}

/// What wakes the daemon: a request on the socket, files the watcher says
/// changed, or word that the watcher dropped events.
pub enum Input {
    Request(Request, Sender<Result<Value>>),
    Files(Vec<PathBuf>),
    Rescan,
}

pub fn run() -> Result<()> {
    let (inputs, inbox) = mpsc::channel();
    listen(inputs.clone())?;
    thread::spawn(eject::guard);
    let exe = std::env::current_exe()?;
    let sources = Sources::start(source::all(&home(), &state_dir()), inputs)?;
    let world = System {
        desktop: Desktop::default(),
        started_from: Binary::at(&exe),
        exe,
        sources,
    };
    let mut daemon = Daemon::new(world, Config::load()?, saved_queue());
    while !daemon.replaced {
        let wait = daemon
            .deadline()
            .saturating_duration_since(daemon.world.instant());
        let first = match inbox.recv_timeout(wait) {
            Ok(input) => Some(input),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => bail!("the socket listener stopped"),
        };
        let inputs: Vec<Input> = first.into_iter().chain(inbox.try_iter()).collect();
        daemon.take(inputs);
        daemon.tick();
    }
    // The queue is saved with every change, and the new daemon rewrites the
    // card when it first checks it.
    eprintln!("agent-leds: the binary was replaced; exiting for launchd to start the new one");
    Ok(())
}

/// Listens on the socket, handing each request to the main loop with a way to answer it.
fn listen(requests: Sender<Input>) -> Result<()> {
    let path = ipc::socket_path();
    std::fs::create_dir_all(state_dir())?;
    if UnixStream::connect(&path).is_ok() {
        bail!("an agent-leds daemon is already running");
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = requests.clone();
            thread::spawn(move || {
                ipc::serve(stream, |request| {
                    let (answer, answered) = mpsc::channel();
                    requests
                        .send(Input::Request(request, answer))
                        .context("the daemon is shutting down")?;
                    answered.recv().context("the daemon is shutting down")?
                })
            });
        }
    });
    Ok(())
}

/// How long to show something that was asked for, in milliseconds.
fn held_ms(seconds: f64) -> Result<u64> {
    if !(seconds > 0.0 && seconds <= 3600.0) {
        bail!("show it for between 0 and 3600 seconds");
    }
    Ok((seconds * 1000.0) as u64)
}

impl<W: World> Daemon<W> {
    /// The daemon at startup: the queue is what the harnesses' files show
    /// now, keeping what was saved where it is still about the same turn.
    /// Nothing is written until the first check.
    fn new(world: W, config: Config, saved: Queue) -> Daemon<W> {
        let now = world.instant();
        let mut daemon = Daemon {
            world,
            config,
            queue: Queue::default(),
            player: Player::default(),
            uptime: None,
            next_check: now,
            next_look: now,
            replaced: false,
            undeclared: 0,
            logged: HashSet::new(),
            overlaps: BTreeMap::new(),
        };
        let seeded = daemon.world.read(Read::Seed(&saved.bound()));
        for o in seeded {
            daemon.apply(&o);
        }
        daemon.queue = Queue::restore(saved, std::mem::take(&mut daemon.queue));
        daemon
    }

    /// When the loop must next wake by itself.
    fn deadline(&self) -> Instant {
        [
            Some(self.next_check),
            self.player.deadline(),
            // Looking is only needed while something is unread, or while a
            // session's process may end without a word.
            (self.queue.unread_since().is_some() || self.queue.tracked()).then_some(self.next_look),
            // A source left bytes to read.
            self.world.behind().then(|| self.world.instant()),
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("a check is always due")
    }

    /// Handles what arrived since the loop last woke. The files are read
    /// first, so a hook or a status request sees them as they are now.
    fn take(&mut self, inputs: Vec<Input>) {
        let mut files = Vec::new();
        let mut rescan = false;
        let mut requests = Vec::new();
        for input in inputs {
            match input {
                Input::Files(paths) => files.extend(paths),
                Input::Rescan => rescan = true,
                Input::Request(request, reply) => requests.push((request, reply)),
            }
        }
        if rescan {
            let read = self.world.read(Read::All);
            self.observe(read);
        } else if !files.is_empty() {
            files.sort();
            files.dedup();
            let read = self.world.read(Read::Paths(&files));
            self.observe(read);
        }
        for (request, reply) in requests {
            let _ = reply.send(self.handle(request));
        }
        if self.world.behind() {
            let read = self.world.read(Read::Behind);
            self.observe(read);
        }
    }

    fn handle(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Hook(hook) => {
                let told = self.world.read(Read::Hook(&hook));
                self.observe(told);
                Ok(Value::Null)
            }
            Request::Status => Ok(json!(Status {
                version: env!("CARGO_PKG_VERSION").into(),
                items: self
                    .queue
                    .items()
                    .into_iter()
                    .filter_map(View::of)
                    .collect(),
                now: self.world.now_ms(),
                config: self.config.clone(),
                device: self.world.card(),
                device_uptime_ms: self.world.uptime_ms().ok(),
                undeclared: self.undeclared,
                overlaps: self
                    .overlaps
                    .iter()
                    .map(|((harness, signals), &count)| Overlap {
                        harness: *harness,
                        signals: signals.clone(),
                        count,
                    })
                    .collect(),
            })),
            Request::Dismiss { session } => {
                let dismissed = self.queue.dismissed(session.as_deref());
                self.observe(dismissed);
                Ok(Value::Null)
            }
            Request::Palette { name } => {
                let named = PALETTES.iter().find(|palette| palette.name == name);
                let colors = named.with_context(|| format!("no palette named {name:?}"))?;
                self.configure(Config {
                    colors: colors.colors,
                    ..self.config.clone()
                })
            }
            Request::Configure { config } => self.configure(config),
            Request::Play { program, seconds } => {
                device::check(&program)?;
                self.hold(vec![Step {
                    program,
                    ms: held_ms(seconds)?,
                }])
            }
            Request::Show {
                scene,
                config,
                seconds,
            } => {
                config.check()?;
                let shown = scene.render(&config);
                // The change starts from the queue it leaves, and ends held on the one it reaches.
                let lead = Step {
                    program: render::steady(&shown.from, &config),
                    ms: SHOW_LEAD_MS,
                };
                let held = Step {
                    program: render::steady(&shown.slots, &config),
                    ms: held_ms(seconds)?,
                };
                self.hold([vec![lead], shown.steps, vec![held]].concat())
            }
        }
    }

    /// Applies one observation, if its harness declares every signal in
    /// it. One that is not declared is dropped, counted, and logged once per
    /// kind: a source produced something its harness does not claim. One
    /// whose signals overlap is applied as the table's order says, counted,
    /// and logged once per set: a source told what it should not together,
    /// as when a harness's format changes.
    fn apply(&mut self, o: &Observation) -> Change {
        let harness = o.key.harness;
        let signals = o.event.signals();
        if o.channel != Channel::Daemon {
            let undeclared: Vec<Signal> = signals
                .iter()
                .copied()
                .filter(|&signal| !harness::declares(harness, signal, o.channel))
                .collect();
            if !undeclared.is_empty() {
                self.undeclared += 1;
                for signal in undeclared {
                    if self.logged.insert((harness, signal, o.channel)) {
                        eprintln!(
                            "agent-leds: {} produced {signal:?} through {:?}, which it does not declare",
                            harness.title(),
                            o.channel
                        );
                    }
                }
                return Change::None;
            }
        }
        let from = self.queue.phase(&o.key);
        if fsm::overlaps(from, &signals) {
            let count = self.overlaps.entry((harness, signals.clone())).or_default();
            if *count == 0 {
                let winner = fsm::next(from, &signals).expect("an overlap has a winner");
                eprintln!(
                    "agent-leds: {} told {signals:?} together, which overlap; from {from:?}, {:?} wins",
                    harness.title(),
                    winner.on
                );
            }
            *count += 1;
        }
        self.queue.apply(o, self.world.now_ms())
    }

    /// Applies a batch of observations, then saves and shows the result
    /// once: the most urgent state any of them announces, or else the wave.
    fn observe(&mut self, observations: Vec<Observation>) {
        let mut wants: Option<State> = None;
        let (mut save, mut settle) = (false, false);
        for o in observations {
            match self.apply(&o) {
                Change::None => {}
                Change::Saved => save = true,
                Change::Settle => {
                    save = true;
                    settle = true;
                }
                Change::Announce(state) => {
                    save = true;
                    wants = Some(wants.map_or(state, |wanted| wanted.min(state)));
                }
            }
        }
        if let Some(state) = wants {
            if matches!(state, State::Stopped | State::Question) {
                // A session whose turn ends while the user is looking at it is read already.
                self.mark_read();
            }
            self.save_queue();
            let slots = self.slots();
            let now = self.world.instant();
            let program = self.player.announce(state, &slots, &self.config, now);
            self.write(program);
        } else if settle {
            self.save_queue();
            self.settle();
        } else if save {
            self.save_queue();
        }
    }

    fn configure(&mut self, config: Config) -> Result<Value> {
        config.check()?;
        self.world.save_config(&config)?;
        self.config = config;
        let program = self.player.refresh(&self.slots(), &self.config);
        self.write(program);
        Ok(json!(self.config))
    }

    /// Shows programs that are not the queue; the queue returns when the last one ends.
    fn hold(&mut self, steps: Vec<Step>) -> Result<Value> {
        if self.world.card().is_none() {
            bail!("no SidePulse volume is mounted");
        }
        let now = self.world.instant();
        let program = self.player.hold(steps, &self.slots(), &self.config, now);
        self.write(program);
        Ok(Value::Null)
    }

    fn tick(&mut self) {
        let now = self.world.instant();
        let program = self.player.advance(&self.slots(), &self.config, now);
        self.write(program);
        if self.next_look <= now {
            self.next_look = now + LOOK;
            // Looking runs a system tool, which could delay an animation's next program.
            if !self.player.animating() {
                let looked = self.looks();
                self.observe(looked);
            }
            // A session whose process ended, as when its terminal was closed
            // or it crashed, ends even when nothing said so.
            let swept = self.queue.sweep(|liveness| self.world.alive(liveness));
            self.observe(swept);
        }
        if self.next_check <= now {
            // A check reads the card and the desktop app's files, which can
            // take long enough to delay the next program of an animation.
            if self.player.animating() {
                self.next_check = now + ANIMATION_CHECK;
                return;
            }
            self.next_check = now + DEVICE_CHECK;
            self.check();
        }
    }

    /// Lets the lights that no longer mean anything go, looks for roots
    /// that have appeared, and rewrites the queue if the card restarted.
    fn check(&mut self) {
        let now = self.world.now_ms();
        let expiry = Expiry {
            keep_read_ms: u64::from(self.config.keep_read_minutes) * 60_000,
            abandon_ms: u64::from(self.config.abandon_days) * 24 * 60 * 60_000,
        };
        let due = self.queue.due(now, expiry);
        self.observe(due);
        let found = self.world.read(Read::Roots);
        self.observe(found);
        match self.world.uptime_ms() {
            Ok(uptime) => {
                let restarted = self.uptime.is_none_or(|last| uptime < last);
                self.uptime = Some(uptime);
                if restarted {
                    // If this write fails it clears the uptime, so the next check tries again.
                    let program = self.player.steady(&self.slots(), &self.config);
                    self.write(Some(program));
                }
            }
            Err(_) => self.uptime = None,
        }
        self.replaced = self.world.replaced();
    }

    /// The sessions the user has looked at since their turn ended, while any is unread.
    fn looks(&mut self) -> Vec<Observation> {
        let Some(since) = self.queue.unread_since() else {
            return Vec::new();
        };
        let now = self.world.now_ms();
        let looked_at = self.world.looked_at(since, now);
        self.queue.looked(&looked_at)
    }

    /// Marks the sessions the user has looked at as read, without showing anything.
    fn mark_read(&mut self) {
        let now = self.world.now_ms();
        for o in self.looks() {
            self.queue.apply(&o, now);
        }
    }

    fn save_queue(&mut self) {
        self.world.save_queue(&self.queue);
    }

    fn slots(&self) -> Vec<State> {
        let states: Vec<State> = self
            .queue
            .items()
            .iter()
            .filter_map(|session| session.phase.state())
            .collect();
        render::slots(&states)
    }

    /// Shows a change that asks nothing of the user.
    fn settle(&mut self) {
        let now = self.world.instant();
        let program = self.player.settle(&self.slots(), &self.config, now);
        self.write(program);
    }

    fn write(&mut self, program: Option<String>) {
        let Some(program) = program else {
            return;
        };
        if let Err(error) = self.world.write(&program) {
            if self.uptime.is_some() {
                eprintln!("agent-leds: could not write to the device: {error}");
            }
            // The next device check rewrites the queue once the device is back.
            self.uptime = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;

    use super::*;
    use crate::fsm::Phase;
    use crate::liveness::StartCheck;
    use crate::queue::Key;
    use crate::source::{Event, Hook, Start};

    /// A world the test controls: a card that records what is written to
    /// it, clocks that move only when told, sources that find what the test
    /// says, and a user who has looked at what the test says.
    struct Fake {
        card: bool,
        written: Vec<String>,
        uptime: io::Result<u64>,
        looked_at: HashMap<String, u64>,
        now_ms: u64,
        instant: Instant,
        saved_queues: usize,
        saved_config: Option<Config>,
        replaced: bool,
        /// What a seed finds.
        seed: Vec<Observation>,
        /// What the next reads of files and hooks find, in turn.
        reads: VecDeque<Vec<Observation>>,
        /// The reads asked for, in order.
        asked: Vec<&'static str>,
        dead: HashSet<Liveness>,
        behind: bool,
    }

    impl World for Fake {
        fn card(&self) -> Option<PathBuf> {
            self.card.then(|| "/Volumes/SidePulse".into())
        }

        fn write(&mut self, program: &str) -> io::Result<()> {
            if !self.card {
                return Err(io::ErrorKind::NotFound.into());
            }
            self.written.push(program.to_owned());
            Ok(())
        }

        fn uptime_ms(&mut self) -> io::Result<u64> {
            match &self.uptime {
                Ok(uptime) => Ok(*uptime),
                Err(error) => Err(io::Error::new(error.kind(), "unreadable")),
            }
        }

        fn looked_at(&mut self, _since: u64, _now: u64) -> HashMap<String, u64> {
            self.looked_at.clone()
        }

        fn now_ms(&self) -> u64 {
            self.now_ms
        }

        fn instant(&self) -> Instant {
            self.instant
        }

        fn save_queue(&mut self, _queue: &Queue) {
            self.saved_queues += 1;
        }

        fn save_config(&mut self, config: &Config) -> Result<()> {
            self.saved_config = Some(config.clone());
            Ok(())
        }

        fn replaced(&self) -> bool {
            self.replaced
        }

        fn read(&mut self, what: Read) -> Vec<Observation> {
            let (asked, found) = match what {
                Read::Seed(_) => ("seed", std::mem::take(&mut self.seed)),
                Read::Roots => ("roots", Vec::new()),
                Read::Paths(_) => ("paths", self.reads.pop_front().unwrap_or_default()),
                Read::All => ("all", self.reads.pop_front().unwrap_or_default()),
                Read::Hook(_) => ("hook", self.reads.pop_front().unwrap_or_default()),
                Read::Behind => ("behind", self.reads.pop_front().unwrap_or_default()),
            };
            self.asked.push(asked);
            found
        }

        fn alive(&mut self, liveness: &Liveness) -> bool {
            !self.dead.contains(liveness)
        }

        fn behind(&self) -> bool {
            self.behind
        }
    }

    fn world() -> Fake {
        Fake {
            card: true,
            written: Vec::new(),
            uptime: Ok(1_000),
            looked_at: HashMap::new(),
            now_ms: 1_000_000,
            instant: Instant::now(),
            saved_queues: 0,
            saved_config: None,
            replaced: false,
            seed: Vec::new(),
            reads: VecDeque::new(),
            asked: Vec::new(),
            dead: HashSet::new(),
            behind: false,
        }
    }

    fn daemon() -> Daemon<Fake> {
        Daemon::new(world(), Config::default(), Queue::default())
    }

    fn process(session: &str) -> Liveness {
        Liveness::Process {
            pid: session.len() as i32,
            start: StartCheck::Second(1),
        }
    }

    fn observation(session: &str, event: Event) -> Observation {
        Observation {
            key: Key {
                harness: Harness::Claude,
                id: session.into(),
            },
            channel: Channel::File,
            event,
            turn: None,
            quiet: false,
        }
    }

    fn started(session: &str) -> Observation {
        observation(
            session,
            Event::Started(Start {
                cwd: "/Users/me/Code/app".into(),
                liveness: process(session),
            }),
        )
    }

    /// What Claude Code's files say of a session in one batch of changes:
    /// that it runs, then `signals`.
    fn tell(daemon: &mut Daemon<Fake>, session: &str, signals: &[Signal]) {
        let mut found = vec![started(session)];
        found.extend(
            signals
                .iter()
                .map(|&signal| observation(session, Event::one(signal))),
        );
        daemon.world.reads.push_back(found);
        daemon.take(vec![Input::Files(vec!["/Users/me/.claude".into()])]);
    }

    /// Lets the clocks run on until whatever is playing has ended.
    fn play_out(daemon: &mut Daemon<Fake>) {
        while let Some(at) = daemon.player.deadline() {
            daemon.world.instant = at;
            daemon.tick();
        }
    }

    fn steady(daemon: &Daemon<Fake>) -> String {
        render::steady(&daemon.slots(), &daemon.config)
    }

    fn states(daemon: &Daemon<Fake>) -> Vec<State> {
        daemon
            .queue
            .items()
            .iter()
            .filter_map(|session| session.phase.state())
            .collect()
    }

    fn status(daemon: &mut Daemon<Fake>) -> Status {
        serde_json::from_value(daemon.handle(Request::Status).unwrap()).unwrap()
    }

    #[test]
    fn a_session_that_stops_is_announced_and_the_queue_follows() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::TurnEnded]);
        let snake = render::change(Some(State::Stopped), &[], &[State::Stopped], &daemon.config);
        assert_eq!(daemon.world.written, [snake[0].program.clone()]);
        assert_eq!(daemon.world.saved_queues, 1);
        play_out(&mut daemon);
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
    }

    #[test]
    fn a_session_that_goes_to_work_settles_in_without_a_snake() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::Prompted]);
        let wave = render::change(None, &[], &[State::Working], &daemon.config);
        assert_eq!(daemon.world.written, [wave[0].program.clone()]);
        // Being told again changes nothing, and writes and saves nothing.
        tell(&mut daemon, "s1", &[Signal::Prompted]);
        assert_eq!(daemon.world.written.len(), 1);
        assert_eq!(daemon.world.saved_queues, 1);
    }

    #[test]
    fn a_batch_that_starts_and_ends_a_turn_plays_one_snake() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::Prompted, Signal::TurnEnded]);
        let snake = render::change(Some(State::Stopped), &[], &[State::Stopped], &daemon.config);
        assert_eq!(daemon.world.written, [snake[0].program.clone()]);
        assert_eq!(daemon.world.saved_queues, 1);
    }

    #[test]
    fn a_turn_that_ends_while_the_user_is_looking_is_read_already() {
        let mut daemon = daemon();
        daemon.world.looked_at = HashMap::from([("s1".to_owned(), daemon.world.now_ms)]);
        tell(&mut daemon, "s1", &[Signal::Prompted, Signal::TurnEnded]);
        let now = daemon.world.now_ms;
        assert_eq!(status(&mut daemon).items[0].read, Some(now));
    }

    #[test]
    fn startup_writes_nothing_until_the_first_check() {
        let mut world = world();
        let mut seeded = vec![
            started("s1"),
            observation("s1", Event::one(Signal::TurnEnded)),
        ];
        for o in &mut seeded {
            o.quiet = true;
        }
        world.seed = seeded;
        let mut daemon = Daemon::new(world, Config::default(), Queue::default());
        assert_eq!(daemon.world.asked, ["seed"]);
        assert!(daemon.world.written.is_empty());
        assert_eq!(states(&daemon), [State::Stopped]);
        daemon.check();
        assert_eq!(daemon.world.written, [steady(&daemon)]);
    }

    #[test]
    fn a_card_that_restarted_gets_the_queue_again() {
        let mut daemon = daemon();
        daemon.check();
        assert_eq!(daemon.world.written, [steady(&daemon)]);
        // The uptime went on, so nothing needs writing.
        daemon.world.uptime = Ok(20_000);
        daemon.check();
        assert_eq!(daemon.world.written.len(), 1);
        // It went back: the card restarted and forgot its program.
        daemon.world.uptime = Ok(5);
        daemon.check();
        assert_eq!(daemon.world.written.len(), 2);
    }

    #[test]
    fn a_card_that_went_away_is_written_again_once_it_is_back() {
        let mut daemon = daemon();
        daemon.check();
        daemon.world.card = false;
        daemon.world.uptime = Err(io::ErrorKind::NotFound.into());
        tell(&mut daemon, "s1", &[Signal::Prompted]);
        assert_eq!(daemon.uptime, None);
        daemon.world.card = true;
        daemon.world.uptime = Ok(30_000);
        play_out(&mut daemon);
        daemon.check();
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
    }

    #[test]
    fn checks_wait_while_a_change_is_playing() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::TurnEnded]);
        let checks = daemon.next_check;
        daemon.tick();
        assert!(daemon.next_check >= checks + ANIMATION_CHECK);
        assert_eq!(daemon.world.written.len(), 1);
    }

    #[test]
    fn showing_something_needs_the_card() {
        let mut daemon = daemon();
        daemon.world.card = false;
        let play = Request::Play {
            program: "#FFFFFF".into(),
            seconds: 1.0,
        };
        let error = daemon.handle(play).unwrap_err();
        assert!(error.to_string().contains("no SidePulse volume"));
        daemon.world.card = true;
        let too_long = Request::Play {
            program: "#FFFFFF".into(),
            seconds: 4000.0,
        };
        assert!(daemon.handle(too_long).is_err());
        let play = Request::Play {
            program: "#FFFFFF".into(),
            seconds: 1.0,
        };
        daemon.handle(play).unwrap();
        assert_eq!(daemon.world.written, ["#FFFFFF"]);
        daemon.world.instant += Duration::from_secs(1);
        daemon.tick();
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
    }

    #[test]
    fn settings_are_checked_saved_and_shown() {
        let mut daemon = daemon();
        let config = Config {
            front_led: 3,
            ..Config::default()
        };
        assert!(daemon.handle(Request::Configure { config }).is_err());
        assert_eq!(daemon.world.saved_config, None);
        let palette = Request::Palette {
            name: "signal".into(),
        };
        daemon.handle(palette).unwrap();
        let saved = daemon.world.saved_config.clone().unwrap();
        assert_eq!(saved.colors, PALETTES[1].colors);
        assert_eq!(daemon.world.written, [steady(&daemon)]);
        let unknown = Request::Palette {
            name: "plaid".into(),
        };
        assert!(daemon.handle(unknown).is_err());
    }

    #[test]
    fn dismissing_clears_what_is_not_working_through_the_table() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::TurnEnded]);
        tell(&mut daemon, "s2", &[Signal::Prompted]);
        play_out(&mut daemon);
        daemon.handle(Request::Dismiss { session: None }).unwrap();
        play_out(&mut daemon);
        assert_eq!(states(&daemon), [State::Working]);
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
        // The dismissed session is still followed, and its next turn shows.
        tell(&mut daemon, "s1", &[Signal::Prompted]);
        assert_eq!(states(&daemon), [State::Working, State::Working]);
    }

    #[test]
    fn a_read_session_leaves_after_keep_read_minutes() {
        let mut daemon = daemon();
        daemon.world.looked_at = HashMap::from([("s1".to_owned(), daemon.world.now_ms)]);
        tell(&mut daemon, "s1", &[Signal::TurnEnded]);
        play_out(&mut daemon);
        daemon.world.now_ms += u64::from(daemon.config.keep_read_minutes) * 60_000;
        daemon.check();
        assert!(states(&daemon).is_empty());
        assert!(daemon.queue.tracked());
    }

    #[test]
    fn a_dead_process_clears_its_light_at_the_next_look() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::Prompted]);
        tell(&mut daemon, "s22", &[Signal::Prompted]);
        play_out(&mut daemon);
        daemon.world.dead.insert(process("s1"));
        daemon.world.instant = daemon.next_look;
        daemon.tick();
        play_out(&mut daemon);
        let left: Vec<&str> = daemon
            .queue
            .items()
            .iter()
            .map(|session| session.key.id.as_str())
            .collect();
        assert_eq!(left, ["s22"]);
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
    }

    #[test]
    fn an_undeclared_observation_is_dropped_and_counted() {
        let mut daemon = daemon();
        // Claude Code declares nothing through a hook.
        let mut hooked = started("s1");
        hooked.channel = Channel::Hook;
        daemon.world.reads.push_back(vec![hooked.clone(), hooked]);
        daemon.take(vec![Input::Files(vec!["/Users/me/.claude".into()])]);
        assert!(!daemon.queue.tracked());
        assert_eq!(status(&mut daemon).undeclared, 2);
        assert_eq!(daemon.logged.len(), 1);
    }

    #[test]
    fn an_observation_with_one_undeclared_signal_is_dropped_whole() {
        let mut daemon = daemon();
        // Cursor declares prompts through its hook, and no waits.
        let cursor = |event| Observation {
            key: Key {
                harness: Harness::Cursor,
                id: "c1".into(),
            },
            channel: Channel::Hook,
            event,
            turn: None,
            quiet: false,
        };
        let mut start = started("c1");
        start.key.harness = Harness::Cursor;
        start.channel = Channel::Hook;
        let told = BTreeSet::from([Signal::Prompted, Signal::PermissionAsked]);
        daemon
            .world
            .reads
            .push_back(vec![start, cursor(Event::Signals(told))]);
        daemon.take(vec![Input::Rescan]);
        assert!(states(&daemon).is_empty());
        assert_eq!(status(&mut daemon).undeclared, 1);
        assert_eq!(
            daemon.logged,
            HashSet::from([(Harness::Cursor, Signal::PermissionAsked, Channel::Hook)])
        );
    }

    /// What Claude Code's files say of a session in one batch: that it
    /// runs, then one record that tells `signals` together.
    fn tell_together(daemon: &mut Daemon<Fake>, session: &str, signals: &[Signal]) {
        let told = signals.iter().copied().collect();
        daemon.world.reads.push_back(vec![
            started(session),
            observation(session, Event::one(Signal::Prompted)),
            observation(session, Event::Signals(told)),
        ]);
        daemon.take(vec![Input::Files(vec!["/Users/me/.claude".into()])]);
    }

    #[test]
    fn signals_of_one_family_told_together_are_not_counted() {
        let mut daemon = daemon();
        tell_together(
            &mut daemon,
            "s1",
            &[
                Signal::TurnEnded,
                Signal::TurnEndedAsking,
                Signal::Backgrounded,
            ],
        );
        assert_eq!(states(&daemon), [State::Question]);
        assert!(status(&mut daemon).overlaps.is_empty());
    }

    #[test]
    fn signals_that_overlap_are_resolved_by_the_table_and_counted_per_set() {
        let mut daemon = daemon();
        let overlap = [Signal::TurnEnded, Signal::PermissionAsked];
        tell_together(&mut daemon, "s1", &overlap);
        tell_together(&mut daemon, "s22", &overlap);
        tell_together(
            &mut daemon,
            "s333",
            &[Signal::Interrupted, Signal::QuestionAsked],
        );
        // A wait ranks before a turn end, and an interrupt before a wait.
        assert_eq!(states(&daemon), [State::Question, State::Question]);
        assert_eq!(daemon.queue.phase(&started("s333").key), Phase::Idle);
        let set = |signals: &[Signal]| signals.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            status(&mut daemon).overlaps,
            [
                Overlap {
                    harness: Harness::Claude,
                    signals: set(&overlap),
                    count: 2
                },
                Overlap {
                    harness: Harness::Claude,
                    signals: set(&[Signal::Interrupted, Signal::QuestionAsked]),
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn files_are_read_before_a_request_in_the_same_drain() {
        let mut daemon = daemon();
        daemon.world.reads.push_back(vec![
            started("s1"),
            observation("s1", Event::one(Signal::Prompted)),
        ]);
        let (reply, replied) = mpsc::channel();
        daemon.take(vec![
            Input::Request(Request::Status, reply),
            Input::Files(vec!["/Users/me/.claude/sessions/1.json".into()]),
        ]);
        let status: Status = serde_json::from_value(replied.recv().unwrap().unwrap()).unwrap();
        assert_eq!(status.items.len(), 1);
        assert_eq!(daemon.world.asked, ["seed", "paths"]);
    }

    #[test]
    fn dropped_events_read_everything_again() {
        let mut daemon = daemon();
        daemon.take(vec![
            Input::Files(vec!["/Users/me/.claude/sessions/1.json".into()]),
            Input::Rescan,
        ]);
        assert_eq!(daemon.world.asked, ["seed", "all"]);
    }

    #[test]
    fn a_source_left_behind_brings_the_deadline_to_now() {
        let mut daemon = daemon();
        play_out(&mut daemon);
        daemon.next_check = daemon.world.instant + DEVICE_CHECK;
        assert!(daemon.deadline() > daemon.world.instant);
        daemon.world.behind = true;
        assert_eq!(daemon.deadline(), daemon.world.instant);
        daemon.take(Vec::new());
        assert_eq!(daemon.world.asked, ["seed", "behind"]);
    }

    #[test]
    fn a_hook_is_read_through_its_harness_s_source() {
        let mut daemon = daemon();
        daemon.world.reads.push_back(vec![
            started("s1"),
            observation("s1", Event::one(Signal::TurnEnded)),
        ]);
        let hook = Hook {
            harness: Harness::Claude,
            payload: json!({}),
            ancestry: Vec::new(),
        };
        daemon.handle(Request::Hook(hook)).unwrap();
        assert_eq!(daemon.world.asked, ["seed", "hook"]);
        assert_eq!(states(&daemon), [State::Stopped]);
    }

    #[test]
    fn status_reports_the_queue_and_the_card() {
        let mut daemon = daemon();
        tell(&mut daemon, "s1", &[Signal::Prompted]);
        tell(&mut daemon, "idle", &[]);
        let status = status(&mut daemon);
        assert_eq!(status.items.len(), 1);
        assert_eq!(status.now, daemon.world.now_ms);
        assert_eq!(status.device_uptime_ms, Some(1_000));
        assert!(status.device.is_some());
        assert_eq!(status.undeclared, 0);
    }

    #[test]
    fn an_upgrade_is_noticed_once_nothing_is_playing() {
        let mut daemon = daemon();
        daemon.check();
        assert!(!daemon.replaced);
        daemon.world.replaced = true;
        tell(&mut daemon, "s1", &[Signal::TurnEnded]);
        daemon.next_check = daemon.world.instant;
        daemon.tick();
        // The snake is playing, so the check waits.
        assert!(!daemon.replaced);
        play_out(&mut daemon);
        daemon.world.instant = daemon.next_check;
        daemon.tick();
        assert!(daemon.replaced);
    }
}
