//! The service: owns the queue and the device, and answers the CLI and hooks
//! over a Unix socket carrying one JSON request and one JSON reply per connection.

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
use crate::device;
use crate::render;
use crate::state::{Change, Queue, State, Update};

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
    /// Show a raw program for a while, then return to the queue.
    Play {
        program: String,
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
    /// When the program on the device ends and the steady queue must be rewritten.
    settle_at: Option<Instant>,
    /// A played program is showing; queue changes wait until it ends.
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
                self.config.colors.set(state, color);
                self.configured()
            }
            Request::Palette { name } => {
                let named = PALETTES.iter().find(|palette| palette.name == name);
                self.config.colors = named
                    .ok_or_else(|| format!("no palette named {name:?}"))?
                    .colors;
                self.configured()
            }
            Request::Brightness { level } => {
                self.config.brightness = level;
                self.configured()
            }
            Request::Front { led } => {
                if led != 0 && led != 7 {
                    return Err("the front light is 0 or 7".into());
                }
                self.config.front_led = led;
                self.configured()
            }
            Request::Balance { weights } => {
                if !weights
                    .iter()
                    .all(|weight| weight.is_finite() && *weight > 0.0)
                {
                    return Err("each die's brightness must be above 0".into());
                }
                self.config.balance = weights;
                self.configured()
            }
            Request::Play { program, seconds } => {
                device::check(&program)?;
                if !(seconds > 0.0 && seconds <= 3600.0) {
                    return Err("play for between 0 and 3600 seconds".into());
                }
                device::write(&program).map_err(|error| error.to_string())?;
                self.holding = true;
                self.settle_at = Some(Instant::now() + Duration::from_secs_f64(seconds));
                Ok(Value::Null)
            }
        }
    }

    fn configured(&mut self) -> Reply {
        self.config.save().map_err(|error| format!("{error:#}"))?;
        self.refresh();
        Ok(json!(self.config))
    }

    fn tick(&mut self) {
        let now = Instant::now();
        if self.settle_at.is_some_and(|settle| settle <= now) {
            self.steady();
        }
        if self.next_check <= now {
            self.next_check = now + DEVICE_CHECK;
            if self.queue.expire(now_ms()) {
                self.save_queue();
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
        self.write(&render::announce(state, &self.slots(), &self.config));
        self.settle_at = Some(Instant::now() + Duration::from_millis(render::ANNOUNCE_MS + 50));
    }

    /// Shows the queue now, unless a played program still has the lights.
    fn refresh(&mut self) {
        if !self.holding {
            self.steady();
        }
    }

    fn steady(&mut self) {
        self.holding = false;
        self.settle_at = None;
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
