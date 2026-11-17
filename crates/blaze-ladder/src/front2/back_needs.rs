//! What the code builder calls on its own account for what a reached body WRITES, queued by
//! the walk ([`super::reach::reach`]) on demand, so every fn the build asks for was checked and
//! tabled first. The builder's own list of always-possible calls (a panic, drop glue) is the
//! seam's [`crate::front::FnLang::ALL`], queued for every program; what is here is queued only
//! when a reached body needs it:
//!
//! - a `format_args!` placeholder: core's `rt::Argument` constructor for its trait
//!   (`new_display`, `new_debug`, ..: [`argument_new_name`], the builder's names), found as the
//!   program's `fmt_argument_new` finds it (an inherent `impl Argument` of `Argument`'s own
//!   module) after the walk has made that impl's header, which the program's lookup reads;
//! - `a[b]` on a type that is not an array or a slice (the Reference, "Array and slice
//!   indexing expressions": "For other types an index expression `a[b]` is equivalent to
//!   `*std::ops::Index::index(&a, b)`, or `*std::ops::IndexMut::index_mut(&mut a, b)` in a
//!   mutable place expression context"): the checker records no callee for it, so the walk
//!   asks the `index` / `index_mut` lang traits' impls at the program's shapes
//!   ([`BackNeeds::overloaded_index`], read by `reach.rs`'s `Instances`), as it asks `Drop`'s;
//! - a fn named as a VALUE, not called (`apply(f)`, `let g: fn() = f`): the builder calls it
//!   from a `Fn*` shim or through the pointer, which has no body of its own; the fn is queued
//!   (a trait's fn so named is decided at the instance, as a call on a parameter is).
//!
//! A constructor that cannot be found is filed in the census as failed with its cause (the
//! program's `left_out` in a survey, its refusal in a run), never skipped.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use blaze_typeck as tc;
use blaze_typeck::{Checked, ItemKey};

use super::body::Body;
use super::check::{Records, make_all};
use super::fill::want_of_item;
use super::fmt_template::{argument_new_name, inherent_on};
use super::keys::{MemberKind, Origin, checker_of};
use super::reach::Reached;
use super::world_items::Fill;

/// The formatting traits a placeholder can ask (blaze-typeck's `FmtTrait`), one slot each.
const FMT_TRAITS: usize = 10;

/// The most references peeled off an indexed base's type before its kind is read (rule 18):
/// a base is a place or a reference to one; autoderef through more is an overloaded `Deref`.
const MOST_REFS: u32 = 16;

const fn fmt_slot(t: tc::FmtTrait) -> usize {
    match t {
        tc::FmtTrait::Display => 0,
        tc::FmtTrait::Debug => 1,
        tc::FmtTrait::LowerHex => 2,
        tc::FmtTrait::UpperHex => 3,
        tc::FmtTrait::Octal => 4,
        tc::FmtTrait::Binary => 5,
        tc::FmtTrait::LowerExp => 6,
        tc::FmtTrait::UpperExp => 7,
        tc::FmtTrait::Pointer => 8,
        tc::FmtTrait::Count => 9,
    }
}

/// What the walk has learned the builder will call beyond what checked bodies name.
pub(super) struct BackNeeds {
    /// Which `rt::Argument` constructors were looked up already, by [`fmt_slot`].
    fmt_done: [bool; FMT_TRAITS],
    /// Whether a reached body indexes a type that is not an array or a slice: the `Index` and
    /// `IndexMut` impls are then asked at the program's shapes.
    pub(super) overloaded_index: bool,
    /// Whether a reached body holds an `.await`: the builder polls the held future by the
    /// selected `Future::poll` impl fn (the Reference, "Await expressions": "This pinned
    /// future is then polled by calling the `Future::poll` method"), which no checked body
    /// names (the checker records `IntoFuture::into_future` alone). The `Future` impls are
    /// then asked at the program's shapes, as `Index`'s are.
    pub(super) awaits: bool,
    /// Whether the `index` / `index_mut` trait fns were queued as declared already.
    index_declared: bool,
    /// Whether a reached body holds an overloaded dereference (an adjustment `Deref` the check
    /// recorded with a `Deref`/`DerefMut` method): the builder calls `Deref::deref` /
    /// `DerefMut::deref_mut` (blaze-build walk.rs `deref_call`, `Adts::deref_fn`), which no
    /// checked body names, so the trait fns are tabled as declared and their impls asked at
    /// the program's shapes, as `Index`'s are.
    pub(super) overloaded_deref: bool,
    /// Whether the `deref` / `deref_mut` trait fns were queued as declared already.
    deref_declared: bool,
}

impl BackNeeds {
    pub(super) const fn new() -> Self {
        BackNeeds {
            fmt_done: [false; FMT_TRAITS],
            overloaded_index: false,
            awaits: false,
            index_declared: false,
            overloaded_deref: false,
            deref_declared: false,
        }
    }

    /// Reads checked body `c` of `owner` (its lowering `body`, when the caller has it: a
    /// `format_args!` and an index are read from it) and queues what the builder will call for
    /// it; a constructor not found is pushed to `fns` as failed.
    pub(super) fn read(&mut self, r: &mut Records<'_, '_>, owner: ItemKey, body: Option<&Body<'_>>, c: &Checked<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>, fns: &mut Vec<(ItemKey, Reached), Global>) {
        // A fn named as a value: walked like a callee (a trait's fn at the instance).
        for d in c.expr_defs.column::<1>().iter() {
            let tc::Def::Fn(k) = *d else { continue };
            let of_trait = matches!(r.keys.origin(k), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !r.store.impls.contains_key(container));
            queue.push((k, of_trait.then_some(Reached::AtInstance)));
        }
        // An overloaded dereference (`*w`, autoderef through `Deref`): the builder names the
        // TRAIT's fn (`Adts::deref_fn`) and reads its ABI before resolving it to the impl's,
        // as for an overloaded index below. Once per program.
        if !self.overloaded_deref {
            self.overloaded_deref = c.adjusts.column::<1>().iter().any(|k| matches!(k, tc::AdjustKind::Deref { overloaded: Some(_) }));
        }
        if self.overloaded_deref && !self.deref_declared {
            self.deref_declared = true;
            for (lang, name) in [("deref", "deref"), ("deref_mut", "deref_mut")] {
                match lang_trait_fn(r, owner, lang, name) {
                    Ok(Some(k)) => queue.push((k, Some(Reached::AtInstance))),
                    // A program declaring no such lang trait: the checker records no
                    // overloaded dereference through it.
                    Ok(None) => {}
                    Err((at, why)) => fns.push((at, Reached::Failed { class: Box::from("deref lang fn"), detail: why })),
                }
            }
        }
        let Some(body) = body else { return };
        // The lowering's expressions and `format_args!` uses only; types are the check's
        // (`c.types`). A type read through this view is refused as past an empty table.
        let view = body.typeck.view(tc::PTys::EMPTY);
        let mut wanted: Vec<tc::FmtTrait, Global> = Vec::new_in(Global);
        for x in view.exprs.iter() {
            match *x {
                tc::Expr::FormatArgs { uses, .. } => {
                    let (start, len) = (uses.start as usize, uses.len as usize);
                    // A use list outside the column is the checker's `Malformed` refusal: this
                    // body did not check, so it is never read here.
                    let Some(list) = view.fmt_uses.get(start..start + len) else { continue };
                    for u in list {
                        let slot = fmt_slot(u.trait_);
                        if !self.fmt_done[slot] {
                            self.fmt_done[slot] = true;
                            wanted.push(u.trait_);
                        }
                    }
                }
                tc::Expr::Index { base, .. } if !self.overloaded_index => {
                    // A base the check gave no type is counted as overloaded: more is asked,
                    // never less.
                    self.overloaded_index = match c.expr_ty.get(base.0 as usize) {
                        Some(&t) => !builtin_indexed(c.types.view(), t),
                        None => true,
                    };
                }
                tc::Expr::Await(_) => self.awaits = true,
                _ => {}
            }
        }
        for t in wanted {
            match argument_new(r, owner, t) {
                Ok(k) => queue.push((k, None)),
                Err((at, why)) => fns.push((at.unwrap_or(owner), Reached::Failed { class: Box::from("fmt argument"), detail: why })),
            }
        }
        // The builder names the TRAIT's fn at the index (`Adts::index_fn`) and reads its ABI
        // before resolving it to the selected impl's: the trait's fn is tabled as declared,
        // as a trait's fn on a parameter is (`Reached::AtInstance`), and `Instances` then asks
        // its impls. Once per program.
        if self.overloaded_index && !self.index_declared {
            self.index_declared = true;
            for (lang, name) in [("index", "index"), ("index_mut", "index_mut")] {
                match lang_trait_fn(r, owner, lang, name) {
                    Ok(Some(k)) => queue.push((k, Some(Reached::AtInstance))),
                    // A program declaring no such lang trait: the checker refused its `a[b]`
                    // already (`MissingLangItem`), or it never writes one in a mutable place.
                    Ok(None) => {}
                    Err((at, why)) => fns.push((at, Reached::Failed { class: Box::from("index lang fn"), detail: why })),
                }
            }
        }
    }
}

/// The fn `name` of the lang trait `lang`, its head made first (a member key is filed only
/// when its trait's head is); `None` when the program declares no such lang trait. The error
/// carries the trait's key and the cause.
fn lang_trait_fn(r: &mut Records<'_, '_>, owner: ItemKey, lang: &str, name: &str) -> Result<Option<ItemKey>, (ItemKey, Box<str>)> {
    let Some(lib) = r.index.lang(lang) else { return Ok(None) };
    let tr = checker_of(lib);
    let refuse = |why: std::string::String| Err((tr, format!("the builder calls `{lang}` lang trait's `{name}` on its own (an overloaded index or dereference): {why}").into_boxed_str()));
    if let Err(clash) = r.keys.item(lib) {
        return refuse(format!("its key: {clash}"));
    }
    if r.keys.filed_member(tr, MemberKind::Fn, name).is_none() {
        let want = match want_of_item(r.names, lib) {
            Ok(w) => w,
            Err(e) => return refuse(format!("its head: {e:?}")),
        };
        let mut fills: Vec<Fill, Global> = Vec::new_in(Global);
        fills.push(Fill::Store(want));
        if let Err(e) = make_all(r, fills, owner) {
            return refuse(format!("making its head: {e}"));
        }
    }
    match r.keys.filed_member(tr, MemberKind::Fn, name) {
        Some(k) => Ok(Some(k)),
        None => refuse(std::string::String::from("the trait has no fn of that name")),
    }
}

/// Whether `t` (a reference to, at most [`MOST_REFS`] deep) is an array or a slice: indexed
/// by the language itself, not by an `Index` impl.
fn builtin_indexed(tys: tc::PTys<'_>, mut t: tc::PTy) -> bool {
    for _ in 0..MOST_REFS {
        match tys.node(t) {
            Ok(tc::PNode::Ref(_, inner)) => t = inner,
            Ok(tc::PNode::Array(..) | tc::PNode::Slice(_)) => return true,
            _ => return false,
        }
    }
    false
}

/// Core's `rt::Argument` constructor for `t`: the one fn of that name in the inherent impls of
/// `Argument` written in `Argument`'s own module (the program's `fmt_argument_new` finds it the
/// same way, so it finds what was made here), each impl's header made first: a member key is
/// filed only when its impl's header is. The error carries the key to file it under (the
/// `Argument` ADT's, when the program declares one) and the cause, naming the constructor.
fn argument_new(r: &mut Records<'_, '_>, owner: ItemKey, t: tc::FmtTrait) -> Result<ItemKey, (Option<ItemKey>, Box<str>)> {
    let name = argument_new_name(t);
    let index = r.index;
    let Some(adt) = index.lang("format_argument") else {
        return Err((None, format!("`format_args!` needs `rt::Argument::{name}`, and the program declares no `#[lang = \"format_argument\"]` type").into_boxed_str()));
    };
    let at = Some(checker_of(adt));
    let refuse = |why: std::string::String| Err((at, format!("`format_args!` needs `rt::Argument::{name}`: {why}").into_boxed_str()));
    let Some(module) = r.names.module_of(adt) else { return refuse(format!("`Argument` ({adt:?}) is in no module")) };
    let mut impls: Vec<LibKey, Global> = Vec::new_in(Global);
    impls.extend(index.impls.iter().filter(|i| i.module == module && inherent_on(i.header, "Argument")).map(|i| i.key));
    for &lib in impls.iter() {
        if r.store.impls.contains_key(&checker_of(lib)) {
            continue;
        }
        if let Err(clash) = r.keys.item(lib) {
            return refuse(format!("the key of `impl Argument` {lib:?}: {clash}"));
        }
        let want = match want_of_item(r.names, lib) {
            Ok(w) => w,
            Err(e) => return refuse(format!("the header of `impl Argument` {lib:?}: {e:?}")),
        };
        let mut fills: Vec<Fill, Global> = Vec::new_in(Global);
        fills.push(Fill::Store(want));
        if let Err(e) = make_all(r, fills, owner) {
            return refuse(format!("making the header of `impl Argument` {lib:?}: {e}"));
        }
    }
    let mut found = impls.iter().filter_map(|&lib| r.keys.filed_member(checker_of(lib), MemberKind::Fn, name));
    match (found.next(), found.next()) {
        (Some(k), None) => Ok(k),
        (None, _) => refuse(format!("none of the {} inherent `impl Argument` of its module has a fn of that name", impls.len())),
        (Some(_), Some(_)) => refuse(std::string::String::from("two inherent `impl Argument` of its module have a fn of that name")),
    }
}
