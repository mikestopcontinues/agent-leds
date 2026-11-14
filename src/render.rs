//! Turns the queue into LED programs for the device.
//!
//! A program is text in the device's LEDS.LED language. The firmware runs it
//! by itself, so a steady display costs nothing on the Mac.

use serde::Serialize;

use crate::config::{Color, Config, Palette, Snake};
use crate::device;
use crate::state::State;

pub const LEDS: usize = 8;

/// The colors of the lights by position along the strip, position 0 being
/// the front of the queue.
type Lights = [Color; LEDS];

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
    let lights = in_led_order(&queue(slots, &balanced(config), 1.0), config);
    let row = lights.map(|color| color.to_string()).join(" ");
    format!("{}{row} ease", settings(config))
}

/// What the lights reach by the end of one line of an animation.
struct Frame {
    lights: Lights,
    /// How long the lights first stay as they are, in milliseconds.
    hold_ms: u32,
    /// How long they then take to get there, at an even pace.
    move_ms: u32,
}

/// A line of a program and how long it runs.
type Line = (String, u32);

/// The programs for a session entering `state`, to be written one after
/// another: the snake.
///
/// The lights fade out. A head in the state's color enters at the end
/// furthest from the front of the queue and pulls the new queue in behind it
/// as a dimmed tail. The head runs off the front end, which leaves the queue
/// in place, and the queue rises to full brightness in a wave from its front
/// to its back.
pub fn announce(state: State, slots: &[State], config: &Config) -> Vec<Step> {
    let colors = balanced(config);
    let snake = &config.snake;

    let mut shown = [Color::OFF; LEDS];
    let mut line = |frame: &Frame| {
        let text = written(&shown, frame, config);
        shown = frame.lights;
        (text, frame.hold_ms + frame.move_ms)
    };
    // The fade and the snake's steps can be split between programs anywhere.
    let mut groups: Vec<Vec<Line>> = vec![vec![(
        format!("off {}ms ease", snake.fade_out_ms),
        snake.fade_out_ms,
    )]];
    for frame in crossing(state, slots, &colors, snake) {
        groups.push(vec![line(&frame)]);
    }
    // The rise is many short lines, so it is kept in one program where it
    // fits: a write in the middle of it could show as a hitch.
    let rise = rise(slots, &colors, snake);
    groups.push(rise.iter().map(&mut line).collect());
    packed(&groups, config)
}

/// About the longest a line of the rise runs. The firmware moves the lights
/// at an even pace within a line, so the rise is only as smooth as its lines
/// are short.
const RISE_LINE_MS: u32 = 45;
/// About the most lines the rise is cut into, which a long rise spreads out over.
const RISE_LINES: u32 = 14;

/// The dimmed queue, in place, rising to full brightness as a wave from the
/// front of the queue to the back.
///
/// Each light takes half the rise, quick at first and settling, and the
/// lights start evenly spaced so that the last one ends as the rise does.
/// The light the wave is about to reach goes dark while the one in front of
/// it starts, so the crest of the wave always stands against a dark light.
fn rise(slots: &[State], colors: &Palette, snake: &Snake) -> Vec<Frame> {
    let items = slots.len() as u32;
    // As shares of the whole rise: how long one light takes, and how long
    // after the light in front of it each starts.
    let (each, stagger) = match items {
        0 | 1 => (1.0, 0.0),
        _ => (0.5, 0.5 / f64::from(items - 1)),
    };
    // A light is darkest at the moment it starts, so the lines are cut to
    // end on every start: a whole number of them between one and the next.
    let lines = if items > 1 {
        let starts = 2 * (items - 1);
        let between = f64::from(snake.fade_in_ms) * stagger / f64::from(RISE_LINE_MS);
        starts * (between.round() as u32).clamp(1, (RISE_LINES / starts).max(1))
    } else {
        snake.fade_in_ms.div_ceil(RISE_LINE_MS).clamp(1, RISE_LINES)
    };

    (1..=lines)
        .filter_map(|line| {
            let from_ms = snake.fade_in_ms * (line - 1) / lines;
            let to_ms = snake.fade_in_ms * line / lines;
            let elapsed = f64::from(line) / f64::from(lines);
            let level = |item: usize| {
                let start = item as f64 * stagger;
                // Only a light behind the front one has a wave coming toward it.
                let dips = item > 0;
                if elapsed <= start {
                    let dipped = if dips {
                        ((elapsed - (start - stagger)) / stagger).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    snake.tail * (1.0 - dipped)
                } else {
                    let progress = ((elapsed - start) / each).min(1.0);
                    let eased_out = 1.0 - (1.0 - progress).powi(2);
                    let from = if dips { 0.0 } else { snake.tail };
                    from + (1.0 - from) * eased_out
                }
            };
            // A rise shorter than its lines has lines of no length, which are left out.
            (to_ms > from_ms).then(|| Frame {
                lights: std::array::from_fn(|position| {
                    slots.get(position).map_or(Color::OFF, |&state| {
                        colors.of(state).scaled(level(position))
                    })
                }),
                hold_ms: 0,
                move_ms: to_ms - from_ms,
            })
        })
        .collect()
}

/// The snake crossing the dark strip, one light at a time, until its head
/// has run off the front end and the dimmed queue is in place.
fn crossing(state: State, slots: &[State], colors: &Palette, snake: &Snake) -> Vec<Frame> {
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
    let tail = queue(slots, colors, snake.tail);

    // With the head's front `reach` lights in, queue item k trails the head at reach - head.len() - k.
    (0..LEDS + head.len())
        .map(|reach| Frame {
            lights: std::array::from_fn(|position| match reach.checked_sub(LEDS - 1 - position) {
                None => Color::OFF,
                Some(behind) if behind < head.len() => head[behind],
                Some(behind) => tail[behind - head.len()],
            }),
            // Each step holds still, then moves. Without the hold a light is at
            // its level only for an instant, so a bright light never looks bright.
            hold_ms: snake.hold_ms,
            move_ms: snake.move_ms,
        })
        .collect()
}

/// The queue at rest, at a share of its brightness.
fn queue(slots: &[State], colors: &Palette, level: f64) -> Lights {
    std::array::from_fn(|position| {
        slots
            .get(position)
            .map_or(Color::OFF, |&state| colors.of(state).scaled(level))
    })
}

/// The number of the light at a position along the queue, as the device
/// counts them. The same sum turns a light's number into its position.
fn led(position: usize, config: &Config) -> usize {
    if config.front_led == 0 {
        position
    } else {
        LEDS - 1 - position
    }
}

/// The lights by number rather than by position along the queue.
fn in_led_order(lights: &Lights, config: &Config) -> [Color; LEDS] {
    std::array::from_fn(|number| lights[led(number, config)])
}

/// The line that takes the lights from what they show to a frame, written
/// whichever of two ways is shorter, so that more lines fit in a program.
fn written(shown: &Lights, frame: &Frame, config: &Config) -> String {
    let (from, to) = (
        in_led_order(shown, config),
        in_led_order(&frame.lights, config),
    );
    // A row of colors from light 0. Lights past the end of a row go dark, so
    // trailing dark lights are left off. A single color would fill the strip,
    // so a row has at least two.
    let length = to
        .iter()
        .rposition(|&color| color != Color::OFF)
        .map_or(0, |last| last + 1)
        .max(2);
    let row = to[..length]
        .iter()
        .map(Color::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    // Only the lights that change, each by its number.
    let changed = (0..LEDS)
        .filter(|&led| to[led] != from[led])
        .map(|led| format!("{led}:{}", to[led]))
        .collect::<Vec<_>>()
        .join(" ");
    let colors = if changed.is_empty() || row.len() <= changed.len() {
        row
    } else {
        changed
    };
    let hold = match frame.hold_ms {
        0 => String::new(),
        ms => format!(" {ms}ms"),
    };
    format!("{colors} {}ms linear{hold}", frame.move_ms)
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

    const DARK: &str = "#000000 #000000 #000000 #000000 #000000 #000000 #000000 #000000";

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
        steps
            .iter()
            .flat_map(|step| step.program.lines())
            .filter(|line| !line.starts_with("brightness"))
            .collect()
    }

    /// The lights by number after each line of an animation, read as the
    /// firmware reads them, with the rest of the line: its timing.
    fn played(steps: &[Step]) -> Vec<(String, String)> {
        let mut lights = vec!["#000000"; LEDS];
        lines(steps)
            .into_iter()
            .map(|line| {
                let mut timings = Vec::new();
                // A line is one or more segments, each with its own lights and timing.
                for segment in line.split(';') {
                    let (colors, timing): (Vec<&str>, Vec<&str>) = segment
                        .split(' ')
                        .partition(|token| token.contains('#') || *token == "off");
                    if colors[0].contains(':') {
                        for (led, color) in
                            colors.iter().map(|token| token.split_once(':').unwrap())
                        {
                            lights[led.parse::<usize>().unwrap()] = color;
                        }
                    } else if colors == ["off"] {
                        lights.fill("#000000");
                    } else {
                        assert!(colors.len() > 1, "a single color would fill the strip");
                        for (led, light) in lights.iter_mut().enumerate() {
                            *light = colors.get(led).copied().unwrap_or("#000000");
                        }
                    }
                    timings.push(timing.join(" "));
                }
                (lights.join(" "), timings.join(";"))
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
        let played = played(&steps);
        assert_eq!(played[0], (DARK.to_owned(), "300ms ease".to_owned()));
        // The head's front enters at light 0, the end furthest from the front, at 60%.
        assert_eq!(
            played[1],
            (
                "#009900 #000000 #000000 #000000 #000000 #000000 #000000 #000000".to_owned(),
                "90ms linear 60ms".to_owned()
            )
        );
        // Front light, bright light, dark light, then the queue at 30% with its front first.
        assert_eq!(
            played[6].0,
            "#000000 #004D00 #4D0000 #000000 #00FF00 #009900 #000000 #000000"
        );
        // The head has left the front end and the dimmed queue is in place.
        assert_eq!(
            played[11].0,
            "#000000 #000000 #000000 #000000 #000000 #000000 #004D00 #4D0000"
        );
        // The rise is cut into twelve lines and ends on the queue.
        assert_eq!(played.len(), 1 + 11 + 12);
        assert_eq!(
            played[23].0,
            "#000000 #000000 #000000 #000000 #000000 #000000 #00FF00 #FF0000"
        );
        // Its running time is the fades plus eleven steps.
        let total: u64 = steps.iter().map(|step| step.ms).sum();
        assert_eq!(total, 300 + 11 * 150 + 500);
    }

    #[test]
    fn a_line_is_written_the_shorter_way() {
        let steps = announce(Stopped, &slots(&[Error, Stopped]), &plain(7));
        let lines = lines(&steps);
        // One light changes: it is named.
        assert_eq!(lines[1], "0:#009900 90ms linear 60ms");
        // The first two lights change and the rest are dark: a short row says so in less.
        assert_eq!(lines[2], "#00FF00 #009900 90ms linear 60ms");
        // With the front on light 0 the snake enters at light 7.
        let steps = announce(Stopped, &[], &plain(0));
        assert_eq!(lines_of(&steps)[1], "7:#009900 90ms linear 60ms");

        fn lines_of(steps: &[Step]) -> Vec<&str> {
            steps.iter().flat_map(|step| step.program.lines()).collect()
        }
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
        let played = played(&announce(Error, &slots(&[Error]), &config));
        // A front light at 0 is left out and there is no gap, so the head is
        // two lights: ten steps. Then the rise, in five lines.
        assert_eq!(played.len(), 1 + 10 + 5);
        assert_eq!(played[0].1, "100ms ease");
        // No hold, so no delay is written.
        assert_eq!(played[1].1, "40ms linear");
        assert!(played[1].0.starts_with("#FF0000 #000000 "));
        // The bright light, the back light at half, then the queue at half with nothing between.
        assert!(
            played[4]
                .0
                .starts_with("#000000 #800000 #800000 #FF0000 #000000 ")
        );
        assert_eq!(played[10].0, format!("{} #800000", &DARK[..55]));
        assert_eq!(
            played[15],
            (format!("{} #FF0000", &DARK[..55]), "40ms linear".to_owned())
        );
    }

    #[test]
    fn a_head_can_be_three_lights_with_no_gap() {
        let mut config = plain(7);
        config.snake.head = [0.2, 1.0, 0.6];
        config.snake.gap = false;
        let played = played(&announce(Stopped, &slots(&[Error]), &config));
        assert_eq!(
            played[5].0,
            "#000000 #4D0000 #009900 #00FF00 #003300 #000000 #000000 #000000"
        );
    }

    /// A snake whose rise takes this long from this tail brightness.
    fn rising(fade_in_ms: u32, tail: f64) -> Snake {
        Snake {
            fade_in_ms,
            tail,
            ..Snake::default()
        }
    }

    #[test]
    fn the_queue_rises_in_a_wave_with_a_dark_light_ahead_of_its_crest() {
        let queue = [Error, Stopped, Working];
        let frames = rise(&queue, &plain(7).colors, &rising(360, 0.5));
        // Each light takes half the rise and starts a quarter of it after the
        // one in front: eight lines of 45 ms, two between starts.
        assert_eq!(frames.len(), 8);
        assert!(frames.iter().all(|frame| frame.move_ms == 45));
        let lights = |line: usize| frames[line - 1].lights[..3].to_vec();
        // An eighth in: the front light is rising, the next is on its way down, the last waits.
        assert_eq!(
            lights(1),
            [Color(183, 0, 0), Color(0, 64, 0), Color(0, 0, 128)]
        );
        // A quarter in, the second light starts, from dark: the front one is the crest.
        assert_eq!(lights(2), [Color(223, 0, 0), Color::OFF, Color(0, 0, 128)]);
        // Halfway, the front light is done, the second is the crest and the last is dark.
        assert_eq!(lights(4), [Color(255, 0, 0), Color(0, 191, 0), Color::OFF]);
        assert_eq!(
            lights(8),
            [Color(255, 0, 0), Color(0, 255, 0), Color(0, 0, 255)]
        );
    }

    #[test]
    fn a_light_rises_quickly_at_first_then_settles() {
        // A single light has no wave to wait for: it takes the whole rise, from the tail's brightness.
        let frames = rise(&[Error], &plain(7).colors, &rising(450, 0.0));
        let reds: Vec<u8> = frames.iter().map(|frame| frame.lights[0].0).collect();
        assert_eq!(reds.len(), 10);
        assert!(reds.is_sorted());
        // Three quarters of the way up at half time.
        assert_eq!(reds[4], 191);
        assert_eq!(reds[9], 255);
        let from_tail = rise(&[Error], &plain(7).colors, &rising(450, 0.4));
        assert!(from_tail[0].lights[0].0 > 102);
    }

    #[test]
    fn the_rise_is_cut_into_a_few_lines_that_add_up_to_it() {
        let colors = plain(7).colors;
        for items in 0..=LEDS {
            for ms in [1, 46, 500, 750, 5000] {
                let frames = rise(&vec![Working; items], &colors, &rising(ms, 0.3));
                assert!(frames.len() <= 14, "{items} items over {ms} ms");
                assert_eq!(frames.iter().map(|frame| frame.move_ms).sum::<u32>(), ms);
                assert!(frames.iter().all(|frame| frame.move_ms > 0));
                // It always ends on the queue.
                assert_eq!(
                    frames.last().unwrap().lights,
                    queue(&vec![Working; items], &colors, 1.0)
                );
            }
        }
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
            config.snake = Snake {
                head: [1.0, 1.0, 1.0],
                hold_ms: 5000,
                move_ms: 5000,
                fade_in_ms: 5000,
                ..config.snake
            };
            device::check(&steady(&crowded, &config)).unwrap();
            let steps = announce(Error, &crowded, &config);
            // Twenty-seven lines, many of them full rows, cannot fit in fewer than three programs.
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
