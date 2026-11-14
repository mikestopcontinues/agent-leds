//! The service: owns the queue and the device, and answers the CLI and hooks
//! over a Unix socket carrying one JSON request and one JSON reply per connection.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, PALETTES, state_dir};
use crate::desktop::Desktop;
use crate::device;
use crate::eject;
use crate::files;
use crate::harness::Report;
use crate::render::{self, Step};
use crate::state::{Change, Expiry, Queue, State};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Report(Report),
    Status,
    Dismiss {
        session: Option<String>,
    },
    Palette {
        name: String,
    },
    /// Replace every setting.
    Configure {
        config: Config,
    },
    /// Show a raw program for a while, then return to the queue.
    Play {
        program: String,
        seconds: f64,
    },
    /// Show a queue as these settings would for a while, after the change
    /// that led to it from another queue, then return to the real queue.
    Show {
        states: Vec<State>,
        announce: Option<State>,
        from: Vec<State>,
        config: Config,
        seconds: f64,
    },
}

type Reply = Result<Value, String>;

pub fn socket_path() -> PathBuf {
    state_dir().join("daemon.sock")
}

/// Sends one request to the running daemon without waiting for its reply.
pub fn send(request: &Request) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path())
        .context("the agent-leds daemon is not running; run `agent-leds install`")?;
    stream.write_all(format!("{}\n", serde_json::to_string(request)?).as_bytes())?;
    Ok(stream)
}

/// Sends one request to the running daemon and returns its reply.
pub fn request(request: &Request) -> Result<Value> {
    let stream = send(request)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let mut reply: Value = serde_json::from_str(&line).context("the daemon sent no reply")?;
    match reply.get("error").and_then(Value::as_str) {
        Some(error) => bail!("{error}"),
        None => Ok(reply["ok"].take()),
    }
}

fn queue_path() -> PathBuf {
    state_dir().join("queue.json")
}

/// The queue as last saved, so a restart keeps what is unread and in progress.
fn saved_queue() -> Queue {
    let Ok(saved) = std::fs::read(queue_path()) else {
        return Queue::default();
    };
    serde_json::from_slice(&saved).unwrap_or_else(|error| {
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

struct Daemon {
    config: Config,
    queue: Queue,
    /// Programs waiting to be written, each when the one before it ends.
    pending: VecDeque<Step>,
    /// When the program on the device ends.
    settle_at: Option<Instant>,
    /// Something other than the queue is showing; queue changes wait until it ends.
    holding: bool,
    /// The device's last uptime, or None while it is missing or needs a rewrite.
    uptime: Option<u64>,
    next_check: Instant,
    /// When to look next for whether the user has read a session.
    next_look: Instant,
    /// The queue the lights hold, or are on their way to.
    shown: Vec<State>,
    /// When the last check ran, in Unix milliseconds. A long gap means the Mac slept.
    checked: u64,
    desktop: Desktop,
}

pub fn run() -> Result<()> {
    let path = socket_path();
    std::fs::create_dir_all(state_dir())?;
    if UnixStream::connect(&path).is_ok() {
        bail!("an agent-leds daemon is already running");
    }
    let _ = std::fs::remove_file(&path);
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;

    let (requests, inbox) = mpsc::channel::<(Request, Sender<Reply>)>();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let requests = requests.clone();
            thread::spawn(move || serve(stream, &requests));
        }
    });

    thread::spawn(eject::guard);

    let mut queue = saved_queue();
    // Nothing was listening while the daemon was down.
    queue.woke(now_ms());
    let mut daemon = Daemon {
        config: Config::load()?,
        queue,
        pending: VecDeque::new(),
        settle_at: None,
        holding: false,
        uptime: None,
        next_check: Instant::now(),
        next_look: Instant::now(),
        shown: Vec::new(),
        checked: now_ms(),
        desktop: Desktop::default(),
    };
    loop {
        let deadline = [
            Some(daemon.next_check),
            daemon.settle_at,
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

fn serve(stream: UnixStream, requests: &Sender<(Request, Sender<Reply>)>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let reply = match serde_json::from_str(&line) {
        Ok(request) => {
            let (answer, answered) = mpsc::channel();
            if requests.send((request, answer)).is_err() {
                return;
            }
            answered
                .recv()
                .unwrap_or_else(|_| Err("the daemon is shutting down".into()))
        }
        Err(error) => Err(format!("bad request: {error}")),
    };
    let body = match reply {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error }),
    };
    // A hook does not wait for its reply, so a closed connection is normal.
    let _ = (&stream).write_all(format!("{body}\n").as_bytes());
}

/// How long to show something that was asked for, in milliseconds.
fn held_ms(seconds: f64) -> Result<u64, String> {
    if !(seconds > 0.0 && seconds <= 3600.0) {
        return Err("show it for between 0 and 3600 seconds".into());
    }
    Ok((seconds * 1000.0) as u64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Daemon {
    fn handle(&mut self, request: Request) -> Reply {
        match request {
            Request::Report(Report {
                harness,
                session,
                label,
                update,
            }) => {
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
                            State::Error | State::Question | State::Stopped => self.announce(state),
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
            Request::Status => Ok(json!({
                "items": self.queue.items(),
                "now": now_ms(),
                "config": self.config,
                "device": device::volume(),
                "device_uptime_ms": device::uptime_ms().ok(),
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
                let colors = named
                    .ok_or_else(|| format!("no palette named {name:?}"))?
                    .colors;
                self.configure(Config {
                    colors,
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
                let slots = render::slots(&states);
                let mut steps = render::change(announce, &render::slots(&from), &slots, &config);
                // The change starts from the queue it leaves.
                steps.insert(
                    0,
                    Step {
                        program: render::steady(&render::slots(&from), &config),
                        ms: 400,
                    },
                );
                steps.push(Step {
                    program: render::steady(&slots, &config),
                    ms: held_ms(seconds)?,
                });
                self.hold(steps)
            }
        }
    }

    fn configure(&mut self, config: Config) -> Reply {
        config.check()?;
        config.save().map_err(|error| format!("{error:#}"))?;
        self.config = config;
        self.refresh();
        Ok(json!(self.config))
    }

    /// Shows programs that are not the queue; the queue returns when the last one ends.
    fn hold(&mut self, steps: Vec<Step>) -> Reply {
        if device::volume().is_none() {
            return Err("no SidePulse volume is mounted".into());
        }
        self.holding = true;
        self.play(steps);
        Ok(Value::Null)
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.settle_at.is_some_and(|settle| settle <= now) {
            self.advance();
        }
        if self.next_look <= now {
            self.next_look = now + LOOK;
            // Looking runs a system tool, which could delay an animation's next program.
            if self.pending.is_empty() && self.mark_read() {
                self.save_queue();
            }
        }
        if self.next_check <= now {
            // A check reads the card and the desktop app's files, which can
            // take long enough to delay the next program of an animation.
            if !self.pending.is_empty() {
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
        let left = self.queue.expire(now, expiry);
        if left {
            self.save_queue();
            self.settle();
        }
        match device::uptime_ms() {
            Ok(uptime) => {
                let restarted = self.uptime.is_none_or(|last| uptime < last);
                self.uptime = Some(uptime);
                if restarted {
                    // If this write fails it clears the uptime, so the next check tries again.
                    self.steady();
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

    fn announce(&mut self, state: State) {
        if self.holding {
            return;
        }
        let slots = self.slots();
        let steps = render::change(Some(state), &self.shown, &slots, &self.config);
        self.shown = slots;
        self.play(steps);
    }

    /// Shows a change that asks nothing of the user: the wave alone, from
    /// the queue the lights hold to the queue as it is now.
    fn settle(&mut self) {
        // Whatever is playing ends on the queue as it is by then.
        if self.holding || self.settle_at.is_some() {
            return;
        }
        let slots = self.slots();
        let steps = render::change(None, &self.shown, &slots, &self.config);
        self.shown = slots;
        if !steps.is_empty() {
            self.play(steps);
        }
    }

    /// Writes the programs one after another, each when the one before it ends, then the queue.
    fn play(&mut self, steps: Vec<Step>) {
        self.pending = steps.into();
        self.advance();
    }

    /// Writes the next waiting program, or the queue once none is left.
    fn advance(&mut self) {
        match self.pending.pop_front() {
            Some(step) => {
                self.write(&step.program);
                self.settle_at = Some(Instant::now() + Duration::from_millis(step.ms));
            }
            None => self.steady(),
        }
    }

    /// Shows the queue now, unless something else is being shown.
    fn refresh(&mut self) {
        if !self.holding {
            self.steady();
        }
    }

    fn steady(&mut self) {
        self.holding = false;
        self.settle_at = None;
        self.pending.clear();
        self.shown = self.slots();
        self.write(&render::steady(&self.shown, &self.config));
    }

    fn write(&mut self, program: &str) {
        if let Err(error) = device::write(program) {
            if self.uptime.is_some() {
                eprintln!("agent-leds: could not write to the device: {error}");
            }
            // The next device check rewrites the queue once the device is back.
            self.uptime = None;
        }
    }
}
