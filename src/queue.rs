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
use crate::harness::{self, Harness};
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
    /// The channel and the phase its last move took it from, when a hook
    /// or the daemon made that move. Its files, which neither writes to,
    /// may show it there still.
    pub moved: Option<(Channel, Phase)>,
    /// While it is blocked, the phase it was blocked from, which a released
    /// wait goes back to.
    pub held: Option<Phase>,
}

/// The sessions, as saved in queue.json. Tracked sessions without a light
/// are kept too, so the sweep can tell when their process ends.
#[derive(Clone, Default, Serialize, Deserialize)]
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

/// What was wrong with an observation `Queue::admit` was given.
#[derive(Debug, PartialEq)]
pub enum Problem {
    /// Signals its harness does not declare through its channel. Nothing
    /// was applied.
    Undeclared(Vec<Signal>),
    /// Signals that overlap from `from`. The table's order chose `winner`,
    /// and that was applied.
    Overlap { from: Phase, winner: Signal },
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
    /// Applies an observation from a source, if its harness declares every
    /// signal in it, and says what was wrong with it. One that is not
    /// declared is dropped: a source produced something its harness does
    /// not claim. One whose signals overlap is applied as the table's order
    /// says: a source told what it should not together, as when a
    /// harness's format changes.
    pub fn admit(&mut self, o: &Observation, now: u64) -> (Change, Option<Problem>) {
        let undeclared = o.undeclared();
        if !undeclared.is_empty() {
            return (Change::None, Some(Problem::Undeclared(undeclared)));
        }
        let from = self.phase(&o.key);
        let signals = o.event.signals();
        let overlap = fsm::overlaps(from, &signals).then(|| Problem::Overlap {
            from,
            winner: fsm::next(from, &signals)
                .expect("an overlap has a winner")
                .on,
        });
        (self.apply(o, now), overlap)
    }

    /// Moves a session by the table. The only place phases change.
    pub fn apply(&mut self, o: &Observation, now: u64) -> Change {
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
        let Some(rule) = fsm::next(from, &o.event.signals()) else {
            return Change::None;
        };
        let held = position.and_then(|index| self.sessions[index].held);
        let to = rule.to.phase(held.unwrap_or(Phase::Idle));
        let announce = fsm::announces(from, rule.on, to);
        // A row that holds the phase changes nothing.
        if to == from && !announce {
            return Change::None;
        }
        let light = to.state() != from.state();
        let moved = (o.channel != Channel::File).then_some((o.channel, from));
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
                moved,
                held: None,
            }),
            (None, Event::Signals(_)) => unreachable!("only Started leaves Gone"),
            (Some(index), _) if to == Phase::Gone => {
                self.sessions.remove(index);
            }
            (Some(index), _) => {
                let session = &mut self.sessions[index];
                session.phase = to;
                session.moved = moved;
                session.held = match to {
                    Phase::Blocked => held.or(Some(from)),
                    _ => None,
                };
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

    /// Where a session stands: Gone if it is not tracked.
    pub fn phase(&self, key: &Key) -> Phase {
        let session = self.sessions.iter().find(|s| s.key == *key);
        session.map_or(Phase::Gone, |s| s.phase)
    }

    /// Every session, with a light or not.
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
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

    /// When the oldest session the daemon can tell is read, and that has
    /// not been read since its turn ended, stopped.
    pub fn unread_since(&self) -> Option<u64> {
        let unread = self.sessions.iter().filter(|s| readable(s));
        unread.map(|s| s.since).min()
    }

    /// Whether any session is followed at all, with a light or not.
    pub fn tracked(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// The unread sessions the user has looked at since their light took
    /// its state, given when each was last looked at, of the harnesses whose
    /// reading the daemon tells: Claude Code's, in the desktop app.
    pub fn looked(&self, at: &HashMap<String, u64>) -> Vec<Observation> {
        self.sessions
            .iter()
            .filter(|s| readable(s))
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
                event: Event::one(Signal::Ended),
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
    /// keeping a saved session's phase when it is about the same turn and
    /// either the daemon alone could have taken it there, from where its
    /// last move by the daemon found it, as with a read mark or a
    /// dismissal, or a hook took it there from where the daemon alone could
    /// take what the files still show, as with a dialog open, a failure only
    /// a hook tells, or a prompt after a dismissal. A turn that ended while
    /// the daemon was down shows unread, a dismissed working session's too;
    /// a wait that began then shows, while a wait that was dismissed stays
    /// so; and a session whose process is gone is not in `fresh` at all. A
    /// session only its hooks told of, which no file can contradict, is kept
    /// as it was saved.
    pub fn restore(saved: Queue, fresh: Queue) -> Queue {
        let daemon: Vec<Signal> = Signal::ALL.into_iter().filter(|s| s.daemon()).collect();
        let mut sessions = fresh.sessions;
        for session in &mut sessions {
            let Some(old) = saved.sessions.iter().find(|old| old.key == session.key) else {
                continue;
            };
            if old.bound == Channel::Hook && old.turn.is_none() && session.turn.is_none() {
                *session = old.clone();
                continue;
            }
            if old.turn != session.turn {
                continue;
            }
            // Nothing new was heard of it, so its silence goes on.
            session.seen = old.seen;
            if old.turn.is_none() {
                continue;
            }
            // Whether the saved phase still stands where the files show the
            // session at `phase`: the daemon could have taken it from there
            // to where its own last move found it, or a hook's, or to the
            // saved phase itself when neither made that move.
            let stands = |phase: Phase| {
                let reached = fsm::reachable(phase, &daemon);
                match old.moved {
                    Some((Channel::Daemon, from)) => reached.contains(&from),
                    Some((_, from)) => reached.contains(&old.phase) || reached.contains(&from),
                    None => reached.contains(&old.phase),
                }
            };
            // A wait that began while the daemon was down shows, and lets go
            // back to where the daemon left the session, a read mark or a
            // dismissal included, if that still stands where the wait found
            // it. A wait the user dismissed is not one.
            let dismissed = old.moved == Some((Channel::Daemon, Phase::Blocked));
            if session.phase == Phase::Blocked && old.phase != Phase::Blocked && !dismissed {
                if let Some(held) = session.held
                    && stands(held)
                {
                    session.held = Some(old.phase);
                }
                continue;
            }
            if stands(session.phase) {
                session.phase = old.phase;
                session.held = old.held;
                session.since = old.since;
                session.entered = old.entered;
            }
        }
        Queue { sessions }
    }
}

/// Whether a session is unread, and of a harness whose reading the daemon
/// tells.
fn readable(session: &Session) -> bool {
    session.phase.unread()
        && harness::declares(session.key.harness, Signal::Looked, Channel::Daemon)
}

/// A signal the daemon tells about a session.
fn told(session: &Session, signal: Signal) -> Observation {
    Observation {
        key: session.key.clone(),
        channel: Channel::Daemon,
        event: Event::one(signal),
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
            event: Event::one(signal),
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
        queue.phase(&key(id))
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
    fn admitting_drops_what_is_undeclared_and_applies_an_overlap_by_the_table() {
        let mut queue = Queue::default();
        tell(&mut queue, "a", &[Prompted], 1);
        // Claude Code declares no hooks.
        let mut hooked = signal("a", TurnEnded, None);
        hooked.channel = Channel::Hook;
        assert_eq!(
            queue.admit(&hooked, 2),
            (Change::None, Some(Problem::Undeclared(vec![TurnEnded])))
        );
        assert_eq!(phase(&queue, "a"), Working);
        let mut both = signal("a", TurnEnded, None);
        both.event = Event::Signals([TurnEnded, PermissionAsked].into());
        let (change, problem) = queue.admit(&both, 3);
        assert_eq!(change, Change::Announce(State::Question));
        assert_eq!(
            problem,
            Some(Problem::Overlap {
                from: Working,
                winner: PermissionAsked
            })
        );
        assert_eq!(phase(&queue, "a"), Blocked);
        assert_eq!(queue.admit(&signal("a", Answered, None), 4).1, None);
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
    fn a_released_wait_goes_back_to_where_it_found_the_session_unannounced() {
        let mut queue = Queue::default();
        // What the daemon did to it while the source looked away counts.
        for (id, before) in [
            ("read", &[TurnEnded, Looked][..]),
            ("dismissed", &[TurnEnded, Dismissed]),
            ("unread", &[TurnEnded]),
            ("failed", &[TurnFailed]),
        ] {
            tell(&mut queue, id, before, 1);
            let found = phase(&queue, id);
            tell(&mut queue, id, &[PermissionAsked, QuestionAsked], 2);
            assert_eq!(phase(&queue, id), Phase::Blocked);
            assert_eq!(tell(&mut queue, id, &[Released], 3), Change::Settle, "{id}");
            assert_eq!(phase(&queue, id), found, "{id}");
            assert_eq!(
                queue
                    .sessions()
                    .iter()
                    .find(|s| s.key.id == id)
                    .unwrap()
                    .held,
                None
            );
        }
        assert_eq!(phase(&queue, "read"), Phase::StoppedRead);
        assert_eq!(phase(&queue, "dismissed"), Phase::Idle);
        // A wait answered in a turn goes on with the turn.
        tell(
            &mut queue,
            "turn",
            &[Prompted, PermissionAsked, Answered],
            4,
        );
        assert_eq!(phase(&queue, "turn"), Phase::Working);
    }

    #[test]
    fn quiet_observations_never_announce() {
        let mut queue = Queue::default();
        queue.apply(&started("a", "/Users/me/app", process(1)), 1);
        let mut ended = signal("a", TurnEnded, Some("t1"));
        ended.quiet = true;
        assert_eq!(queue.apply(&ended, 2), Change::Settle);
        ended.event = Event::one(TurnEndedAsking);
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
        tell(&mut queue, "waits", &[PermissionAsked], 1);
        assert_eq!(tell(&mut queue, "done", &[Backgrounded], 2), Change::Settle);
        assert_eq!(tell(&mut queue, "asks", &[Backgrounded], 2), Change::None);
        assert_eq!(tell(&mut queue, "fails", &[Backgrounded], 2), Change::None);
        // A subagent at work while the user is asked to approve something.
        assert_eq!(tell(&mut queue, "waits", &[Backgrounded], 2), Change::None);
        assert_eq!(
            states(&queue),
            [
                ("fails", State::Error),
                ("asks", State::Question),
                ("waits", State::Question),
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
    fn a_restart_measures_silence_from_what_was_last_heard() {
        let expiry = Expiry {
            keep_read_ms: 0,
            abandon_ms: 3 * DAY_MS,
        };
        let mut saved = one(&[Prompted], false, false);
        saved.sessions[0].seen = 2;
        let mut fresh = Queue::default();
        fresh.apply(&started("a", "/Users/me/app", process(1)), DAY_MS);
        let mut o = signal("a", Prompted, Some("t1"));
        o.quiet = true;
        fresh.apply(&o, DAY_MS);
        let restored = Queue::restore(saved, fresh);
        assert_eq!(restored.due(3 * DAY_MS + 3, expiry).len(), 1);
        // A turn the files show and the saved queue does not was heard just now.
        let restored = Queue::restore(one(&[Prompted], false, false), seeded(&[Prompted], "t2"));
        assert_eq!(restored.sessions[0].seen, 100);
    }

    #[test]
    fn a_restart_keeps_a_session_only_its_hooks_told_of() {
        let mut saved = Queue::default();
        let mut hooked = started("a", "/Users/me/app", process(1));
        hooked.channel = Channel::Hook;
        saved.apply(&hooked, 1);
        let mut ended = signal("a", TurnEnded, None);
        ended.channel = Channel::Hook;
        saved.apply(&ended, 2);
        // Its source starts it again, knowing nothing of where it stood.
        let mut fresh = Queue::default();
        hooked.event = Event::Started(Start {
            cwd: String::new(),
            liveness: process(1),
        });
        hooked.quiet = true;
        fresh.apply(&hooked, 100);
        let restored = Queue::restore(saved, fresh);
        assert_eq!(states(&restored), [("a", State::Stopped)]);
        let session = &restored.sessions[0];
        assert_eq!(
            (session.label.as_str(), session.since, session.seen),
            ("app", 2, 2)
        );
    }

    #[test]
    fn a_restart_keeps_what_a_hook_told_of_a_turn_its_files_still_show_where_it_found_it() {
        let hooked = |signal: Signal| {
            let mut saved = one(&[Prompted], false, false);
            let mut o = self::signal("a", signal, Some("t1"));
            o.channel = Channel::Hook;
            saved.apply(&o, 3);
            saved
        };
        // A dialog still open, and a failure the files cannot tell is final.
        let restored = Queue::restore(hooked(QuestionAsked), seeded(&[Prompted], "t1"));
        assert_eq!(phase(&restored, "a"), Blocked);
        let restored = Queue::restore(hooked(TurnFailed), seeded(&[Prompted], "t1"));
        assert_eq!(phase(&restored, "a"), Failed);
        // The turn went on to end while the daemon was down.
        let restored = Queue::restore(hooked(QuestionAsked), seeded(&[TurnEnded], "t1"));
        assert_eq!(phase(&restored, "a"), Stopped);
        // A prompt a hook told after the session was dismissed, before
        // the files say the turn began.
        let mut saved = one(&[TurnEnded], false, true);
        let mut prompted = signal("a", Prompted, None);
        prompted.channel = Channel::Hook;
        saved.apply(&prompted, 4);
        let restored = Queue::restore(saved, seeded(&[TurnEnded], "t1"));
        assert_eq!(phase(&restored, "a"), Working);
        // What the files told is theirs to tell again.
        let restored = Queue::restore(one(&[TurnFailed], false, false), seeded(&[Prompted], "t1"));
        assert_eq!(phase(&restored, "a"), Working);
    }

    /// A wait that began after the turn while the daemon was down, as a
    /// worker request does, shows, and lets go back to the read mark or the
    /// dismissal the session had, as it would had the daemon been up.
    #[test]
    fn a_wait_that_began_while_the_daemon_was_down_shows_and_lets_go_back() {
        for (looked, dismissed, back) in [(true, false, StoppedRead), (false, true, Idle)] {
            let saved = one(&[TurnEnded], looked, dismissed);
            let mut restored = Queue::restore(saved, seeded(&[TurnEnded, PermissionAsked], "t1"));
            assert_eq!(phase(&restored, "a"), Blocked);
            restored.apply(&signal("a", Released, Some("t1")), 200);
            assert_eq!(phase(&restored, "a"), back);
        }
        // The same wait, open before the daemon stopped, keeps what it held.
        let mut saved = one(&[TurnEnded], true, false);
        saved.apply(&signal("a", PermissionAsked, Some("t1")), 4);
        let mut restored = Queue::restore(saved, seeded(&[TurnEnded, PermissionAsked], "t1"));
        restored.apply(&signal("a", Released, Some("t1")), 200);
        assert_eq!(phase(&restored, "a"), StoppedRead);
    }

    /// A dismissal or an abandon while the session worked covers only that
    /// work: a turn that ended while the daemon was down shows unread.
    #[test]
    fn a_turn_that_ended_after_a_dismissal_of_its_work_shows_unread() {
        // `agent-leds dismiss a` clears a working session too.
        let mut saved = one(&[Prompted], false, false);
        apply_all(&mut saved, |queue| queue.dismissed(Some("a")), 3);
        assert_eq!(phase(&saved, "a"), Idle);
        let restored = Queue::restore(saved.clone(), seeded(&[TurnEnded], "t1"));
        assert_eq!(phase(&restored, "a"), Stopped);
        assert_eq!(restored.unread_since(), Some(100));
        // While the turn still runs, the dismissal stands.
        let restored = Queue::restore(saved, seeded(&[Prompted], "t1"));
        assert_eq!(phase(&restored, "a"), Idle);
        // A dismissal, then a read mark's expiry, of a stopped turn stands.
        let read = one(&[TurnEnded], true, false);
        let mut expired = read.clone();
        apply_all(&mut expired, |queue| queue.due(10, keep(0)), 10);
        assert_eq!(phase(&expired, "a"), Idle);
        let restored = Queue::restore(expired, seeded(&[TurnEnded], "t1"));
        assert_eq!(phase(&restored, "a"), Idle);
    }

    /// A wait the user dismissed stays dismissed while it is still open.
    #[test]
    fn a_dismissed_wait_still_open_at_a_restart_stays_dismissed() {
        let saved = one(&[TurnEnded, PermissionAsked], false, true);
        assert_eq!(phase(&saved, "a"), Idle);
        let restored = Queue::restore(saved, seeded(&[TurnEnded, PermissionAsked], "t1"));
        assert_eq!(phase(&restored, "a"), Idle);
        assert!(restored.items().is_empty());
    }

    /// Only a session the daemon can tell is read keeps it looking.
    #[test]
    fn only_a_session_the_daemon_can_tell_is_read_keeps_it_looking() {
        let mut queue = one(&[TurnEnded], false, false);
        let mut codex = started("b", "/Users/me/app", process(2));
        codex.key.harness = Harness::Codex;
        let mut ended = signal("b", TurnEnded, Some("t1"));
        ended.key.harness = Harness::Codex;
        queue.apply(&codex, 1);
        queue.apply(&ended, 1);
        assert_eq!(queue.unread_since(), Some(2));
        apply_all(&mut queue, |queue| queue.dismissed(Some("a")), 3);
        assert_eq!(queue.unread_since(), None);
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
