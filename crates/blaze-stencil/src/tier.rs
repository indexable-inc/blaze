//! Tier-up from tier 0 (stencil.md, "Tier-up"; compile-time.md, change 13).
//!
//! **The counters.** Code that counts has two cells, 16 bytes, reached through one
//! relocation ([`crate::code::Name::Counter`], `Target::Counter` of its instance, placed by
//! the linker in memory the run process writes and the daemon reads in place): its entry
//! adds one to [`CALLS`], each back edge (a branch to a block at or before it in layout
//! order) one to [`BACK_EDGES`]. Stencil code counts by its glue; Cranelift code of a tiered
//! run counts by [`instrument`], an IR pass run before Cranelift compiles it, so one
//! mechanism measures both promotions. LLVM code, the last tier, does not count. The adds are
//! plain read-modify-writes: a count lost to a race is fine for a heuristic, and an aligned
//! 8-byte store never tears (HotSpot's and V8's invocation counters race the same way).
//!
//! **The reading.** [`Counters`] holds each function's cells' offset in the daemon's view
//! ([`Cells`]); a snapshot reads two words per function that has cells and nothing else: no
//! sampler, no file, no channel round trip.
//!
//! **The decision.** [`Tiering`] is a deterministic state machine over one 16-byte entry per
//! function (tier, flags, the weighted count at the last snapshot, a decayed rate). Fed snapshots, it
//! says which functions to recompile at which tier ([`Promote`]); at most one recompile is in
//! flight per function, tiers only rise, and a tier whose recompile failed is not asked for
//! again in the run. [`Driver`] is one round of it over the cells, and [`Runner`] runs rounds
//! on a thread with workers that call the daemon's [`Recompile`]: compile the unit at the
//! tier's settings, place it, publish it in the function's slot and retire the old code
//! behind blaze-inproc's fence.
//!
//! **The thresholds** ([`Policy::default`]) are the ski-rental rule: recompile once the time
//! the slower code has cost equals what the recompile costs, which is never worse than twice
//! the best choice made knowing the future. A count is calls plus back edges (HotSpot's
//! `i + b`): one unit is about one call of a small function or one loop iteration.
//! - Tier 0 to Cranelift: a Cranelift compile is about 50 stencil compiles, 30 to 100 µs a
//!   function, and its code saves about 10 to 25 ns a unit over stencil code (slot traffic
//!   and no register allocation), so break-even is a few thousand units: [`Policy::warm`] is
//!   4,096 (HotSpot's tier 3 asks 200 calls or 2,000 calls plus back edges, for a C1 compile
//!   that costs about what Cranelift's does over a much faster interpreter).
//! - Cranelift to LLVM: an LLVM compile at full optimization is about 10 Cranelift compiles,
//!   0.3 to 1 ms, and saves about 5 to 10 ns a unit over Cranelift: break-even is about 16
//!   times tier 0's, between HotSpot's tier 3 to 4 ratios (2,000 to 15,000 for counts, 200 to
//!   5,000 for calls): [`Policy::hot`] is 65,536, counted in Cranelift's code. A tier-0
//!   function already past it goes to LLVM at once.

use std::sync::atomic::{AtomicU64, Ordering};

use blaze_codegen::Backend;

mod instrument;
mod runner;

#[cfg(test)]
mod tests;

pub use instrument::instrument;
pub use runner::{Closed, Config, Failure, Runner};

/// Bytes of one function's cells: two `u64`s, 8-aligned.
pub const CELLS: u32 = 16;
/// The addend of the calls cell from the cells' address.
pub const CALLS: i64 = 0;
/// The addend of the back-edges cell.
pub const BACK_EDGES: i64 = 8;
/// The namespace of the user name [`instrument`] gives the cells in Cranelift IR
/// (`ir::UserExternalName { namespace: COUNTER_NAMESPACE, index: 0 }`): codegen maps it, as it
/// maps [`crate::code::Name::Counter`], to `Target::Counter` of the unit. Beside codegen's
/// own namespaces (`u32::MAX` built-in bodies, `- 1` fold bodies, `- 2` coverage).
pub const COUNTER_NAMESPACE: u32 = u32::MAX - 3;

/// The tiers of a dev or test run's function.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Tier {
    /// Copy-and-patch: every function's first code.
    Stencil,
    /// Cranelift: warm functions.
    Cranelift,
    /// LLVM at full optimization: hot functions.
    Llvm,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Stencil, Tier::Cranelift, Tier::Llvm];

    /// The backend that compiles it.
    pub fn backend(self) -> Backend {
        match self {
            Tier::Stencil => Backend::Stencil,
            Tier::Cranelift => Backend::Cranelift { speed: false },
            Tier::Llvm => Backend::Llvm { opt: 3 },
        }
    }

    /// Whether code of this tier counts into its cells: the settings it is compiled at say so
    /// (`Spec::counted`), so counting and plain Cranelift code are different instances.
    pub fn counts(self) -> bool {
        match self {
            Tier::Stencil | Tier::Cranelift => true,
            Tier::Llvm => false,
        }
    }

    fn bit(self) -> u8 {
        1 << (self as u8 + 1)
    }
}

/// When a function moves up. The module's docs argue the defaults.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Weighted count at which tier 0 code is recompiled with Cranelift.
    pub warm: u64,
    /// Weighted count at which Cranelift code (or tier 0 code already this hot) is recompiled
    /// with LLVM.
    pub hot: u64,
    /// What one back edge weighs against one call.
    pub back_edge_weight: u64,
    /// The highest tier asked for: [`Tier::Cranelift`] for a blaze built without LLVM, whose
    /// LLVM units codegen refuses, [`Tier::Stencil`] for a run that is not tiered.
    pub highest: Tier,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy { warm: 1 << 12, hot: 1 << 16, back_edge_weight: 1, highest: Tier::Llvm }
    }
}

impl Policy {
    /// The weighted count of the current tier's cells that earns `tier`.
    pub fn threshold(&self, tier: Tier) -> u64 {
        match tier {
            Tier::Stencil => 0,
            Tier::Cranelift => self.warm,
            Tier::Llvm => self.hot,
        }
    }
}

/// One snapshot of a function's cells.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Counts {
    pub calls: u64,
    pub back_edges: u64,
}

impl Counts {
    /// Calls plus back edges at `back_edge_weight` each, saturating.
    pub fn weighted(self, back_edge_weight: u64) -> u64 {
        self.calls.saturating_add(self.back_edges.saturating_mul(back_edge_weight))
    }
}

/// A recompile [`Tiering`] asks for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Promote {
    /// The function's index among the run's tiered functions: its `LazyId` in a lazy run.
    pub function: u32,
    pub to: Tier,
}

/// The [`State::flags`] bit of a function whose recompile is in flight; the bits above it
/// are the tiers refused ([`Tier::bit`]).
const IN_FLIGHT: u8 = 1;
/// A function's rate keeps `1 - 2^-RATE_SHIFT` of itself each snapshot.
const RATE_SHIFT: u32 = 2;

/// One function's tiering state. A round visits functions in the order their cells were bound
/// (first-call order in a lazy run), not by index, so everything `observe` touches is in one
/// 16-byte entry: one cache line per function, never split.
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct State {
    /// The weighted count of the function's current cells at its last snapshot.
    last: u64,
    /// Its new counts per snapshot, decayed, saturating: which promotion goes first.
    rate: u32,
    tier: Tier,
    flags: u8,
}

const _: () = assert!(size_of::<State>() == 16);

/// The tiering state of one run: one entry per function, by index.
pub struct Tiering {
    policy: Policy,
    state: Vec<State>,
}

impl Tiering {
    /// `functions` functions, all at [`Tier::Stencil`].
    pub fn new(functions: u32, policy: Policy) -> Tiering {
        let start = State { last: 0, rate: 0, tier: Tier::Stencil, flags: 0 };
        // alloc: once per run; `observe` never grows it.
        Tiering { policy, state: vec![start; functions as usize] }
    }

    pub fn len(&self) -> u32 {
        self.state.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn tier(&self, function: u32) -> Tier {
        self.state[function as usize].tier
    }

    /// Every function's tier, by index: what the run reached, for the next run to start at.
    pub fn tiers(&self) -> Vec<Tier> {
        // alloc: the run's outcome, once at its end.
        self.state.iter().map(|it| it.tier).collect()
    }

    pub fn in_flight(&self, function: u32) -> bool {
        self.state[function as usize].flags & IN_FLIGHT != 0
    }

    /// Whether a recompile of `function` at `tier` failed in this run.
    pub fn refused(&self, function: u32, tier: Tier) -> bool {
        self.state[function as usize].flags & tier.bit() != 0
    }

    pub fn rate(&self, function: u32) -> u64 {
        u64::from(self.state[function as usize].rate)
    }

    /// Takes a snapshot of `function`'s cells; pushes to `out` the recompile it now earns,
    /// if any and if none is in flight for it.
    pub fn observe(&mut self, function: u32, counts: Counts, out: &mut Vec<Promote>) {
        let policy = self.policy;
        let state = &mut self.state[function as usize];
        let weighted = counts.weighted(policy.back_edge_weight);
        // A count below the last is a racing add that stored an older value: nothing new. New
        // cells come only with a tier, and `landed` starts their count from zero.
        let new = u32::try_from(weighted.saturating_sub(state.last)).unwrap_or(u32::MAX);
        state.last = weighted;
        state.rate = (state.rate - (state.rate >> RATE_SHIFT)).saturating_add(new);
        if state.flags & IN_FLIGHT != 0 {
            return;
        }
        if let Some(to) = earned(&policy, state, weighted) {
            state.flags |= IN_FLIGHT;
            out.push(Promote { function, to });
        }
    }

    /// The tier the last run left `function` at (by its identity, idea 15): a recompile to
    /// it before any count, if it is above the function's and allowed.
    pub fn remembered(&mut self, function: u32, tier: Tier) -> Option<Promote> {
        let highest = self.policy.highest;
        let state = &mut self.state[function as usize];
        let allowed = tier > state.tier && tier <= highest && state.flags & (IN_FLIGHT | tier.bit()) == 0;
        allowed.then(|| {
            state.flags |= IN_FLIGHT;
            Promote { function, to: tier }
        })
    }

    /// The recompile of `function` at `tier` is published: it is at `tier` now, counting in
    /// new cells.
    pub fn landed(&mut self, function: u32, tier: Tier) {
        let state = &mut self.state[function as usize];
        state.flags &= !IN_FLIGHT;
        if tier > state.tier {
            state.tier = tier;
            state.last = 0;
        }
    }

    /// The recompile of `function` at `tier` failed: it stays where it is and is not asked
    /// for again at `tier` in this run (the failure is reported by the caller, never hidden).
    pub fn failed(&mut self, function: u32, tier: Tier) {
        let state = &mut self.state[function as usize];
        state.flags = (state.flags & !IN_FLIGHT) | tier.bit();
    }

    /// Orders `promotes` hottest first, by rate and then by function: what idle cores
    /// compile first.
    pub fn hottest_first(&self, promotes: &mut [Promote]) {
        // A `u64` key per promotion (rate high, index low), so the sort compares integers.
        let key = |it: &Promote| {
            let rate = self.state[it.function as usize].rate;
            (u64::from(u32::MAX - rate) << 32) | u64::from(it.function)
        };
        promotes.sort_unstable_by_key(key);
    }
}

/// The highest tier above `state`'s that `policy` allows, the run has not refused, and
/// `weighted` is past the threshold of.
fn earned(policy: &Policy, state: &State, weighted: u64) -> Option<Tier> {
    [Tier::Llvm, Tier::Cranelift].into_iter().find(|&tier| {
        tier > state.tier
            && tier <= policy.highest
            && state.flags & tier.bit() == 0
            && weighted >= policy.threshold(tier)
    })
}

/// The memory counter cells are in, as the daemon reads it: blaze-inproc's view of the run
/// process's cells.
pub trait Cells {
    /// The aligned 8-byte word at `offset`, loaded atomically (relaxed is enough); an error
    /// names the offset when it is not in the memory.
    fn load(&self, offset: u64) -> Result<u64, String>;
}

/// Words in this process: the tests' memory, and a daemon that keeps cells of its own.
impl Cells for [AtomicU64] {
    fn load(&self, offset: u64) -> Result<u64, String> {
        if !offset.is_multiple_of(8) {
            // alloc: the error, which stops the run's tiering.
            return Err(format!("a counter load at {offset:#x}, which is not 8-byte aligned"));
        }
        let word = usize::try_from(offset / 8).ok().and_then(|at| self.get(at));
        word.map(|it| it.load(Ordering::Relaxed))
            // alloc: the error, as above.
            .ok_or_else(|| format!("a counter load at {offset:#x}, past the {} bytes of cells", self.len() * 8))
    }
}

impl<T: Cells + ?Sized> Cells for std::sync::Arc<T> {
    fn load(&self, offset: u64) -> Result<u64, String> {
        (**self).load(offset)
    }
}

const UNBOUND: u32 = u32::MAX;

/// Where each function's cells are, and the dense list of those that have some: a snapshot
/// reads only them, two loads each.
pub struct Counters {
    /// By function: the cells' offset, meaningful where `at` is bound.
    cells: Vec<u64>,
    /// By function: its place in `bound`, or [`UNBOUND`].
    at: Vec<u32>,
    bound: Vec<u32>,
}

impl Counters {
    pub fn new(functions: u32) -> Counters {
        let n = functions as usize;
        // alloc: the columns, once per run; `bound` grows to at most `functions` once.
        Counters { cells: vec![0; n], at: vec![UNBOUND; n], bound: Vec::new() }
    }

    /// `function`'s code now counts in the cells at `cells` (an offset in [`Cells`]), or does
    /// not count (`None`).
    pub fn bind(&mut self, function: u32, cells: Option<u64>) -> Result<(), String> {
        let i = function as usize;
        if i >= self.at.len() {
            // alloc: the error, which names the function.
            return Err(format!("counter cells for function {function} of {}", self.at.len()));
        }
        match cells {
            Some(offset) => {
                if !offset.is_multiple_of(8) || offset.checked_add(u64::from(CELLS)).is_none() {
                    // alloc: the error, which names the function.
                    return Err(format!(
                        "function {function}'s counter cells at {offset:#x}: cells are 8-aligned"
                    ));
                }
                self.cells[i] = offset;
                if self.at[i] == UNBOUND {
                    self.at[i] = self.bound.len() as u32;
                    self.bound.push(function);
                }
            }
            None => {
                let at = std::mem::replace(&mut self.at[i], UNBOUND);
                if at != UNBOUND {
                    self.bound.swap_remove(at as usize);
                    if let Some(&moved) = self.bound.get(at as usize) {
                        self.at[moved as usize] = at;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn cells(&self, function: u32) -> Option<u64> {
        let i = function as usize;
        (*self.at.get(i)? != UNBOUND).then(|| self.cells[i])
    }

    /// How many functions have cells.
    pub fn bound(&self) -> usize {
        self.bound.len()
    }

    /// `function`'s counts, if it has cells.
    pub fn read<M: Cells + ?Sized>(&self, function: u32, memory: &M) -> Result<Option<Counts>, String> {
        let Some(offset) = self.cells(function) else { return Ok(None) };
        read(memory, offset).map(Some)
    }

    /// Every function with cells and its counts, appended to `out`.
    pub fn snapshot<M: Cells + ?Sized>(&self, memory: &M, out: &mut Vec<(u32, Counts)>) -> Result<(), String> {
        out.reserve(self.bound.len());
        for &function in &self.bound {
            let counts = read(memory, self.cells[function as usize])
                // alloc: the error, which stops the run's tiering.
                .map_err(|why| format!("reading function {function}'s counter cells: {why}"))?;
            out.push((function, counts));
        }
        Ok(())
    }
}

fn read<M: Cells + ?Sized>(memory: &M, offset: u64) -> Result<Counts, String> {
    Ok(Counts {
        calls: memory.load(offset + CALLS as u64)?,
        back_edges: memory.load(offset + BACK_EDGES as u64)?,
    })
}

/// One run's tiering without threads: rounds over the cells and the outcomes of the
/// recompiles they ask for. [`Runner`] drives it; the simulation tests step it by hand.
pub struct Driver {
    tiering: Tiering,
    counters: Counters,
    read: Vec<(u32, Counts)>,
    promotes: Vec<Promote>,
}

impl Driver {
    pub fn new(functions: u32, policy: Policy) -> Driver {
        Driver {
            tiering: Tiering::new(functions, policy),
            counters: Counters::new(functions),
            read: Vec::new(),     // alloc: reused by every round, grown by the first.
            promotes: Vec::new(), // alloc: as `read`.
        }
    }

    pub fn tiering(&self) -> &Tiering {
        &self.tiering
    }

    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// `function`'s tier-0 code is placed, counting in `cells`.
    pub fn bind(&mut self, function: u32, cells: Option<u64>) -> Result<(), String> {
        self.counters.bind(function, cells)
    }

    /// See [`Tiering::remembered`].
    pub fn remembered(&mut self, function: u32, tier: Tier) -> Option<Promote> {
        self.tiering.remembered(function, tier)
    }

    /// Reads every counting function's cells and says what to recompile, hottest first.
    pub fn round<M: Cells + ?Sized>(&mut self, memory: &M) -> Result<&[Promote], String> {
        self.read.clear();
        self.promotes.clear();
        self.counters.snapshot(memory, &mut self.read)?;
        for &(function, counts) in &self.read {
            self.tiering.observe(function, counts, &mut self.promotes);
        }
        self.tiering.hottest_first(&mut self.promotes);
        Ok(&self.promotes)
    }

    /// `promote` is published, its code counting in `cells` if its tier counts.
    pub fn landed(&mut self, promote: Promote, cells: Option<u64>) -> Result<(), String> {
        let Promote { function, to } = promote;
        if to.counts() != cells.is_some() {
            return Err(match cells {
                // alloc: the error, which stops the run's tiering.
                Some(cells) => format!("function {function} at {to:?}, which counts nothing, has cells at {cells:#x}"),
                None => format!("function {function} at {to:?}, which counts, has no counter cells"), // alloc: as above.
            });
        }
        self.tiering.landed(function, to);
        self.counters.bind(function, cells)
    }

    /// `promote` failed: see [`Tiering::failed`].
    pub fn failed(&mut self, promote: Promote) {
        self.tiering.failed(promote.function, promote.to);
    }
}

/// The daemon's side of a promotion.
pub trait Recompile: Send + Sync + 'static {
    /// Compiles `function` at `to` (its unit at `spec.at(backend)` with `counted` as
    /// [`Tier::counts`] says), places it, publishes it in the function's slot and retires the
    /// code it replaces behind blaze-inproc's fence. Gives the new code's counter cells (an
    /// offset in the run's [`Cells`]) when `to` counts.
    fn recompile(&self, function: u32, to: Tier) -> Result<Option<u64>, String>;
}
