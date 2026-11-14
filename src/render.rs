//! Turns the queue into LED programs for the device.
//!
//! A program is text in the device's LEDS.LED language. The firmware runs it
//! by itself, so a steady display costs nothing on the Mac.

use serde::Serialize;

use crate::config::{Color, Config, FadeIn, Palette, Snake};
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

/// A line of a program and how long it runs.
type Line = (String, u32);

/// The programs for a session entering `state`, to be written one after
/// another: the snake.
///
/// The lights fade out. A head in the state's color enters at the end
/// furthest from the front of the queue and pulls the new queue in behind it
/// as a dimmed tail. The head runs off the front end, which leaves the queue
/// in place. The queue then rises to full brightness, and can crunch against
/// that end and spring back while it does.
///
/// Every step is a full row of colors, so the whole animation does not fit
/// in one program.
pub fn announce(state: State, slots: &[State], config: &Config) -> Vec<Step> {
    let colors = balanced(config);
    let snake = &config.snake;
    let color = colors.of(state);

    // The head from its front back, then the dark light that sets it apart
    // from the tail. A front or back light with no brightness is left out.
    let [front, middle, back] = snake.head;
    let mut head = vec![color.scaled(middle)];
    if front > 0.0 {
        head.insert(0, color.scaled(front));
    }
    if back > 0.0 {
        head.push(color.scaled(back));
    }
    if snake.gap {
        head.push(Color::OFF);
    }

    // With the head's front `reach` lights in, queue item k trails the head at reach - head.len() - k.
    let frame = |reach: usize| {
        row(config, |position| {
            match reach.checked_sub(LEDS - 1 - position) {
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

    // The fade and the snake's steps can be split between programs anywhere.
    let mut groups: Vec<Vec<Line>> = vec![vec![(
        format!("off {}ms ease", snake.fade_out_ms),
        snake.fade_out_ms,
    )]];
    // The last step has the whole head past the front end and the queue in place.
    groups.extend((0..LEDS + head.len()).map(|reach| {
        vec![(
            format!("{} {}ms linear{hold}", frame(reach), snake.move_ms),
            snake.hold_ms + snake.move_ms,
        )]
    }));
    // The arrival is quick, so it is kept in one program: a write in the
    // middle of the crunch would show as a hitch.
    groups.push(arrival(slots, &colors, config));
    packed(&groups, config)
}

/// When each item of the arriving queue rises from the tail's brightness to full.
struct Rise {
    /// How long one item takes, in milliseconds.
    each_ms: f64,
    /// How long after the item in front of it an item starts.
    stagger_ms: f64,
}

impl Rise {
    fn new(snake: &Snake, items: usize) -> Rise {
        let whole_ms = f64::from(snake.fade_in_ms);
        match snake.fade_in {
            // Each item takes half the rise, and they start evenly spaced so
            // that the last one ends as the rise does.
            FadeIn::FrontToBack if items > 1 => Rise {
                each_ms: whole_ms / 2.0,
                stagger_ms: whole_ms / 2.0 / (items - 1) as f64,
            },
            _ => Rise {
                each_ms: whole_ms,
                stagger_ms: 0.0,
            },
        }
    }

    fn start_ms(&self, item: usize) -> f64 {
        item as f64 * self.stagger_ms
    }

    /// How far an item has risen this long into the rise, from 0 to 1.
    fn risen(&self, item: usize, elapsed_ms: u32) -> f64 {
        ((f64::from(elapsed_ms) - self.start_ms(item)) / self.each_ms).clamp(0.0, 1.0)
    }
}

/// The lines for the dimmed queue arriving at the front end: it rises to
/// full brightness, and if it crunches it meanwhile squeezes toward that end,
/// springs back a little past its resting length if it bounces, and comes to
/// rest.
fn arrival(slots: &[State], colors: &Palette, config: &Config) -> Vec<Line> {
    let snake = &config.snake;
    // Each stage of the crunch: the queue's length, how long it takes to get there, and the easing.
    let mut stages = Vec::new();
    if snake.crunch > 0.0 {
        stages.push((1.0 - snake.crunch, snake.crunch_ms, "ease-out"));
        let mut rest_ms = snake.spring_ms;
        if snake.bounce > 0.0 {
            let out_ms = (snake.spring_ms * 3 / 5).max(1);
            rest_ms = (snake.spring_ms - out_ms).max(1);
            stages.push((1.0 + snake.crunch * snake.bounce, out_ms, "ease-in-out"));
        }
        stages.push((1.0, rest_ms, "ease-in-out"));
    }

    // The rise starts with the crunch, so each stage shows every item as
    // bright as it has got by the time that stage ends.
    let rise = Rise::new(snake, slots.len());
    let mut elapsed_ms = 0;
    let mut lines: Vec<Line> = stages
        .into_iter()
        .map(|(length, ms, easing)| {
            elapsed_ms += ms;
            let lights = stretched(slots, colors, length, |item| {
                snake.tail + (1.0 - snake.tail) * rise.risen(item, elapsed_ms)
            });
            let colors = row(config, |position| lights[position]);
            (format!("{colors} {ms}ms {easing}"), ms)
        })
        .collect();
    lines.extend(rest_of_rise(slots, colors, config, &rise, elapsed_ms));
    lines
}

/// The line that takes the queue, at rest, the rest of the way to full
/// brightness from `elapsed_ms` into the rise, if any of the rise is left.
fn rest_of_rise(
    slots: &[State],
    colors: &Palette,
    config: &Config,
    rise: &Rise,
    elapsed_ms: u32,
) -> Option<Line> {
    let left_ms = config.snake.fade_in_ms.checked_sub(elapsed_ms)?;
    // A light that has not started rising eases in and out; one on its way eases out.
    let easing = |started: bool| if started { "ease-out" } else { "ease" };
    if rise.stagger_ms == 0.0 {
        let queue = row(config, |position| queue_color(slots, colors, position, 1.0));
        let line = format!("{queue} {left_ms}ms {}", easing(elapsed_ms > 0));
        return (left_ms > 0).then_some((line, left_ms));
    }
    // Each light has its own start and duration, so each is its own segment.
    let lights: Vec<String> = slots
        .iter()
        .enumerate()
        .filter_map(|(item, &state)| {
            let from_ms = rise.start_ms(item).max(f64::from(elapsed_ms));
            let ms = (rise.start_ms(item) + rise.each_ms - from_ms).round() as u32;
            let delay = match (from_ms - f64::from(elapsed_ms)).round() as u32 {
                0 => String::new(),
                ms => format!(" {ms}ms"),
            };
            let started = rise.start_ms(item) < f64::from(elapsed_ms);
            (ms > 0).then(|| {
                format!(
                    "{}:{} {ms}ms {}{delay}",
                    led(item, config),
                    colors.of(state),
                    easing(started)
                )
            })
        })
        .collect();
    (!lights.is_empty()).then(|| (lights.join(";"), left_ms))
}

/// The queue with its length scaled from the front end and each item at a
/// share of its brightness, as the color at each position along the strip.
///
/// Each item covers `length` lights, centered `length` apart, and a light
/// shows the items that cover it, in proportion. A stretched queue is as
/// bright as it is at rest and simply reaches further; what reaches past the
/// last light is not shown. A squeezed queue keeps all its light, so its
/// items add up on fewer lights and it is brightest at the end it is pressed
/// against. At a length of 1 this is the queue as it rests.
fn stretched(
    slots: &[State],
    colors: &Palette,
    length: f64,
    level: impl Fn(usize) -> f64,
) -> [Color; LEDS] {
    // A queue squeezed to nothing still sits on the front light.
    let length = length.max(0.01);
    let mut light = [[0.0_f64; 3]; LEDS];
    for (item, &state) in slots.iter().enumerate() {
        let color = colors.of(state);
        let density = level(item) / length.min(1.0);
        let center = item as f64 * length;
        for (position, sums) in light.iter_mut().enumerate() {
            let covered = (center + length / 2.0).min(position as f64 + 0.5)
                - (center - length / 2.0).max(position as f64 - 0.5);
            if covered > 0.0 {
                for (sum, value) in sums.iter_mut().zip([color.0, color.1, color.2]) {
                    *sum += f64::from(value) * covered * density;
                }
            }
        }
    }
    light.map(|dies| {
        let [red, green, blue] = dies.map(|value| value.round().clamp(0.0, 255.0) as u8);
        Color(red, green, blue)
    })
}

/// Packs an animation's lines into as few programs as the device accepts.
///
/// The lines of a group stay in one program when one can hold them: a group
/// that does not fit in what is left of the last program starts a new one.
fn packed(groups: &[Vec<Line>], config: &Config) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for group in groups {
        let together = group
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let fits_last = steps
            .last()
            .is_some_and(|last| device::check(&format!("{}\n{together}", last.program)).is_ok());
        let fits_alone = device::check(&format!("{}{together}", settings(config))).is_ok();
        let mut start_new = !fits_last && fits_alone;
        for (line, ms) in group {
            let longer = match steps.last() {
                Some(last) if !start_new => Some(format!("{}\n{line}", last.program))
                    .filter(|longer| device::check(longer).is_ok()),
                _ => None,
            };
            match (longer, steps.last_mut()) {
                (Some(longer), Some(last)) => {
                    last.program = longer;
                    last.ms += u64::from(*ms);
                }
                _ => steps.push(Step {
                    program: format!("{}{line}", settings(config)),
                    ms: u64::from(*ms),
                }),
            }
            start_new = false;
        }
    }
    steps
}

/// The color of the queue's item at `position`, position 0 being the front, at a share of its brightness.
fn queue_color(slots: &[State], colors: &Palette, position: usize, level: f64) -> Color {
    slots
        .get(position)
        .map_or(Color::OFF, |&state| colors.of(state).scaled(level))
}

/// The light at a position along the strip, position 0 being the front of the queue.
fn led(position: usize, config: &Config) -> usize {
    if config.front_led == 0 {
        position
    } else {
        LEDS - 1 - position
    }
}

/// The colors of the eight lights in physical order, given the color for each
/// position along the strip.
fn row(config: &Config, color_at: impl Fn(usize) -> Color) -> String {
    let mut lights = [Color::OFF; LEDS];
    for position in 0..LEDS {
        lights[led(position, config)] = color_at(position);
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
        // The queue rises front first: each light takes half the rise, the second starting as the first ends.
        assert_eq!(lines[12], "7:#FF0000 250ms ease;6:#00FF00 250ms ease 250ms");
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
        config.snake = Snake {
            head: [0.0, 1.0, 0.5],
            gap: false,
            tail: 0.5,
            hold_ms: 0,
            move_ms: 40,
            fade_out_ms: 100,
            fade_in_ms: 200,
            ..Snake::default()
        };
        let steps = announce(Error, &slots(&[Error]), &config);
        let lines = lines(&steps);
        // A front light at 0 is left out and there is no gap, so the head is two lights: ten steps.
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[0], "off 100ms ease");
        // No hold, so no delay is written.
        assert_eq!(
            lines[1],
            "#FF0000 #000000 #000000 #000000 #000000 #000000 #000000 #000000 40ms linear"
        );
        // The bright light, the back light at half, then the queue at half with nothing between.
        assert!(lines[4].starts_with("#000000 #800000 #800000 #FF0000 #000000 "));
        assert!(lines[11].ends_with("#FF0000 200ms ease"));
    }

    #[test]
    fn a_head_can_be_three_lights_with_no_gap() {
        let mut config = plain(7);
        config.snake.head = [0.2, 1.0, 0.6];
        config.snake.gap = false;
        let steps = announce(Stopped, &slots(&[Error]), &config);
        assert_eq!(
            lines(&steps)[5],
            "#000000 #4D0000 #009900 #00FF00 #003300 #000000 #000000 #000000 90ms linear 60ms"
        );
    }

    #[test]
    fn the_queue_crunches_against_the_front_end_then_springs_back() {
        let mut config = plain(7);
        config.snake = Snake {
            tail: 1.0,
            crunch: 0.5,
            crunch_ms: 120,
            bounce: 0.5,
            spring_ms: 300,
            ..Snake::default()
        };
        config.snake.fade_in = FadeIn::Together;
        let steps = announce(Working, &slots(&[Error, Stopped, Working]), &config);
        let arrival: Vec<&str> = lines(&steps).split_off(12);
        assert_eq!(
            arrival,
            [
                // Squeezed to half its length: the items pile up on the two lights at the front.
                "#000000 #000000 #000000 #000000 #000000 #000000 #0080FF #FF8000 120ms ease-out",
                // Sprung back a quarter past its resting length: as bright, reaching a light further.
                "#000000 #000000 #000000 #000000 #00009F #00609F #20DF00 #FF0000 180ms ease-in-out",
                "#000000 #000000 #000000 #000000 #000000 #0000FF #00FF00 #FF0000 120ms ease-in-out",
                // The rest of the 500 ms rise, which this tail at full brightness does not show.
                "#000000 #000000 #000000 #000000 #000000 #0000FF #00FF00 #FF0000 80ms ease-out",
            ]
        );
        // The arrival is one program, so no write falls in the middle of the crunch.
        let last = steps.last().unwrap();
        assert_eq!(last.program, arrival.join("\n"));
        assert_eq!(last.ms, 500);

        // Without a bounce the queue returns straight to rest.
        config.snake.bounce = 0.0;
        let steps = announce(Working, &slots(&[Error, Stopped, Working]), &config);
        let arrival: Vec<&str> = lines(&steps).split_off(12);
        assert_eq!(arrival.len(), 3);
        assert!(arrival[1].ends_with("#FF0000 300ms ease-in-out"));
    }

    #[test]
    fn the_queue_rises_while_it_crunches() {
        let mut config = plain(7);
        config.snake = Snake {
            tail: 0.5,
            fade_in_ms: 400,
            crunch: 0.5,
            crunch_ms: 100,
            bounce: 0.5,
            spring_ms: 300,
            ..Snake::default()
        };
        let steps = announce(Error, &slots(&[Error]), &config);
        // A quarter of the way up after the squeeze, 70% after springing back, full at rest.
        assert_eq!(
            lines(&steps).split_off(12),
            [
                "#000000 #000000 #000000 #000000 #000000 #000000 #000000 #9F0000 100ms ease-out",
                "#000000 #000000 #000000 #000000 #000000 #000000 #1B0000 #D90000 180ms ease-in-out",
                "#000000 #000000 #000000 #000000 #000000 #000000 #000000 #FF0000 120ms ease-in-out",
            ]
        );
        // A rise shorter than the crunch is over before the queue is at rest.
        config.snake.fade_in_ms = 100;
        let steps = announce(Error, &slots(&[Error]), &config);
        assert!(lines(&steps)[12].ends_with("#FF0000 100ms ease-out"));
    }

    #[test]
    fn the_queue_can_rise_all_at_once_or_front_to_back() {
        let mut config = plain(7);
        config.snake.fade_in_ms = 400;
        let queue = slots(&[Error, Stopped, Working]);
        let last = |config: &Config| {
            lines(&announce(Error, &queue, config))
                .pop()
                .unwrap()
                .to_owned()
        };

        config.snake.fade_in = FadeIn::Together;
        assert_eq!(
            last(&config),
            "#000000 #000000 #000000 #000000 #000000 #0000FF #00FF00 #FF0000 400ms ease"
        );
        // Each light takes half the rise and starts a quarter of it after the one in front.
        config.snake.fade_in = FadeIn::FrontToBack;
        assert_eq!(
            last(&config),
            "7:#FF0000 200ms ease;6:#00FF00 200ms ease 100ms;5:#0000FF 200ms ease 200ms"
        );
        // With the front on light 0 the rise runs the other way.
        config.front_led = 0;
        assert!(last(&config).starts_with("0:#FF0000 200ms ease;1:#00FF00"));
    }

    #[test]
    fn a_front_to_back_rise_runs_through_the_crunch() {
        let mut config = plain(7);
        config.snake = Snake {
            tail: 0.0,
            fade_in_ms: 400,
            crunch: 0.5,
            crunch_ms: 100,
            bounce: 0.0,
            spring_ms: 100,
            ..Snake::default()
        };
        let steps = announce(Error, &slots(&[Error, Working]), &config);
        assert_eq!(
            lines(&steps).split_off(12),
            [
                // The front light is halfway up as the squeeze ends; the one behind has not started.
                "#000000 #000000 #000000 #000000 #000000 #000000 #000000 #800000 100ms ease-out",
                "#000000 #000000 #000000 #000000 #000000 #000000 #000000 #FF0000 100ms ease-in-out",
                // The front light is done, so only the one behind it is left to rise.
                "6:#0000FF 200ms ease",
            ]
        );
    }

    #[test]
    fn a_full_strip_stays_lit_when_the_queue_springs_past_its_end() {
        let mut config = plain(7);
        config.snake = Snake {
            tail: 1.0,
            crunch: 0.5,
            bounce: 1.0,
            ..Snake::default()
        };
        let full = slots(&[[Error; 4].as_slice(), &[Working; 4]].concat());
        let steps = announce(Error, &full, &config);
        // One and a half times its length: every light is still fully lit, the errors reach
        // into a sixth light and most of the workers are past the end.
        assert_eq!(
            lines(&steps)[13],
            "#0000FF #0000FF #BF0040 #FF0000 #FF0000 #FF0000 #FF0000 #FF0000 210ms ease-in-out"
        );
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
        for (brightness, fade_in) in [
            (255, FadeIn::Together),
            (128, FadeIn::Together),
            (255, FadeIn::FrontToBack),
            (128, FadeIn::FrontToBack),
        ] {
            let mut config = Config {
                brightness,
                ..Config::default()
            };
            config.snake.fade_in = fade_in;
            config.snake = Snake {
                head: [1.0, 1.0, 1.0],
                hold_ms: 5000,
                move_ms: 5000,
                crunch: 1.0,
                crunch_ms: 5000,
                bounce: 1.0,
                spring_ms: 5000,
                ..config.snake
            };
            device::check(&steady(&crowded, &config)).unwrap();
            let steps = announce(Error, &crowded, &config);
            // Seventeen lines of full rows cannot fit in fewer than three programs.
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
