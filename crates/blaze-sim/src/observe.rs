//! Vacuity and power, in observe mode (docs/v2/plan/design/test.md, "End state: the search
//! itself", piece S4): what a search's passing cases never exercised, and how hard a control's
//! plant was to catch, as one block of lines after the test's own output.
//!
//! The worst test is one that passes without exercising anything. [`Config::must_reach`] already
//! fails a search whose passing cases never reached a named point (`Verdict::Unreached`); this
//! module looks at what every passing case DREW, with no point named by the test:
//!
//! - a labelled draw that could give more than one value (its bound is above 0) and gave the same
//!   one value in every one of at least [`LEAST_CASES`] passing cases: "never varied";
//! - with blaze-sim's cargo feature `edge-first`, an edge kind of the edge tables ([`EdgeTable`])
//!   that no passing case produced for a label drawn at least [`LEAST_EDGE_DRAWS`] times;
//! - from `control`, the case that caught each plant, and a plant caught only in the last tenth
//!   of the cases as weak.
//!
//! The [`Tally`] is fed in the search (search.rs, `Search::ran_case`, each passing case of the
//! run) and its lines are printed by the driver (check.rs `say_observed`) once the search is over.
//! Nothing here fails a test or changes a `Verdict`: [`ObserveMode::Observe`] is the only mode
//! built; [`ObserveMode::Enforce`] is refused by name until the harvest (S8) has read the lines.
//!
//! Deterministic: the tally is a function of the passing cases in their order, and the lines are
//! written in label order, so one config and seed give the same block on any machine.
//!
//! [`Config::must_reach`]: crate::search::Config::must_reach

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt::Debug;

use blaze_own::{String, format_into};

use crate::tape::Record;

#[cfg(test)]
pub(crate) use plant::TallyPlant;

/// The labels one tally holds (rule bounded-progress: a table has a cap). Draws of labels past
/// the cap are counted, not tallied, and the block says how many.
pub const LABELS: usize = 64;

/// The passing cases a draw needs before "never varied" is said of it: below this a constant
/// value can be chance.
pub const LEAST_CASES: u32 = 32;

/// The draws of a label before an edge kind it never produced is listed.
pub const LEAST_EDGE_DRAWS: u64 = 256;

/// The edge kinds an [`EdgeTable`] may have: one bit each of a `u64`.
pub const MOST_EDGE_KINDS: u32 = u64::BITS;

/// What the search does with what it observed. A `Config` field (`Config::observe`), so that
/// enforcing can be built later with no change of signature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObserveMode {
    /// Print the block after the test's own output; fail nothing. The default.
    Observe,
    /// Fail a test on its block's findings. Not built yet: a search asked for it panics naming
    /// it (never silently observes) until the harvest has read the observed lines.
    Enforce,
}

impl ObserveMode {
    /// Refuses a mode that is not built: `Enforce` panics, naming itself.
    pub(crate) fn in_force(self) {
        match self {
            ObserveMode::Observe => {}
            ObserveMode::Enforce => panic!(
                "blaze-sim: `ObserveMode::Enforce` is not built yet (test.md, piece S4: observe mode until the harvest has read the observed lines); use `ObserveMode::Observe`"
            ),
        }
    }
}

/// The edge tables of piece S1 as the tally asks them: which edge kinds a draw of bound `most`
/// can produce, which kind a drawn value is, and a kind's name. The tables implement it and
/// set [`EDGES`]; until then [`NoEdges`] has no kinds and the edge lines print nothing.
///
/// To verify by a test, with the tables: a recorded choice's canonical `value` under its `most`
/// is enough to tell its edge kind. When a kind is a choice of its own, `kind_of` is asked of
/// that choice (its label is the tables' to name).
pub trait EdgeTable: Sync {
    /// The kinds a draw of bound `most` can produce: bit `k` for kind `k`, `k` below
    /// [`MOST_EDGE_KINDS`].
    fn kinds(&self, most: u64) -> u64;
    /// The kind `value`, drawn under bound `most`, is, if it is one.
    fn kind_of(&self, most: u64, value: u64) -> Option<u32>;
    /// The name the block gives kind `kind` (`"MAX"`, `"2^31+1"`): one of the kinds `kinds` gave.
    fn kind_name(&self, kind: u32) -> &'static str;
}

/// No edge kinds: what the tally asks until the edge tables exist.
pub struct NoEdges;

impl EdgeTable for NoEdges {
    fn kinds(&self, _most: u64) -> u64 {
        0
    }

    fn kind_of(&self, _most: u64, _value: u64) -> Option<u32> {
        None
    }

    fn kind_name(&self, kind: u32) -> &'static str {
        panic!("blaze-sim: `NoEdges` has no edge kinds, and was asked the name of kind {kind}")
    }
}

/// The edge tables the tally asks, under the cargo feature `edge-first`: the one line the edge
/// writers change to their table.
pub static EDGES: &dyn EdgeTable = &NoEdges;

/// What the passing cases drew under one label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    label: &'static str,
    /// Passing cases that drew it at least once.
    cases: u32,
    /// The case (from 1) that drew it last: a case is counted once however often it draws.
    last_case: u32,
    draws: u64,
    /// The least and greatest canonical value drawn: equal exactly when it never varied, which
    /// is the distinct-value count, capped at two, that "never varied" needs.
    least: u64,
    greatest: u64,
    /// The greatest bound of its draws: above 0 when a draw could give more than one value.
    most: u64,
    /// Edge kinds its draws could produce, and those they did (`edge-first` only).
    edges_possible: u64,
    edges_seen: u64,
}

impl Entry {
    const EMPTY: Entry = Entry {
        label: "",
        cases: 0,
        last_case: 0,
        draws: 0,
        least: u64::MAX,
        greatest: 0,
        most: 0,
        edges_possible: 0,
        edges_seen: 0,
    };
}

/// The per-search tally: for each label of a draw (at most [`LABELS`]), how many passing cases
/// drew it, its least and greatest value and its greatest bound. Fixed size: nothing grows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tally {
    entries: [Entry; LABELS],
    len: usize,
    /// Cases fed so far.
    cases: u32,
    /// Draws whose label found no room.
    past_cap: u64,
    /// The entry the last draw went to: a case draws one label many times in a row.
    last_hit: usize,
    #[cfg(test)]
    plant: TallyPlant,
}

impl Default for Tally {
    fn default() -> Tally {
        Tally::new()
    }
}

impl Tally {
    /// Empty.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new() -> Tally {
        Tally {
            entries: [Entry::EMPTY; LABELS],
            len: 0,
            cases: 0,
            past_cap: 0,
            last_hit: 0,
            #[cfg(test)]
            plant: TallyPlant::None,
        }
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) const fn planted(plant: TallyPlant) -> Tally {
        let mut tally = Tally::new();
        tally.plant = plant;
        tally
    }

    /// The cases fed so far.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn cases(&self) -> u32 {
        self.cases
    }

    /// Feeds one passing case's record: each of its choices under its label.
    #[blaze::nonblocking]
    pub fn case(&mut self, record: &Record) {
        self.cases = self.cases.saturating_add(1);
        let case = self.cases;
        let edges = cfg!(feature = "edge-first");
        for choice in &record.choices {
            let Some(at) = self.slot(choice.label) else {
                self.past_cap = self.past_cap.saturating_add(1);
                continue;
            };
            #[cfg(test)]
            let per_case = self.plant == TallyPlant::PerCase;
            #[cfg(not(test))]
            let per_case = false;
            let entry = &mut self.entries[at];
            if entry.last_case != case {
                entry.cases = entry.cases.saturating_add(1);
                entry.last_case = case;
                if per_case {
                    // The planted bug: the values are counted within this case alone.
                    entry.least = u64::MAX;
                    entry.greatest = 0;
                }
            }
            entry.draws = entry.draws.saturating_add(1);
            entry.least = entry.least.min(choice.value);
            entry.greatest = entry.greatest.max(choice.value);
            entry.most = entry.most.max(choice.most);
            if edges {
                entry.edges_possible |= EDGES.kinds(choice.most);
                if let Some(kind) = EDGES.kind_of(choice.most, choice.value) {
                    entry.edges_seen |= kind_bit(kind);
                }
            }
        }
    }

    /// The entry of `label`, made when there is room; `None` past [`LABELS`] labels.
    fn slot(&mut self, label: &'static str) -> Option<usize> {
        if self.last_hit < self.len && self.entries[self.last_hit].label == label {
            return Some(self.last_hit);
        }
        let found = self.entries[..self.len].iter().position(|entry| entry.label == label);
        let at = match found {
            Some(at) => at,
            None if self.len < LABELS => {
                let at = self.len;
                self.entries[at] = Entry { label, ..Entry::EMPTY };
                self.len += 1;
                at
            }
            None => return None,
        };
        self.last_hit = at;
        Some(at)
    }

    /// The tally's lines, each indented by two spaces and ended by a newline, appended to `out`
    /// in label order: "never varied", then (under `edge-first`) each edge kind never produced,
    /// then the draws past the cap. Nothing when there is nothing to say.
    pub fn lines_into<A: Allocator>(&self, out: &mut String<A>) {
        let mut order: [usize; LABELS] = core::array::from_fn(|at| at);
        let order = &mut order[..self.len];
        order.sort_unstable_by(|a, b| self.entries[*a].label.cmp(self.entries[*b].label));
        for at in order.iter() {
            let entry = &self.entries[*at];
            if entry.most > 0 && entry.cases >= LEAST_CASES && entry.least == entry.greatest {
                format_into!(
                    out,
                    "  `{}` never varied ({} cases, always {})\n",
                    entry.label,
                    entry.cases,
                    entry.least
                );
            }
            if cfg!(feature = "edge-first") && entry.draws >= LEAST_EDGE_DRAWS {
                let missing = entry.edges_possible & !entry.edges_seen;
                for kind in 0..MOST_EDGE_KINDS {
                    if missing & kind_bit(kind) != 0 {
                        format_into!(
                            out,
                            "  `{}` never drew edge {} ({} draws)\n",
                            entry.label,
                            EDGES.kind_name(kind),
                            entry.draws
                        );
                    }
                }
            }
        }
        if self.past_cap > 0 {
            format_into!(
                out,
                "  {} draws of labels past the tally's first {LABELS} labels were not tallied\n",
                self.past_cap
            );
        }
    }
}

/// The bit of edge kind `kind`; a kind past [`MOST_EDGE_KINDS`] is refused by name.
fn kind_bit(kind: u32) -> u64 {
    match 1u64.checked_shl(kind) {
        Some(bit) => bit,
        None => panic!(
            "blaze-sim: edge kind {kind} is past the {MOST_EDGE_KINDS} kinds an `EdgeTable` may have"
        ),
    }
}

/// Where a control's planted run caught its plant: case `case` (from 1) of `cases`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Caught {
    pub case: u32,
    pub cases: u32,
}

impl Caught {
    /// Caught only in the last tenth of the cases: a plant the test barely finds.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn weak(self) -> bool {
        u64::from(self.case) * 10 > u64::from(self.cases) * 9
    }
}

/// A plant's lines, appended to `out`: where it was caught, and when weak, that it was. The
/// plant is named by its `Debug` form, as `control`'s messages name it.
#[expect(
    blaze::error_debug,
    reason = "a plant is a value of the caller's own type, which `control` asks only to be `Debug`: its `Debug` form is the name the lines give it"
)]
pub fn plant_lines_into<A: Allocator>(plant: &dyn Debug, caught: Caught, out: &mut String<A>) {
    format_into!(out, "  plant {plant:?}: caught at case {} of {}\n", caught.case, caught.cases);
    if caught.weak() {
        format_into!(
            out,
            "  weak: plant {plant:?} needed {} of {} cases\n",
            caught.case,
            caught.cases
        );
    }
}

/// The block printed after a test's own output, appended to `out`: `blaze-sim observe: <place>`
/// then `lines`; nothing when there are no lines.
pub fn block<A: Allocator>(place: &str, lines: &str, out: &mut String<A>) {
    if !lines.is_empty() {
        format_into!(out, "blaze-sim observe: {place}\n{lines}");
    }
}

#[cfg(test)]
mod plant {
    /// A bug a control plants in the tally.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum TallyPlant {
        None,
        /// The values are counted per case, not across cases: a label drawn once a case looks
        /// as if it never varied.
        PerCase,
    }
}
