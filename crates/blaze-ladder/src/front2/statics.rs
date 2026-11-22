//! The program's `static` items for the back half: each static a checked body names
//! (`Def::Static` in its `expr_defs`), and each static another static's value points at,
//! evaluated ONCE by front2's constant evaluator (consts.rs `Consts::value`, the one that
//! evaluates `const` items), its record the static's first bytes in the program's image
//! (run.rs `static_image`, blaze-build's `StaticImage`).
//!
//! The Reference, "Static items": "Static initializers are constant expressions evaluated at
//! compile time"; "A static item is similar to a constant, except that it represents an
//! allocation in the program that is initialized with the initializer expression. All
//! references and raw pointers to the static refer to the same allocation." So a static is
//! evaluated once, here, and a body's path to it is a place (the builder's `Def::Static`): the
//! address of its one allocation in the image, never a copy of its record.
//!
//! A pointer in an initializer (`static P: &u8 = &A[1];`, `static S: &str = "abc";`, a `fn`
//! pointer) is a relocation of the record (blaze-build data.rs, "The form"): to another
//! allocation of the record, to a static (`DataTarget::Static`: that static is then evaluated
//! too, by the work list below, so the image holds every static a relocation names), or to a
//! fn instance (placed by the image's `fill`; one the build did not make is the image's
//! `NoFn`, by key). The image is one writable block (data.rs, "Statics"): a `static mut` and a
//! static with interior mutability are written in place; an immutable static's pages are not
//! made read-only yet, which is a hardening, not a meaning.
//!
//! A static's kind ([`StaticItem`]) is its head's mutability (heads.rs `Store::statics`, made
//! with its type through the same `fn_head` a `const` item's head is made by) and whether it
//! carries `#[thread_local]` (read from its signature's attribute rows, as program.rs
//! `symbols_of` reads a fn's): a thread-local's record is the thread's template (run.rs
//! `static_image`, blaze-build tls.rs), the same path the old front's statics take.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_build::StaticItem;
use blaze_build::data::{DataAlloc, DataRecord, DataTarget};
use blaze_lower::head;
use blaze_typeck as tc;
use blaze_typeck::{Checked, ItemKey};

use super::check::Records;
use super::heads::CrateCfg;
use super::source::NamesSource;

/// The most statics one program evaluates: a bound on the work list (rule 18); past it the
/// program is refused by the static it stopped at.
pub const MOST_STATICS: usize = 65_536;

/// Every static the program's bodies reach, each evaluated once.
pub struct Statics {
    /// Each static's record, sorted by key: the seam's `static_records`.
    pub values: Vec<(ItemKey, DataRecord<Global>), Global>,
    /// Each static's kind, sorted by key: the seam's `static_item`.
    pub kinds: Vec<(ItemKey, StaticItem), Global>,
    /// Each static an `extern` block declares, with its C symbol, sorted by key: the seam's
    /// `foreign_static`. Its kind is in `kinds`; it has no record in `values` (no initializer).
    pub foreign: Vec<(ItemKey, Box<str>), Global>,
}

impl Statics {
    /// Static `item`'s kind; `None` for an item that is no static of the program.
    pub fn kind(&self, item: ItemKey) -> Option<StaticItem> {
        let at = self.kinds.binary_search_by_key(&item, |(k, _)| *k).ok()?;
        Some(self.kinds[at].1)
    }

    /// Static `item`'s C symbol when an `extern` block declares it; `None` for any other.
    pub fn foreign(&self, item: ItemKey) -> Option<&str> {
        let at = self.foreign.binary_search_by_key(&item, |(k, _)| *k).ok()?;
        Some(&self.foreign[at].1)
    }
}

/// Every static `bodies` name (`Def::Static`), and every static a record among them points
/// at, evaluated once by the evaluator in `r.consts`, by a work list in key order. Returns the
/// statics made and each static refused with its words (the evaluator's refusal, no head, an
/// attribute row that does not read): a run stops at the first, a survey files them.
///
/// A static an `extern` block declares is never evaluated (the Reference, "External blocks",
/// "Statics": it has no initializer; its storage is the foreign object's): its kind and C
/// symbol are filed (program.rs `foreign_static_symbol`, the one rule), and a body's path to
/// it is a place at that symbol (code_records.rs `InstanceNames::c_statics`). secp256k1-sys's
/// `extern "C" { pub static secp256k1_context_no_precomp: *const Context; }` was evaluated
/// as a Rust static and refused for its missing initializer (tempo t1855d).
pub fn static_values<'c, 'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, bodies: impl Iterator<Item = &'c Checked<'static>>) -> (Statics, Vec<(ItemKey, Box<str>), Global>) {
    let mut todo: Vec<ItemKey, Global> = Vec::new_in(Global);
    for c in bodies {
        todo.extend(c.expr_defs.column::<1>().iter().filter_map(|d| match d {
            tc::Def::Static(k, _) => Some(*k),
            _ => None,
        }));
    }
    // Popped from the end: sorted descending, so the smallest key is evaluated first.
    todo.sort_unstable_by(|a, b| b.cmp(a));
    todo.dedup();
    let mut done: Vec<ItemKey, Global> = Vec::new_in(Global);
    let mut values: Vec<(ItemKey, DataRecord<Global>), Global> = Vec::new_in(Global);
    let mut kinds: Vec<(ItemKey, StaticItem), Global> = Vec::new_in(Global);
    let mut refused: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
    let mut foreign: Vec<(ItemKey, Box<str>), Global> = Vec::new_in(Global);
    while let Some(k) = todo.pop() {
        let Err(at) = done.binary_search(&k) else { continue };
        done.insert(at, k);
        if done.len() > MOST_STATICS {
            refused.push((k, format!("more than {MOST_STATICS} statics reached (the work list's bound)").into_boxed_str()));
            break;
        }
        match super::program::foreign_static_symbol(r.names, &*r.keys, &*r.store, k) {
            Ok(Some(symbol)) => {
                match foreign_kind(r, k) {
                    Ok(kind) => {
                        kinds.push((k, kind));
                        foreign.push((k, symbol));
                    }
                    Err(why) => refused.push((k, why)),
                }
                continue;
            }
            Ok(None) => {}
            Err(why) => {
                refused.push((k, why));
                continue;
            }
        }
        match one_static(r, k) {
            Ok((record, kind)) => {
                for a in record.allocs.iter() {
                    for reloc in a.relocs.iter() {
                        if let DataTarget::Static(s) = reloc.target
                            && done.binary_search(&s).is_err()
                        {
                            todo.push(s);
                        }
                    }
                }
                values.push((k, record));
                kinds.push((k, kind));
            }
            Err(why) => refused.push((k, why)),
        }
    }
    values.sort_unstable_by_key(|(k, _)| *k);
    kinds.sort_unstable_by_key(|(k, _)| *k);
    foreign.sort_unstable_by_key(|(k, _)| *k);
    (Statics { values, kinds, foreign }, refused)
}

/// Static `k`'s record (copied out of the evaluator's arena) and its kind. The run's static
/// rows (lazy/fill.rs `static_value`) ask one static by this, the work list above many.
pub(crate) fn one_static<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, k: ItemKey) -> Result<(DataRecord<Global>, StaticItem), Box<str>> {
    let Some((consts, prepared)) = r.consts.take() else {
        return Err(Box::from("no evaluator: the program's records hold none"));
    };
    let made = consts.value(r, prepared, k).map(|record| {
        let mut allocs = Vec::with_capacity_in(record.data.allocs.len(), Global);
        for a in record.data.allocs.iter() {
            let mut bytes = Vec::with_capacity_in(a.bytes.len(), Global);
            bytes.extend_from_slice(&a.bytes);
            let mut relocs = Vec::with_capacity_in(a.relocs.len(), Global);
            relocs.extend_from_slice(&a.relocs);
            allocs.push(DataAlloc { bytes, align: a.align, mutable: a.mutable, relocs });
        }
        DataRecord { allocs }
    });
    r.consts = Some((consts, prepared));
    // The refusal WHOLE, on one line (rule 19: a cut at 200 characters hid Class Q's gap, the
    // key that had no row, in the middle of its number; hello run 8s, 2026-11-16).
    let record = made.map_err(|e| {
        let said = format!("{e}");
        said.chars().map(|c| if c == '\n' { ' ' } else { c }).collect::<Box<str>>()
    })?;
    // The head is made by the path's resolution (world_paths.rs) or by the evaluation's check.
    let Some(&m) = r.store.statics.get(&k) else {
        return Err(Box::from("no static head in the store (heads.rs `Store::statics`)"));
    };
    let thread_local = thread_local_of(r, k)?;
    Ok((record, StaticItem { thread_local, mutable: matches!(m, tc::Mutbl::Mut) }))
}

/// Static `k`'s kind when an `extern` block declares it: its head's mutability, nothing
/// evaluated (it has no initializer: the Reference, "External blocks", "Statics"). A foreign
/// `#[thread_local]` is refused by name: no foreign thread-local is built.
pub(crate) fn foreign_kind<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Result<StaticItem, Box<str>> {
    let Some(&m) = r.store.statics.get(&k) else {
        return Err(Box::from("no static head in the store (heads.rs `Store::statics`)"));
    };
    if thread_local_of(r, k)? {
        return Err(Box::from("a `#[thread_local]` static an `extern` block declares: no foreign thread-local is built (statics.rs `foreign_kind`)"));
    }
    Ok(StaticItem { thread_local: false, mutable: matches!(m, tc::Mutbl::Mut) })
}

/// Whether static `k`'s signature carries `#[thread_local]` (its attribute rows, read as
/// program.rs `symbols_of` reads a fn's).
fn thread_local_of<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Result<bool, Box<str>> {
    let refuse = |why: String| Box::<str>::from(format!("its attributes: {why}"));
    let f = r.store.fns.get(&k).ok_or_else(|| refuse(String::from("no head in the store")))?;
    let krate = r.names.crate_of(f.lib).ok_or_else(|| refuse(format!("no crate holds {:x}", f.lib)))?;
    let atoms = r.names.cfg_atoms(krate).ok_or_else(|| refuse(format!("crate {krate}'s cfg was not answered by the names source")))?;
    let edition = r.names.edition_of(krate).ok_or_else(|| refuse(format!("crate {krate}'s edition was not answered by the names source")))?;
    let sig = blaze_lower::lower_signature(f.text, edition, &mut CrateCfg(atoms), r.store.tys.alloc())
        .map_err(|_| refuse(String::from("its signature needs a `cfg` the crate does not answer")))?;
    let v = sig.view();
    for &n in v.rows(v.list(head::ATTRS)).map_err(|e| refuse(format!("{e:?}")))? {
        let a = v.attr(n).map_err(|e| refuse(format!("{e:?}")))?;
        if v.text_at(a.path).map_err(|e| refuse(format!("{e:?}")))?.trim() == "thread_local" {
            return Ok(true);
        }
    }
    Ok(false)
}
