//! The allocator's entry points on the run path: the `extern "Rust"` declarations the pinned
//! alloc calls to allocate, whose bodies no library holds, bound to the fns that are their
//! bodies, as [`super::panic_entry`] binds core's `panic_impl` to the `#[panic_handler]` fn.
//!
//! The pinned alloc (library/alloc/src/alloc.rs, nightly-2026-10-27, lines 12-44):
//! ```text
//! unsafe extern "Rust" {
//!     // These are the magic symbols to call the global allocator. rustc generates
//!     // them to call the global allocator if there is a `#[global_allocator]` attribute
//!     // (the code expanding that attribute macro generates those functions), or to call
//!     // the default implementations in std (`__rdl_alloc` etc. in `library/std/src/alloc.rs`)
//!     // otherwise.
//!     #[rustc_allocator]
//!     #[rustc_nounwind]
//!     #[rustc_std_internal_symbol]
//!     #[rustc_allocator_zeroed_variant = "__rust_alloc_zeroed"]
//!     fn __rust_alloc(size: usize, align: Alignment) -> *mut u8;
//!     ..
//!     fn __rust_dealloc(ptr: NonNull<u8>, size: usize, align: Alignment);
//!     ..
//!     fn __rust_realloc(ptr: NonNull<u8>, old_size: usize, align: Alignment, new_size: usize) -> *mut u8;
//!     ..
//!     fn __rust_alloc_zeroed(size: usize, align: Alignment) -> *mut u8;
//!
//!     #[rustc_nounwind]
//!     #[rustc_std_internal_symbol]
//!     fn __rust_no_alloc_shim_is_unstable_v2();
//! }
//! ```
//! and (lines 630-637):
//! ```text
//! unsafe extern "Rust" {
//!     // This is the magic symbol to call the global alloc error handler. rustc generates
//!     // it to call `__rg_oom` if there is a `#[alloc_error_handler]`, or to call the
//!     // default implementations below (`__rdl_alloc_error_handler`) otherwise.
//!     #[rustc_std_internal_symbol]
//!     fn __rust_alloc_error_handler(size: usize, align: usize) -> !;
//! }
//! ```
//! The defaults: the pinned std (library/std/src/alloc.rs, `pub mod __default_lib_allocator`,
//! lines 456-510): "These magic symbol names are used as a fallback for implementing the
//! `__rust_alloc` etc symbols (see `src/liballoc/alloc.rs`) when there is no
//! `#[global_allocator]` attribute.", each `#[rustc_std_internal_symbol] pub unsafe extern "C"
//! fn __rdl_alloc(size: usize, align: usize) -> *mut u8` (and `__rdl_dealloc(ptr: *mut u8,
//! size: usize, align: usize)`, `__rdl_realloc(ptr: *mut u8, old_size: usize, align: usize,
//! new_size: usize) -> *mut u8`, `__rdl_alloc_zeroed(size: usize, align: usize) -> *mut u8`);
//! the pinned alloc (alloc.rs, `pub mod __alloc_error_handler`, line 698): "called via
//! generated `__rust_alloc_error_handler` if there is no `#[alloc_error_handler]`":
//! `#[rustc_std_internal_symbol] pub unsafe fn __rdl_alloc_error_handler(size: usize, _align:
//! usize) -> !`.
//!
//! So, with no `#[global_allocator]` in the graph, a call of `__rust_alloc` IS a call of
//! `__rdl_alloc` with the same arguments (`Alignment` is `#[repr(transparent)]` over a
//! `#[repr(usize)]` enum, `NonNull<u8>` over `*const u8`: the same scalars the default takes):
//!
//! - the walk ([`reached`], asked by reach.rs where it meets a fn with no body) queues the
//!   default as a callee, like any Rust fn a body calls;
//! - the table (program/tabled.rs `row`, by [`declares`]) files the declaration as a row with
//!   no body and NO symbol, so nothing asks the process for `__rust_alloc`;
//! - instance resolution (program.rs `resolve`, by [`bound`]) answers the default for the
//!   declaration, so the builder makes an ordinary direct call of a known instance, with the
//!   default's ABI (the builder reads it from the instance the call resolves to).
//!
//! `__rust_no_alloc_shim_is_unstable_v2` is a fn rustc generates with an empty body (a
//! marker that a program links the shim): it has no default to bind to. The table files it as
//! a call the build makes in place from blaze-build's intrinsics table, under its own name,
//! built as nothing ([`Shim::Marker`]); the walk files it `NoBody` and queues nothing.
//!
//! The defaults are found by NAME across the graph, as rustc's generated shim calls them by
//! symbol, whichever crate defines them: the one fn with a body named `__rdl_alloc` (two are
//! [`Refused::DefaultTwice`], none [`Refused::NoDefault`]: a `#![no_std]` program with no
//! allocator, which the language refuses too).
//!
//! A program with its own `#[global_allocator]` static (the names library files it in the lang
//! rows under [`OWN_ALLOCATOR_ROW`], as it files `#[panic_handler]`) is refused by name
//! ([`Refused::OwnAllocator`]): the binding is then to that static's `GlobalAlloc` methods,
//! which the built-in attribute's expansion makes (rustc's `__rg_alloc` etc.), and blaze has
//! no such expansion yet. Never bound to std's defaults then: that would quietly ignore the
//! program's allocator.
//!
//! Not here: a `#[alloc_error_handler]` fn (unstable `alloc_error_handler`) is not looked
//! for; a declaration that reaches one of these symbols only by `#[link_name]` under another
//! name stays the foreign fn it is (the process lookup refuses it by name); a declaration
//! named so with a `#[link_name]` naming another symbol is not bound.

use blaze_lower::head;
use blaze_own::Alloc;
use super::source::NamesSource;
use blaze_typeck::ItemKey;
use std::boxed::Box;
use std::format;

use super::check::Records;
use super::heads::{CrateCfg, Store};
use super::index::Index;
use super::keys::checker_of;
use super::reach::Reached;
use blaze_id::ItemKey as LibKey;

/// The lang row the program's `#[global_allocator]` static is filed under.
pub const OWN_ALLOCATOR_ROW: &str = "global_allocator";

/// One of the allocator's `extern "Rust"` declarations, by its symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shim {
    Alloc,
    Dealloc,
    Realloc,
    AllocZeroed,
    ErrorHandler,
    /// `__rust_no_alloc_shim_is_unstable_v2`: generated empty, bound to nothing.
    Marker,
}

impl Shim {
    pub const ALL: [Shim; 6] = [Shim::Alloc, Shim::Dealloc, Shim::Realloc, Shim::AllocZeroed, Shim::ErrorHandler, Shim::Marker];

    /// The symbol alloc declares it by.
    pub fn declared(self) -> &'static str {
        match self {
            Shim::Alloc => "__rust_alloc",
            Shim::Dealloc => "__rust_dealloc",
            Shim::Realloc => "__rust_realloc",
            Shim::AllocZeroed => "__rust_alloc_zeroed",
            Shim::ErrorHandler => "__rust_alloc_error_handler",
            Shim::Marker => "__rust_no_alloc_shim_is_unstable_v2",
        }
    }

    /// The fn a call of it runs when the graph has no `#[global_allocator]`: std's
    /// `__default_lib_allocator` fns, alloc's `__rdl_alloc_error_handler`. `None` for the
    /// marker, which runs nothing.
    pub fn default_fn(self) -> Option<&'static str> {
        match self {
            Shim::Alloc => Some("__rdl_alloc"),
            Shim::Dealloc => Some("__rdl_dealloc"),
            Shim::Realloc => Some("__rdl_realloc"),
            Shim::AllocZeroed => Some("__rdl_alloc_zeroed"),
            Shim::ErrorHandler => Some("__rdl_alloc_error_handler"),
            Shim::Marker => None,
        }
    }

    fn of_symbol(symbol: &str) -> Option<Shim> {
        Shim::ALL.into_iter().find(|s| s.declared() == symbol)
    }
}

/// Why an allocator declaration is not bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Its text's attributes did not read.
    Attrs { shim: Shim },
    /// No fn with a body in the graph is named its default.
    NoDefault { shim: Shim },
    /// Two fns with bodies are named its default.
    DefaultTwice { shim: Shim },
    /// The graph has a `#[global_allocator]` static, at this path.
    OwnAllocator { shim: Shim, static_path: Box<str> },
    /// The default's key is another item's in the key map.
    KeyClash { shim: Shim },
    /// The names source did not answer a fact the declaration needs (its crate's cfg or
    /// edition, or whether an `extern` block declares it): the error is in the source's log.
    Unanswered { shim: Shim },
}

impl Refused {
    /// The refusal's class: the rule that refused, for a survey's grouping.
    pub fn class(&self) -> &'static str {
        match self {
            Refused::Attrs { .. } => "an allocator declaration (`extern \"Rust\"` `__rust_*`) whose attributes do not read",
            Refused::NoDefault { .. } => {
                "a call of an allocator declaration with no `#[global_allocator]` and no default allocator fn (std's `__rdl_*`) in the graph"
            }
            Refused::DefaultTwice { .. } => "a default allocator fn (`__rdl_*`) defined twice in the graph",
            Refused::OwnAllocator { .. } => {
                "a program with its own `#[global_allocator]` static: the built-in attribute's expansion (the `__rust_*` fns calling its `GlobalAlloc` methods) is not built by blaze yet"
            }
            Refused::KeyClash { .. } => "the default allocator fn's key is another item's in the key map",
            Refused::Unanswered { .. } => "an allocator declaration (`extern \"Rust\"` `__rust_*`) about which the names records did not answer (the error is in the source's log)",
        }
    }

    /// The whole refusal: the class, the declaration and what it names.
    pub fn detail(&self) -> Box<str> {
        let (shim, extra) = match self {
            Refused::Attrs { shim } | Refused::KeyClash { shim } | Refused::Unanswered { shim } => (*shim, Box::<str>::from("")),
            Refused::NoDefault { shim } | Refused::DefaultTwice { shim } => (*shim, format!(" (`{}`)", shim.default_fn().unwrap_or("")).into_boxed_str()),
            Refused::OwnAllocator { shim, static_path } => (*shim, format!(" (the static `{static_path}`)").into_boxed_str()),
        };
        format!("`{}`: {}{extra}", shim.declared(), self.class()).into_boxed_str()
    }
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.detail())
    }
}

/// Which allocator declaration the fn `key` is: a module-level fn of an `extern "Rust"` block
/// named one of [`Shim::ALL`]'s symbols with no `#[link_name]` naming another. `Ok(None)` for
/// every other item (a fn of an `extern "C"` block named so is a C symbol like any other).
/// `alloc` holds the signature's lowering, dropped on return.
pub fn declares<'n, S: NamesSource<'n>>(names: S, store: &Store<'_>, key: ItemKey, alloc: Alloc<'_>) -> Result<Option<Shim>, Refused> {
    let Some(h) = store.fns.get(&key) else { return Ok(None) };
    if h.has_body || h.container.is_some() {
        return Ok(None);
    }
    let Some(shim) = names.name_of(h.lib).and_then(Shim::of_symbol) else { return Ok(None) };
    let unanswered = || Refused::Unanswered { shim };
    let (_, abi) = super::program::placed(names, h.lib).map_err(|_| unanswered())?;
    if abi != Some("Rust") {
        return Ok(None);
    }
    let refuse = || Refused::Attrs { shim };
    let krate = names.crate_of(h.lib).ok_or_else(refuse)?;
    let atoms = names.cfg_atoms(krate).ok_or_else(unanswered)?;
    let edition = names.edition_of(krate).ok_or_else(unanswered)?;
    let sig = blaze_lower::lower_signature(h.text, edition, &mut CrateCfg(atoms), alloc).map_err(|_| refuse())?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| refuse())? {
        let a = v.attr(n).map_err(|_| refuse())?;
        if v.text_at(a.path).map_err(|_| refuse())?.trim() != "link_name" {
            continue;
        }
        if crate::front::unquote(v.text_at(a.args).map_err(|_| refuse())?) != shim.declared() {
            return Ok(None);
        }
    }
    Ok(Some(shim))
}

/// The item for a refusal: front2 item_shown.rs `ItemShown` (its whole path and its key; never
/// a `_` in place of a name).
fn path_of<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Box<str> {
    super::item_shown::ItemShown::of_lib(names, lib).boxed()
}

/// The one fn of the graph named `name` (a proc-macro unit's items are never linked into the
/// program and are skipped, as the index skips them).
///
/// Over records this reads every crate's item rows (`each_item`: rows, no text): the default
/// lives in a crate the calling body need not name (std's `__rdl_*` for alloc's `__rust_*`).
/// Asked only for a body that calls an allocator declaration, once per declaration. The end
/// form is a per-crate table of `#[rustc_std_internal_symbol]` fns by name in the items record
/// (the notes, "## Piece Z", item 1, asked of NAMES runner 20).
fn default_named<'n, S: NamesSource<'n>>(names: S, shim: Shim, name: &str) -> Result<LibKey, Refused> {
    let mut found: Option<LibKey> = None;
    let mut twice = false;
    for krate in 0..names.crate_count() {
        match names.proc_macro(krate) {
            Some(true) => continue,
            Some(false) => {}
            None => return Err(Refused::Unanswered { shim }),
        }
        let read = names.each_item(krate, &mut |key, _| {
            if names.name_of(key) != Some(name) || names.kind_of_item(key) != Some(blaze_surface::ItemKind::Function) {
                return;
            }
            if found.is_some_and(|f| f != key) {
                twice = true;
            }
            found = Some(key);
        });
        if !read {
            return Err(Refused::Unanswered { shim });
        }
        if twice {
            return Err(Refused::DefaultTwice { shim });
        }
    }
    found.ok_or(Refused::NoDefault { shim })
}

/// The fn a call of the declaration `shim` runs: its default, or the refusal.
fn target<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>, shim: Shim) -> Result<Option<LibKey>, Refused> {
    if let Some(own) = index.lang(OWN_ALLOCATOR_ROW) {
        return Err(Refused::OwnAllocator { shim, static_path: path_of(names, own) });
    }
    match shim.default_fn() {
        None => Ok(None),
        Some(name) => default_named(names, shim, name).map(Some),
    }
}

/// The fn a call of `item` runs, when `item` is an allocator declaration with a default
/// (not generic: no arguments). `None` for every other item, the marker, and a declaration
/// [`reached`] refused (it is then in no table, and its caller is refused with that cause).
pub fn bound<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>, store: &Store<'_>, item: ItemKey, alloc: Alloc<'_>) -> Option<ItemKey> {
    match declares(names, store, item, alloc) {
        Ok(Some(shim)) => target(names, index, shim).ok().flatten().map(checker_of),
        Ok(None) | Err(_) => None,
    }
}

fn failed(why: &Refused) -> Reached {
    Reached::Failed { class: why.class().into(), detail: why.detail(), refusal: None }
}

/// The walk's outcome for `k`, a fn whose head says it has no body, when it is an allocator
/// declaration: [`Reached::NoBody`] and its default to queue (its key filed; none for the
/// marker), or the refusal naming why there is none. `None` for every other fn: the next
/// binding's ([`super::panic_entry::reached`]) to say.
pub fn reached(r: &mut Records<'_, '_>, k: ItemKey, alloc: Alloc<'_>) -> Option<(Reached, Option<ItemKey>)> {
    let shim = match declares(r.names, r.store, k, alloc) {
        Ok(None) => return None,
        Err(why) => return Some((failed(&why), None)),
        Ok(Some(shim)) => shim,
    };
    Some(match target(r.names, r.index, shim) {
        Err(why) => (failed(&why), None),
        Ok(None) => (Reached::NoBody, None),
        Ok(Some(lib)) => match r.keys.item(lib) {
            Ok(default) => (Reached::NoBody, Some(default)),
            Err(_) => (failed(&Refused::KeyClash { shim }), None),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each declaration alloc writes maps to the default std or alloc defines for it, and the
    /// marker to none; a symbol that is not one of the six is no shim.
    ///
    /// Prints: `test front2::alloc_shim::tests::each_declaration_has_its_default ... ok`
    #[test]
    fn each_declaration_has_its_default() {
        let pairs: [(&str, Option<&str>); 6] = [
            ("__rust_alloc", Some("__rdl_alloc")),
            ("__rust_dealloc", Some("__rdl_dealloc")),
            ("__rust_realloc", Some("__rdl_realloc")),
            ("__rust_alloc_zeroed", Some("__rdl_alloc_zeroed")),
            ("__rust_alloc_error_handler", Some("__rdl_alloc_error_handler")),
            ("__rust_no_alloc_shim_is_unstable_v2", None),
        ];
        for (declared, default) in pairs {
            let shim = Shim::of_symbol(declared).unwrap_or_else(|| panic!("`{declared}` is a shim"));
            assert_eq!(shim.default_fn(), default, "{declared}");
        }
        assert_eq!(Shim::of_symbol("__rust_start_panic"), None);
        assert_eq!(Shim::of_symbol("malloc"), None);
    }
}
