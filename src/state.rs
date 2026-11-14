//! The light a session shows, which is also the order of the queue.

use serde::{Deserialize, Serialize};

/// What a session is doing, in queue order: the first variant sits at the front.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Error,
    Question,
    Stopped,
    Working,
}

impl State {
    pub const ALL: [State; 4] = [
        State::Error,
        State::Question,
        State::Stopped,
        State::Working,
    ];
}
