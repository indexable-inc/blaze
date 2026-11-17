//! What one crate's resolution holds in its frame, by the stretch of the solve that took it
//! (rule 16: anything large is a P0 until the profile says what held it). The solve marks its
//! stretches through [`Par`] ([`Stage`], and inside an expansion [`ExpandPart`]); [`Metered`]
//! is the serial `Par` that, at each mark, charges the bytes the frame grew by since the last
//! mark (and the bytes growth abandoned in the bump: a `Vec` or table that grew and left its old
//! block behind) to the stretch that just ended. The profile prints the breakdown of the
//! largest crate scope frame of the command (phases.rs, the `scope frame by stretch` row).
//!
//! Nothing is allocated to count: fixed tables of `Cell`s, the crate's name a [`Label`].

use core::cell::Cell;
use core::fmt;

use blaze_own::AVec;
use blaze_scope::task::{ExpandPart, Par, Stage};
use bstd::mem::Frame;

use crate::phases::byte_rows::Label;

/// The stretches outside the solve that [`super::resolve`] marks itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outside {
    /// The job's environment and texts copied into the frame, before the solve.
    Before,
    /// The crate's record written (its sections) and its read lists, after the solve.
    Record,
}

const STAGES: usize = Stage::ALL.len();
const PARTS: usize = ExpandPart::ALL.len();
const OUTSIDE: usize = 2;
const SLOTS: usize = STAGES + PARTS + OUTSIDE;

/// One stretch's bytes: grown, abandoned by growth, and how many times it was entered.
#[derive(Clone, Copy, Default, Debug)]
struct Slot {
    grown: u64,
    abandoned: u64,
    entries: u64,
}

/// A serial [`Par`] over `frame` that charges each stretch the frame's growth across it.
pub(crate) struct Metered<'f> {
    frame: &'f Frame,
    /// The slot being charged now.
    at: Cell<usize>,
    /// The frame's used bytes and abandoned bytes at the last mark.
    used: Cell<usize>,
    gone: Cell<u64>,
    slots: [Cell<Slot>; SLOTS],
}

impl<'f> Metered<'f> {
    /// Charging starts now, to [`Outside::Before`].
    pub(crate) fn new(frame: &'f Frame) -> Metered<'f> {
        let metered = Metered {
            frame,
            at: Cell::new(STAGES + PARTS),
            used: Cell::new(frame.used()),
            gone: Cell::new(frame.abandoned().bytes),
            slots: [const { Cell::new(Slot { grown: 0, abandoned: 0, entries: 0 }) }; SLOTS],
        };
        metered.enter(STAGES + PARTS);
        metered
    }

    /// The growth since the last mark charged to the slot being charged; `slot` charged next.
    fn enter(&self, slot: usize) {
        self.charge();
        self.at.set(slot);
        let now = &self.slots[slot];
        let mut entered = now.get();
        entered.entries += 1;
        now.set(entered);
    }

    /// The growth since the last mark charged to the slot being charged.
    fn charge(&self) {
        let (used, gone) = (self.frame.used(), self.frame.abandoned().bytes);
        let was = &self.slots[self.at.get()];
        let mut charged = was.get();
        // A rollback inside a stretch lowers `used`: it grew by nothing then.
        charged.grown += used.saturating_sub(self.used.get()) as u64;
        charged.abandoned += gone.saturating_sub(self.gone.get());
        was.set(charged);
        self.used.set(used);
        self.gone.set(gone);
    }

    /// [`super::resolve`]'s own stretch `outside` starts now.
    pub(crate) fn outside(&self, outside: Outside) {
        self.enter(STAGES + PARTS + outside as usize);
    }

    /// The breakdown, the last stretch charged; `of` names the crate.
    pub(crate) fn finish(&self, of: Label) -> ScopeBytes {
        self.charge();
        let mut slots = [Slot::default(); SLOTS];
        for (slot, cell) in slots.iter_mut().zip(self.slots.iter()) {
            *slot = cell.get();
        }
        ScopeBytes { slots, peak: self.frame.peak() as u64, abandoned: self.frame.abandoned().bytes, of }
    }
}

impl Par for Metered<'_> {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        for at in 0..n {
            out.push(f(at));
        }
    }

    fn stage(&self, stage: Stage) {
        self.enter(stage as usize);
    }

    fn expand_part(&self, part: ExpandPart, _macro_name: &str, _size: usize) {
        self.enter(STAGES + part as usize);
    }
}

/// One crate scope frame's bytes by stretch ([`Metered`]), its peak and its crate.
#[derive(Clone, Copy)]
pub(crate) struct ScopeBytes {
    slots: [Slot; SLOTS],
    peak: u64,
    abandoned: u64,
    of: Label,
}

impl Default for ScopeBytes {
    fn default() -> ScopeBytes {
        ScopeBytes { slots: [Slot::default(); SLOTS], peak: 0, abandoned: 0, of: Label::of(format_args!("")) }
    }
}

impl ScopeBytes {
    /// The frame's peak.
    pub(crate) fn peak(&self) -> u64 {
        self.peak
    }
}

impl fmt::Debug for ScopeBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ScopeBytes {{ of: {}, peak: {} }}", self.of.as_str(), self.peak)
    }
}

/// The name of slot `i`.
fn slot_name(i: usize) -> &'static str {
    const STAGE_NAMES: [&str; STAGES] = [
        "Try", "Files", "Lower", "Decls", "Solve", "Expand", "Publish", "Write", "ReadList", "NamesRecord", "ItemsRecord", "ExpansionsRecord", "Decode", "Done",
    ];
    const PART_NAMES: [&str; PARTS] = [
        "expand/Find",
        "expand/Kept",
        "expand/Define",
        "expand/LexInput",
        "expand/Shape",
        "expand/RunRules",
        "expand/RunBuiltin",
        "expand/RunDerive",
        "expand/RunProc",
        "expand/Keep",
        "expand/Render",
        "expand/Outline",
        "expand/Lower",
        "expand/Modules",
        "expand/Tables",
        "expand/Blocks",
        "expand/Store",
        "expand/End",
    ];
    const OUTSIDE_NAMES: [&str; OUTSIDE] = ["before the solve", "the record written"];
    match i {
        i if i < STAGES => STAGE_NAMES[i],
        i if i < STAGES + PARTS => PART_NAMES[i - STAGES],
        i => OUTSIDE_NAMES[i - STAGES - PARTS],
    }
}

/// The profile's row, one line: `scope frame by stretch (the largest: <crate>; peak <n>;
/// abandoned by growth <n>) <stretch> <grown> <abandoned> <entries>x · ...`, the stretches that
/// grew the frame, largest first. Nothing when no crate scope ran on the pool.
impl fmt::Display for ScopeBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.peak == 0 {
            return Ok(());
        }
        write!(f, "scope frame by stretch (the largest: {}; peak {}; abandoned by growth {}) (grown, abandoned, entries):", self.of.as_str(), self.peak, self.abandoned)?;
        let mut order = [0usize; SLOTS];
        for (i, at) in order.iter_mut().enumerate() {
            *at = i;
        }
        order.sort_unstable_by_key(|i| core::cmp::Reverse(self.slots[*i].grown));
        for i in order {
            let slot = self.slots[i];
            if slot.grown == 0 && slot.abandoned == 0 {
                continue;
            }
            write!(f, " · {} {} {} {}x", slot_name(i), slot.grown, slot.abandoned, slot.entries)?;
        }
        writeln!(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bstd::mem::Tag;

    /// Each stretch is charged what the frame grew while it was the one marked, and the row
    /// names the largest first. Prints `metered: Solve 4096, expand/RunRules 1024`.
    #[test]
    fn each_stretch_is_charged_its_own_growth() {
        let frame = Frame::with_reserve(1 << 20, Tag::Other).expect("a test frame");
        let metered = Metered::new(&frame);
        metered.stage(Stage::Solve);
        let _solve = frame.alloc_slice_copy(&[0u8; 4096]);
        metered.stage(Stage::Expand);
        metered.expand_part(ExpandPart::RunRules, "m", 0);
        let _rules = frame.alloc_slice_copy(&[0u8; 1024]);
        metered.expand_part(ExpandPart::End, "m", 0);
        metered.stage(Stage::Solve);
        let bytes = metered.finish(Label::of(format_args!("core")));
        let solve = bytes.slots[Stage::Solve as usize];
        let rules = bytes.slots[STAGES + ExpandPart::RunRules as usize];
        assert!(solve.grown >= 4096 && solve.grown < 4096 + 64, "Solve was charged {}", solve.grown);
        assert!(rules.grown >= 1024 && rules.grown < 1024 + 64, "RunRules was charged {}", rules.grown);
        assert_eq!(solve.entries, 2, "Solve was entered twice");
        let row = std::format!("{bytes}");
        assert!(row.starts_with("scope frame by stretch (the largest: core;"), "{row}");
        let at_solve = row.find(" · Solve ").expect("a Solve column");
        let at_rules = row.find(" · expand/RunRules ").expect("a RunRules column");
        assert!(at_solve < at_rules, "the largest stretch is first: {row}");
        std::println!("metered: Solve {}, expand/RunRules {}", solve.grown, rules.grown);
    }
}
