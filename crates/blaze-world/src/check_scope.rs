//! Which crates one owner's bodies are checked against: the ONE rule of the records path
//! (AGENTS.md rule 30), which `blaze check`'s body job (blaze-cli body_types.rs `prepare`) takes
//! for every owner of a file. front2's names path reads the same rule over its resolved names
//! (blaze-ladder front2 linked.rs `Linked`); this is its reading over the world's crate list,
//! which the records path has before any crate's names are read.
//!
//! The rule:
//! - An owner's bodies are checked on ONE side ([`check_side`]): the side of the crates it
//!   links, which the list gives each crate on one side. A proc-macro crate's bodies are built
//!   for and run on the macro side (D89, rule 10: `wasm32-wasip1`, in Wasmtime). So is a member
//!   proc macro's library test harness (the same code, blaze-scope units/macro_crate.rs): the
//!   list points its edges at the macro side's copies (units/macro_side.rs) while the world
//!   files it among the target side's crates, so its side is read from its edges, never from
//!   its row (tempo, 2026-11-17 03:49: `tempo-precompiles-macros`'s harness was checked on the
//!   target side and its first edge refused, which failed 208 body tasks of the seven files the
//!   proc macro and its harness own). A macro-side copy of a crate (a helper both sides build)
//!   is a crate of its own, under a key of its own (the side's domain, units/macro_side.rs
//!   `side_key`), so each side's copy is an owner with its own scope. An owner that links
//!   nothing but proc macros is checked on its own side.
//! - A DEPENDENCY's proc macro on the target side (a registry or git proc macro the program
//!   names: `clap_derive`, `serde_derive`, `thiserror_impl`) has no check scope
//!   ([`ScopeError::MacrosOnly`], [`has_check_scope`]): the list keeps it for its macros only
//!   (blaze-scope units.rs `unit_crates`, "a dependency's come from the module store"), holds
//!   none of its dependencies, and gives it the target toolchain's `proc_macro` only so that its
//!   names resolve (list.rs `world`). Nothing of it is compiled or checked from this list, and
//!   by Rust's rule its code is built for the side its macros run on with that side's
//!   `proc_macro`, which this list does not hold for it: a scope made of what the list holds
//!   would be the target side's, which is not where its bodies are built. So nothing asks for
//!   one: every planner of per-crate work over the list asks [`has_check_scope`] first (rule
//!   77: a task that can only fail is not planned), and no reader needs its impls (a proc macro
//!   is never linked: [`Seen::MacrosOnly`]). Before, its scope was asked and refused as
//!   [`ScopeError::OtherSide`] (`clap_derive` -> `proc_macro`, 2026-11-17, the three derive
//!   projects), which read as a broken list. A macro-side copy of a dependency's proc macro
//!   (reached from a member's proc macro) is given the macro side's toolchain crates by the
//!   list (units/macro_side.rs `add_side_externs`), so its scope is its side's.
//! - The owner sees its dependency closure on that side: its dependencies followed to the end
//!   (a work list, no recursion), and, on the target side, the target's sysroot
//!   ([`World::sysroot`]: core, alloc, std, bstd), which the Reference's extern prelude adds
//!   unless the owner is `#![no_core]`. The macro side's own core, alloc, std and `proc_macro`
//!   are a member proc macro's dependencies in the list (list.rs), so its walk reaches them.
//! - A proc-macro crate the walk reaches (not the owner) is seen for its MACROS only
//!   ([`Seen::MacrosOnly`]): the owner's paths resolve `mac::same!` through its names, but it
//!   is never linked into the owner's program (it runs at build time in the sandbox), so its
//!   impls answer no question of the owner's, it adds no lang row, and its dependencies (the
//!   macro side's crates) are not followed.
//! - A crate of the other side reached any other way (an edge from a crate of the scope to a
//!   crate of the other side, with no proc macro between) is a world that broke the list's
//!   promise (list.rs: a member proc macro's dependencies are the macro side's, everything else
//!   the target's): refused naming the owner and the edge's two crates
//!   ([`ScopeError::OtherSide`]), never joined.
//! - Build scripts are no owners: the world holds no crate of a build script or of its
//!   build-dependencies (lib.rs: "Build scripts are not crates of the names set"), so neither
//!   reaches this rule. Owed, stated: `blaze check` does not check a `build.rs` yet.
//! - The lang crates (the crates whose `#[lang]` rows the owner's table files): the owner, and,
//!   unless it is `#![no_core]`, every TOOLCHAIN crate of the check side it sees, chosen by id
//!   and side, never by a name both sides share. So the two sides' cores never meet in one lang
//!   table, nor their impls in one impl table: before, the owner's set followed `mac` into the
//!   macro side and chose lang crates by name ("the lang item `AsyncGenFinished` is declared
//!   twice: .. (crate 4 of the records)", pm-small, 2026-11-16 19:51).
//!
//! Keys (rule 21): the scope is a function of the world (the unit graph, the lock and the
//! toolchain it was made from: deps, sides, proc-macro kinds, toolchain origins) and of the
//! owner's `#![no_core]` (its names record's root head). The body job's read list holds the unit
//! graph's out and every seen crate's `CrateScope` (name, out) (body_types.rs `FileTypes::new`),
//! and the owner's `CrateScope` holds its root head: a change to any of them moves a read, and
//! the body is checked again. A file both sides build (a helper of a member proc macro and of
//! a target crate) has an owner per side, each a crate under its own key (the macro side's
//! under units/macro_side.rs `SIDE_DOMAIN`): the file's one body task checks each owner in its
//! own scope (blaze-cli body_types.rs `prepare`, every crate whose root reaches the file), and
//! its read list names each owner's `CrateScope` by that key, so the side is in what it reads.
//!
//! Owed, stated: the lang rows of a non-toolchain crate the owner links (a library's
//! `#[global_allocator]` or `#[panic_handler]`) are not filed on the records path; the names
//! path files every linked crate's (front2 linked.rs). Filing them here opens every seen
//! crate's items record per owner, which the lazy records path avoids today.

use blaze_own::{AVec, Alloc, AllocFailure};

use crate::crate_list::CrateGraph;
use crate::{Side, World};

/// How an owner sees a crate of its scope.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Seen {
    /// The owner itself: its items, impls and lang rows are its program's.
    Owner,
    /// A crate the owner links on its side: its items and impls answer the owner's questions.
    /// `toolchain`: a toolchain crate of the check side, whose lang rows are filed too.
    Linked { toolchain: bool },
    /// A proc-macro crate the owner names: its macros only (no impl, no lang row; its
    /// dependencies not followed).
    MacrosOnly,
}

/// The crates one owner's bodies are checked against, and how each is seen.
pub struct CheckScope<'a> {
    /// The owner first, then the rest by world index, each once.
    readers: AVec<'a, (usize, Seen)>,
    /// The side the owner's bodies are checked on.
    side: Side,
    /// The owner is `#![no_core]`: no toolchain crate's lang rows are its.
    no_core: bool,
}

impl CheckScope<'_> {
    /// The seen crates (world indexes), the owner first, then by index.
    pub fn crates(&self) -> impl Iterator<Item = usize> + '_ {
        self.readers.iter().map(|(at, _)| *at)
    }

    /// Each seen crate with how it is seen, in [`CheckScope::crates`]'s order.
    pub fn readers(&self) -> &[(usize, Seen)] {
        &self.readers
    }

    /// The side the owner's bodies are checked on.
    pub fn side(&self) -> Side {
        self.side
    }

    /// The places (in [`CheckScope::crates`]'s order) of the lang crates: the owner, and,
    /// unless it is `#![no_core]`, the check side's toolchain crates it links.
    pub fn lang_places(&self) -> impl Iterator<Item = usize> + '_ {
        let no_core = self.no_core;
        self.readers.iter().enumerate().filter_map(move |(place, (_, seen))| match seen {
            Seen::Owner => Some(place),
            Seen::Linked { toolchain: true } if !no_core => Some(place),
            Seen::Linked { .. } | Seen::MacrosOnly => None,
        })
    }
}

/// Why an owner has no check scope (rule 19: the crate named).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeError<'w> {
    /// The owner's index is past the world's crates.
    NoSuchCrate { krate: usize, crates: usize },
    /// The owner is a crate the world could not make ([`World::refused`]: a member proc macro
    /// with no `wasm32-wasip1` cut on the shelf): its bodies have no side to be checked on.
    Refused { krate: &'w str },
    /// The owner is a dependency's proc macro on the target side, kept for its macros only
    /// (the module's rule): nothing of it is checked from this list, so it has no scope.
    /// [`has_check_scope`] says so before any task is planned for it.
    MacrosOnly { krate: &'w str },
    /// The owner's scope holds an edge `from` -> `krate` to a crate of the other side than the
    /// owner's check side `side`, with no proc macro between (`from` is the owner itself for its
    /// own edges and the target's sysroot).
    OtherSide { owner: &'w str, from: &'w str, krate: &'w str, side: Side },
    /// The allocator the caller handed in refused the memory of the owner's scope (its
    /// by-index marks, its work list or its readers): the caller's job fails with it (rule 20),
    /// the process goes on.
    Memory { owner: &'w str, why: AllocFailure },
}

impl core::fmt::Display for ScopeError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ScopeError::NoSuchCrate { krate, crates } => write!(f, "an owner is crate {krate}, and the world holds {crates} crates (blaze-world check_scope.rs)"),
            ScopeError::Refused { krate } => {
                write!(f, "the crate `{krate}` was refused by the crate list (its refusal is in the world), so its bodies have no side to be checked on (blaze-world check_scope.rs)")
            }
            ScopeError::MacrosOnly { krate } => write!(
                f,
                "the crate `{krate}` is a dependency's proc macro, kept in the crate list for its macros only: none of its bodies is checked from this list (its dependencies are not in it), so it has no check scope, and no task should ask for one (blaze-world check_scope.rs `has_check_scope`)"
            ),
            ScopeError::OtherSide { owner, from, krate, side } => write!(
                f,
                "the bodies of `{owner}` are checked on the {side:?} side, and the edge `{from}` -> `{krate}` reaches `{krate}` of the other side with no proc macro between: the crate list broke its promise (blaze-world check_scope.rs, list.rs)"
            ),
            ScopeError::Memory { owner, why } => write!(f, "the check scope of `{owner}` could not be made: {why} (blaze-world check_scope.rs `check_scope_in`)"),
        }
    }
}

impl ScopeError<'_> {
    /// The refusal as one fixed sentence (for a caller that carries only `&'static str`; the
    /// crate is named by [`core::fmt::Display`], and a refused crate's own names task names it).
    pub fn rule(&self) -> &'static str {
        match self {
            ScopeError::NoSuchCrate { .. } => "an owner past the world's crates (blaze-world check_scope.rs)",
            ScopeError::Refused { .. } => "the owner was refused by the crate list, so its bodies have no side to be checked on (blaze-world check_scope.rs)",
            ScopeError::MacrosOnly { .. } => "a dependency's proc macro is kept for its macros only and has no check scope (blaze-world check_scope.rs `has_check_scope`)",
            ScopeError::OtherSide { .. } => "an edge of an owner's check scope crosses to the other side with no proc macro between (blaze-world check_scope.rs `OtherSide`)",
            ScopeError::Memory { .. } => "the allocator refused the memory of an owner's check scope (blaze-world check_scope.rs `check_scope_in`)",
        }
    }
}

/// The side `owner`'s bodies are checked on (the module's first rule): the side of the crates
/// it links (its edges to crates that are not proc macros), all of which must be on one side;
/// a proc-macro crate's is the macro side (rule 10), and its edges must be the macro side's
/// too; an owner that links nothing is checked on its own side. Two edges to two sides are
/// [`ScopeError::OtherSide`] naming the owner and the edge that differs. A dependency's proc
/// macro has no side to be checked on: [`ScopeError::MacrosOnly`].
pub fn check_side<'n, G: CrateGraph<'n>>(graph: &G, owner: usize) -> Result<Side, ScopeError<'n>> {
    if macros_only(graph, owner) {
        return Err(ScopeError::MacrosOnly { krate: graph.name(owner) });
    }
    let mut side = if graph.proc_macro(owner) { Some(Side::Macro) } else { None };
    for dep in graph.deps(owner) {
        if graph.proc_macro(dep) {
            continue;
        }
        let of = graph.side(dep);
        match side {
            None => side = Some(of),
            Some(side) if side != of => return Err(ScopeError::OtherSide { owner: graph.name(owner), from: graph.name(owner), krate: graph.name(dep), side }),
            Some(_) => {}
        }
    }
    Ok(side.unwrap_or(graph.side(owner)))
}

/// Crate `at` is a dependency's proc macro, kept for its macros only (the module's rule): a
/// proc macro whose row the list put on the target side. A member's proc macro the list builds
/// is on the macro side (list.rs `world`: `UnitCrates::macro_side`), and one it refused for want
/// of that side is refused ([`ScopeError::Refused`]), so the row's side tells the two apart.
fn macros_only<'n, G: CrateGraph<'n>>(graph: &G, at: usize) -> bool {
    graph.proc_macro(at) && graph.side(at) == Side::Target && !graph.refused(at)
}

/// Whether crate `at` of `graph` has a check scope ([`check_scope_in`] makes one): it is a
/// crate of the graph, the list did not refuse it ([`ScopeError::Refused`]: its refusal is
/// reported where its names are asked), and it is not a dependency's proc macro kept for its
/// macros only ([`ScopeError::MacrosOnly`]). A planner of per-crate work that needs a crate's
/// scope (the Heads stage's impl tables, blaze-cli engine_heads.rs `plan`) plans none for a
/// crate without one (rule 77): such a task could only fail, and nothing reads its result.
pub fn has_check_scope<'n, G: CrateGraph<'n>>(graph: &G, at: usize) -> bool {
    at < graph.len() && !graph.refused(at) && !macros_only(graph, at)
}

/// How the owner's walk sees crate `at`, reached by an edge from `from`, a crate of its scope:
/// a proc macro for its macros; else a crate of `side`, a toolchain crate or not; else the
/// refusal naming the edge.
fn seen_as<'n, G: CrateGraph<'n>>(graph: &G, owner: usize, from: usize, at: usize, side: Side) -> Result<Seen, ScopeError<'n>> {
    if graph.proc_macro(at) {
        return Ok(Seen::MacrosOnly);
    }
    if graph.side(at) != side {
        return Err(ScopeError::OtherSide { owner: graph.name(owner), from: graph.name(from), krate: graph.name(at), side });
    }
    Ok(Seen::Linked { toolchain: graph.toolchain(at) })
}

/// The check scope of `owner` (a world index) by the module's rule; `no_core`: the owner's
/// root says `#![no_core]` (its names record's root head).
pub fn check_scope<'w, 'a>(world: &World<'w>, owner: usize, no_core: bool, alloc: Alloc<'a>) -> Result<CheckScope<'a>, ScopeError<'w>> {
    check_scope_in(world, owner, no_core, alloc)
}

/// [`check_scope`] over any crate graph: the world made fresh, or the crate list read in place
/// (crate_list.rs), whose indices are its rows. One rule, two inputs, tested equal
/// (list_tests.rs `the_crate_list_reads_back_as_its_world`).
///
/// Memory: everything is made in `alloc`, each vector at its full size up front (the marks `n`
/// bytes, the work list `8n`, the readers 16 bytes a crate seen), so nothing grows and nothing
/// is copied; the marks and the work list are dropped at return. A caller on a bump frame
/// therefore releases a call's bytes around it (blaze-cli body_types.rs `owner_wants`, a frame
/// scope), or hands a heap that frees them. A refusal is [`ScopeError::Memory`], never a panic.
pub fn check_scope_in<'n, 'a, G: CrateGraph<'n>>(graph: &G, owner: usize, no_core: bool, alloc: Alloc<'a>) -> Result<CheckScope<'a>, ScopeError<'n>> {
    let n = graph.len();
    if owner >= n {
        return Err(ScopeError::NoSuchCrate { krate: owner, crates: n });
    }
    if graph.refused(owner) {
        return Err(ScopeError::Refused { krate: graph.name(owner) });
    }
    let side = check_side(graph, owner)?;
    let memory = |why: AllocFailure| ScopeError::Memory { owner: graph.name(owner), why };
    // By index, how each crate is seen (`None`: not seen); the work list in the order reached,
    // which holds each crate at most once, so `n` slots and no growth (the pushes below and
    // this `extend` stay within the capacity reserved here, so they allocate nothing).
    let mut seen: AVec<'a, Option<Seen>> = AVec::try_with_capacity_in(n, alloc).map_err(memory)?;
    seen.extend((0..n).map(|_| None));
    let mut order: AVec<'a, usize> = AVec::try_with_capacity_in(n, alloc).map_err(memory)?;
    seen[owner] = Some(Seen::Owner);
    order.push(owner);
    // The target side's sysroot, which the extern prelude adds (unless `#![no_core]`); the
    // macro side's toolchain crates are the owner's own dependencies in the list.
    if !no_core && side == Side::Target {
        for at in graph.sysroot().iter().flatten().copied() {
            if seen[at].is_none() {
                seen[at] = Some(seen_as(graph, owner, owner, at, side)?);
                order.push(at);
            }
        }
    }
    let mut next = 0;
    while next < order.len() {
        let at = order[next];
        next += 1;
        if seen[at] == Some(Seen::MacrosOnly) {
            continue;
        }
        for dep in graph.deps(at) {
            if seen[dep].is_none() {
                seen[dep] = Some(seen_as(graph, owner, at, dep, side)?);
                order.push(dep);
            }
        }
    }
    // The owner at 0, then the rest by index: the same graph gives the same order.
    order[1..].sort_unstable();
    let mut readers: AVec<'a, (usize, Seen)> = AVec::try_with_capacity_in(order.len(), alloc).map_err(memory)?;
    for at in order.iter().copied() {
        if let Some(how) = seen[at] {
            readers.push((at, how));
        }
    }
    Ok(CheckScope { readers, side, no_core })
}
