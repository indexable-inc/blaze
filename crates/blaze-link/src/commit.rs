//! The commit: the one serial step. A stage's writes go to a copy of the image (a clone that
//! shares its blocks, or a new file for a full link), which is synced and renamed over the
//! image, so a reader or a crash sees the old image or the new. Running programs are told
//! only by a commit into their code memory ([`Image::commit_into`]), never by a file commit.
//! Effects complete in any order: each step waits for all of its own.
//!
//! The effects of every step are one queue, cut by each step's count, and their tags are
//! given out one after another, so the effects in flight are a range of tags and a bitset
//! of those done: no set, no per-step vector.

#![forbid(unsafe_code)]

use std::collections::VecDeque;

use blaze_core::{Completion, Done, Effect, FileName, Tag};
use blaze_ids::bits::BitSet;
use blaze_inproc::{CodeMemory, CodeRange};

use crate::file;
use crate::state::State;
use crate::{Image, LinkError, Staged};

/// A state the image can go back to: the state before a commit, and the ranges of its file
/// the commit overwrote (read from `state` when rolling back).
pub(crate) struct Undo {
    pub state: State,
    pub full: bool,
    pub len: u64,
    pub writes: Vec<(u64, u64)>,
}

/// The effects of a commit or rollback, step by step.
pub(crate) struct Steps {
    effects: VecDeque<Effect>,
    /// How many effects each step has, in order.
    counts: VecDeque<u32>,
    /// The low half of the first effect's tag.
    first: u32,
}

impl Steps {
    /// Adds a step of `effects`; a step of none is left out.
    fn step(&mut self, effects: impl IntoIterator<Item = Effect>) {
        let before = self.effects.len();
        self.effects.extend(effects);
        let count = (self.effects.len() - before) as u32;
        if count != 0 {
            self.counts.push_back(count);
        }
    }
}

/// A commit or rollback in flight.
pub(crate) struct Pending {
    steps: Steps,
    /// The step in flight: its first tag's low half, how many effects, which are done.
    from: u32,
    count: u32,
    done: BitSet,
    left: u32,
    failed: Option<LinkError>,
    /// What the image goes back to if an effect fails before the rename.
    revert: Option<Revert>,
}

/// What a failed commit or rollback puts back.
pub(crate) enum Revert {
    /// A commit: the state before it is the undo it pushed, the history's last.
    Pop,
    /// A rollback: the state it left, and the undo it took (whose state the image now has).
    Push { state: State, full: bool, len: u64, writes: Vec<(u64, u64)> },
    /// The first commit, which pushed nothing: the state before it.
    Keep(State),
}

/// How many commits back a rollback can go.
pub(crate) const HISTORY: usize = 16;

/// The file writes of one commit, and the encoded message for running programs, which only a
/// commit into their code memory gives them.
pub(crate) struct Writes {
    pub full: bool,
    pub len: u64,
    pub old_len: u64,
    pub writes: Vec<(u64, Vec<u8>)>,
    pub message: Option<Vec<u8>>,
}

/// The image's tags, given out one after another: the high half is the output's.
struct Tags<'t> {
    high: u64,
    low: &'t mut u32,
}

impl Tags<'_> {
    fn next(&mut self) -> Tag {
        *self.low = self.low.wrapping_add(1);
        Tag(self.high | u64::from(*self.low))
    }
}

/// A copy of a file name for an effect.
fn owned(name: &FileName) -> FileName {
    // clone: each effect owns its file name (`blaze_core::Effect`), a few per commit
    name.clone()
}

impl Image {
    /// Whether `staged` may be committed now: no commit in flight, staged on the image's
    /// generation.
    pub(crate) fn can_commit(&self, staged: &Staged) -> Result<(), LinkError> {
        if self.pending.is_some() {
            return Err(LinkError { function: None, what: "a commit is in flight".into(), refusal: None });
        }
        if staged.base != self.state.generation {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "staged on generation {}, and the image is at {}: stage it again",
                    staged.base, self.state.generation
                ),
            });
        }
        Ok(())
    }

    /// Makes `state` the image's, the state before it kept in the history with the ranges
    /// of its file the commit overwrites: what a failed file commit puts back.
    pub(crate) fn take(&mut self, state: State, full: bool, undo: Vec<(u64, u64)>) -> Revert {
        self.generations = self.generations.max(state.generation);
        let before = std::mem::replace(&mut self.state, state);
        if before.laid_out {
            let len = before.file_len;
            self.remember(Undo { state: before, full, len, writes: undo });
            Revert::Pop
        } else {
            Revert::Keep(before)
        }
    }

    /// The writes that give back the file before the last commit, and the message saying so.
    pub(crate) fn roll_back_writes(&self) -> Result<Writes, LinkError> {
        if self.pending.is_some() {
            return Err(LinkError { function: None, what: "a commit is in flight".into(), refusal: None });
        }
        let Some(undo) = self.history.back() else {
            return Err(LinkError { function: None, what: "no commit to roll back".into(), refusal: None });
        };
        let to = undo.state.generation;
        let message =
            self.output.live.map(|_| blaze_live::patch::Message::RollBack { id: self.state.lineage, to }.encode());
        Ok(Writes {
            full: undo.full,
            len: undo.len,
            old_len: self.state.file_len,
            writes: file::restore(&undo.state, undo.full, &undo.writes),
            message,
        })
    }

    /// Makes the history's last state the image's: what a failed file rollback puts back.
    pub(crate) fn take_undo(&mut self) -> Revert {
        let Undo { state, full, len, writes } = self.history.pop_back().expect("a rollback's writes were made of it");
        let current = std::mem::replace(&mut self.state, state);
        Revert::Push { state: current, full, len, writes }
    }

    /// Writes a commit's or rollback's file into `memory`'s image part, as the file's effects
    /// leave it: a full write's file starts as zeros, so the gaps between its writes, and what
    /// the last file had past it, are zeroed; a shorter file zeroes what it no longer has.
    /// Each range written is published. Nothing is written when the file doesn't fit.
    pub(crate) fn write_into(&self, writes: &Writes, memory: &mut CodeMemory) -> Result<(), LinkError> {
        let image = memory.image_len();
        let end = writes.writes.iter().map(|(at, bytes)| at + bytes.len() as u64).fold(writes.len, u64::max);
        if end.max(writes.old_len) > image {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "{} is {:#x} bytes and its code memory's image part {image:#x}: make the code memory with \
                     room for the file",
                    self.to,
                    end.max(writes.old_len),
                ),
            });
        }
        let to = &self.to;
        // alloc: error path
        let fail = |error: blaze_inproc::Error| LinkError { function: None, what: format!("writing {to} into its code memory: {error}"), refusal: None };
        let zero = |memory: &mut CodeMemory, offset: u64, len: u64| -> Result<(), LinkError> {
            if len == 0 {
                return Ok(());
            }
            memory.fill(offset, len, 0).map_err(fail)?;
            memory.publish(CodeRange { offset, len }).map_err(fail)
        };
        let mut at = 0;
        for (offset, bytes) in &writes.writes {
            if writes.full && *offset > at {
                zero(memory, at, offset - at)?;
            }
            memory.write(*offset, bytes).map_err(fail)?;
            let len = bytes.len() as u64;
            memory.publish(CodeRange { offset: *offset, len }).map_err(fail)?;
            at = at.max(offset + len);
        }
        if writes.full {
            zero(memory, at, end.max(writes.old_len).saturating_sub(at))
        } else {
            zero(memory, writes.len, writes.old_len.saturating_sub(writes.len))
        }
    }

    /// The effects of a file commit or rollback. One with a message for running programs is
    /// refused before anything changes: the file reaches no running program, only the code
    /// memory does.
    pub(crate) fn steps(&mut self, writes: Writes) -> Result<Steps, LinkError> {
        if writes.message.is_some() {
            return Err(LinkError {
                function: None,
                refusal: None,
                // alloc: error path
                what: format!(
                    "{}: a live image's patches reach running programs only through the code memory: commit it with \
                     `commit_into`, roll it back with `roll_back_into`",
                    self.to
                ),
            });
        }
        let mut tags = Tags { high: self.output.tags, low: &mut self.tags };
        let (to, next) = (&self.to, &self.next);
        let mut steps = Steps {
            effects: VecDeque::with_capacity(writes.writes.len() + 8),
            counts: VecDeque::with_capacity(9),
            first: tags.low.wrapping_add(1),
        };
        steps.step([Effect::Remove { file: owned(next), tag: tags.next() }]);
        if writes.full {
            steps.step([Effect::Truncate { file: owned(next), len: writes.len, tag: tags.next() }]);
        } else {
            steps.step([Effect::Clone { from: owned(to), to: owned(next), tag: tags.next() }]);
            if writes.len != writes.old_len {
                steps.step([Effect::Truncate { file: owned(next), len: writes.len, tag: tags.next() }]);
            }
        }
        steps.step(
            writes.writes.into_iter().map(|(at, bytes)| Effect::WriteAt { file: owned(next), at, bytes, tag: tags.next() }),
        );
        steps.step([Effect::Sync { file: owned(next), tag: tags.next() }]);
        steps.step([Effect::Mode { file: owned(next), mode: 0o755, tag: tags.next() }]);
        steps.step([Effect::Rename { from: owned(next), to: owned(to), tag: tags.next() }]);
        Ok(steps)
    }

    pub(crate) fn begin(&mut self, steps: Steps, revert: Revert, effects: &mut Vec<Effect>) {
        let from = steps.first;
        let most = steps.counts.iter().copied().max().unwrap_or(0) as usize;
        self.pending = Some(Pending {
            steps,
            from,
            count: 0,
            done: BitSet::from_words(Vec::with_capacity(most.div_ceil(64))),
            left: 0,
            failed: None,
            revert: Some(revert),
        });
        self.next_step(effects);
    }

    fn next_step(&mut self, effects: &mut Vec<Effect>) {
        let pending = self.pending.as_mut().expect("a commit in flight");
        let Some(count) = pending.steps.counts.pop_front() else { return };
        pending.from = pending.from.wrapping_add(pending.count);
        pending.count = count;
        pending.left = count;
        pending.done.reset(count as usize);
        effects.extend(pending.steps.effects.drain(..count as usize));
    }

    pub(crate) fn remember(&mut self, undo: Undo) {
        self.history.push_back(undo);
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
    }

    /// Of a tag, its place in the step in flight, if it is one of its effects not yet done.
    fn in_flight(&self, tag: Tag) -> Option<usize> {
        let pending = self.pending.as_ref()?;
        if tag.0 & !0xffff_ffff != self.output.tags {
            return None;
        }
        let at = (tag.0 as u32).wrapping_sub(pending.from);
        if at >= pending.count {
            return None;
        }
        let at = at as usize;
        (!pending.done.contains(at)).then_some(at)
    }

    /// Whether `tag` is one of this image's effects in flight.
    pub fn owns(&self, tag: Tag) -> bool {
        self.in_flight(tag).is_some()
    }

    /// Takes the completion of one of the image's effects ([`Image::owns`]); asks for the
    /// next step's effects. When the commit or rollback is over, its result: the
    /// generation the image is at, or why it failed (the image is then as it was before).
    pub fn complete(&mut self, completion: &Completion, effects: &mut Vec<Effect>) -> Option<Result<u64, LinkError>> {
        let at = self.in_flight(completion.tag)?;
        let pending = self.pending.as_mut().expect("in flight");
        pending.done.insert(at);
        pending.left -= 1;
        match &completion.done {
            Done::Failed(why) => {
                let what = if pending.revert.is_some() {
                    // alloc: error path
                    format!("writing {}: {why}", self.to)
                } else {
                    // alloc: error path
                    format!("{} is renamed into place, but an effect after the rename failed: {why}", self.to)
                };
                pending.failed.get_or_insert(LinkError { function: None, what, refusal: None });
            }
            Done::Renamed => pending.revert = None,
            _ => {}
        }
        if pending.left != 0 {
            return None;
        }
        if let Some(failed) = pending.failed.take() {
            let pending = self.pending.take().expect("in flight");
            match pending.revert {
                Some(Revert::Pop) => {
                    let undo = self.history.pop_back().expect("the commit pushed its undo");
                    self.state = undo.state;
                }
                Some(Revert::Push { state, full, len, writes }) => {
                    let undone = std::mem::replace(&mut self.state, state);
                    self.remember(Undo { state: undone, full, len, writes });
                }
                Some(Revert::Keep(state)) => self.state = state,
                None => {}
            }
            return Some(Err(failed));
        }
        if pending.steps.counts.is_empty() {
            self.pending = None;
            return Some(Ok(self.state.generation));
        }
        self.next_step(effects);
        None
    }
}
