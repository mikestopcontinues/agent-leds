//! Turns the queue into LED programs for the device.
//!
//! A program is text in the device's LEDS.LED language. The firmware runs it
//! by itself, so a steady display costs nothing on the Mac.

use serde::Serialize;

use crate::config::{Color, Config};
use crate::state::State;

pub const LEDS: usize = 8;

const ANNOUNCE_PULSE_MS: u64 = 700;
const ANNOUNCE_FADE_MS: u64 = 500;
/// How long an announcement runs before the lights are back at rest.
pub const ANNOUNCE_MS: u64 = ANNOUNCE_PULSE_MS + ANNOUNCE_FADE_MS;
const STACK_PULSE_MS: u64 = 1600;

/// One lit light. A stacked light stands for every remaining item of its
/// state when the queue is longer than the strip.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Slot {
    pub state: State,
    pub stacked: bool,
}

/// Assigns lights to queue items given their states in any order.
///
/// Every state present keeps at least one light. The remaining lights go to
/// states in queue order, and a state with more items than lights ends in a
/// stacked light.
pub fn slots(states: &[State]) -> Vec<Slot> {
    let mut shares = State::ALL.map(|state| {
        let total = states.iter().filter(|&&other| other == state).count();
        (state, total, total.min(1))
    });
    let mut spare = LEDS - shares.iter().map(|share| share.2).sum::<usize>();
    for (_, total, lights) in &mut shares {
        let extra = spare.min(*total - *lights);
        *lights += extra;
        spare -= extra;
    }
    shares
        .iter()
        .flat_map(|&(state, total, lights)| {
            (0..lights).map(move |light| Slot {
                state,
                stacked: light + 1 == lights && lights < total,
            })
        })
        .collect()
}

/// The program that holds the queue on the lights. Stacked lights breathe.
pub fn steady(slots: &[Slot], config: &Config) -> String {
    let lights = place(slots, config);
    let mut program = format!("{}{} ease", settings(config), row(&lights, config));
    let dips: Vec<String> = lights
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            let slot = slot.filter(|slot| slot.stacked)?;
            Some(format!("{index}:{}", config.colors.of(slot.state).dimmed()))
        })
        .collect();
    if !dips.is_empty() {
        program += &format!("\n{} {STACK_PULSE_MS}ms pulse\nrepeat", dips.join(" "));
    }
    program
}

/// The program for a session entering `state`: the whole strip pulses that
/// state's color once, then the queue fades in.
pub fn announce(state: State, slots: &[Slot], config: &Config) -> String {
    format!(
        "{}{} {ANNOUNCE_PULSE_MS}ms pulse\n{} {ANNOUNCE_FADE_MS}ms ease",
        settings(config),
        config.colors.of(state),
        row(&place(slots, config), config),
    )
}

/// Slots in physical order, with the front of the queue on `front_led`.
fn place(slots: &[Slot], config: &Config) -> [Option<Slot>; LEDS] {
    let mut lights = [None; LEDS];
    for (position, slot) in slots.iter().enumerate() {
        lights[if config.front_led == 0 {
            position
        } else {
            LEDS - 1 - position
        }] = Some(*slot);
    }
    lights
}

fn row(lights: &[Option<Slot>; LEDS], config: &Config) -> String {
    lights
        .map(|slot| {
            slot.map_or(Color::OFF, |slot| config.colors.of(slot.state))
                .to_string()
        })
        .join(" ")
}

fn settings(config: &Config) -> String {
    if config.brightness == 255 {
        String::new()
    } else {
        format!("brightness {}\n", config.brightness)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device;
    use State::*;

    fn shape(states: &[State]) -> String {
        slots(states)
            .iter()
            .map(|slot| {
                let letter = format!("{:?}", slot.state).chars().next().unwrap();
                if slot.stacked {
                    letter.to_ascii_lowercase()
                } else {
                    letter
                }
            })
            .collect()
    }

    #[test]
    fn short_queues_get_one_light_each() {
        assert_eq!(shape(&[]), "");
        assert_eq!(shape(&[Working, Error, Stopped, Working]), "ESWW");
        assert_eq!(shape(&[Working; 8]), "WWWWWWWW");
    }

    #[test]
    fn long_queues_stack_the_lowest_priority_states_first() {
        assert_eq!(shape(&[Working; 9]), "WWWWWWWw");
        assert_eq!(
            shape(&[[Error; 2].as_slice(), &[Working; 10]].concat()),
            "EEWWWWWw"
        );
        // Every state keeps a light even when errors alone would fill the strip.
        assert_eq!(
            shape(&[[Error; 9].as_slice(), &[Question, Stopped, Working]].concat()),
            "EEEEeQSW"
        );
        assert_eq!(
            shape(&[[Stopped; 5].as_slice(), &[Working; 5]].concat()),
            "SSSSSWWw"
        );
    }

    #[test]
    fn steady_places_the_front_of_the_queue_on_the_front_led() {
        let slots = slots(&[Error, Working]);
        let far = Config::default();
        assert_eq!(
            steady(&slots, &far),
            "#000000 #000000 #000000 #000000 #000000 #000000 #0000FF #FF0000 ease"
        );
        let near = Config {
            front_led: 0,
            brightness: 64,
            ..Config::default()
        };
        assert_eq!(
            steady(&slots, &near),
            "brightness 64\n#FF0000 #0000FF #000000 #000000 #000000 #000000 #000000 #000000 ease"
        );
    }

    #[test]
    fn stacked_lights_breathe() {
        let config = Config {
            front_led: 0,
            ..Config::default()
        };
        let program = steady(&slots(&[Working; 9]), &config);
        assert_eq!(program.lines().nth(1), Some("7:#00001F 1600ms pulse"));
        assert_eq!(program.lines().last(), Some("repeat"));
    }

    #[test]
    fn announce_pulses_the_new_state_then_fades_in_the_queue() {
        let config = Config {
            front_led: 0,
            ..Config::default()
        };
        assert_eq!(
            announce(Question, &slots(&[Question]), &config),
            "#FF5000 700ms pulse\n#FF5000 #000000 #000000 #000000 #000000 #000000 #000000 #000000 500ms ease"
        );
    }

    #[test]
    fn the_largest_programs_fit_the_device() {
        let config = Config {
            brightness: 128,
            ..Config::default()
        };
        let crowded = slots(
            &[
                [Error; 9].as_slice(),
                &[Question; 9],
                &[Stopped; 9],
                &[Working; 9],
            ]
            .concat(),
        );
        assert_eq!(crowded.iter().filter(|slot| slot.stacked).count(), 4);
        device::check(&steady(&crowded, &config)).unwrap();
        device::check(&announce(Error, &crowded, &config)).unwrap();
    }
}
