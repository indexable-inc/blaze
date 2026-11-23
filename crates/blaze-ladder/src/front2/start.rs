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
use blaze_typeck as tc;
use blaze_typeck::PTy;
use blaze_unit_graph::StdIs;

use super::body::BodyError;
use super::heads::{Store, Want};
use super::keys::{Keys, checker_of};
use super::program::Front2Refusal;
use super::source::NamesSource;

/// The `#[lang = ".."]` name of the library's Rust start (std rt.rs `lang_start`).
pub const LANG_START: &str = "start";

/// Whether a program crate whose `Implicit::std` is `std` can link a `#[lang = "start"]` at all:
/// only under Rust's std ([`linked_lang_start`]'s first arm; a blaze edition's `std` is bstd
/// and its entry is exported, D104). A fact of the package's edition, known before any name
/// is resolved: `blaze run` reads it to decide, with no names solved, that a blaze-edition
/// program never needs a process of its own (run.rs `entry_start`).
pub fn may_link_lang_start(std: StdIs) -> bool {
    matches!(std, StdIs::Rust)
}

/// A proc-macro crate's `_DECLS` static as a compile-time program names it (D175,
/// comptime-native.md "The proc_macro route"): blaze-scope task/lower.rs makes it, in the root
/// fragment of every crate whose `Implicit::proc_macro` is set, as the expansion of the crate's
/// harness call; its value is `&'static [proc_macro::bridge::client::Client]`, one `Client` a
/// macro in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcMacroDecls {
    /// Its library key ([`blaze_scope::task::proc_macro_decls_key`]): a function of the crate's
    /// key alone, so asked before any name is solved.
    pub item: LibKey,
    /// Its symbol (symbols.rs `static_symbol` of its checker key): the address the entry's
    /// first argument points at is this static's.
    pub symbol: blaze_code::record::Symbol,
}

/// The `_DECLS` static of proc-macro crate `krate` ([`ProcMacroDecls`]). What `run` is handed
/// for one expansion: the entry `comptime:<symbol of the shim's `blaze_proc_macro_expand`>`
/// (an item of the shim crate, blaze-proc-macro guest/shim.rs, not made here), and as its
/// arguments this static's address (as `*const &'static [Client]`), the macro's index in the
/// list, the input buffer and its length.
pub fn proc_macro_decls(krate: blaze_id::CrateItemKey) -> ProcMacroDecls {
    let item = blaze_scope::task::proc_macro_decls_key(krate);
    ProcMacroDecls { item, symbol: crate::symbols::static_symbol(checker_of(item)) }
}

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
///
/// Generic over the names source (D124): `blaze run` asks it over the names records with
/// nothing solved. It reads crate 0's `Implicit::std`, the crates crate 0 links, each linked
/// crate's lang rows ([`NamesSource::each_lang`]) and which crate is Rust's `std`; no head, no
/// body. A fact the source does not answer is [`Front2Refusal::StartFact`].
///
/// OWED by front2 source.rs (lane M), two methods this is written against:
/// - `NamesSource::linked<'a>(self, roots: &[usize], alloc: Alloc<'a>) ->
///   Option<Result<super::linked::Linked<'a>, super::linked::RootsError>>`: the crates the
///   programs of `roots` link (linked.rs, the one reading of the link rule). Over the solved
///   names `Some(Linked::of(self, roots, alloc))`; over records from each crate's externs as
///   its names record holds them. `None`: not answered (filed in the log).
/// - `NamesSource::sysroot_std(self) -> Option<Option<usize>>`: the index of Rust's `std` among
///   the crates, if the program has one. Over the solved names
///   `Some(self.graph().sysroot().std.map(|s| s.index()))`.
pub fn linked_lang_start<'n, S: NamesSource<'n>>(names: S, alloc: Alloc<'_>) -> Result<Option<LibKey>, Front2Refusal> {
    let count = names.crate_count();
    if count == 0 {
        return Ok(None);
    }
    let std = names.std_is(0).ok_or(Front2Refusal::StartFact { krate: 0, what: "its `Implicit::std`" })?;
    if !may_link_lang_start(std) {
        return Ok(None);
    }
    // The crates crate 0 links: the one reading of the link rule (linked.rs).
    // OWED: `NamesSource::linked` (above).
    let linked = names
        .linked(&[0], alloc)
        .ok_or(Front2Refusal::StartFact { krate: 0, what: "the crates it links" })?
        .map_err(|e| Front2Refusal::Front(super::FrontError::Roots(e)))?;
    // The `#[lang = "start"]` items of the linked crates (each crate's lang rows, as
    // [`super::index::Index::new`] gathers them), in crate order.
    let mut found: Option<LibKey> = None;
    let mut two: Option<(LibKey, LibKey)> = None;
    for i in 0..count {
        if !linked.links(i) {
            continue;
        }
        names.each_lang(i, &mut |name, key| {
            if name != LANG_START || two.is_some() {
                return;
            }
            match found {
                Some(first) if first != key => two = Some((first, key)),
                _ => found = Some(key),
            }
        });
        if let Some((first, second)) = two {
            let mut item = shown(names, first);
            item.push_str("` and `");
            item.push_str(&shown(names, second));
            return Err(Front2Refusal::Start { item: item.into_boxed_str(), what: "are both declared `#[lang = \"start\"]`" });
        }
    }
    // OWED: `NamesSource::sysroot_std` (above).
    let std_index = names.sysroot_std().ok_or(Front2Refusal::StartFact { krate: 0, what: "which crate is Rust's `std`" })?;
    let std_linked = std_index.is_some_and(|s| linked.links(s));
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
///
/// Generic over the names source: it reads `main`'s head and nothing else (run/special.rs
/// `roots` asks it over records).
pub fn rust_start<'n, S: NamesSource<'n>>(names: S, lang_start: LibKey, main: LibKey, store: &mut Store<'n>, keys: &mut Keys) -> Result<RustStart, Front2Refusal> {
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
    let returns = store.fn_output(checker_of(main)).unwrap_or(head.output);
    Ok(RustStart { lang_start, main, returns })
}

/// `lib` for a message: front2 item_shown.rs `ItemShown`, the one printer of an item in a
/// refusal (its whole path and its key).
fn shown<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> String {
    super::item_shown::ItemShown::of_lib(names, lib).to_string()
}

/// The crates looked in for `#[lang = "start"]`: each one's path (its root module's, the
/// crate's name) in the source's order (the program's crate first); a proc-macro unit is
/// never linked and is left out. A crate whose path the source does not answer is shown by
/// its index.
fn searched<'n, S: NamesSource<'n>>(names: S) -> Box<str> {
    let mut out = String::new();
    let mut first = true;
    for i in 0..names.crate_count() {
        if names.proc_macro(i) == Some(true) {
            continue;
        }
        if !first {
            out.push_str(", ");
        }
        first = false;
        match names.root_module(i).and_then(|m| names.module_path(m)) {
            Some(path) => out.push_str(path),
            None => out.push_str(&std::format!("crate {i}")),
        }
    }
    out.into_boxed_str()
}
