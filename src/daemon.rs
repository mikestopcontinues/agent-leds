//! The service: owns the queue and the card, and answers the command line
//! and the hooks. What the lights play is the player's call; everything
//! the daemon reads or changes outside itself goes through a `World`.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::{Config, PALETTES, state_dir};
use crate::desktop::Desktop;
use crate::device;
use crate::eject;
use crate::files;
use crate::harness::Report;
use crate::ipc::{self, Request, Status};
use crate::player::Player;
use crate::render::{self, Step};
use crate::state::{Change, Expiry, Queue, State};

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
/// How often to look for whether the user has read a session, while one is unread.
const LOOK: Duration = Duration::from_secs(3);
/// How long a check that falls in the middle of an animation waits before trying again.
const ANIMATION_CHECK: Duration = Duration::from_secs(1);
/// How long a shown scene holds the queue it leaves before the change plays.
const SHOW_LEAD_MS: u64 = 400;

/// What the daemon needs from outside itself: the card, the clocks, the
/// desktop app, and somewhere to keep its queue and settings.
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
}

/// The Mac the daemon runs on.
struct System {
    desktop: Desktop,
    exe: PathBuf,
    /// The file `exe` led to when the daemon started.
    started_from: Option<Binary>,
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
}

struct Daemon<W: World> {
    world: W,
    config: Config,
    queue: Queue,
    player: Player,
    /// The device's last uptime, or None while it is missing or needs a rewrite.
    uptime: Option<u64>,
    next_check: Instant,
    /// When to look next for whether the user has read a session.
    next_look: Instant,
    /// When the last check ran, in Unix milliseconds. A long gap means the Mac slept.
    checked: u64,
    /// The binary was replaced, so the daemon should exit and let launchd
    /// start the new one.
    replaced: bool,
}

type Inbox = mpsc::Receiver<(Request, Sender<Result<Value>>)>;

pub fn run() -> Result<()> {
    let inbox = listen()?;
    thread::spawn(eject::guard);
    let exe = std::env::current_exe()?;
    let world = System {
        desktop: Desktop::default(),
        started_from: Binary::at(&exe),
        exe,
    };
    let mut daemon = Daemon::new(world, Config::load()?, saved_queue());
    while !daemon.replaced {
        let deadline = [
            Some(daemon.next_check),
            daemon.player.deadline(),
            // Reading is only looked for while something is unread.
            daemon.queue.unread_since().map(|_| daemon.next_look),
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("a check is always due");
        let wait = deadline.saturating_duration_since(daemon.world.instant());
        match inbox.recv_timeout(wait) {
            Ok((request, reply)) => {
                let _ = reply.send(daemon.handle(request));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the socket listener stopped"),
        }
        daemon.tick();
    }
    // The queue is saved with every change, and the new daemon rewrites the
    // card when it first checks it.
    eprintln!("agent-leds: the binary was replaced; exiting for launchd to start the new one");
    Ok(())
}

/// Listens on the socket, handing each request to the main loop with a way to answer it.
fn listen() -> Result<Inbox> {
    let path = ipc::socket_path();
    std::fs::create_dir_all(state_dir())?;
    if UnixStream::connect(&path).is_ok() {
        bail!("an agent-leds daemon is already running");
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    let (requests, inbox) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests: Sender<_> = requests.clone();
            thread::spawn(move || {
                ipc::serve(stream, |request| {
                    let (answer, answered) = mpsc::channel();
                    requests
                        .send((request, answer))
                        .context("the daemon is shutting down")?;
                    answered.recv().context("the daemon is shutting down")?
                })
            });
        }
    });
    Ok(inbox)
}

/// How long to show something that was asked for, in milliseconds.
fn held_ms(seconds: f64) -> Result<u64> {
    if !(seconds > 0.0 && seconds <= 3600.0) {
        bail!("show it for between 0 and 3600 seconds");
    }
    Ok((seconds * 1000.0) as u64)
}

impl<W: World> Daemon<W> {
    fn new(world: W, config: Config, mut queue: Queue) -> Daemon<W> {
        // Nothing was listening while the daemon was down.
        queue.woke(world.now_ms());
        let now = world.instant();
        Daemon {
            checked: world.now_ms(),
            world,
            config,
            queue,
            player: Player::default(),
            uptime: None,
            next_check: now,
            next_look: now,
            replaced: false,
        }
    }

    fn handle(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Report(report) => self.report(report),
            Request::Status => Ok(json!(Status {
                items: self.queue.items().into_iter().cloned().collect(),
                now: self.world.now_ms(),
                config: self.config.clone(),
                device: self.world.card(),
                device_uptime_ms: self.world.uptime_ms().ok(),
            })),
            Request::Dismiss { session } => {
                if self.queue.dismiss(session.as_deref()) {
                    self.save_queue();
                    self.settle();
                }
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

    fn report(&mut self, report: Report) -> Result<Value> {
        let Report {
            harness,
            session,
            label,
            update,
        } = report;
        match self
            .queue
            .apply(harness, &session, &label, update, self.world.now_ms())
        {
            Change::Entered(state) => {
                if matches!(state, State::Stopped | State::Question) {
                    // A session whose turn ends while the user is looking at it is read already.
                    self.mark_read();
                }
                self.save_queue();
                // Only a session that wants the user is announced.
                match state {
                    State::Working => self.settle(),
                    State::Error | State::Question | State::Stopped => {
                        let slots = self.slots();
                        let now = self.world.instant();
                        let program = self.player.announce(state, &slots, &self.config, now);
                        self.write(program);
                    }
                }
            }
            Change::Left => {
                self.save_queue();
                self.settle();
            }
            Change::None => {}
        }
        Ok(Value::Null)
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
            if !self.player.animating() && self.mark_read() {
                self.save_queue();
            }
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

    /// Drops the sessions whose light no longer means anything, and rewrites
    /// the queue if the card restarted.
    fn check(&mut self) {
        let now = self.world.now_ms();
        // The monotonic clock that times the checks stops while the Mac
        // sleeps; the wall clock shows how long it was.
        if now.saturating_sub(self.checked) > 4 * DEVICE_CHECK.as_millis() as u64 {
            self.queue.woke(now);
        }
        self.checked = now;
        let expiry = Expiry {
            keep_read_ms: u64::from(self.config.keep_read_minutes) * 60_000,
            abandon_ms: u64::from(self.config.abandon_days) * 24 * 60 * 60_000,
        };
        if self.queue.expire(now, expiry) {
            self.save_queue();
            self.settle();
        }
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

    /// Notes which sessions the user has looked at since their turn ended.
    fn mark_read(&mut self) -> bool {
        let Some(since) = self.queue.unread_since() else {
            return false;
        };
        let now = self.world.now_ms();
        let looked_at = self.world.looked_at(since, now);
        self.queue.mark_read(&looked_at)
    }

    fn save_queue(&mut self) {
        self.world.save_queue(&self.queue);
    }

    fn slots(&self) -> Vec<State> {
        let states: Vec<State> = self.queue.items().iter().map(|item| item.state).collect();
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
    use std::time::Duration;

    use super::*;
    use crate::harness::Harness;
    use crate::state::Update;

    /// A world the test controls: a card that records what is written to
    /// it, clocks that move only when told, and a user who has looked at
    /// what the test says.
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
    }

    fn daemon() -> Daemon<Fake> {
        let world = Fake {
            card: true,
            written: Vec::new(),
            uptime: Ok(1_000),
            looked_at: HashMap::new(),
            now_ms: 1_000_000,
            instant: Instant::now(),
            saved_queues: 0,
            saved_config: None,
            replaced: false,
        };
        Daemon::new(world, Config::default(), Queue::default())
    }

    fn report(daemon: &mut Daemon<Fake>, session: &str, update: Update) {
        let report = Report {
            harness: Harness::Claude,
            session: session.into(),
            label: "app".into(),
            update,
        };
        daemon.handle(Request::Report(report)).unwrap();
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

    #[test]
    fn a_session_that_stops_is_announced_and_the_queue_follows() {
        let mut daemon = daemon();
        report(&mut daemon, "s1", Update::Enter(State::Stopped));
        let snake = render::change(Some(State::Stopped), &[], &[State::Stopped], &daemon.config);
        assert_eq!(daemon.world.written, [snake[0].program.clone()]);
        assert_eq!(daemon.world.saved_queues, 1);
        play_out(&mut daemon);
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
    }

    #[test]
    fn a_session_that_goes_to_work_settles_in_without_a_snake() {
        let mut daemon = daemon();
        report(&mut daemon, "s1", Update::Enter(State::Working));
        let wave = render::change(None, &[], &[State::Working], &daemon.config);
        assert_eq!(daemon.world.written, [wave[0].program.clone()]);
        // A report that changes nothing writes and saves nothing.
        report(&mut daemon, "s1", Update::Enter(State::Working));
        assert_eq!(daemon.world.written.len(), 1);
        assert_eq!(daemon.world.saved_queues, 1);
    }

    #[test]
    fn a_turn_that_ends_while_the_user_is_looking_is_read_already() {
        let mut daemon = daemon();
        daemon.world.looked_at = HashMap::from([("s1".to_owned(), daemon.world.now_ms)]);
        report(&mut daemon, "s1", Update::Enter(State::Stopped));
        assert_eq!(daemon.queue.items()[0].read, Some(daemon.world.now_ms));
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
        report(&mut daemon, "s1", Update::Enter(State::Working));
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
        report(&mut daemon, "s1", Update::Enter(State::Stopped));
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
    fn dismissing_clears_what_is_not_working() {
        let mut daemon = daemon();
        report(&mut daemon, "s1", Update::Enter(State::Stopped));
        report(&mut daemon, "s2", Update::Enter(State::Working));
        play_out(&mut daemon);
        daemon.handle(Request::Dismiss { session: None }).unwrap();
        play_out(&mut daemon);
        let states: Vec<State> = daemon.queue.items().iter().map(|item| item.state).collect();
        assert_eq!(states, [State::Working]);
        assert_eq!(daemon.world.written.last(), Some(&steady(&daemon)));
    }

    #[test]
    fn status_reports_the_queue_and_the_card() {
        let mut daemon = daemon();
        report(&mut daemon, "s1", Update::Enter(State::Working));
        let status: Status =
            serde_json::from_value(daemon.handle(Request::Status).unwrap()).unwrap();
        assert_eq!(status.items.len(), 1);
        assert_eq!(status.now, daemon.world.now_ms);
        assert_eq!(status.device_uptime_ms, Some(1_000));
        assert!(status.device.is_some());
    }

    #[test]
    fn an_upgrade_is_noticed_once_nothing_is_playing() {
        let mut daemon = daemon();
        daemon.check();
        assert!(!daemon.replaced);
        daemon.world.replaced = true;
        report(&mut daemon, "s1", Update::Enter(State::Stopped));
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
