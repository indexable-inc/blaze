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
//! body; any other fn is `NotConst`. A trait's fn is not selected here: [`ConstWorld::resolve`]
//! answers `None` for it and the evaluation refuses, naming it. A trait's associated CONSTANT
//! at ground arguments is selected by the checker's solver (const_select.rs, through
//! `blaze_typeck::resolve_instance`: the impl's definition at the impl's arguments, or the
//! trait's default); what that selection needs made is made and the step evaluated again.
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
    /// `shown`: what `item` names (check.rs `key_shown`: rule 19), beside the key.
    Nested { item: ItemKey, shown: Box<str, Global> },
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
    /// A trait's constant `constant` of `trait_` at `Self` = `self_ty` (a qualified path in a
    /// head, [`super::const_qpath`], or a path an evaluation reads): no impl of the trait
    /// holds for that type by the checker's solver (front2/const_select.rs).
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
    /// Expression `block` of `owner`'s body, as this evaluator lowered it, is not the
    /// `const { .. }` block an instance's build asked the value of (const_at.rs).
    NotConstBlock { owner: ItemKey, block: ExprId },
    /// A type `item`'s evaluation lays out or grounds (a field's type: `Atomic<T>`'s
    /// `UnsafeCell<T::Storage>`) holds a projection the checker's solver did not normalize:
    /// that type has no ground form (front2/const_project.rs).
    Projection { item: ItemKey, refused: super::const_project::ProjectionRefused },
    /// A trait's constant the solver selected no definition for, for a reason other than no
    /// impl (two impls hold, a question the world has no answer to, a trait declared in a
    /// body): front2/const_select.rs.
    SelectConst { refused: super::const_select::ConstSelectRefused },
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
            ConstFail::Nested { item, shown } => write!(f, "constant {item:?} ({shown}): an array length needs it while another constant is evaluated"),
            ConstFail::NotUsize { item } => write!(f, "constant {item:?}: an array length's value is not a `usize` of 64 bits"),
            ConstFail::TooManyLengths { item, most } => write!(f, "constant {item:?}: one head named more than {most} array-length constants"),
            ConstFail::AdtRow { item, adt, why } => write!(f, "constant {item:?}: a layout it reads needs the row of ADT {adt:?}: {why}"),
            ConstFail::TraitConstNoSelf { item, constant } => write!(f, "constant {item:?}: the trait's constant {constant:?} is read with no `Self` (a checked body's constant path holds no arguments yet): no impl can be selected"),
            ConstFail::NoImplConst { trait_, self_ty, constant } => write!(f, "no impl of trait {trait_:?} holds for the type {self_ty:?} to give its constant {constant:?} (rule: the value is the definition in the impl the solver selects for the trait reference, or the trait's default where that impl writes none)"),
            ConstFail::NoTraitConst { trait_, constant } => write!(f, "trait {trait_:?} declares no constant under the key {constant:?}"),
            ConstFail::QPathType { constant } => write!(f, "the trait's constant {constant:?}: a qualified path in a head has a value only at `bool`, `char` or an integer type of at most 16 bytes"),
            ConstFail::QPathCycle { need } => write!(f, "the qualified constant {need:?} needs itself: an impl header of its own selection writes it"),
            ConstFail::QPathDepth { need, most } => write!(f, "the qualified constant {need:?}: more than {most} are answered inside one another"),
            ConstFail::QPathKey { need, clash } => write!(f, "the qualified constant {need:?}: two origins under one checker key: {clash:?}"),
            ConstFail::NotConstBlock { owner, block } => write!(f, "fn {owner:?}: expression {} of its body as the evaluator lowered it is not a `const {{ .. }}` block", block.0),
            ConstFail::Projection { item, refused } => write!(f, "constant {item:?}: a type it lays out: {refused}"),
            ConstFail::SelectConst { refused } => write!(f, "{refused}"),
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
    tys: tc::OwnTys<'static>,
    checked: Checked<'static>,
    lits: LitTable,
    /// `Some(block)`: the body of a `const { .. }` block (front2/const_at.rs): its owner's body
    /// read from `block` as its root, with no parameters; `key` is the block's own key.
    root: Option<ExprId>,
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
    /// The last trait's constant [`ConstWorld::resolve`] asked the solver for and had refused
    /// (const_select.rs): the step's typed refusal when the evaluation stops there.
    selected: core::cell::Cell<Option<super::const_select::ConstSelectRefused>>,
    /// What the step's projections asked of the records ([`LayoutFacts::normalize`] below):
    /// [`Consts::value`] makes its fills and asks again, or refuses naming the projection.
    project: super::const_project::ProjectAsk<'w>,
}

/// An item's code identity from its key and its text: the ONE identity of a fn's code, which
/// the evaluator's fn pointers (their instance keys, in a static's record) and the run's
/// instance keys (`FrontEnd::code_of`) both take, so a static's fn word names the slot the run
/// built (rule 30).
pub(crate) fn item_code_id(item: ItemKey, text: Option<&str>) -> ContentId {
    let mut h = Hasher::new("blaze-ladder front2 constant item");
    h.update(&item.0.to_le_bytes());
    if let Some(t) = text {
        h.id(&hash_text(t));
    }
    h.finish()
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
    fn free_fn_kind(&self, lib: LibKey) -> FnKind {
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
            // An unknown name is its own kind: the evaluator refuses a call of it as
            // `Refusal::UnknownIntrinsic`, which compiled code keeps like any final refusal.
            return match Intrinsic::named(name) {
                Ok(w) => FnKind::Intrinsic(w),
                Err(_) => FnKind::UnknownIntrinsic,
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

    /// By the checker's solver over the store (front2/const_project.rs): a field's type
    /// grounded for a static's or constant's layout (`Atomic<usize>`'s
    /// `UnsafeCell<<usize as AtomicPrimitive>::Storage>`) normalizes exactly as the run path's
    /// layout does (program.rs `FrontEnd::normalize`). What it could not answer is kept in
    /// `project` for [`Consts::value`].
    fn normalize(&self, trait_: ItemKey, name: tc::Name, args: &[tc::PTy], ground: &mut PortableTys<'_>) -> Result<tc::PTy, blaze_build::BuildError> {
        self.project.normalize(self.names, self.keys, self.index, self.store, trait_, name, args, ground)
    }
}

impl<'n, S: NamesSource<'n>> ConstWorld for ConstsWorld<'_, 'n, S> {
    fn body(&self, item: ItemKey) -> Fetch<Option<Typed<'_>>> {
        let b = self.bodies.iter().find(|b| b.key == item).ok_or(Missing)?;
        // `store.tys` is a table of its own and only grows, so the body's base is in it whole
        // (`OwnTys::view` refuses only a shorter base or an overlay): an invariant, not a gap.
        let tys = b.tys.view(&self.store.tys).expect("a body's types over the records' table it was lowered over");
        let mut view = b.typeck.view(tys);
        // A const block's body: its owner's, evaluated from the block (const_at.rs).
        if let Some(root) = b.root {
            view.root = root;
            view.params = &[];
        }
        Ok(Some(Typed::new(view, &b.checked, b.text, b.lits.lend(), b.hash)))
    }

    fn fn_kind(&self, item: ItemKey) -> FnKind {
        if let Some(c) = self.store.ctors.get(&item) {
            let variant = self.keys.library_in(self.names, item).and_then(|lib| crate::resolve::Paths { names: self.names }.variant_of(lib)).map_or(0, |(_, i)| i);
            return FnKind::Ctor { adt: c.adt, variant };
        }
        match self.keys.library_in(self.names, item) {
            Some(lib) => self.free_fn_kind(lib),
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

    fn resolve(&self, item: ItemKey, args: List, ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Option<(ItemKey, List)> {
        // A trait's constant first: it has a row in `Store::fns` whose container is the trait
        // (heads.rs `trait_members`), which `of_trait` would take for a trait's fn. Selected by
        // the checker's solver (const_select.rs); what it asks the records to make is noted
        // with the projections' (`project`), its refusal kept in `selected`: either way `None`
        // here, and [`Consts::value`] makes the fills and evaluates again, or refuses by name.
        // A default selected is the trait's own item at the same arguments: asked again, the
        // answer is the same (the selection is idempotent).
        if let Some((trait_, _)) = self.trait_const(item) {
            return match super::const_select::select_once(self.names, self.keys, self.index, self.store, self.project.buckets(), item, trait_, args, ground, alloc) {
                super::const_select::Selection::Selected { item, args } => Some((item, args)),
                super::const_select::Selection::Asked { fills, gaps } => {
                    self.project.note(fills, gaps);
                    None
                }
                super::const_select::Selection::Refused(refused) => {
                    self.selected.set(Some(refused));
                    None
                }
            };
        }
        if self.of_trait(item) {
            return None;
        }
        Some((item, args))
    }

    /// From the item's key and its text: the same whether or not its body is checked yet (a
    /// compiled caller keeps the record keys it computed, so the id must never change).
    fn code_id(&self, item: ItemKey) -> ContentId {
        let text = match (self.store.locals.head(item), self.keys.library_in(self.names, item)) {
            // An item declared in a body: its kept text, there from the lowering of its
            // enclosing body on (its key is derived from that body's key and its name, not
            // from its own text, so the text must be in the id: rule 21).
            (Some(local), _) => Some(local.text),
            (None, Some(lib)) => self.names.text_of(lib).map(|t| t.text),
            (None, None) => self.store.fns.get(&item).map(|f| f.text),
        };
        item_code_id(item, text)
    }

    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        self.records.iter().find(|r| r.key == key).ok_or(Missing)
    }

    fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
        None
    }
}

/// Where an evaluation starts ([`Consts::value_at`]): `item` at no arguments; a trait's
/// constant selected for a ground `Self` (a head's qualified path); or `item` at ground
/// arguments already in the evaluator's own table (an instance's need: const_at.rs).
#[derive(Clone, Copy)]
enum Root {
    Plain,
    SelfTy(tc::PNode),
    Args(List),
}

impl<'n> Consts<'n> {
    /// The record of constant `item` (a checker key), evaluating every constant it needs first.
    pub fn value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>,prepared: &Prepares<'n>, item: ItemKey) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        self.value_at(r, prepared, item, Root::Plain)
    }

    /// The record of `<Self as Trait>::NAME` for the ground type `self_ty` (a primitive, or
    /// an ADT with no generic arguments), where `declared` is the TRAIT's constant
    /// (front2/const_qpath.rs: a qualified path leaf of a head's constant expression). The
    /// impl is selected by the checker's solver (const_select.rs `select_made`, which makes
    /// the candidates of (trait, type) it asks for), and what it selects (the impl's
    /// definition, or the trait's default at `[self_ty]`) is evaluated as [`Consts::value`]
    /// evaluates any constant. No impl selected: [`ConstFail::NoImplConst`].
    pub fn trait_const_value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, declared: ItemKey, self_ty: tc::PNode) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        self.value_at(r, prepared, declared, Root::SelfTy(self_ty))
    }

    /// The record of constant `item` at the ground arguments `args` (types of `from`, the
    /// build's ground table: front2/const_at.rs): `[Self, trait args..]` for a trait's constant
    /// as declared, whose impl's definition (or the trait's default) is selected at that `Self`
    /// (const_select.rs `select_made`, the checker's solver, making the records it asks for);
    /// the impl's arguments for an inherent impl's. The arguments are copied into this
    /// evaluator's own table first; every constant the value needs is evaluated first by the
    /// same work list as [`Consts::value`]'s (a cycle is the evaluator's typed refusal).
    pub fn value_with_args<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, from: tc::PTys<'_>, args: &[tc::PTy], scratch: Alloc<'_>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        let list = self.args_in_unit(item, from, args, scratch)?;
        self.value_at(r, prepared, item, Root::Args(list))
    }

    /// The record of `const { .. }` block `block` of `owner`'s body at the owner's ground
    /// arguments `args` (types of `from`): the owner's body is lowered and checked for this
    /// evaluator once per block, under the block's key `key` (const_at.rs `block_key`), and
    /// read from the block as its root ([`ConstBody::root`]); then evaluated as any constant.
    #[allow(clippy::too_many_arguments)]
    pub fn block_value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, owner: ItemKey, block: ExprId, key: ItemKey, from: tc::PTys<'_>, args: &[tc::PTy], scratch: Alloc<'_>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        if !self.bodies.iter().any(|b| b.key == key) {
            self.check_as(r, prepared, owner, key, Some(block))?;
        }
        let list = self.args_in_unit(key, from, args, scratch)?;
        self.value_at(r, prepared, key, Root::Args(list))
    }

    /// `args` (types of `from`, all ground) copied into this evaluator's own type table, as
    /// one list of it.
    fn args_in_unit(&mut self, item: ItemKey, from: tc::PTys<'_>, args: &[tc::PTy], scratch: Alloc<'_>) -> Result<List, ConstFail> {
        let alloc = Alloc::global();
        let (mut ground, layout, codes) = self.unit.take().unwrap_or_else(|| (PortableTys::new_in(alloc), Layouts::new_in(alloc), Codes::new_in(alloc)));
        let list = super::const_at::copy_ground(from, args, &mut ground, scratch).map_err(|why| ConstFail::Eval(format!("constant {item:?}: its arguments: {why}").into_boxed_str()));
        self.unit = Some((ground, layout, codes));
        list
    }

    /// The root of an evaluation that starts at constant `item` at ground `args` (a list of
    /// `ground`): a trait's constant is selected by the checker's solver with the records'
    /// fill loop (const_select.rs `select_made`): the impl's definition at the impl's
    /// arguments, or the trait's default at `args`. Any other constant is its own root.
    fn select_args<S: NamesSource<'n>>(&self, r: &mut Records<'_, 'n, S>, item: ItemKey, args: List, ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<(ItemKey, List), ConstFail> {
        let Some(trait_) = self.trait_of_const(&*r, item) else {
            return Ok((item, args));
        };
        super::const_select::select_made(r, item, trait_, args, ground, alloc)
    }

    /// The root of an evaluation that starts at the trait's constant `declared` for the
    /// ground `self_ty`: `[self_ty]` as the arguments, selected as [`Consts::select_args`]
    /// selects them.
    fn select_root<S: NamesSource<'n>>(&self, r: &mut Records<'_, 'n, S>, declared: ItemKey, self_ty: tc::PNode, ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<(ItemKey, List), ConstFail> {
        let Some(trait_) = self.trait_of_const(&*r, declared) else {
            return Err(ConstFail::TraitConstNoSelf { item: declared, constant: declared });
        };
        let full = |_| ConstFail::Eval(Box::from("the constant evaluator's type table is full"));
        let self_arg = ground.intern(self_ty).map_err(full)?;
        let args = ground.intern_list(&[self_arg]).map_err(full)?;
        super::const_select::select_made(r, declared, trait_, args, ground, alloc)
    }

    /// The trait that declares `item`, when `item` is a trait's associated constant
    /// ([`ConstsWorld::trait_const`], over the records as they are now).
    fn trait_of_const<S: NamesSource<'n>>(&self, r: &Records<'_, 'n, S>, item: ItemKey) -> Option<ItemKey> {
        let world = ConstsWorld { names: r.names, keys: &*r.keys, index: r.index, store: &*r.store, bodies: &self.bodies, records: &self.records, selected: core::cell::Cell::new(None), project: super::const_project::ProjectAsk::new(r.buckets.as_ref()) };
        world.trait_const(item).map(|(trait_, _)| trait_)
    }

    /// [`Consts::value`] (`self_ty` is `None`: `item` at no arguments), or
    /// [`Consts::trait_const_value`] (`item` is a trait's constant, selected for `self_ty`).
    fn value_at<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, root: Root) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        // Counted in and out (`Maps::evaluating`): a head that needs a constant while this
        // runs finds the evaluator BUSY (`ConstFail::Nested`, which the work list below
        // reads), where a caller with no evaluator at all has the head refused by name
        // (check.rs `make`).
        r.store.maps.evaluating.set(r.store.maps.evaluating.get().saturating_add(1));
        let out = self.value_listed(r, prepared, item, root);
        r.store.maps.evaluating.set(r.store.maps.evaluating.get().saturating_sub(1));
        out
    }

    /// [`Consts::value_at`]'s work: the root's evaluation, with every constant it needs first.
    fn value_listed<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, root: Root) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        // A work list of the constants to evaluate FIRST, innermost last: a head made while a
        // body of the evaluation was checked names one (front2/const_open.rs `needed_first`:
        // `[u8; size_of::<Self>()]` on a method of `u8`, met when `u8::MAX` is checked). The
        // failed evaluation is dropped whole, the constant evaluated and filed
        // ([`Consts::first`]), and the failed one evaluated again: never a nested evaluation.
        // One already on the list needs itself: the refusal it failed with is returned.
        let mut firsts: Vec<LibKey, Global> = Vec::new_in(Global);
        let mut rounds = 0u32;
        let key = loop {
            let step = match firsts.last().copied() {
                None => self.value_key(r, prepared, item, root).map(Some),
                Some(c) => self.first(r, prepared, c).map(|()| None),
            };
            match step {
                Ok(Some(key)) => break key,
                Ok(None) => {
                    firsts.pop();
                }
                Err(e) => {
                    let Some(c) = super::const_open::needed_first(&e) else { return Err(e) };
                    // Any constant `check::eval_len` evaluates by its checker key (an item's,
                    // an impl's or trait's member's, an anonymous one's) is filed here the
                    // same way; a qualified leaf's need key is not: it is answered by
                    // selection (`const_qpath::eval_asked`), which needs the evaluator free.
                    let fileable = !r.store.maps.qpaths.borrow().is_leaf(c);
                    if rounds >= super::const_open::MOST_FIRSTS || firsts.contains(&c) || !fileable {
                        return Err(e);
                    }
                    rounds += 1;
                    firsts.push(c);
                }
            }
        };
        self.records.iter().find(|x| x.key == key).ok_or(ConstFail::Eval(format!("constant {item:?}: its record was not kept").into_boxed_str()))
    }

    /// Evaluates the constant `c` a head or a body's conversion needs before an evaluation
    /// can go on ([`Consts::value_at`]'s work list) and files it exactly as `check::eval_len`
    /// files the one it is asked: its bits in `Maps::const_bits` (and `const_lens` when they
    /// fit 64 bits); an ANONYMOUS constant the evaluator refuses is filed in
    /// `Maps::anon_failed`, so the head that wrote it is refused by name and only that head.
    /// A failure that itself names a constant to evaluate first is returned for the list.
    fn first<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, c: LibKey) -> Result<(), ConstFail> {
        // An inherent constant leaf a head asked while this evaluation ran
        // (front2/const_inherent.rs): answered and filed with this evaluator.
        if let Some(done) = super::const_inherent::first(self, r, prepared, c) {
            return done;
        }
        let item = super::keys::checker_of(c);
        let anonymous = super::anon::need_key(item) == c;
        super::anon::file_asked(&mut *r.store, &mut *r.keys).map_err(|e| ConstFail::Eval(format!("constant {item:?}: {e}").into_boxed_str()))?;
        let bits = match self.value_key(r, prepared, item, Root::Plain) {
            Ok(key) => self.records.iter().find(|x| x.key == key).and_then(int_of),
            Err(e) if !anonymous || super::const_open::needed_first(&e).is_some() => return Err(e),
            Err(e) => {
                r.store.maps.anon_failed.insert(c, format!("{e}").into_boxed_str());
                return Ok(());
            }
        };
        match bits {
            Some(bits) => {
                r.store.maps.const_bits.insert(c, bits);
                if let Ok(v) = u64::try_from(bits) {
                    r.store.maps.const_lens.insert(c, v);
                }
                Ok(())
            }
            None if anonymous => {
                r.store.maps.anon_failed.insert(c, Box::from("its value is not an integer of at most 16 bytes"));
                Ok(())
            }
            None => Err(ConstFail::NotUsize { item }),
        }
    }

    /// One evaluation of [`Consts::value_at`]'s root: the key of its record (kept in
    /// `self.records`), or the refusal.
    fn value_key<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, root: Root) -> Result<RecordKey, ConstFail> {
        let alloc = Alloc::global();
        let (mut ground, mut layout, mut codes) = self.unit.take().unwrap_or_else(|| (PortableTys::new_in(alloc), Layouts::new_in(alloc), Codes::new_in(alloc)));
        let root = match root {
            Root::Plain => Ok((item, List::EMPTY)),
            Root::SelfTy(node) => self.select_root(r, item, node, &mut ground, alloc),
            Root::Args(list) => self.select_args(r, item, list, &mut ground, alloc),
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
            let world = ConstsWorld { names: r.names, keys: r.keys, index: r.index, store: r.store, bodies: &self.bodies, records: &self.records, selected: core::cell::Cell::new(None), project: super::const_project::ProjectAsk::new(r.buckets.as_ref()) };
            let answer = value_of(&world, &mut layout, &mut codes, n.item, n.args, &mut ground, Limits::DEFAULT, alloc);
            let selected = world.selected.take();
            let asked = world.project.into_asked();
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
                    // `<T as Trait>::NAME` reached at a ground `T` that the solver selected no
                    // definition for (const_select.rs, kept in `selected` by `resolve`): the
                    // step's refusal is that one, by name. A selection that asked for records
                    // (the impls of (trait, `T`), a header) noted them with the projections'
                    // and is answered below: made, and the same need evaluated again.
                    let no_impl = match e.refusal {
                        Refusal::Missing { what: "impl for an associated item", .. } => selected,
                        _ => None,
                    };
                    // A call of a `#[rustc_intrinsic]` fn the table does not hold (`free_fn_kind`
                    // answered `UnknownIntrinsic`): named by the item's written name, the same
                    // read `free_fn_kind` made, in this evaluation and every later one that runs
                    // the kept code.
                    let unknown = match e.refusal {
                        Refusal::UnknownIntrinsic { item } => Some(item),
                        _ => None,
                    };
                    let text = format!("{e}").into_boxed_str();
                    match (unknown, no_adt, no_impl) {
                        (Some(item), _, _) => Err(match r.keys.library_in(r.names, item).and_then(|lib| written_name(r.names, lib)) {
                            Some(name) => ConstFail::UnknownIntrinsic { item, name: Box::from(name) },
                            // Not reached: `free_fn_kind` answers `UnknownIntrinsic` only after
                            // reading this name; the evaluator's own text names the item and why.
                            None => ConstFail::Eval(text),
                        }),
                        (None, Some(adt), _) => match adt_row(r, n.item, adt) {
                            Ok(()) => continue,
                            Err(e) => Err(e),
                        },
                        (None, None, Some(refused)) => Err(refused.into_fail()),
                        // A projection a type of the step holds (a field's `T::Storage`): the
                        // records its normalization needs are made now and the same need is
                        // evaluated again; one the solver refuses is named
                        // (front2/const_project.rs `answer`).
                        (None, None, None) => match super::const_project::answer(r, n.item, asked) {
                            // The fills change what a type grounds to: code that met the
                            // unanswered projection is not complete (blaze-consteval
                            // machine/late.rs), so the evaluator compiles that body again.
                            Some(Ok(())) => continue,
                            Some(Err(e)) => Err(e),
                            None => Err(ConstFail::Eval(text)),
                        },
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
        last.ok_or(ConstFail::Eval(format!("constant {item:?}: the evaluation settled with no record").into_boxed_str()))
    }

    /// Lowers and checks the body of `item` (a constant, a static or a `const fn`) for the
    /// evaluator.
    fn check_one<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>,prepared: &Prepares<'n>, item: ItemKey) -> Result<(), ConstFail> {
        self.check_as(r, prepared, item, item, None)
    }

    /// [`Consts::check_one`] of `item`'s body, kept under `key`: `item`'s own key, or, with
    /// `root` a `const { .. }` block of that body, the block's key (const_at.rs `block_key`).
    /// A `root` that is not a const block of the body as lowered here is refused by name.
    fn check_as<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, key: ItemKey, root: Option<ExprId>) -> Result<(), ConstFail> {
        // A constant declared in a body has its head made first (const_open.rs `local_head`).
        super::const_open::local_head(r, item)?;
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
        if let Some(block) = root
            && !matches!(lowered.typeck.exprs.get(block.0 as usize), Some(tc::Expr::ConstBlock { .. }))
        {
            return Err(ConstFail::NotConstBlock { owner: item, block });
        }
        let text = match (r.store.locals.head(item), r.keys.library_in(r.names, item)) {
            // An item declared in a body: its kept text (the names library holds none for
            // it; body.rs `lower_fn_into` lowered it from the same text).
            (Some(local), _) => Some(local.text),
            (None, Some(lib)) => r.names.text_of(lib).map(|t| t.text),
            (None, None) => r.store.fns.get(&item).map(|f| f.text),
        }
        .ok_or(ConstFail::Body { item, error: BodyError::NoHead(item) })?;
        self.bodies.push(ConstBody { key, text, hash: hash_text(text), typeck: lowered.typeck, tys: lowered.tys, checked, lits, root });
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
