//! A driver lent over the world's own backend (drive.md, "D114 rulings", "The store under the
//! command", ruling 5: one driver a process). A state machine written over
//! [`blaze_core::Driver`] (blaze-cas's plain store, `Driven<D>`) runs on the command's world
//! through it, with no second native driver made.
//!
//! [`LentDriver`] is rooted at a directory ([`crate::fs::Dir::lend`]): every file an effect
//! names must lie beneath it, else the effect completes at once with [`Done::Failed`] naming
//! the file, as [`crate::fs::Fs`] refuses a path outside its tree; and only file effects and a
//! timer are carried (a stream, a socket or a process is refused by name). Each carried effect
//! gets a fresh tag of the world ([`Io::submit`]), and the lent driver keeps the table from that
//! tag back to the asker's. Its poll takes from the world's kept completions ONLY those of the
//! tags in its table, rewritten back; any other completion a poll brings stays kept for its own
//! waiter ([`Io::take`]), exactly as a [`crate::cx::Cx`] wait leaves it. An [`Effect::Unlock`]'s
//! `lock` (the tag of the lock it lets go) is rewritten to the world's tag of that lock.
//!
//! Every effect the store asks completes once, so a tag leaves the table with its completion.

use alloc::{format, vec::Vec};
use core::time::Duration;

use blaze_core::wait::Deadline;
use blaze_core::{Boot, Completion, Done, Driver, Effect, FileName, IoError, NoBoot, Tag};

use super::Io;
use crate::fs::{Dir, ReadWrite};
use crate::path::Path;

/// A planted bug of the tests below.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LendPlant {
    None,
    /// The poll hands back every completion the world kept, another waiter's too.
    ReturnsOthers,
    /// No path is checked against the directory.
    NoFence,
}

/// The world's backend, lent to one state machine, fenced to one directory.
pub struct LentDriver {
    io: Io,
    /// Every file named must be beneath it (or be it).
    dir: FileName,
    /// In flight: the world's tag, the asker's.
    flight: Vec<(Tag, Tag)>,
    /// Locks taken through this driver and not let go: the asker's tag, the world's.
    locks: Vec<(Tag, Tag)>,
    /// Refused at submit, handed back by the next poll.
    refused: Vec<Completion>,
    #[cfg(test)]
    plant: LendPlant,
}

impl Dir<ReadWrite> {
    /// The world's driver lent to a state machine, fenced to `folder` beneath this directory
    /// (made by the machine's own effects when missing). [`LentDriver::dir`] is the folder's
    /// name as the driver names it: what the machine is configured with.
    pub fn lend(&self, folder: &Path) -> LentDriver {
        // clone: authority held, a refcount.
        let dir = self.path_beneath(folder).name().clone();
        LentDriver {
            io: self.io().clone(),
            dir,
            flight: Vec::new(),
            locks: Vec::new(),
            refused: Vec::new(),
            #[cfg(test)]
            plant: LendPlant::None,
        }
    }
}

impl LentDriver {
    /// The folder it is fenced to, as the driver names files.
    pub fn dir(&self) -> &FileName {
        &self.dir
    }

    #[cfg(test)]
    pub(crate) fn planted(mut self, plant: LendPlant) -> LentDriver {
        self.plant = plant;
        self
    }

    fn fenced(&self) -> bool {
        #[cfg(test)]
        if self.plant == LendPlant::NoFence {
            return false;
        }
        true
    }

    /// Why `effect` is not carried: the file it names outside the folder, or its kind.
    fn refusal(&self, effect: &Effect) -> Option<alloc::string::String> {
        let under = |file: &FileName| !self.fenced() || file.is_under(&self.dir);
        let outside = |file: &FileName| Some(format!("{file} is outside {}, the folder this driver was lent for", self.dir));
        match effect {
            Effect::ReadAll { file, .. }
            | Effect::Read { file, .. }
            | Effect::ReadInto { file, .. }
            | Effect::Stat { file, .. }
            | Effect::Lstat { file, .. }
            | Effect::Map { file, .. }
            | Effect::Append { file, .. }
            | Effect::Sync { file, .. }
            | Effect::Barrier { file, .. }
            | Effect::WriteBack { file, .. }
            | Effect::Truncate { file, .. }
            | Effect::Write { file, .. }
            | Effect::WriteUnsynced { file, .. }
            | Effect::WriteAt { file, .. }
            | Effect::Remove { file, .. }
            | Effect::Lock { file, .. } => (!under(file)).then(|| outside(file)).flatten(),
            Effect::Rename { from, to, .. } | Effect::RenameUnsynced { from, to, .. } => [from, to].into_iter().find(|it| !under(it)).and_then(outside),
            Effect::MakeDir { dir, .. } => (!under(dir)).then(|| outside(dir)).flatten(),
            Effect::List { dir, .. } | Effect::SyncDir { dir, .. } => match dir.name() {
                Some(name) if under(name) => None,
                Some(name) => outside(name),
                None if !self.fenced() => None,
                None => Some(format!("the driver's root is outside {}, the folder this driver was lent for", self.dir)),
            },
            Effect::Unlock { .. } | Effect::Timer { .. } => None,
            other => Some(format!("a lent driver carries file effects and timers only, not the effect of tag {:#x}", other.tag().0)),
        }
    }

    /// Moves every completion of this driver's tags the world kept into `out`, rewritten to the
    /// asker's tags: how many.
    fn take_mine(&mut self, out: &mut Vec<Completion>) -> usize {
        let mut moved = self.refused.len();
        out.append(&mut self.refused);
        #[cfg(test)]
        if self.plant == LendPlant::ReturnsOthers {
            let mut world = self.io.world().lock();
            for completion in world.got.drain(..) {
                let tag = self.flight.iter().find(|(world_tag, _)| *world_tag == completion.tag).map_or(completion.tag, |(_, own)| *own);
                self.flight.retain(|(world_tag, _)| *world_tag != completion.tag);
                out.push(Completion { tag, done: completion.done });
                moved += 1;
            }
            return moved;
        }
        let mut at = 0;
        while at < self.flight.len() {
            let (world_tag, own) = self.flight[at];
            match self.io.take(world_tag) {
                Some(done) => {
                    self.flight.swap_remove(at);
                    // A lock taken: kept so its unlock can name the world's tag. A lock
                    // contended or failed holds nothing; an unlock's own end names nothing.
                    if matches!(done, Done::Locked) {
                        self.locks.push((own, world_tag));
                    }
                    out.push(Completion { tag: own, done });
                    moved += 1;
                }
                None => at += 1,
            }
        }
        moved
    }
}

impl Driver for LentDriver {
    fn submit(&mut self, effects: &mut Vec<Effect>) {
        for mut effect in effects.drain(..) {
            let own = effect.tag();
            if let Some(why) = self.refusal(&effect) {
                self.refused.push(Completion { tag: own, done: Done::Failed(IoError(why)) });
                continue;
            }
            if let Effect::Unlock { lock, .. } = &mut effect {
                match self.locks.iter().position(|(asked, _)| asked == lock) {
                    Some(at) => *lock = self.locks.swap_remove(at).1,
                    None => {
                        self.refused.push(Completion { tag: own, done: Done::Failed(IoError(format!("an unlock of tag {:#x}, which no lock through this driver took", lock.0))) });
                        continue;
                    }
                }
            }
            let world_tag = self.io.submit(move |fresh| {
                *effect.tag_mut() = fresh;
                effect
            });
            self.flight.push((world_tag, own));
        }
    }

    fn poll(&mut self, out: &mut Vec<Completion>, until: Deadline) -> usize {
        loop {
            let moved = self.take_mine(out);
            if moved > 0 || self.flight.is_empty() {
                return moved;
            }
            // Nothing of ours kept: one poll of the world, which keeps what it brings for each
            // waiter. Polled nothing: the world waited to `until` already.
            if self.io.poll(until) == 0 || until.passed(self.io.now()) {
                return self.take_mine(out);
            }
        }
    }

    fn in_flight(&self) -> usize {
        self.flight.len() + self.refused.len()
    }

    fn now(&self) -> Duration {
        self.io.now()
    }

    fn wall_ns(&self) -> u64 {
        self.io.wall_ns()
    }

    fn boot(&self) -> Result<Boot, NoBoot> {
        self.io.boot()
    }
}

#[cfg(test)]
#[path = "lend_tests.rs"]
mod tests;
