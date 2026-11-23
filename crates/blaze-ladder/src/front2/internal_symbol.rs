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

use super::alloc_shim::Shim;
use super::check::Records;
use super::heads::{CrateCfg, Store};
use super::keys::Keys;
use super::reach::Reached;
use super::source::NamesSource;
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
pub struct Declared<'n> {
    pub lib: LibKey,
    pub name: &'n str,
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
    /// The records could not supply one of the crates searched for the definition.
    Unread { name: Box<str>, krate: usize },
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
            InternalRefused::Unread { .. } => "the names source could not answer the internal symbol lookup",
        }
    }

    /// The whole refusal: the class and what it names.
    pub fn detail(&self) -> Box<str> {
        self.to_string().into_boxed_str()
    }
}

impl core::fmt::Display for InternalRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InternalRefused::NoDefinition { name }
            | InternalRefused::NotInRustBlock { name }
            | InternalRefused::Attrs { name }
            | InternalRefused::KeyClash { name }
            | InternalRefused::Unplaced { name } => write!(f, "`{name}`: {}", self.class()),
            InternalRefused::DefinedTwice { name, first, second } => write!(f, "`{name}`: {} (`{first}` and `{second}`)", self.class()),
            InternalRefused::NoPanicRuntime { .. } => f.write_str(self.class()),
            InternalRefused::Unread { name, krate } => write!(f, "`{name}` in crate {krate}: {}", self.class()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Declaration,
    Definition,
}

/// The body flag and cfg-expanded attributes come from the same signature lowering.
fn carries<'n, S: NamesSource<'n>>(names: S, krate: usize, text: &str, role: Role, alloc: Alloc<'_>) -> Result<bool, ()> {
    // The attribute is a word of the item's own text wherever it is written (an expansion's
    // text included): a text without it carries none, and nothing is lowered for it.
    if !text.contains(ATTR) {
        return Ok(false);
    }
    let atoms = names.cfg_atoms(krate).ok_or(())?;
    let sig = blaze_lower::lower_signature(text, names.edition_of(krate).ok_or(())?, &mut CrateCfg(atoms), alloc).map_err(|_| ())?;
    let v = sig.view();
    if role == Role::Definition && v.head[head::FLAGS] & blaze_lower::machine::sig::HAS_BODY == 0 {
        return Ok(false);
    }
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| ())? {
        let a = v.attr(n).map_err(|_| ())?;
        if v.text_at(a.path).map_err(|_| ())?.trim() == ATTR {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The `#[link_name]` written on the lowered item `text`, when it names a symbol other than
/// `name`: such a declaration is not bound by name here. `lib` is the key holding the item's
/// value rows (links.rs `values_key`): a value written as a macro call is the names' row of it
/// (links.rs `symbol_of`).
fn renamed<'n, S: NamesSource<'n>>(names: S, krate: usize, lib: LibKey, text: &str, name: &str, alloc: Alloc<'_>) -> Result<bool, ()> {
    if !text.contains("link_name") {
        return Ok(false);
    }
    let atoms = names.cfg_atoms(krate).ok_or(())?;
    let edition = names.edition_of(krate).ok_or(())?;
    let sig = blaze_lower::lower_signature(text, edition, &mut CrateCfg(atoms), alloc).map_err(|_| ())?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| ())? {
        let a = v.attr(n).map_err(|_| ())?;
        // The value cooked as the native image reads it (links.rs `symbol_of`: a value written
        // as a macro call is the names' row of the item, as the link table's).
        if v.text_at(a.path).map_err(|_| ())?.trim() == "link_name"
            && super::links::with_symbol_of(v.text_at(a.args).map_err(|_| ())?, Some(lib), |item| names.attr_value(item, "link_name"), edition, |res| res.map(|sym| sym != name).map_err(|_| ()))?
        {
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
    runtime_attr(root.text, &root.outline)
}

fn runtime_attr(text: &str, outline: &blaze_file_items::Outline<'_>) -> bool {
    outline.file_attrs().iter().any(|a| {
        if !a.inner {
            return false;
        }
        let Some(written) = text.get(a.range.start as usize..a.range.end as usize) else { return false };
        let inside = written.trim().strip_prefix("#!").map(str::trim_start).and_then(|s| s.strip_prefix('[')).and_then(|s| s.strip_suffix(']'));
        inside.is_some_and(|s| s.trim() == PANIC_RUNTIME)
    })
}

fn marked_runtime<'n, S: NamesSource<'n>>(names: S, krate: usize, alloc: Alloc<'_>) -> Option<bool> {
    if let Some(solved) = names.solved() {
        return Some(is_panic_runtime(solved, krate));
    }
    let text = names.file_text(krate, 0)?;
    let edition = names.edition_of(krate)?;
    Some(runtime_attr(text, &blaze_file_items::outline(text, edition, alloc)))
}

/// The definition a call of the std-internal declaration named `name` runs, under the
/// program's strategy `panic`: for [`START_PANIC`] and [`PANIC_CLEANUP`], the one fn of that
/// name carrying [`ATTR`] in a `#![panic_runtime]` crate named [`runtime_crate`]`(panic)`;
/// for every other name, the one such fn in any crate. A proc-macro unit's items are never
/// linked into the program and are skipped, as the index skips them. Two candidates are
/// [`InternalRefused::DefinedTwice`] naming the two smallest keys, so the answer is the same
/// in any order the graph is walked.
pub fn definition<'n, S: NamesSource<'n>>(names: S, name: &str, panic: Panic, alloc: Alloc<'_>) -> Result<LibKey, InternalRefused> {
    let runtime = is_runtime_name(name);
    let wanted = runtime_crate(panic);
    let mut found: Option<LibKey> = None;
    let mut second: Option<LibKey> = None;
    let mut runtime_seen = false;
    for krate in 0..names.crate_count() {
        let unread = || InternalRefused::Unread { name: name.into(), krate };
        if names.proc_macro(krate).ok_or_else(unread)? {
            continue;
        }
        if runtime {
            let root = names.root_module(krate).ok_or_else(unread)?;
            if names.module_path(root).ok_or_else(unread)? != wanted {
                continue;
            }
            if !marked_runtime(names, krate, alloc).ok_or_else(unread)? {
                continue;
            }
            runtime_seen = true;
        }
        let mut failure = None;
        let listed = names.each_item(krate, &mut |key, _| {
            if failure.is_some() || names.name_of(key) != Some(name) {
                return;
            }
            match candidate(names, krate, key, name, alloc) {
                Ok(false) => return,
                Err(why) => {
                    failure = Some(why);
                    return;
                }
                Ok(true) => {}
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
        });
        if !listed {
            return Err(unread());
        }
        if let Some(why) = failure {
            return Err(why);
        }
    }
    if let (Some(first), Some(second)) = (found, second) {
        let path = |k| super::item_shown::ItemShown::of_lib(names, k).boxed();
        return Err(InternalRefused::DefinedTwice { name: Box::from(name), first: path(first), second: path(second) });
    }
    match found {
        Some(key) => Ok(key),
        None if runtime && !runtime_seen => Err(InternalRefused::NoPanicRuntime { strategy: panic }),
        None => Err(InternalRefused::NoDefinition { name: Box::from(name) }),
    }
}

fn candidate<'n, S: NamesSource<'n>>(names: S, krate: usize, key: LibKey, name: &str, alloc: Alloc<'_>) -> Result<bool, InternalRefused> {
    let unread = || InternalRefused::Unread { name: name.into(), krate };
    if names.kind_of_item(key).ok_or_else(unread)? != blaze_surface::ItemKind::Function {
        return Ok(false);
    }
    let text = names.text_of(key).ok_or_else(unread)?.text;
    carries(names, krate, text, Role::Definition, alloc).map_err(|()| InternalRefused::Attrs { name: name.into() })
}

/// A code-cache hit can reuse the caller before instance resolution runs. File the search
/// domain in the caller's BodyCheck reads whenever it used a potentially internal fn head.
/// The items section covers crate metadata, file digests and the complete set of item rows,
/// including the absence of a competing definition in an otherwise unrelated crate.
pub(crate) fn record_search<'n>(
    tables: super::tables::view::Tables<'_, 'n>, store: &Store<'n>, facts: &super::reads::BodyReads<'_>,
) {
    use super::reads::FactKind;
    let searches = facts.rows.iter().filter(|r| r.fact.kind == FactKind::Head).any(|r| {
        // A type check can finish before linkage is decided. An unread declaration keeps
        // the covering reads too. Linkage reports its own typed refusal at the call.
        !matches!(declares(tables, store, ItemKey(r.fact.key), store.tys.alloc()), Ok(None))
    });
    if searches {
        for k in 0..tables.crate_count() {
            // Tables indexes the records' crate-key slice. Its length is crate_count().
            let Some(key) = tables.crate_key(k) else {
                tables.gap("a crate in the internal symbol search has no crate key", None);
                return;
            };
            tables.reads().read(search_read(key));
        }
    }
}

/// One covering read for all the facts the search can ask of this crate: its `#![panic_runtime]`
/// flag and any `#[rustc_std_internal_symbol]` function items (`NameCell::InternalSymbols`), which
/// is absent for every crate with neither so ordinary edits in unrelated crates wake no callers.
pub(crate) fn search_read(key: blaze_id::CrateItemKey) -> super::tables::recorder::TableRead {
    use blaze_scope::names_cells::NameCell;
    super::tables::recorder::TableRead::Names(NameCell::InternalSymbols { krate: key.prefix() })
}

fn may_declare(head: &super::heads::FnHead<'_>) -> bool {
    !head.has_body && head.container.is_none() && head.text.contains(ATTR)
}

/// Which std-internal declaration the fn `key` is: a fn with no body, declared in an `extern`
/// block, whose attributes carry [`ATTR`], named none of alloc_shim's names, with no
/// `#[link_name]` naming another symbol. `Ok(None)` for every other item;
/// [`InternalRefused::NotInRustBlock`] when the block's ABI is not `"Rust"`. `alloc` holds the
/// signature's lowering, dropped on return.
pub fn declares<'n, S: NamesSource<'n>>(names: S, store: &Store<'n>, key: ItemKey, alloc: Alloc<'_>) -> Result<Option<Declared<'n>>, InternalRefused> {
    let Some(h) = store.fns.get(&key) else { return Ok(None) };
    if !may_declare(h) {
        return Ok(None);
    }
    // A module-level declaration is named and placed by the names library; one an `extern`
    // block in a body declares, by the store (panic_entry's `local_name`, `local_abi`).
    let local_abi = super::panic_entry::local_abi(store, key);
    let name = if local_abi.is_some() {
        super::panic_entry::local_name(store, key).map_err(|_| InternalRefused::Attrs { name: super::item_shown::ItemShown::of_lib(names, h.lib).boxed() })?
    } else {
        names.name_of(h.lib)
    };
    let Some(name) = name else { return Ok(None) };
    if Shim::ALL.iter().any(|s| s.declared() == name) {
        return Ok(None);
    }
    let abi = match local_abi {
        Some(local) => Some(local),
        None => names.extern_abi(h.lib).ok_or_else(|| InternalRefused::Unplaced { name: name.into() })?,
    };
    let Some(abi) = abi else { return Ok(None) };
    let attrs = || InternalRefused::Attrs { name: Box::from(name) };
    let krate = names.crate_of(h.lib).ok_or_else(attrs)?;
    if !carries(names, krate, h.text, Role::Declaration, alloc).map_err(|()| attrs())? {
        return Ok(None);
    }
    if abi != "Rust" {
        return Err(InternalRefused::NotInRustBlock { name: Box::from(name) });
    }
    if renamed(names, krate, super::links::values_key(names, store, h.lib), h.text, name, alloc).map_err(|()| attrs())? {
        return Ok(None);
    }
    Ok(Some(Declared { lib: h.lib, name }))
}

/// The definition a call of this declaration runs. Filing the library key checks the same
/// collision in the reach walk and when the records supply a definition for the first time.
pub fn bound<'n, S: NamesSource<'n>>(names: S, keys: &Keys, d: Declared<'_>, panic: Panic, alloc: Alloc<'_>) -> Result<ItemKey, InternalRefused> {
    let lib = definition(names, d.name, panic, alloc)?;
    keys.item(lib).map_err(|_| InternalRefused::KeyClash { name: d.name.into() })
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
    Some(match bound(r.names, r.keys, d, panic, alloc) {
        Err(why) => (failed(&why), None),
        Ok(def) => (Reached::NoBody, Some(def)),
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
