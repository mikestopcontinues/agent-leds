//! The harnesses whose own files are not read yet. Until each has a source of
//! its own, its hooks say what its sessions do, as they did before files
//! led, and the hook process's ancestors say which process runs the session,
//! so a session whose process ends leaves even when no hook says so.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{Event, Hook, Observation, Source, Start};
use crate::fsm::{Channel, Signal};
use crate::harness::{self, Harness};
use crate::liveness::{self, Kernel, Liveness, Proc, StartCheck};
use crate::queue::{Key, TurnKey};

/// One harness read from its hooks alone.
pub struct Interim {
    harness: Harness,
    /// Where Junie's hook keeps the session its last prompt named.
    state: PathBuf,
}

impl Interim {
    pub fn new(harness: Harness, state: &Path) -> Interim {
        Interim {
            harness,
            state: state.to_owned(),
        }
    }
}

/// Which of the hook's ancestors runs the session, until the harness's own
/// files name it: the parent for OpenCode and Pi, whose plugins run the hook
/// themselves, and for Codex, Grok and Junie the ancestor with the harness's
/// process name.
fn process(harness: Harness, ancestry: &[Proc]) -> Option<Liveness> {
    let exact = |proc: &Proc| Liveness::Process {
        pid: proc.pid,
        start: StartCheck::ExactUs(proc.started_us),
    };
    let named = |name: &str| ancestry.iter().find(|proc| proc.name == name).map(exact);
    match harness {
        Harness::OpenCode | Harness::Pi => ancestry.first().map(exact),
        Harness::Codex => named("codex"),
        Harness::Grok => named("grok-native"),
        Harness::Junie => named("junie"),
        Harness::Claude | Harness::Cursor => None,
    }
}

impl Source for Interim {
    fn harness(&self) -> Harness {
        self.harness
    }

    fn roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Each hook-bound session whose process still runs, started again. No
    /// file says where it stood, so the saved queue does.
    fn seed(&mut self, k: &dyn Kernel, bound: &[(String, Liveness)]) -> Result<Vec<Observation>> {
        Ok(bound
            .iter()
            .filter(|(_, liveness)| liveness::alive(k, liveness))
            .map(|(id, liveness)| Observation {
                key: Key {
                    harness: self.harness,
                    id: id.clone(),
                },
                channel: Channel::Hook,
                event: Event::Started(Start {
                    cwd: String::new(),
                    liveness: liveness.clone(),
                }),
                turn: None,
                quiet: true,
            })
            .collect())
    }

    fn changed(&mut self, _k: &dyn Kernel, _paths: Option<&[PathBuf]>) -> Vec<Observation> {
        Vec::new()
    }

    fn hook(&mut self, _k: &dyn Kernel, hook: &Hook) -> Vec<Observation> {
        let Some((id, cwd, signals)) = harness::interim(self.harness, &hook.payload, &self.state)
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
        // Every event but the session's end says its process runs it.
        if !signals.contains(&Signal::Ended)
            && let Some(liveness) = process(self.harness, &hook.ancestry)
        {
            out.push(observation(Event::Started(Start { cwd, liveness })));
        }
        out.push(observation(Event::Signals(signals)));
        out
    }

    /// Nothing: where a hook-bound session stands is the saved queue's to say.
    fn current(&self, _id: &str) -> Vec<(Signal, Option<TurnKey>)> {
        Vec::new()
    }

    fn behind(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::Phase;
    use crate::liveness::System;
    use crate::source::fixture;

    #[test]
    fn a_daemon_starting_again_starts_each_hook_bound_session_still_running() {
        let home = tempfile::tempdir().unwrap();
        let mut source = Interim::new(Harness::Codex, home.path());
        let pid = std::process::id() as i32;
        let running = Liveness::Process {
            pid,
            start: StartCheck::ExactUs(System.started_us(pid).unwrap()),
        };
        let ended = Liveness::Process {
            pid,
            start: StartCheck::ExactUs(1),
        };
        let bound = [("a".to_owned(), running.clone()), ("b".to_owned(), ended)];
        let seeded = source.seed(&System, &bound).unwrap();
        assert_eq!(seeded.len(), 1);
        let o = &seeded[0];
        assert_eq!(
            (o.key.id.as_str(), o.channel, o.quiet),
            ("a", Channel::Hook, true)
        );
        assert!(matches!(&o.event, Event::Started(start) if start.liveness == running));
    }

    #[test]
    fn a_subagent_at_work_keeps_its_parent_asking() {
        let replay = fixture::replay(
            Harness::OpenCode,
            "a_subagent_at_work_keeps_its_parent_asking",
        );
        let phases: Vec<Phase> = replay.phases().into_values().collect();
        assert_eq!(phases, [Phase::Blocked]);
    }
}
