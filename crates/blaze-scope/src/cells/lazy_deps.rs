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
//! OPEN: a dependency is still walked whole when first read (its every file outlined and
//! lowered: the decision is a walk per `mod` line, its parent's record naming the child); each
//! one-group solve walks it again (lane CK's cost piece); impl tables are not here (a trait
//! question needs every module that holds an `impl`: an expansion of the crate whole, impl
//! headers resolved).

use core::cell::RefCell;

use blaze_format::FormatError;
use blaze_hash::HashMap;
use blaze_id::{CrateItemKey, ItemKey};
use blaze_own::{AVec, Alloc, Box};
use blaze_surface::{Edition, ScopeBuilder, ScopeView};

use super::edit::{Members, OneGroup, OneGroupRefusal};
use super::groups::{NameGroup, RecordError};
use super::lazy::{DemandError, Demanded, DemandedOpts, demand};
use crate::crate_scope::Dep;
use crate::graph::{CrateGraph, CrateId, Sysroot};
use crate::prepares::Prepares;
use crate::resolve::{Built, CrateInput, CrateNames, NamesError, SourceTexts, graph_of, resolve_one};
use crate::task::{First, MacroSrc, Par, Serial, Variant};

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

impl<'a> LazyDep<'a> {
    fn is_module(&self, key: ItemKey) -> bool {
        self.solves.iter().any(|c| c.tree.by_key.contains_key(&key) || c.made.contains_key(&key))
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
}

impl<'a> Held<'a> {
    fn walked(&self) -> bool {
        !self.solves.is_empty()
    }

    /// The scopes of the groups `demanded` kept since the last call, from their records.
    fn hold(&mut self, demanded: &Demanded<'a>, alloc: Alloc<'a>) -> Result<(), OnDemandError<'a>> {
        for g in demanded.groups[self.groups_held..].iter() {
            for &m in g.members.iter() {
                let bytes = g.scope_of(m).map_err(|error| OnDemandError::Record { krate: self.name, module: m, error })?;
                let bytes = crate::resolve::aligned(bytes, alloc);
                let view = ScopeView::read(bytes).map_err(|error| OnDemandError::Scope { krate: self.name, module: m, error })?;
                let scope: &'a ScopeBuilder<'a, &'a str> = Box::leak(Box::new_in(ScopeBuilder::from(&view, alloc), alloc));
                self.scopes.insert(m, scope);
            }
        }
        self.groups_held = demanded.groups.len();
        Ok(())
    }

    /// Of `modules`, those that are modules of the crate with no scope here yet.
    fn unsolved(&self, modules: &[ItemKey], alloc: Alloc<'a>) -> AVec<'a, ItemKey> {
        let mut out = AVec::new_in(alloc);
        let is_module = |k: &ItemKey| self.solves.iter().any(|c| c.tree.by_key.contains_key(k) || c.made.contains_key(k));
        out.extend(modules.iter().copied().filter(|m| is_module(m) && !self.scopes.contains_key(m)));
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
    /// from the crate's walk (rule 19), when it names one the walk holds.
    Demand { krate: &'a str, path: Option<&'a str>, error: DemandError<OneGroupRefusal<Refused<'a>>> },
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
    /// Each crate's walks (solves with no members); a crate never read has none.
    pub walks: AVec<'a, usize>,
    /// The solves of root crates, voided ones counted.
    pub root_solves: usize,
}

/// The serial solve, groups written, `members` alone when given.
struct OnDemandPar<'r> {
    members: Option<Members<'r>>,
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
}

struct Ctx<'c, 'a, S> {
    crates: &'c [CrateInput<'a>],
    graph: CrateGraph<'a, &'a str>,
    sources: &'c S,
    held: AVec<'a, RefCell<Held<'a>>>,
    roots: RefCell<AVec<'a, Option<&'a CrateNames<'a>>>>,
    by_prefix: HashMap<[u8; 8], usize, Alloc<'a>>,
    prepares: &'a Prepares<'a>,
    walks: RefCell<AVec<'a, usize>>,
    alloc: Alloc<'a>,
}

impl<'a, S: SourceTexts<'a>> Ctx<'_, 'a, S> {
    /// Crate `k` solved over what is held now, and what it wanted (every crate drained).
    fn solve(&self, k: usize, par: &OnDemandPar<'_>) -> (Result<CrateNames<'a>, NamesError<'a>>, AVec<'a, Want<'a>>) {
        let alloc = self.alloc;
        let sink: &'a Sink<'a> = Box::leak(Box::new_in(Sink { wants: RefCell::new(HashMap::new_in(alloc)), alloc }, alloc));
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
                let solves: &'a AVec<'a, &'a CrateNames<'a>> = Box::leak(Box::new_in(AVec::from_slice_in(&h.solves[..], alloc), alloc));
                let solves: &'a [&'a CrateNames<'a>] = solves.as_slice();
                let mut scopes = HashMap::new_in(alloc);
                scopes.extend(h.scopes.iter().map(|(&m, &s)| (m, s)));
                let scopes: &'a HashMap<ItemKey, &'a ScopeBuilder<'a, &'a str>, Alloc<'a>> = Box::leak(Box::new_in(scopes, alloc));
                let root = crate::task::root_key(h.key);
                let l = LazyDep { name: h.name, key: h.key, root, edition: h.edition, proc_key: h.proc_key, krate: i as u32, solves, scopes, wants: sink };
                Some(Dep::Lazy(Box::leak(Box::new_in(l, alloc))))
            });
        }
        drop(roots);
        let built = Built { crates: &slots, by_prefix: &self.by_prefix, asked: None };
        let kept = crate::expansions::KeptExpansions::none(alloc);
        let id = CrateId(k as u32);
        let r = resolve_one::<S, OnDemandPar<'_>>(&self.graph, &self.crates[k], id, k, &built, self.sources, par, First::Prefetch, self.prepares, &kept, alloc);
        let mut wants: AVec<'a, Want<'a>> = AVec::new_in(alloc);
        let taken = core::mem::replace(&mut *sink.wants.borrow_mut(), HashMap::new_in(alloc));
        for (_, mut w) in taken {
            w.modules.sort_unstable();
            w.modules.dedup();
            wants.push(w);
        }
        // By crate (rule 25: the map's order is not the answer's).
        wants.sort_unstable_by_key(|w| w.krate);
        (r, wants)
    }

    /// Answers `want` in its crate: its walk, then its modules' groups on demand; what those
    /// solves want of other crates first, `depth` crates down.
    fn satisfy(&self, demanded: &mut [Demanded<'a>], want: &Want<'a>, depth: usize) -> Result<(), OnDemandError<'a>> {
        let k = want.krate;
        let name = self.crates[k].name;
        if depth > self.crates.len() {
            return Err(OnDemandError::Depth { krate: name, most: self.crates.len() });
        }
        for _ in 0..MOST_ROUNDS {
            if !self.held[k].borrow().walked() {
                let (r, wants) = self.solve(k, &OnDemandPar { members: Some(Members { members: &[], records: &demanded[k], tree: None }) });
                if !wants.is_empty() {
                    for w in wants.iter() {
                        self.satisfy(demanded, w, depth + 1)?;
                    }
                    continue;
                }
                let c: &'a CrateNames<'a> = Box::leak(Box::new_in(r.map_err(OnDemandError::Names)?, self.alloc));
                self.held[k].borrow_mut().solves.push(c);
                self.walks.borrow_mut()[k] += 1;
            }
            let modules = self.held[k].borrow().unsolved(&want.modules, self.alloc);
            if modules.is_empty() {
                return Ok(());
            }
            let r = {
                let solve_crate = |m: Members<'_>| -> Result<AVec<'a, NameGroup<'a>>, Refused<'a>> {
                    let (r, wants) = self.solve(k, &OnDemandPar { members: Some(m) });
                    if !wants.is_empty() {
                        return Err(Refused::Wanted(wants));
                    }
                    let c: &'a CrateNames<'a> = Box::leak(Box::new_in(r.map_err(Refused::Names)?, self.alloc));
                    self.held[k].borrow_mut().solves.push(c);
                    let mut out = AVec::new_in(self.alloc);
                    out.extend(c.groups.iter().cloned());
                    Ok(out)
                };
                let mut solver = OneGroup { solve_crate, alloc: self.alloc, tree: None };
                demand(&modules, &mut solver, &mut demanded[k], self.alloc)
            };
            self.held[k].borrow_mut().hold(&demanded[k], self.alloc)?;
            match r {
                Ok(()) => return Ok(()),
                Err(DemandError::Solver(OneGroupRefusal::Crate(Refused::Wanted(wants)))) => {
                    for w in wants.iter() {
                        self.satisfy(demanded, w, depth + 1)?;
                    }
                }
                Err(error) => {
                    let path = demand_module(&error).and_then(|m| self.held[k].borrow().solves.iter().find_map(|&c| c.path_of(m)));
                    return Err(OnDemandError::Demand { krate: name, path, error });
                }
            }
        }
        Err(OnDemandError::Rounds { krate: name, most: MOST_ROUNDS })
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

/// The names of `crates` with `roots` (crate indices) solved whole, in graph order, and every
/// other crate solved on demand from what the roots' solves read. Scratch and results in
/// `alloc`.
pub fn names_on_demand<'a, S: SourceTexts<'a>>(crates: &[CrateInput<'a>], sysroot: Sysroot, sources: &S, roots: &[usize], alloc: Alloc<'a>) -> Result<OnDemand<'a>, OnDemandError<'a>> {
    let graph = graph_of(crates, sysroot, alloc).map_err(OnDemandError::Graph)?;
    let mut held = AVec::with_capacity_in(crates.len(), alloc);
    let mut by_prefix = HashMap::new_in(alloc);
    let mut demanded = AVec::with_capacity_in(crates.len(), alloc);
    let mut none = AVec::with_capacity_in(crates.len(), alloc);
    let mut walks = AVec::with_capacity_in(crates.len(), alloc);
    for (k, c) in crates.iter().enumerate() {
        let proc_key = if c.implicit.proc_macro { sources.proc_macro_key(k) } else { None };
        held.push(RefCell::new(Held {
            name: c.name,
            key: c.key,
            edition: c.edition,
            proc_key,
            solves: AVec::new_in(alloc),
            scopes: HashMap::new_in(alloc),
            groups_held: 0,
        }));
        by_prefix.insert(c.key.prefix(), k);
        demanded.push(Demanded::new(DemandedOpts { alloc }));
        none.push(None);
        walks.push(0usize);
    }
    let mut order: AVec<'a, usize> = AVec::new_in(alloc);
    for wave in graph.waves() {
        order.extend(wave.iter().map(|id| id.index()).filter(|i| roots.contains(i)));
    }
    let cx = Ctx { crates, graph, sources, held, roots: RefCell::new(none), by_prefix, prepares: Prepares::new_in(alloc), walks: RefCell::new(walks), alloc };
    let mut root_solves = 0usize;
    for &r in order.iter() {
        let mut rounds = 0usize;
        loop {
            rounds += 1;
            if rounds > MOST_ROUNDS {
                return Err(OnDemandError::Rounds { krate: crates[r].name, most: MOST_ROUNDS });
            }
            root_solves += 1;
            let (res, wants) = cx.solve(r, &OnDemandPar { members: None });
            if wants.is_empty() {
                let c: &'a CrateNames<'a> = Box::leak(Box::new_in(res.map_err(OnDemandError::Names)?, alloc));
                cx.roots.borrow_mut()[r] = Some(c);
                break;
            }
            for w in wants.iter() {
                cx.satisfy(&mut demanded, w, 0)?;
            }
        }
    }
    Ok(OnDemand { roots: cx.roots.into_inner(), demanded, walks: cx.walks.into_inner(), root_solves })
}
