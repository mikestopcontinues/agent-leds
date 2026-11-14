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

use crate::config::{Color, Config, PALETTES, state_dir};
use crate::render::{self, Step};
use crate::state::{Change, Queue, State, Update};
use crate::{desktop, device};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Report {
        session: String,
        label: String,
        update: Update,
    },
    Status,
    Dismiss {
        session: Option<String>,
    },
    Color {
        state: State,
        color: Color,
    },
    Palette {
        name: String,
    },
    Brightness {
        level: u8,
    },
    Front {
        led: u8,
    },
    Balance {
        weights: [f64; 3],
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
    /// Show a queue as these settings would, announcing a change first if
    /// asked, for a while, then return to the real queue.
    Show {
        states: Vec<State>,
        announce: Option<State>,
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
        .context("the leds daemon is not running; run `leds install`")?;
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
        eprintln!("leds: ignoring the saved queue: {error}");
        Queue::default()
    })
}

/// How often to read the device's uptime, which also keeps the SD reader awake.
const DEVICE_CHECK: Duration = Duration::from_secs(15);

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
}

pub fn run() -> Result<()> {
    let path = socket_path();
    std::fs::create_dir_all(state_dir())?;
    if UnixStream::connect(&path).is_ok() {
        bail!("a leds daemon is already running");
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

    let mut daemon = Daemon {
        config: Config::load()?,
        queue: saved_queue(),
        pending: VecDeque::new(),
        settle_at: None,
        holding: false,
        uptime: None,
        next_check: Instant::now(),
    };
    loop {
        let deadline = daemon
            .settle_at
            .map_or(daemon.next_check, |settle| settle.min(daemon.next_check));
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
            Request::Report {
                session,
                label,
                update,
            } => {
                match self.queue.apply(&session, &label, update, now_ms()) {
                    Change::Entered(state) => {
                        if state == State::Stopped {
                            // A session that stops while the user is looking at it is read already.
                            self.mark_read();
                        }
                        self.save_queue();
                        self.announce(state);
                    }
                    Change::Left => {
                        self.save_queue();
                        self.refresh();
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
                    self.refresh();
                }
                Ok(Value::Null)
            }
            Request::Color { state, color } => {
                let mut config = self.config.clone();
                config.colors.set(state, color);
                self.configure(config)
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
            Request::Brightness { level } => self.configure(Config {
                brightness: level,
                ..self.config.clone()
            }),
            Request::Front { led } => self.configure(Config {
                front_led: led,
                ..self.config.clone()
            }),
            Request::Balance { weights } => self.configure(Config {
                balance: weights,
                ..self.config.clone()
            }),
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
                config,
                seconds,
            } => {
                config.check()?;
                let slots = render::slots(&states);
                let mut steps = announce
                    .map_or_else(Vec::new, |state| render::announce(state, &slots, &config));
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
        if self.next_check <= now {
            self.next_check = now + DEVICE_CHECK;
            let read = self.mark_read();
            let keep_read_ms = u64::from(self.config.keep_read_minutes) * 60_000;
            let left = self.queue.expire(now_ms(), keep_read_ms);
            if read || left {
                self.save_queue();
            }
            if left {
                self.refresh();
            }
            match device::uptime_ms() {
                Ok(uptime) => {
                    if self.uptime.is_none_or(|last| uptime < last) {
                        self.steady();
                    }
                    self.uptime = Some(uptime);
                }
                Err(_) => self.uptime = None,
            }
        }
    }

    /// Notes which stopped sessions the user has looked at since they stopped.
    fn mark_read(&mut self) -> bool {
        let Some(since) = self.queue.unread_since() else {
            return false;
        };
        self.queue.mark_read(&desktop::looked_at(since, now_ms()))
    }

    fn save_queue(&self) {
        let saved = serde_json::to_vec(&self.queue).expect("a queue is plain data");
        if let Err(error) = std::fs::write(queue_path(), saved) {
            eprintln!("leds: could not save the queue: {error}");
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
        self.play(render::announce(state, &self.slots(), &self.config));
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
        self.write(&render::steady(&self.slots(), &self.config));
    }

    fn write(&mut self, program: &str) {
        if let Err(error) = device::write(program) {
            if self.uptime.is_some() {
                eprintln!("leds: could not write to the device: {error}");
            }
            // The next device check rewrites the queue once the device is back.
            self.uptime = None;
        }
    }
}
