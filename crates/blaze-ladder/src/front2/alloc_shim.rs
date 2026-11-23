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
//!   default's ABI (the seam's `fn_abi` answers the bound default's for the declaration:
//!   blaze-build `Adts::abi`).
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
//! rows under [`OWN_ALLOCATOR_ROW`], as it files `#[panic_handler]`): the pinned alloc's
//! comment above says what runs then, "the code expanding that attribute macro generates those
//! functions". The built-in attribute (core/src/macros/mod.rs, `pub macro global_allocator(
//! $item:item)`, `#[rustc_builtin_macro]`, `#[allow_internal_unstable(rustc_attrs,
//! ptr_alignment_type)]`) keeps the static and writes, beside it in the static's crate, one
//! `#[rustc_std_internal_symbol] unsafe fn` per ALLOCATOR method, named as alloc declares it,
//! whose body calls the static's `GlobalAlloc` method:
//! ```text
//! #[rustc_std_internal_symbol]
//! unsafe fn __rust_alloc(size: usize, align: ..) -> *mut u8 {
//!     $crate::alloc::GlobalAlloc::alloc(&ALLOC, $crate::alloc::Layout::from_size_align..(size, align))
//! }
//! // `__rust_dealloc(ptr, size, align)` -> `GlobalAlloc::dealloc(&ALLOC, ptr, layout)`,
//! // `__rust_realloc(ptr, old_size, align, new_size)` -> `GlobalAlloc::realloc(&ALLOC, ptr, layout, new_size)`,
//! // `__rust_alloc_zeroed(size, align)` -> `GlobalAlloc::alloc_zeroed(&ALLOC, layout)`.
//! ```
//! So then a call of `__rust_alloc` (and the other three, [`Shim::of_allocator`]) IS a call of
//! the fn of that name the expansion wrote: the one fn with a body named so, carrying
//! `#[rustc_std_internal_symbol]`, in the static's crate ([`expansion_fn`]); the walk queues it
//! like a default, and its body is an ordinary call of the impl's `GlobalAlloc` method. None
//! there is [`Refused::NotExpanded`] (the names library did not expand the attribute), naming
//! the static: never a binding to std's defaults, which would quietly ignore the program's
//! allocator. The other two are not the allocator's: `__rust_alloc_error_handler` binds to
//! alloc's `__rdl_alloc_error_handler` as with no `#[global_allocator]` (the comment at alloc.rs
//! line 633: the default "if there is no `#[alloc_error_handler]`"), and the marker is built in
//! place as nothing, with or without one.
//!
//! Not here: a `#[alloc_error_handler]` fn (unstable `alloc_error_handler`; the pinned std
//! writes one, `std::alloc::rust_oom`, under `#[cfg(not(test))]`) is not looked for, so a std
//! program's `__rust_alloc_error_handler` runs alloc's default, not std's hook-then-abort (a
//! known gap: its message differs on an allocation failure); a declaration that reaches one of
//! these symbols only by `#[link_name]` under another name stays the foreign fn it is (the
//! process lookup refuses it by name); a declaration named so with a `#[link_name]` naming
//! another symbol is not bound.

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

    /// Whether a `#[global_allocator]`'s expansion writes this declaration's fn (rustc's
    /// ALLOCATOR methods: `alloc`, `dealloc`, `realloc`, `alloc_zeroed`): the four calls that go
    /// to the program's allocator. The error handler and the marker are not its.
    pub fn of_allocator(self) -> bool {
        match self {
            Shim::Alloc | Shim::Dealloc | Shim::Realloc | Shim::AllocZeroed => true,
            Shim::ErrorHandler | Shim::Marker => false,
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
    /// The graph has a `#[global_allocator]` static, at this path, and its crate holds no fn
    /// of the attribute's expansion for the declaration (the names library did not expand it).
    NotExpanded { shim: Shim, static_path: Box<str> },
    /// The `#[global_allocator]` static's crate holds two fns of the declaration's name
    /// carrying `#[rustc_std_internal_symbol]`: `first` and `second`, the two smallest by key.
    ExpandedTwice { shim: Shim, first: Box<str>, second: Box<str> },
    /// The default's key, or the expansion's fn's, is another item's in the key map.
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
            Refused::NotExpanded { .. } => {
                "a program with its own `#[global_allocator]` static whose crate holds no fn of the attribute's expansion (the `#[rustc_std_internal_symbol]` `__rust_*` fn calling its `GlobalAlloc` method): the names library did not expand the built-in attribute"
            }
            Refused::ExpandedTwice { .. } => {
                "two `#[rustc_std_internal_symbol]` fns of one allocator symbol in the crate of the `#[global_allocator]` static"
            }
            Refused::KeyClash { .. } => "the allocator fn's key (the default's or the expansion's) is another item's in the key map",
            Refused::Unanswered { .. } => "an allocator declaration (`extern \"Rust\"` `__rust_*`) about which the names records did not answer (the error is in the source's log)",
        }
    }

    /// The whole refusal: the class, the declaration and what it names.
    pub fn detail(&self) -> Box<str> {
        let (shim, extra) = match self {
            Refused::Attrs { shim } | Refused::KeyClash { shim } | Refused::Unanswered { shim } => (*shim, Box::<str>::from("")),
            Refused::NoDefault { shim } | Refused::DefaultTwice { shim } => (*shim, format!(" (`{}`)", shim.default_fn().unwrap_or("")).into_boxed_str()),
            Refused::NotExpanded { shim, static_path } => (*shim, format!(" (the static `{static_path}`)").into_boxed_str()),
            Refused::ExpandedTwice { shim, first, second } => (*shim, format!(" (`{first}` and `{second}`)").into_boxed_str()),
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
        // The value cooked as the native image reads it (links.rs `symbol_of`: a value written
        // as a macro call is the names' row of the item, as the link table's).
        let written = v.text_at(a.args).map_err(|_| refuse())?;
        let values_at = super::links::values_key(names, store, h.lib);
        if super::links::with_symbol_of(written, Some(values_at), |item| names.attr_value(item, "link_name"), edition, |res| res.map(|sym| sym != shim.declared()).map_err(|_| ())).map_err(|_| refuse())? {
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

/// The attribute the expansion's fns carry (rustc's `AllocFnFactory::attrs`).
const INTERNAL_SYMBOL: &str = "rustc_std_internal_symbol";

/// Whether the fn `key` of crate `krate` is written as the `#[global_allocator]` expansion
/// writes its fns: a fn item outside every `extern` block and every trait or impl (so it has a
/// body), whose attributes, `cfg_attr` evaluated under the crate's cfg, hold
/// `#[rustc_std_internal_symbol]`. `alloc` holds the signature's lowering, dropped on return.
fn is_expansion_fn<'n, S: NamesSource<'n>>(names: S, krate: usize, key: LibKey, shim: Shim, alloc: Alloc<'_>) -> Result<bool, Refused> {
    let unanswered = || Refused::Unanswered { shim };
    if names.kind_of_item(key) != Some(blaze_surface::ItemKind::Function) || names.member_owner(key).is_some() {
        return Ok(false);
    }
    match names.extern_abi(key) {
        Some(None) => {}
        Some(Some(_)) => return Ok(false),
        None => return Err(unanswered()),
    }
    let text = names.text_of(key).ok_or_else(unanswered)?.text;
    // The attribute is a word of the item's own text wherever it is written (an expansion's
    // text included): a text without it carries none, and nothing is lowered for it.
    if !text.contains(INTERNAL_SYMBOL) {
        return Ok(false);
    }
    let refuse = || Refused::Attrs { shim };
    let atoms = names.cfg_atoms(krate).ok_or_else(unanswered)?;
    let edition = names.edition_of(krate).ok_or_else(unanswered)?;
    let sig = blaze_lower::lower_signature(text, edition, &mut CrateCfg(atoms), alloc).map_err(|_| refuse())?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| refuse())? {
        let a = v.attr(n).map_err(|_| refuse())?;
        if v.text_at(a.path).map_err(|_| refuse())?.trim() == INTERNAL_SYMBOL {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The fn the `#[global_allocator]` static `own`'s expansion wrote for the declaration
/// `shim` (one of [`Shim::of_allocator`]'s four): the one fn of the static's crate named as the
/// declaration that [`is_expansion_fn`] says the expansion wrote. rustc's expansion writes it in
/// the static's own crate, so no other crate is read. None is [`Refused::NotExpanded`]; two are
/// [`Refused::ExpandedTwice`] naming the two smallest keys, the same in any walk order.
fn expansion_fn<'n, S: NamesSource<'n>>(names: S, own: LibKey, shim: Shim, alloc: Alloc<'_>) -> Result<LibKey, Refused> {
    let krate = names.crate_of(own).ok_or(Refused::Unanswered { shim })?;
    let name = shim.declared();
    let mut found: Option<LibKey> = None;
    let mut second: Option<LibKey> = None;
    let mut refused: Option<Refused> = None;
    let read = names.each_item(krate, &mut |key, _| {
        if refused.is_some() || names.name_of(key) != Some(name) {
            return;
        }
        match is_expansion_fn(names, krate, key, shim, alloc) {
            Err(why) => refused = Some(why),
            Ok(false) => {}
            Ok(true) => match found {
                None => found = Some(key),
                Some(f) if f == key => {}
                Some(f) if key < f => {
                    second = Some(f);
                    found = Some(key);
                }
                Some(_) => {
                    if second.is_none_or(|s| key < s) {
                        second = Some(key);
                    }
                }
            },
        }
    });
    if !read {
        return Err(Refused::Unanswered { shim });
    }
    if let Some(why) = refused {
        return Err(why);
    }
    match (found, second) {
        (Some(first), Some(second)) => Err(Refused::ExpandedTwice { shim, first: path_of(names, first), second: path_of(names, second) }),
        (Some(first), None) => Ok(first),
        (None, _) => {
            let shown = super::item_shown::ItemShown::of_lib(names, own);
            let static_path = if shown.path().is_some() {
                shown.boxed()
            } else if let Some(solved) = names.solved()
                && let Some(c) = solved.crates().get(krate)
                && let Some(lang) = c.tables.langs.iter().find(|l| l.key == own)
                && let Some(offset) = usize::try_from(lang.loc.offset).ok()
                && let Some(text) = names.file_text(krate, lang.loc.file)
                && let Some(tail) = text.get(offset..)
                && let Some(name) = super::static_shown::static_name(tail)
            {
                let full = blaze_own::format_in!(alloc, "{}::{name}", c.name);
                full.as_str().into()
            } else {
                shown.boxed()
            };
            Err(Refused::NotExpanded { shim, static_path })
        }
    }
}

/// The fn a call of the declaration `shim` runs: with a `#[global_allocator]` static in the
/// graph, for the allocator's four the fn its expansion wrote; otherwise its default; or the
/// refusal. `alloc` holds the lowerings [`expansion_fn`] reads, dropped on return.
fn target<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>, shim: Shim, alloc: Alloc<'_>) -> Result<Option<LibKey>, Refused> {
    if let Some(own) = index.lang(OWN_ALLOCATOR_ROW)
        && shim.of_allocator()
    {
        return expansion_fn(names, own, shim, alloc).map(Some);
    }
    match shim.default_fn() {
        None => Ok(None),
        Some(name) => default_named(names, shim, name).map(Some),
    }
}

/// The fn a call of `item` runs, when `item` is an allocator declaration with a default or a
/// `#[global_allocator]` expansion's fn (not generic: no arguments). `None` for every other
/// item, the marker, and a declaration [`reached`] refused (it is then in no table, and its
/// caller is refused with that cause).
pub fn bound<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>, store: &Store<'_>, item: ItemKey, alloc: Alloc<'_>) -> Option<ItemKey> {
    let shim = match declares(names, store, item, alloc) {
        Ok(Some(shim)) => shim,
        Ok(None) | Err(_) => return None,
    };
    let slot = match shim {
        Shim::Alloc => 0,
        Shim::Dealloc => 1,
        Shim::Realloc => 2,
        Shim::AllocZeroed => 3,
        Shim::ErrorHandler => 4,
        Shim::Marker => 5,
    };
    let mut cache = store.alloc_shims.get();
    if let Some(&Some(hit)) = cache.get(slot) {
        return hit;
    }
    let resolved = target(names, index, shim, alloc).ok().flatten().map(checker_of);
    if let Some(entry) = cache.get_mut(slot) {
        *entry = Some(resolved);
    }
    store.alloc_shims.set(cache);
    resolved
}

fn failed(why: &Refused) -> Reached {
    Reached::Failed { class: why.class().into(), detail: why.detail(), refusal: None }
}

/// The walk's outcome for `k`, a fn whose head says it has no body, when it is an allocator
/// declaration: [`Reached::NoBody`] and the fn to queue (its default, or the
/// `#[global_allocator]` expansion's fn; its key filed; none for the marker), or the refusal
/// naming why there is none. `None` for every other fn: the next binding's
/// ([`super::panic_entry::reached`]) to say.
pub fn reached(r: &mut Records<'_, '_>, k: ItemKey, alloc: Alloc<'_>) -> Option<(Reached, Option<ItemKey>)> {
    let shim = match declares(r.names, r.store, k, alloc) {
        Ok(None) => return None,
        Err(why) => return Some((failed(&why), None)),
        Ok(Some(shim)) => shim,
    };
    Some(match target(r.names, r.index, shim, alloc) {
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

    /// With a `#[global_allocator]`, exactly rustc's four ALLOCATOR methods go to the
    /// attribute's expansion; the error handler keeps alloc's default and the marker stays
    /// nothing. A refusal for a missing expansion names the declaration and the static.
    ///
    /// Prints: `test front2::alloc_shim::tests::the_allocator_four_go_to_the_expansion ... ok`
    #[test]
    fn the_allocator_four_go_to_the_expansion() {
        let own: Vec<&str> = Shim::ALL.into_iter().filter(|s| s.of_allocator()).map(Shim::declared).collect();
        assert_eq!(own, ["__rust_alloc", "__rust_dealloc", "__rust_realloc", "__rust_alloc_zeroed"]);
        assert_eq!(Shim::ErrorHandler.default_fn(), Some("__rdl_alloc_error_handler"));
        assert!(!Shim::ErrorHandler.of_allocator() && !Shim::Marker.of_allocator());
        let why = Refused::NotExpanded { shim: Shim::Alloc, static_path: Box::from("tempo::ALLOC") };
        let detail = why.detail();
        assert!(detail.starts_with("`__rust_alloc`: "), "{detail}");
        assert!(detail.contains("`#[global_allocator]` static") && detail.ends_with("(the static `tempo::ALLOC`)"), "{detail}");
    }
}
