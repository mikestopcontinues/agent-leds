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
    /// When the user first looked at the session after it stopped, in Unix milliseconds.
    pub read: Option<u64>,
}

impl Item {
    /// Whether the session has stopped and the user has not looked at it since.
    fn is_unread(&self) -> bool {
        self.state == State::Stopped && self.read.is_none()
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
        match (update, position) {
            (Update::Enter(state), Some(index)) => {
                let item = &mut self.items[index];
                item.seen = now;
                if item.state == state {
                    return Change::None;
                }
                item.state = state;
                item.since = now;
                item.read = None;
                Change::Entered(state)
            }
            (Update::Enter(state), None) => {
                self.items.push(Item {
                    session: session.to_owned(),
                    label: label.to_owned(),
                    state,
                    since: now,
                    seen: now,
                    read: None,
                });
                Change::Entered(state)
            }
            (Update::Background, Some(index)) => {
                let item = &mut self.items[index];
                item.seen = now;
                if item.state != State::Stopped {
                    return Change::None;
                }
                item.state = State::Working;
                item.since = now;
                item.read = None;
                Change::Entered(State::Working)
            }
            (Update::Leave, Some(index)) => {
                self.items.remove(index);
                Change::Left
            }
            (Update::Background | Update::Leave, None) => Change::None,
        }
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

    /// When the oldest stopped session that has not been read stopped.
    pub fn unread_since(&self) -> Option<u64> {
        let unread = self.items.iter().filter(|item| item.is_unread());
        unread.map(|item| item.since).min()
    }

    /// Marks the stopped sessions the user has looked at since they stopped as
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

    /// Drops working sessions that have gone silent, and stopped sessions
    /// that were read at least `keep_read_ms` ago.
    pub fn expire(&mut self, now: u64, keep_read_ms: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|item| match item.state {
            State::Working => now.saturating_sub(item.seen) <= WORKING_SILENCE_MS,
            State::Stopped => item
                .read
                .is_none_or(|read| now.saturating_sub(read) < keep_read_ms),
            State::Error | State::Question => true,
        });
        self.items.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(reloaded.expire(5 + WORKING_SILENCE_MS + 1, 0));
        assert_eq!(states(&reloaded), [("b", State::Stopped)]);
    }

    #[test]
    fn a_stopped_session_leaves_a_while_after_it_is_read() {
        const KEEP: u64 = 1000;
        let looked_at = |session: &str, at: u64| HashMap::from([(session.to_owned(), at)]);
        let mut queue = Queue::default();
        queue.apply("done", "done", Update::Enter(State::Stopped), 100);
        queue.apply("asks", "asks", Update::Enter(State::Question), 100);
        assert_eq!(queue.unread_since(), Some(100));
        // Looking at it before it stopped does not count, and unread it stays however long.
        assert!(!queue.mark_read(&looked_at("done", 99)));
        assert!(!queue.expire(1_000_000, KEEP));
        // Only a stopped session can be read: a question still needs an answer.
        assert!(!queue.mark_read(&looked_at("asks", 150)));
        assert!(queue.mark_read(&looked_at("done", 150)));
        assert_eq!(queue.unread_since(), None);
        // The first look counts, not a later one.
        assert!(!queue.mark_read(&looked_at("done", 900)));
        assert!(!queue.expire(150 + KEEP - 1, KEEP));
        assert!(queue.expire(150 + KEEP, KEEP));
        assert_eq!(states(&queue), [("asks", State::Question)]);
    }

    #[test]
    fn a_session_that_stops_again_is_unread_again() {
        let mut queue = Queue::default();
        queue.apply("a", "a", Update::Enter(State::Stopped), 1);
        assert!(queue.mark_read(&HashMap::from([("a".to_owned(), 2)])));
        queue.apply("a", "a", Update::Enter(State::Working), 3);
        queue.apply("a", "a", Update::Enter(State::Stopped), 4);
        assert_eq!(queue.unread_since(), Some(4));
        assert!(!queue.expire(1_000_000, 0));
    }

    #[test]
    fn silent_working_sessions_expire_unless_seen() {
        let mut queue = Queue::default();
        queue.apply("quiet", "quiet", Update::Enter(State::Working), 0);
        queue.apply("busy", "busy", Update::Enter(State::Working), 0);
        queue.apply("done", "done", Update::Enter(State::Stopped), 0);
        queue.apply("busy", "busy", Update::Background, WORKING_SILENCE_MS);
        assert!(queue.expire(WORKING_SILENCE_MS + 1, 0));
        assert_eq!(
            states(&queue),
            [("done", State::Stopped), ("busy", State::Working)]
        );
    }
}
