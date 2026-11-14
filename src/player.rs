//! What the lights play: the programs of a change written one after
//! another, each when the one before it ends, then the queue held steady.
//!
//! The player decides what to write and when; it never touches the card or
//! the clock. Each call is given the time and returns the program to write
//! now, if there is one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::render::{self, Step};
use crate::state::State;

#[derive(Default)]
pub struct Player {
    /// Programs waiting to be written, each when the one before it ends.
    pending: VecDeque<Step>,
    /// When the program on the card ends, while one of a change is playing.
    ends: Option<Instant>,
    /// Something other than the queue is showing; the queue waits until it ends.
    holding: bool,
    /// The queue the lights hold, or are on their way to.
    shown: Vec<State>,
}

impl Player {
    /// When the program on the card ends and the next one is due.
    pub fn deadline(&self) -> Option<Instant> {
        self.ends
    }

    /// Whether a change is in the middle of playing, so that anything slow
    /// done now could delay its next program.
    pub fn animating(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Announces that a session entered `state`: the snake, then the queue
    /// `slots`. Something held stays; the queue shows once it ends.
    pub fn announce(
        &mut self,
        state: State,
        slots: &[State],
        config: &Config,
        now: Instant,
    ) -> Option<String> {
        if self.holding {
            return None;
        }
        let steps = render::change(Some(state), &self.shown, slots, config);
        self.shown = slots.to_vec();
        self.play(steps, slots, config, now)
    }

    /// Shows a change that asks nothing of the user: the wave alone, from
    /// the queue the lights hold to `slots`. A change already playing is not
    /// interrupted; it ends on the queue as it is by then.
    pub fn settle(&mut self, slots: &[State], config: &Config, now: Instant) -> Option<String> {
        if self.holding || self.ends.is_some() {
            return None;
        }
        let steps = render::change(None, &self.shown, slots, config);
        self.shown = slots.to_vec();
        if steps.is_empty() {
            return None;
        }
        self.play(steps, slots, config, now)
    }

    /// Shows programs that are not the queue; the queue returns when the last one ends.
    pub fn hold(
        &mut self,
        steps: Vec<Step>,
        slots: &[State],
        config: &Config,
        now: Instant,
    ) -> Option<String> {
        self.holding = true;
        self.play(steps, slots, config, now)
    }

    /// The next program once the one on the card has ended: the next of the
    /// change, or the queue `slots` once none is left.
    pub fn advance(&mut self, slots: &[State], config: &Config, now: Instant) -> Option<String> {
        if self.ends.is_none_or(|ends| ends > now) {
            return None;
        }
        Some(self.next(slots, config, now))
    }

    /// Shows the queue `slots` now, unless something is held.
    pub fn refresh(&mut self, slots: &[State], config: &Config) -> Option<String> {
        (!self.holding).then(|| self.steady(slots, config))
    }

    /// Shows the queue `slots` now, ending whatever was playing.
    pub fn steady(&mut self, slots: &[State], config: &Config) -> String {
        self.holding = false;
        self.ends = None;
        self.pending.clear();
        self.shown = slots.to_vec();
        render::steady(slots, config)
    }

    fn play(
        &mut self,
        steps: Vec<Step>,
        slots: &[State],
        config: &Config,
        now: Instant,
    ) -> Option<String> {
        self.pending = steps.into();
        Some(self.next(slots, config, now))
    }

    fn next(&mut self, slots: &[State], config: &Config, now: Instant) -> String {
        match self.pending.pop_front() {
            Some(step) => {
                self.ends = Some(now + Duration::from_millis(step.ms));
                step.program
            }
            None => self.steady(slots, config),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::State::{Error, Stopped, Working};

    fn ms(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    /// Plays a change to the end, collecting each program and when it was written.
    fn run_out(player: &mut Player, slots: &[State], config: &Config) -> Vec<(Instant, String)> {
        let mut written = Vec::new();
        while let Some(at) = player.deadline() {
            if let Some(program) = player.advance(slots, config, at) {
                written.push((at, program));
            }
        }
        written
    }

    #[test]
    fn an_announcement_plays_its_programs_in_turn_then_holds_the_queue() {
        let config = Config::default();
        let mut player = Player::default();
        let start = Instant::now();
        player.steady(&[Working], &config);
        let slots = [Stopped, Working];
        let steps = render::change(Some(Stopped), &[Working], &slots, &config);
        let first = player.announce(Stopped, &slots, &config, start);
        assert_eq!(first.as_deref(), Some(steps[0].program.as_str()));
        assert!(player.animating() || steps.len() == 1);
        // Nothing is written before the program on the card ends.
        assert_eq!(player.advance(&slots, &config, start), None);
        // Each later program is written as the one before it ends, and the
        // queue once the last has.
        let mut expected = Vec::new();
        let mut at = start;
        for (index, step) in steps.iter().enumerate() {
            at = ms(at, step.ms);
            let next = steps.get(index + 1).map(|next| next.program.clone());
            expected.push((at, next.unwrap_or_else(|| render::steady(&slots, &config))));
        }
        assert_eq!(run_out(&mut player, &slots, &config), expected);
        assert_eq!(player.deadline(), None);
    }

    #[test]
    fn a_quiet_change_waits_for_a_snake_and_the_snake_ends_on_the_queue_as_it_is_then() {
        let config = Config::default();
        let mut player = Player::default();
        let start = Instant::now();
        player.announce(Stopped, &[Stopped, Working], &config, start);
        // The working session leaves while the snake runs.
        assert_eq!(player.settle(&[Stopped], &config, start), None);
        let written = run_out(&mut player, &[Stopped], &config);
        assert_eq!(
            written.last().unwrap().1,
            render::steady(&[Stopped], &config)
        );
    }

    #[test]
    fn a_change_the_lights_would_not_show_writes_nothing() {
        let config = Config::default();
        let mut player = Player::default();
        player.steady(&[Working], &config);
        assert_eq!(player.settle(&[Working], &config, Instant::now()), None);
        assert_eq!(player.deadline(), None);
    }

    #[test]
    fn something_held_keeps_the_lights_until_it_ends() {
        let config = Config::default();
        let mut player = Player::default();
        let start = Instant::now();
        let held = Step {
            program: "#FFFFFF".into(),
            ms: 5000,
        };
        let slots = [Error];
        assert_eq!(
            player.hold(vec![held], &slots, &config, start).as_deref(),
            Some("#FFFFFF")
        );
        // Neither a new error nor new settings take the lights from it.
        assert_eq!(player.announce(Error, &slots, &config, start), None);
        assert_eq!(player.settle(&slots, &config, start), None);
        assert_eq!(player.refresh(&slots, &config), None);
        assert_eq!(player.advance(&slots, &config, ms(start, 4999)), None);
        // Then the queue returns, as it is by then.
        assert_eq!(
            player.advance(&slots, &config, ms(start, 5000)),
            Some(render::steady(&slots, &config))
        );
        assert_eq!(
            player.refresh(&slots, &config),
            Some(render::steady(&slots, &config))
        );
    }
}
