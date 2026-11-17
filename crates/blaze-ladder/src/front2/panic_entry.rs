//! A callee that an `extern` block in a BODY declares, as the run path sees it: a foreign fn
//! like a module-level one, or the one declaration that is no symbol at all: the pinned core's
//! panic entry.
//!
//! The pinned core (library/core/src/panicking.rs, `panic_fmt`):
//! ```text
//! // NOTE This function never crosses the FFI boundary; it's a Rust-to-Rust call
//! // that gets resolved to the `#[panic_handler]` function.
//! unsafe extern "Rust" {
//!     #[lang = "panic_impl"]
//!     fn panic_impl(pi: &PanicInfo<'_>) -> !;
//! }
//! ..
//! // SAFETY: `panic_impl` is defined in safe Rust code and thus is safe to call.
//! unsafe { panic_impl(&pi) }
//! ```
//! and the pinned std (library/std/src/panicking.rs) defines the fn it resolves to in a std
//! program: "Entry point of panics from the core crate (`panic_impl` lang item)":
//! `#[panic_handler] pub(crate) fn panic_handler(info: &core::panic::PanicInfo<'_>) -> !`.
//!
//! The Reference, "Panic", "The `panic_handler` attribute" (the pinned nightly's copy,
//! share/doc/rust/html/reference/panic.html, read 2026-11-16): [panic.panic_handler.intro]
//! "The panic_handler attribute can be applied to a function to define the behavior of
//! panics." [panic.panic_handler.allowed-positions] "The panic_handler attribute can only be
//! applied to a function with signature fn(&PanicInfo) -> !." [panic.panic_handler.unique]
//! "There must be a single panic_handler function in the dependency graph."
//!
//! So a call of the `#[lang = "panic_impl"]` declaration IS a call of the program's one
//! `#[panic_handler]` fn, a Rust fn with a body:
//!
//! - the walk ([`reached`], asked by reach.rs where it meets a fn with no body) queues the
//!   handler as a callee, like any Rust fn a body calls;
//! - the table (program/tabled.rs `row`, by [`declares`]) files the declaration as a row with
//!   no body and NO symbol (the builder reads a call's ABI from the item the call names);
//! - instance resolution (program.rs `resolve`, by [`bound`]) answers the handler for the
//!   declaration, so the builder is told an ordinary direct call of a known instance.
//!
//! The handler is found in the lang rows under [`HANDLER_ROW`]: the names library files a
//! `#[panic_handler]` fn there under the attribute's own name (blaze-scope task/lower.rs, the
//! attribute match), so the items record carries it as one more lang row and two handlers in
//! the graph are the index's `LangTwice` naming `panic_handler` (index.rs), before any body.
//! None is [`Refused::NoHandler`], when a body that calls the declaration is reached: a
//! program that cannot panic needs no handler to be found.
//!
//! Every other fn of an `extern` block in a body is foreign exactly as a module-level one is
//! ([`local_abi`]: the block's ABI, where `placed` reads a module-level block's from the
//! names library's outline; the symbol is `row`'s one rule: `#[link_name]`, else the name). A
//! `#[lang]` other than `panic_impl` on such a declaration is refused by name
//! ([`Refused::OtherLang`]): which item it stands for is that lang item's rule, not guessed.
//!
//! Not here: the handler's own signature is not compared with `fn(&PanicInfo) -> !` (the
//! declaration's is what each call is checked against); a foreign STATIC of a block in a body
//! is not placed by the statics' image yet (statics.rs).

use blaze_lower::head;
use blaze_own::Alloc;
use super::source::NamesSource;
use blaze_typeck::ItemKey;

use super::check::Records;
use super::foreign_locals::ForeignKind;
use super::heads::{CrateCfg, Store};
use super::index::Index;
use super::keys::checker_of;
use super::reach::Reached;

/// The lang name the pinned core declares its panic entry under.
pub const DECLARED_AS: &str = "panic_impl";

/// The lang row the program's `#[panic_handler]` fn is filed under.
pub const HANDLER_ROW: &str = "panic_handler";

/// Why a fn an `extern` block in a body declares is not bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The store holds its foreign row and not its head's text.
    NoHead,
    /// No crate of the names library holds its key.
    NoCrate,
    /// Its text's attributes did not read (a `cfg` the crate does not answer, a row out of
    /// its table).
    Attrs,
    /// It carries a `#[lang]` other than `panic_impl`.
    OtherLang,
    /// It is the `panic_impl` declaration and no fn of the graph is `#[panic_handler]`.
    NoHandler,
    /// The handler's key is another item's in the key map.
    KeyClash,
}

impl Refused {
    /// The refusal's words: the rule that refused, for a survey's class and a run's error.
    pub fn form(self) -> &'static str {
        match self {
            Refused::NoHead => "a fn of an extern block in a block whose text the store does not hold",
            Refused::NoCrate => "a fn of an extern block in a block whose key no crate holds",
            Refused::Attrs => "a fn of an extern block in a block whose attributes do not read",
            Refused::OtherLang => "a `#[lang]` other than `panic_impl` on a fn of an extern block in a block",
            Refused::NoHandler => {
                "a call of the `#[lang = \"panic_impl\"]` declaration with no `#[panic_handler]` fn to bind it to (the Reference, [panic.panic_handler.unique]: \"There must be a single panic_handler function in the dependency graph\")"
            }
            Refused::KeyClash => "the `#[panic_handler]` fn's key is another item's in the key map",
        }
    }
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.form())
    }
}

/// The ABI (unquoted, as `placed` gives a module-level block's) of the block that declares the
/// fn `key`, when an `extern` block in a body does: what makes it a foreign fn to the table.
/// `None` for every other item.
pub fn local_abi<'n>(store: &Store<'n>, key: ItemKey) -> Option<&'n str> {
    let f = store.locals.foreign(key)?;
    (f.kind == ForeignKind::Fn).then(|| crate::front::unquote(f.abi))
}

/// The name the fn `key` is declared by, when an `extern` block in a body declares it: its
/// symbol where it carries no `#[link_name]` (the names library names no local item, so the
/// path a finding shows it by does not end in it). `Ok(None)` for every other item.
pub fn local_name<'n>(store: &Store<'n>, key: ItemKey) -> Result<Option<&'n str>, Refused> {
    if store.locals.foreign(key).is_none() {
        return Ok(None);
    }
    let local = store.locals.head(key).ok_or(Refused::NoHead)?;
    let rows = store.locals.body(local.enclosing).ok_or(Refused::NoHead)?.rows;
    let row = rows.iter().find(|r| r.key == local.lib).ok_or(Refused::NoHead)?;
    Ok(Some(row.name))
}

/// Whether the fn `key` is the `#[lang = "panic_impl"]` declaration of an `extern` block in a
/// body: read from its own text's attribute rows, as its `#[link_name]` is. `Ok(false)` for
/// every item no such block declares and for a declaration with no `#[lang]`. `alloc` holds
/// the signature's lowering, dropped on return.
pub fn declares<'n, S: NamesSource<'n>>(names: S, store: &Store<'_>, key: ItemKey, alloc: Alloc<'_>) -> Result<bool, Refused> {
    let Some(f) = store.locals.foreign(key) else { return Ok(false) };
    if f.kind != ForeignKind::Fn {
        return Ok(false);
    }
    let local = store.locals.head(key).ok_or(Refused::NoHead)?;
    let krate = names.crate_of(local.lib).ok_or(Refused::NoCrate)?;
    // Over records an unanswered fact is in the source's log; the crate is refused here.
    let atoms = names.cfg_atoms(krate).ok_or(Refused::NoCrate)?;
    let edition = names.edition_of(krate).ok_or(Refused::NoCrate)?;
    let sig = blaze_lower::lower_signature(local.text, edition, &mut CrateCfg(atoms), alloc).map_err(|_| Refused::Attrs)?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|_| Refused::Attrs)? {
        let a = v.attr(n).map_err(|_| Refused::Attrs)?;
        if v.text_at(a.path).map_err(|_| Refused::Attrs)?.trim() != "lang" {
            continue;
        }
        let name = crate::front::unquote(v.text_at(a.args).map_err(|_| Refused::Attrs)?);
        return if name == DECLARED_AS { Ok(true) } else { Err(Refused::OtherLang) };
    }
    Ok(false)
}

/// The fn a call of `item` runs, when `item` is the `panic_impl` declaration and the graph
/// has its handler: the program's `#[panic_handler]` fn (not generic: no arguments). `None`
/// for every other item, and for a declaration [`reached`] refused (it is then in no table,
/// and its caller is refused with that cause).
pub fn bound<'n, S: NamesSource<'n>>(names: S, index: &Index<'_>, store: &Store<'_>, item: ItemKey, alloc: Alloc<'_>) -> Option<ItemKey> {
    match declares(names, store, item, alloc) {
        Ok(true) => index.lang(HANDLER_ROW).map(checker_of),
        Ok(false) | Err(_) => None,
    }
}

fn failed(why: Refused) -> Reached {
    Reached::Failed { class: why.form().into(), detail: why.form().into(), refusal: None }
}

/// The walk's outcome for `k`, a fn whose head says it has no body, and the fn to queue as
/// its callee: for the `panic_impl` declaration, [`Reached::NoBody`] and the program's
/// `#[panic_handler]` fn (its key filed), or the refusal naming why there is none; for every
/// other fn `(Reached::NoBody, None)`: what calls it is the table's to say.
pub fn reached(r: &mut Records<'_, '_>, k: ItemKey, alloc: Alloc<'_>) -> (Reached, Option<ItemKey>) {
    match declares(r.names, r.store, k, alloc) {
        Ok(false) => (Reached::NoBody, None),
        Err(why) => (failed(why), None),
        Ok(true) => match r.index.lang(HANDLER_ROW) {
            None => (failed(Refused::NoHandler), None),
            Some(lib) => match r.keys.item(lib) {
                Ok(handler) => (Reached::NoBody, Some(handler)),
                Err(_) => (failed(Refused::KeyClash), None),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal for a program that panics with no handler names the attribute and quotes
    /// the rule ([panic.panic_handler.unique]), and never reads as a missing symbol.
    ///
    /// Prints: `test front2::panic_entry::tests::no_handler_names_the_rule ... ok`
    #[test]
    fn no_handler_names_the_rule() {
        let words = Refused::NoHandler.form();
        assert!(words.contains("#[panic_handler]"), "{words}");
        assert!(words.contains("There must be a single panic_handler function in the dependency graph"), "{words}");
        assert!(!words.contains("symbol"), "{words}");
    }
}
