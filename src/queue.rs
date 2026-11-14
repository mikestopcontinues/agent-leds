//! The sessions the daemon follows, and the one place their phases change.
//!
//! Every observation, from a harness's files, a hook or the daemon itself,
//! goes through `Queue::apply`, which moves the session by the table in
//! `fsm`. The queue the lights show is the sessions whose phase has a light,
//! in the order of their state and then how long they have held it.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fsm::{self, Channel, Phase, Signal};
use crate::harness::Harness;
use crate::liveness::Liveness;
use crate::source::{Event, Observation};
use crate::state::State;

/// Which session: its harness and the harness's id for it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    pub harness: Harness,
    pub id: String,
}

/// A harness's own name for a turn: Claude's promptId, Codex's turn_id,
/// Grok's turn_number, Cursor's count of user lines, Junie's taskId,
/// OpenCode's user message id, Pi's user entry id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnKey(pub String);

/// One session the daemon follows, with or without a light.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub key: Key,
    /// What the queue calls the session: the name of its folder.
    pub label: String,
    pub phase: Phase,
    /// The last turn an observation named.
    pub turn: Option<TurnKey>,
    /// When its light took its state or was last announced, in Unix
    /// milliseconds. The queue is ordered by it.
    pub since: u64,
    /// When it entered its phase; for a read phase, when it was read.
    pub entered: u64,
    /// When its harness last said anything about it.
    pub seen: u64,
    /// How to tell its process still runs.
    pub liveness: Liveness,
    /// The channel that started it, which a sweep's `Ended` comes through.
    pub bound: Channel,
}

/// The sessions, as saved in queue.json. Tracked sessions without a light
/// are kept too, so the sweep can tell when their process ends.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Queue {
    sessions: Vec<Session>,
}

/// What applying an observation changed.
#[derive(Debug, PartialEq)]
pub enum Change {
    None,
    /// The queue changed, but not what the lights show.
    Saved,
    /// The lights change, with nothing to announce.
    Settle,
    /// A session wants the user.
    Announce(State),
}

/// How long sessions keep their light.
#[derive(Clone, Copy)]
pub struct Expiry {
    /// How long a session that reading settles keeps its light once it is read.
    pub keep_read_ms: u64,
    /// How long a session can go without its harness saying anything before
    /// its light goes, whatever its state.
    pub abandon_ms: u64,
}

impl Queue {
    /// Moves a session by the table. The only place phases change.
    pub fn apply(&mut self, o: &Observation, now: u64) -> Change {
        let signal = o.event.signal();
        let position = self.sessions.iter().position(|s| s.key == o.key);
        if let (Event::Started(start), Some(index)) = (&o.event, position) {
            // A resume in a new process, or a folder learned later.
            let session = &mut self.sessions[index];
            session.seen = now;
            let label = label(&start.cwd);
            let mut changed = false;
            if !start.cwd.is_empty() && session.label != label {
                session.label = label;
                changed = true;
            }
            if session.liveness != start.liveness {
                session.liveness = start.liveness.clone();
                changed = true;
            }
            if o.turn.is_some() && session.turn != o.turn {
                session.turn = o.turn.clone();
                changed = true;
            }
            return if changed { Change::Saved } else { Change::None };
        }
        let from = position.map_or(Phase::Gone, |index| self.sessions[index].phase);
        if let Some(index) = position {
            let session = &mut self.sessions[index];
            if o.channel != Channel::Daemon {
                session.seen = now;
            }
            if o.turn.is_some() {
                session.turn = o.turn.clone();
            }
        }
        let Some(to) = fsm::next(from, signal) else {
            return Change::None;
        };
        let announce = fsm::announces(from, signal, to);
        let light = to.state() != from.state();
        match (position, &o.event) {
            (None, Event::Started(start)) => self.sessions.push(Session {
                key: o.key.clone(),
                label: label(&start.cwd),
                phase: to,
                turn: o.turn.clone(),
                since: now,
                entered: now,
                seen: now,
                liveness: start.liveness.clone(),
                bound: o.channel,
            }),
            (None, Event::Signal(_)) => unreachable!("only Started leaves Gone"),
            (Some(index), _) if to == Phase::Gone => {
                self.sessions.remove(index);
            }
            (Some(index), _) => {
                let session = &mut self.sessions[index];
                session.phase = to;
                session.entered = now;
                if light || announce {
                    session.since = now;
                }
            }
        }
        match to.state() {
            Some(state) if announce && !o.quiet => Change::Announce(state),
            _ if light => Change::Settle,
            _ => Change::Saved,
        }
    }

    /// The sessions with a light, in queue order: by state, then longest in
    /// that state first.
    pub fn items(&self) -> Vec<&Session> {
        let mut items: Vec<&Session> = self
            .sessions
            .iter()
            .filter(|s| s.phase.state().is_some())
            .collect();
        items.sort_by_key(|s| (s.phase.state(), s.since));
        items
    }

    /// When the oldest session that has not been read since its turn ended stopped.
    pub fn unread_since(&self) -> Option<u64> {
        let unread = self.sessions.iter().filter(|s| s.phase.unread());
        unread.map(|s| s.since).min()
    }

    /// Whether any session is followed at all, with a light or not.
    pub fn tracked(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// The unread Claude Code sessions the user has looked at since their
    /// light took its state, given when each was last looked at.
    pub fn looked(&self, at: &HashMap<String, u64>) -> Vec<Observation> {
        self.sessions
            .iter()
            .filter(|s| s.key.harness == Harness::Claude && s.phase.unread())
            .filter(|s| at.get(&s.key.id).is_some_and(|&at| at >= s.since))
            .map(|s| told(s, Signal::Looked))
            .collect()
    }

    /// The sessions whose light no longer means anything: read long enough
    /// ago, or silent for longer than anyone waits.
    pub fn due(&self, now: u64, expiry: Expiry) -> Vec<Observation> {
        let mut due = Vec::new();
        for session in &self.sessions {
            let read = matches!(session.phase, Phase::StoppedRead | Phase::AskedRead);
            if read && now.saturating_sub(session.entered) >= expiry.keep_read_ms {
                due.push(told(session, Signal::ReadExpired));
            } else if session.phase.state().is_some()
                && now.saturating_sub(session.seen) > expiry.abandon_ms
            {
                due.push(told(session, Signal::Abandoned));
            }
        }
        due
    }

    /// The sessions `agent-leds dismiss` clears: those whose id starts with
    /// the prefix, or every one that is not working.
    pub fn dismissed(&self, prefix: Option<&str>) -> Vec<Observation> {
        self.sessions
            .iter()
            .filter(|s| match (s.phase.state(), prefix) {
                (None, _) => false,
                (Some(_), Some(prefix)) => s.key.id.starts_with(prefix),
                (Some(state), None) => state != State::Working,
            })
            .map(|s| told(s, Signal::Dismissed))
            .collect()
    }

    /// The sessions whose process has ended, by `alive`, each through the
    /// channel that started it.
    pub fn sweep(&self, mut alive: impl FnMut(&Liveness) -> bool) -> Vec<Observation> {
        self.sessions
            .iter()
            .filter(|s| !alive(&s.liveness))
            .map(|s| Observation {
                key: s.key.clone(),
                channel: s.bound,
                event: Event::Signal(Signal::Ended),
                turn: None,
                quiet: false,
            })
            .collect()
    }

    /// The sessions a hook started, which no file of their harness names, so
    /// the daemon hands them to their source at startup.
    pub fn bound(&self) -> Vec<(Key, Liveness)> {
        self.sessions
            .iter()
            .filter(|s| s.bound == Channel::Hook)
            .map(|s| (s.key.clone(), s.liveness.clone()))
            .collect()
    }

    /// The queue at startup: what the harnesses' files show now (`fresh`),
    /// keeping a saved session's read mark or dismissal when it is about the
    /// same turn and the daemon alone could have taken it there. A turn that
    /// ended while the daemon was down shows unread, and a session whose
    /// process is gone is not in `fresh` at all.
    pub fn restore(saved: Queue, fresh: Queue) -> Queue {
        let daemon: Vec<Signal> = Signal::ALL.into_iter().filter(|s| s.daemon()).collect();
        let mut sessions = fresh.sessions;
        for session in &mut sessions {
            let Some(old) = saved.sessions.iter().find(|old| old.key == session.key) else {
                continue;
            };
            if old.turn.is_some()
                && old.turn == session.turn
                && fsm::reachable(session.phase, &daemon).contains(&old.phase)
            {
                session.phase = old.phase;
                session.since = old.since;
                session.entered = old.entered;
            }
        }
        Queue { sessions }
    }
}

/// A signal the daemon tells about a session.
fn told(session: &Session, signal: Signal) -> Observation {
    Observation {
        key: session.key.clone(),
        channel: Channel::Daemon,
        event: Event::Signal(signal),
        turn: None,
        quiet: false,
    }
}

/// The name of the folder a session works in.
fn label(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map_or(cwd.into(), |name| name.to_string_lossy())
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::liveness::StartCheck;
    use crate::source::Start;
    use Phase::*;
    use Signal::*;

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    /// Read sessions leave after `keep_read_ms`, and abandoned ones after a week.
    fn keep(keep_read_ms: u64) -> Expiry {
        Expiry {
            keep_read_ms,
            abandon_ms: 7 * DAY_MS,
        }
    }

    fn key(id: &str) -> Key {
        Key {
            harness: Harness::Claude,
            id: id.into(),
        }
    }

    fn process(pid: i32) -> Liveness {
        Liveness::Process {
            pid,
            start: StartCheck::Second(1),
        }
    }

    fn started(id: &str, cwd: &str, liveness: Liveness) -> Observation {
        Observation {
            key: key(id),
            channel: Channel::File,
            event: Event::Started(Start {
                cwd: cwd.into(),
                liveness,
            }),
            turn: None,
            quiet: false,
        }
    }

    fn signal(id: &str, signal: Signal, turn: Option<&str>) -> Observation {
        Observation {
            key: key(id),
            channel: Channel::File,
            event: Event::Signal(signal),
            turn: turn.map(|turn| TurnKey(turn.into())),
            quiet: false,
        }
    }

    /// A tracked session told `signals` in turn at `now`, with what the last one changed.
    fn tell(queue: &mut Queue, id: &str, signals: &[Signal], now: u64) -> Change {
        if !queue.sessions.iter().any(|s| s.key.id == id) {
            queue.apply(&started(id, &format!("/Users/me/{id}"), process(1)), now);
        }
        let mut change = Change::None;
        for &s in signals {
            change = queue.apply(&signal(id, s, None), now);
        }
        change
    }

    /// Applies what `told` says of the queue as it is.
    fn apply_all(queue: &mut Queue, told: impl FnOnce(&Queue) -> Vec<Observation>, now: u64) {
        for o in told(queue) {
            queue.apply(&o, now);
        }
    }

    fn states(queue: &Queue) -> Vec<(&str, State)> {
        queue
            .items()
            .iter()
            .map(|s| (s.key.id.as_str(), s.phase.state().unwrap()))
            .collect()
    }

    fn phase(queue: &Queue, id: &str) -> Phase {
        queue
            .sessions
            .iter()
            .find(|s| s.key.id == id)
            .map_or(Gone, |s| s.phase)
    }

    #[test]
    fn the_queue_orders_errors_then_questions_then_stops_then_work() {
        let mut queue = Queue::default();
        tell(&mut queue, "a", &[Prompted], 1);
        tell(&mut queue, "b", &[TurnEnded], 2);
        tell(&mut queue, "c", &[TurnFailed], 3);
        tell(&mut queue, "d", &[TurnEnded], 4);
        tell(&mut queue, "e", &[QuestionAsked], 5);
        tell(&mut queue, "idle", &[], 6);
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
    fn a_signal_for_an_untracked_session_changes_nothing() {
        let mut queue = Queue::default();
        assert_eq!(queue.apply(&signal("a", Prompted, None), 1), Change::None);
        assert_eq!(queue.apply(&signal("a", Ended, None), 1), Change::None);
        assert!(!queue.tracked());
        assert_eq!(
            queue.apply(&started("a", "/Users/me/app", process(1)), 1),
            Change::Saved
        );
        assert!(queue.tracked());
        assert!(queue.items().is_empty());
        assert_eq!(queue.apply(&signal("a", Prompted, None), 2), Change::Settle);
        // Working again is nothing new.
        assert_eq!(queue.apply(&signal("a", Prompted, None), 3), Change::None);
        assert_eq!(
            queue.apply(&signal("a", TurnEnded, None), 4),
            Change::Announce(State::Stopped)
        );
        assert_eq!(queue.apply(&signal("a", Ended, None), 5), Change::Settle);
        assert!(!queue.tracked());
    }

    #[test]
    fn started_again_refreshes_the_label_and_liveness() {
        let mut queue = Queue::default();
        queue.apply(&started("a", "/Users/me/app", process(1)), 1);
        tell(&mut queue, "a", &[TurnEnded], 2);
        assert_eq!(
            queue.apply(&started("a", "/Users/me/app", process(1)), 3),
            Change::None
        );
        // Resumed in a new process, in another folder.
        assert_eq!(
            queue.apply(&started("a", "/Users/me/web", process(2)), 4),
            Change::Saved
        );
        let session = queue.items()[0];
        assert_eq!(session.label, "web");
        assert_eq!(session.liveness, process(2));
        // Restarting is not a new turn: the light and its age stay.
        assert_eq!(session.phase, Stopped);
        assert_eq!(session.since, 2);
        // A start that names no folder keeps the one known.
        queue.apply(&started("a", "", process(2)), 5);
        assert_eq!(queue.items()[0].label, "web");
    }

    #[test]
    fn quiet_observations_never_announce() {
        let mut queue = Queue::default();
        queue.apply(&started("a", "/Users/me/app", process(1)), 1);
        let mut ended = signal("a", TurnEnded, Some("t1"));
        ended.quiet = true;
        assert_eq!(queue.apply(&ended, 2), Change::Settle);
        ended.event = Event::Signal(TurnEndedAsking);
        assert_eq!(queue.apply(&ended, 3), Change::Settle);
        // A turn that ends again on the same light only moves it to the back.
        assert_eq!(queue.apply(&ended, 4), Change::Saved);
        assert_eq!(phase(&queue, "a"), Asked);
        assert_eq!(queue.items()[0].since, 4);
    }

    #[test]
    fn dismiss_by_prefix_and_dismiss_without_one_keeps_working_sessions() {
        let mut queue = Queue::default();
        tell(&mut queue, "a1", &[Prompted], 1);
        tell(&mut queue, "b1", &[TurnEnded], 1);
        tell(&mut queue, "c1", &[TurnFailed], 1);
        tell(&mut queue, "idle", &[], 1);
        let dismissed = queue.dismissed(None);
        assert_eq!(dismissed.len(), 2);
        apply_all(&mut queue, |_| dismissed, 2);
        assert_eq!(states(&queue), [("a1", State::Working)]);
        // Dismissed sessions stay tracked while their process runs.
        assert_eq!(phase(&queue, "b1"), Idle);
        apply_all(&mut queue, |queue| queue.dismissed(Some("a")), 3);
        assert!(queue.items().is_empty());
        assert!(queue.dismissed(None).is_empty());
        // Its next turn shows again.
        tell(&mut queue, "b1", &[Prompted], 4);
        assert_eq!(states(&queue), [("b1", State::Working)]);
    }

    #[test]
    fn a_question_or_an_error_beats_work_going_on_in_the_background() {
        let mut queue = Queue::default();
        tell(&mut queue, "done", &[TurnEnded], 1);
        tell(&mut queue, "asks", &[TurnEndedAsking], 1);
        tell(&mut queue, "fails", &[TurnFailed], 1);
        assert_eq!(tell(&mut queue, "done", &[Backgrounded], 2), Change::Settle);
        assert_eq!(tell(&mut queue, "asks", &[Backgrounded], 2), Change::None);
        assert_eq!(tell(&mut queue, "fails", &[Backgrounded], 2), Change::None);
        assert_eq!(
            states(&queue),
            [
                ("fails", State::Error),
                ("asks", State::Question),
                ("done", State::Working)
            ]
        );
        // The work it left running finishing is news.
        assert_eq!(
            tell(&mut queue, "done", &[BackgroundDone], 3),
            Change::Announce(State::Stopped)
        );
    }

    #[test]
    fn a_saved_queue_reloads_as_it_was() {
        let mut queue = Queue::default();
        tell(&mut queue, "a", &[Prompted], 5);
        tell(&mut queue, "b", &[TurnEnded], 6);
        tell(&mut queue, "idle", &[], 7);
        let reloaded: Queue =
            serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
        assert_eq!(states(&reloaded), states(&queue));
        assert!(reloaded.tracked());
        assert_eq!(reloaded.sessions.len(), 3);
        // What was last seen survives too, so silence is still measured from then.
        let abandoned = reloaded.due(6 + 7 * DAY_MS + 1, keep(0));
        assert_eq!(abandoned.len(), 2);
    }

    #[test]
    fn a_read_session_leaves_after_keep_read_minutes() {
        let looked_at = |id: &str, at: u64| HashMap::from([(id.to_owned(), at)]);
        let mut queue = Queue::default();
        tell(&mut queue, "done", &[TurnEnded], 100);
        tell(&mut queue, "asks", &[QuestionAsked], 100);
        assert_eq!(queue.unread_since(), Some(100));
        // Looking at it before it stopped does not count, and unread it stays.
        assert!(queue.looked(&looked_at("done", 99)).is_empty());
        assert!(queue.due(DAY_MS, keep(1000)).is_empty());
        // A prompt the agent is blocked on cannot be read away.
        assert!(queue.looked(&looked_at("asks", 150)).is_empty());
        let looked = queue.looked(&looked_at("done", 150));
        assert_eq!(looked.len(), 1);
        assert_eq!(queue.apply(&looked[0], 150), Change::Saved);
        assert_eq!(phase(&queue, "done"), StoppedRead);
        assert_eq!(queue.unread_since(), None);
        // The first look counts, not a later one.
        assert!(queue.looked(&looked_at("done", 900)).is_empty());
        assert!(queue.due(150 + 999, keep(1000)).is_empty());
        let due = queue.due(150 + 1000, keep(1000));
        apply_all(&mut queue, |_| due, 150 + 1000);
        assert_eq!(states(&queue), [("asks", State::Question)]);
        assert_eq!(phase(&queue, "done"), Idle);
    }

    #[test]
    fn a_weak_question_is_settled_by_reading_and_a_blocked_one_is_not() {
        let looked_at = HashMap::from([("offer".to_owned(), 20), ("prompt".to_owned(), 20)]);
        let mut queue = Queue::default();
        assert_eq!(
            tell(&mut queue, "offer", &[TurnEndedAsking], 10),
            Change::Announce(State::Question)
        );
        tell(&mut queue, "prompt", &[PermissionAsked], 10);
        apply_all(&mut queue, |queue| queue.looked(&looked_at), 20);
        assert_eq!(phase(&queue, "offer"), AskedRead);
        assert_eq!(phase(&queue, "prompt"), Blocked);
        apply_all(&mut queue, |queue| queue.due(20, keep(0)), 20);
        assert_eq!(states(&queue), [("prompt", State::Question)]);
        // A wait in a session that had only asked in passing is the stronger signal.
        tell(&mut queue, "offer", &[TurnEndedAsking], 30);
        assert_eq!(
            tell(&mut queue, "offer", &[QuestionAsked], 31),
            Change::Announce(State::Question)
        );
        assert_eq!(queue.unread_since(), None);
    }

    #[test]
    fn a_turn_that_ends_again_is_announced_again_and_loses_its_read_mark() {
        let mut queue = Queue::default();
        tell(&mut queue, "a", &[TurnEnded], 1);
        let looked = queue.looked(&HashMap::from([("a".to_owned(), 2)]));
        apply_all(&mut queue, |_| looked, 2);
        assert_eq!(phase(&queue, "a"), StoppedRead);
        // Its turn ends again with nothing reported in between: there is something new to read.
        assert_eq!(
            tell(&mut queue, "a", &[TurnEnded], 4),
            Change::Announce(State::Stopped)
        );
        assert_eq!(queue.unread_since(), Some(4));
        assert!(queue.due(DAY_MS, keep(0)).is_empty());
    }

    #[test]
    fn a_shown_session_goes_idle_after_abandon_days_and_stays_tracked() {
        let expiry = Expiry {
            keep_read_ms: 0,
            abandon_ms: 3 * DAY_MS,
        };
        let mut queue = Queue::default();
        tell(&mut queue, "error", &[TurnFailed], 0);
        tell(&mut queue, "prompt", &[QuestionAsked], 0);
        tell(&mut queue, "busy", &[Prompted], 0);
        tell(&mut queue, "later", &[TurnEnded], DAY_MS);
        assert!(queue.due(3 * DAY_MS, expiry).is_empty());
        let due = queue.due(3 * DAY_MS + 1, expiry);
        assert_eq!(due.len(), 3);
        apply_all(&mut queue, |_| due, 3 * DAY_MS + 1);
        assert_eq!(states(&queue), [("later", State::Stopped)]);
        assert_eq!(phase(&queue, "busy"), Idle);
        // Its next turn shows again.
        tell(&mut queue, "busy", &[Prompted], 4 * DAY_MS);
        assert_eq!(phase(&queue, "busy"), Working);
    }

    #[test]
    fn a_session_whose_process_is_gone_ends_through_the_channel_that_started_it() {
        let mut queue = Queue::default();
        queue.apply(&started("files", "/Users/me/app", process(1)), 1);
        let mut hooked = started("hooked", "/Users/me/app", process(2));
        hooked.channel = Channel::Hook;
        queue.apply(&hooked, 1);
        tell(&mut queue, "files", &[Prompted], 2);
        tell(&mut queue, "hooked", &[TurnEnded], 2);
        assert!(queue.sweep(|_| true).is_empty());
        let swept = queue.sweep(|liveness| *liveness != process(2));
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].key, key("hooked"));
        assert_eq!(swept[0].channel, Channel::Hook);
        assert_eq!(queue.bound(), [(key("hooked"), process(2))]);
        apply_all(&mut queue, |_| swept, 3);
        assert_eq!(states(&queue), [("files", State::Working)]);
        assert!(queue.bound().is_empty());
    }

    /// A queue of one session: started, then told `signals` on turn `t1`.
    fn one(signals: &[Signal], looked: bool, dismissed: bool) -> Queue {
        let mut queue = Queue::default();
        queue.apply(&started("a", "/Users/me/app", process(1)), 1);
        for &s in signals {
            queue.apply(&signal("a", s, Some("t1")), 2);
        }
        if looked {
            apply_all(
                &mut queue,
                |queue| queue.looked(&HashMap::from([("a".into(), 3)])),
                3,
            );
        }
        if dismissed {
            apply_all(&mut queue, |queue| queue.dismissed(None), 3);
        }
        queue
    }

    /// What the files show at a restart: the session started again, quietly.
    fn seeded(signals: &[Signal], turn: &str) -> Queue {
        let mut queue = Queue::default();
        queue.apply(&started("a", "/Users/me/app", process(1)), 100);
        for &s in signals {
            let mut o = signal("a", s, Some(turn));
            o.quiet = true;
            queue.apply(&o, 100);
        }
        queue
    }

    #[test]
    fn a_restart_keeps_a_read_mark_for_the_same_turn() {
        let restored = Queue::restore(one(&[TurnEnded], true, false), seeded(&[TurnEnded], "t1"));
        assert_eq!(phase(&restored, "a"), StoppedRead);
        let session = &restored.sessions[0];
        assert_eq!((session.since, session.entered), (2, 3));
    }

    #[test]
    fn a_restart_keeps_a_dismissal_for_the_same_turn() {
        let restored = Queue::restore(
            one(&[TurnEndedAsking], false, true),
            seeded(&[TurnEndedAsking], "t1"),
        );
        assert_eq!(phase(&restored, "a"), Idle);
        assert!(restored.items().is_empty());
    }

    #[test]
    fn a_turn_that_ended_while_the_daemon_was_down_shows_unread() {
        // Read, then a new turn ran and ended while nothing was watching.
        let restored = Queue::restore(one(&[TurnEnded], true, false), seeded(&[TurnEnded], "t2"));
        assert_eq!(phase(&restored, "a"), Stopped);
        assert_eq!(restored.unread_since(), Some(100));
    }

    #[test]
    fn a_saved_working_session_whose_turn_was_interrupted_takes_the_seed() {
        // Working is not reachable from Idle by the daemon's signals alone.
        let restored = Queue::restore(one(&[Prompted], false, false), seeded(&[], "t1"));
        assert_eq!(phase(&restored, "a"), Idle);
        // A session whose process is gone is not seeded, and goes.
        let restored = Queue::restore(one(&[TurnEnded], false, false), Queue::default());
        assert!(!restored.tracked());
    }
}
