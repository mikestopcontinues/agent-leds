//! Turns the queue into LED programs for the device.
//!
//! A program is text in the device's LEDS.LED language. The firmware runs it
//! by itself, so a steady display costs nothing on the Mac.

use serde::Serialize;

use crate::config::{Color, Config, Palette};
use crate::device;
use crate::state::State;

pub const LEDS: usize = 8;

/// One program of an animation and how long it runs before the next is written.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    pub program: String,
    pub ms: u64,
}

/// Chooses the lights for the queue, front first, given its items' states in any order.
///
/// Every state present keeps at least one light and the remaining lights go
/// to states in queue order. A queue longer than the strip is cut short;
/// nothing shows that items are hidden.
pub fn slots(states: &[State]) -> Vec<State> {
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
        .flat_map(|&(state, _, lights)| std::iter::repeat_n(state, lights))
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

/// The program that holds the queue on the lights.
pub fn steady(slots: &[State], config: &Config) -> String {
    let colors = balanced(config);
    let queue = row(config, |position| {
        queue_color(slots, &colors, position, 1.0)
    });
    format!("{}{queue} ease", settings(config))
}

/// The programs for a session entering `state`, to be written one after
/// another: the snake.
///
/// The lights fade out. A head in the state's color enters at the end
/// furthest from the front of the queue: a dimmer nose, a bright light, then
/// a dark one. It pulls the new queue in behind it as a dimmed tail and runs
/// off the front end, which leaves the queue in place, and the queue then
/// rises to full brightness.
///
/// Every step is a full row of colors, so the whole animation does not fit
/// in one program.
pub fn announce(state: State, slots: &[State], config: &Config) -> Vec<Step> {
    let colors = balanced(config);
    let snake = &config.snake;
    let color = colors.of(state);
    let head = [color.scaled(snake.nose), color, Color::OFF];
    // With the nose `nose` lights in, queue item k trails the head at nose - head.len() - k.
    let frame = |nose: usize| {
        row(config, |position| {
            match nose.checked_sub(LEDS - 1 - position) {
                None => Color::OFF,
                Some(behind) if behind < head.len() => head[behind],
                Some(behind) => queue_color(slots, &colors, behind - head.len(), snake.tail),
            }
        })
    };
    // Each step holds still, then moves. Without the hold a light is at its
    // level only for an instant, so the bright light never looks bright.
    let hold = match snake.hold_ms {
        0 => String::new(),
        ms => format!(" {ms}ms"),
    };

    let mut lines = vec![(
        format!("off {}ms ease", snake.fade_out_ms),
        snake.fade_out_ms,
    )];
    // The last step has the whole head past the front end and the queue in place.
    lines.extend((0..LEDS + head.len()).map(|nose| {
        (
            format!("{} {}ms linear{hold}", frame(nose), snake.move_ms),
            snake.hold_ms + snake.move_ms,
        )
    }));
    let queue = row(config, |position| {
        queue_color(slots, &colors, position, 1.0)
    });
    lines.push((
        format!("{queue} {}ms ease", snake.fade_in_ms),
        snake.fade_in_ms,
    ));
    packed(&lines, config)
}

/// Packs an animation's lines into as few programs as the device accepts.
fn packed(lines: &[(String, u32)], config: &Config) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for (line, ms) in lines {
        if let Some(last) = steps.last_mut() {
            let longer = format!("{}\n{line}", last.program);
            if device::check(&longer).is_ok() {
                last.program = longer;
                last.ms += u64::from(*ms);
                continue;
            }
        }
        steps.push(Step {
            program: format!("{}{line}", settings(config)),
            ms: u64::from(*ms),
        });
    }
    steps
}

/// The color of the queue's item at `position`, position 0 being the front, at a share of its brightness.
fn queue_color(slots: &[State], colors: &Palette, position: usize, level: f64) -> Color {
    slots
        .get(position)
        .map_or(Color::OFF, |&state| colors.of(state).scaled(level))
}

/// The colors of the eight lights in physical order, given the color for each
/// position along the strip, position 0 being the front of the queue.
fn row(config: &Config, color_at: impl Fn(usize) -> Color) -> String {
    let mut lights = [Color::OFF; LEDS];
    for position in 0..LEDS {
        let led = if config.front_led == 0 {
            position
        } else {
            LEDS - 1 - position
        };
        lights[led] = color_at(position);
    }
    lights.map(|color| color.to_string()).join(" ")
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
    use State::*;

    /// One letter per light.
    fn shape(states: &[State]) -> String {
        slots(states)
            .iter()
            .map(|state| format!("{state:?}").chars().next().unwrap())
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

    /// Every line of an animation, across its programs.
    fn lines(steps: &[Step]) -> Vec<&str> {
        steps.iter().flat_map(|step| step.program.lines()).collect()
    }

    #[test]
    fn short_queues_get_one_light_each() {
        assert_eq!(shape(&[]), "");
        assert_eq!(shape(&[Working, Error, Stopped, Working]), "ESWW");
        assert_eq!(shape(&[Working; 8]), "WWWWWWWW");
    }

    #[test]
    fn long_queues_keep_a_light_for_every_state() {
        assert_eq!(shape(&[Working; 9]), "WWWWWWWW");
        assert_eq!(
            shape(&[[Error; 2].as_slice(), &[Working; 10]].concat()),
            "EEWWWWWW"
        );
        // Errors alone would fill the strip, but each other state keeps one light.
        assert_eq!(
            shape(&[[Error; 9].as_slice(), &[Question, Stopped, Working]].concat()),
            "EEEEEQSW"
        );
        assert_eq!(
            shape(&[[Stopped; 5].as_slice(), &[Working; 5]].concat()),
            "SSSSSWWW"
        );
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
    fn a_full_strip_holds_still() {
        assert_eq!(steady(&slots(&[Working; 12]), &plain(0)).lines().count(), 1);
    }

    #[test]
    fn the_snake_leads_the_dimmed_queue_in_then_the_queue_rises() {
        let steps = announce(Stopped, &slots(&[Error, Stopped]), &plain(7));
        let lines = lines(&steps);
        assert_eq!(lines.len(), 13);
        assert_eq!(lines[0], "off 300ms ease");
        // The nose enters at light 0, the end furthest from the front, at 60%.
        assert_eq!(
            lines[1],
            "#009900 #000000 #000000 #000000 #000000 #000000 #000000 #000000 90ms linear 60ms"
        );
        // Nose, bright light, dark light, then the queue at 30% with its front first.
        assert_eq!(
            lines[6],
            "#000000 #004D00 #4D0000 #000000 #00FF00 #009900 #000000 #000000 90ms linear 60ms"
        );
        // The head has left the front end and the dimmed queue is in place.
        assert_eq!(
            lines[11],
            "#000000 #000000 #000000 #000000 #000000 #000000 #004D00 #4D0000 90ms linear 60ms"
        );
        assert_eq!(
            lines[12],
            "#000000 #000000 #000000 #000000 #000000 #000000 #00FF00 #FF0000 500ms ease"
        );
        // Its running time is the fades plus eleven steps.
        let total: u64 = steps.iter().map(|step| step.ms).sum();
        assert_eq!(total, 300 + 11 * 150 + 500);
    }

    #[test]
    fn the_snake_runs_toward_whichever_end_is_the_front() {
        let steps = announce(Stopped, &[], &plain(0));
        assert!(lines(&steps)[1].ends_with("#009900 90ms linear 60ms"));
    }

    #[test]
    fn the_snake_follows_its_settings() {
        let mut config = plain(7);
        config.snake = crate::config::Snake {
            nose: 1.0,
            tail: 0.5,
            hold_ms: 0,
            move_ms: 40,
            fade_out_ms: 100,
            fade_in_ms: 200,
        };
        let steps = announce(Error, &slots(&[Error]), &config);
        let lines = lines(&steps);
        assert_eq!(lines[0], "off 100ms ease");
        // No hold, so no delay is written.
        assert_eq!(
            lines[1],
            "#FF0000 #000000 #000000 #000000 #000000 #000000 #000000 #000000 40ms linear"
        );
        assert!(lines[4].starts_with("#800000 #000000 #FF0000 #FF0000 "));
        assert!(lines[12].ends_with("#FF0000 200ms ease"));
    }

    #[test]
    fn every_program_fits_the_device() {
        let crowded = slots(
            &[
                [Error; 9].as_slice(),
                &[Question; 9],
                &[Stopped; 9],
                &[Working; 9],
            ]
            .concat(),
        );
        for brightness in [255, 128] {
            let mut config = Config {
                brightness,
                ..Config::default()
            };
            config.snake.hold_ms = 5000;
            config.snake.move_ms = 5000;
            device::check(&steady(&crowded, &config)).unwrap();
            let steps = announce(Error, &crowded, &config);
            // Thirteen lines of full rows cannot fit in fewer than three programs.
            assert!(steps.len() >= 3);
            for step in &steps {
                device::check(&step.program).unwrap();
                if brightness != 255 {
                    assert!(step.program.starts_with("brightness 128\n"));
                }
            }
        }
    }
}
