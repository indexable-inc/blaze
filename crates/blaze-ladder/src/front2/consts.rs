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
    ConstError, ConstRecord, ConstWorld, FnKind, LayoutFacts, Layouts, Lits, Missing as LayoutMissing, NeedKind, Refusal, ReprAttrs, Settle, Typed,
    ValidRange,
};
use blaze_consteval::{Answer, Codes, Limits, Need, RecordKey, value_of};
use blaze_id::ItemKey as LibKey;
use blaze_ids::{ContentId, Hasher};
use blaze_lower::scoped::Prepares;
use blaze_lower::typeck::TypeckBody;
use blaze_lower::View;
use blaze_own::Alloc;
use super::source::NamesSource;
use blaze_typeck as tc;
use blaze_typeck::{Checked, ExprId, Fetch, ItemKey, List, Missing, PatId, PortableTys, Range};

use super::body::{BodyError, lower_fn, lower_member};
use super::check::{CheckFail, Records, check_body};
use super::heads::Store;
use super::index::Index;
use super::keys::Keys;
use super::spell::Spelling;
use super::types::Why;

/// How deep constants may need each other (rule 18).
pub const MOST_NEEDS: u32 = 256;

/// The most evaluation steps (one `value_of` or one body checked) for one constant: past this
/// the needs do not settle (rule 18).
pub const MOST_STEPS: u32 = 4096;

/// The first failure encountered while preparing a body for interpretation.
pub enum RequiredBodyCause {
    Lower(BodyError),
    Spelling(Why),
    Check(CheckFail),
    Diagnostic(Box<str>),
    Literal,
}

// The body task prints its verdict through Debug. Use the checker's diagnostic
// rendering there too, including the Rust error code, item and range.
impl core::fmt::Debug for RequiredBodyCause {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(self, f)
    }
}

impl core::fmt::Display for RequiredBodyCause {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Lower(error) => write!(f, "{error}"),
            Self::Spelling(why) => write!(f, "spelling the body's names: {why:?}"),
            Self::Check(error) => write!(f, "{error}"),
            Self::Diagnostic(code) => write!(f, "the body has the error {code}"),
            Self::Literal => write!(f, "a literal's spelling is missing from the lowered body"),
        }
    }
}

/// Why a constant has no value.
#[derive(Debug)]
pub enum ConstFail {
    /// Checking a body demanded by this anonymous constant stopped. The
    /// expression is quoted source. The original failure retains its type.
    RequiredBody {
        constant: ItemKey,
        expression: blaze_lower::typeck::GapText,
        needed: ItemKey,
        shown: Option<Box<str>>,
        cause: RequiredBodyCause,
    },
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
    /// The work list returned a record key absent from its own value table.
    MissingRecord { item: ItemKey, key: RecordKey },
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
    /// Expression `borrow` of `owner`'s body, as this evaluator lowered and checked it, is not
    /// a `&operand` its check lists in `Checked::promoted`: the promoted borrow an instance's
    /// build asked the value of (const_at.rs).
    NotPromoted { owner: ItemKey, borrow: ExprId },
    /// A type `item`'s evaluation lays out or grounds (a field's type: `Atomic<T>`'s
    /// `UnsafeCell<T::Storage>`) holds a projection the checker's solver did not normalize:
    /// that type has no ground form (front2/const_project.rs).
    Projection { item: ItemKey, refused: super::const_project::ProjectionRefused },
    /// A trait's constant the solver selected no definition for, for a reason other than no
    /// impl (two impls hold, a question the world has no answer to, a trait declared in a
    /// body): front2/const_select.rs.
    SelectConst { refused: super::const_select::ConstSelectRefused },
    /// A call of `callee` (`shown`: its path, item_shown.rs) whose head was read and has no
    /// `const` (the Reference, "Constant evaluation": a call in a constant context is of a
    /// const fn or a tuple constructor).
    NotConstFn { callee: ItemKey, shown: Box<str> },
    /// A call of `callee` (`shown`: its path) whose kind needs `fact`, which front2 does not
    /// hold: `made` is the want made for it in this evaluation, if one was; `why` the store's or
    /// the head maker's own words, if any.
    FnFact { callee: ItemKey, shown: Box<str>, fact: blaze_consteval::Fact, made: Option<super::heads::Want>, why: Option<Box<str>> },
    /// Constants whose values need each other (blaze-consteval's `Refusal::Cycle`), in the
    /// order met, the first needed again last; `shown`: each by its path (rule 19). rustc's
    /// E0391, "cycle detected when const-evaluating": no value, the program is refused.
    Cycle { chain: Vec<ItemKey, Global>, shown: Box<str> },
}

impl ConstFail {
    fn required_by<'n, S: NamesSource<'n>>(self, r: &Records<'_, 'n, S>, constant: ItemKey, needed: ItemKey) -> Self {
        if super::const_open::needed_first(&self).is_some()
            || !matches!(r.keys.origin(constant), Some(super::keys::Origin::Member { kind: super::keys::MemberKind::Anon, .. }))
        {
            return self;
        }
        let Some(head) = r.store.fns.get(&constant) else { return self };
        let cause = match self {
            Self::Body { error, .. } => RequiredBodyCause::Lower(error),
            Self::Spelling { why, .. } => RequiredBodyCause::Spelling(why),
            Self::Check { error, .. } => RequiredBodyCause::Check(error),
            Self::Diag { code, .. } => RequiredBodyCause::Diagnostic(code),
            Self::Literal { .. } => RequiredBodyCause::Literal,
            other => return other,
        };
        let shown = if needed == constant {
            None
        } else if let Some(local) = r.store.locals.head(needed) {
            r.store.locals.body(local.enclosing).and_then(|b| b.rows.iter().find(|row| row.key == local.lib)).map(|row| Box::from(row.name))
        } else {
            Some(super::check::key_shown(r, needed))
        };
        Self::RequiredBody { constant, expression: blaze_lower::typeck::GapText::of(head.text), needed, shown, cause }
    }
}

impl core::fmt::Display for ConstFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConstFail::RequiredBody { constant, expression, needed, shown, cause } => {
                write!(f, "constant {constant:?} `{expression}` needs body {needed:?}")?;
                if let Some(name) = shown {
                    write!(f, " ({name})")?;
                }
                write!(f, ": {cause}")
            }
            ConstFail::Body { item, error } => write!(f, "constant {item:?}: a body it needs: {error}"),
            ConstFail::Spelling { item, why } => write!(f, "constant {item:?}: spelling its body's names: {why:?}"),
            ConstFail::Check { item, error } => write!(f, "constant {item:?}: a body it needs: {error}"),
            ConstFail::Diag { item, code } => write!(f, "constant {item:?}: the body of {item:?} has the error {code}"),
            ConstFail::Literal { item } => write!(f, "the body of {item:?}: a literal's text could not be read"),
            ConstFail::MissingRecord { item, key } => write!(f, "constant {item:?} settled with missing value record {key:?}"),
            ConstFail::UnknownIntrinsic { item, name } => write!(f, "fn {item:?} is the intrinsic `{name}`, which blaze's intrinsics table does not hold"),
            ConstFail::Eval(e) => write!(f, "{e}"),
            ConstFail::Steps { item, last } => write!(f, "constant {item:?}: its needs did not settle in {MOST_STEPS} steps; the last: {last:?}"),
            ConstFail::Nested { item, shown } => write!(f, "constant {item:?} ({shown}): an array length needs it while another constant is evaluated"),
            ConstFail::NotUsize { item } => write!(f, "constant {item:?}: an array length's value is not a `usize` of 64 bits"),
            ConstFail::TooManyLengths { item, most } => write!(f, "constant {item:?}: one head named more than {most} array-length constants"),
            ConstFail::AdtRow { item, adt, why } => write!(f, "constant {item:?}: a layout it reads needs the row of ADT {adt:?}: {why}"),
            ConstFail::TraitConstNoSelf { item, constant } => write!(f, "constant {item:?}: the trait's constant {constant:?} is read with no `Self` (a checked body's constant path holds no arguments yet): no impl can be selected"),
            ConstFail::NoImplConst { trait_, self_ty, constant } => write!(f, "no impl of trait {trait_:?} holds for the type {self_ty:?} to give its associated item {constant:?} (rule: the value is the definition in the impl the solver selects for the trait reference, or the trait's default where that impl writes none)"),
            ConstFail::NoTraitConst { trait_, constant } => write!(f, "trait {trait_:?} declares no constant under the key {constant:?}"),
            ConstFail::QPathType { constant } => write!(f, "the trait's constant {constant:?}: a qualified path in a head has a value only at `bool`, `char` or an integer type of at most 16 bytes"),
            ConstFail::QPathCycle { need } => write!(f, "the qualified constant {need:?} needs itself: an impl header of its own selection writes it"),
            ConstFail::QPathDepth { need, most } => write!(f, "the qualified constant {need:?}: more than {most} are answered inside one another"),
            ConstFail::QPathKey { need, clash } => write!(f, "the qualified constant {need:?}: two origins under one checker key: {clash:?}"),
            ConstFail::NotConstBlock { owner, block } => write!(f, "fn {owner:?}: expression {} of its body as the evaluator lowered it is not a `const {{ .. }}` block", block.0),
            ConstFail::NotPromoted { owner, borrow } => write!(f, "fn {owner:?}: expression {} of its body as the evaluator lowered and checked it is not a promoted borrow (`&operand` in `Checked::promoted`)", borrow.0),
            ConstFail::Projection { item, refused } => write!(f, "constant {item:?}: a type it lays out: {refused}"),
            ConstFail::SelectConst { refused } => write!(f, "{refused}"),
            ConstFail::NotConstFn { shown, .. } => write!(f, "a call of {shown}, which is not a `const fn` (its head was read and has no `const`)"),
            ConstFail::FnFact { shown, fact, made, why, .. } => {
                write!(f, "a call of {shown}: its kind is not known: {fact}")?;
                if let Some(w) = made {
                    write!(f, "; made {w:?}")?;
                }
                match why {
                    Some(why) => write!(f, ": {why}"),
                    None => Ok(()),
                }
            }
            ConstFail::Cycle { shown, .. } => write!(f, "a cycle of constants (E0391): {shown}"),
        }
    }
}

/// The constants of a cycle the evaluator met (`Refusal::Cycle`'s chain, the first needed
/// again last), each by its path: "`A` needs `B`, which needs `A`".
fn cycle_of<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, chain: &[ItemKey]) -> ConstFail {
    let mut shown = String::new();
    for (i, &k) in chain.iter().enumerate() {
        let name = super::check::key_shown(r, k);
        match i {
            0 => shown.push_str(&name),
            1 => shown.push_str(&format!(" needs {name}")),
            _ => shown.push_str(&format!(", which needs {name}")),
        }
    }
    let mut kept = Vec::with_capacity_in(chain.len(), Global);
    kept.extend_from_slice(chain);
    ConstFail::Cycle { chain: kept, shown: shown.into_boxed_str() }
}

/// Makes the store's row of ADT `adt`, which the evaluator's layout asked for and did not find
/// (`size_of::<S>()` in constant `item`): by `Store::ensure(Want::Adt)`, the one maker of an
/// ADT's row (the run path's too: program.rs), so the evaluator and the code builder lay out
/// the same row. A row that cannot be made, or that is made and still gives no layout, is the
/// constant's refusal, naming the ADT. A row whose head names a constant (`struct S { b: [u8;
/// LEN] }`: the evaluator is busy with `item`, so the store cannot evaluate `LEN` inside) is
/// returned as the head's `NeedConst`, the form [`super::const_open::needed_first`] reads: the
/// work list (`Consts::value_listed`) evaluates `LEN` first, then this need again, which makes
/// the row (the Reference, "Constant evaluation": an array type's length is a const context,
/// evaluated as a constant item is, to any depth).
fn adt_row<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,item: ItemKey, adt: ItemKey) -> Result<(), ConstFail> {
    let refuse = |why: String| ConstFail::AdtRow { item, adt, why: why.into_boxed_str() };
    if r.store.adts.contains_key(&adt) {
        return Err(refuse(String::from("its row is made and the layout still does not find it")));
    }
    let lib = r.keys.library_in(r.names, adt).ok_or_else(|| refuse(String::from("it has no library key")))?;
    match r.store.ensure(r.names, &mut *r.keys, super::heads::Want::Adt(lib)) {
        Ok(()) => {}
        Err(error @ super::heads::HeadError::Item(_, Why::NeedConst(..))) => return Err(ConstFail::Body { item, error: BodyError::Head { item: lib, error } }),
        Err(e) => return Err(refuse(format!("{e}"))),
    }
    match r.store.adts.get(&adt) {
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
    inputs: Option<super::heads::ConstInputs>,
    text: &'n str,
    hash: ContentId,
    typeck: TypeckBody<'static>,
    tys: tc::OwnTys<'static>,
    checked: Checked<'static>,
    lits: LitTable,
    /// `Some(at)`: a constant read from inside its owner's body ([`BodyRoot`], front2/const_at.rs):
    /// the owner's body read from expression `at` as its root, with no parameters (a const
    /// block, or a promoted borrow's operand); `key` is that constant's own key.
    root: Option<ExprId>,
}

/// Where in its owner's body a constant read from inside a body starts (front2/const_at.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyRoot {
    /// A `const { .. }` block: the block expression itself.
    Block(ExprId),
    /// A promoted borrow `&operand` (`Checked::promoted`): the value is the OPERAND's, and the
    /// builder takes its row's address (blaze-build walk/promote.rs). The Reference, "Constant
    /// promotion": the expression "could be written in a constant and borrowed".
    Promoted(ExprId),
}

/// The constants' state of one build: the bodies checked and the records made.
pub struct Consts<'n> {
    bodies: Vec<ConstBody<'n>, Global>,
    records: Vec<Evaluated>,
    unit: Option<(PortableTys<'static>, Layouts<Alloc<'static>>, Codes<Alloc<'static>>)>,
    /// The facts of every closure written in a body checked here ([`Consts::check_as`]: a
    /// constant's, a const block's owner's), made by the build's one rule
    /// (`run::ClosureTable::add`); `None` until the first body with a closure is checked.
    closures: Option<crate::run::ClosureTable>,
}

/// An instance's arguments refer to this evaluator's ground table. Only the
/// content-keyed record leaves it.
struct Evaluated {
    instance: blaze_build::Instance,
    record: ConstRecord<Alloc<'static>>,
    inputs: Option<super::heads::ConstInputs>,
}

impl Default for Consts<'_> {
    fn default() -> Self {
        Consts { bodies: Vec::new_in(Global), records: Vec::new_in(Global), unit: None, closures: None }
    }
}

/// The world one evaluation step reads: front2's records as they are now.
struct ConstsWorld<'w, 'n, S> {
    names: S,
    keys: &'w Keys,
    index: &'w Index<'n>,
    store: &'w Store<'n>,
    bodies: &'w [ConstBody<'n>],
    records: &'w [Evaluated],
    /// The last trait's constant [`ConstWorld::resolve`] asked the solver for and had refused
    /// (const_select.rs): the step's typed refusal when the evaluation stops there.
    selected: core::cell::Cell<Option<super::const_select::ConstSelectRefused>>,
    /// What the step's projections asked of the records ([`LayoutFacts::normalize`] below):
    /// [`Consts::value`] makes its fills and asks again, or refuses naming the projection.
    project: super::const_project::ProjectAsk<'w>,
    /// What a callee's head is lowered in ([`ConstWorld::fn_kind`]): handed down from the
    /// evaluation (D102), never named here.
    alloc: Alloc<'w>,
    /// The closures of the bodies this evaluator checked ([`Consts::closures`]) and of the
    /// build's checked fn bodies (the caller's table: a closure type among the instance's
    /// arguments, `<{closure} as SizedTypeProperties>::SIZE` under `Box::new`), which
    /// [`LayoutFacts::closure`] answers from.
    closures: Closures<'w>,
}

/// Where [`ConstsWorld`]'s closure facts come from: the evaluator's own checked bodies and the
/// build's, both tables made by `run::ClosureTable::add` from the body that holds the closure.
#[derive(Clone, Copy)]
struct Closures<'w> {
    own: Option<&'w crate::run::ClosureTable>,
    build: Option<&'w crate::run::ClosureTable>,
}

impl Closures<'_> {
    fn get(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
        self.own.and_then(|t| t.get(owner, ordinal)).or_else(|| self.build.and_then(|t| t.get(owner, ordinal)))
    }
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
pub(super) fn is_panic_lang(name: &str) -> bool {
    matches!(name, "panic" | "panic_fmt" | "const_panic_fmt" | "panic_display" | "panic_str_2015" | "panic_nounwind" | "panic_explicit" | "panic_bounds_check" | "panic_misaligned_pointer_dereference")
        || name.starts_with("panic_const_")
}

/// Library item `lib`'s name as written: its item text's own name (`Names::item_text`, as
/// program.rs `item_named` reads it). Not sliced from the outline's text by the entry's name
/// range: that range was not the text's (`size_of` came out as "onst_ev", seven bytes of
/// another item, and the intrinsic was refused as unknown).
pub(super) fn written_name<'n, S: NamesSource<'n>>(names: S, lib: LibKey) -> Option<&'n str> {
    names.name_of(lib)
}

impl<'n, S: NamesSource<'n>> ConstsWorld<'_, 'n, S> {
    /// `item` as an associated constant DECLARED by a trait (`Specifier::BITS`): the trait and
    /// the constant's name; `None` for any other item (an impl's constant, a free one).
    fn trait_const(&self, item: ItemKey) -> Option<(ItemKey, tc::Name)> {
        match self.keys.origin(item)? {
            super::keys::Origin::Member { container, kind: super::keys::MemberKind::Const, name } if self.store.trait_defs.contains_key(container) => Some((*container, tc::Name::of(name))),
            _ => None,
        }
    }

    /// The trait that declares fn `item`, when it is a trait's member (its call or its fn
    /// pointer is selected at the instance).
    fn trait_fn(&self, item: ItemKey) -> Option<ItemKey> {
        self.store.fns.get(&item).and_then(|f| f.container).filter(|c| self.store.trait_defs.contains_key(c))
    }
}

impl<'n, S: NamesSource<'n>> LayoutFacts for ConstsWorld<'_, 'n, S> {
    /// The store's row of the ADT (heads.rs `Store::adt`: the one maker, the same row the run
    /// path hands the code builder: program.rs `FrontEnd::adt`). `None` while the row is not
    /// made: the layout refuses naming the ADT, and [`Consts::value`] makes the row and asks
    /// again.
    fn adt(&self, key: ItemKey) -> Option<tc::Adt<'_>> {
        let a = self.store.adts.get(&key)?;
        Some(tc::Adt { kind: a.kind, generics: a.generics, variants: &a.variants, fields: &a.fields, types: self.store.tys.view() })
    }

    /// As the row read the ADT's `#[repr(..)]`; no row: `Missing` (the machine's head need, as
    /// `adt`'s `None`), never a default `repr(Rust)` (D194).
    fn repr(&self, key: ItemKey) -> Result<ReprAttrs, LayoutMissing> {
        self.store.adts.get(&key).map(|a| a.repr).ok_or(LayoutMissing { item: key })
    }

    /// From the ADT's row: every variant's value, explicit or implicit (heads.rs `discrs`),
    /// evaluated when the row was made, exactly as the run path answers (program.rs
    /// `FrontEnd::discriminant`). Never `Err`: that names a constant's record to make, and no
    /// record is owed here (the row holds the values; the layout asks only after `adt`).
    fn discriminant(&self, key: ItemKey, variant: u32) -> Result<Option<i128>, LayoutMissing> {
        Ok(self.store.adts.get(&key).and_then(|a| a.discrs.get(variant as usize)).copied())
    }

    /// From the ADT's row (heads.rs `AdtRow::valid`), as the run path answers (program.rs
    /// `FrontEnd::valid_range`): a constant and the code agree on every niche.
    fn valid_range(&self, key: ItemKey) -> Result<ValidRange, LayoutMissing> {
        self.store.adts.get(&key).map(|a| a.valid).ok_or(LayoutMissing { item: key })
    }

    /// By the checker's solver over the store (front2/const_project.rs): a field's type
    /// grounded for a static's or constant's layout (`Atomic<usize>`'s
    /// `UnsafeCell<<usize as AtomicPrimitive>::Storage>`) normalizes exactly as the run path's
    /// layout does (program.rs `FrontEnd::normalize`). What it could not answer is kept in
    /// `project` for [`Consts::value`].
    fn normalize(&self, trait_: ItemKey, name: tc::Name, args: &[tc::PTy], ground: &mut PortableTys<'_>) -> Result<tc::PTy, blaze_build::BuildError> {
        self.project.normalize(self.names, self.keys, self.index, self.store, trait_, name, args, ground)
    }

    /// From the check record of the body that holds the closure, whatever kind of body it is
    /// ([`Closures`]): a constant's or const block's owner checked here, or a fn the build
    /// checked. `None`: no such body was checked, and the layout refuses naming the closure.
    fn closure(&self, owner: ItemKey, ordinal: u32) -> Option<blaze_build::layout::ClosureFacts<'_>> {
        self.closures.get(owner, ordinal)
    }
}

impl<'n, S: NamesSource<'n>> ConstWorld for ConstsWorld<'_, 'n, S> {
    fn entered_body(&self, item: ItemKey) {
        if self.store.maps.const_proofs.is_some()
            && let Some(body) = self.bodies.iter().find(|body| body.key == item)
        {
            self.store.maps.read_inputs(body.inputs.as_ref());
        }
    }

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

    /// What a call of `item` does (front2/const_fn_fact.rs `fn_fact`). A fact it needs and
    /// does not find is `FnKind::Missing(fact)`: the evaluator refuses the call as the pending
    /// `Refusal::FnFact`, and [`Consts::value`] makes the fact or names it (const_fn_fact.rs
    /// `answer`). `NotConst` only from a head read without `const`.
    fn fn_kind(&self, item: ItemKey) -> FnKind {
        super::const_fn_fact::fn_fact(self.names, self.keys, self.index, self.store, item)
    }

    fn resolve(&self, item: ItemKey, args: List, ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Option<(ItemKey, List)> {
        // A trait's constant (it has a row in `Store::fns` whose container is the trait:
        // heads.rs `trait_members`) or a trait's fn, selected by the checker's solver
        // (const_select.rs); what it asks the records to make is noted with the projections'
        // (`project`), its refusal kept in `selected`: either way `None` here, and
        // [`Consts::value`] makes the fills and evaluates again, or refuses by name. A default
        // selected is the trait's own item at the same arguments: asked again, the answer is
        // the same (the selection is idempotent).
        //
        // A trait's fn as its constant is (the Reference, "Associated Items": an associated
        // item named through a trait at a type is the definition in the impl of that trait for
        // that type, or the trait's default where the impl writes none): the fn pointer a
        // constant makes of `<T as Tr>::f`, or a call of it, is that definition's instance
        // (rustc reifies `Instance::resolve`'s answer, compiler/rustc_middle/src/ty/instance.rs;
        // specialization picks the most specific impl there). alloc's `in_place_collect`
        // `from_iter` (`const { if .. { from_iter_in_place } else { SpecFromIterNested::<T,
        // I>::from_iter } }`) at tempo's `[Bytes, Map<IntoIter<Vec<u8>>, ..>]` was refused
        // "the world has no impl for an associated item" (t1545): every trait fn was `None`.
        let trait_ = self.trait_const(item).map(|(t, _)| t).or_else(|| self.trait_fn(item));
        if let Some(trait_) = trait_ {
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
            (None, None) => self.store.fns.get(&item).map(|f| super::tables::head::member_definition(self.names, self.keys, item, f)),
        };
        item_code_id(item, text)
    }

    /// From the heads' table of statics (heads.rs `statics`, the checker's `Def::Static`).
    fn is_static(&self, item: ItemKey) -> bool {
        self.store.statics.contains_key(&item)
    }

    fn record(&self, key: RecordKey) -> Fetch<&ConstRecord<Alloc<'_>>> {
        let row = self.records.iter().find(|r| r.record.key == key).ok_or(Missing)?;
        self.store.maps.read_inputs(row.inputs.as_ref());
        Ok(&row.record)
    }

    fn explicit_discriminant(&self, _: ItemKey, _: u32) -> Option<ItemKey> {
        None
    }

    /// By the records (front2/const_drop.rs): the ADT's impl of the `Drop` lang trait in its
    /// own bucket (blaze-typeck `adt_drop_impl`, as run.rs answers the builder); what is not
    /// made yet is noted in `project` as fills, made by `value_key`, and the step asked again.
    fn drop_impl(&self, ty: tc::PTy, ground: &mut PortableTys<'_>) -> Option<Result<Option<ItemKey>, blaze_build::BuildError>> {
        super::const_drop::drop_impl(self.names, self.keys, self.index, self.store, &self.project, ty, ground)
    }

    /// core's `#[lang = "manually_drop"]` ADT (front2/const_drop.rs).
    fn manually_drop(&self) -> Option<Option<ItemKey>> {
        super::const_drop::manually_drop(self.names, self.index, &self.project)
    }

    /// The item paths `type_name` prints (the `TypeNames` impl below).
    fn type_names(&self) -> Option<&dyn blaze_build::type_name::TypeNames> {
        Some(self)
    }

    /// The `Fn*` lang trait's method by the one map (front2/langs.rs `fn_trait_method`), as the
    /// run answers the builder (program.rs `lang_trait_fn`: the trait `#[lang]` names, its
    /// member filed with its head), so a closure's fn pointer here names the instance the run
    /// builds. A member is filed only once its trait's head is made: until then the head is
    /// noted as a fill (its want's refusal as a gap) in `project`, which [`Consts::value`]
    /// makes before it evaluates the need again, or refuses by name. `None` with nothing noted:
    /// the library declares no such lang trait, and the evaluator refuses the coercion by name.
    fn fn_trait_method(&self, which: blaze_build::FnTrait) -> blaze_consteval::FnTraitMethod {
        use blaze_consteval::FnTraitMethod::{Absent, Found, Pending};
        let (lang, name) = super::langs::fn_trait_method(which);
        let Some(lib) = super::langs::lang_name(lang).and_then(|n| self.index.lang(n)) else { return Absent };
        let trait_ = super::keys::checker_of(lib);
        if let Some(method) = self.keys.filed_member(trait_, super::keys::MemberKind::Fn, name) {
            return Found(method);
        }
        let mut fills = Vec::new_in(Global);
        let mut gaps = Vec::new_in(Global);
        // The trait's head made and still no such member: the trait declares none (`Absent`,
        // which the evaluator refuses by name); not made yet: noted as a fill, and the
        // evaluation is asked again once it is (`Pending`, naming the trait).
        let method = match super::fill::want_of_item(self.names, lib) {
            Ok(want) if self.store.made(want) => return Absent,
            Ok(want) => {
                fills.push(super::world_items::Fill::Store(want));
                Pending { trait_ }
            }
            Err(e) => {
                gaps.push(super::world_items::Gap::Fill(e));
                Absent
            }
        };
        self.project.note(fills, gaps);
        method
    }
}

/// `core::any::type_name`'s item paths over the names source and store
/// (`super::type_name::item_path`, shared with the build seam).
impl<'n, S: NamesSource<'n>> blaze_build::type_name::TypeNames for ConstsWorld<'_, 'n, S> {
    fn item_path(&self, item: ItemKey, out: &mut dyn FnMut(&str)) -> bool {
        super::type_name::item_path(self.names, self.keys, self.store, item, out)
    }
}

/// Where an evaluation starts ([`Consts::value_at`]): `item` at no arguments; a trait's
/// constant selected for a ground `Self` (a head's qualified path); or `item` at ground
/// arguments already in the evaluator's own table (an instance's need: const_at.rs); or a
/// constant at `at` of `owner`'s body (a `const { .. }` block, a promoted operand) at the
/// owner's ground `args`, whose body check runs INSIDE the evaluation ([`Consts::body_value`]).
#[derive(Clone, Copy)]
enum Root {
    Plain,
    SelfTy(tc::PNode),
    Args(List),
    Body { owner: ItemKey, at: BodyRoot, args: List },
}

impl<'n> Consts<'n> {
    /// The record of constant `item` (a checker key), evaluating every constant it needs first.
    pub fn value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>,prepared: &Prepares<'n>, item: ItemKey) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        self.value_at(r, prepared, item, Root::Plain, None)
    }

    /// The record of `<Self as Trait>::NAME` for the ground type `self_ty` (a primitive, or
    /// an ADT with no generic arguments), where `declared` is the TRAIT's constant
    /// (front2/const_qpath.rs: a qualified path leaf of a head's constant expression). The
    /// impl is selected by the checker's solver (const_select.rs `select_made`, which makes
    /// the candidates of (trait, type) it asks for), and what it selects (the impl's
    /// definition, or the trait's default at `[self_ty]`) is evaluated as [`Consts::value`]
    /// evaluates any constant. No impl selected: [`ConstFail::NoImplConst`].
    pub fn trait_const_value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, declared: ItemKey, self_ty: tc::PNode) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        self.value_at(r, prepared, declared, Root::SelfTy(self_ty), None)
    }

    /// The record of constant `item` at the ground arguments `args` (types of `from`, the
    /// build's ground table: front2/const_at.rs): `[Self, trait args..]` for a trait's constant
    /// as declared, whose impl's definition (or the trait's default) is selected at that `Self`
    /// (const_select.rs `select_made`, the checker's solver, making the records it asks for);
    /// the impl's arguments for an inherent impl's. The arguments are copied into this
    /// evaluator's own table first; every constant the value needs is evaluated first by the
    /// same work list as [`Consts::value`]'s (a cycle is the evaluator's typed refusal).
    /// `build` is the build's table of its checked fn bodies' closures: a closure type among
    /// `args` is laid out from its owner's facts there.
    #[allow(clippy::too_many_arguments)]
    pub fn value_with_args<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, from: tc::PTys<'_>, args: &[tc::PTy], build: Option<&crate::run::ClosureTable>, scratch: Alloc<'_>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        let list = self.args_in_unit(item, from, args, r.store.tys.alloc(), scratch)?;
        self.value_at(r, prepared, item, Root::Args(list), build)
    }

    /// The record of the constant at `root` of `owner`'s body (a `const { .. }` block, or a
    /// promoted borrow's operand) at the owner's ground arguments `args` (types of `from`): the
    /// owner's body is lowered and checked for this evaluator once per such constant, under its
    /// key `key` (const_at.rs `block_key` / `promoted_key`), and read from the root expression
    /// ([`ConstBody::root`]); then evaluated as any constant. `build` as
    /// [`Consts::value_with_args`] takes it.
    #[allow(clippy::too_many_arguments)]
    pub fn body_value<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, owner: ItemKey, root: BodyRoot, key: ItemKey, from: tc::PTys<'_>, args: &[tc::PTy], build: Option<&crate::run::ClosureTable>, scratch: Alloc<'_>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        // The owner's body is checked inside `value_at` (`Root::Body`), never before it: a head
        // that check makes may name a constant, and only inside the count (`Maps::evaluating`)
        // is that a need the work list evaluates first. Checked outside it, a caller whose
        // evaluator is taken (const_at's `Records`, `consts: None`) had check.rs `make` file
        // the head's anonymous constant as "no constant evaluator" in `anon_failed` for good,
        // and every later check of a body using that head refused (tempo t2115d: RawVec's
        // `new_in`, a head's `1 << 0`).
        let list = self.args_in_unit(key, from, args, r.store.tys.alloc(), scratch)?;
        self.value_at(r, prepared, key, Root::Body { owner, at: root, args: list }, build)
    }

    /// The fn instance that key `key` names in a record this evaluator made (each record names
    /// the instance every fn word of its value was made from: blaze-consteval
    /// `ConstRecord::fns`), its arguments copied from this evaluator's own type table, where
    /// they were made, into `ground` (the build's): the seam's `FrontEnd::const_fn_instance`.
    /// Read from the record, never searched for among the program's fns, so a generic fn's
    /// instance (`const G: fn(u8) -> u8 = id::<u8>;`), a fn declared inside the constant's own
    /// block (std's `thread_local!` expansion) and a closure's `call_once` are answered alike.
    /// Equal keys name equal instances (the key is a hash of the fn's code identity and its
    /// arguments as trees), so the first record naming it answers. `None`: no record names the
    /// key, or its arguments do not copy (a full table): the builder refuses by name.
    pub fn fn_instance(&self, key: blaze_build::InstanceKey, ground: &mut PortableTys<'_>, scratch: Alloc<'_>) -> Option<blaze_build::Instance> {
        let named = self.records.iter().find_map(|r| r.record.fn_named(key))?;
        let (unit, _, _) = self.unit.as_ref()?;
        let from = unit.view();
        let args = from.list(named.instance.args).ok()?;
        let args = super::const_at::copy_ground(from, args, ground, scratch).ok()?;
        Some(blaze_build::Instance { item: named.instance.item, args })
    }

    /// `args` (types of `from`, all ground) copied into this evaluator's own type table, as
    /// one list of it.
    /// `alloc` is the store's: the unit's tables are kept with the evaluator.
    fn args_in_unit(&mut self, item: ItemKey, from: tc::PTys<'_>, args: &[tc::PTy], alloc: Alloc<'static>, scratch: Alloc<'_>) -> Result<List, ConstFail> {
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
        let Some(trait_) = self.trait_of_const(&*r, item, alloc) else {
            return Ok((item, args));
        };
        super::const_select::select_made(r, item, trait_, args, ground, alloc)
    }

    /// The root of an evaluation that starts at the trait's constant `declared` for the
    /// ground `self_ty`: `[self_ty]` as the arguments, selected as [`Consts::select_args`]
    /// selects them.
    fn select_root<S: NamesSource<'n>>(&self, r: &mut Records<'_, 'n, S>, declared: ItemKey, self_ty: tc::PNode, ground: &mut PortableTys<'_>, alloc: Alloc<'_>) -> Result<(ItemKey, List), ConstFail> {
        let Some(trait_) = self.trait_of_const(&*r, declared, alloc) else {
            return Err(ConstFail::TraitConstNoSelf { item: declared, constant: declared });
        };
        let full = |_| ConstFail::Eval(Box::from("the constant evaluator's type table is full"));
        let self_arg = ground.intern(self_ty).map_err(full)?;
        let args = ground.intern_list(&[self_arg]).map_err(full)?;
        super::const_select::select_made(r, declared, trait_, args, ground, alloc)
    }

    /// The trait that declares `item`, when `item` is a trait's associated constant
    /// ([`ConstsWorld::trait_const`], over the records as they are now).
    fn trait_of_const<S: NamesSource<'n>>(&self, r: &Records<'_, 'n, S>, item: ItemKey, alloc: Alloc<'_>) -> Option<ItemKey> {
        let world = ConstsWorld { names: r.names, keys: &*r.keys, index: r.index, store: &*r.store, bodies: &self.bodies, records: &self.records, selected: core::cell::Cell::new(None), project: super::const_project::ProjectAsk::new(r.buckets.as_ref()), alloc, closures: Closures { own: self.closures.as_ref(), build: None } };
        world.trait_const(item).map(|(trait_, _)| trait_)
    }

    /// [`Consts::value`] (`self_ty` is `None`: `item` at no arguments), or
    /// [`Consts::trait_const_value`] (`item` is a trait's constant, selected for `self_ty`).
    /// `build`: the build's closure facts, when an instance asked (const_at.rs).
    fn value_at<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, root: Root, build: Option<&crate::run::ClosureTable>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
        // Counted in and out (`Maps::evaluating`): a head that needs a constant while this
        // runs finds the evaluator BUSY (`ConstFail::Nested`, which the work list below
        // reads), where a caller with no evaluator at all has the head refused by name
        // (check.rs `make`).
        r.store.maps.evaluating.set(r.store.maps.evaluating.get().saturating_add(1));
        let out = self.value_listed(r, prepared, item, root, build);
        let left = r.store.maps.evaluating.get().saturating_sub(1);
        r.store.maps.evaluating.set(left);
        if left == 0 {
            r.store.clear_all_provisional();
            if let Some(b) = r.buckets.as_ref() {
                b.clear_provisional();
            }
        }
        out
    }

    /// [`Consts::value_at`]'s work: the root's evaluation, with every constant it needs first.
    fn value_listed<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, root: Root, build: Option<&crate::run::ClosureTable>) -> Result<&ConstRecord<Alloc<'static>>, ConstFail> {
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
                None => self.value_key(r, prepared, item, root, build).map(Some),
                Some(c) => self.first(r, prepared, c).map(|()| None),
            };
            match step {
                Ok(Some(key)) => break key,
                Ok(None) => {
                    if let Some(done_c) = firsts.pop()
                        && r.store.clear_provisional_for(done_c)
                        && let Some(b) = r.buckets.as_ref()
                    {
                        b.clear_provisional();
                    }
                }
                Err(e) => {
                    let Some(c) = super::const_open::needed_first(&e) else { return Err(e) };
                    // Any constant `check::eval_len` evaluates by its checker key (an item's,
                    // an impl's or trait's member's, an anonymous one's) is filed here the
                    // same way; a qualified leaf's need key is not: it is answered by
                    // selection (`const_qpath::eval_asked`), which needs the evaluator free.
                    let fileable = !r.store.maps.qpaths.borrow().is_leaf(c);
                    if rounds >= super::const_open::MOST_FIRSTS
                        || firsts.contains(&c)
                        || (firsts.is_empty() && super::keys::checker_of(c) == item)
                        || !fileable
                    {
                        return Err(e);
                    }
                    rounds += 1;
                    firsts.push(c);
                }
            }
        };
        self.records.iter().find(|x| x.record.key == key).map(|x| &x.record).ok_or(ConstFail::MissingRecord { item, key })
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
        let bits = match self.value_key(r, prepared, item, Root::Plain, None) {
            Ok(key) => {
                let row = self.records.iter().find(|x| x.record.key == key).ok_or(ConstFail::MissingRecord { item, key })?;
                int_of(&row.record)
            }
            Err(e) if !anonymous || super::const_open::needed_first(&e).is_some() => return Err(e),
            Err(e) => {
                r.store.maps.noted_const_result(c);
                r.store.maps.anon_failed.insert(c, format!("{e}").into_boxed_str());
                return Ok(());
            }
        };
        match bits {
            Some(bits) => {
                r.store.maps.file_bits(c, bits);
                if let Ok(v) = u64::try_from(bits) {
                    r.store.maps.const_lens.insert(c, v);
                }
                Ok(())
            }
            None if anonymous => {
                r.store.maps.noted_const_result(c);
                r.store.maps.anon_failed.insert(c, Box::from("its value is not an integer of at most 16 bytes"));
                Ok(())
            }
            None => Err(ConstFail::NotUsize { item }),
        }
    }

    /// One evaluation of [`Consts::value_at`]'s root: the key of its record (kept in
    /// `self.records`), or the refusal.
    fn value_key<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, root: Root, build: Option<&crate::run::ClosureTable>) -> Result<RecordKey, ConstFail> {
        let alloc = r.store.tys.alloc();
        // A body's constant: its owner's body checked for this evaluator once, under the
        // constant's key, before the evaluator's table is taken (a failure that names a
        // constant to evaluate first goes to `value_listed`'s work list).
        if let Root::Body { owner, at, .. } = root {
            if !self.bodies.iter().any(|b| b.key == item) {
                self.check_as(r, prepared, owner, item, Some(at))?;
            }
        }
        let (mut ground, mut layout, mut codes) = self.unit.take().unwrap_or_else(|| (PortableTys::new_in(alloc), Layouts::new_in(alloc), Codes::new_in(alloc)));
        let root = match root {
            Root::Plain => Ok((item, List::EMPTY)),
            Root::SelfTy(node) => self.select_root(r, item, node, &mut ground, alloc),
            Root::Args(list) | Root::Body { args: list, .. } => self.select_args(r, item, list, &mut ground, alloc),
        };
        let (item, root_args) = match root {
            Ok(root) => root,
            Err(e) => {
                self.unit = Some((ground, layout, codes));
                return Err(e);
            }
        };
        // A root ask does not go through ConstWorld::record as a nested value
        // read does. The same evaluator and ground table serve both.
        let instance = blaze_build::Instance { item, args: root_args };
        if let Some(done) = self.records.iter().find(|r| r.instance == instance) {
            r.store.maps.read_inputs(done.inputs.as_ref());
            self.unit = Some((ground, layout, codes));
            return Ok(done.record.key);
        }
        let mut settle = Settle::new(Need { kind: NeedKind::Value, item, args: root_args }, MOST_NEEDS, alloc);
        let mut last = None;
        let mut out = Ok(());
        let mut steps = 0u32;
        // The callees whose missing record (an impl's fn's signature) this evaluation made
        // (const_fn_fact.rs `answer`): each at most once.
        let mut made: Vec<ItemKey, Global> = Vec::new_in(Global);
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
                if let Err(cause) = self.check_one(r, prepared, n.item) {
                    // Leave pending needs visible to the work list. Annotate a
                    // final refusal while its item names and source are available.
                    out = Err(cause.required_by(r, item, n.item));
                    break;
                }
                if n.kind == NeedKind::Body {
                    settle.done();
                }
                continue;
            }
            let capture = r.store.maps.capture_const(alloc);
            let world = ConstsWorld { names: r.names, keys: r.keys, index: r.index, store: r.store, bodies: &self.bodies, records: &self.records, selected: core::cell::Cell::new(None), project: super::const_project::ProjectAsk::new(r.buckets.as_ref()), alloc, closures: Closures { own: self.closures.as_ref(), build } };
            let answer = value_of(&world, &mut layout, &mut codes, n.item, n.args, &mut ground, Limits::DEFAULT, alloc);
            let selected = world.selected.take();
            // A call refused by its callee's kind: the refusal carries what `fn_kind` said
            // (`None`: a head read without `const`; `Some(fact)`: a fact front2 does not hold).
            let called = match &answer {
                Err(e) => match e.refusal {
                    Refusal::NotConstFn { item: callee } => Some((callee, None)),
                    Refusal::FnFact { item: callee, fact } => Some((callee, Some(fact))),
                    _ => None,
                },
                Ok(_) => None,
            };
            let asked = world.project.into_asked();
            let inputs = r.store.maps.finish_capture(capture);
            let step = match answer {
                Ok(Answer::Done(rec)) => {
                    let key = rec.key;
                    if !self.records.iter().any(|x| x.record.key == key) {
                        self.records.push(Evaluated { instance: blaze_build::Instance { item: n.item, args: n.args }, record: rec, inputs });
                    }
                    last = Some(key);
                    settle.done();
                    Ok(())
                }
                Ok(Answer::Need(m)) => settle.need(m).map_err(|e: ConstError<'_>| match &e.refusal {
                    Refusal::Cycle { chain } => cycle_of(&*r, chain),
                    _ => ConstFail::Eval(format!("{e}").into_boxed_str()),
                }),
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
                    match (called, unknown, no_adt, no_impl) {
                        (Some((callee, None)), _, _, _) => Err(ConstFail::NotConstFn { callee, shown: super::const_fn_fact::shown(r, callee) }),
                        (Some((callee, Some(fact))), _, _, _) => match super::const_fn_fact::answer(r, &mut made, callee, fact) {
                            // The fact was made: the code holding the refusal is pending
                            // (blaze-consteval machine/late.rs `Wait::FnFact`), so the
                            // evaluator compiles it again; the same need is evaluated again.
                            Ok(()) => continue,
                            Err(e) => Err(e),
                        },
                        (None, Some(item), _, _) => Err(match r.keys.library_in(r.names, item).and_then(|lib| written_name(r.names, lib)) {
                            Some(name) => ConstFail::UnknownIntrinsic { item, name: Box::from(name) },
                            // Not reached: `free_fn_kind` answers `UnknownIntrinsic` only after
                            // reading this name; the evaluator's own text names the item and why.
                            None => ConstFail::Eval(text),
                        }),
                        (None, None, Some(adt), _) => match adt_row(r, n.item, adt) {
                            Ok(()) => continue,
                            Err(e) => Err(e),
                        },
                        (None, None, None, Some(refused)) => Err(refused.into_fail()),
                        // A projection a type of the step holds (a field's `T::Storage`): the
                        // records its normalization needs are made now and the same need is
                        // evaluated again; one the solver refuses is named
                        // (front2/const_project.rs `answer`).
                        (None, None, None, None) => match super::const_project::answer(r, n.item, asked) {
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
    /// `root` a constant inside that body ([`BodyRoot`]), that constant's key (const_at.rs
    /// `block_key` / `promoted_key`). A `root` that is not a const block, or not a borrow the
    /// check here lists as promoted, of the body as lowered here is refused by name.
    fn check_as<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, key: ItemKey, root: Option<BodyRoot>) -> Result<(), ConstFail> {
        let capture = r.store.maps.capture_const(r.store.tys.alloc());
        let result = self.checked_body(r, prepared, item, key, root);
        let inputs = r.store.maps.finish_capture(capture);
        let mut body = result?;
        body.inputs = inputs;
        self.bodies.push(body);
        Ok(())
    }

    fn checked_body<S: NamesSource<'n>>(&mut self, r: &mut Records<'_, 'n, S>, prepared: &Prepares<'n>, item: ItemKey, key: ItemKey, root: Option<BodyRoot>) -> Result<ConstBody<'n>, ConstFail> {
        // A constant declared in a body has its head made first (const_open.rs `local_head`).
        super::const_open::local_head(r, item)?;
        let lowered = match r.keys.library_in(r.names, item) {
            Some(lib) => lower_fn(r.names, prepared, r.keys, r.store, lib),
            None => lower_member(r.names, prepared, r.keys, r.store, item),
        };
        // Lowering a member may make its head. Record the defining tokens even
        // on a refusal, retaining lowering's original diagnostic when it failed.
        let source = r.store.read_const_body(r.names, r.keys, item);
        let lowered = lowered.map_err(|error| ConstFail::Body { item, error })?;
        source.map_err(|error| ConstFail::Body { item, error })?;
        let (mut checked, lits) = {
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
        let root = match root {
            None => None,
            Some(BodyRoot::Block(block)) => match lowered.typeck.exprs.get(block.0 as usize) {
                Some(tc::Expr::ConstBlock { .. }) => Some(block),
                _ => return Err(ConstFail::NotConstBlock { owner: item, block }),
            },
            // The operand is evaluated; the builder borrows its row. Only a borrow THIS check
            // promoted: one it did not is a runtime temporary, never a constant.
            Some(BodyRoot::Promoted(borrow)) => {
                if checked.promoted.binary_search_by_key(&borrow.0, |x| x.0).is_err() {
                    return Err(ConstFail::NotPromoted { owner: item, borrow });
                }
                match lowered.typeck.exprs.get(borrow.0 as usize) {
                    Some(&tc::Expr::Ref { mutbl: tc::Mutbl::Not, operand }) => Some(operand),
                    Some(_) => {
                        // A promoted shared autoref (`AdjustKind::Borrow { mutbl: Not, raw: false }`
                        // at index 0 on `borrow`): the promoted value is `borrow`'s unadjusted
                        // value before its leading borrow and any coercions after it. Strip
                        // `borrow`'s own adjustment rows from this `ConstBody`'s `checked` so the
                        // evaluator produces `borrow`'s value directly.
                        let adj_e = checked.adjusts.column::<0>();
                        let start = adj_e.partition_point(|e| e.0 < borrow.0);
                        let end = adj_e.partition_point(|e| e.0 <= borrow.0);
                        if start == end
                            || !matches!(
                                checked.adjusts.column::<1>().get(start),
                                Some(tc::AdjustKind::Borrow { mutbl: tc::Mutbl::Not, raw: false, .. })
                            )
                        {
                            return Err(ConstFail::NotPromoted { owner: item, borrow });
                        }
                        let mut stripped = blaze_own::Soa::new_in(r.store.tys.alloc());
                        for (i, ((&e, &k), &t)) in checked
                            .adjusts
                            .column::<0>()
                            .iter()
                            .zip(checked.adjusts.column::<1>().iter())
                            .zip(checked.adjusts.column::<2>().iter())
                            .enumerate()
                        {
                            if i < start || i >= end {
                                stripped.push((e, k, t));
                            }
                        }
                        checked.adjusts = stripped;
                        Some(borrow)
                    }
                    None => return Err(ConstFail::NotPromoted { owner: item, borrow }),
                }
            }
        };
        let text = match (r.store.locals.head(item), r.keys.library_in(r.names, item)) {
            // An item declared in a body: its kept text (the names library holds none for
            // it; body.rs `lower_fn_into` lowered it from the same text).
            (Some(local), _) => Some(local.text),
            (None, Some(lib)) => r.names.text_of(lib).map(|t| t.text),
            (None, None) => r.store.fns.get(&item).map(|f| super::tables::head::member_definition(r.names, r.keys, item, f)),
        }
        .ok_or(ConstFail::Body { item, error: BodyError::NoHead(item) })?;
        // Its closures' facts, by the build's one rule (`run::ClosureTable::add`): a closure
        // written in a constant's body is laid out from the body that holds it, as a fn's is.
        // Its owner's parameters: the head's (a free constant has none and no row).
        if !checked.closures.is_empty() {
            let alloc = r.store.tys.alloc();
            let generics = r.store.fns.get(&item).map_or(0, |h| h.parent + h.own);
            let tys = lowered.tys.view(&r.store.tys).expect("a body's types over the records' table it was lowered over");
            let view = lowered.typeck.view(tys);
            self.closures.get_or_insert_with(|| crate::run::ClosureTable::new(alloc)).add(&view, &checked, generics, alloc);
        }
        Ok(ConstBody { key, inputs: None, text, hash: hash_text(text), typeck: lowered.typeck, tys: lowered.tys, checked, lits, root })
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
