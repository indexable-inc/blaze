//! The crate graph names are resolved over: each crate as its dependents see it, its edition
//! and which `std` it has, its features, its dependencies by the names its code calls them,
//! and the order crates are resolved in (docs/v2/plan/design/bottom.md, section 6, "Across
//! crates").
//!
//! A crate's names are resolved after every crate it depends on, and a lookup that leaves the
//! crate reads the other crate's finished `ModuleScope` records, never resolving it again; so
//! the graph's one job beyond holding the facts is the order, in waves: wave 0 is the crates
//! with no dependency, wave `n` those whose deepest dependency is in wave `n - 1`, and the
//! crates of one wave resolve in parallel. A cycle between crates is a [`GraphError::Cycle`]
//! naming its crates.
//!
//! What a crate has in scope from other crates without naming them ([`CrateGraph::externs`])
//! is Rust's rule (rustc's `Resolver::new` and `std_inject`), with the blaze edition's one
//! change (docs/v2/plan/design/std.md, "The blaze edition: opting in"):
//!
//! - the extern prelude: every dependency by its extern name; `core` unless `#![no_core]`;
//!   `std` unless `#![no_std]` (under a blaze edition `std` is bstd); `alloc` only by an
//!   `extern crate alloc;` at the root, which the resolver adds from the root's own items;
//! - the crates an `extern crate` may name: the extern prelude and the sysroot's (`core`,
//!   `alloc`, `std`; under a blaze edition `std` is bstd and Rust's `std` has no name);
//! - the injected `#[macro_use] extern crate`: `std` (bstd), or `core` under `#![no_std]`;
//! - the standard prelude: `std::prelude::rust_20xx`, `core::prelude::rust_20xx` under
//!   `#![no_std]`, or bstd's `prelude::<the blaze edition's module>`.
//!
//! The graph is sans-IO and allocates only in the allocator it is given; the walks are
//! explicit (no recursion) and bounded by [`MAX_GRAPH_CRATES`].

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc};
use blaze_surface::Edition;
use blaze_unit_graph::{Implicit, ToolchainUnit};

use crate::Name;

/// The most crates one graph holds.
pub const MAX_GRAPH_CRATES: usize = 1 << 14;

/// A crate's index in its graph.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CrateId(pub u32);

impl CrateId {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A dependency edge: the name the dependent's code calls the crate by (a `[dependencies]`
/// key renamed with `package = ".."`, else the dependency's library name), and the crate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct GraphDep<N> {
    pub name: N,
    pub krate: CrateId,
}

/// One crate of the graph.
#[derive(Clone, Debug)]
pub struct GraphCrate<'a, N> {
    /// Its library name: what a dependent calls it unless it renames it.
    pub name: N,
    /// The Rust edition its syntax and names are.
    pub edition: Edition,
    /// What it is given without naming it: which `std` it has and its prelude
    /// (blaze-unit-graph's `implicit`, the rule's one home).
    pub implicit: Implicit,
    /// The features turned on, sorted.
    pub features: AVec<'a, N>,
    /// The cfg it is compiled under, sorted: blaze-unit-graph's `Unit::cfg` as it is (the
    /// platform's atoms, `feature = ".."` per feature, `debug_assertions`, ...), each `(name,
    /// value)`. A module's declarations are lowered under it, and it is part of their key.
    pub cfg: AVec<'a, (N, Option<N>)>,
    /// Its dependencies (normal ones; build and dev dependencies are other crates' graphs).
    pub deps: AVec<'a, GraphDep<N>>,
}

/// The crates every crate may reach without a dependency line: the toolchain's and blaze's.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Sysroot {
    pub core: Option<CrateId>,
    pub alloc: Option<CrateId>,
    /// Rust's `std`.
    pub std: Option<CrateId>,
    /// bstd: `std` under a blaze edition.
    pub bstd: Option<CrateId>,
}

impl Sysroot {
    /// `core`, `alloc`, `std`, bstd, in that order.
    pub fn all(self) -> [Option<CrateId>; 4] {
        [self.core, self.alloc, self.std, self.bstd]
    }

    /// As blaze-unit-graph's sysroot, so its rule ([`Implicit::externs`]) answers in this
    /// graph's crates: a crate's index is its `ToolchainUnit`, and a crate the graph lacks (and
    /// `proc_macro`, which no graph here holds) is [`ABSENT`], read back as `None` by
    /// [`Sysroot::crate_of`].
    fn units(self) -> blaze_unit_graph::Sysroot {
        let unit = |c: Option<CrateId>| c.map_or(ABSENT, |c| ToolchainUnit(c.0));
        blaze_unit_graph::Sysroot {
            core: unit(self.core),
            alloc: unit(self.alloc),
            std: unit(self.std),
            proc_macro: ABSENT,
            bstd: unit(self.bstd),
        }
    }

    /// The graph's crate a unit of [`Sysroot::units`] is.
    fn crate_of(unit: ToolchainUnit) -> Option<CrateId> {
        (unit != ABSENT).then_some(CrateId(unit.0))
    }
}

/// A toolchain crate the graph does not hold, in [`Sysroot::units`]: no graph holds
/// [`MAX_GRAPH_CRATES`] or more crates, so no crate's index is this.
const ABSENT: ToolchainUnit = ToolchainUnit(u32::MAX);

/// The crate attributes of a root that change what is injected.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct RootAttrs {
    pub no_std: bool,
    pub no_core: bool,
    /// `#![blaze::arbitrary_ints]`: injects nothing; kept on the root's head, where the
    /// builtin types are looked up ([`crate::builtin_type_in`]).
    pub arbitrary_ints: bool,
}

/// The standard prelude a crate's modules import: a module of `krate`, by the path of module
/// names from its root (`prelude`, `rust_2024`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PreludePath<N> {
    pub krate: CrateId,
    pub path: [N; 2],
}

/// What a crate has from other crates without naming them: [`CrateGraph::externs`].
#[derive(Clone, Debug)]
pub struct Externs<'a, N> {
    /// The extern prelude (before the root's `extern crate` items), sorted by name.
    pub prelude: AVec<'a, GraphDep<N>>,
    /// What an `extern crate` may name, sorted by name: the extern prelude and the sysroot's.
    pub deps: AVec<'a, GraphDep<N>>,
    /// The injected `#[macro_use] extern crate`'s crate.
    pub macro_use: Option<CrateId>,
    pub std_prelude: Option<PreludePath<N>>,
    /// The extern prelude's crates the rule gives this crate that the graph cannot: never
    /// silent, each a note of the crate's resolution (`NamesNote::ExternDropped`).
    pub dropped: AVec<'a, DroppedExtern>,
}

/// An implicit crate the rule (blaze-unit-graph's `implicit`) gives a crate and its graph
/// cannot give it: [`Externs::dropped`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DroppedExtern {
    /// The name it would have in the extern prelude (`core`, `std`, `proc_macro`).
    pub name: &'static str,
    pub why: ExternGone,
}

/// Why an implicit crate is not given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExternGone {
    /// The graph's [`Sysroot`] holds no such crate: a Rust-edition crate's `std` in a graph
    /// made with only bstd, or `proc_macro`, which no graph here holds.
    NotInGraph,
    /// It is `krate`, a sysroot crate this crate is built into, so resolved after it.
    BuiltInto { krate: CrateId },
}

impl fmt::Display for ExternGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExternGone::NotInGraph => f.write_str("the graph's sysroot holds no such crate"),
            ExternGone::BuiltInto { krate } => write!(f, "it is crate {}, which this crate is built into", krate.0),
        }
    }
}

/// Why no graph: each names the crates.
#[derive(Clone, Debug)]
pub enum GraphError<'a, N> {
    /// More crates than [`MAX_GRAPH_CRATES`].
    TooMany { crates: usize },
    /// `krate`'s dependency `name` is no crate of the graph.
    NoSuchCrate { krate: N, name: N, index: u32 },
    /// The [`Sysroot`] names crate `index`, which the graph does not hold.
    NoSuchSysrootCrate { index: u32 },
    /// Two dependencies of `krate` by one name.
    TwoDepsOfOneName { krate: N, name: N },
    /// A cycle of dependencies: each depends on the next, the last on the first.
    Cycle { crates: AVec<'a, N> },
}

impl<N: Name> fmt::Display for GraphError<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::TooMany { crates } => write!(f, "{crates} crates: a graph holds at most {MAX_GRAPH_CRATES}"),
            GraphError::NoSuchCrate { krate, name, index } => {
                write!(f, "crate `{}`: dependency `{}` is crate {index}, which the graph does not hold", krate.as_ref(), name.as_ref())
            }
            GraphError::NoSuchSysrootCrate { index } => write!(f, "the sysroot names crate {index}, which the graph does not hold"),
            GraphError::TwoDepsOfOneName { krate, name } => {
                write!(f, "crate `{}`: two dependencies are called `{}`", krate.as_ref(), name.as_ref())
            }
            GraphError::Cycle { crates } => {
                f.write_str("a cycle of crate dependencies: ")?;
                for (at, name) in crates.iter().enumerate() {
                    write!(f, "{}`{}`", if at == 0 { "" } else { " -> " }, name.as_ref())?;
                }
                match crates.first() {
                    Some(first) => write!(f, " -> `{}`", first.as_ref()),
                    None => Ok(()),
                }
            }
        }
    }
}

/// The crates and the order they resolve in.
#[derive(Debug)]
pub struct CrateGraph<'a, N> {
    crates: AVec<'a, GraphCrate<'a, N>>,
    sysroot: Sysroot,
    /// Every crate, each after its dependencies; by wave, then by index.
    order: AVec<'a, CrateId>,
    /// Each crate's wave, by index.
    wave: AVec<'a, u32>,
    /// By index: bit `s` set when the crate is [`Sysroot::all`]'s `s`-th or built into it; such
    /// a crate is resolved before that sysroot crate and cannot name it.
    built_from: AVec<'a, u8>,
    alloc: Alloc<'a>,
}

impl<'a, N: Name> CrateGraph<'a, N> {
    /// The graph of `crates` (each dependency an index into it), ordered.
    pub fn new_in(crates: AVec<'a, GraphCrate<'a, N>>, sysroot: Sysroot, alloc: Alloc<'a>) -> Result<CrateGraph<'a, N>, GraphError<'a, N>> {
        let n = crates.len();
        if n > MAX_GRAPH_CRATES {
            return Err(GraphError::TooMany { crates: n });
        }
        for c in crates.iter() {
            for (at, d) in c.deps.iter().enumerate() {
                if d.krate.index() >= n {
                    return Err(GraphError::NoSuchCrate { krate: c.name, name: d.name, index: d.krate.0 });
                }
                if c.deps[..at].iter().any(|e| e.name == d.name) {
                    return Err(GraphError::TwoDepsOfOneName { krate: c.name, name: d.name });
                }
            }
        }
        // Which sysroot crates each crate is built from: bit `s` of `built_from[c]` is set when
        // `c` is the sysroot crate `s` or one of its dependencies, near or far.
        let mut built_from: AVec<'a, u8> = AVec::with_capacity_in(n, alloc);
        built_from.resize(n, 0);
        let mut stack: AVec<'a, u32> = AVec::new_in(alloc);
        for (bit, s) in sysroot.all().iter().enumerate() {
            let Some(s) = s else { continue };
            if s.index() >= n {
                return Err(GraphError::NoSuchSysrootCrate { index: s.0 });
            }
            stack.push(s.0);
            while let Some(c) = stack.pop() {
                if built_from[c as usize] & (1 << bit) != 0 {
                    continue;
                }
                built_from[c as usize] |= 1 << bit;
                for d in crates[c as usize].deps.iter() {
                    stack.push(d.krate.0);
                }
            }
        }
        // Dependents, as a flat list of (dependency, dependent) sorted by dependency: the
        // dependency lines, and every sysroot crate before each crate it is not built from.
        let mut edges: AVec<'a, (u32, u32)> = AVec::new_in(alloc);
        for (i, c) in crates.iter().enumerate() {
            for d in c.deps.iter() {
                edges.push((d.krate.0, i as u32));
            }
            for (bit, s) in sysroot.all().iter().enumerate() {
                if let Some(s) = s {
                    if sysroot_before(built_from[i], bit) && !c.deps.iter().any(|d| d.krate == *s) {
                        edges.push((s.0, i as u32));
                    }
                }
            }
        }
        edges.sort_unstable();
        // Kahn's algorithm by waves: a crate's wave is one past its deepest dependency's.
        let mut left: AVec<'a, u32> = AVec::with_capacity_in(n, alloc);
        let mut wave: AVec<'a, u32> = AVec::with_capacity_in(n, alloc);
        left.resize(n, 0);
        wave.resize(n, 0);
        for &(_, dependent) in edges.iter() {
            left[dependent as usize] += 1;
        }
        let mut order: AVec<'a, CrateId> = AVec::with_capacity_in(n, alloc);
        let mut current: AVec<'a, u32> = AVec::new_in(alloc);
        for (i, l) in left.iter().enumerate() {
            if *l == 0 {
                current.push(i as u32);
            }
        }
        let mut depth = 0u32;
        while !current.is_empty() {
            let mut next: AVec<'a, u32> = AVec::new_in(alloc);
            for &c in current.iter() {
                wave[c as usize] = depth;
                order.push(CrateId(c));
                let start = edges.partition_point(|e| e.0 < c);
                for &(_, dependent) in edges[start..].iter().take_while(|e| e.0 == c) {
                    left[dependent as usize] -= 1;
                    if left[dependent as usize] == 0 {
                        next.push(dependent);
                    }
                }
            }
            next.sort_unstable();
            current = next;
            depth += 1;
        }
        if order.len() < n {
            return Err(GraphError::Cycle { crates: cycle_of(&crates, &left, alloc) });
        }
        Ok(CrateGraph { crates, sysroot, order, wave, built_from, alloc })
    }

    pub fn crates(&self) -> &[GraphCrate<'a, N>] {
        &self.crates
    }

    pub fn get(&self, id: CrateId) -> &GraphCrate<'a, N> {
        &self.crates[id.index()]
    }

    pub fn sysroot(&self) -> Sysroot {
        self.sysroot
    }

    /// Every crate, each after every crate it depends on.
    pub fn order(&self) -> &[CrateId] {
        &self.order
    }

    /// The crate's wave: crates of one wave depend on none of each other.
    pub fn wave(&self, id: CrateId) -> u32 {
        self.wave[id.index()]
    }

    /// The order cut into waves, in order.
    pub fn waves(&self) -> impl Iterator<Item = &[CrateId]> + '_ {
        let mut at = 0;
        core::iter::from_fn(move || {
            let first = *self.order.get(at)?;
            let w = self.wave(first);
            let len = self.order[at..].iter().take_while(|c| self.wave(**c) == w).count();
            let out = &self.order[at..at + len];
            at += len;
            Some(out)
        })
    }

    /// What `id` has from other crates without naming them, its root's attributes being
    /// `attrs` (Rust's rule, and the blaze edition's: see the module's documentation).
    pub fn externs(&self, id: CrateId, attrs: RootAttrs) -> Externs<'a, N>
    where
        N: NameOf,
    {
        let c = self.get(id);
        let word = |text: &'static str| c.name.of_static(text);
        let sys = self.sysroot;
        let units = sys.units();
        let implicit = c.implicit;
        // A sysroot crate is nameable from the crates resolved after it (`sysroot_before`).
        let built_from = self.built_from[id.index()];
        let not_self = |k: Option<CrateId>| {
            k.filter(|k| sys.all().iter().enumerate().all(|(bit, s)| *s != Some(*k) || sysroot_before(built_from, bit)))
        };
        let mut prelude: AVec<'a, GraphDep<N>> = AVec::new_in(self.alloc);
        prelude.extend(c.deps.iter().copied());
        let add = |list: &mut AVec<'a, GraphDep<N>>, name: N, krate: Option<CrateId>| {
            if let Some(krate) = krate {
                if !list.iter().any(|d| d.name == name) {
                    list.push(GraphDep { name, krate });
                }
            }
        };
        // The extern prelude the rule adds (`core`, `std`, `proc_macro`), less what the root's
        // `#![no_core]` and `#![no_std]` take away.
        let mut dropped: AVec<'a, DroppedExtern> = AVec::new_in(self.alloc);
        if !attrs.no_core {
            for (name, unit) in implicit.externs(&units) {
                if name == "std" && attrs.no_std {
                    continue;
                }
                let krate = Sysroot::crate_of(unit);
                let kept = not_self(krate);
                match (krate, kept) {
                    (None, _) => dropped.push(DroppedExtern { name, why: ExternGone::NotInGraph }),
                    (Some(krate), None) => dropped.push(DroppedExtern { name, why: ExternGone::BuiltInto { krate } }),
                    (Some(_), Some(_)) => {}
                }
                add(&mut prelude, word(name), kept);
            }
        }
        // What `extern crate` may name besides: the rule's `core`, `alloc` and `std`.
        let mut deps = prelude.clone_in(self.alloc);
        for name in ["core", "alloc", "std"] {
            add(&mut deps, word(name), not_self(implicit.extern_crate(name, &units).and_then(Sysroot::crate_of)));
        }
        prelude.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        deps.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let (macro_use, std_prelude) = if attrs.no_core {
            (None, None)
        } else {
            // The injected `#[macro_use] extern crate std;` (`core` under `#![no_std]`) names
            // its crate as every `extern crate` does: by the extern prelude's entry of that name
            // (the rule's `std`: Rust's, or bstd under a blaze edition, blaze-unit-graph's rule).
            // A dependency named `core` or `std` is that entry (the macro side's own, D89 (a);
            // `core = { package = "rustc-std-workspace-core" }`), else the sysroot's added above.
            // Taking the sysroot's always gave a macro-side crate the target side's preludes:
            // every glob of its own side's `core` macros then had another outer candidate
            // (`Ambiguous(GlobVsOuter)`, tempo l3d: libc's `assert!`).
            let name = word(if attrs.no_std { "core" } else { "std" });
            let injected = prelude.iter().find(|d| d.name == name).map(|d| d.krate);
            // `#![no_std]`: core's prelude of the syntax edition; else the rule's prelude.
            let module = if attrs.no_std { rust_prelude(c.edition) } else { implicit.prelude.module() };
            let path = injected.map(|krate| PreludePath { krate, path: [word("prelude"), word(module)] });
            (injected, path)
        };
        Externs { prelude, deps, macro_use, std_prelude, dropped }
    }
}

/// Whether the sysroot crate [`Sysroot::all`]`[bit]` is resolved before a crate whose
/// `built_from` bits are these: when it comes before the first sysroot crate the crate is
/// built into (a crate built into none comes after all). So `core` precedes `alloc` precedes
/// `std` precedes bstd, `std`'s own dependencies (libc, hashbrown) come after `core` and
/// `alloc` but before `std`, and no order between sysroot crates makes a cycle. Public: a
/// driver that resolves one crate at a time (blaze-cli names/scope.rs) gives each crate the
/// sysroot crates by this one rule.
pub fn sysroot_before(built_from: u8, bit: usize) -> bool {
    (bit as u32) < built_from.trailing_zeros()
}

/// The module of Rust's standard prelude of `edition`, under `prelude`.
pub fn rust_prelude(edition: Edition) -> &'static str {
    match edition {
        Edition::E2015 => "rust_2015",
        Edition::E2018 => "rust_2018",
        Edition::E2021 => "rust_2021",
        Edition::E2024 => "rust_2024",
    }
}

/// A name type that can name a word of the language (`std`, `prelude`): every `&str` can.
pub trait NameOf: Name {
    fn of_static(self, word: &'static str) -> Self;
}

impl<'n> NameOf for &'n str {
    fn of_static(self, word: &'static str) -> &'n str {
        word
    }
}

/// A cycle among the crates Kahn's algorithm left (`left[i] > 0`): from the first of them,
/// follow a dependency that is also left until a crate repeats; the crates from its first
/// visit on are the cycle.
fn cycle_of<'a, N: Name>(crates: &[GraphCrate<'a, N>], left: &[u32], alloc: Alloc<'a>) -> AVec<'a, N> {
    let mut path: AVec<'a, u32> = AVec::new_in(alloc);
    let mut out: AVec<'a, N> = AVec::new_in(alloc);
    let Some(start) = left.iter().position(|l| *l > 0) else { return out };
    let mut at = start as u32;
    for _ in 0..=crates.len() {
        if let Some(first) = path.iter().position(|p| *p == at) {
            for &p in path[first..].iter() {
                out.push(crates[p as usize].name);
            }
            return out;
        }
        path.push(at);
        match crates[at as usize].deps.iter().find(|d| left[d.krate.index()] > 0) {
            Some(d) => at = d.krate.0,
            None => return out,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::string::ToString;

    use super::*;

    fn a() -> Alloc<'static> {
        Alloc::global()
    }

    /// A Rust 2024 library's implicit crates and prelude.
    fn rust() -> Implicit {
        blaze_unit_graph::implicit(edition::PackageEdition::of_manifest("2024").expect("an edition"), &blaze_unit_graph::UnitKind::Lib)
    }

    /// A blaze-edition library's.
    fn blaze() -> Implicit {
        blaze_unit_graph::implicit(edition::PackageEdition::of_manifest("blaze-2026.11").expect("the edition"), &blaze_unit_graph::UnitKind::Lib)
    }

    fn krate(name: &'static str, deps: &[(&'static str, u32)], implicit: Implicit) -> GraphCrate<'static, &'static str> {
        let mut d = AVec::new_in(a());
        for (n, k) in deps {
            d.push(GraphDep { name: *n, krate: CrateId(*k) });
        }
        GraphCrate { name, edition: Edition::E2024, implicit, features: AVec::new_in(a()), cfg: AVec::new_in(a()), deps: d }
    }

    fn graph(crates: std::vec::Vec<GraphCrate<'static, &'static str>>, sys: Sysroot) -> Result<CrateGraph<'static, &'static str>, GraphError<'static, &'static str>> {
        let mut v = AVec::new_in(a());
        v.extend(crates);
        CrateGraph::new_in(v, sys, a())
    }

    /// core, alloc on core, app on alloc and core: three waves, each after its dependencies.
    #[test]
    fn graph_orders_crates_in_waves() {
        let g = graph(
            std::vec![
                krate("app", &[("alloc", 2), ("core", 1)], rust()),
                krate("core", &[], rust()),
                krate("alloc", &[("core", 1)], rust()),
                krate("leaf", &[], rust()),
            ],
            Sysroot::default(),
        )
        .expect("no cycle");
        assert_eq!(g.order(), &[CrateId(1), CrateId(3), CrateId(2), CrateId(0)]);
        let waves: std::vec::Vec<std::vec::Vec<u32>> = g.waves().map(|w| w.iter().map(|c| c.0).collect()).collect();
        assert_eq!(waves, std::vec![std::vec![1, 3], std::vec![2], std::vec![0]]);
    }

    /// A cycle is a typed error naming exactly the crates on it.
    #[test]
    fn graph_cycle_is_a_typed_error_naming_its_crates() {
        let e = graph(
            std::vec![krate("a", &[("b", 1)], rust()), krate("b", &[("c", 2)], rust()), krate("c", &[("b", 1)], rust())],
            Sysroot::default(),
        )
        .expect_err("b and c depend on each other");
        match &e {
            GraphError::Cycle { crates } => assert_eq!(&crates[..], &["b", "c"]),
            other => panic!("not a cycle: {other:?}"),
        }
        assert_eq!(e.to_string(), "a cycle of crate dependencies: `b` -> `c` -> `b`");
    }

    fn names(list: &[GraphDep<&'static str>]) -> std::vec::Vec<(&'static str, u32)> {
        list.iter().map(|d| (d.name, d.krate.0)).collect()
    }

    /// Rust's injection: a std crate has `core` and `std`; a `#![no_std]` one `core` only and
    /// core's prelude; `#![no_core]` nothing; `alloc` is only nameable by `extern crate`.
    #[test]
    fn externs_follow_rusts_injection() {
        let sys = Sysroot { core: Some(CrateId(0)), alloc: Some(CrateId(1)), std: Some(CrateId(2)), bstd: None };
        let g = graph(
            std::vec![
                krate("core", &[], rust()),
                krate("alloc", &[], rust()),
                krate("std", &[], rust()),
                krate("app", &[("serde_x", 4)], rust()),
                krate("serde", &[], rust()),
            ],
            sys,
        )
        .expect("no cycle");
        let app = CrateId(3);
        let e = g.externs(app, RootAttrs::default());
        assert_eq!(names(&e.prelude), std::vec![("core", 0), ("serde_x", 4), ("std", 2)]);
        assert_eq!(names(&e.deps), std::vec![("alloc", 1), ("core", 0), ("serde_x", 4), ("std", 2)]);
        assert_eq!(e.macro_use, Some(CrateId(2)));
        assert_eq!(e.std_prelude, Some(PreludePath { krate: CrateId(2), path: ["prelude", "rust_2024"] }));
        let e = g.externs(app, RootAttrs { no_std: true, ..RootAttrs::default() });
        assert_eq!(names(&e.prelude), std::vec![("core", 0), ("serde_x", 4)]);
        assert_eq!(e.macro_use, Some(CrateId(0)));
        assert_eq!(e.std_prelude, Some(PreludePath { krate: CrateId(0), path: ["prelude", "rust_2024"] }));
        let e = g.externs(CrateId(0), RootAttrs { no_std: true, no_core: true, ..RootAttrs::default() });
        assert!(e.prelude.is_empty() && e.macro_use.is_none() && e.std_prelude.is_none());
    }

    /// Expected line: `test graph::tests::the_injected_crate_is_the_extern_preludes ... ok`.
    ///
    /// The injected `#[macro_use] extern crate` and the standard prelude name their crate by the
    /// extern prelude's entry, as every `extern crate` does (the Reference, "Extern prelude":
    /// "The `core` crate is always added to the extern prelude. The `std` crate is added as long
    /// as the `no_std` attribute is not specified in the crate root"): `libc`, whose
    /// dependencies name `core` and `std` (the macro side's copies, D89 (a)), is given those,
    /// never the sysroot's (the target side's). Before: the sysroot's, and on tempo libc's
    /// `use crate::prelude::*` bringing its side's `assert` beside the target core's was
    /// `Ambiguous(GlobVsOuter)`. The control: `app`, without such dependencies, keeps the
    /// sysroot's.
    #[test]
    fn the_injected_crate_is_the_extern_preludes() {
        let sys = Sysroot { core: Some(CrateId(0)), alloc: Some(CrateId(1)), std: Some(CrateId(2)), bstd: None };
        let g = graph(
            std::vec![
                krate("core", &[], rust()),
                krate("alloc", &[], rust()),
                krate("std", &[], rust()),
                krate("core", &[], rust()),
                krate("std", &[("core", 3)], rust()),
                krate("libc", &[("core", 3), ("std", 4)], rust()),
                krate("app", &[], rust()),
            ],
            sys,
        )
        .expect("no cycle");
        let libc = CrateId(5);
        let e = g.externs(libc, RootAttrs { no_std: true, ..RootAttrs::default() });
        assert_eq!(e.macro_use, Some(CrateId(3)), "the side's core, its extern prelude's `core`");
        assert_eq!(e.std_prelude, Some(PreludePath { krate: CrateId(3), path: ["prelude", "rust_2024"] }));
        let e = g.externs(libc, RootAttrs::default());
        assert_eq!(e.macro_use, Some(CrateId(4)), "the side's std, its extern prelude's `std`");
        assert_eq!(e.std_prelude.map(|p| p.krate), Some(CrateId(4)));
        let control = g.externs(CrateId(6), RootAttrs::default());
        assert_eq!(control.macro_use, Some(CrateId(2)), "the control: the sysroot's std");
        assert_eq!(control.std_prelude.map(|p| p.krate), Some(CrateId(2)));
    }

    /// The sysroot comes before every crate without a dependency line on it, in the order
    /// core, alloc, std, bstd; a crate std is built from (libc) comes before std and cannot
    /// name it, nor bstd; no implicit edge makes a cycle.
    #[test]
    fn graph_orders_the_sysroot_before_its_users() {
        let sys = Sysroot { core: Some(CrateId(0)), alloc: Some(CrateId(1)), std: Some(CrateId(2)), bstd: Some(CrateId(4)) };
        let g = graph(
            std::vec![
                krate("core", &[], rust()),
                krate("alloc", &[("core", 0)], rust()),
                krate("std", &[("alloc", 1), ("core", 0), ("libc", 3)], rust()),
                krate("libc", &[], rust()),
                krate("bstd", &[("blaze_core", 5)], rust()),
                krate("blaze_core", &[], rust()),
                krate("hello", &[], blaze()),
            ],
            sys,
        )
        .expect("the implicit edges make no cycle");
        let waves: std::vec::Vec<std::vec::Vec<u32>> = g.waves().map(|w| w.iter().map(|c| c.0).collect()).collect();
        assert_eq!(waves, std::vec![std::vec![0], std::vec![1], std::vec![3], std::vec![2], std::vec![5], std::vec![4], std::vec![6]]);
        let libc = g.externs(CrateId(3), RootAttrs { no_std: true, ..RootAttrs::default() });
        assert_eq!(names(&libc.deps), std::vec![("alloc", 1), ("core", 0)]);
        let blaze_core = g.externs(CrateId(5), RootAttrs::default());
        assert_eq!(names(&blaze_core.prelude), std::vec![("core", 0), ("std", 2)]);
        let hello = g.externs(CrateId(6), RootAttrs::default());
        assert_eq!(names(&hello.prelude), std::vec![("core", 0), ("std", 4)]);
    }

    /// Under a blaze edition `std` names bstd and the prelude is bstd's edition module;
    /// Rust's `std` has no name.
    #[test]
    fn externs_under_a_blaze_edition_name_bstd() {
        let sys = Sysroot { core: Some(CrateId(0)), alloc: Some(CrateId(1)), std: Some(CrateId(2)), bstd: Some(CrateId(3)) };
        let g = graph(
            std::vec![
                krate("core", &[], rust()),
                krate("alloc", &[], rust()),
                krate("std", &[], rust()),
                krate("bstd", &[], rust()),
                krate("hello", &[], blaze()),
            ],
            sys,
        )
        .expect("no cycle");
        let e = g.externs(CrateId(4), RootAttrs::default());
        assert_eq!(names(&e.prelude), std::vec![("core", 0), ("std", 3)]);
        assert!(e.deps.iter().all(|d| d.krate != CrateId(2)), "Rust's std is nameable: {:?}", names(&e.deps));
        assert_eq!(e.macro_use, Some(CrateId(3)));
        assert_eq!(e.std_prelude, Some(PreludePath { krate: CrateId(3), path: ["prelude", "blaze_2026_11"] }));
        assert!(e.dropped.is_empty(), "{:?}", e.dropped);
    }

    /// The Rust Reference, "Extern prelude": a crate without `#![no_std]` has `std` in its
    /// extern prelude, and under a Rust edition that is Rust's `std`, also for a crate bstd is
    /// built from (blaze_mem, Rust 2024, `use std::ffi::OsStr`). A graph made with bstd but no
    /// Rust `std` cannot give it: the drop is named, never silent; a `#![no_std]` crate loses
    /// nothing; a crate std is built from cannot name std, and says so.
    #[test]
    fn a_dropped_implicit_crate_is_named() {
        let sys = Sysroot { core: Some(CrateId(0)), alloc: Some(CrateId(1)), std: None, bstd: Some(CrateId(2)) };
        let g = graph(
            std::vec![
                krate("core", &[], rust()),
                krate("alloc", &[("core", 0)], rust()),
                krate("bstd", &[("blaze_mem", 3)], rust()),
                krate("blaze_mem", &[], rust()),
            ],
            sys,
        )
        .expect("no cycle");
        let mem = g.externs(CrateId(3), RootAttrs::default());
        assert_eq!(names(&mem.prelude), std::vec![("core", 0)]);
        assert_eq!(&mem.dropped[..], &[DroppedExtern { name: "std", why: ExternGone::NotInGraph }]);
        let bare = g.externs(CrateId(3), RootAttrs { no_std: true, ..RootAttrs::default() });
        assert!(bare.dropped.is_empty(), "{:?}", bare.dropped);
        let sys = Sysroot { core: Some(CrateId(0)), alloc: Some(CrateId(1)), std: Some(CrateId(2)), bstd: None };
        let g = graph(
            std::vec![krate("core", &[], rust()), krate("alloc", &[("core", 0)], rust()), krate("std", &[("libc", 3)], rust()), krate("libc", &[], rust())],
            sys,
        )
        .expect("no cycle");
        let libc = g.externs(CrateId(3), RootAttrs::default());
        assert_eq!(&libc.dropped[..], &[DroppedExtern { name: "std", why: ExternGone::BuiltInto { krate: CrateId(2) } }]);
    }
}
