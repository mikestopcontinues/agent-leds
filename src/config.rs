//! Saved settings: the color of each state, brightness and which end is the front.

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::state::State;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Color(pub u8, pub u8, pub u8);

impl Color {
    pub const OFF: Color = Color(0, 0, 0);

    /// The same hue with its light multiplied by `factor`.
    pub fn scaled(self, factor: f64) -> Color {
        let channel = |value: u8| (f64::from(value) * factor).round().clamp(0.0, 255.0) as u8;
        Color(channel(self.0), channel(self.1), channel(self.2))
    }
}

impl FromStr for Color {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let digits = text.strip_prefix('#').unwrap_or(text);
        if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!("{text:?} is not a color; use RRGGBB"));
        }
        let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).unwrap();
        Ok(Color(channel(0), channel(2), channel(4)))
    }
}

impl TryFrom<String> for Color {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        text.parse()
    }
}

impl From<Color> for String {
    fn from(color: Color) -> String {
        color.to_string()
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Palette {
    pub error: Color,
    pub question: Color,
    pub stopped: Color,
    pub working: Color,
}

impl Palette {
    pub fn of(&self, state: State) -> Color {
        match state {
            State::Error => self.error,
            State::Question => self.question,
            State::Stopped => self.stopped,
            State::Working => self.working,
        }
    }

    pub fn set(&mut self, state: State, color: Color) {
        match state {
            State::Error => self.error = color,
            State::Question => self.question = color,
            State::Stopped => self.stopped = color,
            State::Working => self.working = color,
        }
    }
}

pub struct NamedPalette {
    pub name: &'static str,
    pub about: &'static str,
    pub colors: Palette,
}

const fn palette(error: u32, question: u32, stopped: u32, working: u32) -> Palette {
    const fn color(hex: u32) -> Color {
        Color((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }
    Palette {
        error: color(error),
        question: color(question),
        stopped: color(stopped),
        working: color(working),
    }
}

/// Palettes chosen for the Pro's RGB LEDs. Their light is linear in the value
/// sent and their three dies are nearly single-wavelength, so a mixed color
/// needs far less of its second die than the same color on a screen. A palette
/// sets hues only: the renderer scales the colors to look equally bright.
pub const PALETTES: [NamedPalette; 11] = [
    NamedPalette {
        name: "signal",
        about: "red, orange, green, blue: one die each, orange the only mix",
        colors: palette(0xFF0000, 0xFF5000, 0x00FF00, 0x0000FF),
    },
    NamedPalette {
        name: "bright",
        about: "signal with a yellower orange and a lighter blue",
        colors: palette(0xFF0000, 0xFF7000, 0x00FF30, 0x0050FF),
    },
    NamedPalette {
        name: "terminal",
        about: "red, yellow, green, cyan, like a terminal's colors",
        colors: palette(0xFF0000, 0xFFC000, 0x40FF00, 0x00A0FF),
    },
    NamedPalette {
        name: "neon",
        about: "pink, amber, mint, violet",
        colors: palette(0xFF0060, 0xFFA000, 0x00FF80, 0x3000FF),
    },
    NamedPalette {
        name: "vapor",
        about: "pink, yellow, cyan, purple: the widest spread of hues",
        colors: palette(0xFF0080, 0xFFD000, 0x00E0FF, 0x5000FF),
    },
    NamedPalette {
        name: "candy",
        about: "pink-red, orange, lime, cyan: no deep blue, so it runs brighter",
        colors: palette(0xFF0040, 0xFF8000, 0xA0FF00, 0x00C0FF),
    },
    NamedPalette {
        name: "sunset",
        about: "red, yellow, magenta, violet: no green states",
        colors: palette(0xFF2000, 0xFFD000, 0xFF00C0, 0x4000FF),
    },
    NamedPalette {
        name: "ice",
        about: "magenta, white, cyan, violet: cool, with white for questions",
        colors: palette(0xFF00C0, 0xFFFFFF, 0x00FFE0, 0x3000FF),
    },
    NamedPalette {
        name: "colorblind",
        about: "pink, yellow, white, blue: told apart without comparing red to green",
        colors: palette(0xFF0050, 0xFFB000, 0xFFFFFF, 0x0030FF),
    },
    NamedPalette {
        name: "soft",
        about: "signal with a little of every die in each color, less saturated",
        colors: palette(0xFF2010, 0xFF7020, 0x30FF40, 0x2040FF),
    },
    NamedPalette {
        name: "pastel",
        about: "washed-out red, yellow, green, blue: the brightest and gentlest",
        colors: palette(0xFF6050, 0xFFD060, 0x70FF90, 0x7090FF),
    },
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Overall brightness, 0-255.
    pub brightness: u8,
    /// The light that shows the front of the queue: 0 or 7.
    pub front_led: u8,
    /// How bright the red, green and blue dies look at full value, relative to
    /// each other. The renderer uses it to make every state equally bright.
    /// The default is an estimate from the dies' luminance and how much
    /// brighter saturated colors look than their luminance; tune it by eye.
    pub balance: [f64; 3],
    pub colors: Palette,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            brightness: 255,
            front_led: 7,
            balance: [0.32, 0.54, 0.14],
            colors: PALETTES[0].colors,
        }
    }
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_file();
        match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_file();
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, toml::to_string(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME is set"))
}

fn config_file() -> PathBuf {
    home().join(".config/leds/config.toml")
}

/// Where the daemon keeps its socket and log.
pub fn state_dir() -> PathBuf {
    home().join(".local/state/leds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_and_print() {
        assert_eq!("ff3a00".parse::<Color>().unwrap(), Color(0xFF, 0x3A, 0x00));
        assert_eq!("#00FF66".parse::<Color>().unwrap().to_string(), "#00FF66");
        assert!("red".parse::<Color>().is_err());
        assert!("#fff".parse::<Color>().is_err());
    }

    #[test]
    fn config_round_trips_and_fills_defaults() {
        let config = Config {
            brightness: 90,
            ..Config::default()
        };
        assert_eq!(
            toml::from_str::<Config>(&toml::to_string(&config).unwrap()).unwrap(),
            config
        );
        assert_eq!(toml::from_str::<Config>("brightness = 90").unwrap(), config);
    }

    #[test]
    fn palettes_keep_the_states_distinct() {
        for named in &PALETTES {
            let colors = State::ALL.map(|state| named.colors.of(state));
            for (index, color) in colors.iter().enumerate() {
                assert!(
                    !colors[..index].contains(color),
                    "{} repeats {color}",
                    named.name
                );
            }
        }
    }
}
