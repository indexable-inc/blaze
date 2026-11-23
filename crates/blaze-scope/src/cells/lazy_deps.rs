//! Names on demand ACROSS crates (engine.md piece E13, D134: lazy over what is not yours). The
//! root crates (yours) are solved whole; every other crate is a [`LazyDep`]: its groups are
//! solved only as a path walks into them, by the in-crate driver ([`super::demand`]) over the
//! one-group solve, never the crate whole.
//!
//! The seam is the dependency itself. A [`LazyDep`] answers a module's scope from the groups
//! solved so far; a read of a module it has not solved (a scope, a chain, an enum's variants or
//! a macro of a crate not walked yet) answers nothing AND is noted as wanted. A solve that
//! noted any want is void, whatever it answered (Ok or a refusal): nothing computed from a
//! missing read is kept (rule 3). The driver solves what was wanted, in that crate (first its
//! walk: the module tree and tables, from a solve with no members; then the wanted modules'
//! groups on demand), and asks the reader again. One solve notes every module it missed, so
//! a round answers all of them at once. A dependency's own wants go down the crate graph, which
//! has no cycles: the depth is bounded by the crate count, the rounds by [`MOST_ROUNDS`] (rule
//! 18), each a typed refusal naming the crate.
//!
//! What it holds after: per crate, the groups of the closure of what the root crates' solves
//! read, each record byte-equal to the same group of a whole solve (a group's record is a
//! function of the group and the answers it read: equal answers, equal bytes), whatever the
//! order (rule 25); and the root crates' names, equal to a whole solve's.
//!
//! ONE WALK PER CRATE (plan row B2): a dependency is walked whole once, when first read; the
//! walk's module tree record ([`crate::tree_record`]) is kept in its [`Held`] and handed to
//! every one-group solve of it ([`Members::tree`]), which reads, outlines and lowers its
//! members' files alone (and the modules that define macros). Which keys are modules of the
//! crate is the record's answer ([`Held::modules`]), with the modules expansions made. A solve
//! that finds the record's inputs moved walks again (the one stated path, counted in
//! [`OnDemand::walks`]); the texts of one request are fixed, so on demand it never should.
//!
//! OPEN: the walk itself on a machine that walked the crate before (a settled class's files
//! never change: its record could be read by key; the walk's tables, enums among them, are
//! what a record-only first solve lacks); impl tables are not here (a trait question needs
//! every module that holds an `impl`: an expansion of the crate whole, impl headers resolved).

use core::cell::{Cell, RefCell};

use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, BVec, Bump};
use blaze_surface::{Edition, ScopeBuilder, ScopeView};

use super::edit::{MemberRefusal, Members, OneGroup, OneGroupRefusal, Solved};
use super::groups::{NameGroup, RecordError};
use super::lazy::{DemandError, Demanded, DemandedOpts, demand};
use crate::crate_scope::Dep;
use crate::graph::{CrateGraph, CrateId, Sysroot};
use crate::prepares::Prepares;
use crate::resolve::{Built, CrateInput, CrateNames, NamesError, SourceTexts, graph_of, resolve_one};
use crate::task::{First, MacroSrc, NamesPart, Par, Serial, Variant};
use crate::tree_record::TreeRecord;

/// The most rounds one crate's wants take (each round answers every module one solve missed).
pub const MOST_ROUNDS: usize = 4096;

/// A dependency solved on demand, as one solve reads it: its head (known from its input,
/// before anything is solved), and what the solves before this one hold, frozen. Read through
/// [`Dep::Lazy`]; made by [`names_on_demand`] for each solve. A miss is told to `wants`.
///
/// Frozen, and its sink a trait object, so that it is covariant in `'a` as `Dep` and `Names`
/// must be (a `RefCell` of `'a` data would make them invariant).
pub struct LazyDep<'a> {
    pub(crate) name: &'a str,
    pub(crate) key: CrateItemKey,
    pub(crate) root: ItemKey,
    pub(crate) edition: Edition,
    pub(crate) proc_key: Option<CrateItemKey>,
    krate: u32,
    /// Every solve of the crate so far, in order: its tree and tables (empty: not walked).
    solves: &'a [&'a CrateNames<'a>],
    /// The modules of the crate's tree record, sorted ([`Held::modules`]).
    modules: &'a [ItemKey],
    /// Each solved group's members' scopes, read from the group's record.
    scopes: &'a HashMap<ItemKey, &'a ScopeBuilder<'a, &'a str>, Alloc<'a>>,
    wants: &'a (dyn WantSink + 'a),
}

impl core::fmt::Debug for LazyDep<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "LazyDep({})", self.name)
    }
}

/// Where a [`LazyDep`]'s misses go: `module` read with no scope (`None`: a table read), `walk`
/// when the crate was not walked yet.
pub(crate) trait WantSink {
    fn want(&self, krate: u32, module: Option<ItemKey>, walk: bool);
}

/// Whether `key` is a module of the crate: a row of its tree record (`modules`, sorted), or a
/// module an expansion of one of its solves made (no tree record holds those).
fn is_module_of(modules: &[ItemKey], solves: &[&CrateNames<'_>], key: ItemKey) -> bool {
    modules.binary_search(&key).is_ok() || solves.iter().any(|c| c.made.contains_key(&key))
}

impl<'a> LazyDep<'a> {
    fn is_module(&self, key: ItemKey) -> bool {
        is_module_of(self.modules, self.solves, key)
    }

    pub(crate) fn scope(&self, module: ItemKey) -> Option<&'a ScopeBuilder<'a, &'a str>> {
        if let Some(&s) = self.scopes.get(&module) {
            return Some(s);
        }
        if self.solves.is_empty() {
            self.wants.want(self.krate, Some(module), true);
        } else if self.is_module(module) {
            self.wants.want(self.krate, Some(module), false);
        }
        None
    }

    /// The first solve's answer of `f` (one text, one answer); a read before the walk is a want.
    fn first<T>(&self, f: impl Fn(&'a CrateNames<'a>) -> Option<T>) -> Option<T> {
        if self.solves.is_empty() {
            self.wants.want(self.krate, None, true);
            return None;
        }
        self.solves.iter().find_map(|c| f(c))
    }

    pub(crate) fn chain(&self, module: ItemKey) -> Option<&'a [ItemKey]> {
        self.first(|c| c.chains.get(&module).map(|v| v.as_slice()))
    }

    pub(crate) fn enum_of(&self, key: ItemKey) -> Option<&'a [Variant<&'a str>]> {
        self.first(|c| c.tables.enums.get(&key).map(|v| v.as_slice()))
    }

    pub(crate) fn macro_of(&self, key: ItemKey) -> Option<MacroSrc<'a>> {
        self.first(|c| c.tables.macros.get(&key).copied())
    }
}

/// A crate's state across solves, owned by the driver.
struct Held<'a> {
    name: &'a str,
    key: CrateItemKey,
    edition: Edition,
    proc_key: Option<CrateItemKey>,
    solves: AVec<'a, &'a CrateNames<'a>>,
    scopes: HashMap<ItemKey, &'a ScopeBuilder<'a, &'a str>, Alloc<'a>>,
    /// The groups of the crate's [`Demanded`] whose scopes are in `scopes`.
    groups_held: usize,
    /// The crate's module tree record, from its walk (or a solve that walked again): handed
    /// to every one-group solve of it. `None` until the walk.
    tree: Option<&'a [u8]>,
    /// The record's module keys, sorted: what "is this a module of the crate" asks.
    modules: &'a [ItemKey],
}

impl<'a> Held<'a> {
    fn walked(&self) -> bool {
        !self.solves.is_empty()
    }

    /// `tree` (a solve's `tree_record`) kept as the crate's record, its module keys read.
    fn take_tree(&mut self, tree: &'a [u8], bump: Bump<'a>) -> Result<(), NamesError<'a>> {
        let record = TreeRecord::read(tree, bump.alloc()).map_err(|error| NamesError::TreeRecord { krate: self.name, error })?;
        let mut modules: BVec<'a, ItemKey> = BVec::with_capacity_in(record.modules.len(), bump);
        modules.extend(record.modules.iter().map(|m| m.key));
        modules.sort_unstable();
        self.modules = modules.into_bump_slice();
        self.tree = Some(tree);
        Ok(())
    }

    /// The scopes of the groups `demanded` kept since the last call, from their records.
    fn hold(&mut self, demanded: &Demanded<'a>, bump: Bump<'a>) -> Result<(), OnDemandError<'a>> {
        let alloc = bump.alloc();
        for g in demanded.groups[self.groups_held..].iter() {
            for &m in g.members.iter() {
                let bytes = g.scope_of(m).map_err(|error| OnDemandError::Record { krate: self.name, module: m, error })?;
                let bytes = crate::resolve::aligned(bytes, bump);
                let view = ScopeView::read(bytes).map_err(|error| OnDemandError::Scope { krate: self.name, module: m, error })?;
                let scope: &'a ScopeBuilder<'a, &'a str> = bump.alloc_val(ScopeBuilder::from(&view, alloc));
                self.scopes.insert(m, scope);
            }
        }
        self.groups_held = demanded.groups.len();
        Ok(())
    }

    /// Of `modules`, those that are modules of the crate with no scope here yet.
    fn unsolved(&self, modules: &[ItemKey], alloc: Alloc<'a>) -> AVec<'a, ItemKey> {
        let mut out = AVec::new_in(alloc);
        out.extend(modules.iter().copied().filter(|&m| is_module_of(self.modules, &self.solves, m) && !self.scopes.contains_key(&m)));
        out
    }
}

/// The wants of one solve, by crate.
struct Sink<'a> {
    wants: RefCell<HashMap<u32, Want<'a>, Alloc<'a>>>,
    alloc: Alloc<'a>,
}

impl WantSink for Sink<'_> {
    fn want(&self, krate: u32, module: Option<ItemKey>, walk: bool) {
        let mut w = self.wants.borrow_mut();
        let e = w.entry(krate).or_insert_with(|| Want { krate: krate as usize, modules: AVec::new_in(self.alloc), walk: false });
        e.walk |= walk;
        if let Some(m) = module {
            e.modules.push(m);
        }
    }
}

/// What one solve read of a crate that it did not hold.
#[derive(Debug)]
pub struct Want<'a> {
    pub krate: usize,
    pub modules: AVec<'a, ItemKey>,
    /// The crate is to be walked first.
    pub walk: bool,
}

/// Why a one-group solve of a dependency answered nothing.
#[derive(Debug)]
pub enum Refused<'a> {
    Names(NamesError<'a>),
    /// It read what other crates (or this one, before its walk) do not hold yet: void.
    Wanted(AVec<'a, Want<'a>>),
}

impl super::lazy::NoRecordOf for Refused<'_> {
    fn no_record(&self) -> Option<ItemKey> {
        match self {
            Refused::Names(e) => super::lazy::NoRecordOf::no_record(e),
            Refused::Wanted(_) => None,
        }
    }
}

/// Why [`names_on_demand`] stopped, naming the crate (rule 19).
#[derive(Debug)]
pub enum OnDemandError<'a> {
    Graph(NamesError<'a>),
    /// A root crate's solve, or a dependency's walk, failed with nothing wanted.
    Names(NamesError<'a>),
    /// The in-crate driver stopped; `path`: the module path of the module the error names,
    /// from the crate's walk (rule 19), when it names one the walk holds; `source`: the path of
    /// the second item it names (a glob's source: a module's path, or for an enum the path of
    /// the module that declares it), likewise; `counts`: what the request did before it stopped.
    Demand { krate: &'a str, path: Option<&'a str>, source: Option<&'a str>, error: DemandError<OneGroupRefusal<Refused<'a>>>, counts: OnDemandCounts<'a> },
    /// A kept group's record did not give `module`'s scope.
    Record { krate: &'a str, module: ItemKey, error: RecordError },
    Scope { krate: &'a str, module: ItemKey, error: FormatError },
    /// A crate's wants took more than `most` rounds (rule 18).
    Rounds { krate: &'a str, most: usize },
    /// Wants went `most` crates deep: past the crate count, a cycle the graph cannot have.
    Depth { krate: &'a str, most: usize },
}

/// What [`names_on_demand`] answers.
pub struct OnDemand<'a> {
    /// The root crates' names, by crate index (`None`: a dependency).
    pub roots: AVec<'a, Option<&'a CrateNames<'a>>>,
    /// Each crate's groups solved on demand, by crate index (a root's: none).
    pub demanded: AVec<'a, Demanded<'a>>,
    /// Each crate's walks of its whole tree: its first walk, and every one-group solve that
    /// found the tree record's inputs moved and walked again (`NamesPart::TreeAgain`). The
    /// floor: 1 for a crate read, 0 for one never read.
    pub walks: AVec<'a, usize>,
    /// Each crate's one-group solves (members named, the walk not counted), voided ones too.
    pub one_group_solves: AVec<'a, usize>,
    /// Each crate's module tree record as the driver ended with it (`None`: never walked):
    /// what a caller with a store keeps for the crate.
    pub trees: AVec<'a, Option<&'a [u8]>>,
    /// The solves of root crates, voided ones counted.
    pub root_solves: usize,
    /// What the request did, by crate ([`OnDemandCounts`]: printed as one line).
    pub counts: OnDemandCounts<'a>,
}

/// What the driver did, by crate index (rule 16: the cost of a request, printed with its
/// answer and with its refusal): every solve, every walk, every decode of a tree record.
#[derive(Debug)]
pub struct OnDemandCounts<'a> {
    pub names: AVec<'a, &'a str>,
    /// Walks of the whole tree (the first, and each one-group solve that walked again).
    pub walks: AVec<'a, usize>,
    /// One-group solves asked, voided ones too.
    pub one_group_solves: AVec<'a, usize>,
    /// Solves (one-group or the walk) voided because they read another crate not held yet:
    /// each is asked again after the wants are answered.
    pub voided: AVec<'a, usize>,
    /// Decodes of the crate's module tree record (`TreeRecord::read`: the driver's own and
    /// each record-backed solve's), and the bytes they decoded.
    pub tree_reads: AVec<'a, usize>,
    pub tree_read_bytes: AVec<'a, usize>,
}

impl<'a> OnDemandCounts<'a> {
    fn new_in(names: AVec<'a, &'a str>, alloc: Alloc<'a>) -> OnDemandCounts<'a> {
        let n = names.len();
        let zeros = || {
            let mut v = AVec::with_capacity_in(n, alloc);
            v.resize(n, 0usize);
            v
        };
        OnDemandCounts { walks: zeros(), one_group_solves: zeros(), voided: zeros(), tree_reads: zeros(), tree_read_bytes: zeros(), names }
    }
}

/// One clause per crate the driver touched, by crate index: `alloc: 41 one-group (3 voided),
/// 1 walk, 42 tree reads of 1.2 MB`.
impl core::fmt::Display for OnDemandCounts<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for k in 0..self.names.len() {
            if self.walks[k] == 0 && self.one_group_solves[k] == 0 && self.tree_reads[k] == 0 {
                continue;
            }
            if !first {
                f.write_str("; ")?;
            }
            first = false;
            let mb = self.tree_read_bytes[k] as f64 / 1e6;
            write!(f, "{}: {} one-group ({} voided), {} walk(s), {} tree reads of {mb:.1} MB", self.names[k], self.one_group_solves[k], self.voided[k], self.walks[k], self.tree_reads[k])?;
        }
        Ok(())
    }
}

/// The serial solve, groups written, `members` alone when given; each walk again counted, and
/// each tree record decoded (its bytes).
struct OnDemandPar<'r> {
    members: Option<Members<'r>>,
    again: &'r Cell<usize>,
    tree_reads: &'r Cell<(usize, usize)>,
}

impl Par for OnDemandPar<'_> {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn names_groups(&self) -> bool {
        true
    }

    fn names_members(&self) -> Option<Members<'_>> {
        self.members
    }

    fn names_part(&self, part: NamesPart) {
        if part == NamesPart::TreeAgain {
            self.again.set(self.again.get() + 1);
        }
    }

    fn tree_record_read(&self, bytes: usize) {
        let (n, b) = self.tree_reads.get();
        self.tree_reads.set((n + 1, b + bytes));
    }
}

struct Ctx<'c, 'a, S> {
    crates: &'c [CrateInput<'a>],
    graph: CrateGraph<'a, &'a str>,
    sources: &'c S,
    held: AVec<'a, RefCell<Held<'a>>>,
    roots: RefCell<AVec<'a, Option<&'a CrateNames<'a>>>>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    prepares: &'a Prepares<'a>,
    counts: RefCell<OnDemandCounts<'a>>,
    alloc: Alloc<'a>,
    bump: Bump<'a>,
}

impl<'a, S: SourceTexts<'a>> Ctx<'_, 'a, S> {
    /// Crate `k` solved over what is held now (`members` alone when given), what it wanted
    /// (every crate drained), and whether it walked the tree again (counted in `walks`).
    fn solve(&self, k: usize, members: Option<Members<'_>>) -> (Result<CrateNames<'a>, NamesError<'a>>, AVec<'a, Want<'a>>, bool) {
        let again = Cell::new(0usize);
        let tree_reads = Cell::new((0usize, 0usize));
        let par = OnDemandPar { members, again: &again, tree_reads: &tree_reads };
        let par = &par;
        let alloc = self.alloc;
        let bump = self.bump;
        let sink: &'a Sink<'a> = bump.alloc_val(Sink { wants: RefCell::new(HashMap::new_in(alloc)), alloc });
        let roots = self.roots.borrow();
        let mut slots: AVec<'a, Option<Dep<'a>>> = AVec::with_capacity_in(self.crates.len(), alloc);
        for i in 0..self.crates.len() {
            slots.push(if i == k {
                None
            } else if let Some(c) = roots[i] {
                Some(Dep::Whole(c))
            } else {
                // This solve's frozen view of crate `i`.
                let h = self.held[i].borrow();
                let solves: &'a [&'a CrateNames<'a>] = bump.alloc_slice_copy(&h.solves[..]);
                let mut scopes = HashMap::new_in(alloc);
                scopes.extend(h.scopes.iter().map(|(&m, &s)| (m, s)));
                let scopes: &'a HashMap<ItemKey, &'a ScopeBuilder<'a, &'a str>, Alloc<'a>> = bump.alloc_val(scopes);
                let root = crate::task::root_key(h.key);
                let l = LazyDep { name: h.name, key: h.key, root, edition: h.edition, proc_key: h.proc_key, krate: i as u32, solves, modules: h.modules, scopes, wants: sink };
                Some(Dep::Lazy(bump.alloc_val(l)))
            });
        }
        drop(roots);
        let built = Built { crates: &slots, by_prefix: &self.by_prefix, asked: None };
        let kept = crate::expansions::KeptExpansions::none(alloc);
        let id = CrateId(k as u32);
        let r = match self.crates.get(k) {
            Some(input) => resolve_one::<S, OnDemandPar<'_>>(&self.graph, input, id, k, &built, self.sources, par, First::Prefetch, self.prepares, &kept, bump),
            None => Err(NamesError::NotResolved { krate: "" }),
        };
        let mut wants: AVec<'a, Want<'a>> = AVec::new_in(alloc);
        let taken = core::mem::replace(&mut *sink.wants.borrow_mut(), HashMap::new_in(alloc));
        for (_, mut w) in taken {
            w.modules.sort_unstable();
            w.modules.dedup();
            wants.push(w);
        }
        // By crate (rule 25: the map's order is not the answer's).
        wants.sort_unstable_by_key(|w| w.krate);
        let walked_again = again.get() > 0;
        {
            let mut counts = self.counts.borrow_mut();
            counts.walks[k] += again.get();
            let (n, b) = tree_reads.get();
            counts.tree_reads[k] += n;
            counts.tree_read_bytes[k] += b;
            if !wants.is_empty() {
                counts.voided[k] += 1;
            }
        }
        (r, wants, walked_again)
    }

    /// The crate's tree record `tree` kept in `h`: one decode, counted.
    fn take_tree(&self, k: usize, h: &mut Held<'a>, tree: &'a [u8]) -> Result<(), NamesError<'a>> {
        let mut counts = self.counts.borrow_mut();
        counts.tree_reads[k] += 1;
        counts.tree_read_bytes[k] += tree.len();
        drop(counts);
        h.take_tree(tree, self.bump)
    }

    /// The counts so far, taken (the driver stops after: a refusal carries them).
    fn take_counts(&self) -> OnDemandCounts<'a> {
        let mut names: AVec<'a, &'a str> = AVec::with_capacity_in(self.crates.len(), self.alloc);
        names.extend(self.crates.iter().map(|c| c.name));
        core::mem::replace(&mut *self.counts.borrow_mut(), OnDemandCounts::new_in(names, self.alloc))
    }

    /// Answers `want` in its crate: its walk, then its modules' groups on demand; what those
    /// solves want of other crates first, `depth` crates down.
    fn satisfy(&self, demanded: &mut [Demanded<'a>], want: &Want<'a>, depth: usize) -> Result<(), OnDemandError<'a>> {
        struct Frame<'a> {
            want: Option<Want<'a>>,
            depth: usize,
            round: usize,
        }
        let mut stack: AVec<'a, Frame<'a>> = AVec::new_in(self.alloc);
        stack.push(Frame { want: None, depth, round: 0 });
        while let Some(top) = stack.last_mut() {
            let cur_want = top.want.as_ref().unwrap_or(want);
            let k = cur_want.krate;
            let name = self.crates[k].name;
            if top.depth > self.crates.len() {
                return Err(OnDemandError::Depth { krate: name, most: self.crates.len() });
            }
            if top.round >= MOST_ROUNDS {
                return Err(OnDemandError::Rounds { krate: name, most: MOST_ROUNDS });
            }
            if !self.held[k].borrow().walked() {
                // The crate's one walk: its tree, tables and tree record, no members.
                let (r, mut wants, _) = self.solve(k, Some(Members { members: &[], records: &demanded[k], tree: None }));
                self.counts.borrow_mut().walks[k] += 1;
                if !wants.is_empty() {
                    let next_depth = top.depth + 1;
                    top.round += 1;
                    while let Some(w) = wants.pop() {
                        stack.push(Frame { want: Some(w), depth: next_depth, round: 0 });
                    }
                    continue;
                }
                let c: &'a CrateNames<'a> = self.bump.alloc_val(r.map_err(OnDemandError::Names)?);
                let mut h = self.held[k].borrow_mut();
                self.take_tree(k, &mut h, c.tree_record).map_err(OnDemandError::Names)?;
                h.solves.push(c);
            }
            let modules = self.held[k].borrow().unsolved(&cur_want.modules, self.alloc);
            if modules.is_empty() {
                stack.pop();
                continue;
            }
            let r = {
                let solve_crate = |m: Members<'_>| -> Result<Solved<'a>, Refused<'a>> {
                    self.counts.borrow_mut().one_group_solves[k] += 1;
                    let (r, wants, walked_again) = self.solve(k, Some(m));
                    if !wants.is_empty() {
                        return Err(Refused::Wanted(wants));
                    }
                    let c: &'a CrateNames<'a> = self.bump.alloc_val(r.map_err(Refused::Names)?);
                    let mut h = self.held[k].borrow_mut();
                    if walked_again {
                        // The record's inputs moved under it: the new record is the crate's.
                        self.take_tree(k, &mut h, c.tree_record).map_err(Refused::Names)?;
                    }
                    h.solves.push(c);
                    let mut out: AVec<'a, NameGroup<'a>> = AVec::new_in(self.alloc);
                    out.extend(c.groups.iter().cloned());
                    Ok(Solved { groups: out, tree: c.tree_record })
                };
                // Every one-group solve of the crate reads its tree record: no walk.
                let tree = self.held[k].borrow().tree;
                let mut solver = OneGroup { solve_crate, alloc: self.alloc, tree };
                demand(&modules, &mut solver, &mut demanded[k], self.alloc)
            };
            if let (Some(held), Some(dem)) = (self.held.get(k), demanded.get(k)) {
                held.borrow_mut().hold(dem, self.bump)?;
            }
            match r {
                Ok(()) => {
                    stack.pop();
                }
                Err(DemandError::Solver(OneGroupRefusal::Crate(Refused::Wanted(mut wants)))) => {
                    let next_depth = top.depth + 1;
                    top.round += 1;
                    while let Some(w) = wants.pop() {
                        stack.push(Frame { want: Some(w), depth: next_depth, round: 0 });
                    }
                }
                Err(error) => {
                    let h = self.held[k].borrow();
                    let path = demand_module(&error).and_then(|m| h.solves.iter().find_map(|&c| c.path_of(m)));
                    let source = demand_source(&error).and_then(|s| h.solves.iter().find_map(|&c| c.path_of(s).or_else(|| c.enum_path_of(s))));
                    drop(h);
                    return Err(OnDemandError::Demand { krate: name, path, source, error, counts: self.take_counts() });
                }
            }
        }
        Ok(())
    }
}

/// The module a demand's error names, if it names one.
fn demand_module(e: &DemandError<OneGroupRefusal<Refused<'_>>>) -> Option<ItemKey> {
    match e {
        DemandError::Depth { module, .. } | DemandError::Recorded { module } | DemandError::NotCovered { module } | DemandError::Recut { module } => Some(*module),
        DemandError::Solver(OneGroupRefusal::Crate(Refused::Names(NamesError::Cells { error: super::switch::Unwritten::Member(r), .. }))) => r.module(),
        _ => None,
    }
}

/// The second item a demand's error names (a glob's source), if it names one.
fn demand_source(e: &DemandError<OneGroupRefusal<Refused<'_>>>) -> Option<ItemKey> {
    match e {
        DemandError::Solver(OneGroupRefusal::Crate(Refused::Names(NamesError::Cells { error: super::switch::Unwritten::Member(MemberRefusal::GlobSource { source, .. }), .. }))) => Some(*source),
        _ => None,
    }
}

/// The names of `crates` with `roots` (crate indices) solved whole, in graph order, and every
/// other crate solved on demand from what the roots' solves read. Scratch and results in
/// `alloc`.
pub fn names_on_demand<'a, S: SourceTexts<'a>>(crates: &[CrateInput<'a>], sysroot: Sysroot, sources: &S, roots: &[usize], bump: Bump<'a>) -> Result<OnDemand<'a>, OnDemandError<'a>> {
    let alloc = bump.alloc();
    let graph = graph_of(crates, sysroot, alloc).map_err(OnDemandError::Graph)?;
    let mut held = AVec::with_capacity_in(crates.len(), alloc);
    let mut by_prefix = HashMap::new_in(alloc);
    let mut demanded = AVec::with_capacity_in(crates.len(), alloc);
    let mut none = AVec::with_capacity_in(crates.len(), alloc);
    let mut names: AVec<'a, &'a str> = AVec::with_capacity_in(crates.len(), alloc);
    for (k, c) in crates.iter().enumerate() {
        names.push(c.name);
        let proc_key = if c.implicit.proc_macro { sources.proc_macro_key(k) } else { None };
        held.push(RefCell::new(Held {
            name: c.name,
            key: c.key,
            edition: c.edition,
            proc_key,
            solves: AVec::new_in(alloc),
            scopes: HashMap::new_in(alloc),
            groups_held: 0,
            tree: None,
            modules: &[],
        }));
        by_prefix.insert(c.key.prefix(), k);
        demanded.push(Demanded::new(DemandedOpts { alloc }));
        none.push(None);
    }
    let mut order: AVec<'a, usize> = AVec::new_in(alloc);
    for wave in graph.waves() {
        order.extend(wave.iter().map(|id| id.index()).filter(|i| roots.contains(i)));
    }
    let cx = Ctx { crates, graph, sources, held, roots: RefCell::new(none), by_prefix, prepares: Prepares::new_in(bump), counts: RefCell::new(OnDemandCounts::new_in(names, alloc)), alloc, bump };
    let mut root_solves = 0usize;
    for &r in order.iter() {
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            if rounds > MOST_ROUNDS {
                return Err(OnDemandError::Rounds { krate: crates[r].name, most: MOST_ROUNDS });
            }
            root_solves += 1;
            let (res, wants, _) = cx.solve(r, None);
            if wants.is_empty() {
                let c: &'a CrateNames<'a> = bump.alloc_val(res.map_err(OnDemandError::Names)?);
                cx.roots.borrow_mut()[r] = Some(c);
                break;
            }
            for w in wants.iter() {
                cx.satisfy(&mut demanded, w, 0)?;
            }
        }
    }
    let mut trees = AVec::with_capacity_in(crates.len(), alloc);
    trees.extend(cx.held.iter().map(|h| h.borrow().tree));
    let counts = cx.counts.into_inner();
    let walks = AVec::from_slice_in(&counts.walks, alloc);
    let one_group_solves = AVec::from_slice_in(&counts.one_group_solves, alloc);
    Ok(OnDemand { roots: cx.roots.into_inner(), demanded, walks, one_group_solves, trees, root_solves, counts })
}
