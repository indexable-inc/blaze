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
//! The four starts ([`Entry2`]): bstd's hosted entry (a blaze edition, the appended
//! `__blaze_entry`), a C `main` the program exports itself (`#![no_main]`), the Rust start, and
//! the crate root's `fn main` called directly: a `#![no_core]` or `#![no_std]` root (no std, so
//! no `lang_start` of the library's; such a root names its own start) or a blaze-edition root
//! without the appended entry. The last is today's form, kept for those roots only; a Rust
//! program with std never takes it.

use std::{boxed::Box, string::String};

use blaze_id::ItemKey as LibKey;
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
    /// The crate root's `fn main`, called directly: a `#![no_core]` or `#![no_std]` root, or a
    /// blaze-edition root with no appended entry ([`through_lang_start`] is false for both).
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

/// Whether the program's crate (index 0) starts through `#[lang = "start"]` when it exports no
/// entry of its own: a Rust edition (its `std` is Rust's, not bstd) with std linked (neither
/// `#![no_core]` nor `#![no_std]`).
pub fn through_lang_start(names: &Names<'_>) -> bool {
    let Some(root) = names.crates().first() else { return false };
    matches!(root.implicit.std, StdIs::Rust) && !root.root_attrs.no_core && !root.root_attrs.no_std
}

/// The item `#[lang = "start"]` declares, read from every crate's lang table (the names
/// library's `tables.langs`, as [`super::index::Index::new`] gathers them; a proc-macro unit is
/// never linked, so skipped). None is [`Front2Refusal::NoLangStart`] naming the crates looked
/// in; two different items is [`Front2Refusal::Start`] naming both.
pub fn lang_start(names: &Names<'_>) -> Result<LibKey, Front2Refusal> {
    let mut found: Option<LibKey> = None;
    for c in names.crates().iter() {
        if c.implicit.proc_macro {
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
    found.ok_or_else(|| Front2Refusal::NoLangStart { shelf: searched(names) })
}

/// The Rust start of the root's `main` (`main` is its key, found by the caller): the library's
/// `lang_start` and `main`'s return type, read from `main`'s head, made in `store` here. A
/// `main` of a shape the Reference forbids is refused by name: parameters, generic parameters,
/// an ABI other than Rust, `unsafe`. That `T: Termination` holds is the checker's question at
/// `lang_start::<T>`, not answered here.
///
/// The signature takes `store` and `keys` beside LADDER.md L1a's `(names, main)`: a `PTy` is an
/// index into a type table, and only the program's store holds the one the back half reads.
pub fn rust_start<'n>(names: &'n Names<'n>, main: LibKey, store: &mut Store<'n>, keys: &mut Keys) -> Result<RustStart, Front2Refusal> {
    let lang_start = lang_start(names)?;
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
