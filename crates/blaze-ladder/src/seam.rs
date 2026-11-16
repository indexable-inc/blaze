//! The seam between a front end and the runner's back half (rule 78: a choice between
//! strategies is a type parameter at one seam). [`FrontEnd`] is every question run.rs's back
//! half (consts, statics, the instance loop, the code builder's [`blaze_build::Adts`], the
//! entry's shape) asks of a front end; [`crate::run::build_with`] is generic over it.
//!
//! Two implementations: the old front end ([`Front`], front.rs: the nine rungs' product today)
//! and the new one ([`crate::front2::program::Front2Program`]). Which one runs is the runner's
//! [`crate::run::FrontChoice`], one explicit input; the old one is the default. A question the
//! new front end cannot answer yet is a typed refusal naming it, never a hand-off to the old one.

use std::alloc::Global;
use std::vec::Vec;

use blaze_build::layout::ReprAttrs;
use blaze_build::{Abi, Intrinsic, StaticItem};
use blaze_ids::ContentId;
use blaze_lower::typeck::TypeckBody;
use blaze_own::Alloc;
use blaze_typeck::{
    Adt, Checked, ExprId, FnSig, InferCtx, ItemKey, LangItem, List, Needs, Outcome, PTy, PTys, PatId, PortableTys, ResolveError,
    resolve_instance, select_impl,
};

use crate::front::{At, FnKind, FnLang, Front, LitTexts};
use crate::run::{Named, RunError};

/// The fns the back half calls on its own account, with no call any checked body recorded:
/// the one list both front ends answer from, so each tables the same set. A front end that
/// tables only what it walks starts its walk at each one the program's world declares.
impl FnLang {
    pub const ALL: [FnLang; 2] = [FnLang::PanicBoundsCheck, FnLang::DropInPlace];

    /// Its `#[lang = ".."]` name.
    pub const fn lang(self) -> &'static str {
        match self {
            FnLang::PanicBoundsCheck => "panic_bounds_check",
            FnLang::DropInPlace => "drop_in_place",
        }
    }

    pub fn named(lang: &str) -> Option<FnLang> {
        FnLang::ALL.into_iter().find(|l| l.lang() == lang)
    }

    /// The `#[lang = ".."]` names a library declares this fn under, the pinned core's first.
    /// The pinned core (library/core/src/ptr/mod.rs) declares the glue as
    /// `#[lang = "drop_glue"] pub(crate) const unsafe fn drop_glue<T: PointeeSized>(_: &mut T)`
    /// (the public `drop_in_place` is only `#[rustc_diagnostic_item = "ptr_drop_in_place"]`
    /// and calls it); a `#![no_core]` program written to the older form declares
    /// `#[lang = "drop_in_place"] unsafe fn drop_in_place<T: ?Sized>(_: *mut T)`. Both are one
    /// fn of one type parameter and one thin-or-wide pointer parameter, built as the glue.
    pub const fn declared_as(self) -> &'static [&'static str] {
        match self {
            FnLang::PanicBoundsCheck => &["panic_bounds_check"],
            FnLang::DropInPlace => &["drop_glue", "drop_in_place"],
        }
    }
}

/// A lang item core marks that the front end could not answer: the item and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LangRefusal {
    pub item: LangItem,
    pub why: &'static str,
}

/// One function as the back half reads it, borrowed from its front end.
#[derive(Clone, Copy)]
pub struct FnFacts<'a> {
    pub key: ItemKey,
    /// Its path from the crate root, for messages.
    pub path: &'a str,
    pub at: At,
    /// The item's text: what its body's ranges are relative to.
    pub text: &'a str,
    pub kind: FnKind,
    pub abi: Abi,
    /// The symbol an outside fn is called by.
    pub symbol: &'a str,
    /// The symbol it is exported as.
    pub export: Option<&'a str>,
    /// Its body in the checker's form, with its literals' texts.
    pub body: Option<(&'a TypeckBody<'static>, &'a LitTexts)>,
    pub const_fn: bool,
    /// A `where const` condition of its owner: evaluated at each selection, never on its own.
    pub condition: bool,
}

impl FnFacts<'_> {
    pub fn named(&self) -> Named {
        Named { path: self.path.into(), at: self.at }
    }
}

/// What the back half asks of a front end. Functions are told apart by a dense index in
/// `0..fn_count()`; `entry()` is the one the program starts at.
pub trait FrontEnd {
    fn fn_count(&self) -> usize;
    fn function(&self, i: usize) -> FnFacts<'_>;
    fn entry(&self) -> Option<usize>;
    /// The program's one type table: signatures' and bodies' types.
    fn types(&self) -> PTys<'_>;
    /// A call in fn `i`'s body of `item` at ground `args`, as the instance that runs: a trait's
    /// fn as the selected impl's (`blaze_typeck::resolve_instance` in `i`'s world).
    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError>;
    /// Whether trait `trait_` has an impl for `ty`, selected in fn `i`'s world.
    fn selects(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError>;
    fn sig(&self, i: usize) -> FnSig<'_>;
    fn fn_index(&self, key: ItemKey) -> Option<usize>;
    /// Every body checked, by index: `None` for a fn without one. A body with any diagnostic
    /// or gap is a refusal, never a `Some`.
    fn check_all(&mut self) -> Result<Vec<Option<Checked<'static>>, Global>, RunError>;
    fn fn_lang(&self, l: FnLang) -> Option<ItemKey>;
    /// The `drop` lang trait and its `drop` fn.
    fn drop_trait(&self) -> Option<(ItemKey, Option<ItemKey>)>;
    /// The item core marks as `item`: `Ok(None)` where the program's crates mark none; a
    /// marked item the front end cannot read is a refusal naming it, never a panic.
    fn lang_item(&self, item: LangItem) -> Result<Option<ItemKey>, LangRefusal>;
    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey>;
    /// Trait `t` as a trait object sees it (blaze-build's `Adts::object_trait`); `None` where
    /// the front end made no table of it (the builder then refuses the vtable by name).
    fn object_trait(&self, t: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>>;
    fn adt(&self, key: ItemKey) -> Option<Adt<'_>>;
    /// An item's name for messages, where the front end can give it.
    fn item_named(&self, key: ItemKey) -> Option<Box<str>>;
    /// Why a survey left `key` out of the program (a reached fn whose check failed, an ADT row
    /// not made, a fn with no head or no body): the build's "no ABI", "no ADT row" and "no
    /// callee" refusals of a call or a type naming it then say this cause instead of only the
    /// symptom. `None` in a run (a run stops at the first such cause) and for the old front.
    fn left_out(&self, _key: ItemKey) -> Option<&str> {
        None
    }
    /// The item the library marks `#[lang = "name"]`, for the builder's own items the
    /// checker's `LangItem` has no variant for (front2/coroutines.rs: the `Poll` enum, the
    /// `Pin` struct, the coroutine resume fn). `None`: no such mark, or a front end that does
    /// not answer (the old front): the builder then refuses the form by name.
    fn lang_named(&self, _name: &str) -> Option<ItemKey> {
        None
    }
    /// The fn whose head made the opaque type `opaque` (an `impl Trait` in its return type, an
    /// `async fn`'s future), and the opaque's index among that fn's. `None`: no head of the
    /// program made it, or a front end that does not answer.
    fn opaque_of(&self, _opaque: ItemKey) -> Option<(ItemKey, u32)> {
        None
    }
    /// The cause of the last `normalize` that gave no type (the trait by name, `Self`'s form,
    /// the solver's error or "no answer"): the build's `Projection` refusal then names it.
    fn last_projection(&self) -> Option<Box<str>> {
        None
    }
    /// Constant item `item`'s value as the front end evaluated it (front2 evaluates every
    /// constant a checked body reads, once, when the program is made); `None` where it made
    /// none (the old front: run.rs `eval_consts` evaluates its tabled constants instead).
    fn const_record(&self, _item: ItemKey) -> Option<&blaze_build::data::DataRecord<std::alloc::Global>> {
        None
    }
    /// The value of `need` (a constant read by path, or a `const { .. }` block) at ground
    /// arguments `args` (types of `from`, the build's ground table), evaluated now: the
    /// build's instance loop pulls each need of an instance before it walks it
    /// (front2/const_at.rs). The front end may check a body or make an impl row it has not
    /// made yet. The default evaluates none: each need is refused by name. Asked only of a
    /// front end whose [`FrontEnd::evaluates_at_args`] is true.
    fn const_at(&mut self, need: crate::front2::const_at::ConstNeed, from: blaze_typeck::PTys<'_>, args: &[blaze_typeck::PTy]) -> Result<blaze_build::data::DataRecord<Global>, crate::front2::const_at::ConstAtWhy> {
        let _ = (need, from, args);
        Err(crate::front2::const_at::ConstAtWhy::NoEvaluator)
    }
    /// Whether the build pulls each instance's constants at its ground arguments from
    /// [`FrontEnd::const_at`] (front2), or the front end evaluates constants only at no
    /// arguments, as records the build reads (the old front: run.rs `eval_consts`).
    fn evaluates_at_args(&self) -> bool {
        false
    }
    /// Every `static` item's record as the front end evaluated it, sorted by key (front2
    /// evaluates every static its bodies name, once, when the program is made: front2
    /// statics.rs); empty where it made none (the old front: run.rs `eval_consts` evaluates its
    /// tabled statics instead). run.rs `static_image` places both.
    fn static_records(&self) -> &[(ItemKey, blaze_build::data::DataRecord<std::alloc::Global>)] {
        &[]
    }
    /// `format_args!` expression `e` of `owner`'s body as the builder reads its template
    /// ([`blaze_build::Adts::format_template`]); `None` where the front end made none (the
    /// builder refuses it by name).
    fn format_template(&self, _owner: ItemKey, _e: ExprId) -> Option<&[blaze_build::FmtPiece<'_>]> {
        None
    }
    /// The pinned core's `rt::Argument` constructor for `trait_`
    /// ([`blaze_build::Adts::fmt_argument_new`]); `None` where the front end found none.
    fn fmt_argument_new(&self, _trait_: blaze_typeck::FmtTrait) -> Option<ItemKey> {
        None
    }
    /// The struct `core::fmt::rt::Argument` (`#[lang = "format_argument"]`).
    fn fmt_argument_adt(&self) -> Option<ItemKey> {
        None
    }
    fn int_lit(&self, owner: ItemKey, e: ExprId) -> Option<u128>;
    fn int_pat(&self, owner: ItemKey, p: PatId) -> Option<u128>;
    fn string(&self, owner: ItemKey, e: ExprId) -> Option<&[u8]>;
    /// Float literal `e` of `owner`'s body, its text rounded to the nearest `f64` and to the
    /// nearest `f32` (each rounded once, from the text): the back half picks by the type the
    /// check gave it. `None`: the front end did not read it (the builder refuses it).
    fn float_lit(&self, owner: ItemKey, e: ExprId) -> Option<(u64, u32)> {
        let _ = (owner, e);
        None
    }
    /// Float literal pattern `p` of `owner`'s body (`Pat::Lit` or `Pat::NegLit`): its
    /// magnitude's text rounded to the nearest `f64` and `f32`, as [`FrontEnd::float_lit`].
    fn float_pat(&self, owner: ItemKey, p: PatId) -> Option<(u64, u32)> {
        let _ = (owner, p);
        None
    }
    /// String literal pattern `p` of `owner`'s body, unescaped.
    fn str_pat(&self, owner: ItemKey, p: PatId) -> Option<&str> {
        let _ = (owner, p);
        None
    }
    /// The projection `<args[0] as trait_<args[1..]>>::name` at ground `args` (in `ground`),
    /// normalized by the front end's solver (`blaze_typeck::normalize_projection`): `Ok(None)`
    /// when it is not normalized. The default: not answered (the builder refuses it by name).
    fn normalize(&self, trait_: ItemKey, name: blaze_typeck::Name, args: &[PTy], ground: &mut PortableTys<'_>) -> Result<Option<PTy>, ResolveError> {
        let _ = (trait_, name, args, ground);
        Ok(None)
    }
    fn repr(&self, key: ItemKey) -> ReprAttrs;
    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic>;
    fn static_item(&self, key: ItemKey) -> Option<StaticItem>;
    /// Variant `variant` of enum `adt`'s discriminant value, explicit or implicit; `None` is
    /// its index (the old front refuses an explicit discriminant, so it answers `None`).
    fn discriminant(&self, adt: ItemKey, variant: u32) -> Option<i128> {
        let _ = (adt, variant);
        None
    }
    fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)>;
    /// The fn's code identity: what its instances' keys are made from.
    fn code_of(&self, i: usize) -> ContentId;
}

/// The old front end: every answer is the inherent one of the same name, unchanged.
impl FrontEnd for Front<'_> {
    fn fn_count(&self) -> usize {
        self.fns.len()
    }

    fn function(&self, i: usize) -> FnFacts<'_> {
        let f = &self.fns[i];
        FnFacts {
            key: f.key,
            path: &f.path,
            at: f.at,
            text: f.text,
            kind: f.kind,
            abi: f.abi,
            symbol: &f.symbol,
            export: f.export.as_deref(),
            body: f.body.as_ref().map(|b| (&b.body, &b.lits)),
            const_fn: f.const_fn,
            condition: f.condition.is_some(),
        }
    }

    fn entry(&self) -> Option<usize> {
        self.entry
    }

    fn types(&self) -> PTys<'_> {
        self.tys.view()
    }

    fn resolve(&self, i: usize, item: ItemKey, args: List, ground: &mut PortableTys<'_>) -> Result<(ItemKey, List), ResolveError> {
        let r = resolve_instance(&Front::world(self, i), item, args, ground, Alloc::global())?;
        Ok((r.item, r.args))
    }

    fn selects(&self, i: usize, trait_: ItemKey, ty: PTy, ground: &mut PortableTys<'_>) -> Result<bool, ResolveError> {
        Ok(select_impl(&Front::world(self, i), trait_, &[ty], ground, Alloc::global())?.is_some())
    }

    fn sig(&self, i: usize) -> FnSig<'_> {
        Front::sig(self, i)
    }

    fn fn_index(&self, key: ItemKey) -> Option<usize> {
        Front::fn_index(self, key)
    }

    /// Every body checked once, generically, in its own world.
    fn check_all(&mut self) -> Result<Vec<Option<Checked<'static>>, Global>, RunError> {
        let types = self.tys.view();
        let mut checked = Vec::with_capacity_in(self.fns.len(), Global);
        for (i, function) in self.fns.iter().enumerate() {
            let Some(body) = function.body.as_ref() else {
                checked.push(None);
                continue;
            };
            let name = || Named { path: function.path.clone(), at: function.at };
            let view = body.body.view(types);
            let world = Front::world(self, i);
            let mut cx = InferCtx::new_in(Alloc::global());
            let done = match blaze_typeck::check(&view, &world, &mut cx, &Needs::new_in(Alloc::global()), Alloc::global()) {
                Ok(Outcome::Done(done)) => done,
                Ok(Outcome::Need(_)) => return Err(RunError::Need { function: name() }),
                Err(error) => return Err(RunError::Check { function: name(), error }),
            };
            if let Some(first) = done.diags.first() {
                return Err(RunError::Types { function: name(), first: first.code, count: done.diags.len() as u32 });
            }
            checked.push(Some(done));
        }
        Ok(checked)
    }

    fn fn_lang(&self, l: FnLang) -> Option<ItemKey> {
        Front::fn_lang(self, l)
    }

    fn drop_trait(&self) -> Option<(ItemKey, Option<ItemKey>)> {
        Front::drop_trait(self)
    }

    fn lang_item(&self, item: LangItem) -> Result<Option<ItemKey>, LangRefusal> {
        Ok(Front::lang_item(self, item))
    }

    /// The old front end makes no trait-object tables: a vtable is the builder's `NoTrait`.
    fn object_trait(&self, _: ItemKey) -> Option<blaze_build::vtable::ObjectTrait<'_>> {
        None
    }

    fn lang_trait_fn(&self, trait_: LangItem, name: &str) -> Option<ItemKey> {
        Front::lang_trait_fn(self, trait_, name)
    }

    fn adt(&self, key: ItemKey) -> Option<Adt<'_>> {
        Front::adt(self, key)
    }

    /// The old front end names no item by key alone.
    fn item_named(&self, _: ItemKey) -> Option<Box<str>> {
        None
    }

    fn int_lit(&self, owner: ItemKey, e: ExprId) -> Option<u128> {
        Front::int_lit(self, owner, e)
    }

    fn int_pat(&self, owner: ItemKey, p: PatId) -> Option<u128> {
        Front::int_pat(self, owner, p)
    }

    fn string(&self, owner: ItemKey, e: ExprId) -> Option<&[u8]> {
        Front::string(self, owner, e)
    }

    fn repr(&self, key: ItemKey) -> ReprAttrs {
        Front::repr(self, key)
    }

    fn intrinsic(&self, item: ItemKey) -> Option<Intrinsic> {
        Front::intrinsic(self, item)
    }

    fn static_item(&self, key: ItemKey) -> Option<StaticItem> {
        Front::static_item(self, key)
    }

    fn ctor_of(&self, key: ItemKey) -> Option<(ItemKey, u32)> {
        Front::ctor_of(self, key)
    }

    fn code_of(&self, i: usize) -> ContentId {
        Front::code_of(self, i)
    }
}
