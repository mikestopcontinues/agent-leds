//! The harnesses whose own files are not read yet. Until each has a source of
//! its own, its hooks say what its sessions do, as they did before files
//! led, and the hook process's ancestors say which process runs the session,
//! so a session whose process ends leaves even when no hook says so.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{Event, Hook, Observation, Source, Start, Told};
use crate::fsm::{Channel, Signal};
use crate::harness::{self, Harness};
use crate::liveness::{self, Kernel, Liveness, Proc, StartCheck};
use crate::queue::{Key, TurnKey};

/// One harness read from its hooks alone.
pub struct Interim {
    harness: Harness,
    home: PathBuf,
    /// Where Junie's hook keeps the session its last prompt named.
    state: PathBuf,
    /// What the hooks have told of each session, and the process running it.
    told: HashMap<String, (Told, Option<Liveness>)>,
}

impl Interim {
    pub fn new(harness: Harness, home: &Path, state: &Path) -> Interim {
        Interim {
            harness,
            home: home.to_owned(),
            state: state.to_owned(),
            told: HashMap::new(),
        }
    }
}

/// Which of the hook's ancestors runs the session, until the harness's own
/// files name it: the parent for OpenCode and Pi, whose plugins run the hook
/// themselves; the ancestor with Cursor's own `.running` marker; and for
/// Codex, Grok and Junie the ancestor with the harness's process name.
fn process(harness: Harness, home: &Path, ancestry: &[Proc]) -> Option<Liveness> {
    let exact = |proc: &Proc| Liveness::Process {
        pid: proc.pid,
        start: StartCheck::ExactUs(proc.started_us),
    };
    let named = |name: &str| ancestry.iter().find(|proc| proc.name == name).map(exact);
    match harness {
        Harness::OpenCode | Harness::Pi => ancestry.first().map(exact),
        Harness::Cursor => ancestry.iter().find_map(|proc| cursor(home, proc)),
        Harness::Codex => named("codex"),
        Harness::Grok => named("grok-native"),
        Harness::Junie => named("junie"),
        Harness::Claude => None,
    }
}

/// The process, if Cursor marked it running: Cursor writes
/// `versions/<version>/.running/<pid>` holding `<command>|<ps lstart>`, and
/// the command is `cursor-agent` or, since 2026.11, `node`.
fn cursor(home: &Path, proc: &Proc) -> Option<Liveness> {
    let versions = home.join(".local/share/cursor-agent/versions");
    for version in fs::read_dir(versions).ok()?.flatten() {
        let marker = version.path().join(".running").join(proc.pid.to_string());
        let Ok(text) = fs::read_to_string(marker) else {
            continue;
        };
        let start = text
            .trim()
            .split_once('|')
            .and_then(|(_, start)| liveness::lstart(start));
        if start == Some((proc.started_us / 1_000_000) as i64) {
            return Some(Liveness::Process {
                pid: proc.pid,
                start: StartCheck::Second(start?),
            });
        }
    }
    None
}

impl Source for Interim {
    fn harness(&self) -> Harness {
        self.harness
    }

    fn roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// No file says where a hook-bound session stood, so its next hook
    /// starts it again.
    fn seed(&mut self, _k: &dyn Kernel, _bound: &[(String, Liveness)]) -> Result<Vec<Observation>> {
        Ok(Vec::new())
    }

    fn changed(&mut self, _k: &dyn Kernel, _paths: Option<&[PathBuf]>) -> Vec<Observation> {
        Vec::new()
    }

    fn hook(&mut self, k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let Some((id, cwd, signal)) = harness::interim(self.harness, &hook.payload, &self.state)
        else {
            return Vec::new();
        };
        let key = Key {
            harness: self.harness,
            id: id.clone(),
        };
        let observation = |event| Observation {
            key: key.clone(),
            channel: Channel::Hook,
            event,
            turn: None,
            quiet: false,
        };
        let mut out = Vec::new();
        if signal != Signal::Ended
            && let Some(liveness) = process(self.harness, &self.home, &hook.ancestry)
        {
            let (told, running) = self
                .told
                .entry(id.clone())
                .or_insert((Told::default(), None));
            // A session whose process ended starts again from Idle.
            if running
                .as_ref()
                .is_some_and(|running| !liveness::alive(k, running))
            {
                *told = Told::default();
            }
            *running = Some(liveness.clone());
            out.push(observation(Event::Started(Start { cwd, liveness })));
        }
        if let Some((told, _)) = self.told.get_mut(&id) {
            told.tell(signal);
        }
        if signal == Signal::Ended {
            self.told.remove(&id);
        }
        out.push(observation(Event::Signal(signal)));
        out
    }

    fn current(&self, id: &str) -> Vec<(Signal, Option<TurnKey>)> {
        let Some((told, _)) = self.told.get(id) else {
            return Vec::new();
        };
        told.current()
            .into_iter()
            .map(|signal| (signal, None))
            .collect()
    }

    fn behind(&self) -> bool {
        false
    }
}
