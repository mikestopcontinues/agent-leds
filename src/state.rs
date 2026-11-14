//! The queue of agent sessions that are doing something or need something.

use std::collections::HashMap;

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

/// What one agent event means for its session.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Update {
    Enter(State),
    /// The turn ended on a question. Unlike a prompt the agent is blocked on,
    /// this is a weak signal: the user may read it and leave it unanswered.
    Asked,
    /// Something other than the main agent is working in the session: a
    /// subagent ran a tool. A session that had stopped is working again;
    /// one that needs the user still does.
    Background,
    /// The session no longer belongs in the queue.
    Leave,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub session: String,
    pub label: String,
    pub state: State,
    /// When the session entered this state, in Unix milliseconds.
    pub since: u64,
    /// When the session last reported anything, in Unix milliseconds.
    seen: u64,
    /// Whether this is a question the turn merely ended on, which reading
    /// settles, rather than a prompt the agent is blocked on.
    #[serde(default)]
    pub weak: bool,
    /// When the user first looked at the session since its turn ended, in Unix milliseconds.
    pub read: Option<u64>,
}

impl Item {
    /// Whether looking at the session is enough to settle it: its turn has
    /// ended and nothing is blocked on the user.
    fn settled_by_reading(&self) -> bool {
        self.state == State::Stopped || (self.state == State::Question && self.weak)
    }

    /// Whether the session's turn has ended and the user has not looked at it since.
    fn is_unread(&self) -> bool {
        self.settled_by_reading() && self.read.is_none()
    }
}

#[derive(Debug, PartialEq)]
pub enum Change {
    None,
    Entered(State),
    Left,
}

/// A working session that has been silent this long is assumed to have been
/// interrupted: Claude Code fires no hook when the user stops a turn.
const WORKING_SILENCE_MS: u64 = 30 * 60 * 1000;

#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Queue {
    items: Vec<Item>,
}

impl Queue {
    pub fn apply(&mut self, session: &str, label: &str, update: Update, now: u64) -> Change {
        let position = self.items.iter().position(|item| item.session == session);
        let (state, weak) = match (update, position) {
            (Update::Enter(state), _) => (state, false),
            (Update::Asked, _) => (State::Question, true),
            (Update::Background, Some(index)) => {
                let item = &mut self.items[index];
                item.seen = now;
                if item.state != State::Stopped {
                    return Change::None;
                }
                (State::Working, false)
            }
            (Update::Leave, Some(index)) => {
                self.items.remove(index);
                return Change::Left;
            }
            (Update::Background | Update::Leave, None) => return Change::None,
        };
        let Some(index) = position else {
            self.items.push(Item {
                session: session.to_owned(),
                label: label.to_owned(),
                state,
                since: now,
                seen: now,
                weak,
                read: None,
            });
            return Change::Entered(state);
        };
        let item = &mut self.items[index];
        item.seen = now;
        // The end of a turn is news even to a session that had already
        // stopped: it has said something the user has not read.
        let turn_ended = matches!(update, Update::Enter(State::Stopped) | Update::Asked);
        if item.state == state && item.weak == weak && !turn_ended {
            return Change::None;
        }
        item.state = state;
        item.weak = weak;
        item.since = now;
        item.read = None;
        Change::Entered(state)
    }

    /// Items in queue order: by state, then longest in that state first.
    pub fn items(&self) -> Vec<&Item> {
        let mut items: Vec<&Item> = self.items.iter().collect();
        items.sort_by_key(|item| (item.state, item.since));
        items
    }

    /// Removes one session by id prefix, or everything that is not working.
    pub fn dismiss(&mut self, session: Option<&str>) -> bool {
        let before = self.items.len();
        match session {
            Some(prefix) => self.items.retain(|item| !item.session.starts_with(prefix)),
            None => self.items.retain(|item| item.state == State::Working),
        }
        self.items.len() != before
    }

    /// When the oldest session that has not been read since its turn ended stopped.
    pub fn unread_since(&self) -> Option<u64> {
        let unread = self.items.iter().filter(|item| item.is_unread());
        unread.map(|item| item.since).min()
    }

    /// Marks the sessions the user has looked at since their turn ended as
    /// read, given when each session was last looked at.
    pub fn mark_read(&mut self, looked_at: &HashMap<String, u64>) -> bool {
        let mut marked = false;
        for item in self.items.iter_mut().filter(|item| item.is_unread()) {
            if let Some(&at) = looked_at.get(&item.session)
                && at >= item.since
            {
                item.read = Some(at);
                marked = true;
            }
        }
        marked
    }

    /// The daemon was not watching for a while: the Mac slept, or the daemon
    /// was restarted. Working sessions could not report then, so their
    /// silence is counted from now.
    pub fn woke(&mut self, now: u64) {
        for item in &mut self.items {
            if item.state == State::Working {
                item.seen = now;
            }
        }
    }

    /// Drops the sessions whose light no longer means anything.
    pub fn expire(&mut self, now: u64, expiry: Expiry) -> bool {
        let before = self.items.len();
        self.items.retain(|item| {
            let silent_ms = now.saturating_sub(item.seen);
            if silent_ms > expiry.abandon_ms {
                return false;
            }
            if item.state == State::Working {
                return silent_ms <= WORKING_SILENCE_MS;
            }
            let read_ms = item.read.map(|read| now.saturating_sub(read));
            !item.settled_by_reading() || read_ms.is_none_or(|ms| ms < expiry.keep_read_ms)
        });
        self.items.len() != before
    }
}

/// How long sessions keep their light.
#[derive(Clone, Copy)]
pub struct Expiry {
    /// How long a session that reading settles keeps its light once it is read.
    pub keep_read_ms: u64,
    /// How long a session can go without reporting anything before its light
    /// is dropped, whatever its state.
    pub abandon_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    /// Read sessions leave after `keep_read_ms`, and abandoned ones after a week.
    fn keep(keep_read_ms: u64) -> Expiry {
        Expiry {
            keep_read_ms,
            abandon_ms: 7 * DAY_MS,
        }
    }

    fn states(queue: &Queue) -> Vec<(&str, State)> {
        queue
            .items()
            .iter()
            .map(|item| (item.session.as_str(), item.state))
            .collect()
    }

    #[test]
    fn orders_by_state_then_age() {
        let mut queue = Queue::default();
        queue.apply("a", "a", Update::Enter(State::Working), 1);
        queue.apply("b", "b", Update::Enter(State::Stopped), 2);
        queue.apply("c", "c", Update::Enter(State::Error), 3);
        queue.apply("d", "d", Update::Enter(State::Stopped), 4);
        queue.apply("e", "e", Update::Enter(State::Question), 5);
        assert_eq!(
            states(&queue),
            [
                ("c", State::Error),
                ("e", State::Question),
                ("b", State::Stopped),
                ("d", State::Stopped),
                ("a", State::Working),
            ]
        );
    }

    #[test]
    fn reports_only_real_changes() {
        let mut queue = Queue::default();
        assert_eq!(queue.apply("a", "a", Update::Background, 1), Change::None);
        assert_eq!(queue.apply("a", "a", Update::Leave, 1), Change::None);
        assert_eq!(
            queue.apply("a", "a", Update::Enter(State::Working), 1),
            Change::Entered(State::Working)
        );
        assert_eq!(
            queue.apply("a", "a", Update::Enter(State::Working), 2),
            Change::None
        );
        assert_eq!(
            queue.apply("a", "a", Update::Enter(State::Stopped), 3),
            Change::Entered(State::Stopped)
        );
        assert_eq!(queue.apply("a", "a", Update::Leave, 4), Change::Left);
        assert!(queue.items().is_empty());
    }

    #[test]
    fn dismiss_keeps_working_sessions() {
        let mut queue = Queue::default();
        queue.apply("a", "a", Update::Enter(State::Working), 1);
        queue.apply("b", "b", Update::Enter(State::Stopped), 1);
        queue.apply("c", "c", Update::Enter(State::Error), 1);
        assert!(queue.dismiss(None));
        assert_eq!(states(&queue), [("a", State::Working)]);
        assert!(queue.dismiss(Some("a")));
        assert!(!queue.dismiss(None));
    }

    #[test]
    fn background_work_resumes_a_stopped_session_only() {
        let mut queue = Queue::default();
        queue.apply("done", "done", Update::Enter(State::Stopped), 1);
        queue.apply("asks", "asks", Update::Enter(State::Question), 1);
        queue.apply("fails", "fails", Update::Enter(State::Error), 1);
        assert_eq!(
            queue.apply("done", "done", Update::Background, 2),
            Change::Entered(State::Working)
        );
        assert_eq!(
            queue.apply("asks", "asks", Update::Background, 2),
            Change::None
        );
        assert_eq!(
            queue.apply("fails", "fails", Update::Background, 2),
            Change::None
        );
        assert_eq!(
            states(&queue),
            [
                ("fails", State::Error),
                ("asks", State::Question),
                ("done", State::Working)
            ]
        );
    }

    #[test]
    fn a_saved_queue_reloads_as_it_was() {
        let mut queue = Queue::default();
        queue.apply("a", "alpha", Update::Enter(State::Working), 5);
        queue.apply("b", "beta", Update::Enter(State::Stopped), 6);
        let mut reloaded: Queue =
            serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
        assert_eq!(states(&reloaded), states(&queue));
        // What was last seen survives too, so silence is still measured from then.
        assert!(reloaded.expire(5 + WORKING_SILENCE_MS + 1, keep(0)));
        assert_eq!(states(&reloaded), [("b", State::Stopped)]);
    }

    #[test]
    fn a_stopped_session_leaves_a_while_after_it_is_read() {
        let looked_at = |session: &str, at: u64| HashMap::from([(session.to_owned(), at)]);
        let mut queue = Queue::default();
        queue.apply("done", "done", Update::Enter(State::Stopped), 100);
        queue.apply("asks", "asks", Update::Enter(State::Question), 100);
        assert_eq!(queue.unread_since(), Some(100));
        // Looking at it before it stopped does not count, and unread it stays.
        assert!(!queue.mark_read(&looked_at("done", 99)));
        assert!(!queue.expire(DAY_MS, keep(1000)));
        // A prompt the agent is blocked on cannot be read away.
        assert!(!queue.mark_read(&looked_at("asks", 150)));
        assert!(queue.mark_read(&looked_at("done", 150)));
        assert_eq!(queue.unread_since(), None);
        // The first look counts, not a later one.
        assert!(!queue.mark_read(&looked_at("done", 900)));
        assert!(!queue.expire(150 + 999, keep(1000)));
        assert!(queue.expire(150 + 1000, keep(1000)));
        assert_eq!(states(&queue), [("asks", State::Question)]);
    }

    #[test]
    fn a_question_the_turn_ended_on_can_be_read_away_but_a_prompt_cannot() {
        let looked_at = HashMap::from([("offer".to_owned(), 20), ("prompt".to_owned(), 20)]);
        let mut queue = Queue::default();
        assert_eq!(
            queue.apply("offer", "offer", Update::Asked, 10),
            Change::Entered(State::Question)
        );
        queue.apply("prompt", "prompt", Update::Enter(State::Question), 10);
        assert!(queue.mark_read(&looked_at));
        assert!(queue.expire(20, keep(0)));
        assert_eq!(states(&queue), [("prompt", State::Question)]);
        // A prompt in a session that had only asked in passing is the stronger signal.
        queue.apply("offer", "offer", Update::Asked, 30);
        assert_eq!(
            queue.apply("offer", "offer", Update::Enter(State::Question), 31),
            Change::Entered(State::Question)
        );
        assert_eq!(queue.unread_since(), None);
    }

    #[test]
    fn a_session_that_stops_again_is_unread_again() {
        let mut queue = Queue::default();
        queue.apply("a", "a", Update::Enter(State::Stopped), 1);
        assert!(queue.mark_read(&HashMap::from([("a".to_owned(), 2)])));
        // Its turn ends again with nothing reported in between: there is something new to read.
        assert_eq!(
            queue.apply("a", "a", Update::Enter(State::Stopped), 4),
            Change::Entered(State::Stopped)
        );
        assert_eq!(queue.unread_since(), Some(4));
        assert!(!queue.expire(DAY_MS, keep(0)));
    }

    #[test]
    fn any_session_is_dropped_once_it_has_been_silent_for_days() {
        let expiry = Expiry {
            keep_read_ms: 0,
            abandon_ms: 3 * DAY_MS,
        };
        let mut queue = Queue::default();
        queue.apply("error", "error", Update::Enter(State::Error), 0);
        queue.apply("prompt", "prompt", Update::Enter(State::Question), 0);
        queue.apply("done", "done", Update::Enter(State::Stopped), 0);
        queue.apply("later", "later", Update::Enter(State::Stopped), DAY_MS);
        assert!(!queue.expire(3 * DAY_MS, expiry));
        assert!(queue.expire(3 * DAY_MS + 1, expiry));
        assert_eq!(states(&queue), [("later", State::Stopped)]);
    }

    #[test]
    fn working_sessions_are_not_blamed_for_silence_while_nobody_was_watching() {
        let mut queue = Queue::default();
        queue.apply("busy", "busy", Update::Enter(State::Working), 0);
        queue.apply("done", "done", Update::Enter(State::Stopped), 0);
        // The Mac slept for a day. Working sessions get the full silence again.
        queue.woke(DAY_MS);
        assert!(!queue.expire(DAY_MS + WORKING_SILENCE_MS, keep(0)));
        assert!(queue.expire(DAY_MS + WORKING_SILENCE_MS + 1, keep(0)));
        assert_eq!(states(&queue), [("done", State::Stopped)]);
    }

    #[test]
    fn silent_working_sessions_expire_unless_seen() {
        let mut queue = Queue::default();
        queue.apply("quiet", "quiet", Update::Enter(State::Working), 0);
        queue.apply("busy", "busy", Update::Enter(State::Working), 0);
        queue.apply("done", "done", Update::Enter(State::Stopped), 0);
        queue.apply("busy", "busy", Update::Background, WORKING_SILENCE_MS);
        assert!(queue.expire(WORKING_SILENCE_MS + 1, keep(0)));
        assert_eq!(
            states(&queue),
            [("done", State::Stopped), ("busy", State::Working)]
        );
    }
}
