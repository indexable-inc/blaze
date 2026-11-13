//! Seal groups: a host's crates as the engine sees them, so a write to one crate's inputs costs
//! that crate and never the rows of the crates beside it (docs/v2/plan/design/restore.md,
//! "Sealed crates").
//!
//! Three durability kinds for the whole database cannot say "only this crate": a write of a
//! kind makes every row of that kind verify again. A seal group is a kind of its own a crate.
//!
//! # What a group is
//!
//! - The host makes a group a crate ([`Storage::seal_group`]), says which groups it depends on
//!   (the crate graph, only ever added to), and says of each input which group it is an input
//!   of ([`super::plumbing::input_seal_group`]). That is the index from an input to its group:
//!   one word a node, read at the write, no walk.
//! - A row's group is worked out from its edges as its run ends ([`Seals::derived`]), beside
//!   its summary: the one group among those of what it read that depends on all the others;
//!   none when it read nothing of any group or when no one of them covers the rest.
//! - [`LOOSE`], a bit of the input summary, says what the groups do not cover: it is on every
//!   input field of no group and on every row whose reads no one group covers, and so (the
//!   summaries' own rule, kept complete by [`Storage::complete_summaries`]) on every row that
//!   reads one of them at any depth. **A row without it reads, transitively, only inputs of
//!   its own group and of the groups that one depends on.** A row whose group moves to one its
//!   old group does not depend on is loose from then on ([`POISONED`]).
//!
//! # A seal
//!
//! A pass ([`Storage::seal_quiet_groups`], after a build's answer) seals each group that has
//! been quiet long enough ([`SealKind`]): every row of it that is current is put behind the
//! seal, and the group keeps, sorted, what those rows read from outside it (its *imports*: rows
//! and fields that are not behind this seal and not inputs of this group), the view it was
//! sealed in, and the OR of the summaries of the imports that are loose (`wild`).
//!
//! The invariant: **while a group's state is held, every row behind its seal holds the value
//! it held at `sealed_at`.** A row changes only by running again for a read that changed; a
//! read of a sealed row is a row behind the same seal, an input of the group, or an import. So
//! the rows hold while no input of the group was written and no import's stamp passed
//! `sealed_at`. That is one check of the group, never one of each row:
//!
//! - *A write* ([`Storage::seal_wrote`], in the setter, `&mut`) reads the field's group word
//!   and opens that group and the groups declared to open with it; then, in one loop over the
//!   groups (no rows), every held group that depends on the field's group, or whose `wild`
//!   shares a bit with the field's, becomes suspect. Nothing else is touched: a group that
//!   neither depends on the written one nor reads it loosely stays held.
//! - *A suspect group* is checked the first time a row of it is asked, or by the walk
//!   ([`resolve`]): imports that are behind another seal that holds, of the generation this
//!   group last saw, are passed over as one run; the others are brought up to date as a
//!   reader's edge is, and their stamps compared with `sealed_at`. All unchanged: the group
//!   is held again, and each of its rows is proved by one load when asked
//!   ([`Seals::proves`]). One changed: the seal is broken.
//! - *A broken seal* proves nothing, and is never the grain of what is redone: its rows are
//!   verified as any rows, by their own edges. The walk indexes the group's rows from their
//!   own edges ([`Seals::index_broken`]) and hands out only the rows that read an import that
//!   moved ([`moved_readers`]); a row of the group that no changed read reaches is proved by
//!   one load once the walk ends. The next `&mut` opens the group
//!   ([`Storage::seal_finalize`]).
//!
//! Why a write that can change a sealed row always makes its group suspect or opens it. Take
//! the written field `w` and a path of reads from a row `m` behind the seal of `G` down to
//! `w`, and the first node `i` on it that is not behind `G`'s seal. If `i` is `w` and an input
//! of `G`, `G` is opened. Otherwise `i` is an import. If `i`'s summary has [`LOOSE`], `wild`
//! holds its summary, which holds `w`'s bits. If not, `i` is an input of, or a row of, a group
//! `T` that `G` depends on (a row's group covers the groups of its reads), and everything `i`
//! reads is an input of `T` or of a group below `T`: so `w`'s group is below `G`, and `G` is
//! made suspect by the dependency rule. An import that turns loose after the seal gains the
//! bit, and a gain opens every sealed group that imports the node
//! ([`Storage::seal_open_for_gain`]), so `wild` is never short.
//!
//! # The reverse index, and what an unseal costs
//!
//! A row behind a seal needs no reverse edges: no walk enters a sealed group
//! ([`super::reverse::Plan::suspects`]). They are dropped when the index is next made
//! ([`Storage::shed_sealed_readers`]) and made again from the group's own rows when it opens
//! ([`Storage::seal_open`]): the cost of an unseal is the group's rows and their edges, never
//! another group's. Forward edges are kept for a local group: its opened rows verify by them,
//! with no run. A library's group ([`SealKind::lets_go_edges`]) lets go of its rows' forward
//! edges as they go behind its seal (memory.md 4.4), once the seal's imports are listed: a
//! library's inputs are written almost never, and its rows are most of a build's. Such a row
//! ([`Seals::let_go`]) is proved by its seal, its durability or its summary, none of which
//! reads an edge, and by nothing else: no reverse edge reaches it, so the walk's reach and
//! end say nothing of it, and a verify that would walk its edges runs it again. When its
//! group stops holding (opened by a write, or its seal broken), each such row is queued
//! ([`Seals::assume`]) and every walk names it as changed and sets it apart until it has
//! run again; a pass seals it again only after it has (`sealable`).
//!
//! # The order of a walk (end-state.md, rule 2 of the rebuild)
//!
//! A walk checks every seal a write may have reached, in dependency order
//! ([`resolve_all`]), and indexes the groups whose seal broke, before it traverses a row and
//! before it works out its reach: no seal changes state while rows are traversed, so what
//! the walk gathers of a group is gathered once, from the group's final state in the view.
//!
//! # What sealing must not break
//!
//! The proofs the engine had (durability, summary, floor, reach) stay sound for a row whether
//! or not it is behind a seal: summaries are kept complete through sealed groups by opening
//! any group a gain passes through, and the floor is not set while a group waits for its check.
//! A seal only adds a proof.
//!
//! Persistence: none. A storage is memory; a group, its seal and its imports are made from
//! the rows of this process.

use std::{
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering},
};

use blaze_mem::Tag;
use crossbeam_queue::SegQueue;

use super::{
    Cancelled, OutOfFuel, OutOfSteps,
    dense::Dense,
    frontier::{self, Plant},
    function::Verified,
    ingredient,
    reverse::{Reverse, Seen},
    row::{CYCLE, DURABILITY, HEAD, PROVISIONAL, RUNNING, STORED, UNTRACKED, VIEW_MASK, span},
    storage::{
        FIELD, LOOSE, NODES, NodeRow, Restamp, Storage, Summary, input_bits, key_parts, loose, or,
    },
    verify,
};

/// A node's word: its group's number, 0 for none.
const GROUP: u32 = 0xffff;
/// A row's word: its forward edges were let go when it went behind a library group's seal
/// ([`SealKind::lets_go_edges`]). Such a row is proved only by its seal while that holds;
/// otherwise it runs again ([`Seals::let_go`]). Cleared by its next run, which stores edges
/// again (its word is made anew as the run ends, [`Seals::derived`]).
const LET_GO: u32 = 1 << 16;
/// A node's word: some sealed group imports the node, or did. A gain of its summary looks for
/// the groups that do ([`Storage::seal_open_for_gain`]); never cleared, so it may say more.
const IMPORTED: u32 = 1 << 28;
/// A field's word: the host said the field is an input of its group.
const INPUT: u32 = 1 << 29;
/// A row's word: its group once moved to one its old group does not depend on, so rows that
/// read it may no longer be covered by their own group: it is loose at every run from then on.
const POISONED: u32 = 1 << 30;
/// A row's word: it was put behind its group's seal by the group's last sealing. It says
/// nothing while the group is open.
const SEALED: u32 = 1 << 31;

/// A group's states. Readers move a group from suspect to checking and from there to held or
/// broken; everything else is a writer's (`&mut`).
const OPEN: u8 = 0;
const HELD: u8 = 1;
const SUSPECT: u8 = 2;
const CHECKING: u8 = 3;
const BROKEN: u8 = 4;

/// The readers behind seals past which a pass makes the reverse index again without them
/// ([`Storage::shed_sealed_readers`]), when they are also half of what the index holds.
const SHED_AT: u64 = if cfg!(test) { 8 } else { 1 << 16 };

/// A seal group: what a host makes a crate ([`Storage::seal_group`]).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SealGroup(u16);

impl SealGroup {
    /// Its number in its storage, from 1.
    pub fn number(self) -> u16 {
        self.0
    }
}

/// When a group is sealed. A pass ([`Storage::seal_quiet_groups`]) counts, a group, the passes
/// in a row that found no write to an input of it (or of a group it opens with) and no broken
/// seal since the pass before; a host calls one pass a build, after the answer, so the count
/// is of builds, never of a clock.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SealKind {
    /// A crate nobody edits (the toolchain's, a registry or git package): sealed by the first
    /// pass that finds rows of it.
    Library,
    /// A crate of the workspace, or a path dependency: sealed by the pass that ends the
    /// second build in a row with no write to its inputs.
    Local,
    /// A test's crate that stands for a [`SealKind::Local`] one with its quiet builds
    /// skipped: sealed by the first pass that finds rows of it, its rows' edges kept. No host
    /// makes one.
    #[cfg(test)]
    LocalAtOnce,
}

impl SealKind {
    fn quiet_passes(self) -> u32 {
        match self {
            SealKind::Library => 0,
            SealKind::Local => 2,
            #[cfg(test)]
            SealKind::LocalAtOnce => 0,
        }
    }

    /// Whether the rows put behind a seal of this kind let go of their forward edges: a
    /// library's, which nobody edits. Such a row is proved by its seal, or by its durability
    /// or summary (neither reads an edge), never by an edge or the walk; when its group opens
    /// or its seal breaks and neither of those proves it, it runs again (with its value kept,
    /// so an equal result is backdated and its readers stop there). Right for a library, whose group opens only
    /// when the toolchain or a registry package changes under the build; wrong for a local
    /// crate, whose rows an edit verifies one by one by their edges (restore.md, "Sealed
    /// crates").
    pub fn lets_go_edges(self) -> bool {
        matches!(self, SealKind::Library)
    }
}

/// What a group is in now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SealState {
    /// Not sealed: its rows are verified one by one and the reverse index names them.
    Open,
    /// Sealed, and no write since its last check could have reached it: each row behind the
    /// seal is proved by one load.
    Sealed,
    /// Sealed, and a write may have reached what its rows read from outside: checked once
    /// before a row of it is taken as unchanged.
    Suspect,
    /// Its check found a changed import: the seal proves nothing, and the next write opens it.
    Broken,
}

/// One stretch of a group's imports: those of one source group that are, or are not, behind
/// that group's seal.
struct Run {
    /// The source group's number shifted left once, and whether the imports are behind its
    /// seal: what the imports are sorted by.
    class: u32,
    start: u32,
    end: u32,
    /// For imports behind the source's seal: the source's generation when every import of the
    /// run was last seen behind its seal with a stamp at or before this group's `sealed_at`;
    /// 0 for never. While the source holds at that generation, the run is passed over whole.
    generation: AtomicU32,
}

impl Run {
    fn source(&self) -> usize {
        (self.class >> 1) as usize
    }

    fn behind_seal(&self) -> bool {
        self.class & 1 != 0
    }
}

struct Group {
    name: Box<str>,
    kind: SealKind,
    state: AtomicU8,
    /// The groups it depends on, transitively, not itself: a bit a group's number.
    below: Vec<u64>,
    /// The groups a write to one of its inputs opens with it.
    with: Vec<u16>,
    /// An input of it was written, or its seal broke, since the last pass.
    written: bool,
    /// Passes in a row that found it quiet.
    quiet: u32,
    /// Bumped each time it is opened: a seal of another generation is another seal.
    generation: u32,
    /// The stamp of the view it was last sealed (or added to) in.
    sealed_at: u32,
    /// The rows behind its seal. Empty while open.
    members: Vec<u32>,
    /// Rows whose reads place them in it and that are not behind its seal: those that ran
    /// since, those a pass found not current, and every row of it while it is open. May name
    /// a row more than once, and rows that moved to another group since.
    pending: Vec<u32>,
    /// What the rows behind its seal read from outside it, by `runs`.
    imports: Vec<u32>,
    runs: Vec<Run>,
    /// The OR of the summaries of its loose imports.
    wild: Summary,
    /// The edges of the rows behind its seal that are kept.
    edges: u64,
    /// The edges of the rows behind its seal that were let go ([`LET_GO`]).
    let_go: u64,
    /// Whether the reverse index names every row behind its seal as a reader. False once the
    /// index was made without them.
    indexed: bool,
    /// While its seal is broken: the rows behind it as readers of what they read, made from
    /// their own edges by the walk that first met the seal broken ([`Seals::index_broken`]);
    /// null otherwise. Freed when the group opens (the storage's index names the rows then)
    /// or the storage goes.
    overlay: AtomicPtr<Reverse>,
    /// A group it depends on is suspect, being checked or broken: a row of it is not proved
    /// by a walk's reach ([`Seals::reach_proves`]).
    shadow: AtomicBool,
}

impl Group {
    fn new(name: &str, kind: SealKind) -> Group {
        Group {
            name: name.into(), // alloc: a group's name, once a group
            kind,
            state: AtomicU8::new(OPEN),
            below: Vec::new(),
            with: Vec::new(),
            written: false,
            quiet: 0,
            generation: 1,
            sealed_at: 0,
            members: Vec::new(),
            pending: Vec::new(),
            imports: Vec::new(),
            runs: Vec::new(),
            wild: [0; 2],
            edges: 0,
            let_go: 0,
            indexed: true,
            overlay: AtomicPtr::new(std::ptr::null_mut()),
            shadow: AtomicBool::new(false),
        }
    }
}

fn has(bits: &[u64], at: usize) -> bool {
    bits.get(at / 64).is_some_and(|word| (word >> (at % 64)) & 1 != 0)
}

fn set(bits: &mut Vec<u64>, at: usize) {
    if bits.len() <= at / 64 {
        bits.resize(at / 64 + 1, 0); // alloc: a group's dependencies, a bit a group
    }
    bits[at / 64] |= 1 << (at % 64);
}

fn or_into(bits: &mut Vec<u64>, other: &[u64]) {
    if bits.len() < other.len() {
        bits.resize(other.len(), 0); // alloc: as `set`
    }
    for (word, &more) in bits.iter_mut().zip(other) {
        *word |= more;
    }
}

/// The storage's seal groups.
pub(crate) struct Seals {
    /// A word a node: its group and [`IMPORTED`], [`INPUT`], [`POISONED`], [`SEALED`].
    /// Committed up to the last node given a word; a node past it has none.
    words: Dense<AtomicU32>,
    /// The groups by number; 0 is no group's.
    groups: Vec<Group>,
    /// Groups not open. Changed under `&mut` only, so a row's proof reads it plainly.
    sealed: u32,
    /// Groups suspect, being checked or broken.
    unsettled: AtomicU32,
    /// Rows whose group changed as a run ended, for the next pass to list under their group.
    moved: SegQueue<u32>,
    /// [`Plant::SecondClaimAdmitted`]: a group being checked is claimed again by whoever
    /// asks. False outside that control.
    second_claims: bool,
    /// Checks run, seals broken and groups opened, over the storage's life.
    checks: AtomicU64,
    breaks: AtomicU64,
    opens: u64,
    /// Rows whose edges a pass let go, over the storage's life ([`LET_GO`]): 0 lets a proof
    /// pass over the word's load.
    let_go_rows: u64,
    /// Rows whose edges were let go and whose group stopped holding (opened, or its seal
    /// broke): no reverse edge reaches them, so the next walk starts from each as changed,
    /// and every walk after it until the row has run again ([`super::frontier`]'s `shed`).
    assumed: SegQueue<u32>,
    /// [`Plant::LetGoRowsNotAssumed`]: a group that stops holding queues none of its rows
    /// without edges. False outside that control.
    forgets_assumed: bool,
}

impl Drop for Seals {
    fn drop(&mut self) {
        for group in &mut self.groups {
            let overlay = *group.overlay.get_mut();
            if !overlay.is_null() {
                // SAFETY: from `Box::into_raw` in `Seals::index_broken`; the storage is
                // going, one owner.
                drop(unsafe { Box::from_raw(overlay) });
            }
        }
    }
}

impl Seals {
    pub(crate) fn new() -> Seals {
        Seals {
            words: Dense::growing(NODES, Tag::Query, "seal groups"),
            groups: vec![Group::new("", SealKind::Library)], // alloc: once a storage, group 0's place
            sealed: 0,
            unsettled: AtomicU32::new(0),
            moved: SegQueue::new(),
            second_claims: false,
            checks: AtomicU64::new(0),
            breaks: AtomicU64::new(0),
            opens: 0,
            let_go_rows: 0,
            assumed: SegQueue::new(),
            forgets_assumed: false,
        }
    }

    /// Whether the row of `node` let go of its edges behind a library's seal and has not run
    /// since: it is proved by nothing but a seal that holds ([`Seals::proves`]), never by its
    /// edges (it has none), its summary, the floor or a walk's reach (no reverse edge reaches
    /// it). Its callers run it again instead.
    #[inline]
    pub(crate) fn let_go(&self, node: u32) -> bool {
        self.let_go_rows != 0 && self.word(node) & LET_GO != 0
    }

    /// The group of the row of `node`, a row that [`Seals::let_go`] says let go of its edges:
    /// the library group whose seal it is behind.
    #[inline]
    pub(crate) fn sealed_group(&self, node: u32) -> SealGroup {
        let group = self.word(node) & GROUP;
        assert!(group != 0, "a row that let go of its edges is in no seal group");
        SealGroup(group as u16)
    }

    /// The rows of `members` whose edges were let go, for the next walk to start from as
    /// changed: what a group that stops holding does (opened, or its seal broken). A row that
    /// ran meanwhile is passed over by the walk.
    fn assume(&self, group: &Group, members: &[u32]) {
        if group.let_go == 0 || self.forgets_assumed {
            return;
        }
        for &node in members {
            if self.word(node) & LET_GO != 0 {
                self.assumed.push(node); // alloc: a word a row of a library group that opened
            }
        }
    }

    /// Takes the rows [`Seals::assume`] queued, into `out`.
    pub(crate) fn take_assumed(&self, out: &mut Vec<u32>) {
        while let Some(node) = self.assumed.pop() {
            out.push(node); // alloc: the walk's list of rows without edges
        }
    }

    /// Whether rows are queued by [`Seals::assume`] for the next walk.
    pub(crate) fn any_assumed(&self) -> bool {
        !self.assumed.is_empty()
    }

    /// The bytes its column of words has committed.
    pub(crate) fn bytes(&self) -> u64 {
        self.words.committed_bytes() as u64
    }

    /// Whether any group was made: a run works out its row's group only then.
    #[inline(always)]
    fn declared(&self) -> bool {
        self.groups.len() > 1
    }

    #[inline]
    fn word(&self, node: u32) -> u32 {
        self.words.get_checked(node as usize).map_or(0, |it| it.load(Ordering::Relaxed))
    }

    fn set_word(&self, node: u32, word: u32) {
        self.words.ensure(node as usize + 1);
        self.words.get(node as usize).store(word, Ordering::Relaxed);
    }

    fn state(&self, group: u32) -> u8 {
        self.groups[group as usize].state.load(Ordering::Acquire)
    }

    /// Whether group `a` depends on group `b`, at any depth.
    fn depends(&self, a: u32, b: u32) -> bool {
        has(&self.groups[a as usize].below, b as usize)
    }

    /// Whether the row of `node` is behind a seal that holds: it holds the value it held when
    /// its group was sealed (the module's invariant), with no look at its edges.
    #[inline]
    pub(crate) fn proves(&self, node: u32) -> bool {
        if self.sealed == 0 {
            return false;
        }
        let word = self.word(node);
        word & SEALED != 0 && self.state(word & GROUP) == HELD
    }

    /// Whether the row of `node` is behind a seal that is held, suspect or being checked: its
    /// group answers for it, and a walk does not enter it. A row of a broken seal is a row as
    /// any other.
    #[inline]
    pub(crate) fn sealed_member(&self, node: u32) -> bool {
        if self.sealed == 0 {
            return false;
        }
        let word = self.word(node);
        word & SEALED != 0 && matches!(self.state(word & GROUP), HELD | SUSPECT | CHECKING)
    }

    /// The group of the row of `node` when the row is behind its seal and the group is
    /// suspect: the caller checks the group ([`resolve`]) before it looks at the row.
    #[inline]
    pub(crate) fn suspect_of(&self, node: u32) -> Option<u16> {
        if self.unsettled.load(Ordering::Acquire) == 0 {
            return None;
        }
        let word = self.word(node);
        if word & SEALED == 0 {
            return None;
        }
        let state = self.state(word & GROUP);
        (state == SUSPECT || (self.second_claims && state == CHECKING))
            .then_some((word & GROUP) as u16)
    }

    /// Plants or takes out [`Plant::SecondClaimAdmitted`].
    #[cfg(test)]
    pub(crate) fn admit_second_claims(&mut self, admit: bool) {
        self.second_claims = admit;
    }

    /// Plants or takes out [`Plant::LetGoRowsNotAssumed`].
    #[cfg(test)]
    pub(crate) fn forget_assumed(&mut self, forget: bool) {
        self.forgets_assumed = forget;
    }

    /// Whether a group is suspect, being checked or broken.
    #[inline]
    pub(crate) fn unsettled(&self) -> bool {
        self.unsettled.load(Ordering::Acquire) != 0
    }

    /// Whether a walk's reach may prove the row of `node` unchanged
    /// ([`super::frontier::unreached`]). The reach follows the reverse edges and does not
    /// enter a sealed group, so a row that reads through a group that waits for its check is
    /// outside the reach whether or not the write reaches it. While any group is unsettled,
    /// the reach proves only a row of a group, not loose, with no unsettled group below it:
    /// what such a row reads is its group's and the groups' below (the module's rule), and
    /// every one of those is held, or open and so in the index.
    ///
    /// A row of a group that is not open is no group's for this proof (end-state.md, rule 3
    /// of the rebuild): one behind the seal is its group's to answer for, and one made after
    /// the seal is in neither the seal nor, while the reach does not enter the group, the
    /// reach. Until 2026-11-13 such a row was proved as a row of its group, and a walk whose
    /// reach was worked out before the group's seal broke proved it with its old value (the
    /// third stale answer the refinement found, [`Plant::ReachBeforeSealsChecked`]).
    #[inline]
    pub(crate) fn reach_proves(&self, storage: &Storage, node: u32) -> bool {
        if self.unsettled.load(Ordering::Acquire) == 0 {
            return true;
        }
        let word = self.word(node);
        let group = word & GROUP;
        if group == 0 || loose(storage.summary_of(node)) {
            return false;
        }
        let of = &self.groups[group as usize];
        let open = of.state.load(Ordering::Acquire) == OPEN;
        let planted = storage.frontier.plant == Plant::ReachBeforeSealsChecked;
        if !open && (word & SEALED != 0 || !planted) {
            return false;
        }
        !of.shadow.load(Ordering::Acquire)
    }

    /// The group that covers the groups of `edges`' nodes, and whether none does: the one
    /// that depends on every other, 0 when they name no group.
    fn join_of(&self, edges: &[u32]) -> (u32, bool) {
        let (mut join, mut mixed) = (0u32, false);
        for &edge in edges {
            let group = self.word(edge) & GROUP;
            if group == 0 || group == join {
                continue;
            }
            if join == 0 || self.depends(group, join) {
                join = group;
            } else if !self.depends(join, group) {
                mixed = true;
            }
        }
        (if mixed { 0 } else { join }, mixed)
    }

    /// Gives the row of `node`, whose run ended with `edges` and the summary `summary`, its
    /// group, and returns the summary to store: with [`LOOSE`] when no one group covers what
    /// it read, or when its group moved where its readers' groups may not follow. `made`: the
    /// row had run before. Called by the row's flight holder, before it stores the summary.
    #[inline]
    pub(crate) fn derived(
        &self,
        storage: &Storage,
        node: u32,
        edges: &[u32],
        summary: Summary,
        made: bool,
    ) -> Summary {
        if !self.declared() {
            return summary;
        }
        let (group, mixed) = self.join_of(edges);
        let summary = if mixed { or(summary, LOOSE) } else { summary };
        let old = self.word(node);
        let summary = self.placed_as(storage, node, old, group, summary, made);
        if group != old & GROUP && group != 0 {
            self.moved.push(node); // alloc: a word a row whose group changed, for the next pass
        }
        summary
    }

    /// [`Seals::derived`] for the fields of a tracked struct being made: the summary to give
    /// them and their group, from what their creator read so far.
    pub(crate) fn derived_field(&self, summary: Summary, edges: &[u32]) -> (Summary, u32) {
        if !self.declared() {
            return (summary, 0);
        }
        let (group, mixed) = self.join_of(edges);
        (if mixed { or(summary, LOOSE) } else { summary }, group)
    }

    /// Gives a tracked field's node its group ([`Seals::derived_field`]'s) and returns the
    /// summary to store for it. `made`: the field was there before.
    pub(crate) fn placed(
        &self,
        storage: &Storage,
        node: u32,
        group: u32,
        summary: Summary,
        made: bool,
    ) -> Summary {
        if !self.declared() {
            return summary;
        }
        self.placed_as(storage, node, self.word(node), group, summary, made)
    }

    /// Stores `node`'s word for group `group` where it was `old`, and returns its summary.
    ///
    /// The poison rule: a node that had no [`LOOSE`] was read by rows as one that reads only
    /// its old group and the groups below it. Its new reads are covered by that only when the
    /// new group is the old one or one the old one depends on (or it reads no group at all).
    /// Otherwise it is loose from now on: the bit is a gain, which reaches every reader and
    /// opens every sealed group in the way.
    fn placed_as(
        &self,
        storage: &Storage,
        node: u32,
        old: u32,
        group: u32,
        summary: Summary,
        made: bool,
    ) -> Summary {
        let was = old & GROUP;
        let mut poisoned = old & POISONED;
        if poisoned == 0
            && made
            && group != 0
            && group != was
            && !loose(storage.summary_of(node))
            && !(was != 0 && self.depends(was, group))
        {
            poisoned = POISONED;
        }
        // A row that ran again in its group stays behind its seal (a value let go and made
        // again: the same reads); one that changed group is no row of the old seal.
        let sealed = if group == was { old & SEALED } else { 0 };
        let word = group | poisoned | sealed | (old & IMPORTED);
        if word != old {
            self.set_word(node, word);
        }
        if poisoned != 0 { or(summary, LOOSE) } else { summary }
    }

    /// Whether the row of `node`, whose run's edges a fold was about to add, is behind a
    /// seal: its edges are not added, and its group is noted as not in the index.
    pub(crate) fn unindex_if_sealed(&mut self, node: u32) -> bool {
        if !self.sealed_member(node) {
            return false;
        }
        let group = self.word(node) & GROUP;
        self.groups[group as usize].indexed = false;
        true
    }

    /// The reverse index was made without the rows behind seals.
    pub(crate) fn unindexed_all(&mut self) {
        for group in &mut self.groups[1..] {
            if *group.state.get_mut() != OPEN {
                group.indexed = false;
            }
        }
    }

    /// Passes each group's stamp through the views' renumbering.
    pub(crate) fn restamp(&mut self, restamp: &mut Restamp<'_>) {
        for group in &mut self.groups[1..] {
            group.sealed_at = restamp.apply(group.sealed_at);
        }
    }

    /// Makes, for each broken seal that has none, the group's own reverse index from the
    /// edges of the rows behind it: called by the walk (one walker at a time, its flight), so
    /// a group is indexed once. Costs the group's rows and their
    /// edges, loads only: no row is verified or run.
    pub(crate) fn index_broken(&self, storage: &Storage) {
        for (number, group) in self.groups.iter().enumerate().skip(1) {
            if group.state.load(Ordering::Acquire) != BROKEN
                || !group.overlay.load(Ordering::Acquire).is_null()
            {
                continue;
            }
            // Named: what a broken seal costs before any row of it is looked at (rule 16).
            let span = tracing::info_span!(
                target: "blaze",
                "seal group index broken",
                group = %group.name,
                rows = group.members.len(),
                edges = tracing::field::Empty,
            );
            let _entered = span.enter();
            let mut reverse = Box::new(Reverse::default()); // alloc: a broken group's index, until it opens
            let mut seen = Seen::with_hasher(blaze_hash::FixedState);
            let mut edges: Vec<u32> = Vec::new(); // alloc: scratch of one row's edges
            let mut read = 0u64;
            for &node in &group.members {
                let word = self.word(node);
                if word & SEALED == 0 || (word & GROUP) as usize != number {
                    continue;
                }
                let Some((header, _, _)) = frontier::row_of(storage, node) else { continue };
                storage.copy_edges(header.edges.load(Ordering::Acquire), &mut edges);
                read += edges.len() as u64;
                reverse.stored(node, &[], &edges, &mut seen);
            }
            reverse.pack();
            span.record("edges", read);
            // Release: a walk that loads the pointer sees the index whole.
            group.overlay.store(Box::into_raw(reverse), Ordering::Release);
        }
    }

    /// The groups whose seal is broken and whose rows are indexed ([`Seals::index_broken`]).
    pub(crate) fn broken(&self) -> Vec<u16> {
        self.groups
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, group)| {
                group.state.load(Ordering::Acquire) == BROKEN
                    && !group.overlay.load(Ordering::Acquire).is_null()
            })
            .map(|(number, _)| number as u16)
            .collect() // alloc: the groups whose seal broke in this view, few
    }

    /// Gives `each` the rows behind a broken seal that read `dep`, from the groups' own
    /// indexes ([`Seals::index_broken`]); returns how many. One load for a node that is
    /// neither behind a seal nor imported by one.
    pub(crate) fn readers_in_broken(&self, dep: u32, mut each: impl FnMut(u32)) -> u64 {
        if self.unsettled.load(Ordering::Acquire) == 0 {
            return 0;
        }
        let word = self.word(dep);
        let of = |group: &Group, each: &mut dyn FnMut(u32)| -> u64 { // dyn: one of two callers' closures, on a walk's cold path
            if group.state.load(Ordering::Acquire) != BROKEN {
                return 0;
            }
            let overlay = group.overlay.load(Ordering::Acquire);
            if overlay.is_null() {
                return 0;
            }
            // SAFETY: a non-null overlay is a live `Box<Reverse>` made by `index_broken` and
            // freed only by `seal_open` or the storage's drop, both under `&mut`, which this
            // borrow of the storage excludes; it is never written after it is published.
            unsafe { &*overlay }.readers(dep, each)
        };
        let mut found = 0;
        // A row behind a seal is read, inside its group, only by rows of that group.
        if word & SEALED != 0 {
            found += of(&self.groups[(word & GROUP) as usize], &mut each);
        }
        // And from outside, by the groups that import it.
        if word & IMPORTED != 0 {
            for (number, group) in self.groups.iter().enumerate().skip(1) {
                if word & SEALED != 0 && number == (word & GROUP) as usize {
                    continue;
                }
                found += of(group, &mut each);
            }
        }
        found
    }

    /// Every row behind the seal of `group` that is made and not up to date in this view,
    /// with its `verified` word, pushed onto `out`: what a walk named before 2026-11-12, kept
    /// for the control that shows the difference ([`Plant::BrokenSealNamesEveryRow`]).
    pub(crate) fn every_row_behind(&self, storage: &Storage, group: u16, out: &mut Vec<(u32, u32)>) {
        let view = storage.view_bits();
        for &node in &self.groups[group as usize].members {
            let word = self.word(node);
            if word & SEALED == 0 || word & GROUP != u32::from(group) {
                continue;
            }
            let Some((header, _, _)) = frontier::row_of(storage, node) else { continue };
            let verified = header.verified.load(Ordering::Acquire);
            if verified & (VIEW_MASK | RUNNING | PROVISIONAL) == 0 {
                continue;
            }
            if verified & (RUNNING | PROVISIONAL) == 0 && verified & VIEW_MASK == view {
                continue;
            }
            out.push((node, verified)); // alloc: the control's list
        }
    }

    /// Sets each group's `shadow`: whether a group it depends on is unsettled. Under `&mut`,
    /// after any group became suspect; a reader that settles a group leaves it set, which
    /// only costs a proof.
    fn shadow(&mut self) {
        let mut unsettled: Vec<u64> = Vec::new(); // alloc: a bit a group, at a write that reached a seal
        for (number, group) in self.groups.iter_mut().enumerate().skip(1) {
            if matches!(*group.state.get_mut(), SUSPECT | CHECKING | BROKEN) {
                set(&mut unsettled, number);
            }
        }
        for group in &mut self.groups[1..] {
            let under = group.below.iter().zip(&unsettled).any(|(below, it)| below & it != 0);
            *group.shadow.get_mut() = under;
        }
    }
}

/// What [`Storage::seal_quiet_groups`] did.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct SealPass {
    /// Groups it sealed.
    pub sealed: u32,
    /// Sealed groups it put more rows behind the seal of.
    pub added_to: u32,
    /// Rows it put behind seals.
    pub rows: u64,
    /// Rows of the groups it looked at that were not current, and stay outside their seal.
    pub excluded: u64,
    /// What the groups it sealed or added to read from outside themselves.
    pub imports: u64,
    /// Forward edges it let go, of the rows it put behind a library's seal.
    pub let_go: u64,
    /// Open groups with rows that are not quiet long enough yet.
    pub waiting: u32,
    /// Whether it made the reverse index again without the rows behind seals, and the
    /// readers the index held before and after.
    pub shed: Option<(u64, u64)>,
}

/// Why a group's imports are not every part its rows' values rest on
/// ([`Storage::seal_group_whole`]): how many of its rows are each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SealNotWhole {
    /// Rows whose last run was answered from a store by a key.
    pub stored: u64,
    /// Rows emptied and not made again.
    pub emptied: u64,
    /// Rows whose last run made an untracked read.
    pub untracked: u64,
}

impl std::fmt::Display for SealNotWhole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} rows answered from a store by a key, {} rows emptied and not made again, {} \
             rows with an untracked read",
            self.stored, self.emptied, self.untracked
        )
    }
}

/// One group in a [`SealCensus`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SealGroupCensus {
    pub group: SealGroup,
    pub name: String,
    pub state: SealState,
    pub kind: SealKind,
    /// Rows whose reads place them in the group.
    pub rows: u64,
    /// Of them, rows behind its seal (0 while it is open).
    pub behind_seal: u64,
    /// What the rows behind its seal read from outside it.
    pub imports: u64,
    /// The edges of the rows behind its seal, kept.
    pub edges: u64,
    /// The edges its rows let go of as they went behind its seal (a library's,
    /// [`SealKind::lets_go_edges`]): no longer in the edge array, 4 bytes each freed once
    /// the edges are compacted.
    pub edges_let_go: u64,
    /// Whether the reverse index holds the rows behind its seal as readers.
    pub indexed: bool,
    /// How many times it was opened, and one.
    pub generation: u32,
}

/// What [`Storage::seal_census`] found: the rows by group and state.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct SealCensus {
    pub groups: Vec<SealGroupCensus>,
    /// Function rows that ran and are of no group.
    pub ungrouped: u64,
    /// Checks of a suspect group, seals broken and groups opened, over the storage's life.
    pub checks: u64,
    pub breaks: u64,
    pub opens: u64,
}

impl SealCensus {
    /// Groups sealed: held or waiting for their check.
    pub fn sealed_groups(&self) -> u64 {
        self.groups
            .iter()
            .filter(|it| matches!(it.state, SealState::Sealed | SealState::Suspect))
            .count() as u64
    }

    /// Rows behind a seal.
    pub fn rows_behind_seals(&self) -> u64 {
        self.groups.iter().map(|it| it.behind_seal).sum()
    }

    /// The forward edges of the rows behind seals: 4 bytes each in the edge array, kept so
    /// an opened group's rows verify with no run.
    pub fn edges_behind_seals(&self) -> u64 {
        self.groups.iter().filter(|it| it.state != SealState::Open).map(|it| it.edges).sum()
    }

    /// The forward edges let go by the rows behind seals ([`SealGroupCensus::edges_let_go`]).
    pub fn edges_let_go(&self) -> u64 {
        self.groups.iter().filter(|it| it.state != SealState::Open).map(|it| it.edges_let_go).sum()
    }

    /// Of the edges behind seals, kept or let go, those of groups whose rows the reverse
    /// index still names as readers (not yet made again without them): as many readers there.
    pub fn edges_behind_seals_indexed(&self) -> u64 {
        self.groups
            .iter()
            .filter(|it| it.state != SealState::Open && it.indexed)
            .map(|it| it.edges + it.edges_let_go)
            .sum()
    }

    /// Rows that are of some group.
    pub fn grouped_rows(&self) -> u64 {
        self.groups.iter().map(|it| it.rows).sum()
    }
}

impl std::fmt::Display for SealCensus {
    /// One line of totals, then the twenty groups of most rows, a line each.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = |state: SealState| self.groups.iter().filter(|it| it.state == state).count();
        writeln!(
            f,
            "seal groups: {} of {} sealed ({} waiting for a check, {} broken), {} rows behind \
             seals ({} forward edges kept, {} let go, {} still in the reverse index) of {} \
             in groups, {} rows in no group; {} checks, {} seals broken, {} opened",
            self.sealed_groups(),
            self.groups.len(),
            count(SealState::Suspect),
            count(SealState::Broken),
            self.rows_behind_seals(),
            self.edges_behind_seals(),
            self.edges_let_go(),
            self.edges_behind_seals_indexed(),
            self.grouped_rows(),
            self.ungrouped,
            self.checks,
            self.breaks,
            self.opens,
        )?;
        let mut most: Vec<&SealGroupCensus> = self.groups.iter().collect();
        most.sort_by(|a, b| (b.rows, &a.name).cmp(&(a.rows, &b.name)));
        for it in most.into_iter().take(20) {
            writeln!(
                f,
                "  group {} ({:?}, {:?}): {} rows, {} behind its seal ({} edges, {} let go), {} \
                 imports{}",
                it.name,
                it.kind,
                it.state,
                it.rows,
                it.behind_seal,
                it.edges,
                it.edges_let_go,
                it.imports,
                if it.indexed { "" } else { ", no reverse edges" },
            )?;
        }
        Ok(())
    }
}

impl Storage {
    /// Makes a seal group named `name` (a crate's name, for the census and spans) that
    /// depends on `deps`. With `&mut`: a host makes a crate's group as it makes the crate,
    /// before a row of it runs.
    ///
    /// # Panics
    ///
    /// When the storage has 65,535 groups, saying so.
    pub fn seal_group(&mut self, name: &str, kind: SealKind, deps: &[SealGroup]) -> SealGroup {
        let number = self.seals.groups.len();
        assert!(
            number < GROUP as usize,
            "{}",
            blaze_report::say!(
                "the query database ran out of seal groups (",
                blaze_report::say::Count(u64::from(GROUP)),
                ") making one for ",
                blaze_report::say::Code(name),
            )
            .flat()
        );
        self.seals.groups.push(Group::new(name, kind)); // alloc: a group, once a crate
        let group = SealGroup(number as u16);
        self.seal_group_depends(group, deps);
        group
    }

    /// Adds `deps` to what `group` depends on. Dependencies are only added: a row that ran
    /// under a dependency stays covered by it, so a crate that drops one keeps the edge here,
    /// which costs a check and never an answer.
    pub fn seal_group_depends(&mut self, group: SealGroup, deps: &[SealGroup]) {
        let at = self.seal_number(group);
        let seals = &mut self.seals;
        let mut below = std::mem::take(&mut seals.groups[at].below);
        for dep in deps {
            let dep = dep.0 as usize;
            assert!(
                dep != 0 && dep < seals.groups.len(),
                "{}",
                blaze_report::say!(
                    "seal group ",
                    blaze_report::say::Count(dep as u64),
                    ", given as a dependency of ",
                    blaze_report::say::Code(&seals.groups[at].name),
                    ", is no group of this storage",
                )
                .flat()
            );
            if dep == at {
                continue;
            }
            set(&mut below, dep);
            or_into(&mut below, &seals.groups[dep].below);
        }
        // Not below itself, whatever cycle the host's graph has.
        if has(&below, at) {
            below[at / 64] &= !(1 << (at % 64));
        }
        // Every group above it depends on what it depends on now.
        for other in 1..seals.groups.len() {
            if other != at && has(&seals.groups[other].below, at) {
                let above = &mut seals.groups[other].below;
                or_into(above, &below);
                if has(above, other) {
                    above[other / 64] &= !(1 << (other % 64));
                }
            }
        }
        seals.groups[at].below = below;
    }

    /// Says a write to an input of `group` opens `with` too, and that `with` depends on
    /// `group`: for a group of inputs several crates are made of (a package's files, read by
    /// the crate and its test twin), so an edit of a file opens each crate made of it at the
    /// write, with its rows indexed, where a check would break its seal and name every row.
    pub fn seal_group_opens_with(&mut self, group: SealGroup, with: SealGroup) {
        let (at, other) = (self.seal_number(group), self.seal_number(with));
        if at == other {
            return;
        }
        let opened = &mut self.seals.groups[at].with;
        if !opened.contains(&with.0) {
            opened.push(with.0); // alloc: a group's list, once a pair
        }
        self.seal_group_depends(with, &[group]);
    }

    fn seal_number(&self, group: SealGroup) -> usize {
        let at = group.0 as usize;
        assert!(
            at != 0 && at < self.seals.groups.len(),
            "seal group {at} is no group of this storage, which has {}",
            self.seals.groups.len() - 1
        );
        at
    }

    /// What `group` is in now.
    pub fn seal_group_state(&self, group: SealGroup) -> SealState {
        match self.seals.state(self.seal_number(group) as u32) {
            OPEN => SealState::Open,
            HELD => SealState::Sealed,
            SUSPECT | CHECKING => SealState::Suspect,
            _ => SealState::Broken,
        }
    }

    /// Groups opened over the storage's life: by a write to their inputs, a broken seal, or
    /// a summary's gain.
    pub fn seal_opens(&self) -> u64 {
        self.seals.opens
    }

    /// Field node `node` (of input `name`) is an input of `group`.
    ///
    /// A field that is an input of another group moves: a removal from that group and an
    /// addition to this one. The rows that read it were placed, and proved, as rows that read
    /// an input of the old group, so the move makes the field loose first: the old group is
    /// opened, the bit is a gain that reaches every row that reads the field at any depth
    /// and opens every sealed group in the way ([`Storage::complete_summaries`], run here),
    /// and from then on those rows are covered by their summaries, as readers of an input of
    /// no group are. Only then is the field given its new group, for the rows that read it
    /// from here on. The cost is the old group's rows and the field's readers; a move is a
    /// file changing its source root, not an edit.
    pub(crate) fn seal_input(&mut self, node: u32, group: SealGroup, name: &str) {
        let at = self.seal_number(group) as u32;
        let old = self.seals.word(node);
        if old & INPUT != 0 {
            let from = old & GROUP;
            if from == at {
                return;
            }
            if self.frontier.plant == Plant::InputMovesWithoutRemoval {
                self.seals.set_word(node, at | INPUT | (old & IMPORTED));
                return;
            }
            tracing::info!(
                target: "blaze",
                input = name,
                from = %self.seals.groups[from as usize].name,
                to = %self.seals.groups[at as usize].name,
                "an input moved to another seal group: removed from the one, added to the other"
            );
            self.seals.groups[from as usize].written = true;
            self.seals.groups[at as usize].written = true;
            self.seal_open(from as u16, "an input of it moved to another group");
            self.fold_reverse();
            self.grow_summary(node, LOOSE);
            self.complete_summaries();
        }
        let old = self.seals.word(node);
        self.seals.set_word(node, at | INPUT | (old & IMPORTED));
        // Its group covers it now: a row that reads it from here on is not loose by it. A
        // row that read it before keeps the bit, and is verified as before.
        self.set_summary(node, input_bits(node));
    }

    /// An input field's node was written: its group opens, with the groups that open with
    /// it, and every held group that depends on it, or reads the field loosely, becomes
    /// suspect. One load of the field's word and one loop over the groups; no row is read
    /// but those of the groups opened, to index them.
    pub(crate) fn seal_wrote(&mut self, node: u32) {
        if !self.seals.declared() {
            return;
        }
        let plant = self.frontier.plant;
        let word = self.seals.word(node);
        let own = if word & INPUT != 0 { (word & GROUP) as usize } else { 0 };
        if own != 0 {
            self.seals.groups[own].written = true;
            for at in 0..self.seals.groups[own].with.len() {
                let with = self.seals.groups[own].with[at] as usize;
                self.seals.groups[with].written = true;
            }
        }
        if self.seals.sealed == 0 || plant == Plant::SealSurvivesAWrite {
            return;
        }
        let bits = input_bits(node);
        let mut opened: Vec<u16> = Vec::new(); // alloc: the groups one write opens, a few
        let mut reached = false;
        for at in 1..self.seals.groups.len() {
            let state = *self.seals.groups[at].state.get_mut();
            if state == OPEN {
                continue;
            }
            if own != 0 && (at == own || self.seals.groups[own].with.contains(&(at as u16))) {
                opened.push(at as u16); // alloc: as above
                continue;
            }
            if state != HELD {
                continue;
            }
            let group = &mut self.seals.groups[at];
            let below = own != 0
                && has(&group.below, own)
                && plant != Plant::SealedDependentsNeverSuspect;
            let wild = (group.wild[0] & bits[0]) | (group.wild[1] & bits[1]) != 0;
            if below || wild {
                *group.state.get_mut() = SUSPECT;
                *self.seals.unsettled.get_mut() += 1;
                reached = true;
            }
        }
        for &group in &opened {
            self.seal_open(group, "a write to its inputs");
        }
        if reached {
            self.seals.shadow();
        }
    }

    /// A write no field names ([`Storage::synthetic_write`]): every held group with a loose
    /// import becomes suspect. A group without one reads only inputs of groups, which the
    /// write did not touch.
    pub(crate) fn seal_wrote_all(&mut self) {
        if self.seals.sealed == 0 || self.frontier.plant == Plant::SealSurvivesAWrite {
            return;
        }
        let mut reached = false;
        for at in 1..self.seals.groups.len() {
            let group = &mut self.seals.groups[at];
            if *group.state.get_mut() == HELD && group.wild != [0; 2] {
                *group.state.get_mut() = SUSPECT;
                *self.seals.unsettled.get_mut() += 1;
                reached = true;
            }
        }
        if reached {
            self.seals.shadow();
        }
    }

    /// Opens group `group` (`why`, for its span): its rows are ordinary rows again, named by
    /// the reverse index as readers. Costs the rows behind its seal and their edges when the
    /// index was made without them, a move of its row list otherwise; never a row of another
    /// group.
    pub(crate) fn seal_open(&mut self, group: u16, why: &'static str) {
        let at = group as usize;
        let state = *self.seals.groups[at].state.get_mut();
        if state == OPEN {
            return;
        }
        let members = std::mem::take(&mut self.seals.groups[at].members);
        let indexed = self.seals.groups[at].indexed;
        // Named, so the cost of an unseal is its own on the write's path (rule 16).
        let span = tracing::info_span!(
            target: "blaze",
            "seal group open",
            group = %self.seals.groups[at].name,
            why,
            rows = members.len(),
            indexed,
            edges = tracing::field::Empty,
        );
        let _entered = span.enter();
        if !indexed {
            span.record("edges", self.index_rows(&members));
        }
        // Its rows that let go of their edges are reached by no reverse edge: the next walk
        // starts from each ([`Seals::assume`]). A check that broke the seal queued them
        // already; twice costs a duplicate the walk drops.
        self.seals.assume(&self.seals.groups[at], &members);
        let seals = &mut self.seals;
        let of = &mut seals.groups[at];
        *of.state.get_mut() = OPEN;
        of.indexed = true;
        of.generation += 1;
        of.quiet = 0;
        of.written = true;
        of.imports = Vec::new();
        of.runs = Vec::new();
        of.wild = [0; 2];
        of.edges = 0;
        of.let_go = 0;
        let overlay = std::mem::replace(of.overlay.get_mut(), std::ptr::null_mut());
        if !overlay.is_null() {
            // SAFETY: from `Box::into_raw` in `Seals::index_broken`, taken out here under
            // `&mut` of the storage: no walk reads it, and this is its one owner.
            drop(unsafe { Box::from_raw(overlay) });
        }
        if of.pending.is_empty() {
            of.pending = members;
        } else {
            of.pending.extend_from_slice(&members); // alloc: the group's own rows, listed once
        }
        seals.sealed -= 1;
        seals.opens += 1;
        if state != HELD {
            *seals.unsettled.get_mut() -= 1;
        }
    }

    /// Opens every group whose seal a reader broke: with `&mut`, before the reverse index is
    /// folded, settled or asked.
    pub(crate) fn seal_finalize(&mut self) {
        if !self.seals.unsettled() {
            return;
        }
        let mut any = false;
        for at in 1..self.seals.groups.len() {
            if *self.seals.groups[at].state.get_mut() == BROKEN {
                self.seal_open(at as u16, "its seal broke");
                any = true;
            }
        }
        if any {
            self.seals.shadow();
        }
    }

    /// `node`'s summary gained a bit: every sealed group that imports it, and the sealed
    /// group it is behind the seal of, is opened, so the gain reaches their rows through the
    /// index as it reaches any reader. One load for a node no sealed group ever imported.
    pub(crate) fn seal_open_for_gain(&mut self, node: u32) {
        if self.seals.sealed == 0 {
            return;
        }
        let word = self.seals.word(node);
        if word & SEALED != 0 && self.seals.state(word & GROUP) != OPEN {
            self.seal_open((word & GROUP) as u16, "a row of it gained an input");
        }
        if word & IMPORTED == 0 {
            return;
        }
        for at in 1..self.seals.groups.len() {
            let group = &mut self.seals.groups[at];
            if *group.state.get_mut() == OPEN {
                continue;
            }
            let imports = group.runs.iter().any(|run| {
                group.imports[run.start as usize..run.end as usize].binary_search(&node).is_ok()
            });
            if imports {
                self.seal_open(at as u16, "an import of it gained an input");
            }
        }
    }

    /// Whether the row of `node` may go behind a seal now: it ran, made no untracked read,
    /// and is current (verified in this view, or proved unchanged with no look at its edges).
    fn sealable(&self, node: u32) -> bool {
        let Some((header, _, _)) = frontier::row_of(self, node) else { return false };
        let word = header.verified.load(Ordering::Relaxed);
        let meta = header.meta.load(Ordering::Relaxed);
        if word & (RUNNING | PROVISIONAL) != 0 || word & VIEW_MASK == 0 || meta & UNTRACKED != 0 {
            return false;
        }
        // A row whose edges an earlier pass let go and that has not run since (proved by its
        // summary, say): what it reads is not known, so no import of the seal would stand
        // for it. It goes behind the seal once it has run and has edges again.
        if self.seals.let_go(node) {
            return false;
        }
        word & VIEW_MASK == self.view_bits()
            || self.holds(node, self.node(node), (meta & DURABILITY) as u8, word & VIEW_MASK)
    }

    /// The seal pass: what a host calls once at each build's end, after its answer and after
    /// it settled the changed set ([`Storage::settle_changed_set`]), with `&mut`.
    ///
    /// Each open group that has been quiet long enough ([`SealKind`]) is sealed: its current
    /// rows go behind the seal and what they read from outside is listed. A sealed group that
    /// holds takes the rows that ran in it since. Then, when the readers behind seals are
    /// half the reverse index, the index is made again without them.
    ///
    /// Cost: the rows not yet behind a seal of the groups it seals or adds to, and their
    /// edges; the groups it leaves alone cost one compare each. The remaking of the index, when
    /// it happens, costs the edges of the rows outside seals.
    pub fn seal_quiet_groups(&mut self) -> SealPass {
        let mut pass = SealPass::default();
        if !self.seals.declared() {
            return pass;
        }
        let span = tracing::info_span!(
            target: "blaze",
            "seal groups",
            view = self.view_bits(),
            sealed = tracing::field::Empty,
            added_to = tracing::field::Empty,
            rows = tracing::field::Empty,
            excluded = tracing::field::Empty,
            imports = tracing::field::Empty,
            waiting = tracing::field::Empty,
        );
        let _entered = span.enter();
        // Broken seals come off, the runs since the last boundary are in the index, and
        // every summary holds what its row reads now.
        self.fold_reverse();
        self.complete_summaries();
        self.seal_list_moved();
        let view = self.view_bits();
        // The groups this pass seals (open, quiet) or adds to (held, with rows outside).
        let mut chosen: Vec<(u16, bool)> = Vec::new(); // alloc: the pass's list, a word a group
        for at in 1..self.seals.groups.len() {
            let group = &mut self.seals.groups[at];
            if std::mem::take(&mut group.written) {
                group.quiet = 0;
            } else {
                group.quiet = group.quiet.saturating_add(1);
            }
            let quiet = group.quiet >= group.kind.quiet_passes();
            match *group.state.get_mut() {
                OPEN if group.pending.is_empty() => {}
                OPEN if quiet => chosen.push((at as u16, false)), // alloc: as above
                OPEN => pass.waiting += 1,
                HELD if !group.pending.is_empty() => chosen.push((at as u16, true)), // alloc: as above
                _ => {}
            }
        }
        // First every chosen group's rows go behind its seal, so what one chosen group reads
        // of another is seen as behind that one's seal.
        let mut fresh: Vec<Vec<u32>> = Vec::with_capacity(chosen.len()); // alloc: as above
        for &(number, was_held) in &chosen {
            let at = number as usize;
            let mut pending = std::mem::take(&mut self.seals.groups[at].pending);
            pending.sort_unstable();
            pending.dedup();
            let (mut behind, mut left): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new()); // alloc: the group's own rows
            for &node in &pending {
                let word = self.seals.word(node);
                if word & GROUP != u32::from(number) {
                    continue;
                }
                if was_held && word & SEALED != 0 {
                    continue;
                }
                if self.sealable(node) {
                    self.seals.set_word(node, word | SEALED);
                    behind.push(node); // alloc: as above
                } else {
                    self.seals.set_word(node, word & !SEALED);
                    left.push(node); // alloc: as above
                }
            }
            pass.excluded += left.len() as u64;
            let group = &mut self.seals.groups[at];
            group.pending = left;
            if !behind.is_empty() && !was_held {
                *group.state.get_mut() = HELD;
                self.seals.sealed += 1;
                pass.sealed += 1;
            } else if !behind.is_empty() {
                pass.added_to += 1;
            }
            pass.rows += behind.len() as u64;
            fresh.push(behind); // alloc: as above
        }
        // Then each group's imports: what its new rows read that is not behind its seal and
        // not its own input, sorted by source group and by whether it is behind that seal.
        for (&(number, _), behind) in chosen.iter().zip(fresh) {
            if behind.is_empty() {
                continue;
            }
            let at = number as usize;
            let own = u32::from(number);
            let inside = |word: u32| word & GROUP == own && word & (SEALED | INPUT) != 0;
            let mut imports = std::mem::take(&mut self.seals.groups[at].imports);
            let mut edges = 0u64;
            for &node in &behind {
                let Some((header, _, _)) = frontier::row_of(self, node) else { continue };
                let read = header.edges.load(Ordering::Relaxed);
                let (start, len) = (span::start(read), span::len(read));
                edges += len;
                for edge in start..start + len {
                    let read = self.edge(edge);
                    if !inside(self.seals.word(read)) {
                        imports.push(read); // alloc: the group's imports, what it reads from outside
                    }
                }
            }
            imports.sort_unstable();
            imports.dedup();
            // What was an import of the seal may be behind it now.
            imports.retain(|&read| !inside(self.seals.word(read)));
            let mut keyed: Vec<(u32, u32)> = imports // alloc: scratch of the group's imports
                .iter()
                .map(|&read| {
                    let word = self.seals.word(read);
                    let source = word & GROUP;
                    let behind_seal =
                        word & SEALED != 0 && source != 0 && self.seals.state(source) != OPEN;
                    ((source << 1) | u32::from(behind_seal), read)
                })
                .collect();
            keyed.sort_unstable();
            let mut runs: Vec<Run> = Vec::new(); // alloc: a run a source group
            let mut wild = [0; 2];
            for (place, &(class, read)) in keyed.iter().enumerate() {
                if runs.last().is_none_or(|run| run.class != class) {
                    let source = class >> 1;
                    let generation = match class & 1 != 0 && self.seals.state(source) == HELD {
                        true => self.seals.groups[source as usize].generation,
                        false => 0,
                    };
                    runs.push(Run {
                        class,
                        start: place as u32,
                        end: place as u32,
                        generation: AtomicU32::new(generation),
                    }); // alloc: as above
                }
                if let Some(run) = runs.last_mut() {
                    run.end = place as u32 + 1;
                }
                let source = class >> 1;
                let summary = self.summary_of(read);
                if loose(summary) || source == 0 || (source != own && !self.seals.depends(own, source)) {
                    wild = or(wild, summary);
                }
                self.seals.words.ensure(read as usize + 1);
                self.seals.words.get(read as usize).fetch_or(IMPORTED, Ordering::Relaxed);
            }
            imports.clear();
            imports.extend(keyed.iter().map(|it| it.1)); // alloc: within what `imports` held
            imports.shrink_to_fit();
            pass.imports += imports.len() as u64;
            // A library's rows let go of their forward edges now that its imports are listed
            // (memory.md 4.4: edges of a sealed group are not resident). A cycle's rows keep
            // theirs (a member's one edge is its head, which the cycle's own rules read), and
            // so do rows answered from a store or with an untracked read (never behind a seal).
            let mut let_go = 0u64;
            if self.seals.groups[at].kind.lets_go_edges()
                && self.frontier.plant != Plant::WalkEntersSealedGroups
            {
                for &node in &behind {
                    let Some((header, _, _)) = frontier::row_of(self, node) else { continue };
                    if header.meta.load(Ordering::Relaxed) & (CYCLE | HEAD | STORED | UNTRACKED) != 0 {
                        continue;
                    }
                    let kept = header.edges.swap(0, Ordering::Relaxed);
                    let_go += span::len(kept);
                    self.forget_edges(kept);
                    let word = self.seals.word(node);
                    self.seals.set_word(node, word | LET_GO);
                    self.seals.let_go_rows += 1;
                }
                pass.let_go += let_go;
            }
            let group = &mut self.seals.groups[at];
            if group.members.is_empty() {
                group.members = behind;
            } else {
                group.members.extend_from_slice(&behind); // alloc: the rows behind the seal, a word each
            }
            group.imports = imports;
            group.runs = runs;
            group.wild = wild;
            group.edges += edges - let_go;
            group.let_go += let_go;
            group.sealed_at = view;
        }
        // The readers behind seals, once they are half the index, leave it: those of rows
        // that let go of their edges too, which the index names until it is made again.
        let behind: u64 = self.seals.groups[1..]
            .iter_mut()
            .filter_map(|group| {
                (*group.state.get_mut() != OPEN && group.indexed).then_some(group.edges + group.let_go)
            })
            .sum();
        // (Not under the plant that has a walk enter sealed groups: its control needs the
        // readers there to enter by.)
        if behind >= SHED_AT
            && behind * 2 >= self.frontier.held()
            && self.frontier.plant != Plant::WalkEntersSealedGroups
        {
            pass.shed = Some(self.shed_sealed_readers());
        }
        self.seals.shadow();
        span.record("sealed", pass.sealed);
        span.record("added_to", pass.added_to);
        span.record("rows", pass.rows);
        span.record("excluded", pass.excluded);
        span.record("imports", pass.imports);
        span.record("waiting", pass.waiting);
        pass
    }

    /// Lists each row whose group changed as a run ended under its group, for a pass or a
    /// read of the group's rows.
    fn seal_list_moved(&mut self) {
        while let Some(node) = self.seals.moved.pop() {
            let group = (self.seals.word(node) & GROUP) as usize;
            if group != 0 {
                self.seals.groups[group].pending.push(node); // alloc: a word a row not behind a seal
            }
        }
    }

    /// What the rows of `group` read from outside it: every row and input field that a row
    /// of the group read and that is not of the group, each once, in node order. For a host
    /// that keys what it made of one crate by the parts of other crates it read (a unit's
    /// record): the same set the group's seal is checked against, by value where the seal
    /// goes by stamp. Works for a group that is open (a crate just made) as for a sealed one.
    ///
    /// A row is of a group by what it read, so a row keyed in this crate that read only a
    /// dependency's things is not of this group: it is named here when a row of the group
    /// read it. Reads made for the crate outside any query are no row's edges and are not
    /// here.
    ///
    /// Cost: the group's rows and their edges, no other group's. With `&mut`: no run is in
    /// flight, so the rows' edges are whole.
    pub fn seal_group_imports(&mut self, group: SealGroup) -> Vec<super::Read> {
        self.seal_group_imports_beyond(group, &[])
    }

    /// [`Storage::seal_group_imports`] without what is of the groups `own` either: for a
    /// crate whose own inputs are in more groups than its own (its source root's, which its
    /// group opens with), so what is left is other crates' alone.
    pub fn seal_group_imports_beyond(
        &mut self,
        group: SealGroup,
        own: &[SealGroup],
    ) -> Vec<super::Read> {
        let at = self.seal_number(group);
        self.seal_list_moved();
        let own_group = at as u32;
        let outside = |word: u32| {
            let of = word & GROUP;
            of != own_group && !own.iter().any(|it| u32::from(it.0) == of)
        };
        let own = own_group;
        let seals = &self.seals;
        let mut nodes: Vec<u32> = Vec::new(); // alloc: the caller's answer, the group's imports
        let of = &seals.groups[at];
        for &node in of.members.iter().chain(&of.pending) {
            if seals.word(node) & GROUP != own {
                continue;
            }
            let Some((header, _, _)) = frontier::row_of(self, node) else { continue };
            let read = header.edges.load(Ordering::Relaxed);
            let (start, len) = (span::start(read), span::len(read));
            for edge in start..start + len {
                let read = self.edge(edge);
                if outside(seals.word(read)) {
                    nodes.push(read); // alloc: as above
                }
            }
        }
        // The rows that let go of their edges read, from outside, nothing but what the seal
        // listed as its imports when they went behind it: the list stands for their edges.
        if of.let_go != 0 {
            for &read in &of.imports {
                if outside(seals.word(read)) {
                    nodes.push(read); // alloc: as above
                }
            }
        }
        nodes.sort_unstable();
        nodes.dedup();
        // alloc: the caller's answer
        nodes.into_iter().map(|node| super::capture::read_of_key(self.key_of(node))).collect()
    }

    /// Whether [`Storage::seal_group_imports`] of `group` names every part its rows' values
    /// rest on: every row of the group ran in this storage, by its own reads. `Err` says the
    /// first reason it does not, with how many rows it is true of: a row answered from a store
    /// by a key ([`super::report_stored_read`]), a row emptied and not made again (its run or
    /// its cycle unwound: no edges), a row whose last run made an untracked read.
    ///
    /// A row that made an untracked read, or a cycle's, is of no group in the first place:
    /// it is named among the imports of the group whose row read it, as a row of no group,
    /// which its reader maps to a part or refuses.
    ///
    /// Cost: the group's rows, a header each. With `&mut`, as the imports are read.
    pub fn seal_group_whole(&mut self, group: SealGroup) -> Result<(), SealNotWhole> {
        let at = self.seal_number(group);
        self.seal_list_moved();
        let own = at as u32;
        let seals = &self.seals;
        let of = &seals.groups[at];
        let (mut stored, mut emptied, mut untracked) = (0u64, 0u64, 0u64);
        for &node in of.members.iter().chain(&of.pending) {
            if seals.word(node) & GROUP != own {
                continue;
            }
            let Some((header, _, _)) = frontier::row_of(self, node) else {
                emptied += 1;
                continue;
            };
            let meta = header.meta.load(Ordering::Relaxed);
            // A row whose edges a pass let go, in a group whose import list no longer stands
            // for them (it opened since): no edges, as an emptied row.
            let shed = of.let_go == 0 && seals.let_go(node);
            if header.verified.load(Ordering::Relaxed) & VIEW_MASK == 0 || shed {
                emptied += 1;
            } else if meta & UNTRACKED != 0 {
                untracked += 1;
            } else if meta & STORED != 0 {
                stored += 1;
            }
        }
        if stored + emptied + untracked == 0 {
            return Ok(());
        }
        Err(SealNotWhole { stored, emptied, untracked })
    }

    /// The rows by seal group and state ([`SealCensus`]): a walk of every node, so it runs
    /// only when asked. Exact with no run in flight.
    pub fn seal_census(&self) -> SealCensus {
        let seals = &self.seals;
        let mut census = SealCensus {
            groups: Vec::with_capacity(seals.groups.len().saturating_sub(1)), // alloc: the census's own list
            ungrouped: 0,
            checks: seals.checks.load(Ordering::Relaxed),
            breaks: seals.breaks.load(Ordering::Relaxed),
            opens: seals.opens,
        };
        for (number, group) in seals.groups.iter().enumerate().skip(1) {
            census.groups.push(SealGroupCensus {
                group: SealGroup(number as u16),
                name: group.name.to_string(), // alloc: the census's own text
                state: self.seal_group_state(SealGroup(number as u16)),
                kind: group.kind,
                rows: 0,
                behind_seal: 0,
                imports: group.imports.len() as u64,
                edges: group.edges,
                edges_let_go: group.let_go,
                indexed: group.indexed,
                generation: group.generation,
            }); // alloc: as above
        }
        for node in 1..self.nodes_made() {
            let Some((header, _, _)) = frontier::row_of(self, node) else { continue };
            if header.verified.load(Ordering::Relaxed) & VIEW_MASK == 0 {
                continue;
            }
            let word = seals.word(node);
            let group = (word & GROUP) as usize;
            if group == 0 {
                census.ungrouped += 1;
                continue;
            }
            let of = &mut census.groups[group - 1];
            of.rows += 1;
            if word & SEALED != 0 && of.state != SealState::Open {
                of.behind_seal += 1;
            }
        }
        census
    }
}

/// Puts a group being checked back to suspect when its check unwinds (a write pending, a
/// bound that ran out): the next asker checks it again.
///
/// The invariant: a group is held only by a check that ran to its end and found every import
/// unchanged. A claim is a state of its own (`CHECKING`), which proves nothing to anyone; a
/// check that does not end gives the claim back, and one whose claim a walk took away
/// (`CHECKING` to `BROKEN`) cannot put `HELD` over it (its last step is a compare-and-swap
/// from `CHECKING`).
struct Checking<'a> {
    state: &'a AtomicU8,
    done: bool,
    /// [`Plant::ClaimKeptAcrossUnwind`].
    kept: bool,
}

impl Drop for Checking<'_> {
    fn drop(&mut self) {
        if !self.done {
            let to = if self.kept { HELD } else { SUSPECT };
            _ = self.state.compare_exchange(CHECKING, to, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

/// What a test sets on its own thread to interleave a check with what another task or a
/// writer does, at a point no query's body reaches.
#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    std::thread_local! {
        /// How many of this thread's next claims of a group unwind right after the claim, as
        /// a check does when a write is pending ([`super::Cancelled::PendingWrite`]).
        pub(crate) static UNWIND_CLAIMS: Cell<u32> = const { Cell::new(0) };
        /// Called by this thread with the group it just claimed, before its check: where a
        /// preempted checker is while another task meets the claim.
        pub(crate) static AFTER_CLAIM: Cell<Option<fn(u16)>> = const { Cell::new(None) };
        /// Called by this thread with the group whose check just ended, before the result
        /// is stored: where a preempted checker is while a walk takes its claim away.
        pub(crate) static BEFORE_RESULT: Cell<Option<fn(u16)>> = const { Cell::new(None) };
    }
}

/// Checks group `group` if it is suspect and no task is checking it: whether it is held
/// afterwards. `false` leaves the caller to verify the row it asked for by the row's own
/// edges: the seal broke, or another task (or this one, further up its stack) is checking the
/// group now. No wait: a check claims the group with one compare-and-swap.
///
/// `apart`: the walk's rows set apart, which a check takes as changed without verifying; none
/// for a pull.
pub(crate) fn resolve(db: *const (), storage: &Storage, group: u16, apart: Option<&Seen>) -> bool {
    let seals = &storage.seals;
    let of = &seals.groups[group as usize];
    if let Err(now) =
        of.state.compare_exchange(SUSPECT, CHECKING, Ordering::AcqRel, Ordering::Acquire)
    {
        // Another task's claim is not this one's: one check a group at a time.
        if !(now == CHECKING && seals.second_claims) {
            return now == HELD;
        }
    }
    let kept = storage.frontier.plant == Plant::ClaimKeptAcrossUnwind;
    let mut claim = Checking { state: &of.state, done: false, kept };
    #[cfg(test)]
    if probe::UNWIND_CLAIMS.with(|left| {
        let now = left.get();
        left.set(now.saturating_sub(1));
        now > 0
    }) {
        Cancelled::PendingWrite.throw();
    }
    #[cfg(test)]
    if let Some(stop) = probe::AFTER_CLAIM.with(std::cell::Cell::get) {
        stop(group);
    }
    // Named: the one check that stands for every row behind the seal (rule 16).
    let span = tracing::info_span!(
        target: "blaze",
        "seal group check",
        group = %of.name,
        imports = of.imports.len(),
        held = tracing::field::Empty,
    );
    let _entered = span.enter();
    let held = check(db, storage, group, apart);
    #[cfg(test)]
    if let Some(stop) = probe::BEFORE_RESULT.with(std::cell::Cell::get) {
        stop(group);
    }
    claim.done = true;
    seals.checks.fetch_add(1, Ordering::Relaxed);
    span.record("held", held);
    if !held {
        // From checking, or from broken where a walk gave up waiting for this check.
        if of.state.compare_exchange(CHECKING, BROKEN, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            // Its rows without edges are reached by no reverse edge: the walk starts from them.
            seals.assume(of, &of.members);
        }
        seals.breaks.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if storage.frontier.plant == Plant::CheckStoresHeldBlindly {
        of.state.store(HELD, Ordering::Release);
    } else if of
        .state
        .compare_exchange(CHECKING, HELD, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // A walk took the claim away and broke the seal: its verdict stands.
        return false;
    }
    seals.unsettled.fetch_sub(1, Ordering::AcqRel);
    true
}

/// The walk's part: checks every suspect group, and breaks the seal of any group another
/// task is still checking when the walk gets there (a broken seal is always sound: its rows
/// are verified one by one). When it returns no group is suspect or being checked. Returns
/// the checks run and the seals broken meanwhile, by any task.
///
/// In dependency order (end-state.md, rule 2 of the rebuild): a group is checked after every
/// group it depends on, so what its imports behind those seals say is settled before it is
/// read, and the walk calls this before any row is traversed, so no seal changes state while
/// rows are. A group's dependencies are transitive, so one that depends on another has more
/// of them: ordering by their count is a topological order.
pub(crate) fn resolve_all(db: *const (), storage: &Storage, apart: &Seen) -> (u64, u64) {
    let seals = &storage.seals;
    let before =
        (seals.checks.load(Ordering::Relaxed), seals.breaks.load(Ordering::Relaxed));
    let mut order: Vec<(u32, u16)> = (1..seals.groups.len()) // alloc: the groups waiting for their check, a word each
        .filter(|&at| seals.groups[at].state.load(Ordering::Acquire) == SUSPECT)
        .map(|at| (seals.groups[at].below.iter().map(|it| it.count_ones()).sum(), at as u16))
        .collect();
    order.sort_unstable();
    for (_, group) in order {
        if seals.groups[group as usize].state.load(Ordering::Acquire) == SUSPECT {
            resolve(db, storage, group, Some(apart));
        }
    }
    for group in &seals.groups[1..] {
        if group
            .state
            .compare_exchange(CHECKING, BROKEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            seals.assume(group, &group.members);
            seals.breaks.fetch_add(1, Ordering::Relaxed);
        }
    }
    (
        seals.checks.load(Ordering::Relaxed) - before.0,
        seals.breaks.load(Ordering::Relaxed) - before.1,
    )
}

/// Gives `each` the rows behind the broken seal of `group` that read an import that moved:
/// each import is brought up to date as the check does (the check stopped at the first that
/// had moved), and the readers of each whose stamp passed the seal's view, or that the walk
/// set apart, are given from the group's own index ([`Seals::index_broken`], which the
/// caller ran). These, and no other row of the group, are what a walk verifies for the break;
/// a row of them that changes passes the walk on to its own readers as any row does.
///
/// The invariant: a row behind the seal that this gives no reader path to holds its value.
/// It changes only by running again for a read that changed; its reads are rows behind the
/// same seal, the group's inputs (a write of one opens the group, it does not break it), and
/// imports; so the first changed read on any path from it is an import that moved, whose
/// readers are given here.
///
/// Cost: the group's imports (a stamp each, a verification where one is behind) and the rows
/// given.
pub(crate) fn moved_readers(
    db: *const (),
    storage: &Storage,
    group: u16,
    apart: &Seen,
    mut each: impl FnMut(u32),
) {
    let seals = &storage.seals;
    let of = &seals.groups[group as usize];
    let view = storage.view_bits();
    let overlay = of.overlay.load(Ordering::Acquire);
    if overlay.is_null() {
        return;
    }
    // SAFETY: as in `Seals::readers_in_broken`: live until a `&mut` of the storage.
    let overlay = unsafe { &*overlay };
    for &node in &of.imports {
        let row = storage.node(node);
        let mut seen = row.color();
        let moved = if (seen >> 32) as u32 >= FIELD {
            seen as u32 > of.sealed_at
        } else if seals.proves(node) {
            verify::function_header(storage, row)
                .is_none_or(|header| header.changed.load(Ordering::Relaxed) > of.sealed_at)
        } else if apart.contains(&node) {
            true
        } else {
            if (seen >> 32) as u32 != view && up_to_date(db, storage, row) {
                seen = row.color();
            }
            (seen >> 32) as u32 != view || seen as u32 > of.sealed_at
        };
        if moved {
            overlay.readers(node, &mut each);
        }
    }
}

/// Whether no import of group `group` changed since it was sealed. A run of imports behind
/// another seal that holds, of the generation this group last saw, is passed over whole; any
/// other import is brought up to date as a reader's edge is, and its stamp compared.
fn check(db: *const (), storage: &Storage, group: u16, apart: Option<&Seen>) -> bool {
    let seals = &storage.seals;
    let of = &seals.groups[group as usize];
    let view = storage.view_bits();
    let sealed_at = of.sealed_at;
    for run in &of.runs {
        if run.behind_seal() {
            let source = &seals.groups[run.source()];
            if source.state.load(Ordering::Acquire) == SUSPECT {
                resolve(db, storage, run.source() as u16, apart);
            }
            if source.state.load(Ordering::Acquire) == HELD
                && run.generation.load(Ordering::Relaxed) == source.generation
            {
                continue;
            }
        }
        // Whether every import of the run is behind a seal that holds.
        let mut whole = run.behind_seal();
        for &node in &of.imports[run.start as usize..run.end as usize] {
            let row = storage.node(node);
            let mut seen = row.color();
            if (seen >> 32) as u32 >= FIELD {
                // A field: its stamp is the view it last changed in.
                if seen as u32 > sealed_at {
                    return false;
                }
                whole = false;
                continue;
            }
            if seals.proves(node) {
                // Behind a seal that holds: its stamp stands, whatever view its word names.
                let Some(header) = verify::function_header(storage, row) else { return false };
                if header.changed.load(Ordering::Relaxed) > sealed_at {
                    return false;
                }
                continue;
            }
            whole = false;
            if apart.is_some_and(|apart| apart.contains(&node)) {
                return false;
            }
            if (seen >> 32) as u32 != view {
                if !up_to_date(db, storage, row) {
                    return false;
                }
                seen = row.color();
                if (seen >> 32) as u32 != view {
                    return false;
                }
            }
            if seen as u32 > sealed_at {
                return false;
            }
        }
        if whole {
            let source = &seals.groups[run.source()];
            if source.state.load(Ordering::Acquire) == HELD {
                run.generation.store(source.generation, Ordering::Relaxed);
            }
        }
    }
    true
}

/// Brings the row of node `row` up to this view as a reader's edge would: whether it is
/// current now. An unwind that is not the caller's own is contained, as the walk contains
/// one: an import keyed by something an edit removed fails its run, and the seal that read it
/// is broken, where a row verified by its own edges would have met a changed edge first and
/// never asked.
fn up_to_date(db: *const (), storage: &Storage, row: &NodeRow) -> bool {
    let (index, id, _) = key_parts(row.key());
    let Some(verify) = ingredient::vtable(index).and_then(|it| it.verify) else { return false };
    // SAFETY: `db` is this storage's database as every function of it names it
    // (`Storage::check_db`): the caller's `&F::Db` (`function::refresh`) or the walk's
    // (`frontier::changed_since`), and `id` is a key of ingredient `index` (its node's key).
    match catch_unwind(AssertUnwindSafe(|| unsafe { verify(db, storage, id) })) {
        Ok(Verified::Current) => true,
        Ok(Verified::Changed | Verified::Assumed) => false,
        Err(payload) => {
            if payload.is::<Cancelled>() || payload.is::<OutOfFuel>() || payload.is::<OutOfSteps>()
            {
                resume_unwind(payload);
            }
            tracing::warn!(
                target: "blaze",
                query = ingredient::name(index),
                key = ?id,
                why = %super::unwind_text(&*payload).unwrap_or_default(),
                "a seal group's check of an import unwound: contained, the seal is broken"
            );
            false
        }
    }
}
