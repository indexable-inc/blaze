//! Constants in front2: the constant evaluator's world ([`ConstWorld`], blaze-consteval) over
//! front2's records. A constant's body is lowered by [`lower_fn`] (a member's by
//! [`lower_member`]) and checked by [`check_body`], the same loop as any fn's body, then
//! evaluated by `value_of` under a [`Settle`] work list (no recursion): a body the evaluation
//! lacks (`NeedKind::Body`) is checked first, a constant it reads (`NeedKind::Value`) evaluated
//! first; a cycle of constants is the evaluator's typed `Cycle` refusal naming each one.
//!
//! A called fn is what its signature and the names library say ([`FnKind`]): a tuple ctor from
//! the store's ctor rows; a fn `#[lang]` names as one of core's panic entries is `Panic`; a fn
//! marked `#[rustc_intrinsic]` is `Intrinsic(Which::named(name))` (an unknown name is
//! [`ConstFail::UnknownIntrinsic`] naming it); a `const fn` is interpreted from its checked
//! body; any other fn is `NotConst`. A trait's member is not selected here (the checker's
//! `resolve_instance` needs the records' mutable fill loop): [`ConstWorld::resolve`] answers
//! `None` for it and the evaluation refuses, naming it. A trait's associated CONSTANT at a
//! ground `Self` is selected (`impl_const`: the impl's definition, or the trait's default).
//! An ADT's layout (`size_of::<S>()`) reads the store's row of the ADT ([`LayoutFacts::adt`]);
//! a row not made yet is made when the layout asks for it ([`adt_row`]): never a guessed
//! layout.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_consteval::{
    ConstError, ConstRecord, ConstWorld, FnKind, Intrinsic, LayoutFacts, Layouts, Lits, Missing as LayoutMissing, NeedKind, Refusal, ReprAttrs, Settle, Typed,
    ValidRange,
};
use blaze_consteval::{Answer, Codes, Limits, Need, RecordKey, value_of};
use blaze_id::ItemKey as LibKey;
use blaze_ids::{ContentId, Hasher};
use blaze_lower::scoped::Prepares;
use blaze_lower::typeck::TypeckBody;
use blaze_lower::{View, head};
use blaze_own::Alloc;
use super::source::NamesSource;
use blaze_typeck as tc;
use blaze_typeck::{Checked, ExprId, Fetch, ItemKey, List, Missing, PatId, PortableTys, Range};

use super::body::{BodyError, lower_fn, lower_member};
use super::check::{CheckFail, Records, check_body};
use super::heads::{CrateCfg, Store};
use super::index::Index;
use super::keys::Keys;
use super::spell::Spelling;
use super::types::Why;

/// How deep constants may need each other (rule 18).
pub const MOST_NEEDS: u32 = 256;

/// The most evaluation steps (one `value_of` or one body checked) for one constant: past this
/// the needs do not settle (rule 18).
pub const MOST_STEPS: u32 = 4096;

/// Why a constant has no value.
#[derive(Debug)]
pub enum ConstFail {
    /// A body the evaluation needs could not be lowered.
    Body { item: ItemKey, error: BodyError },
    /// Its names could not be spelled for the check.
    Spelling { item: ItemKey, why: Why },
    /// A body the evaluation needs did not check.
    Check { item: ItemKey, error: CheckFail },
    /// The body checked with an error of the program (the checker's diagnostic code).
    Diag { item: ItemKey, code: Box<str> },
    /// A literal's written text could not be read back from the lowering.
    Literal { item: ItemKey },
    /// A fn marked `#[rustc_intrinsic]` whose name blaze's intrinsics table does not hold.
    UnknownIntrinsic { item: ItemKey, name: Box<str> },
    /// The evaluator refused (a cycle of constants included): its error names the constant.
    Eval(Box<str>),
    /// The needs did not settle in [`MOST_STEPS`]; `last` is the need being met then.
    Steps { item: ItemKey, last: Need },
    /// An array length in a head names `item` while another constant is being evaluated (or
    /// where the caller gave no evaluator): not evaluated nested; owed: one work list.
    Nested { item: ItemKey },
    /// An array length's constant `item` is not a `usize` value that fits in 64 bits.
    NotUsize { item: ItemKey },
    /// One fill named more than `most` array-length constants.
    TooManyLengths { item: ItemKey, most: u32 },
    /// The layout of a type constant `item` reads (`size_of::<S>()`) needs the row of ADT
    /// `adt`, which could not be made or cannot be laid out here: `why` names the cause.
    AdtRow { item: ItemKey, adt: ItemKey, why: Box<str> },
    /// The evaluation of `item` asked for the value of a TRAIT's constant `constant` with no
    /// arguments, so with no `Self` to select an impl by. A checked body records a constant
    /// path as `Def::Const(key)` alone (blaze-typeck `Checked::expr_defs`) and the evaluator
    /// instances it at `List::EMPTY` (blaze-consteval machine/code.rs, `Expr::Path`), never
    /// asking [`ConstWorld::resolve`]. Refused by name: evaluating the trait's own text would
    /// give the DEFAULT where an impl overrides it. Owed in those two crates.
    TraitConstNoSelf { item: ItemKey, constant: ItemKey },
    /// `<Self as Trait>::NAME` in a head ([`super::const_qpath`]): no impl of `trait_` for
    /// `self_ty` was selected for the trait's constant `constant` (none is made in the store,
    /// more than one is, or only the solver could choose: [`ConstsWorld::impl_const`]), or the
    /// one selected neither writes the constant nor has a default in the trait.
    NoImplConst { trait_: ItemKey, self_ty: tc::PNode, constant: ItemKey },
    /// `<Self as Trait>::NAME` in a head: the trait `trait_` declares no constant under the
    /// member key `constant`.
    NoTraitConst { trait_: ItemKey, constant: ItemKey },
    /// `<Self as Trait>::NAME` in a head: the type of the trait's constant `constant` is not
    /// `bool`, `char` or an integer, or its value is not one of at most 16 bytes.
    QPathType { constant: ItemKey },
    /// The qualified constant leaf under need key `need` is needed while it is being answered
    /// (an impl header of its own selection writes it).
    QPathCycle { need: ItemKey },
    /// More than `most` qualified constant leaves are being answered inside one another.
    QPathDepth { need: ItemKey, most: u32 },
    /// A key of the qualified constant leaf `need` (its trait, its type, the trait's
    /// constant) clashes in the front end's key map.
    QPathKey { need: ItemKey, clash: super::keys::KeyClash },
}

impl core::fmt::Display for ConstFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConstFail::Body { item, error } => write!(f, "constant {item:?}: a body it needs: {error}"),
            ConstFail::Spelling { item, why } => write!(f, "constant {item:?}: spelling its body's names: {why:?}"),
            ConstFail::Check { item, error } => write!(f, "constant {item:?}: a body it needs: {error}"),
            ConstFail::Diag { item, code } => write!(f, "constant {item:?}: the body of {item:?} has the error {code}"),
            ConstFail::Literal { item } => write!(f, "the body of {item:?}: a literal's text could not be read"),
            ConstFail::UnknownIntrinsic { item, name } => write!(f, "fn {item:?} is the intrinsic `{name}`, which blaze's intrinsics table does not hold"),
            ConstFail::Eval(e) => write!(f, "{e}"),
            ConstFail::Steps { item, last } => write!(f, "constant {item:?}: its needs did not settle in {MOST_STEPS} steps; the last: {last:?}"),
            ConstFail::Nested { item } => write!(f, "constant {item:?}: an array length needs it while another constant is evaluated"),
            ConstFail::NotUsize { item } => write!(f, "constant {item:?}: an array length's value is not a `usize` of 64 bits"),
            ConstFail::TooManyLengths { item, most } => write!(f, "constant {item:?}: one head named more than {most} array-length constants"),
            ConstFail::AdtRow { item, adt, why } => write!(f, "constant {item:?}: a layout it reads needs the row of ADT {adt:?}: {why}"),
            ConstFail::TraitConstNoSelf { item, constant } => write!(f, "constant {item:?}: the trait's constant {constant:?} is read with no `Self` (a checked body's constant path holds no arguments yet): no impl can be selected"),
            ConstFail::NoImplConst { trait_, self_ty, constant } => write!(f, "no impl of trait {trait_:?} for the type {self_ty:?} gives its constant {constant:?} (rule: one impl with no generics and that self type, or `impl<T> Tr for T`, is selected; none made, several, or one only the solver can choose is not)"),
            ConstFail::NoTraitConst { trait_, constant } => write!(f, "trait {trait_:?} declares no constant under the key {constant:?}"),
            ConstFail::QPathType { constant } => write!(f, "the trait's constant {constant:?}: a qualified path in a head has a value only at `bool`, `char` or an integer type of at most 16 bytes"),
            ConstFail::QPathCycle { need } => write!(f, "the qualified constant {need:?} needs itself: an impl header of its own selection writes it"),
            ConstFail::QPathDepth { need, most } => write!(f, "the qualified constant {need:?}: more than {most} are answered inside one another"),
            ConstFail::QPathKey { need, clash } => write!(f, "the qualified constant {need:?}: two origins under one checker key: {clash:?}"),
        }
    }
}

/// Makes the store's row of ADT `adt`, which the evaluator's layout asked for and did not find
/// (`size_of::<S>()` in constant `item`): by `Store::ensure(Want::Adt)`, the one maker of an
/// ADT's row (the run path's too: program.rs), so the evaluator and the code builder lay out
/// the same row. A row that cannot be made, or that is made and still gives no layout (a
/// narrowed valid range: [`ConstsWorld::adt`]), is the constant's refusal, naming the ADT. A
/// row whose head names a constant (`struct S([u8; N])`) is refused here by that name: the
/// evaluator is busy with `item` (owed: one work list over constants and heads).
fn adt_row<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,item: ItemKey, adt: ItemKey) -> Result<(), ConstFail> {
    let refuse = |why: String| ConstFail::AdtRow { item, adt, why: why.into_boxed_str() };
    let narrowed = "its valid range is narrowed (`rustc_layout_scalar_valid_range_*`), which the constant evaluator's layout does not read yet";
    match r.store.adts.get(&adt) {
        Some(a) if a.narrowed => return Err(refuse(String::from(narrowed))),
        Some(_) => return Err(refuse(String::from("its row is made and the layout still does not find it"))),
        None => {}
    }
    let lib = r.keys.library_in(r.names, adt).ok_or_else(|| refuse(String::from("it has no library key")))?;
    r.store.ensure(r.names, &mut *r.keys, super::heads::Want::Adt(lib)).map_err(|e| refuse(format!("{e}")))?;
    match r.store.adts.get(&adt) {
        Some(a) if a.narrowed => Err(refuse(String::from(narrowed))),
        Some(_) => Ok(()),
        None => Err(refuse(String::from("its head was made, but no row is filed under its key"))),
    }
}

/// A body's literals' texts, owned; lent as [`Lits`].
struct LitTable {
    text: String,
    exprs: (Vec<ExprId, Global>, Vec<Range, Global>),
    pats: (Vec<PatId, Global>, Vec<Range, Global>),
}

impl LitTable {
    /// Every literal of `body`, read where it was written (`View::text_at`), never at its site.
    fn of(view: &View<'_>, body: &TypeckBody<'_>, item: ItemKey) -> Result<LitTable, ConstFail> {
        let mut t = LitTable { text: String::new(), exprs: (Vec::new_in(Global), Vec::new_in(Global)), pats: (Vec::new_in(Global), Vec::new_in(Global)) };
        let put = |text: &mut String, s: &str| {
            let start = text.len() as u32;
            text.push_str(s);
            Range { start, end: text.len() as u32 }
        };
        for (i, e) in body.exprs.iter().enumerate() {
            if let tc::Expr::Lit(_) = e {
                let id = ExprId(i as u32);
                let s = view.expr_range(id).ok().and_then(|r| view.text_at(r).ok()).ok_or(ConstFail::Literal { item })?;
                let r = put(&mut t.text, s);
                t.exprs.0.push(id);
                t.exprs.1.push(r);
            }
        }
        for (i, p) in body.pats.iter().enumerate() {
            if let tc::Pat::Lit(_) | tc::Pat::NegLit(_) = p {
                let id = PatId(i as u32);
                let s = view.pat_range(id).ok().and_then(|r| view.text_at(r).ok()).ok_or(ConstFail::Literal { item })?;
                let r = put(&mut t.text, s);
                t.pats.0.push(id);
                t.pats.1.push(r);
            }
        }
        Ok(t)
    }

    fn lend(&self) -> Lits<'_> {
        Lits { text: &self.text, exprs: (&self.exprs.0, &self.exprs.1), pats: (&self.pats.0, &self.pats.1) }
    }
}

/// One checked body the evaluator reads: its types in its own table (`Body::tys`).
struct ConstBody<'n> {
    key: ItemKey,
    text: &'n str,
    hash: ContentId,
    typeck: TypeckBody<'static>,
    tys: PortableTys<'static>,
    checked: Checked<'static>,
    lits: LitTable,
}

/// The constants' state of one build: the bodies checked and the records made.
pub struct Consts<'n> {
    bodies: Vec<ConstBody<'n>, Global>,
    records: Vec<ConstRecord<Alloc<'static>>, Global>,
    unit: Option<(PortableTys<'static>, Layouts<Alloc<'static>>, Codes<Alloc<'static>>)>,
}

impl Default for Consts<'_> {
    fn default() -> Self {
        Consts { bodies: Vec::new_in(Global), records: Vec::new_in(Global), unit: None }
    }
}

/// The world one evaluation step reads: front2's records as they are now.
struct ConstsWorld<'w, 'n, S> {
    names: S,
    keys: &'w Keys,
    index: &'w Index<'n>,
    store: &'w Store<'n>,
    bodies: &'w [ConstBody<'n>],
    records: &'w [ConstRecord<Alloc<'static>>],
    /// The first `#[rustc_intrinsic]` fn met whose name the table does not hold.
    unknown: core::cell::RefCell<Option<(ItemKey, Box<str>)>>,
}

fn hash_text(text: &str) -> ContentId {
    let mut h = Hasher::new("blaze-ladder front2 constant text");
    h.update(text.as_bytes());
    h.finish()
}

/// The panic entries of core, by the names `#[lang]` gives them.
fn is_panic_lang(name: &str) -> bool {
    matches!(name, "panic" | "panic_fmt" | "const_panic_fmt" | "panic_display" | "panic_str_2015" | "panic_nounwind" | "panic_explicit" | "panic_bounds_check" | "panic_misaligned_pointer_dereference")
        || name.starts_with("panic_const_")
}

/// Library item `lib`'s name as written: its item text's own name (`Names::item_text`, as
/// program.rs `item_named` reads it). Not sliced from the outline's text by the entry's name
/// range: that range was not the text's (`size_of` came out as "onst_ev", seven bytes of
/// another item, and the intrinsic was refused as unknown).
fn written_name<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<&'n str> {
    names.name_of(lib)
}

impl<'n, S: NamesSource<'n>> ConstsWorld<'_, 'n, S> {
    /// A free fn's kind from its signature: `const`, `#[rustc_intrinsic]`, or neither.
    fn free_fn_kind(&self, item: ItemKey, lib: LibKey) -> FnKind {
        if self.index.lang_name_of(lib).is_some_and(is_panic_lang) {
            return FnKind::Panic;
        }
        let (Some(it), Some(krate)) = (self.names.text_of(lib), self.names.crate_of(lib)) else { return FnKind::NotConst };
        let (Some(atoms), Some(edition)) = (self.names.cfg_atoms(krate), self.names.edition_of(krate)) else { return FnKind::NotConst };
        let Ok(sig) = blaze_lower::lower_signature(it.text, edition, &mut CrateCfg(atoms), Alloc::global()) else { return FnKind::NotConst };
        let v = sig.view();
        let intrinsic = v.rows(v.list(head::ATTRS)).ok().is_some_and(|rows| {
            rows.iter().any(|&n| v.attr(n).ok().and_then(|a| v.text_at(a.path).ok()).is_some_and(|p| p == "rustc_intrinsic"))
        });
        if intrinsic {
            let Some(name) = written_name(self.names, lib) else { return FnKind::NotConst };
            return match Intrinsic::named(name) {
                Ok(w) => FnKind::Intrinsic(w),
                Err(_) => {
                    self.unknown.borrow_mut().get_or_insert((item, Box::from(name)));
                    FnKind::NotConst
                }
            };
        }
        if v.head[head::FLAGS] & blaze_lower::machine::sig::CONST != 0 { FnKind::Const } else { FnKind::NotConst }
    }

    /// `item` as an associated constant DECLARED by a trait (`Specifier::BITS`): the trait and
    /// the constant's name; `None` for any other item (an impl's constant, a free one).
    fn trait_const(&self, item: ItemKey) -> Option<(ItemKey, tc::Name)> {
        match self.keys.origin(item)? {
            super::keys::Origin::Member { container, kind: super::keys::MemberKind::Const, name } if self.store.trait_defs.contains_key(container) => Some((*container, tc::Name::of(name))),
            _ => None,
        }
    }

    /// The constant `name` of the impl of `trait_` for the ground type `args[0]`
    /// (`<B4 as Specifier>::BITS`; the Reference, "Associated constants": the value is the
    /// implementing type's definition). Selected here only where no solver is needed and the
    /// answer cannot depend on one: the trait takes no argument but `Self`, `Self` is a
    /// primitive (`bool`, `char`, an integer) or an ADT with no generic arguments, and EXACTLY
    /// ONE impl of the trait made in the store has no generics and that very self type and
    /// writes the constant or leaves it to the trait's default (`declared`, the trait's own
    /// constant item, then evaluated at `args`). By coherence (the Reference, "Trait
    /// implementation coherence": no two impls of a trait overlap) that impl is the one that
    /// applies. One more form is selected, for any ground `Self`: the blanket impl `impl<T> Tr
    /// for T` whose only clauses are `T: Sized` (see the loop). Anything else (another generic
    /// impl of `Self`, an impl whose head is not made) is `None`:
    /// the evaluation refuses, naming the item; never a guessed impl. The impls are scanned
    /// whole and the answer is the only match, so the map's order does not reach it.
    fn impl_const(&self, declared: ItemKey, trait_: ItemKey, name: tc::Name, args: List, ground: &mut PortableTys<'_>) -> Option<(ItemKey, List)> {
        let asked = {
            let g = ground.view();
            let list = g.list(args).ok()?;
            let [self_ty] = list else { return None };
            match g.node(*self_ty).ok()? {
                n @ (tc::PNode::Bool | tc::PNode::Char | tc::PNode::Int(_) | tc::PNode::Uint(_)) => (Some(n), None),
                tc::PNode::Adt(k, l) if g.list(l).ok()?.is_empty() => (None, Some(k)),
                // Any other ground type: only a blanket impl (below) can be selected for it.
                _ => (None, None),
            }
        };
        let s = self.store.tys.view();
        let sized = self.store.sized.map(super::keys::checker_of);
        let mut found: Option<ItemKey> = None;
        let mut blanket = false;
        for (ik, h) in self.store.impls.iter() {
            if h.trait_ != Some(trait_) {
                continue;
            }
            // A blanket impl `impl<T> Tr for T {}` (the pinned core, library/core/src/mem/
            // mod.rs: "impl<T> SizedTypeProperties for T {}", through which `size_of::<T>()`
            // is "<T as SizedTypeProperties>::SIZE"): it applies to every type its clauses
            // admit. Selected only when those are nothing but `T: Sized` (the implied bound;
            // the body that named the constant was checked, so `Self: Tr` holds and the type
            // is sized); with any other clause the choice needs the solver: `None`.
            if matches!(s.node(h.self_ty).ok()?, tc::PNode::Param(0)) {
                let clauses = &self.store.preds.get(ik)?.clauses;
                let only_sized = clauses.iter().all(|c| matches!(*c, tc::PClause::Trait { trait_: t, .. } if Some(t) == sized));
                if h.generics != 1 || !only_sized {
                    return None;
                }
                blanket = true;
            } else {
                if h.generics != 0 {
                    continue;
                }
                let same = match (s.node(h.self_ty).ok()?, asked) {
                    (tc::PNode::Adt(k, l), (None, Some(a))) => k == a && s.list(l).ok()?.is_empty(),
                    (n, (Some(a), None)) => n == a,
                    _ => false,
                };
                if !same {
                    continue;
                }
            }
            // The impl's own definition, or, where the impl writes none, the trait's default
            // (the Reference, "Associated Items", "Associated Constants", its example "Using
            // default values": `trait ConstantIdDefault { const ID: i32 = 1; }` with `impl
            // ConstantIdDefault for Struct {}` gives `assert_eq!(1, <Struct as
            // ConstantIdDefault>::ID)`). The default is the trait's own row (`declared`),
            // evaluated at the asked `Self` (`args`); a trait constant with no default that
            // the impl does not write is an error of the program (E0046): `None`.
            let c = match h.consts.iter().find(|(n, _)| *n == name) {
                Some(&(_, c)) => c,
                None if self.store.fns.get(&declared).is_some_and(|f| f.has_body) => declared,
                None => return None,
            };
            if found.replace(c).is_some() {
                return None;
            }
        }
        // The trait's default is generic over `Self`, and a blanket impl's own definition over
        // its one parameter, which is `Self`: both are evaluated at `args`. A non-generic
        // impl's definition has no parameters.
        found.map(|c| (c, if c == declared || blanket { args } else { List::EMPTY }))
    }

    /// Whether `item` is a member of a trait (its call is selected at the instance).
    fn of_trait(&self, item: ItemKey) -> bool {
        self.store.fns.get(&item).and_then(|f| f.container).is_some_and(|c| self.store.trait_defs.contains_key(&c))
    }
}

impl<'n, S: NamesSource<'n>> LayoutFacts for ConstsWorld<'_, 'n, S> {
    /// The store's row of the ADT (heads.rs `Store::adt`: the one maker, the same row the run
    /// path hands the code builder: program.rs `FrontEnd::adt`). `None` while the row is not
    /// made: the layout refuses naming the ADT, and [`Consts::value`] makes the row and asks
    /// again. `None` too for a row with a narrowed valid range
    /// (`rustc_layout_scalar_valid_range_*`), which `valid_range` below does not read: its
    /// niche would be wrong, so it has no layout here (`Consts::value` names that refusal).
    fn adt(&self, key: ItemKey) -> Option<tc::Adt<'_>> {
        let a = self.store.adts.get(&key).filter(|a| !a.narrowed)?;
        Some(tc::Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.store.tys.view() })
    }

    /// As the row read the ADT's `#[repr(..)]`.
    fn repr(&self, key: ItemKey) -> ReprAttrs {
        self.store.adts.get(&key).map_or(ReprAttrs::RUST, |a| a.repr)
    }

    /// From the ADT's row: every variant's value, explicit or implicit (heads.rs `discrs`),
    /// evaluated when the row was made, exactly as the run path answers (program.rs
    /// `FrontEnd::discriminant`). Never `Err`: that names a constant's record to make, and no
    /// record is owed here (the row holds the values; the layout asks only after `adt`).
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, LayoutMissing> {
        Ok(self.store.adts.get(&key).and_then(|a| a.discrs.get(variant as usize)).copied())
    }

    fn valid_range(&self, _: ItemKey) -> ValidRange {
        ValidRange::default()
    }
}

impl<'n, S: NamesSource<'n>> ConstWorld for ConstsWorld<'_, 'n, S> {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        let b = self.bodies.iter().find(|b| b.key == item).ok_or(Missing)?;
        Ok(Some(Typed::new(b.typeck.view(b.tys.view()), &b.checked, b.text, b.lits.lend(), b.hash)))
    }

    fn fn_kind(&self, item: ItemKey) -> FnKind {
        if let Some(c) = self.store.ctors.get(&item) {
            let variant = self.keys.library_in(self.names, item).and_then(|lib| crate::resolve::Paths { names: self.names }.variant_of(lib)).map_or(0, |(_, i)| i);
            return FnKind::Ctor { adt: c.adt, variant };
        }
        match self.keys.library_in(self.names, item) {
            Some(lib) => self.free_fn_kind(item, lib),
            None => match self.store.fns.get(&item) {
                Some(f) if f.has_body => {
                    let v = f.text;
                    let atoms_ok = self.names.crate_of(f.lib).and_then(|k| Some((self.names.edition_of(k)?, self.names.cfg_atoms(k)?)));
                    let Some((edition, atoms)) = atoms_ok else { return FnKind::NotConst };
                    match blaze_lower::lower_signature(v, edition, &mut CrateCfg(atoms), Alloc::global()) {
                        Ok(sig) if sig.view().head[head::FLAGS] & blaze_lower::machine::sig::CONST != 0 => FnKind::Const,
                        _ => FnKind::NotConst,
                    }
                }
                _ => FnKind::NotConst,
            },
        }
    }

    fn resolve(&self, item: ItemKey, args: List, ground: &mut PortableTys<'_>, _: Alloc<'_>) -> Option<(ItemKey, List)> {
        // A trait's constant first: it has a row in `Store::fns` whose container is the trait
        // (heads.rs `trait_members`), which `of_trait` would take for a trait's fn.
        if let Some((trait_, name)) = self.trait_const(item) {
            // A default selected for a ground `Self` is the trait's own item at the same
            // arguments: asked again, the answer is the same (the selection is idempotent).
            return self.impl_const(item, trait_, name, args, ground);
        }
        if self.of_trait(item) {
            return None;
        }
        Some((item, args))
    }

    /// From the item's key and its text: the same whether or not its body is checked yet (a
    /// compiled caller keeps the record keys it computed, so the id must never change).
    fn code_id(&self, item: ItemKey) -> ContentId {
        let mut h = Hasher::new("blaze-ladder front2 constant item");
        h.update(&item.0.to_le_bytes());
        let text = match self.keys.library_in(self.names, item) {
            Some(lib) => self.names.text_of(lib).map(|t| t.text),
            None => self.store.fns.get(&item).map(|f| f.text),
        };
        if let Some(t) = text {
            h.id(&hash_text(t));
        }
        h.finish()
    }

    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        self.records.iter().find(|r| r.key == key).ok_or(Missing)
    }

    fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
        None
    }
}

impl<'n> Consts<'n> {
    /// The record of constant `item` (a checker key), evaluating every constant it needs first.
    pub fn value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>,prepared: &Prepares<'n>, item: ItemKey) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        self.value_at(r, prepared, item, None)
    }

    /// The record of `<Self as Trait>::NAME` for the ground type `self_ty` (a primitive, or
    /// an ADT with no generic arguments), where `declared` is the TRAIT's constant
    /// (front2/const_qpath.rs: a qualified path leaf of a head's constant expression). The
    /// impl is selected by [`ConstsWorld::impl_const`] over the impls made in the store (the
    /// caller makes the candidates of (trait, type) first), and what it selects (the impl's
    /// definition, or the trait's default at `[self_ty]`) is evaluated as [`Consts::value`]
    /// evaluates any constant. No impl selected: [`ConstFail::NoImplConst`].
    pub fn trait_const_value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, declared: ItemKey, self_ty: tc::PNode) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        self.value_at(r, prepared, declared, Some(self_ty))
    }

    /// The root of an evaluation that starts at the trait's constant `declared` for the
    /// ground `self_ty`: the selected item and its arguments (in `ground`).
    fn select_root<S: NamesSource<'n>>(&self, r: &Records<'_, 'n, S>, declared: ItemKey, self_ty: tc::PNode, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ConstFail> {
        let world = ConstsWorld { names: r.names, keys: &*r.keys, index: r.index, store: &*r.store, bodies: &self.bodies, records: &self.records, unknown: core::cell::RefCell::new(None) };
        let Some((trait_, name)) = world.trait_const(declared) else {
            return Err(ConstFail::TraitConstNoSelf { item: declared, constant: declared });
        };
        let full = |_| ConstFail::Eval(Box::from("the constant evaluator's type table is full"));
        let self_arg = ground.intern(self_ty).map_err(full)?;
        let args = ground.intern_list(&[self_arg]).map_err(full)?;
        world.impl_const(declared, trait_, name, args, ground).ok_or(ConstFail::NoImplConst { trait_, self_ty, constant: declared })
    }

    /// [`Consts::value`] (`self_ty` is `None`: `item` at no arguments), or
    /// [`Consts::trait_const_value`] (`item` is a trait's constant, selected for `self_ty`).
    fn value_at<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, self_ty: Option<tc::PNode>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        let alloc = Alloc::global();
        let (mut ground, mut layout, mut codes) = self.unit.take().unwrap_or_else(|| (PortableTys::new_in(alloc), Layouts::new_in(alloc), Codes::new_in(alloc)));
        let root = match self_ty {
            None => Ok((item, List::EMPTY)),
            Some(node) => self.select_root(&*r, item, node, &mut ground),
        };
        let (item, root_args) = match root {
            Ok(root) => root,
            Err(e) => {
                self.unit = Some((ground, layout, codes));
                return Err(e);
            }
        };
        let mut settle = Settle::new(Need { kind: NeedKind::Value, item, args: root_args }, MOST_NEEDS, alloc);
        let mut last = None;
        let mut out = Ok(());
        let mut steps = 0u32;
        while let Some(n) = settle.next() {
            steps += 1;
            if steps > MOST_STEPS {
                out = Err(ConstFail::Steps { item, last: n });
                break;
            }
            // A trait's constant has `Self` among its parameters: a need for it with no
            // arguments has no impl to select ([`ConstFail::TraitConstNoSelf`]).
            let of_a_trait = matches!(
                r.keys.origin(n.item),
                Some(super::keys::Origin::Member { container, kind: super::keys::MemberKind::Const, .. }) if r.store.trait_defs.contains_key(container)
            );
            if of_a_trait && ground.view().list(n.args).map_or(true, |l| l.is_empty()) {
                out = Err(ConstFail::TraitConstNoSelf { item, constant: n.item });
                break;
            }
            if !self.bodies.iter().any(|b| b.key == n.item) && (n.kind == NeedKind::Body || n.item == item) {
                if let Err(e) = self.check_one(r, prepared, n.item) {
                    out = Err(e);
                    break;
                }
                if n.kind == NeedKind::Body {
                    settle.done();
                }
                continue;
            }
            let world = ConstsWorld { names: r.names, keys: r.keys, index: r.index, store: r.store, bodies: &self.bodies, records: &self.records, unknown: core::cell::RefCell::new(None) };
            let answer = value_of(&world, &mut layout, &mut codes, n.item, n.args, &mut ground, Limits::DEFAULT, alloc);
            let unknown = world.unknown.into_inner();
            let step = match answer {
                Ok(Answer::Done(rec)) => {
                    let key = rec.key;
                    if !self.records.iter().any(|x| x.key == key) {
                        self.records.push(rec);
                    }
                    last = Some(key);
                    settle.done();
                    Ok(())
                }
                Ok(Answer::Need(m)) => settle.need(m).map_err(|e: ConstError<'_>| ConstFail::Eval(format!("{e}").into_boxed_str())),
                Err(e) => {
                    // The layout met an ADT whose row the store has not made (`size_of::<S>()`:
                    // blaze-consteval's layout.rs turns the layout's `NoAdt` into this typed
                    // refusal): the row is made now and the same need is evaluated again
                    // (`settle.next()` gives it again; a refusal caches no layout). Each ADT
                    // is made at most once (`adt_row` refuses one already made), and every
                    // round counts against `MOST_STEPS`.
                    let no_adt = match e.refusal {
                        Refusal::Missing { what: "ADT definition", item: adt } => Some(adt),
                        _ => None,
                    };
                    let text = format!("{e}").into_boxed_str();
                    match (unknown, no_adt) {
                        (Some((item, name)), _) => Err(ConstFail::UnknownIntrinsic { item, name }),
                        (None, Some(adt)) => match adt_row(r, n.item, adt) {
                            Ok(()) => continue,
                            Err(e) => Err(e),
                        },
                        (None, None) => Err(ConstFail::Eval(text)),
                    }
                }
            };
            if let Err(e) = step {
                out = Err(e);
                break;
            }
        }
        self.unit = Some((ground, layout, codes));
        out?;
        let key = last.ok_or(ConstFail::Eval(format!("constant {item:?}: the evaluation settled with no record").into_boxed_str()))?;
        self.records.iter().find(|x| x.key == key).ok_or(ConstFail::Eval(format!("constant {item:?}: its record was not kept").into_boxed_str()))
    }

    /// Lowers and checks the body of `item` (a constant, a static or a `const fn`) for the
    /// evaluator.
    fn check_one<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>,prepared: &Prepares<'n>, item: ItemKey) -> Result<(), ConstFail> {
        let lowered = match r.keys.library_in(r.names, item) {
            Some(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
            None => lower_member(r.names, prepared, r.keys, r.store, item),
        }
        .map_err(|error| ConstFail::Body { item, error })?;
        let (checked, lits) = {
            let view = lowered.lowered.view();
            let spell = Spelling::of(&view).map_err(|why| ConstFail::Spelling { item, why })?;
            let scope = lowered.scope(&spell);
            let checked = check_body(r, &lowered, &scope).map_err(|error| ConstFail::Check { item, error })?;
            if let Some(d) = checked.diags.first() {
                return Err(ConstFail::Diag { item, code: format!("{:?}", d.code).into_boxed_str() });
            }
            let lits = LitTable::of(&view, &lowered.typeck, item)?;
            (checked, lits)
        };
        let text = match r.keys.library_in(r.names, item) {
            Some(lib) => r.names.text_of(lib).map(|t| t.text),
            None => r.store.fns.get(&item).map(|f| f.text),
        }
        .ok_or(ConstFail::Body { item, error: BodyError::NoHead(item) })?;
        self.bodies.push(ConstBody { key: item, text, hash: hash_text(text), typeck: lowered.typeck, tys: lowered.tys, checked, lits });
        Ok(())
    }
}

/// A constant's value as an unsigned integer of at most 16 bytes, from its record's root.
pub fn int_of(record: &ConstRecord<Alloc<'_>>) -> Option<u128> {
    let root = record.data.allocs.first()?;
    if !root.relocs.is_empty() || root.bytes.len() > 16 {
        return None;
    }
    let mut v = 0u128;
    for (k, &b) in root.bytes.iter().enumerate() {
        v |= u128::from(b) << (8 * k);
    }
    Some(v)
}
