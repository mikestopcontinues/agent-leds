//! Turns the queue into LED programs for the device.
//!
//! A program is text in the device's LEDS.LED language. The firmware runs it
//! by itself, so a steady display costs nothing on the Mac.

use serde::Serialize;

use crate::config::{Color, Config, Palette};
use crate::state::State;

pub const LEDS: usize = 8;

const FADE_OUT_MS: u64 = 300;
/// Each light pulses for this long as the slide passes it.
const SLIDE_PULSE_MS: u64 = 560;
/// The slide reaches the next light after this long.
const SLIDE_STEP_MS: u64 = 160;
const FADE_IN_MS: u64 = 500;
/// How long an announcement runs before the lights are back at rest.
pub const ANNOUNCE_MS: u64 =
    FADE_OUT_MS + SLIDE_PULSE_MS + (LEDS as u64 - 1) * SLIDE_STEP_MS + FADE_IN_MS;

const STACK_PULSE_MS: u64 = 1600;
/// How far a stacked light dips when it breathes.
const STACK_DIP: f64 = 0.125;

/// One lit light and the number of queue items it stands for. A light with a
/// count above one is "stacked": it holds every remaining item of its state
/// when the queue is longer than the strip.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Slot {
    pub state: State,
    pub count: usize,
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
                count: if light + 1 == lights {
                    total + 1 - lights
                } else {
                    1
                },
            })
        })
        .collect()
}

/// The state colors scaled to look equally bright.
///
/// A color's brightness is the light of its three dies weighted by
/// `config.balance`. Each color keeps its hue and is scaled to the brightness
/// of the dimmest state color at full strength, which is the highest level
/// all four can reach.
pub fn balanced(config: &Config) -> Palette {
    let brightness = |color: Color| -> f64 {
        let light = [color.0, color.1, color.2].map(|value| f64::from(value) / 255.0);
        light
            .iter()
            .zip(config.balance)
            .map(|(light, weight)| light * weight)
            .sum()
    };
    let at_full_strength =
        |color: Color| brightness(color) * 255.0 / f64::from(color.0.max(color.1).max(color.2));
    let lit = State::ALL
        .map(|state| config.colors.of(state))
        .into_iter()
        .filter(|&color| color != Color::OFF);
    let target = lit.map(at_full_strength).fold(f64::INFINITY, f64::min);

    let mut palette = config.colors;
    for state in State::ALL {
        let color = config.colors.of(state);
        if color != Color::OFF {
            palette.set(state, color.scaled(target / brightness(color)));
        }
    }
    palette
}

/// The program that holds the queue on the lights. Stacked lights breathe.
pub fn steady(slots: &[Slot], config: &Config) -> String {
    let colors = balanced(config);
    let lights = place(slots, config);
    let mut program = format!("{}{} ease", settings(config), row(&lights, &colors));
    let dips: Vec<String> = lights
        .iter()
        .enumerate()
        .filter_map(|(index, slot)| {
            let slot = slot.filter(|slot| slot.count > 1)?;
            Some(format!(
                "{index}:{}",
                colors.of(slot.state).scaled(STACK_DIP)
            ))
        })
        .collect();
    if !dips.is_empty() {
        program += &format!("\n{} {STACK_PULSE_MS}ms pulse\nrepeat", dips.join(" "));
    }
    program
}

/// The program for a session entering `state`: the lights fade out, the
/// state's color slides along the dark strip toward the front of the queue,
/// then the queue fades in.
pub fn announce(state: State, slots: &[Slot], config: &Config) -> String {
    let colors = balanced(config);
    let color = colors.of(state);
    let slide: Vec<String> = (0..LEDS)
        .map(|step| {
            let delay = if step == 0 {
                String::new()
            } else {
                format!(" {}ms", step as u64 * SLIDE_STEP_MS)
            };
            format!(
                "{}:{color} {SLIDE_PULSE_MS}ms pulse{delay}",
                led_at(LEDS - 1 - step, config)
            )
        })
        .collect();
    format!(
        "{}off {FADE_OUT_MS}ms ease\n{}\n{} {FADE_IN_MS}ms ease",
        settings(config),
        slide.join(";"),
        row(&place(slots, config), &colors),
    )
}

/// The light for a position in the queue, position 0 being the front.
fn led_at(position: usize, config: &Config) -> usize {
    if config.front_led == 0 {
        position
    } else {
        LEDS - 1 - position
    }
}

/// Slots in physical order.
fn place(slots: &[Slot], config: &Config) -> [Option<Slot>; LEDS] {
    let mut lights = [None; LEDS];
    for (position, slot) in slots.iter().enumerate() {
        lights[led_at(position, config)] = Some(*slot);
    }
    lights
}

fn row(lights: &[Option<Slot>; LEDS], colors: &Palette) -> String {
    lights
        .map(|slot| {
            slot.map_or(Color::OFF, |slot| colors.of(slot.state))
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

    /// One letter per light, lower case when it is stacked.
    fn shape(states: &[State]) -> String {
        slots(states)
            .iter()
            .map(|slot| {
                let letter = format!("{:?}", slot.state).chars().next().unwrap();
                if slot.count > 1 {
                    letter.to_ascii_lowercase()
                } else {
                    letter
                }
            })
            .collect()
    }

    /// The signal palette with dies that look equally bright, so its single-die colors are shown unchanged.
    fn plain(front_led: u8) -> Config {
        Config {
            front_led,
            balance: [1.0, 1.0, 1.0],
            colors: crate::config::PALETTES[0].colors,
            ..Config::default()
        }
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
    fn a_stacked_light_counts_what_it_holds() {
        let counts: Vec<usize> = slots(&[[Error; 2].as_slice(), &[Working; 10]].concat())
            .iter()
            .map(|slot| slot.count)
            .collect();
        assert_eq!(counts, [1, 1, 1, 1, 1, 1, 1, 5]);
    }

    #[test]
    fn states_are_scaled_to_the_dimmest_one() {
        // Green looks two and a half times as bright as red here, and blue a fifth as bright.
        let config = Config {
            balance: [1.0, 2.5, 0.2],
            ..plain(7)
        };
        let colors = balanced(&config);
        assert_eq!(colors.error.to_string(), "#330000");
        assert_eq!(colors.stopped.to_string(), "#001400");
        assert_eq!(colors.working.to_string(), "#0000FF");
        // A dim setting is a hue, not a level: it comes out as bright as the rest.
        let mut dim = config.clone();
        dim.colors.error = Color(0x20, 0, 0);
        assert_eq!(balanced(&dim).error.to_string(), "#330000");
    }

    #[test]
    fn steady_places_the_front_of_the_queue_on_the_front_led() {
        let slots = slots(&[Error, Working]);
        assert_eq!(
            steady(&slots, &plain(7)),
            "#000000 #000000 #000000 #000000 #000000 #000000 #0000FF #FF0000 ease"
        );
        assert_eq!(
            steady(
                &slots,
                &Config {
                    brightness: 64,
                    ..plain(0)
                }
            ),
            "brightness 64\n#FF0000 #0000FF #000000 #000000 #000000 #000000 #000000 #000000 ease"
        );
    }

    #[test]
    fn stacked_lights_breathe() {
        let program = steady(&slots(&[Working; 9]), &plain(0));
        assert_eq!(program.lines().nth(1), Some("7:#000020 1600ms pulse"));
        assert_eq!(program.lines().last(), Some("repeat"));
    }

    #[test]
    fn announce_fades_out_slides_toward_the_front_then_fades_the_queue_in() {
        let program = announce(Stopped, &slots(&[Stopped]), &plain(7));
        let lines: Vec<&str> = program.lines().collect();
        assert_eq!(lines[0], "off 300ms ease");
        assert!(lines[1].starts_with("0:#00FF00 560ms pulse;1:#00FF00 560ms pulse 160ms;"));
        assert!(lines[1].ends_with(";7:#00FF00 560ms pulse 1120ms"));
        assert_eq!(
            lines[2],
            "#000000 #000000 #000000 #000000 #000000 #000000 #000000 #00FF00 500ms ease"
        );
        // With the front on light 0 the slide runs the other way.
        assert!(
            announce(Stopped, &[], &plain(0))
                .lines()
                .nth(1)
                .unwrap()
                .starts_with("7:")
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
        assert_eq!(crowded.iter().filter(|slot| slot.count > 1).count(), 4);
        device::check(&steady(&crowded, &config)).unwrap();
        device::check(&announce(Error, &crowded, &config)).unwrap();
    }
}
