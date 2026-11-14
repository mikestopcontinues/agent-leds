//! Reading what the harnesses write about their sessions, and what their
//! hooks tell.

// Read by the harness sources, which come in the next layer.
#[allow(dead_code)]
pub mod tail;

use crate::fsm::{Channel, Signal};
use crate::liveness::Liveness;
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
