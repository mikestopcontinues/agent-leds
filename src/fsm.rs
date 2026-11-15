//! The life of one session as a state machine: the phases it can be in, the
//! signals that move it, and the moves themselves, written as data so the
//! coverage document can cross them with what each harness produces.
//!
//! Nothing here reads a file or a clock. A guess, if one is ever wanted, is a
//! named signal with rows of its own here, never a hidden timer.

use std::collections::{BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::state::State;

use Phase::*;
use Signal::*;

/// Where one session stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Not tracked: no process of the harness is known to run it. Never stored.
    Gone,
    /// Tracked and alive, nothing to show: no turn yet, interrupted, dismissed,
    /// read long enough ago, or abandoned.
    Idle,
    /// A turn is running.
    Working,
    /// The turn ended but work it started goes on, such as a background
    /// subagent. It shows as working.
    Background,
    /// Held up on the user by a permission or a question: the strong question.
    Blocked,
    /// The turn ended on a message ending in '?' or on a plan proposal: the
    /// weak question.
    Asked,
    /// A weak question the user has looked at.
    AskedRead,
    /// The turn ended and the user has not looked yet.
    Stopped,
    /// A stopped turn the user has looked at.
    StoppedRead,
    /// The turn ended on an error.
    Failed,
}

impl Phase {
    /// Every phase, in declaration order.
    pub const ALL: [Phase; 10] = [
        Gone,
        Idle,
        Working,
        Background,
        Blocked,
        Asked,
        AskedRead,
        Stopped,
        StoppedRead,
        Failed,
    ];

    /// Where the phase sits in `ALL`. The match has no wildcard, so a new
    /// phase does not compile until it is placed here, and a test then holds
    /// it to its place in `ALL`.
    #[cfg(test)]
    pub const fn index(self) -> usize {
        match self {
            Gone => 0,
            Idle => 1,
            Working => 2,
            Background => 3,
            Blocked => 4,
            Asked => 5,
            AskedRead => 6,
            Stopped => 7,
            StoppedRead => 8,
            Failed => 9,
        }
    }

    /// The light the phase shows, if any.
    pub fn state(self) -> Option<State> {
        match self {
            Gone | Idle => None,
            Working | Background => Some(State::Working),
            Blocked | Asked | AskedRead => Some(State::Question),
            Stopped | StoppedRead => Some(State::Stopped),
            Failed => Some(State::Error),
        }
    }

    /// Whether entering the phase wants the user, so the lights play the
    /// snake rather than the wave.
    pub fn wants_user(self) -> bool {
        matches!(self, Blocked | Asked | Stopped | Failed)
    }

    /// Whether the phase is settled by reading and has not been read yet.
    pub fn unread(self) -> bool {
        matches!(self, Stopped | Asked)
    }
}

/// Something that happened to a session, told by its harness or by the
/// daemon itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Signal {
    /// A process of the harness runs the session.
    Started,
    /// A turn started, by the user or by the harness waking itself.
    Prompted,
    /// A tool waits for the user's approval.
    PermissionAsked,
    /// The agent waits for an answer to its question, or a dialog is open.
    QuestionAsked,
    /// The approval or question was resolved and the turn goes on.
    Answered,
    /// The turn completed on a message that is not a question.
    TurnEnded,
    /// The turn completed on a message ending in '?' or on a plan proposal.
    TurnEndedAsking,
    /// The turn ended on an error.
    TurnFailed,
    /// The user stopped the turn.
    Interrupted,
    /// The turn ended while work it started runs on.
    Backgrounded,
    /// That work finished without starting a new turn.
    BackgroundDone,
    /// The process is gone, by quitting or crashing, or the session was deleted.
    Ended,
    /// The user looked at the session.
    Looked,
    /// The session was read longer ago than `keep_read_minutes`.
    ReadExpired,
    /// The session has been silent longer than `abandon_days`.
    Abandoned,
    /// `agent-leds dismiss` cleared the session.
    Dismissed,
}

impl Signal {
    /// Every signal, in declaration order.
    pub const ALL: [Signal; 16] = [
        Started,
        Prompted,
        PermissionAsked,
        QuestionAsked,
        Answered,
        TurnEnded,
        TurnEndedAsking,
        TurnFailed,
        Interrupted,
        Backgrounded,
        BackgroundDone,
        Ended,
        Looked,
        ReadExpired,
        Abandoned,
        Dismissed,
    ];

    /// Where the signal sits in `ALL`, exhaustive as `Phase::index` is.
    #[cfg(test)]
    pub const fn index(self) -> usize {
        match self {
            Started => 0,
            Prompted => 1,
            PermissionAsked => 2,
            QuestionAsked => 3,
            Answered => 4,
            TurnEnded => 5,
            TurnEndedAsking => 6,
            TurnFailed => 7,
            Interrupted => 8,
            Backgrounded => 9,
            BackgroundDone => 10,
            Ended => 11,
            Looked => 12,
            ReadExpired => 13,
            Abandoned => 14,
            Dismissed => 15,
        }
    }

    /// Whether the daemon tells it, rather than a harness.
    pub fn daemon(self) -> bool {
        matches!(self, Looked | ReadExpired | Abandoned | Dismissed)
    }

    /// What the user sees when a harness cannot produce the signal, for the
    /// coverage document.
    pub fn when_missing(self) -> &'static str {
        match self {
            Started => {
                "the harness's sessions never show, since every other signal needs a tracked session"
            }
            Prompted => {
                "a new turn never shows as working, and keeps the light it had until the turn waits or ends"
            }
            PermissionAsked => "an approval wait shows as the light it interrupts",
            QuestionAsked => "a question the agent waits on shows as the light it interrupts",
            Answered => "an answered approval or question shows as a question until the turn ends",
            TurnEnded => {
                "a finished turn keeps its light, usually working, until the session moves on"
            }
            TurnEndedAsking => "a turn that ends on a question shows as stopped, not as a question",
            TurnFailed => "a turn that ends on an error never shows the error light",
            Interrupted => {
                "an interrupted turn keeps its light until the next turn or the process ends"
            }
            Backgrounded => {
                "a turn whose background work runs on shows as stopped while that work runs"
            }
            BackgroundDone => {
                "background work that finishes is never announced, and shows as working until the next turn"
            }
            Ended => "a crashed or quit session keeps its light until abandon_days or dismiss",
            Looked => "looking at a session never marks it read",
            ReadExpired => {
                "a read session keeps its light until its next turn, its process ends, or dismiss"
            }
            Abandoned => "a session left silent keeps its light until its process ends or dismiss",
            Dismissed => "dismiss cannot clear the session's light",
        }
    }
}

/// The way an observation reached the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// A file the harness writes.
    File,
    /// A hook or plugin of ours that the harness runs.
    Hook,
    /// The daemon's own reckoning: looks, expiry and dismissal.
    Daemon,
}

/// One row of the table: `on` moves a session in any of `from` to `to`.
pub struct Rule {
    pub from: &'static [Phase],
    pub on: Signal,
    pub to: Phase,
}

/// Every phase but `Gone`.
const TRACKED: &[Phase] = &[
    Idle,
    Working,
    Background,
    Blocked,
    Asked,
    AskedRead,
    Stopped,
    StoppedRead,
    Failed,
];

/// Every phase with a light.
const SHOWN: &[Phase] = &[
    Working,
    Background,
    Blocked,
    Asked,
    AskedRead,
    Stopped,
    StoppedRead,
    Failed,
];

/// Every move a session can make, in priority order. One record can tell
/// several signals, each a fact of its own, and the first row here that
/// matches the session's phase and one of them is the move. A pair of
/// phase and signal with no row changes nothing; a row that leaves the
/// phase as it is holds the session there against the rows after it.
pub const TABLE: &[Rule] = &[
    Rule {
        from: TRACKED,
        on: Ended,
        to: Gone,
    },
    // A turn the user stopped is no failure, though OpenCode tells the stop
    // as an error, MessageAbortedError.
    Rule {
        from: TRACKED,
        on: Interrupted,
        to: Idle,
    },
    // A second failure is not news: Failed holds, and is not announced again.
    Rule {
        from: TRACKED,
        on: TurnFailed,
        to: Failed,
    },
    Rule {
        from: TRACKED,
        on: PermissionAsked,
        to: Blocked,
    },
    Rule {
        from: TRACKED,
        on: QuestionAsked,
        to: Blocked,
    },
    Rule {
        from: &[Blocked],
        on: Answered,
        to: Working,
    },
    // A question beats work going on in the background.
    Rule {
        from: TRACKED,
        on: TurnEndedAsking,
        to: Asked,
    },
    Rule {
        from: &[Idle, Working, Background, Stopped, StoppedRead],
        on: Backgrounded,
        to: Background,
    },
    // A wait on the user, a question or an error beats work going on in
    // the background, and holds against a turn end told with it.
    Rule {
        from: &[Blocked],
        on: Backgrounded,
        to: Blocked,
    },
    Rule {
        from: &[Asked],
        on: Backgrounded,
        to: Asked,
    },
    Rule {
        from: &[AskedRead],
        on: Backgrounded,
        to: AskedRead,
    },
    Rule {
        from: &[Failed],
        on: Backgrounded,
        to: Failed,
    },
    Rule {
        from: &[Background],
        on: BackgroundDone,
        to: Stopped,
    },
    Rule {
        from: TRACKED,
        on: TurnEnded,
        to: Stopped,
    },
    Rule {
        from: &[
            Idle,
            Background,
            Blocked,
            Asked,
            AskedRead,
            Stopped,
            StoppedRead,
            Failed,
        ],
        on: Prompted,
        to: Working,
    },
    Rule {
        from: &[Gone],
        on: Started,
        to: Idle,
    },
    Rule {
        from: SHOWN,
        on: Dismissed,
        to: Idle,
    },
    Rule {
        from: SHOWN,
        on: Abandoned,
        to: Idle,
    },
    Rule {
        from: &[StoppedRead, AskedRead],
        on: ReadExpired,
        to: Idle,
    },
    // Reading settles a weak question, never a blocked one.
    Rule {
        from: &[Stopped],
        on: Looked,
        to: StoppedRead,
    },
    Rule {
        from: &[Asked],
        on: Looked,
        to: AskedRead,
    },
];

/// The signals one record can tell together, each family a set of facts
/// that are true at once of a sound session. Signals told together that
/// share no family overlap: the table still decides, and the daemon counts
/// and logs it, since a source told it something it did not expect.
pub const FAMILIES: &[&[Signal]] = &[
    // How a turn ended: on a question, with work running on, on an error,
    // or cut short by the user, all of them an end.
    &[
        TurnEnded,
        TurnEndedAsking,
        Backgrounded,
        TurnFailed,
        Interrupted,
    ],
    // A turn held up on the user. Prompted belongs here because a turn can
    // start already held up: Claude Code writes a prompt after its session
    // file says it waits when a permission is asked at once, and a hook
    // that runs a tool that asks says that the turn runs too.
    &[Prompted, PermissionAsked, QuestionAsked],
];

/// The move the signals, told together, make from `from`: the first row of
/// the table that matches. None means they change nothing.
pub fn next(from: Phase, signals: &BTreeSet<Signal>) -> Option<&'static Rule> {
    first(TABLE, from, signals)
}

/// The phase one signal told alone moves to.
pub fn step(from: Phase, on: Signal) -> Option<Phase> {
    next(from, &BTreeSet::from([on])).map(|rule| rule.to)
}

fn first<'a>(table: &'a [Rule], from: Phase, signals: &BTreeSet<Signal>) -> Option<&'a Rule> {
    table
        .iter()
        .find(|rule| signals.contains(&rule.on) && rule.from.contains(&from))
}

/// Whether the signals, told together to a session in `from`, overlap:
/// more than one row matches, and one that loses is of no family with the
/// one that wins.
pub fn overlaps(from: Phase, signals: &BTreeSet<Signal>) -> bool {
    let mut matching = TABLE
        .iter()
        .filter(|rule| signals.contains(&rule.on) && rule.from.contains(&from));
    let Some(winner) = matching.next() else {
        return false;
    };
    matching.any(|loser| {
        !FAMILIES
            .iter()
            .any(|family| family.contains(&winner.on) && family.contains(&loser.on))
    })
}

/// The signals in the order the table ranks them, each once.
pub fn priority() -> Vec<Signal> {
    let mut order = Vec::new();
    for rule in TABLE {
        if !order.contains(&rule.on) {
            order.push(rule.on);
        }
    }
    order
}

/// Whether the move is news the user should see. A turn that ends again is
/// news, even to a session that had already stopped; a wait or a failure
/// told again is not.
pub fn announces(from: Phase, on: Signal, to: Phase) -> bool {
    to.wants_user() && (to != from || matches!(on, TurnEnded | TurnEndedAsking))
}

/// Whether applying the signal twice is the same as applying it once, in
/// phase and in what is announced.
#[cfg(test)]
pub fn idempotent(on: Signal) -> bool {
    Phase::ALL
        .into_iter()
        .filter_map(|from| step(from, on))
        .all(|once| match step(once, on) {
            None => true,
            Some(twice) => twice == once && !announces(once, on, twice),
        })
}

/// The phases a session in `from` can reach through `signals` alone,
/// `from` included.
pub fn reachable(from: Phase, signals: &[Signal]) -> BTreeSet<Phase> {
    let mut reached = BTreeSet::from([from]);
    let mut queue = VecDeque::from([from]);
    while let Some(phase) = queue.pop_front() {
        for &signal in signals {
            if let Some(to) = step(phase, signal)
                && reached.insert(to)
            {
                queue.push_back(to);
            }
        }
    }
    reached
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_signals() -> Vec<Signal> {
        Signal::ALL.into_iter().filter(|s| s.daemon()).collect()
    }

    fn told(signals: &[Signal]) -> BTreeSet<Signal> {
        signals.iter().copied().collect()
    }

    #[test]
    fn every_row_of_the_table_can_win() {
        for (index, rule) in TABLE.iter().enumerate() {
            for &from in rule.from {
                let earlier = TABLE[..index]
                    .iter()
                    .position(|other| other.on == rule.on && other.from.contains(&from));
                assert!(
                    earlier.is_none(),
                    "row {index} ({:?} from {from:?}) can never win: row {} comes first",
                    rule.on,
                    earlier.unwrap_or_default()
                );
            }
        }
    }

    #[test]
    fn the_order_of_the_rows_decides_which_signal_told_together_wins() {
        const ENDS: Rule = Rule {
            from: &[Working],
            on: TurnEnded,
            to: Stopped,
        };
        const GOES_ON: Rule = Rule {
            from: &[Working],
            on: Backgrounded,
            to: Background,
        };
        let both = told(&[TurnEnded, Backgrounded]);
        let to = |table: &[Rule]| first(table, Working, &both).map(|rule| rule.to);
        assert_eq!(to(&[ENDS, GOES_ON]), Some(Stopped));
        assert_eq!(to(&[GOES_ON, ENDS]), Some(Background));
    }

    #[test]
    fn the_table_ranks_the_signals_in_the_order_it_states() {
        assert_eq!(
            priority(),
            [
                Ended,
                Interrupted,
                TurnFailed,
                PermissionAsked,
                QuestionAsked,
                Answered,
                TurnEndedAsking,
                Backgrounded,
                BackgroundDone,
                TurnEnded,
                Prompted,
                Started,
                Dismissed,
                Abandoned,
                ReadExpired,
                Looked,
            ]
        );
    }

    #[test]
    fn a_question_or_an_error_told_with_work_going_on_wins() {
        let to = |from, signals: &[Signal]| next(from, &told(signals)).map(|rule| rule.to);
        assert_eq!(to(Working, &[TurnEnded, Backgrounded]), Some(Background));
        assert_eq!(
            to(Working, &[TurnEnded, TurnEndedAsking, Backgrounded]),
            Some(Asked)
        );
        assert_eq!(to(Working, &[TurnEnded, TurnFailed]), Some(Failed));
        assert_eq!(to(Working, &[TurnEnded, Interrupted]), Some(Idle));
        // A stop told as an error too is a stop.
        assert_eq!(
            to(Working, &[TurnEnded, TurnFailed, Interrupted]),
            Some(Idle)
        );
        // A wait already shown holds against a turn end told with work.
        assert_eq!(to(Blocked, &[TurnEnded, Backgrounded]), Some(Blocked));
        assert_eq!(to(Asked, &[TurnEnded, Backgrounded]), Some(Asked));
        // A turn that starts already held up is held up.
        assert_eq!(to(Idle, &[Prompted, PermissionAsked]), Some(Blocked));
        assert_eq!(to(Blocked, &[Prompted, QuestionAsked]), Some(Blocked));
    }

    #[test]
    fn a_second_failure_told_with_a_turn_end_is_not_news() {
        let rule = next(Failed, &told(&[TurnEnded, TurnFailed])).unwrap();
        assert_eq!((rule.on, rule.to), (TurnFailed, Failed));
        assert!(!announces(Failed, rule.on, rule.to));
    }

    #[test]
    fn signals_of_one_family_told_together_do_not_overlap() {
        assert!(!overlaps(Working, &told(&[TurnEnded, TurnEndedAsking])));
        assert!(!overlaps(
            Working,
            &told(&[TurnEnded, Backgrounded, TurnFailed])
        ));
        assert!(!overlaps(Idle, &told(&[Prompted, PermissionAsked])));
        assert!(!overlaps(Working, &told(&[TurnEnded])));
    }

    #[test]
    fn signals_of_different_families_told_together_overlap() {
        assert!(overlaps(Working, &told(&[TurnEnded, PermissionAsked])));
        assert!(overlaps(Blocked, &told(&[Answered, PermissionAsked])));
        assert!(overlaps(Working, &told(&[Ended, Prompted, TurnEnded])));
        // A signal that matches no row from the phase loses nothing.
        assert!(!overlaps(Working, &told(&[Prompted, Answered])));
    }

    #[test]
    fn the_variant_lists_are_complete() {
        for (index, phase) in Phase::ALL.into_iter().enumerate() {
            assert_eq!(phase.index(), index, "{phase:?}");
        }
        for (index, signal) in Signal::ALL.into_iter().enumerate() {
            assert_eq!(signal.index(), index, "{signal:?}");
        }
    }

    #[test]
    fn gone_is_left_only_by_started_and_reached_only_by_ended() {
        for on in Signal::ALL {
            let expected = (on == Started).then_some(Idle);
            assert_eq!(step(Gone, on), expected, "{on:?} from Gone");
        }
        for from in Phase::ALL {
            for on in Signal::ALL {
                if step(from, on) == Some(Gone) {
                    assert_eq!(on, Ended, "{on:?} from {from:?} reaches Gone");
                }
            }
        }
        for &from in TRACKED {
            assert_eq!(step(from, Ended), Some(Gone), "Ended from {from:?}");
        }
    }

    #[test]
    fn every_signal_but_a_turn_end_is_idempotent() {
        for on in Signal::ALL {
            let turn_end = matches!(on, TurnEnded | TurnEndedAsking);
            assert_eq!(idempotent(on), !turn_end, "{on:?}");
        }
    }

    #[test]
    fn the_daemon_only_quiets_a_light() {
        for rule in TABLE.iter().filter(|rule| rule.on.daemon()) {
            assert!(
                !matches!(rule.to, Working | Blocked | Failed),
                "{:?} moves to {:?}",
                rule.on,
                rule.to
            );
            for &from in rule.from {
                assert!(!announces(from, rule.on, rule.to), "{:?}", rule.on);
            }
        }
    }

    #[test]
    fn the_phases_with_a_light_are_the_shown_ones() {
        for phase in Phase::ALL {
            assert_eq!(phase.state().is_some(), SHOWN.contains(&phase), "{phase:?}");
            if phase.unread() {
                assert!(phase.wants_user(), "{phase:?}");
            }
        }
    }

    #[test]
    fn a_turn_that_ends_again_is_announced_again() {
        assert!(announces(Stopped, TurnEnded, Stopped));
        assert!(announces(StoppedRead, TurnEnded, Stopped));
        assert!(announces(Asked, TurnEndedAsking, Asked));
        assert!(announces(AskedRead, TurnEndedAsking, Asked));
    }

    #[test]
    fn entering_a_phase_that_wants_the_user_is_announced() {
        assert!(announces(Working, PermissionAsked, Blocked));
        assert!(announces(Working, QuestionAsked, Blocked));
        assert!(announces(Working, TurnFailed, Failed));
        assert!(announces(Background, BackgroundDone, Stopped));
        assert!(announces(Stopped, TurnEndedAsking, Asked));
    }

    #[test]
    fn work_and_quiet_phases_are_never_announced() {
        assert!(!announces(Idle, Prompted, Working));
        assert!(!announces(Blocked, Answered, Working));
        assert!(!announces(Stopped, Backgrounded, Background));
        assert!(!announces(Working, Backgrounded, Background));
        assert!(!announces(Stopped, Looked, StoppedRead));
        assert!(!announces(Working, Interrupted, Idle));
        assert!(!announces(Gone, Started, Idle));
    }

    #[test]
    fn a_wait_that_is_told_again_is_not_announced_again() {
        assert_eq!(step(Blocked, PermissionAsked), Some(Blocked));
        assert!(!announces(Blocked, PermissionAsked, Blocked));
        assert_eq!(step(Failed, TurnFailed), Some(Failed));
        assert!(!announces(Failed, TurnFailed, Failed));
    }

    #[test]
    fn the_whole_table_reaches_every_phase_from_gone() {
        assert_eq!(reachable(Gone, &Signal::ALL), BTreeSet::from(Phase::ALL));
    }

    #[test]
    fn the_daemon_alone_never_starts_or_wakes_a_session() {
        let daemon = daemon_signals();
        assert_eq!(reachable(Gone, &daemon), BTreeSet::from([Gone]));
        assert_eq!(reachable(Working, &daemon), BTreeSet::from([Working, Idle]));
        assert_eq!(
            reachable(Stopped, &daemon),
            BTreeSet::from([Stopped, StoppedRead, Idle])
        );
        assert_eq!(reachable(Blocked, &daemon), BTreeSet::from([Blocked, Idle]));
    }

    #[test]
    fn a_harness_that_never_asks_never_blocks() {
        let signals = [Started, Prompted, TurnEnded, Ended];
        assert_eq!(
            reachable(Gone, &signals),
            BTreeSet::from([Gone, Idle, Working, Stopped])
        );
    }

    #[test]
    fn every_signal_says_what_its_absence_costs() {
        for signal in Signal::ALL {
            let missing = signal.when_missing();
            assert!(!missing.is_empty(), "{signal:?}");
            assert!(!missing.ends_with('.'), "{signal:?} ends its own sentence");
        }
    }
}
