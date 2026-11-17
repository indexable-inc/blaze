//! The library's std-internal symbols on the run path: a foreign fn declared in an
//! `extern "Rust"` block with `#[rustc_std_internal_symbol]` is a Rust-to-Rust call of the one
//! definition of that name carrying the same attribute in the program's graph, as
//! [`super::alloc_shim`] binds alloc's allocator declarations to std's defaults. Nothing asks
//! the process for such a symbol.
//!
//! The pinned std (library/std/src/panicking.rs, nightly-2026-10-27, lines 47-64):
//! ```text
//! // Binary interface to the panic runtime that the standard library depends on.
//! //
//! // The standard library is tagged with `#![needs_panic_runtime]` (introduced in
//! // RFC 1513) to indicate that it requires some other crate tagged with
//! // `#![panic_runtime]` to exist somewhere. Each panic runtime is intended to
//! // implement these symbols (with the same signatures) so we can get matched up
//! // to them.
//! ..
//! unsafe extern "Rust" {
//!     #[rustc_std_internal_symbol]
//!     fn __rust_panic_cleanup(payload: *mut u8) -> Box<dyn Any + Send + 'static>;
//!
//!     /// `PanicPayload` lazily performs allocation only when needed (this avoids
//!     /// allocations when using the "abort" panic runtime).
//!     #[rustc_std_internal_symbol]
//!     safe fn __rust_start_panic(payload: &mut dyn PanicPayload) -> u32;
//! }
//! ```
//! and (lines 66-80) std DEFINES the two the runtime calls back:
//! ```text
//! /// This function is called by the panic runtime if FFI code catches a Rust
//! /// panic but doesn't rethrow it. ..
//! #[cfg(not(test))]
//! #[rustc_std_internal_symbol]
//! fn __rust_drop_panic() -> ! { rtabort!("Rust panics must be rethrown"); }
//!
//! /// This function is called by the panic runtime if it catches an exception
//! /// object which does not correspond to a Rust panic.
//! #[cfg(not(test))]
//! #[rustc_std_internal_symbol]
//! fn __rust_foreign_exception() -> ! { rtabort!("Rust cannot catch foreign exceptions"); }
//! ```
//! The pinned panic_unwind (library/panic_unwind/src/lib.rs): line 23 `#![panic_runtime]`, and
//! lines 70-92:
//! ```text
//! unsafe extern "Rust" {
//!     /// Handler in std called when a panic object is dropped outside of
//!     /// `catch_unwind`.
//!     #[rustc_std_internal_symbol]
//!     safe fn __rust_drop_panic() -> !;
//!
//!     /// Handler in std called when a foreign exception is caught.
//!     #[rustc_std_internal_symbol]
//!     safe fn __rust_foreign_exception() -> !;
//! }
//!
//! #[rustc_std_internal_symbol]
//! pub unsafe fn __rust_panic_cleanup(payload: *mut u8) -> Box<dyn Any + Send + 'static> {
//!     unsafe { imp::cleanup(payload) }
//! }
//!
//! // Entry point for raising an exception, just delegates to the platform-specific
//! // implementation.
//! #[rustc_std_internal_symbol]
//! pub fn __rust_start_panic(payload: &mut dyn PanicPayload) -> u32 {
//!     imp::panic(payload)
//! }
//! ```
//! The pinned panic_abort (library/panic_abort/src/lib.rs): line 9 `#![panic_runtime]`, and it
//! defines the same two names (`__rust_panic_cleanup` is `unreachable!()`, `__rust_start_panic`
//! aborts). The pinned std (library/std/src/lib.rs, line 239): `#![needs_panic_runtime]`
//! ("Tell the compiler to link to either panic_abort or panic_unwind").
//!
//! The Rust Reference, [panic.strategy.intro]: "When generating a binary, dylib, cdylib, or
//! staticlib and linking with std, the `-C panic` CLI flag also influences which panic handler
//! is used."
//!
//! So:
//! - the two names a panic runtime defines ([`START_PANIC`], [`PANIC_CLEANUP`]) bind to the
//!   definition in the crate whose root says `#![panic_runtime]` AND whose name, as the library
//!   ships it, is `panic_unwind` for [`Panic::Unwind`] and `panic_abort` for [`Panic::Abort`]
//!   (rustc picks the runtime by the same two facts). The strategy is the program's (one value
//!   for every crate; a key input already) and comes in as a parameter. A graph without the
//!   chosen runtime is [`InternalRefused::NoPanicRuntime`]: never a silent switch to the other
//!   (a std whose `panic-unwind` feature is off in the shelf's lock has no `panic_unwind`, and
//!   a program built `panic = "unwind"` over it is refused, not run as abort);
//! - every other such declaration (`__rust_drop_panic`, `__rust_foreign_exception`, ..) binds to
//!   the one fn with a body of that name carrying the attribute, in any crate of the graph;
//! - alloc's allocator declarations are [`super::alloc_shim`]'s: [`declares`] answers `None`
//!   for every name `Shim::ALL` holds, and each call seam asks alloc_shim first.
//!
//! The three seams are alloc_shim's: the table (program/tabled.rs `row`, by [`declares`]) files
//! the declaration as a row with no body and NO symbol; instance resolution (program.rs
//! `resolve`, by [`bound`]) answers the definition, so the builder makes an ordinary direct
//! call of a known instance with the definition's ABI; the walk (reach.rs, by [`reached`])
//! queues the definition as the declaration's callee.
//!
//! Read by name, as alloc_shim does: an attribute is read from the item's lowered signature
//! (`cfg_attr` evaluated under the crate's cfg); a crate root's `#![panic_runtime]` is read
//! from its root file's own inner attributes as written.
//!
//! Not here (each a gap by name, not a guess): a `#![cfg_attr(.., panic_runtime)]` root (the
//! library writes it plainly) is not read as a panic runtime; a declaration with a
//! `#[link_name]` naming another symbol is not bound (as alloc_shim: it stays the foreign fn it
//! is, and the process lookup refuses it by name).

use blaze_build::Panic;
use blaze_lower::head;
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_typeck::ItemKey;
use std::boxed::Box;
use std::format;

use super::alloc_shim::Shim;
use super::check::Records;
use super::heads::{CrateCfg, Store};
use super::index::Index;
use super::keys::checker_of;
use super::reach::Reached;
use blaze_id::ItemKey as LibKey;

/// The attribute this file binds by.
pub const ATTR: &str = "rustc_std_internal_symbol";
/// The crate-root attribute a panic runtime carries.
pub const PANIC_RUNTIME: &str = "panic_runtime";
/// The panic runtime's entry that raises a panic.
pub const START_PANIC: &str = "__rust_start_panic";
/// The panic runtime's entry that takes a caught panic's payload.
pub const PANIC_CLEANUP: &str = "__rust_panic_cleanup";
/// The library's unwinding panic runtime, by its crate name.
pub const PANIC_UNWIND_CRATE: &str = "panic_unwind";
/// The library's aborting panic runtime, by its crate name.
pub const PANIC_ABORT_CRATE: &str = "panic_abort";

/// Whether `name` is one of the two a panic runtime defines.
pub fn is_runtime_name(name: &str) -> bool {
    name == START_PANIC || name == PANIC_CLEANUP
}

/// The panic runtime's crate name for `panic`.
pub fn runtime_crate(panic: Panic) -> &'static str {
    match panic {
        Panic::Unwind => PANIC_UNWIND_CRATE,
        Panic::Abort => PANIC_ABORT_CRATE,
    }
}

/// A declaration this file binds: the fn `lib`, declared `extern "Rust"` with [`ATTR`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Declared {
    pub lib: LibKey,
}

/// Why a std-internal declaration is not bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InternalRefused {
    /// No fn with a body in the graph is named `name` and carries the attribute.
    NoDefinition { name: Box<str> },
    /// Two fns with bodies that count are named `name` and carry the attribute: `first` and
    /// `second`, the two smallest by key (the graph's order is no order).
    DefinedTwice { name: Box<str>, first: Box<str>, second: Box<str> },
    /// The program's strategy names a panic runtime crate the graph does not hold.
    NoPanicRuntime { strategy: Panic },
    /// The attribute on a declaration outside an `extern "Rust"` block.
    NotInRustBlock { name: Box<str> },
    /// The declaration's or a candidate definition's attributes did not read.
    Attrs { name: Box<str> },
    /// The definition's key is another item's in the key map.
    KeyClash { name: Box<str> },
    /// The declaration's place (and so its block's ABI) is not in the names library
    /// (program.rs `placed`'s `Unplaced`, D106 piece Z).
    Unplaced { name: Box<str> },
}

impl InternalRefused {
    /// The refusal's class: the rule that refused, for a survey's grouping.
    pub fn class(&self) -> &'static str {
        match self {
            InternalRefused::NoDefinition { .. } => {
                "a call of an `extern \"Rust\"` `#[rustc_std_internal_symbol]` declaration with no fn of that name carrying the attribute in the graph"
            }
            InternalRefused::DefinedTwice { .. } => "a `#[rustc_std_internal_symbol]` fn defined twice in the graph",
            InternalRefused::NoPanicRuntime { strategy: Panic::Unwind } => {
                "the program is built `panic = \"unwind\"` and no `#![panic_runtime]` crate `panic_unwind` is in its graph"
            }
            InternalRefused::NoPanicRuntime { strategy: Panic::Abort } => {
                "the program is built `panic = \"abort\"` and no `#![panic_runtime]` crate `panic_abort` is in its graph"
            }
            InternalRefused::NotInRustBlock { .. } => "`#[rustc_std_internal_symbol]` on a declaration outside an `extern \"Rust\"` block",
            InternalRefused::Attrs { .. } => "a `#[rustc_std_internal_symbol]` fn whose attributes do not read",
            InternalRefused::KeyClash { .. } => "the `#[rustc_std_internal_symbol]` definition's key is another item's in the key map",
            InternalRefused::Unplaced { .. } => "a declaration the names library does not place (its block's ABI is unknown)",
        }
    }

    /// The whole refusal: the class and what it names.
    pub fn detail(&self) -> Box<str> {
        match self {
            InternalRefused::NoDefinition { name }
            | InternalRefused::NotInRustBlock { name }
            | InternalRefused::Attrs { name }
            | InternalRefused::KeyClash { name }
            | InternalRefused::Unplaced { name } => format!("`{name}`: {}", self.class()).into_boxed_str(),
            InternalRefused::DefinedTwice { name, first, second } => format!("`{name}`: {} (`{first}` and `{second}`)", self.class()).into_boxed_str(),
            InternalRefused::NoPanicRuntime { .. } => Box::from(self.class()),
        }
    }
}

impl core::fmt::Display for InternalRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail())
    }
}

/// Whether the item whose text (from its first attribute) is `text`, of crate `krate`, carries
/// [`ATTR`]: its lowered signature's attributes, `cfg_attr` evaluated under the crate's cfg.
/// `Err(())` when the text does not lower. `alloc` holds the lowering, dropped on return.
fn carries(names: &Names<'_>, krate: usize, text: &str, alloc: Alloc<'_>) -> Result<bool, ()> {
    // The attribute is a word of the item's own text wherever it is written (an expansion's
    // text included): a text without it carries none, and nothing is lowered for it.
    if !text.contains(ATTR) {
        return Ok(false);
    }
    let atoms = names.krate(krate).cfg.atoms();
    let sig = blaze_lower::lower_signature(text, names.edition(krate), &mut CrateCfg(atoms), alloc).map_err(|_| ())?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| ())? {
        let a = v.attr(n).map_err(|_| ())?;
        if v.text_at(a.path).map_err(|_| ())?.trim() == ATTR {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The `#[link_name]` written on the lowered item `text`, when it names a symbol other than
/// `name`: such a declaration is not bound by name here.
fn renamed(names: &Names<'_>, krate: usize, text: &str, name: &str, alloc: Alloc<'_>) -> Result<bool, ()> {
    if !text.contains("link_name") {
        return Ok(false);
    }
    let atoms = names.krate(krate).cfg.atoms();
    let sig = blaze_lower::lower_signature(text, names.edition(krate), &mut CrateCfg(atoms), alloc).map_err(|_| ())?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| ())? {
        let a = v.attr(n).map_err(|_| ())?;
        if v.text_at(a.path).map_err(|_| ())?.trim() == "link_name" && crate::front::unquote(v.text_at(a.args).map_err(|_| ())?) != name {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether crate `krate`'s root file says `#![panic_runtime]` among its own inner attributes,
/// as written (`#![panic_runtime]`, spaces allowed inside the brackets).
pub fn is_panic_runtime(names: &Names<'_>, krate: usize) -> bool {
    // The root file is the first a crate's resolution reads (blaze-scope resolve.rs: the
    // root's file is added to the empty set first, index 0).
    let Some(root) = names.krate(krate).files.first() else { return false };
    root.outline.file_attrs().iter().any(|a| {
        if !a.inner {
            return false;
        }
        let Some(written) = root.text.get(a.range.start as usize..a.range.end as usize) else { return false };
        let inside = written.trim().strip_prefix("#!").map(str::trim_start).and_then(|s| s.strip_prefix('[')).and_then(|s| s.strip_suffix(']'));
        inside.is_some_and(|s| s.trim() == PANIC_RUNTIME)
    })
}

/// Whether the outline entry at `lib` is a fn with a body (not a declaration in an `extern`
/// block, which has none).
fn is_defined_fn(names: &Names<'_>, lib: LibKey) -> bool {
    use blaze_surface::ItemDetail;
    let Some(p) = names.place(lib) else { return false };
    let Some((_, outline)) = names.outline_at(p.krate, p.at) else { return false };
    let item = outline.item(p.item);
    matches!(item.detail, ItemDetail::Fn { .. }) && item.body.is_some()
}

/// The item for a refusal: front2 item_shown.rs `ItemShown` (its whole path and its key; never
/// a `_` in place of a name).
fn path_of(names: &Names<'_>, lib: LibKey) -> Box<str> {
    super::item_shown::ItemShown::of_lib(names, lib).boxed()
}

/// The definition a call of the std-internal declaration named `name` runs, under the
/// program's strategy `panic`: for [`START_PANIC`] and [`PANIC_CLEANUP`], the one fn of that
/// name carrying [`ATTR`] in a `#![panic_runtime]` crate named [`runtime_crate`]`(panic)`;
/// for every other name, the one such fn in any crate. A proc-macro unit's items are never
/// linked into the program and are skipped, as the index skips them. Two candidates are
/// [`InternalRefused::DefinedTwice`] naming the two smallest keys, so the answer is the same
/// in any order the graph is walked.
pub fn definition(names: &Names<'_>, name: &str, panic: Panic, alloc: Alloc<'_>) -> Result<LibKey, InternalRefused> {
    let runtime = is_runtime_name(name);
    let wanted = runtime_crate(panic);
    let mut found: Option<LibKey> = None;
    let mut second: Option<LibKey> = None;
    let mut runtime_seen = false;
    for (krate, c) in names.crates().iter().enumerate() {
        if c.implicit.proc_macro {
            continue;
        }
        if runtime {
            if c.name != wanted || !is_panic_runtime(names, krate) {
                continue;
            }
            runtime_seen = true;
        }
        for (key, _, _) in names.items(krate) {
            let Some(t) = names.item_text(key) else { continue };
            if t.name != Some(name) || !is_defined_fn(names, key) {
                continue;
            }
            if !carries(names, krate, t.text, alloc).map_err(|()| InternalRefused::Attrs { name: Box::from(name) })? {
                continue;
            }
            // Keep the two smallest keys: the refusal names the same pair in any walk order.
            match found {
                None => found = Some(key),
                Some(f) if key == f => {}
                Some(f) if key < f => {
                    second = Some(f);
                    found = Some(key);
                }
                Some(_) => {
                    if second.is_none_or(|s| key < s) {
                        second = Some(key);
                    }
                }
            }
        }
    }
    if let (Some(first), Some(second)) = (found, second) {
        return Err(InternalRefused::DefinedTwice { name: Box::from(name), first: path_of(names, first), second: path_of(names, second) });
    }
    match found {
        Some(key) => Ok(key),
        None if runtime && !runtime_seen => Err(InternalRefused::NoPanicRuntime { strategy: panic }),
        None => Err(InternalRefused::NoDefinition { name: Box::from(name) }),
    }
}

/// Which std-internal declaration the fn `key` is: a fn with no body, declared in an `extern`
/// block, whose attributes carry [`ATTR`], named none of alloc_shim's names, with no
/// `#[link_name]` naming another symbol. `Ok(None)` for every other item;
/// [`InternalRefused::NotInRustBlock`] when the block's ABI is not `"Rust"`. `alloc` holds the
/// signature's lowering, dropped on return.
pub fn declares(names: &Names<'_>, store: &Store<'_>, key: ItemKey, alloc: Alloc<'_>) -> Result<Option<Declared>, InternalRefused> {
    let Some(h) = store.fns.get(&key) else { return Ok(None) };
    if h.has_body || h.container.is_some() || !h.text.contains(ATTR) {
        return Ok(None);
    }
    // A module-level declaration is named and placed by the names library; one an `extern`
    // block in a body declares, by the store (panic_entry's `local_name`, `local_abi`).
    let local_name = super::panic_entry::local_name(store, key).ok().flatten();
    let Some(name) = names.item_text(h.lib).and_then(|t| t.name).or(local_name) else { return Ok(None) };
    if Shim::ALL.iter().any(|s| s.declared() == name) {
        return Ok(None);
    }
    let (_, placed_abi) = super::program::placed(names, h.lib).map_err(|_| InternalRefused::Unplaced { name: Box::from(name) })?;
    let Some(abi) = placed_abi.or_else(|| super::panic_entry::local_abi(store, key)) else { return Ok(None) };
    let attrs = || InternalRefused::Attrs { name: Box::from(name) };
    let krate = names.crate_of(h.lib).ok_or_else(attrs)?;
    if !carries(names, krate, h.text, alloc).map_err(|()| attrs())? {
        return Ok(None);
    }
    if abi != "Rust" {
        return Err(InternalRefused::NotInRustBlock { name: Box::from(name) });
    }
    if renamed(names, krate, h.text, name, alloc).map_err(|()| attrs())? {
        return Ok(None);
    }
    Ok(Some(Declared { lib: h.lib }))
}

/// The declared name of `d`, as [`declares`] read it.
fn declared_name<'s>(names: &'s Names<'_>, store: &'s Store<'_>, key: ItemKey, d: Declared) -> Option<&'s str> {
    match names.item_text(d.lib).and_then(|t| t.name) {
        Some(name) => Some(name),
        None => super::panic_entry::local_name(store, key).ok().flatten(),
    }
}

/// The definition a call of `item` runs, when `item` is a std-internal declaration (not
/// generic: no arguments), under the program's strategy `panic`. `None` for every other item
/// and a declaration [`reached`] refused (it is then in no table, and its caller is refused
/// with that cause). `index` is the seam's, as alloc_shim's: nothing here asks it.
pub fn bound(names: &Names<'_>, index: &Index<'_>, store: &Store<'_>, item: ItemKey, panic: Panic, alloc: Alloc<'_>) -> Option<ItemKey> {
    let _ = index;
    let d = declares(names, store, item, alloc).ok().flatten()?;
    let name = declared_name(names, store, item, d)?;
    definition(names, name, panic, alloc).ok().map(checker_of)
}

fn failed(why: &InternalRefused) -> Reached {
    Reached::Failed { class: why.class().into(), detail: why.detail(), refusal: None }
}

/// The walk's outcome for `k`, a fn whose head says it has no body, when it is a std-internal
/// declaration: [`Reached::NoBody`] and its definition to queue (its key filed), or the
/// refusal naming why there is none. `None` for every other fn: the next binding's
/// ([`super::panic_entry::reached`]) to say.
pub fn reached(r: &mut Records<'_, '_>, k: ItemKey, panic: Panic, alloc: Alloc<'_>) -> Option<(Reached, Option<ItemKey>)> {
    let d = match declares(r.names, r.store, k, alloc) {
        Ok(None) => return None,
        Err(why) => return Some((failed(&why), None)),
        Ok(Some(d)) => d,
    };
    let name = declared_name(r.names, r.store, k, d)?;
    Some(match definition(r.names, name, panic, alloc) {
        Err(why) => (failed(&why), None),
        Ok(lib) => match r.keys.item(lib) {
            Ok(def) => (Reached::NoBody, Some(def)),
            Err(_) => (failed(&InternalRefused::KeyClash { name: Box::from(name) }), None),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two runtime names, and only they, pick a runtime crate; each strategy names its
    /// library crate; alloc's names are alloc_shim's, never this file's.
    ///
    /// Prints: `test front2::internal_symbol::tests::runtime_names_and_crates ... ok`
    #[test]
    fn runtime_names_and_crates() {
        assert!(is_runtime_name("__rust_start_panic"));
        assert!(is_runtime_name("__rust_panic_cleanup"));
        assert!(!is_runtime_name("__rust_drop_panic"));
        assert!(!is_runtime_name("__rust_foreign_exception"));
        assert_eq!(runtime_crate(Panic::Unwind), "panic_unwind");
        assert_eq!(runtime_crate(Panic::Abort), "panic_abort");
        assert!(Shim::ALL.iter().any(|s| s.declared() == "__rust_alloc"));
        assert!(!Shim::ALL.iter().any(|s| s.declared() == START_PANIC));
        let refused = InternalRefused::NoPanicRuntime { strategy: Panic::Unwind };
        assert_eq!(
            &*refused.detail(),
            "the program is built `panic = \"unwind\"` and no `#![panic_runtime]` crate `panic_unwind` is in its graph"
        );
    }
}
