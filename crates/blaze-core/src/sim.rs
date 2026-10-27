//! A deterministic [`Driver`]: one thread, a virtual clock and a seeded scheduler, so a state
//! machine's run under it is a function of the seed. It delays completions, delivers the ones
//! that are ready in a shuffled order, fails effects at a chosen rate, and [`Sim::crash`]
//! loses what was not synced, keeping a random prefix of it (a torn append), as a power cut
//! does. A failing seed replays exactly.

use std::{collections::BTreeMap, time::Duration};

use crate::{Completion, Done, Driver, Effect, FileName, IoError};

/// SplitMix64: small, fast, and the same everywhere.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True once in `n` on average; never when `n` is 0.
    pub fn one_in(&mut self, n: u32) -> bool {
        n != 0 && self.below(u64::from(n)) == 0
    }
}

/// How unkind the simulated machine is.
#[derive(Clone, Debug)]
pub struct Faults {
    /// An effect fails once in this many (0: never). A failed append writes a random part of
    /// its bytes, as a short write does; a failed sync makes nothing durable.
    pub fail_one_in: u32,
    /// Each completion is ready after a delay drawn up to this.
    pub max_latency: Duration,
}

impl Default for Faults {
    fn default() -> Faults {
        Faults { fail_one_in: 0, max_latency: Duration::from_micros(200) }
    }
}

#[derive(Default, Clone, Debug)]
struct SimFile {
    /// What survives a crash.
    durable: Vec<u8>,
    /// Appended since the last sync that covered it; read as part of the file, lost (in part)
    /// by a crash.
    volatile: Vec<u8>,
}

#[derive(Debug)]
struct Pending {
    ready: Duration,
    effect: Effect,
    /// For a sync: how many volatile bytes the file had when it was submitted.
    covers: usize,
}

pub struct Sim {
    rng: Rng,
    faults: Faults,
    now: Duration,
    files: BTreeMap<FileName, SimFile>,
    pending: Vec<Pending>,
}

impl Sim {
    pub fn new(seed: u64, faults: Faults) -> Sim {
        Sim {
            rng: Rng::new(seed),
            faults,
            now: Duration::ZERO,
            files: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    /// The machine loses power: every effect in flight is gone without a completion, and each
    /// file keeps what was synced plus a random prefix of what was appended after.
    pub fn crash(&mut self) {
        self.pending.clear();
        for file in self.files.values_mut() {
            let kept = self.rng.below(file.volatile.len() as u64 + 1) as usize;
            file.durable.extend_from_slice(&file.volatile[..kept]);
            file.volatile.clear();
        }
    }

    /// What a file holds now, synced or not.
    pub fn contents(&self, file: &FileName) -> Vec<u8> {
        self.files
            .get(file)
            .map(|it| [it.durable.as_slice(), &it.volatile].concat())
            .unwrap_or_default()
    }

    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }

    fn complete(&mut self, pending: Pending) -> Completion {
        let tag = pending.effect.tag();
        if !matches!(pending.effect, Effect::Timer { .. })
            && self.rng.one_in(self.faults.fail_one_in)
        {
            if let Effect::Append { file, bytes, .. } = &pending.effect {
                let torn = self.rng.below(bytes.len() as u64) as usize;
                self.files
                    .entry(file.clone())
                    .or_default()
                    .volatile
                    .extend_from_slice(&bytes[..torn]);
            }
            return Completion {
                tag,
                done: Done::Failed(IoError("injected by the simulation".into())),
            };
        }
        let done = match pending.effect {
            Effect::ReadAll { file, .. } => Done::Read(self.contents(&file)),
            Effect::Read { file, at, len, .. } => {
                let all = self.contents(&file);
                let at = (at as usize).min(all.len());
                Done::Read(all[at..(at + len).min(all.len())].to_vec())
            }
            Effect::Append { file, bytes, .. } => {
                self.files.entry(file).or_default().volatile.extend_from_slice(&bytes);
                Done::Appended
            }
            Effect::Sync { file, .. } => {
                let file = self.files.entry(file).or_default();
                let covered = pending.covers.min(file.volatile.len());
                file.durable.extend(file.volatile.drain(..covered));
                Done::Synced
            }
            // Simpler than a disk: the cut is not undone by a crash before the next sync.
            Effect::Truncate { file, len, .. } => {
                let file = self.files.entry(file).or_default();
                let len = len as usize;
                if len <= file.durable.len() {
                    file.durable.truncate(len);
                    file.volatile.clear();
                } else {
                    file.volatile.truncate(len - file.durable.len());
                }
                Done::Truncated
            }
            Effect::Timer { .. } => Done::Fired,
        };
        Completion { tag, done }
    }
}

impl Driver for Sim {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for effect in effects.drain(..) {
            let delay = match &effect {
                Effect::Timer { after, .. } => *after,
                _ => Duration::from_nanos(
                    self.rng.below(self.faults.max_latency.as_nanos() as u64 + 1),
                ),
            };
            let covers = match &effect {
                Effect::Sync { file, .. } => self.files.get(file).map_or(0, |it| it.volatile.len()),
                _ => 0,
            };
            self.pending.push(Pending { ready: self.now + delay, effect, covers });
        }
    }

    /// Never waits: time jumps to the first completion when none is ready.
    fn poll(&mut self, out: &mut Vec<Completion>, _wait: Duration) -> usize {
        let Some(first) = self.pending.iter().map(|it| it.ready).min() else { return 0 };
        self.now = self.now.max(first);
        let mut ready = Vec::new();
        let mut index = 0;
        while index < self.pending.len() {
            if self.pending[index].ready <= self.now {
                ready.push(self.pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        // `swap_remove` already disturbed the order; shuffle so it is the seed's, not the vec's.
        for at in (1..ready.len()).rev() {
            let other = self.rng.below(at as u64 + 1) as usize;
            ready.swap(at, other);
        }
        let count = ready.len();
        for pending in ready {
            let completion = self.complete(pending);
            out.push(completion);
        }
        count
    }

    fn in_flight(&self) -> usize {
        self.pending.len()
    }

    fn now(&self) -> Duration {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tag, run_until_idle};

    fn name(it: &str) -> FileName {
        FileName::new(it).unwrap()
    }

    #[test]
    fn a_crash_keeps_what_was_synced_and_a_prefix_of_the_rest() {
        for seed in 0..200 {
            let mut sim = Sim::new(seed, Faults::default());
            let file = name("log");
            let mut effects =
                vec![Effect::Append { file: file.clone(), bytes: b"synced".to_vec(), tag: Tag(0) }];
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Sync { file: file.clone(), tag: Tag(1) });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            effects.push(Effect::Append {
                file: file.clone(),
                bytes: b"-lost".to_vec(),
                tag: Tag(2),
            });
            run_until_idle(&mut sim, &mut effects, |_, _| {});
            sim.crash();
            let left = sim.contents(&file);
            assert!(left.starts_with(b"synced") && b"synced-lost".starts_with(&left), "{left:?}");
        }
    }

    #[test]
    fn the_same_seed_is_the_same_run() {
        let run = |seed| {
            let mut sim = Sim::new(seed, Faults { fail_one_in: 4, ..Faults::default() });
            let mut effects: Vec<Effect> = (0..32)
                .map(|n| Effect::Append { file: name("f"), bytes: vec![n as u8], tag: Tag(n) })
                .collect();
            let mut order = Vec::new();
            run_until_idle(&mut sim, &mut effects, |done, _| order.push(done));
            (order, sim.contents(&name("f")))
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn a_sync_covers_only_what_was_appended_before_it() {
        let mut sim = Sim::new(1, Faults { fail_one_in: 0, max_latency: Duration::ZERO });
        let file = name("log");
        let mut effects =
            vec![Effect::Append { file: file.clone(), bytes: b"a".to_vec(), tag: Tag(0) }];
        run_until_idle(&mut sim, &mut effects, |_, _| {});
        sim.submit(&mut vec![Effect::Sync { file: file.clone(), tag: Tag(1) }]);
        // Appended after the sync was asked for, completed before it.
        sim.files.get_mut(&file).unwrap().volatile.extend_from_slice(b"b");
        let mut out = Vec::new();
        sim.poll(&mut out, Duration::ZERO);
        let file = &sim.files[&file];
        assert_eq!((file.durable.as_slice(), file.volatile.as_slice()), (&b"a"[..], &b"b"[..]));
    }
}
