//! The crates one program links, from its roots: front2's one reading of Rust's link rule
//! (rule 30), which the lang index ([`super::index::Index::new`]) and the start rule
//! ([`super::start::linked_lang_start`]) both take.
//!
//! The rule: a crate links what it names: its extern prelude (its dependencies, and the `core`
//! and `std` the Reference's "Preludes" add, less what `#![no_std]` and `#![no_core]` take away:
//! "If the `no_std` attribute is specified, then the `std` crate is not added to the extern
//! prelude"; blaze-scope graph.rs `externs` is the one reading of it) and each `extern crate` it
//! writes; linking is transitive; a proc-macro unit is never linked (it runs at build time in
//! the sandbox).
//!
//! Why a program's lang rows are filed from its link closure and never from every crate of a
//! workspace: the attributes that name ONE item per program are scoped to the program's
//! dependency graph, not to a workspace. The Reference (the pinned nightly's copy, panic.html,
//! as panic_entry.rs quotes it): [panic.panic_handler.unique] "There must be a single
//! panic_handler function in the dependency graph."; the pinned library's
//! library/std/src/alloc.rs:53: "The `#[global_allocator]` can only be used once in a crate
//! or its recursive dependencies." A workspace with two binaries, each with its own
//! `#[global_allocator]`, is two programs, each with one allocator.
//!
//! A [`Linked`] is made FOR its roots: what it holds is a function of the names and the roots
//! alone (rule 21: the roots are its key), and two programs with different roots never share
//! one. The per-crate lang rows stay the names library's per-crate tables (each crate's work
//! done once, rule 77); a program's table is a join over the closure, a scan of a bitmap.

use blaze_id::ItemKey as LibKey;
use blaze_own::{Alloc, Vec as OwnVec};
use blaze_scope::graph::CrateId;
use blaze_scope::resolve::Names;
use blaze_surface::ExternTarget;

/// Why a program's roots name no crate set (rule 19: the root named).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootsError {
    /// A root crate index past the names' crates.
    NoSuchCrate { krate: usize, crates: usize },
    /// A root fn's key that is of no crate of the names.
    NoCrateOf { root: LibKey },
}

impl core::fmt::Display for RootsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RootsError::NoSuchCrate { krate, crates } => {
                write!(f, "a program's root is crate {krate}, and the names hold {crates} crates (front2 linked.rs)")
            }
            RootsError::NoCrateOf { root } => write!(f, "a program's root {:#x} is of no crate of the names (front2 linked.rs)", root.0),
        }
    }
}

/// The link rule's proc-macro clause, the one home of it: a proc-macro unit is linked only as
/// its program's own root (a proc macro's bodies are its own program's, checked on the macro
/// side: blaze-world check_scope.rs); one a program reaches is never linked (it runs at build
/// time in the sandbox), so its impls answer none of that program's questions and it adds no
/// lang row. [`Linked::of`] reads it over many roots; the records path, whose program is crate
/// 0 of its records, reads it in buckets.rs `Buckets::admit` and buckets/asked.rs
/// `Buckets::admit_asked`.
pub fn links_unit(root: bool, proc_macro: bool) -> bool {
    root || !proc_macro
}

/// The crates a program of the roots links: the roots, sorted and each once, and by crate
/// index whether it is linked.
pub struct Linked<'a> {
    roots: OwnVec<usize, Alloc<'a>>,
    linked: OwnVec<bool, Alloc<'a>>,
}

impl<'a> Linked<'a> {
    /// The link closure of the crates `roots` (indexes of `names`): a work list over the
    /// graph (no recursion), each linked crate's links read once. A root is linked whatever
    /// its kind; a proc-macro unit reached from one is not. No roots: nothing is linked (a
    /// program of no crate), never a guessed root.
    pub fn of(names: &Names<'_>, roots: &[usize], alloc: Alloc<'a>) -> Result<Linked<'a>, RootsError> {
        let crates = names.crates();
        let graph = names.graph();
        let closure = Linked::closure(
            crates.len(),
            roots,
            alloc,
            |at, link| {
                let c = crates[at];
                let externs = graph.externs(CrateId(at as u32), c.root_attrs);
                for dep in externs.prelude.iter() {
                    link(dep.krate.index());
                }
                // `extern crate name;` in any module (lowered under the crate's cfg): `name` is
                // what `extern crate` may name (`externs.deps`); one it cannot is the names
                // library's refusal, already made. `extern crate self` links nothing new.
                for (_, (_, decls)) in c.decls.iter() {
                    for e in decls.extern_crates.iter() {
                        if let ExternTarget::Dep(name) = &e.target {
                            if let Some(dep) = externs.deps.iter().find(|d| d.name == *name) {
                                link(dep.krate.index());
                            }
                        }
                    }
                }
                Ok::<(), core::convert::Infallible>(())
            },
            |i| Ok(crates[i].implicit.proc_macro),
        );
        match closure {
            Ok(linked) => linked,
            Err(never) => match never {},
        }
    }

    /// The link closure over ANY reading of each crate's links (the one work list, rule 30):
    /// `links(at, link)` calls `link(i)` for each crate `at` names (its extern prelude and each
    /// `extern crate` it writes), `proc_macro(i)` says whether crate `i` is a proc-macro unit.
    /// A reader that cannot answer stops the closure with its own error `E` (the records'
    /// reader, front2 source.rs, files why in its log); a link past `crates` is
    /// [`RootsError::NoSuchCrate`].
    pub fn closure<E>(
        crates: usize,
        roots: &[usize],
        alloc: Alloc<'a>,
        mut links: impl FnMut(usize, &mut dyn FnMut(usize)) -> Result<(), E>,
        proc_macro: impl Fn(usize) -> Result<bool, E>,
    ) -> Result<Result<Linked<'a>, RootsError>, E> {
        let mut sorted: OwnVec<usize, Alloc<'a>> = OwnVec::with_capacity_in(roots.len(), alloc);
        for &krate in roots {
            if krate >= crates {
                return Ok(Err(RootsError::NoSuchCrate { krate, crates }));
            }
            sorted.push(krate);
        }
        sorted.sort_unstable();
        sorted.dedup();
        let mut linked: OwnVec<bool, Alloc<'a>> = OwnVec::from_elem_in(false, crates, alloc);
        let mut work: OwnVec<usize, Alloc<'a>> = OwnVec::with_capacity_in(sorted.len(), alloc);
        for &krate in sorted.iter() {
            linked[krate] = true;
            work.push(krate);
        }
        while let Some(at) = work.pop() {
            // What `at` names, gathered first: `proc_macro` is asked of each outside the reader.
            let mut named: OwnVec<usize, Alloc<'a>> = OwnVec::new_in(alloc);
            links(at, &mut |i| named.push(i))?;
            for i in named {
                if i >= crates {
                    return Ok(Err(RootsError::NoSuchCrate { krate: i, crates }));
                }
                if !linked[i] && links_unit(false, proc_macro(i)?) {
                    linked[i] = true;
                    work.push(i);
                }
            }
        }
        Ok(Ok(Linked { roots: sorted, linked }))
    }

    /// The link closure of the crates of the root fns `roots` (each a library key, of any
    /// crate): a root of no crate is [`RootsError::NoCrateOf`].
    pub fn of_keys(names: &Names<'_>, roots: &[LibKey], alloc: Alloc<'a>) -> Result<Linked<'a>, RootsError> {
        let mut krates: OwnVec<usize, Alloc<'a>> = OwnVec::with_capacity_in(roots.len(), alloc);
        for &root in roots {
            krates.push(names.crate_of(root).ok_or(RootsError::NoCrateOf { root })?);
        }
        Linked::of(names, &krates, alloc)
    }

    /// The roots, sorted, each once: the key of what this holds (with the names).
    pub fn roots(&self) -> &[usize] {
        &self.roots
    }

    /// Whether crate `krate` is linked (a root, or reached from one).
    pub fn links(&self, krate: usize) -> bool {
        self.linked.get(krate).copied().unwrap_or(false)
    }

    /// The linked crates, in crate order.
    pub fn crates(&self) -> impl Iterator<Item = usize> + '_ {
        self.linked.iter().enumerate().filter_map(|(k, &l)| l.then_some(k))
    }
}
