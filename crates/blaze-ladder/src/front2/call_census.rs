//! THE CALL CENSUS on real code (end-state.md D190 step 2): each body the engine checks,
//! checked again under BOTH call rules (blaze-flow-lower call_census.rs), its calls compared
//! site by site, its findings counted under each rule. Production is untouched: `blaze check`
//! answers with its own `check_borrows`; this runs beside it only when a census is asked
//! (tables/task.rs `BodyFrame::census`), and its answer is a count, never a verdict.
//!
//! Per body ([`ProjectWorld::call_census`]): the type check (`check_then`, as `check_borrows`),
//! the facts gathered as `borrows_of` gathers them, then `borrows_of`'s walk twice, once per
//! rule: each closure and nested async block innermost first, each filing its requirements,
//! then the owner's flows; each flow lowered LOGGED under the rule (every call's record kept,
//! a call refusal held and the walk going on) and checked by `file_flow`, the coarse rule
//! applied. The two runs' logs are compared flow by flow ([`compare_calls`]).
//!
//! What is counted ([`CallCensusTotals`], fixed tables summed in any order: D179 GATHERED):
//! - bodies: checked, not checked and why, those where the one rule related every call, those
//!   checked whole under each rule;
//! - calls: by class ((a), (b), (c), hub, same, both refuse, unpaired) and callee kind; the one
//!   rule's refusals by `OneRuleWhy` and callee kind; `PositionsNotSlots` by node;
//! - findings by code under each rule: on every body whole under that rule, and on the bodies
//!   whole under both where the one rule related every call (the reviewer's "fully related");
//! - every (b) call outside the hub: owner path, item-relative range, callee path, sorted and
//!   capped at [`MOST_FEWER`].
//!
//! Rule 3: nothing here is a fallback; a body the census cannot read is counted by why.

use core::fmt::Write as _;

use blaze_flow_lower::call_census::{
    CallClass, CallLabel, CallLog, CallRow, CallTally, CensusRule, WhyKind, compare_calls, lower_async_logged, lower_closure_logged, lower_owner_logged,
};
use blaze_flow_lower::{Form, OneRuleWhy, Refusal, Requirement, Uncounted};
use blaze_own::{AVec, Alloc, Box, SharedAlloc, Vec};
use blaze_typeck as tc;

use super::body::Body;
use super::borrows::{BorrowLangs, BorrowRefused, BorrowVerdict, FactsPlant, ReadBy, file_flow, gather_facts_planted, reads_regions};
use super::borrows_nested::{FlowOf, NestedFacts, Requirements, nested_inner_first, nested_range, next_nested};
use super::check::Records;
use super::project::{BodyRef, ProjectWorld};
use super::project_records::BodySource;
use super::source::NamesSource;
use super::stalled_goals::Stalls;
use super::types::{Declared, declared_lifetimes};

/// The finding codes counted, in print order; the last counts every other code.
pub const CODES: [&str; 16] =
    ["E0382", "E0505", "E0499", "E0502", "E0503", "E0506", "E0507", "E0508", "E0509", "E0515", "E0597", "E0521", "E0310", "outlives", "E0106", "other"];

/// The most (b) rows kept; the count of all of them is kept apart.
pub const MOST_FEWER: usize = 200;

/// The longest path text a (b) row keeps (cut, not refused: the row's key is its range too).
pub const MOST_PATH: usize = 96;

fn code_index(code: &str) -> usize {
    CODES.iter().position(|c| *c == code).unwrap_or(CODES.len() - 1)
}

/// A path, cut to [`MOST_PATH`] bytes at a char boundary. Ordered by its text: the bytes come
/// first and the unused tail is zeros, so a prefix orders before what extends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Shown {
    bytes: [u8; MOST_PATH],
    len: u8,
}

impl Shown {
    pub const EMPTY: Shown = Shown { bytes: [0; MOST_PATH], len: 0 };

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("<not utf-8>")
    }
}

impl core::fmt::Write for Shown {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for ch in s.chars() {
            let mut b = [0u8; 4];
            let enc = ch.encode_utf8(&mut b).as_bytes();
            let at = self.len as usize;
            if at + enc.len() > MOST_PATH {
                return Ok(());
            }
            self.bytes[at..at + enc.len()].copy_from_slice(enc);
            self.len += enc.len() as u8;
        }
        Ok(())
    }
}

/// One (b) call: both rules related it, the one rule's pairs miss some of the old's, and the
/// old paths did not go through the hub. Sorted by owner, range, callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FewerRow {
    /// `module::item` of the body (its module path is where its file is).
    pub owner: Shown,
    /// The call's range relative to the body's item text.
    pub start: u32,
    pub end: u32,
    /// `module::item` of the callee, or its key in hex when no path is known.
    pub callee: Shown,
    /// The first old pair the one rule does not derive, and how many it lacks (flow-lower
    /// call_census.rs `CallRow::missing`): what says whether the old path over-relates.
    pub missing: Option<(CallLabel, CallLabel)>,
    pub missing_count: u32,
}

/// The most (a) rows kept; the count of all of them is kept apart.
pub const MOST_ACCEPTS: usize = 64;

/// One (a) call: the old paths refused it and the one rule related it, with the form the old
/// refused by. A one rule more capable is the expected direction; the form says whether the
/// old refusal was a gap of the old paths (a head not lined up, a hidden output) or a rule the
/// one rule lacks (then the one rule is looser there: a bug). Sorted by owner, range, callee.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AcceptRow {
    pub owner: Shown,
    pub start: u32,
    pub end: u32,
    pub callee: Shown,
    pub old: Option<blaze_flow_lower::Form>,
}

/// `row` into the sorted `rows`, the largest dropped past `N`: the kept set is the smallest rows
/// whatever the order of filing. A row already there is not kept twice. The list grows by need
/// in its own allocator and never holds more than `N`.
fn file_sorted<T: Ord, const N: usize>(rows: &mut Vec<T, CensusAlloc>, row: T) {
    let at = rows.partition_point(|r| *r < row);
    if (at < rows.len() && rows[at] == row) || at >= N {
        return;
    }
    if rows.len() >= N {
        rows.truncate(N - 1);
    }
    rows.insert_shift(at, row);
}

/// What the census's lists live in: the totals cross threads (blaze-cli's `RunOut` goes to a
/// worker) and outlive the stretch that filled them, so they are made in the process heap's
/// shared handle, handed in by the caller ([`CallCensusTotals::new`], [`CensusCell::new`]).
pub type CensusAlloc = SharedAlloc<'static>;

/// Why a body was not compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NotCompared {
    /// Its types were not checked clean (`check_then` never reached the borrow step, or did with
    /// diagnostics).
    Untyped,
    /// An ADT fact the lowering needs was not known (`BorrowRefused::FactUnknown`).
    FactUnknown,
    /// The body's own type table was not read (`BorrowRefused::TypeTable`).
    TypeTable,
}

impl NotCompared {
    pub const ALL: [NotCompared; 3] = [NotCompared::Untyped, NotCompared::FactUnknown, NotCompared::TypeTable];

    pub const fn name(self) -> &'static str {
        match self {
            NotCompared::Untyped => "types not checked clean",
            NotCompared::FactUnknown => "an ADT fact unknown",
            NotCompared::TypeTable => "the body's type table unread",
        }
    }
}

/// Why a `Count` refusal's alias or trait had no count in the body's facts, asked again of the
/// ONE read the gather answers by (types.rs `declared_lifetimes`): which half of that read is
/// silent, or whether it answers now (then the gather did not file the pair: its list missed
/// it). A diagnosis of the census only: nothing is related by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Unfiled {
    /// The trait's checker key begins no library item (`NamesSource::key_beginning`).
    NoLibraryKey,
    /// The names do not count the trait's own lifetimes.
    TraitUncounted,
    /// The names count the trait but not the associated type's own (a GAT's) lifetimes.
    AssocUncounted,
    /// The read answers: the gather's list lacked the pair.
    NotGathered,
}

impl Unfiled {
    pub const ALL: [Unfiled; 4] = [Unfiled::NoLibraryKey, Unfiled::TraitUncounted, Unfiled::AssocUncounted, Unfiled::NotGathered];

    pub const fn name(self) -> &'static str {
        match self {
            Unfiled::NoLibraryKey => "the trait's key begins no library item",
            Unfiled::TraitUncounted => "the names do not count the trait",
            Unfiled::AssocUncounted => "the names do not count the associated type",
            Unfiled::NotGathered => "the read answers: the gather did not file it",
        }
    }
}

/// The most traits named by an [`Unfiled`] table: the smallest paths kept, each with its exact
/// count (a trait kept in the sum is among the smallest in every part that saw it).
pub const MOST_UNFILED_NAMED: usize = 24;

/// Findings by code under one rule.
pub type ByCode = [u64; CODES.len()];

/// The census's counts: every field a sum, [`CallCensusTotals::add`] commutative.
///
/// A small value that moves freely (rule 54: at most 1 KiB, asserted below the type). Its own
/// bytes: the allocator (8), eleven counts (88), `not_compared` (24), the call tally's handle
/// (16), the four [`ByCode`] tables (128 each, 512), the [`Unfiled`] table (64) and three
/// lists' handles (32 each, 96): 808. What is large is in [`CensusAlloc`], allocated by need:
/// the call tally ([`CallTally`], 2,271 counts, 18,168 bytes, made at the first call filed) and
/// the kept (b), (a) and named-trait rows (each list grown by need, never past its cap). Before,
/// all of it was inline (the tally, 200 (b) rows of two 97-byte paths each, 64 (a) rows, 24
/// named rows: over 60 KiB), and a test holding some thirty totals as locals and temporaries
/// overflowed its thread's 2 MiB stack at its first instruction (wave-1119 t68,
/// `the_call_census_on_a_fixture`: the fault at the test fn's entry, no recursion).
#[derive(Debug)]
pub struct CallCensusTotals {
    /// Where the tally and the lists grow (the caller's; [`CensusAlloc`]).
    alloc: CensusAlloc,
    /// Bodies the census was asked about.
    pub bodies: u64,
    pub not_compared: [u64; NotCompared::ALL.len()],
    /// Bodies compared (typed, facts known).
    pub compared: u64,
    /// Compared bodies where every call the one rule's walk met was related (zero calls too).
    pub one_related_all: u64,
    /// Compared bodies checked to their end (`refused` none) under each rule.
    pub whole_old: u64,
    pub whole_one: u64,
    /// Compared bodies whose one-rule walk stopped on a call-rule refusal (`Form::OneRule`).
    pub one_lost_to_call: u64,
    /// A comparison that passed its reach bound (rule 18): that flow's rows are not counted.
    pub reach_bound: u64,
    /// The census's own type check left a seed error (tables/task.rs counts it, then drops it:
    /// the body's verdict is `check_borrows`', not this one).
    pub seed_errors: u64,
    /// The calls counted ([`CallCensusTotals::calls`]); `None` until a call is filed.
    calls: Option<Box<CallTally, CensusAlloc>>,
    /// Findings under each rule on every body whole under that rule.
    pub findings_old: ByCode,
    pub findings_one: ByCode,
    /// Findings under each rule on the bodies whole under both where the one rule related every
    /// call (the gate: the old's here must equal the one's, but for the hub's).
    pub related_bodies: u64,
    pub related_old: ByCode,
    pub related_one: ByCode,
    /// Every (b) row counted; the first [`MOST_FEWER`] by order kept ([`CallCensusTotals::fewer`]).
    pub fewer_total: u64,
    fewer: Vec<FewerRow, CensusAlloc>,
    /// Every (a) row counted; the first [`MOST_ACCEPTS`] by order kept ([`CallCensusTotals::accepts`]).
    pub accepts_total: u64,
    accepts: Vec<AcceptRow, CensusAlloc>,
    /// `Count` refusals for an alias's lifetimes (`[0]`) and for a trait's (`[1]`), by
    /// [`Unfiled`]; and the traits named, `(alias?, why, trait path, count)`, the smallest
    /// [`MOST_UNFILED_NAMED`] kept ([`CallCensusTotals::unfiled_named`]).
    pub unfiled: [[u64; Unfiled::ALL.len()]; 2],
    unfiled_named: Vec<(bool, Unfiled, Shown, u64), CensusAlloc>,
}

// Rule 54: the totals stay a small value (the type's doc counts its bytes); a table or a list
// put back inline fails the build here.
const _: () = assert!(core::mem::size_of::<CallCensusTotals>() <= 1024);

/// The tally of a census that filed no call: every count zero.
static NO_CALLS: CallTally = CallTally::new();

/// Two totals are equal when every count and every kept row is: a tally never made equals an
/// all-zero one, and the allocator is not compared.
impl PartialEq for CallCensusTotals {
    fn eq(&self, other: &CallCensusTotals) -> bool {
        self.bodies == other.bodies
            && self.not_compared == other.not_compared
            && self.compared == other.compared
            && self.one_related_all == other.one_related_all
            && self.whole_old == other.whole_old
            && self.whole_one == other.whole_one
            && self.one_lost_to_call == other.one_lost_to_call
            && self.reach_bound == other.reach_bound
            && self.seed_errors == other.seed_errors
            && self.calls() == other.calls()
            && self.findings_old == other.findings_old
            && self.findings_one == other.findings_one
            && self.related_bodies == other.related_bodies
            && self.related_old == other.related_old
            && self.related_one == other.related_one
            && self.fewer_total == other.fewer_total
            && self.fewer() == other.fewer()
            && self.accepts_total == other.accepts_total
            && self.accepts() == other.accepts()
            && self.unfiled == other.unfiled
            && self.unfiled_named() == other.unfiled_named()
    }
}

impl Eq for CallCensusTotals {}

impl CallCensusTotals {
    /// Empty totals whose tally and lists will grow in `alloc`; nothing is allocated until a
    /// call or a row is filed.
    pub const fn new(alloc: CensusAlloc) -> CallCensusTotals {
        CallCensusTotals {
            alloc,
            bodies: 0,
            not_compared: [0; NotCompared::ALL.len()],
            compared: 0,
            one_related_all: 0,
            whole_old: 0,
            whole_one: 0,
            one_lost_to_call: 0,
            reach_bound: 0,
            seed_errors: 0,
            calls: None,
            findings_old: [0; CODES.len()],
            findings_one: [0; CODES.len()],
            related_bodies: 0,
            related_old: [0; CODES.len()],
            related_one: [0; CODES.len()],
            fewer_total: 0,
            fewer: Vec::new_in(alloc),
            accepts_total: 0,
            accepts: Vec::new_in(alloc),
            unfiled: [[0; Unfiled::ALL.len()]; 2],
            unfiled_named: Vec::new_in(alloc),
        }
    }

    /// The calls counted (all zero when none was filed).
    pub fn calls(&self) -> &CallTally {
        self.calls.as_deref().unwrap_or(&NO_CALLS)
    }

    /// The tally to file into, made in the totals' allocator at the first call.
    fn calls_mut(&mut self) -> &mut CallTally {
        let alloc = self.alloc;
        self.calls.get_or_insert_with(|| Box::new_in(CallTally::new(), alloc))
    }

    /// The kept (b) rows, sorted, at most [`MOST_FEWER`].
    pub fn fewer(&self) -> &[FewerRow] {
        &self.fewer
    }

    /// The kept (a) rows, sorted, at most [`MOST_ACCEPTS`].
    pub fn accepts(&self) -> &[AcceptRow] {
        &self.accepts
    }

    /// The named traits of the [`Unfiled`] table, `(alias?, why, trait path, count)`, path order,
    /// at most [`MOST_UNFILED_NAMED`].
    pub fn unfiled_named(&self) -> &[(bool, Unfiled, Shown, u64)] {
        &self.unfiled_named
    }

    /// `n` `Count` refusals of one trait (`alias`: for an alias of it) filed under `why`.
    pub fn file_unfiled(&mut self, alias: bool, why: Unfiled, trait_: Shown, n: u64) {
        let c = &mut self.unfiled[usize::from(!alias)][why as usize];
        *c = c.saturating_add(n);
        let key = (alias, why, trait_);
        let at = self.unfiled_named.partition_point(|r| (r.0, r.1, r.2) < key);
        if let Some(row) = self.unfiled_named.get_mut(at)
            && (row.0, row.1, row.2) == key
        {
            row.3 = row.3.saturating_add(n);
            return;
        }
        if at >= MOST_UNFILED_NAMED {
            return;
        }
        if self.unfiled_named.len() >= MOST_UNFILED_NAMED {
            self.unfiled_named.truncate(MOST_UNFILED_NAMED - 1);
        }
        self.unfiled_named.insert_shift(at, (alias, why, trait_, n));
    }

    /// A (b) row filed: kept sorted, the largest dropped past [`MOST_FEWER`], so the kept set is
    /// the smallest rows whatever the order of filing.
    pub fn file_fewer(&mut self, row: FewerRow) {
        self.fewer_total = self.fewer_total.saturating_add(1);
        file_sorted::<_, MOST_FEWER>(&mut self.fewer, row);
    }

    /// An (a) row filed, as [`CallCensusTotals::file_fewer`] files a (b) row.
    pub fn file_accepts(&mut self, row: AcceptRow) {
        self.accepts_total = self.accepts_total.saturating_add(1);
        file_sorted::<_, MOST_ACCEPTS>(&mut self.accepts, row);
    }

    pub fn add(&mut self, other: &CallCensusTotals) {
        fn sum(a: &mut u64, b: u64) {
            *a = a.saturating_add(b);
        }
        fn sums(a: &mut [u64], b: &[u64]) {
            for (x, y) in a.iter_mut().zip(b.iter()) {
                sum(x, *y);
            }
        }
        sum(&mut self.bodies, other.bodies);
        sums(&mut self.not_compared, &other.not_compared);
        sum(&mut self.compared, other.compared);
        sum(&mut self.one_related_all, other.one_related_all);
        sum(&mut self.whole_old, other.whole_old);
        sum(&mut self.whole_one, other.whole_one);
        sum(&mut self.one_lost_to_call, other.one_lost_to_call);
        sum(&mut self.reach_bound, other.reach_bound);
        sum(&mut self.seed_errors, other.seed_errors);
        if let Some(calls) = other.calls.as_deref() {
            self.calls_mut().add(calls);
        }
        sums(&mut self.findings_old, &other.findings_old);
        sums(&mut self.findings_one, &other.findings_one);
        sum(&mut self.related_bodies, other.related_bodies);
        sums(&mut self.related_old, &other.related_old);
        sums(&mut self.related_one, &other.related_one);
        // The other's rows re-filed: `file_fewer` counts each, so its total is added less them.
        let total = self.fewer_total.saturating_add(other.fewer_total);
        for row in other.fewer.iter() {
            self.file_fewer(*row);
        }
        self.fewer_total = total;
        let total = self.accepts_total.saturating_add(other.accepts_total);
        for row in other.accepts.iter() {
            self.file_accepts(*row);
        }
        self.accepts_total = total;
        // The other's named rows re-filed (their counts into `unfiled` too), then its class
        // counts added less what the rows carried, so `unfiled` is the sum of both.
        let before = self.unfiled;
        for &(alias, why, name, n) in other.unfiled_named.iter() {
            self.file_unfiled(alias, why, name, n);
        }
        for (s, (b, o)) in self.unfiled.iter_mut().zip(before.iter().zip(other.unfiled.iter())) {
            for (x, (y, z)) in s.iter_mut().zip(b.iter().zip(o.iter())) {
                *x = y.saturating_add(*z);
            }
        }
    }

    /// The report: every table, then the (b) list, then the summary block (the lines to paste).
    pub fn write(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        out.write_str("== call census (D190 step 2): old paths vs the one rule ==\n")?;
        writeln!(out, "bodies asked {}; compared {}", self.bodies, self.compared)?;
        for (i, n) in NotCompared::ALL.iter().enumerate() {
            writeln!(out, "  not compared: {} {}", self.not_compared[i], n.name())?;
        }
        writeln!(out, "one rule related every call: {} of {} compared", self.one_related_all, self.compared)?;
        writeln!(out, "checked whole: old {}, one {}; one rule's walk stopped by a call refusal: {}", self.whole_old, self.whole_one, self.one_lost_to_call)?;
        writeln!(out, "comparisons past the reach bound: {}; census seed errors: {}", self.reach_bound, self.seed_errors)?;
        self.calls().write(out)?;
        for (i, what) in ["an alias's", "a trait's"].iter().enumerate() {
            write!(out, "Count for {what} lifetimes, why not filed:")?;
            for w in Unfiled::ALL.iter() {
                write!(out, " {} {};", w.name(), self.unfiled[i][*w as usize])?;
            }
            out.write_str("\n")?;
        }
        writeln!(out, "  traits named (smallest {} of them, path order):", self.unfiled_named.len())?;
        for &(alias, why, name, n) in self.unfiled_named.iter() {
            writeln!(out, "    {n:>8} {} {}: {}", if alias { "alias of" } else { "trait" }, name.as_str(), why.name())?;
        }
        out.write_str("findings by code on bodies whole under the rule (old / one):\n")?;
        Self::codes(&self.findings_old, &self.findings_one, out)?;
        writeln!(out, "findings by code on {} fully-related bodies (whole under both, every call related) (old / one):", self.related_bodies)?;
        Self::codes(&self.related_old, &self.related_one, out)?;
        writeln!(out, "(b) outside hub calls: {} (listed {}, sorted, at most {MOST_FEWER}):", self.fewer_total, self.fewer.len())?;
        for row in self.fewer.iter() {
            writeln!(
                out,
                "  (b) {} @{}..{} calls {}: lacks {} pair(s), first {:?}",
                row.owner.as_str(),
                row.start,
                row.end,
                row.callee.as_str(),
                row.missing_count,
                row.missing
            )?;
        }
        writeln!(out, "(a) one rule accepts, old refused: {} (listed {}, sorted, at most {MOST_ACCEPTS}):", self.accepts_total, self.accepts.len())?;
        for row in self.accepts.iter() {
            writeln!(out, "  (a) {} @{}..{} calls {}: old refused by {:?}", row.owner.as_str(), row.start, row.end, row.callee.as_str(), row.old)?;
        }
        self.summary(out)
    }

    fn codes(old: &ByCode, one: &ByCode, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        let (mut a, mut b) = (0u64, 0u64);
        for (i, code) in CODES.iter().enumerate() {
            if old[i] > 0 || one[i] > 0 {
                writeln!(out, "  {code:>9} {:>7} / {:>7}", old[i], one[i])?;
            }
            a = a.saturating_add(old[i]);
            b = b.saturating_add(one[i]);
        }
        writeln!(out, "  {:>9} {a:>7} / {b:>7}", "total")
    }

    /// The pasteable block: one line per gate quantity.
    pub fn summary(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        let total = |c: &ByCode| c.iter().fold(0u64, |s, x| s.saturating_add(*x));
        let calls = self.calls();
        out.write_str("---- call census summary ----\n")?;
        writeln!(out, "bodies compared {} of {}; fully related {}; whole old {} one {}", self.compared, self.bodies, self.one_related_all, self.whole_old, self.whole_one)?;
        writeln!(
            out,
            "calls {}: same {} hub {} (b) {} (a) {} (c) {} both-refuse {} unpaired {}",
            calls.calls(),
            calls.class_total(CallClass::Same),
            calls.class_total(CallClass::Hub),
            calls.class_total(CallClass::Fewer),
            calls.class_total(CallClass::OneAccepts),
            calls.class_total(CallClass::OneRefuses),
            calls.class_total(CallClass::BothRefuse),
            calls.class_total(CallClass::Unpaired),
        )?;
        let mut order = WhyKind::ALL;
        order.sort_unstable_by(|a, b| calls.why_total(*b).cmp(&calls.why_total(*a)).then(a.index().cmp(&b.index())));
        out.write_str("top refusals:")?;
        for w in order.iter().take(5) {
            if calls.why_total(*w) > 0 {
                write!(out, " {} {};", w.name(), calls.why_total(*w))?;
            }
        }
        out.write_str("\n")?;
        writeln!(out, "findings whole: old {} one {}; on fully related: old {} one {}", total(&self.findings_old), total(&self.findings_one), total(&self.related_old), total(&self.related_one))?;
        writeln!(out, "(b) outside hub: {}; reach bound {}; seed errors {}", self.fewer_total, self.reach_bound, self.seed_errors)?;
        out.write_str("---- end ----\n")
    }
}

/// The census of one flow-run of a body under one rule.
struct Run<'a> {
    verdict: BorrowVerdict<'a>,
    logs: AVec<'a, (FlowOf, CallLog<Alloc<'a>>)>,
    /// The lowering refusal that stopped the walk, if one did.
    stopped: Option<Refusal>,
}

/// `borrows_of`'s walk (front2 borrows.rs, after the facts) under `rule`, every flow logged.
/// Kept in step with it: the nested flows innermost first with their requirements, then the
/// owner's flows, then the coarse rule; only the lowering entries differ (logged, the rule
/// asked) and a lowering refusal is kept as `Lower` (the census never names it further).
fn run_rule<'a>(rule: CensusRule, view: &tc::BodyView<'_>, done: &tc::Checked<'_>, lowering: ReadBy<'_, '_>, alloc: Alloc<'a>) -> Run<'a> {
    let mut verdict = BorrowVerdict { findings: AVec::new_in(alloc), refused: None, within: None };
    let mut logs: AVec<'a, (FlowOf, CallLog<Alloc<'a>>)> = AVec::new_in(alloc);
    let mut stopped = None;
    let mut requirements = Requirements::new_in(alloc);
    let mut needs: AVec<'a, Requirement> = AVec::new_in(alloc);
    let mut coarse: Option<(Refusal, FlowOf)> = None;
    for &of in nested_inner_first(view, alloc).iter() {
        let (lowered, log) = {
            let nested = NestedFacts::with_requirements(lowering, &requirements);
            match of {
                FlowOf::Closure(closure) => {
                    let (r, log) = lower_closure_logged(rule, view, done, closure, &nested, alloc);
                    (r.map(|cf| (cf.lowered.flow, cf.lowered.params, cf.externals, cf.lowered.coarse)), log)
                }
                FlowOf::AsyncBlock(block) => {
                    let (r, log) = lower_async_logged(rule, view, done, block, &nested, alloc);
                    (r.map(|la| (la.flow, AVec::new_in(alloc), la.externals, la.coarse)), log)
                }
                FlowOf::Owner => continue,
            }
        };
        logs.push((of, log));
        let (flow, params, externals, flow_coarse) = match lowered {
            Ok(x) => x,
            Err(refusal) => {
                stopped = Some(refusal);
                verdict.refused = Some(BorrowRefused::Lower(refusal));
                verdict.within = nested_range(view, of);
                break;
            }
        };
        if coarse.is_none() {
            coarse = flow_coarse.map(|c| (c, of));
        }
        needs.clear();
        if file_flow(&flow, &params, of, view, Some(&externals[..]), &mut needs, &mut verdict, alloc) {
            break;
        }
        let made = match of {
            FlowOf::Closure(e) | FlowOf::AsyncBlock(e) => e,
            FlowOf::Owner => continue,
        };
        for &n in needs.iter() {
            requirements.add(made, n);
        }
    }
    let nested = NestedFacts::with_requirements(lowering, &requirements);
    let mut next = if verdict.refused.is_some() { None } else { Some(FlowOf::Owner) };
    while let Some(of) = next {
        if of.is_nested(view) {
            next = next_nested(view, of);
            continue;
        }
        let (lowered, log) = match of {
            FlowOf::Owner => {
                let (r, log) = lower_owner_logged(rule, view, done, &nested, alloc);
                (r.map(|l| (l.flow, l.params, l.coarse)), log)
            }
            FlowOf::Closure(closure) => {
                let (r, log) = lower_closure_logged(rule, view, done, closure, &nested, alloc);
                (r.map(|cf| (cf.lowered.flow, cf.lowered.params, cf.lowered.coarse)), log)
            }
            FlowOf::AsyncBlock(block) => {
                let (r, log) = lower_async_logged(rule, view, done, block, &nested, alloc);
                (r.map(|la| (la.flow, AVec::new_in(alloc), la.coarse)), log)
            }
        };
        logs.push((of, log));
        let (flow, params, flow_coarse) = match lowered {
            Ok(f) => f,
            Err(refusal) => {
                stopped = Some(refusal);
                verdict.refused = Some(BorrowRefused::Lower(refusal));
                verdict.within = nested_range(view, of);
                break;
            }
        };
        if coarse.is_none() {
            coarse = flow_coarse.map(|c| (c, of));
        }
        if file_flow(&flow, &params, of, view, None, &mut needs, &mut verdict, alloc) {
            break;
        }
        next = next_nested(view, of);
    }
    if let Some((c, of)) = coarse
        && verdict.findings.iter().any(reads_regions)
    {
        verdict.findings.retain(|d| !reads_regions(d));
        if verdict.refused.is_none() {
            verdict.refused = Some(BorrowRefused::Stricter(c));
            verdict.within = nested_range(view, of);
        }
    }
    verdict.findings.sort_unstable();
    verdict.findings.dedup();
    Run { verdict, logs, stopped }
}

/// `module::name` of a library item, or its key's hex.
fn shown_lib<'n, S: NamesSource<'n>>(names: S, lib: blaze_id::ItemKey) -> Shown {
    let mut s = Shown::EMPTY;
    match (names.home_of(lib).and_then(|m| names.module_path(m)), names.name_of(lib)) {
        (Some(module), Some(name)) => {
            let _ = write!(s, "{module}::{name}");
        }
        (None, Some(name)) => {
            let _ = s.write_str(name);
        }
        _ => {
            for b in lib.0.0.iter().take(16) {
                let _ = write!(s, "{b:02x}");
            }
        }
    }
    s
}

/// The census of `body` filed into `into`: one body's compare (module doc).
fn census_of<'n, 'a, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, body: &Body<'n>, done: &tc::Checked<'_>, plant: FactsPlant, into: &mut CallCensusTotals, alloc: Alloc<'a>) {
    if !done.diags.is_empty() {
        into.not_compared[NotCompared::Untyped as usize] += 1;
        return;
    }
    let langs = BorrowLangs::of(r.index);
    let facts = gather_facts_planted(r, body.owner, done, langs, plant, alloc);
    let reader = r.store.regions.reader(r.store.tys.alloc());
    let lowering = facts.read_by(&reader, r.store.tys.view());
    if !facts.unknown().is_empty() {
        into.not_compared[NotCompared::FactUnknown as usize] += 1;
        return;
    }
    let Ok(tys) = body.tys.view(&r.store.tys) else {
        into.not_compared[NotCompared::TypeTable as usize] += 1;
        return;
    };
    let view = body.typeck.view(tys);
    into.compared += 1;
    let old = run_rule(CensusRule::Old, &view, done, lowering, alloc);
    // The control `FactsPlant::ClosureArgsUntied` (rule 23) runs the one rule without its
    // closure-argument tie; every other census runs the rule itself.
    let one_rule = if plant == FactsPlant::ClosureArgsUntied { CensusRule::OneUntied } else { CensusRule::One };
    let one = run_rule(one_rule, &view, done, lowering, alloc);
    let mut rows: AVec<'a, CallRow> = AVec::new_in(alloc);
    let none: CallLog<Alloc<'a>> = CallLog::empty();
    for (of, old_log) in old.logs.iter() {
        let one_log = one.logs.iter().find(|(o, _)| o == of).map_or(&none, |(_, l)| l);
        if compare_calls(old_log, one_log, &view, done, &mut rows, alloc).is_err() {
            into.reach_bound += 1;
        }
    }
    for (of, one_log) in one.logs.iter() {
        if !old.logs.iter().any(|(o, _)| o == of) && compare_calls(&none, one_log, &view, done, &mut rows, alloc).is_err() {
            into.reach_bound += 1;
        }
    }
    rows.sort_unstable();
    let related_all = one.logs.iter().all(|(_, l)| l.refused_calls() == 0);
    if related_all {
        into.one_related_all += 1;
    }
    if matches!(one.stopped, Some(Refusal { form: Form::OneRule { .. }, .. })) {
        into.one_lost_to_call += 1;
    }
    let (whole_old, whole_one) = (old.verdict.refused.is_none(), one.verdict.refused.is_none());
    if whole_old {
        into.whole_old += 1;
        for d in old.verdict.findings.iter() {
            into.findings_old[code_index(d.code)] += 1;
        }
    }
    if whole_one {
        into.whole_one += 1;
        for d in one.verdict.findings.iter() {
            into.findings_one[code_index(d.code)] += 1;
        }
    }
    if whole_old && whole_one && related_all {
        into.related_bodies += 1;
        for d in old.verdict.findings.iter() {
            into.related_old[code_index(d.code)] += 1;
        }
        for d in one.verdict.findings.iter() {
            into.related_one[code_index(d.code)] += 1;
        }
    }
    let owner = shown_lib(r.names, body.item);
    for row in rows.iter() {
        // A `Count` refusal of an alias's or a trait's lifetimes: why its pair was not filed,
        // asked again of the one read (types.rs `declared_lifetimes`, as the gather asks it).
        let uncounted = match row.one {
            Some(Form::OneRule { why: OneRuleWhy::Count { why: Uncounted::AliasLifetimes { trait_, name }, .. }, .. }) => Some((true, trait_, Some(name))),
            Some(Form::OneRule { why: OneRuleWhy::Count { why: Uncounted::TraitLifetimes(trait_), .. }, .. }) => Some((false, trait_, None)),
            _ => None,
        };
        if let Some((alias, trait_, name)) = uncounted {
            let lib = r.names.key_beginning(trait_.0.to_be_bytes());
            let heads = &r.store.maps;
            let why = match lib {
                None => Unfiled::NoLibraryKey,
                Some(_) if declared_lifetimes(heads, r.names, trait_, Declared::Trait).is_none() => Unfiled::TraitUncounted,
                Some(_) if name.is_some_and(|n| declared_lifetimes(heads, r.names, trait_, Declared::Assoc(n)).is_none()) => Unfiled::AssocUncounted,
                Some(_) => Unfiled::NotGathered,
            };
            let shown = match lib {
                Some(lib) => shown_lib(r.names, lib),
                None => {
                    let mut s = Shown::EMPTY;
                    let _ = write!(s, "{:032x}", trait_.0);
                    s
                }
            };
            into.file_unfiled(alias, why, shown, 1);
        }
        into.calls_mut().file(row);
        if !matches!(row.class, CallClass::Fewer | CallClass::OneAccepts) {
            continue;
        }
        let range = view.expr_ranges.get(row.e.ix()).copied().unwrap_or_default();
        let callee = match row.callee {
            Some(k) => match r.keys.library_in(r.names, k) {
                Some(lib) => shown_lib(r.names, lib),
                None => {
                    let mut s = Shown::EMPTY;
                    let _ = write!(s, "{:032x}", k.0);
                    s
                }
            },
            None => {
                let mut s = Shown::EMPTY;
                let _ = s.write_str("<no callee key>");
                s
            }
        };
        if row.class == CallClass::OneAccepts {
            into.file_accepts(AcceptRow { owner, start: range.start, end: range.end, callee, old: row.old });
            continue;
        }
        into.file_fewer(FewerRow { owner, start: range.start, end: range.end, callee, missing: row.missing, missing_count: row.missing_count });
    }
}

impl<'n, S: BodySource<'n>> ProjectWorld<'n, S> {
    /// The call census of one body (module doc), filed into `into`. Type checks the body again
    /// (`check_then`, as `check_borrows`): run it AFTER the body's own `check_borrows` and after
    /// its goal hits were taken (`check_then` clears them). Its verdict is dropped: the body's
    /// answer is `check_borrows`'.
    pub fn call_census<'a>(&mut self, body: BodyRef<'n>, into: &mut CallCensusTotals, alloc: Alloc<'a>) {
        self.call_census_planted(body, FactsPlant::None, into, alloc);
    }

    /// [`ProjectWorld::call_census`] with one fact provider turned off ([`FactsPlant`]), for a
    /// test's control (rule 23). Never used by the command.
    #[doc(hidden)]
    pub fn call_census_planted<'a>(&mut self, body: BodyRef<'n>, plant: FactsPlant, into: &mut CallCensusTotals, alloc: Alloc<'a>) {
        into.bodies += 1;
        let mut reached = false;
        let _verdict = self.check_then(body, Stalls::Decide, alloc, |r, lowered, done| {
            reached = true;
            census_of(r, lowered, done, plant, into, alloc);
        });
        if !reached {
            into.not_compared[NotCompared::Untyped as usize] += 1;
        }
        if self.take_seed_error().is_some() {
            into.seed_errors += 1;
        }
    }
}

/// A census's totals for one worker's stretch (tables/task.rs `BodyFrame::census`), filled by
/// each body's task in turn on that worker's thread and read when the stretch ends. Interior
/// mutability because the frame is shared by reference with every body of the stretch; never
/// borrowed across a wait (rule 27: the borrow is one call's).
///
/// A cell placed in a frame that runs no drop leaks nothing once its totals are taken
/// ([`CensusCell::take`]): what it is left holding has nothing allocated.
pub struct CensusCell {
    totals: core::cell::RefCell<CallCensusTotals>,
}

impl CensusCell {
    /// An empty cell whose totals grow in `alloc` ([`CensusAlloc`]).
    pub const fn new(alloc: CensusAlloc) -> CensusCell {
        CensusCell { totals: core::cell::RefCell::new(CallCensusTotals::new(alloc)) }
    }

    /// One body's census (tables/task.rs, after `check_borrows` and its goal hits).
    pub fn census_of<'n, 'a, S: BodySource<'n>>(&self, world: &mut ProjectWorld<'n, S>, body: BodyRef<'n>, alloc: Alloc<'a>) {
        let mut totals = self.totals.borrow_mut();
        world.call_census(body, &mut totals, alloc);
    }

    /// The totals so far, moved out; the cell is left empty, holding nothing allocated, so the
    /// next body counts from zero.
    pub fn take(&self) -> CallCensusTotals {
        let mut totals = self.totals.borrow_mut();
        let empty = CallCensusTotals::new(totals.alloc);
        core::mem::replace(&mut *totals, empty)
    }

    /// The command's name for [`CensusCell::take`] (blaze-cli engine_bodies.rs `Census::add_cell`
    /// reads a stretch's cell once, when the stretch ends): the totals by value, the cell left
    /// empty.
    pub fn totals(&self) -> CallCensusTotals {
        self.take()
    }
}

