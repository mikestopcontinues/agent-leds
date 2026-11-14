//! How the command line and the hooks talk to the daemon: one JSON request
//! and one JSON reply per connection to a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{Config, state_dir};
use crate::harness::Report;
use crate::render::Scene;
use crate::state::Item;

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
        scene: Scene,
        config: Config,
        seconds: f64,
    },
}

/// What the daemon says in answer to `Request::Status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Status {
    pub items: Vec<Item>,
    /// The daemon's clock, in Unix milliseconds, which the items' times are on.
    pub now: u64,
    pub config: Config,
    /// Where the card is mounted, if it is.
    pub device: Option<PathBuf>,
    pub device_uptime_ms: Option<u64>,
}

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
    answer(&line)
}

/// Sends one request and reads its reply as `T`.
pub fn ask<T: DeserializeOwned>(request: &Request) -> Result<T> {
    serde_json::from_value(self::request(request)?).context("reading the daemon's reply")
}

/// The value a reply line carries, or the error it reports.
fn answer(line: &str) -> Result<Value> {
    let mut reply: Value = serde_json::from_str(line).context("the daemon sent no reply")?;
    match reply.get("error").and_then(Value::as_str) {
        Some(error) => bail!("{error}"),
        None => Ok(reply["ok"].take()),
    }
}

/// Reads one request from a connection, has `handle` answer it, and writes
/// the reply back.
pub fn serve(stream: UnixStream, handle: impl FnOnce(Request) -> Result<Value>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let reply = serde_json::from_str(&line)
        .context("bad request")
        .and_then(handle);
    // A hook does not wait for its reply, so a closed connection is normal.
    let _ = (&stream).write_all(format!("{}\n", reply_line(reply)).as_bytes());
}

fn reply_line(reply: Result<Value>) -> Value {
    match reply {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": format!("{error:#}") }),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixStream;
    use std::thread;

    use super::*;
    use crate::harness::Harness;
    use crate::state::{State, Update};

    #[test]
    fn a_request_is_tagged_by_its_type() {
        let report = Request::Report(Report {
            harness: Harness::Codex,
            session: "s1".into(),
            label: "app".into(),
            update: Update::Enter(State::Working),
        });
        let line = serde_json::to_value(&report).unwrap();
        assert_eq!(line["type"], "report");
        assert_eq!(line["harness"], "codex");
        let back: Request = serde_json::from_value(line).unwrap();
        assert!(matches!(back, Request::Report(Report { session, .. }) if session == "s1"));
    }

    #[test]
    fn a_reply_carries_a_value_or_an_error() {
        assert_eq!(
            answer(&reply_line(Ok(json!(3))).to_string()).unwrap(),
            json!(3)
        );
        let failed = reply_line(Err(anyhow::anyhow!("inner").context("outer")));
        let error = answer(&failed.to_string()).unwrap_err().to_string();
        assert_eq!(error, "outer: inner");
        assert!(answer("").is_err());
    }

    #[test]
    fn a_connection_gets_the_answer_to_its_request() {
        let (client, server) = UnixStream::pair().unwrap();
        let daemon = thread::spawn(move || {
            serve(server, |request| match request {
                Request::Dismiss { session } => Ok(json!(session)),
                _ => bail!("unexpected"),
            })
        });
        (&client)
            .write_all(b"{\"type\":\"dismiss\",\"session\":\"abc\"}\n")
            .unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        daemon.join().unwrap();
        assert_eq!(answer(&line).unwrap(), json!("abc"));
    }

    #[test]
    fn a_request_that_cannot_be_read_is_answered_with_why() {
        let (client, server) = UnixStream::pair().unwrap();
        let daemon = thread::spawn(move || serve(server, |_| Ok(Value::Null)));
        (&client).write_all(b"{\"type\":\"dance\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        daemon.join().unwrap();
        assert!(
            answer(&line)
                .unwrap_err()
                .to_string()
                .starts_with("bad request")
        );
    }
}
