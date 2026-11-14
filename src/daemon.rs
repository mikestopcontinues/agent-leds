//! The service: owns the queue and the card, and answers the command line
//! and the hooks. It does the I/O; what the lights play is the player's call.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
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

struct Daemon {
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
    desktop: Desktop,
}

type Inbox = mpsc::Receiver<(Request, Sender<Result<Value>>)>;

pub fn run() -> Result<()> {
    let inbox = listen()?;
    thread::spawn(eject::guard);
    let mut queue = saved_queue();
    // Nothing was listening while the daemon was down.
    queue.woke(now_ms());
    let mut daemon = Daemon {
        config: Config::load()?,
        queue,
        player: Player::default(),
        uptime: None,
        next_check: Instant::now(),
        next_look: Instant::now(),
        checked: now_ms(),
        desktop: Desktop::default(),
    };
    loop {
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
        match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((request, reply)) => {
                let _ = reply.send(daemon.handle(request));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("the socket listener stopped"),
        }
        daemon.tick();
    }
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Daemon {
    fn handle(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Report(report) => self.report(report),
            Request::Status => Ok(json!(Status {
                items: self.queue.items().into_iter().cloned().collect(),
                now: now_ms(),
                config: self.config.clone(),
                device: device::volume(),
                device_uptime_ms: device::uptime_ms().ok(),
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
                states,
                announce,
                from,
                config,
                seconds,
            } => {
                config.check()?;
                let from = render::slots(&from);
                let slots = render::slots(&states);
                // The change starts from the queue it leaves, and ends held on the one it reaches.
                let lead = Step {
                    program: render::steady(&from, &config),
                    ms: SHOW_LEAD_MS,
                };
                let held = Step {
                    program: render::steady(&slots, &config),
                    ms: held_ms(seconds)?,
                };
                let change = render::change(announce, &from, &slots, &config);
                self.hold([vec![lead], change, vec![held]].concat())
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
            .apply(harness, &session, &label, update, now_ms())
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
                        let program =
                            self.player
                                .announce(state, &slots, &self.config, Instant::now());
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
        config.save()?;
        self.config = config;
        let program = self.player.refresh(&self.slots(), &self.config);
        self.write(program);
        Ok(json!(self.config))
    }

    /// Shows programs that are not the queue; the queue returns when the last one ends.
    fn hold(&mut self, steps: Vec<Step>) -> Result<Value> {
        if device::volume().is_none() {
            bail!("no SidePulse volume is mounted");
        }
        let program = self
            .player
            .hold(steps, &self.slots(), &self.config, Instant::now());
        self.write(program);
        Ok(Value::Null)
    }

    fn tick(&mut self) {
        let now = Instant::now();
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
        let now = now_ms();
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
        match device::uptime_ms() {
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
    }

    /// Notes which sessions the user has looked at since their turn ended.
    fn mark_read(&mut self) -> bool {
        let Some(since) = self.queue.unread_since() else {
            return false;
        };
        let looked_at = self.desktop.looked_at(since, now_ms());
        self.queue.mark_read(&looked_at)
    }

    fn save_queue(&self) {
        let saved = serde_json::to_string(&self.queue).expect("a queue is plain data");
        if let Err(error) = files::replace(&queue_path(), &saved) {
            eprintln!("agent-leds: could not save the queue: {error:#}");
        }
    }

    fn slots(&self) -> Vec<State> {
        let states: Vec<State> = self.queue.items().iter().map(|item| item.state).collect();
        render::slots(&states)
    }

    /// Shows a change that asks nothing of the user.
    fn settle(&mut self) {
        let program = self
            .player
            .settle(&self.slots(), &self.config, Instant::now());
        self.write(program);
    }

    fn write(&mut self, program: Option<String>) {
        let Some(program) = program else {
            return;
        };
        if let Err(error) = device::write(&program) {
            if self.uptime.is_some() {
                eprintln!("agent-leds: could not write to the device: {error}");
            }
            // The next device check rewrites the queue once the device is back.
            self.uptime = None;
        }
    }
}
