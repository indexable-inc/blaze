//! Which crates one owner's bodies are checked against: the ONE rule of the records path
//! (AGENTS.md rule 30), which `blaze check`'s body job (blaze-cli body_types.rs `prepare`) takes
//! for every owner of a file. front2's names path reads the same rule over its resolved names
//! (blaze-ladder front2 linked.rs `Linked`); this is its reading over the world's crate list,
//! which the records path has before any crate's names are read.
//!
//! The rule:
//! - An owner's bodies are checked on ONE side ([`check_side`]): a proc-macro crate's bodies are
//!   built for and run on the macro side (D89, rule 10: `wasm32-wasip1`, in Wasmtime), so they
//!   are checked against the macro side's crates; every other crate's on its own side.
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
//! - A crate of the other side reached any other way is a world that broke the list's promise
//!   (list.rs: a member proc macro's dependencies are the macro side's, everything else the
//!   target's): refused by name ([`ScopeError::OtherSide`]), never joined.
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
//! the body is checked again.
//!
//! Owed, stated: the lang rows of a non-toolchain crate the owner links (a library's
//! `#[global_allocator]` or `#[panic_handler]`) are not filed on the records path; the names
//! path files every linked crate's (front2 linked.rs). Filing them here opens every seen
//! crate's items record per owner, which the lazy records path avoids today.

use blaze_own::{AVec, Alloc};

use crate::{Crate, Origin, Side, World};

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
    /// The owner's walk reached a crate of the other side not through a proc macro.
    OtherSide { owner: &'w str, krate: &'w str, side: Side },
}

impl core::fmt::Display for ScopeError<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ScopeError::NoSuchCrate { krate, crates } => write!(f, "an owner is crate {krate}, and the world holds {crates} crates (blaze-world check_scope.rs)"),
            ScopeError::Refused { krate } => {
                write!(f, "the crate `{krate}` was refused by the crate list (its refusal is in the world), so its bodies have no side to be checked on (blaze-world check_scope.rs)")
            }
            ScopeError::OtherSide { owner, krate, side } => write!(
                f,
                "the bodies of `{owner}` are checked on the {side:?} side, and its dependencies reach `{krate}` of the other side not through a proc macro: the crate list broke its promise (blaze-world check_scope.rs, list.rs)"
            ),
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
            ScopeError::OtherSide { .. } => "an owner's dependencies reach the other side not through a proc macro (blaze-world check_scope.rs)",
        }
    }
}

/// The side a crate's own bodies are checked on: a proc-macro crate's on the macro side (they
/// are built for it and run there), any other crate's on its own.
pub fn check_side(c: &Crate<'_>) -> Side {
    if c.proc_macro { Side::Macro } else { c.side }
}

/// How the owner's walk sees `c`, reached from a crate of its scope: a proc macro for its
/// macros; else a crate of `side`, a toolchain crate or not; else the refusal.
fn seen_as<'w>(world: &World<'w>, owner: usize, at: usize, side: Side) -> Result<Seen, ScopeError<'w>> {
    let c = &world.crates[at];
    if c.proc_macro {
        return Ok(Seen::MacrosOnly);
    }
    if c.side != side {
        return Err(ScopeError::OtherSide { owner: world.crates[owner].input.name, krate: c.input.name, side });
    }
    Ok(Seen::Linked { toolchain: matches!(c.origin, Origin::Toolchain(_)) })
}

/// The check scope of `owner` (a world index) by the module's rule; `no_core`: the owner's
/// root says `#![no_core]` (its names record's root head).
pub fn check_scope<'w, 'a>(world: &World<'w>, owner: usize, no_core: bool, alloc: Alloc<'a>) -> Result<CheckScope<'a>, ScopeError<'w>> {
    let crates = &world.crates;
    if owner >= crates.len() {
        return Err(ScopeError::NoSuchCrate { krate: owner, crates: crates.len() });
    }
    if world.refusal(owner).is_some() {
        return Err(ScopeError::Refused { krate: crates[owner].input.name });
    }
    let side = check_side(&crates[owner]);
    // By world index, how each crate is seen (`None`: not seen); the work list in the order
    // reached.
    let mut seen: AVec<'a, Option<Seen>> = AVec::with_capacity_in(crates.len(), alloc);
    seen.extend((0..crates.len()).map(|_| None));
    let mut order: AVec<'a, usize> = AVec::new_in(alloc);
    seen[owner] = Some(Seen::Owner);
    order.push(owner);
    // The target side's sysroot, which the extern prelude adds (unless `#![no_core]`); the
    // macro side's toolchain crates are the owner's own dependencies in the list.
    if !no_core && side == Side::Target {
        let sysroot = [world.sysroot.core, world.sysroot.alloc, world.sysroot.std, world.sysroot.bstd];
        for at in sysroot.iter().flatten().map(|id| id.0 as usize) {
            if seen[at].is_none() {
                seen[at] = Some(seen_as(world, owner, at, side)?);
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
        for (_, dep) in crates[at].input.deps.iter() {
            if seen[*dep].is_none() {
                seen[*dep] = Some(seen_as(world, owner, *dep, side)?);
                order.push(*dep);
            }
        }
    }
    // The owner at 0, then the rest by index: the same world gives the same order.
    order[1..].sort_unstable();
    let mut readers: AVec<'a, (usize, Seen)> = AVec::with_capacity_in(order.len(), alloc);
    for at in order.iter().copied() {
        if let Some(how) = seen[at] {
            readers.push((at, how));
        }
    }
    Ok(CheckScope { readers, side, no_core })
}
