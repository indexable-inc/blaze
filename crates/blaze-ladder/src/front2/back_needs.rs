//! What the code builder calls on its own account for what a reached body WRITES, queued by
//! the walk ([`super::reach::reach`]) on demand, so every fn the build asks for was checked and
//! tabled first. The builder's own list of always-possible calls (a panic, drop glue) is the
//! seam's [`crate::front::FnLang::ALL`], queued for every program; what is here is queued only
//! when a reached body needs it:
//!
//! - a `format_args!` placeholder: core's `rt::Argument` constructor for its trait
//!   (`new_display`, `new_debug`, ..), asked by fmt_template.rs `argument_new`, the one lookup
//!   both programs make (the impl index's inherent question), whose answer the program's
//!   `fmt_argument_new` reads;
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

use blaze_typeck as tc;
use blaze_typeck::{Checked, ItemKey};

use super::body::Body;
use super::check::{Records, make_all};
use super::fill::want_of_item;
use super::fmt_template::{argument_new, placeholder_traits};
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
    /// Whether the `Fn*` traits' fns were queued as declared already: a closure called
    /// directly (`f()`, the Reference, "Call expressions": `Fn::call(&f, ())` for an `Fn`
    /// closure) is built as its kind's method (blaze-build closure.rs `body_instance`,
    /// `Adts::fn_trait_method`), which no checked body names unless a bound did.
    fn_traits_declared: bool,
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
            fn_traits_declared: false,
        }
    }

    /// Reads checked body `c` of `owner` (its lowering `body`, when the caller has it: a
    /// `format_args!` and an index are read from it) and queues what the builder will call for
    /// it; a constructor not found is pushed to `fns` as failed.
    pub(super) fn read(&mut self, r: &mut Records<'_, '_>, owner: ItemKey, body: Option<&Body<'_>>, c: &Checked<'_>, queue: &mut Vec<(ItemKey, Option<Reached>), Global>, fns: &mut Vec<(ItemKey, Reached), Global>) {
        // A fn named as a value: walked like a callee (a trait's fn at the instance).
        for d in c.expr_defs.column::<1>().iter() {
            match *d {
                tc::Def::Fn(k) => {
                    let of_trait = matches!(r.keys.origin(k), Some(Origin::Member { container, kind: MemberKind::Fn, .. }) if !r.store.impls.contains_key(container));
                    queue.push((k, of_trait.then_some(Reached::AtInstance)));
                }
                // A tuple struct's or variant's ctor named as a value (`let f = E::A;`,
                // `.map(Some)`, a fn pointer `= W`): the language's fn (the Reference,
                // "Structs", "Enumerations"), tabled as a ctor as a call of it is
                // (`callee_target`); the builder builds it (blaze-build ctor_fn.rs). A
                // written call `W(..)` names it here too: the walk tables each key once.
                tc::Def::Ctor { adt, variant, kind: tc::CtorKind::Tuple } => {
                    let key = r.store.adts.get(&adt).and_then(|a| a.variants.get(variant as usize)).map(|v| v.key);
                    if let Some(k) = key
                        && r.store.ctors.contains_key(&k)
                    {
                        queue.push((k, Some(Reached::Ctor)));
                    }
                }
                _ => {}
            }
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
                    Err((at, why)) => fns.push((at, Reached::Failed { class: Box::from("deref lang fn"), detail: why, refusal: None })),
                }
            }
        }
        // A body that makes a closure: the builder calls it by its kind's `Fn*` method, and an
        // entry of a weaker one, by name (`Adts::fn_trait_method`). Once per program.
        if !self.fn_traits_declared && !c.closures.is_empty() {
            self.fn_traits_declared = true;
            for (lang, name) in [("fn", "call"), ("fn_mut", "call_mut"), ("fn_once", "call_once")] {
                match lang_trait_fn(r, owner, lang, name) {
                    Ok(Some(k)) => queue.push((k, Some(Reached::AtInstance))),
                    // A program declaring no such lang trait has no closure: the checker
                    // refused it already (`MissingLangItem`).
                    Ok(None) => {}
                    Err((at, why)) => fns.push((at, Reached::Failed { class: Box::from("fn trait lang fn"), detail: why, refusal: None })),
                }
            }
        }
        let Some(body) = body else { return };
        // The lowering's expressions and `format_args!` uses only; types are the check's
        // (`c.types`). A type read through this view is refused as past an empty table.
        let view = body.typeck.view(tc::PTys::EMPTY);
        let mut wanted = placeholder_traits(body);
        wanted.retain(|&t| !core::mem::replace(&mut self.fmt_done[fmt_slot(t)], true));
        for x in view.exprs.iter() {
            match *x {
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
                // Filed under the `Argument` type's key when the program declares one.
                Err(why) => {
                    let at = r.index.lang("format_argument").map_or(owner, checker_of);
                    fns.push((at, Reached::Failed { class: Box::from("fmt argument"), detail: format!("{why}").into_boxed_str(), refusal: None }));
                }
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
                    Err((at, why)) => fns.push((at, Reached::Failed { class: Box::from("index lang fn"), detail: why, refusal: None })),
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
