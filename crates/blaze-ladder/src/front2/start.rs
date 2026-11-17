//! How a program's process starts, as front2 answers it (LADDER.md L1, "The Rust entry through
//! `#[lang = "start"]`"): the one place that says which fn the walk starts at and, for a Rust
//! program, which library item runs `main`.
//!
//! The rule, from the Rust Reference, "Crates and source files" (`crate.main`): "A crate that
//! contains a `main` function can be compiled to an executable. If a `main` function is present,
//! it must take no arguments, must not declare any trait or lifetime bounds, must not have any
//! where clauses, and its return type must implement the `Termination` trait." The library runs
//! it: the pinned std (nightly-2026-10-27, library/std/src/rt.rs:197) declares
//!
//! ```text
//! #[cfg(not(any(test, doctest)))]
//! #[lang = "start"]
//! fn lang_start<T: crate::process::Termination + 'static>(
//!     main: fn() -> T, argc: isize, argv: *const *const u8, sigpipe: u8) -> isize
//! ```
//!
//! which sets the main thread up (`rt::init`), calls `main`, turns its value into the exit
//! status through `Termination::report` (library/std/src/process.rs: "Is called to get the
//! representation of the value as status code"), and flushes stdout in `rt::cleanup`. Calling a
//! Rust `fn main` directly skips all of that, so a Rust program with std starts through
//! `lang_start::<T>`, `T` its `main`'s return type: [`Entry2::Rust`]. The builder makes the C
//! entry that calls it (L1b); this file only names the two fns and `T`.
//!
//! The four starts ([`Entry2`]): bstd's hosted entry (a blaze edition: the
//! `bstd_hosted_start` that `std::main` makes of the root's `fn main`, D104), a C `main` the
//! program exports itself (`#![no_main]`), the Rust start, and the crate root's `fn main`
//! called directly: a program that links no `#[lang = "start"]` (a `#![no_core]` root, a
//! `#![no_std]` root that writes no `extern crate std`, a program run with no library at
//! all). Which one is decided from what the crate graph links ([`linked_lang_start`]), never from the edition
//! alone; a Rust program that links std never takes the direct call.

use std::{boxed::Box, string::String};

use blaze_id::ItemKey as LibKey;
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_typeck as tc;
use blaze_typeck::PTy;
use blaze_unit_graph::StdIs;

use super::body::BodyError;
use super::heads::{Store, Want};
use super::keys::{Keys, checker_of};
use super::program::Front2Refusal;

/// The `#[lang = ".."]` name of the library's Rust start (std rt.rs `lang_start`).
pub const LANG_START: &str = "start";

/// A Rust program's start: `lang_start::<returns>(main as fn() -> returns, argc, argv, 0)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RustStart {
    /// The library item `#[lang = "start"]` declares (std's `rt::lang_start`).
    pub lang_start: LibKey,
    /// The crate root's `fn main`.
    pub main: LibKey,
    /// `main`'s return type, `lang_start`'s `T`: a type of the store's table
    /// ([`Store::tys`]), which the program's table forks, so the same `PTy` in both.
    pub returns: PTy,
}

/// Where the process starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry2 {
    /// bstd's hosted entry, the free fn exported as `bstd_hosted_start` (a blaze edition).
    Hosted(LibKey),
    /// A free fn the program exports as `main` itself: the C entry the platform calls.
    CMain(LibKey),
    /// A Rust program with std: through the library's `#[lang = "start"]`.
    Rust(RustStart),
    /// The crate root's `fn main`, called directly: a program that links no `#[lang = "start"]`
    /// ([`linked_lang_start`] is `None`).
    Direct(LibKey),
}

impl Entry2 {
    /// The fn the walk starts at: the entry itself, or a Rust program's `main` (`lang_start` is
    /// reached beside it, queued by the caller as one more root of the walk).
    pub fn root(&self) -> LibKey {
        match self {
            Entry2::Hosted(k) | Entry2::CMain(k) | Entry2::Direct(k) => *k,
            Entry2::Rust(s) => s.main,
        }
    }
}

/// The library item the program starts through when it exports no entry of its own: `Some`
/// for [`Entry2::Rust`], `None` for [`Entry2::Direct`].
///
/// Rust's rule: a binary's `main` is run by the `start` lang item of the crates LINKED into it,
/// and a crate links what it names: its extern prelude (its dependencies, and the `core` and
/// `std` the Reference's "Preludes" add, less what `#![no_std]` and `#![no_core]` take away:
/// "If the `no_std` attribute is specified, then the `std` crate is not added to the extern
/// prelude"; blaze-scope graph.rs `externs` is the one reading of it) and each `extern crate`
/// it writes; linking is transitive; a proc-macro unit is never linked. So:
/// - crate 0 of a blaze edition (its `std` is bstd): `None`; its entry is exported (D104);
/// - a `#[lang = "start"]` among the linked crates: `Some` (std's `rt::lang_start`);
/// - none, and Rust's `std` linked: [`Front2Refusal::NoLangStart`] naming the crates looked
///   in, never a direct call that would skip `rt::init` and `rt::cleanup`;
/// - none, and no `std` linked (a `#![no_core]` root, a `#![no_std]` root without `extern
///   crate std`, a program run with no library, as the ladder's shelf-less rungs): `None`;
/// - two different items declared `#[lang = "start"]`: [`Front2Refusal::Start`] naming both.
///
/// `alloc` holds the walk's two lists for the call.
pub fn linked_lang_start(names: &Names<'_>, alloc: Alloc<'_>) -> Result<Option<LibKey>, Front2Refusal> {
    let crates = names.crates();
    let Some(root) = crates.first() else { return Ok(None) };
    if !matches!(root.implicit.std, StdIs::Rust) {
        return Ok(None);
    }
    // The crates crate 0 links: the one reading of the link rule (linked.rs).
    let linked = super::linked::Linked::of(names, &[0], alloc).map_err(|e| Front2Refusal::Front(super::FrontError::Roots(e)))?;
    // The `#[lang = "start"]` items of the linked crates (the names library's `tables.langs`,
    // as [`super::index::Index::new`] gathers them), in crate order.
    let mut found: Option<LibKey> = None;
    for (i, c) in crates.iter().enumerate() {
        if !linked.links(i) {
            continue;
        }
        for l in c.tables.langs.iter() {
            if l.name != LANG_START {
                continue;
            }
            match found {
                Some(first) if first != l.key => {
                    let mut item = shown(names, first);
                    item.push_str("` and `");
                    item.push_str(&shown(names, l.key));
                    return Err(Front2Refusal::Start { item: item.into_boxed_str(), what: "are both declared `#[lang = \"start\"]`" });
                }
                _ => found = Some(l.key),
            }
        }
    }
    let std_linked = names.graph().sysroot().std.is_some_and(|s| linked.links(s.index()));
    match found {
        Some(key) => Ok(Some(key)),
        None if std_linked => Err(Front2Refusal::NoLangStart { shelf: searched(names) }),
        None => Ok(None),
    }
}

/// The Rust start of the root's `main` (`main` is its key, found by the caller; `lang_start`
/// the item [`linked_lang_start`] found): `main`'s return type, read from `main`'s head, made
/// in `store` here. A
/// `main` of a shape the Reference forbids is refused by name: parameters, generic parameters,
/// an ABI other than Rust, `unsafe`. That `T: Termination` holds is the checker's question at
/// `lang_start::<T>`, not answered here.
///
/// The signature takes `store` and `keys` beside LADDER.md L1a's `(names, main)`: a `PTy` is an
/// index into a type table, and only the program's store holds the one the back half reads.
pub fn rust_start<'n>(names: &'n Names<'n>, lang_start: LibKey, main: LibKey, store: &mut Store<'n>, keys: &mut Keys) -> Result<RustStart, Front2Refusal> {
    store.ensure(names, keys, Want::Fn(main)).map_err(|error| Front2Refusal::Lower(BodyError::Head { item: main, error }))?;
    let Some(head) = store.fns.get(&checker_of(main)) else {
        return Err(Front2Refusal::NoHead { function: shown(names, main).into_boxed_str() });
    };
    let refuse = |what: &'static str| Front2Refusal::Start { item: shown(names, main).into_boxed_str(), what };
    if !head.inputs.is_empty() {
        return Err(refuse("takes parameters; the Reference's `main` takes none (`std::env::args` reads them)"));
    }
    if head.own != 0 || head.parent != 0 {
        return Err(refuse("has generic parameters; the Reference's `main` declares none"));
    }
    if !matches!(head.header.abi, tc::Abi::Rust) {
        return Err(refuse("is not of the Rust ABI; a C entry is exported under `#![no_main]`"));
    }
    if !matches!(head.header.safety, tc::Safety::Safe) {
        return Err(refuse("is `unsafe`; `lang_start` takes a safe `fn() -> T`"));
    }
    Ok(RustStart { lang_start, main, returns: head.output })
}

/// `lib` as `module::name` for a message (as program.rs `item_named`), or its key.
fn shown(names: &Names<'_>, lib: LibKey) -> String {
    let mut out = String::new();
    let name = names.item_text(lib).and_then(|t| t.name);
    let module = names.crate_of(lib).and_then(|k| names.module_of(lib).and_then(|m| names.krate(k).path_of(m)));
    match (module, name) {
        (Some(m), Some(n)) => {
            out.push_str(m);
            out.push_str("::");
            out.push_str(n);
        }
        (None, Some(n)) => out.push_str(n),
        _ => {
            use core::fmt::Write;
            write!(out, "{lib:x}").expect("writing into a `String` cannot fail");
        }
    }
    out
}

/// The crates looked in for `#[lang = "start"]`: each one's name and root file, in the names
/// library's order (the program's crate first).
fn searched(names: &Names<'_>) -> Box<str> {
    let mut out = String::new();
    for (i, c) in names.crates().iter().enumerate() {
        if c.implicit.proc_macro {
            continue;
        }
        if i != 0 {
            out.push_str(", ");
        }
        out.push_str(c.name);
        out.push_str(" (");
        out.push_str(c.files.first().map_or("no file", |f| f.path));
        out.push(')');
    }
    out.into_boxed_str()
}
