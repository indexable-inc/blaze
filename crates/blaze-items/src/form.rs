//! An item's normalized form: what is hashed of it, with every item it refers to a slot.
//!
//! A form says what an item is to the program: its types, bounds, attributes that reach the
//! code or the object file, visibility, and for a body the MIR the code generator is given. It
//! leaves out what the program cannot observe: names (but where they are linked against or a
//! type's name is asked for), the order where clauses or `dyn` bounds are written in, trivia,
//! and the positions of code, which go to [`Code::spans`] instead. Every other item it mentions
//! is a slot, numbered in the order the form first meets it, for [`crate::hash`] to fill.
//!
//! Dense ids (query database ids, arena indexes across items) are never hashed. Inside one body the
//! blocks and locals are numbered by a walk from the start block, and expression ids, binding
//! ids and closure positions are positions in that body's own store.

use std::{
    cell::{Cell, RefCell},
    fmt::{Debug, Write as _},
    hash::BuildHasher,
    mem,
    sync::LazyLock,
};

use base_db::{Crate, Db};
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_ids::{
    ContentId,
    bits::{self, Bits},
    radix,
};
use blaze_mem::{Frame, Tag, frame};
use hir_def::{
    AdtId, AssocItemId, AttrDefId, CallableDefId, DefWithBodyId, ExpressionStoreOwnerId, FieldId,
    FunctionId, GenericDefId, GenericParamId, HasModule, ItemContainerId, LocalFieldId, Lookup,
    MacroId, ModuleDefId, ModuleId, ModuleIdLt, TraitId, VariantId,
    attrs::AttrFlags,
    hir::{ClosureKind, generics::GenericParamDataRef},
    item_tree::FieldsShape,
    nameres::crate_def_map,
    signatures::{
        ConstSignature, EnumSignature, FnFlags, FunctionSignature, ImplSignature, StaticFlags,
        StaticSignature, StructSignature, TraitFlags, TraitSignature, TypeAliasSignature,
        UnionSignature, VariantFields,
    },
    visibility::{Visibility, written_visibility},
};
use hir_expand::proc_macro::ProcMacroKind;
use hir_ty::{
    GenericPredicates, ImplTraitId, InferBodyId, InferenceResult, MemoryMap, SelfAndAssocBounds,
    TyDefId,
    closure_analysis::{CapturedPlace, PlaceBase, ProjectionKind, UpvarCapture},
    db::{
        ClosureKey, HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId,
        InternedOpaqueTyId,
    },
    intrinsic::{Intrinsic, intrinsic_of},
    method_resolution::loaded_crates,
    mir::{
        AggregateKind, AsmOperand, AssertKind, BasicBlockId, LocalId, MirBody, MirSpan, Operand,
        OperandKind, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
        is_by_move_coroutine,
    },
    next_solver::{
        AliasConstKind, Allocation, Binder, BoundRegion, Clause, Const, ConstKind, DbInterner,
        FnSig, GenericArgs, ImplTraitIdx, OpaqueTyIdWrapper, Pattern, Region, StoredTy, Term,
        TermId, TraitRef, Ty, TyKind, ValTree,
    },
    rpit_hidden_types,
    traits::{ParamEnvAndCrate, implements_traits_unique},
};
use intern::Symbol;
use rustc_abi::ReprOptions;
use rustc_type_ir::{
    AliasTermKind, AliasTyKind, BoundRegionKind, BoundTyKind, BoundVarIndexKind, ClauseKind,
    ExistentialPredicate, GenericArgKind, InferConst, InferTy, Interner as _, OutlivesClause,
    PatternKind, RegionKind, TermKind, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    ItemId, ItemKey,
    key::{self, generic_def_item, item_key},
};

const DOMAIN: &str = "blaze v2 form 2";
const SPANS_DOMAIN: &str = "blaze v2 form spans 1";

/// Why a form is not of a program rustc accepts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Missed {
    ErrorType,
    ErrorConst,
    NoMir,
    NoRules,
    ImplItemNotInTrait,
    VisibilityOutside,
    ProcMacroWithoutFn,
}

impl Missed {
    /// What is hashed of it, and what the error says.
    pub(crate) fn what(self) -> &'static str {
        match self {
            Missed::ErrorType => "a type with an error",
            Missed::ErrorConst => "a constant with an error",
            Missed::NoMir => "MIR that failed to lower",
            Missed::NoRules => "a macro without rules",
            Missed::ImplItemNotInTrait => "an impl item its trait does not have",
            Missed::VisibilityOutside => "a visibility of a module the item is not in",
            Missed::ProcMacroWithoutFn => "a procedural macro without its function",
        }
    }
}

/// A form, hashed: what every hash of an item reads of it. Its positions are apart, in
/// [`Code::spans`].
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Sub {
    pub(crate) digest: ContentId,
    /// Distinct, in slot order: slot 0 is `refs[0]`.
    pub(crate) refs: Box<[ItemId]>,
    /// Set only when the program is not one rustc accepts.
    pub(crate) missed: Option<Missed>,
}

const _: () = assert!(size_of::<Sub>() == 56);

/// A body's form and the hash of the positions (MirSpans) of its code, in order: read by what
/// fills positions in, never by a hash of the item.
#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Code {
    pub(crate) sub: Sub,
    pub(crate) spans: ContentId,
}

const _: () = assert!(size_of::<Code>() == 88);

#[cfg(test)]
impl Code {
    /// A copy, for [`Cx::reference_ty`]'s cache.
    fn copied(&self) -> Code {
        let Sub { digest, ref refs, missed } = self.sub;
        Code { sub: Sub { digest, refs: refs.clone(), missed }, spans: self.spans }
    }
}

/// The spans hash of a form with no positions, which most are.
static NO_SPANS: LazyLock<ContentId> = LazyLock::new(|| ContentId::of(SPANS_DOMAIN, &[]));

/// The preimages of a form's digest and spans, hashed when it is finished: whole, or many
/// small ones in one batched call ([`Cx::set`]). Its buffers come from the worker's [`SPARE`]
/// and go back there when it is dropped, so a form allocates only when it outgrows them.
struct Form {
    digest: Vec<u8>,
    spans: Vec<u8>,
    /// Distinct, in slot order.
    refs: Vec<ItemId>,
    /// The slot of each of `refs`, filled once there are more than [`SCAN`].
    slots: HashMap<ItemId, u32>,
    missed: Option<Missed>,
    /// The first walk of a body, which only finds the locals it uses: nothing is written, and
    /// what names no local (types, constants, allocations, variants) is not spelled.
    quiet: bool,
}

/// Up to this many refs a slot is found by scanning them, which beats hashing.
const SCAN: usize = 8;

#[derive(Default)]
struct FormBuffers {
    digest: Vec<u8>,
    spans: Vec<u8>,
    refs: Vec<ItemId>,
    slots: HashMap<ItemId, u32>,
}

/// What one worker keeps of finished forms and walks to write the next ones into: at most
/// [`SPARE_COUNT`] of each, none of more than [`SPARE_BYTES`]. Per thread, so taking and
/// giving back is a borrow flag, never a lock or a shared write.
struct Spare {
    forms: Vec<FormBuffers>,
    canons: Vec<CanonBuffers>,
}

const SPARE_COUNT: usize = 64;
const SPARE_BYTES: usize = 1 << 20;

thread_local! {
    static SPARE: RefCell<Spare> =
        // alloc: none: empty until forms are given back, then at most `SPARE_COUNT` deep.
        const { RefCell::new(Spare { forms: Vec::new(), canons: Vec::new() }) };
}

impl Form {
    fn new(tag: &str) -> Form {
        let FormBuffers { digest, spans, refs, slots } = SPARE
            .try_with(|spare| spare.try_borrow_mut().ok().and_then(|mut it| it.forms.pop()))
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut form = Form { digest, spans, refs, slots, missed: None, quiet: false };
        form.str(tag);
        form
    }

    fn quiet() -> Form {
        let FormBuffers { digest, spans, refs, slots } = FormBuffers::default();
        Form { digest, spans, refs, slots, missed: None, quiet: true }
    }

    fn update(&mut self, bytes: &[u8]) {
        if !self.quiet {
            self.digest.extend_from_slice(bytes);
        }
    }

    fn u8(&mut self, value: u8) {
        self.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.update(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.update(&value.to_le_bytes());
    }

    fn u128(&mut self, value: u128) {
        self.update(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.u64(len as u64);
    }

    /// A length to be written by [`Form::set_len`] once what it counts is: the bytes
    /// [`Form::len`] before them writes, without collecting them first.
    fn len_at(&mut self) -> usize {
        let at = self.digest.len();
        self.u64(0);
        at
    }

    fn set_len(&mut self, at: usize, len: usize) {
        if !self.quiet {
            self.digest[at..at + 8].copy_from_slice(&(len as u64).to_le_bytes());
        }
    }

    fn bool(&mut self, value: bool) {
        self.u8(value as u8);
    }

    /// Length-prefixed, so `("ab", "c")` and `("a", "bc")` hash apart.
    fn bytes(&mut self, bytes: &[u8]) {
        self.u64(bytes.len() as u64);
        self.update(bytes);
    }

    fn str(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    fn symbol(&mut self, symbol: Option<Symbol>) {
        match symbol {
            Some(it) => {
                self.u8(1);
                self.str(it.as_str());
            }
            None => self.u8(0),
        }
    }

    /// For what holds no id: what one build of blaze writes of it is the same in every run, and
    /// the store's keys hash what blaze was built from.
    fn debug<T: Debug + ?Sized>(&mut self, value: &T) {
        if self.quiet {
            return;
        }
        let at = self.len_at();
        write!(Text(&mut self.digest), "{value:?}").expect("`Debug` into a `Vec` does not fail");
        let len = self.digest.len() - at - 8;
        self.set_len(at, len);
    }

    /// The slot of `id`: the first item referred to is 0, the next other one 1.
    fn item(&mut self, id: ItemId) {
        if self.quiet {
            return;
        }
        let slot = if self.refs.len() <= SCAN {
            match self.refs.iter().position(|&it| it == id) {
                Some(at) => at as u32,
                None => {
                    self.refs.push(id);
                    (self.refs.len() - 1) as u32
                }
            }
        } else {
            if self.slots.is_empty() {
                self.slots.extend(self.refs.iter().enumerate().map(|(at, &it)| (it, at as u32)));
            }
            let next = self.refs.len() as u32;
            let slot = *self.slots.entry(id).or_insert(next);
            if slot == next {
                self.refs.push(id);
            }
            slot
        };
        self.u32(slot);
    }

    /// A finished form written into this one: its digest and positions, and its refs as slots
    /// of this one.
    fn merge(
        &mut self,
        digest: &ContentId,
        spans: &ContentId,
        refs: &[ItemId],
        missed: Option<Missed>,
    ) {
        if self.quiet {
            return;
        }
        self.update(&digest.0);
        self.spans.extend_from_slice(&spans.0);
        for &id in refs {
            self.item(id);
        }
        if let Some(why) = missed {
            self.missed.get_or_insert(why);
        }
    }

    fn error(&mut self, why: Missed) {
        self.missed.get_or_insert(why);
        self.u8(0xff);
        self.str(why.what());
    }

    fn span(&mut self, span: MirSpan) {
        if self.quiet {
            return;
        }
        let (tag, at) = match span {
            MirSpan::ExprId(it) => (0u8, it.into_raw().into_u32()),
            MirSpan::ExprEnd(it) => (1, it.into_raw().into_u32()),
            MirSpan::PatId(it) => (2, it.into_raw().into_u32()),
            MirSpan::BindingId(it) => (3, it.into_raw().into_u32()),
            MirSpan::SelfParam => (4, 0),
            MirSpan::Unknown => (5, 0),
            MirSpan::Inlined(it) => (6, it),
        };
        self.spans.push(tag);
        self.spans.extend_from_slice(&at.to_le_bytes());
    }

    /// An operand's position, if it has one.
    fn operand_span(&mut self, span: Option<MirSpan>) {
        if self.quiet {
            return;
        }
        match span {
            Some(span) => {
                self.spans.push(1);
                self.span(span);
            }
            None => self.spans.push(0),
        }
    }

    fn digest_id(&self) -> ContentId {
        ContentId::of(DOMAIN, &self.digest)
    }

    fn spans_id(&self) -> ContentId {
        if self.spans.is_empty() { *NO_SPANS } else { ContentId::of(SPANS_DOMAIN, &self.spans) }
    }

    fn sub(self) -> Sub {
        Sub { digest: self.digest_id(), refs: self.refs.as_slice().into(), missed: self.missed }
    }

    fn code(self) -> Code {
        let spans = self.spans_id();
        Code { sub: self.sub(), spans }
    }
}

impl Drop for Form {
    fn drop(&mut self) {
        if self.quiet
            || self.digest.capacity() > SPARE_BYTES
            || self.spans.capacity() > SPARE_BYTES
            || self.refs.capacity() * size_of::<ItemId>() > SPARE_BYTES
        {
            return;
        }
        let mut buffers = FormBuffers {
            digest: mem::take(&mut self.digest),
            spans: mem::take(&mut self.spans),
            refs: mem::take(&mut self.refs),
            slots: mem::take(&mut self.slots),
        };
        buffers.digest.clear();
        buffers.spans.clear();
        buffers.refs.clear();
        buffers.slots.clear();
        let _ = SPARE.try_with(|spare| {
            if let Ok(mut spare) = spare.try_borrow_mut()
                && spare.forms.len() < SPARE_COUNT
            {
                spare.forms.push(buffers);
            }
        });
    }
}

/// Text written into a preimage as it is formatted.
struct Text<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Text<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// A body written inside another: a closure or a coroutine, whose code is part of its owner's.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Nested<'db> {
    Closure(InternedClosureId<'db>),
    Coroutine(InternedCoroutineId<'db>),
}

/// The bodies written in one, in the order the walks first meet them.
#[derive(Default)]
struct Nests<'db> {
    order: Vec<Nested<'db>>,
    seen: HashSet<Nested<'db>>,
}

impl<'db> Nests<'db> {
    fn push(&mut self, it: Nested<'db>) {
        if self.seen.insert(it) {
            self.order.push(it);
        }
    }
}

/// Entries of [`Cx::seen`].
const SEEN: usize = 64;

/// 16 bytes an entry, 1 KiB a [`Cx`].
const _: () = assert!(size_of::<Option<(Ty<'static>, &'static Code)>>() == 16);

pub(crate) struct Cx<'db> {
    db: &'db Db,
    /// The type forms asked of [`ty_form`] lately, direct-mapped by the type's hash: one probe
    /// and no allocation, against the engine's interning of the key and its memo lookup. A `Cx` lives inside one query, so a hit skips only a read that query recorded.
    seen: [Cell<Option<(Ty<'db>, &'db Code)>>; SEEN],
    /// Writing signature forms ([`Cx::leak_free`]): [`LEAK`] in place of each leak.
    leak_free: bool,
    #[cfg(test)]
    reference: Option<Reference<'db>>,
}

/// A type as [`ty_form`] and [`signature_ty_form`] keep it, interned: a tracked function's one
/// argument is an ID.
#[blaze_query::interned(debug)]
pub(crate) struct FormTy<'db> {
    #[returns(copy)]
    pub(crate) ty: StoredTy,
}

/// A type's form, spelled once per revision for every item that writes it and kept across
/// revisions while what it reads is unchanged: rustc's stable hash cached in each interned
/// type, with the query engine's backdating on top.
#[blaze_query::tracked(returns(ref), cycle_result = ty_form_cycle)]
pub(crate) fn ty_form<'db>(db: &'db Db, ty: FormTy<'db>) -> Code {
    let mut form = Form::new("ty");
    Cx::new(db).spell(&mut form, ty.ty(db).as_ref());
    form.code()
}

/// A type met again while it is spelled contains itself, which only a program rustc rejects
/// has: every type of the cycle gets this, whichever of them was asked for first.
fn ty_form_cycle<'db>(_: &'db Db, _: blaze_query::db::Id, _: FormTy<'db>) -> Code {
    let mut form = Form::new("ty");
    form.error(Missed::ErrorType);
    form.code()
}

/// A type's form as a signature form writes it: [`ty_form`]'s, with [`LEAK`] in place of a
/// closure's captures and a coroutine's held locals, which are of the inference and MIR of
/// the body the closure is written in.
#[blaze_query::tracked(returns(ref), cycle_result = ty_form_cycle)]
pub(crate) fn signature_ty_form<'db>(db: &'db Db, ty: FormTy<'db>) -> Code {
    let mut form = Form::new("ty");
    Cx::leak_free(db).spell(&mut form, ty.ty(db).as_ref());
    form.code()
}

/// What a signature form writes in place of what a body leaks into an interface.
const LEAK: u8 = 0xfe;

/// The type forms as each `Cx` spelled them before [`ty_form`]: memoized for its lifetime,
/// with a stack of the types being spelled to stop at one that contains itself. The tests
/// check that both give the same forms.
#[cfg(test)]
#[derive(Default)]
struct Reference<'db> {
    tys: RefCell<HashMap<Ty<'db>, Code>>,
    spelling: RefCell<Vec<Ty<'db>>>,
}

/// The attributes of documentation, lints, diagnostics, completion and the IDE: nothing of the
/// program changes with them. Every other flag is hashed.
fn unobserved() -> AttrFlags {
    AttrFlags::RUST_ANALYZER_SKIP
        | AttrFlags::HAS_DOC_ALIASES
        | AttrFlags::HAS_DOC_KEYWORD
        | AttrFlags::IS_DOC_NOTABLE_TRAIT
        | AttrFlags::IS_DOC_HIDDEN
        | AttrFlags::IS_DEPRECATED
        | AttrFlags::HAS_CFG
        | AttrFlags::COMPLETE_IGNORE_FLYIMPORT
        | AttrFlags::COMPLETE_IGNORE_FLYIMPORT_METHODS
        | AttrFlags::COMPLETE_IGNORE_METHODS
        | AttrFlags::MACRO_STYLE_BRACES
        | AttrFlags::MACRO_STYLE_BRACKETS
        | AttrFlags::MACRO_STYLE_PARENTHESES
        | AttrFlags::PREFER_UNDERSCORE_IMPORT
        | AttrFlags::IS_MUST_USE
        | AttrFlags::DIAGNOSTIC_DO_NOT_RECOMMEND
        | AttrFlags::HAS_RUSTC_MUST_IMPLEMENT_ONE_OF
}

/// The auto traits a crate declares, by key.
#[blaze_query::tracked(returns(ref))]
fn crate_auto_traits(db: &Db, krate: Crate) -> Box<[(ItemKey, TraitId)]> {
    // alloc: the memo's own list, its length known once the crate is walked.
    let mut traits = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::TraitId(it) = def
                && TraitSignature::of(db, it).flags.contains(TraitFlags::AUTO)
            {
                traits.push((item_key(db, ItemId::Trait(it)), it));
            }
        }
    }
    traits.into_boxed_slice()
}

/// The auto traits of the crates `krate` loads, in the order of their keys.
#[blaze_query::tracked(returns(ref))]
fn loaded_auto_traits(db: &Db, krate: Crate) -> Box<[TraitId]> {
    frame::task(Tag::Other, |frame| {
        let mut traits: Vec<(ItemKey, TraitId), &Frame> = Vec::new_in(frame);
        for &it in loaded_crates(db, krate).iter() {
            traits.extend_from_slice(crate_auto_traits(db, it));
        }
        traits.sort_unstable();
        traits.dedup();
        crate::table::column(traits.iter().map(|&(_, it)| it))
    })
}

/// What an item's visibility is resolved to, once per item.
#[blaze_query::tracked(returns(copy))]
fn item_visibility(db: &Db, id: ItemId) -> Visibility {
    item_visibility_uncached(db, id)
}

impl<'db> Cx<'db> {
    pub(crate) fn new(db: &'db Db) -> Cx<'db> {
        Cx {
            db,
            seen: [const { Cell::new(None) }; SEEN],
            leak_free: false,
            #[cfg(test)]
            reference: None,
        }
    }

    /// A `Cx` that writes signature forms: an interface form with [`LEAK`] in place of each
    /// leak of a body (a `const fn`'s, a constant's, an immutable static's and an explicit
    /// discriminant's body, an opaque type's hidden type and auto traits, a closure's captures,
    /// a coroutine's held locals), so that writing one infers no body.
    pub(crate) fn leak_free(db: &'db Db) -> Cx<'db> {
        Cx { leak_free: true, ..Cx::new(db) }
    }

    /// A `Cx` that spells types itself, as every `Cx` did before [`ty_form`].
    #[cfg(test)]
    pub(crate) fn reference(db: &'db Db) -> Cx<'db> {
        Cx { reference: Some(Reference::default()), ..Cx::new(db) }
    }

    /// `ty`'s form as a [`Cx::reference`] spells it.
    #[cfg(test)]
    pub(crate) fn reference_ty(&self, ty: Ty<'db>) -> Code {
        let reference = self.reference.as_ref().expect("a reference `Cx`");
        if let Some(known) = reference.tys.borrow().get(&ty) {
            return known.copied();
        }
        let mut form = Form::new("ty");
        if reference.spelling.borrow().contains(&ty) {
            form.error(Missed::ErrorType);
            return form.code();
        }
        reference.spelling.borrow_mut().push(ty);
        self.spell(&mut form, ty);
        reference.spelling.borrow_mut().pop();
        let code = form.code();
        reference.tys.borrow_mut().insert(ty, code.copied());
        code
    }

    /// `form` finished and written into `f`.
    fn nest(&self, f: &mut Form, form: Form) {
        f.merge(&form.digest_id(), &form.spans_id(), &form.refs, form.missed);
    }

    /// Forms as a set, in an order no order they are written in changes: by digest, then by
    /// the keys of what they refer to. Most sets have one form or none, which need no sorting
    /// and no batch.
    fn set(&self, f: &mut Form, mut forms: impl Iterator<Item = Form>) {
        let Some(first) = forms.next() else { return f.len(0) };
        let Some(second) = forms.next() else {
            f.len(1);
            return self.nest(f, first);
        };
        // alloc: a set of two forms or more; spelling nests as types do, deeper than frames.
        let forms: Vec<Form> = [first, second].into_iter().chain(forms).collect();
        // alloc: as above.
        let digests: Vec<&[u8]> = forms.iter().map(|it| it.digest.as_slice()).collect();
        let digests = ContentId::of_many(DOMAIN, &digests);
        let positioned: Vec<&[u8]> = forms
            .iter()
            .filter(|it| !it.spans.is_empty())
            .map(|it| it.spans.as_slice())
            // alloc: as above.
            .collect();
        let mut positioned = ContentId::of_many(SPANS_DOMAIN, &positioned).into_iter();
        let mut finished: Vec<(ContentId, ContentId, &Form)> = forms
            .iter()
            .zip(digests)
            .map(|(form, digest)| {
                let spans = if form.spans.is_empty() {
                    *NO_SPANS
                } else {
                    positioned.next().expect("a spans hash per form with positions")
                };
                (digest, spans, form)
            })
            // alloc: as above.
            .collect();
        let db = self.db;
        finished.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                let a = a.2.refs.iter().map(|&it| item_key(db, it));
                a.cmp(b.2.refs.iter().map(|&it| item_key(db, it)))
            })
        });
        f.len(finished.len());
        for (digest, spans, form) in finished {
            f.merge(&digest, &spans, &form.refs, form.missed);
        }
    }

    /// What the item's dependents see of it.
    pub(crate) fn interface(&self, id: ItemId) -> Sub {
        self.item_form("interface", id)
    }

    /// The interface without what bodies leak into it; asked of a [`Cx::leak_free`].
    pub(crate) fn signature(&self, id: ItemId) -> Sub {
        debug_assert!(self.leak_free, "a signature form is written by a leak-free `Cx`");
        self.item_form("signature", id)
    }

    fn item_form(&self, tag: &str, id: ItemId) -> Sub {
        let mut form = Form::new(tag);
        let f = &mut form;
        match id {
            ItemId::Function(it) => {
                f.u8(0);
                self.function(f, it);
            }
            ItemId::Adt(it) => {
                f.u8(1);
                self.adt(f, it);
            }
            ItemId::Trait(it) => {
                f.u8(2);
                self.trait_(f, it);
            }
            ItemId::Impl(it) => {
                f.u8(3);
                self.impl_(f, it);
            }
            ItemId::Const(it) => {
                f.u8(4);
                self.konst_item(f, it);
            }
            ItemId::Static(it) => {
                f.u8(5);
                self.static_(f, it);
            }
            ItemId::Alias(it) => {
                f.u8(6);
                self.alias(f, it);
            }
            ItemId::Macro(it) => {
                f.u8(7);
                self.macro_(f, it);
            }
        }
        form.sub()
    }

    /// The MIR of a function, static or constant with the closures and coroutines written in
    /// it; `None` for an item with no body.
    pub(crate) fn body(&self, id: ItemId) -> Option<Code> {
        let def = self.body_owner(id)?;
        if let DefWithBodyId::ConstId(it) = def
            && let Some(value) = self.const_value(it)
        {
            let mut f = Form::new("const value");
            self.allocation(&mut f, value);
            return Some(f.code());
        }
        Some(self.code_form(InferBodyId::DefWithBodyId(def)).code())
    }

    /// The value of constant `it` when it is one for every use: it has no generics, its own or
    /// its container's, and evaluates. As [`crate::facts`]' `ConstValue` reads it.
    fn const_value(&self, it: hir_def::ConstId) -> Option<Allocation<'db>> {
        let db = self.db;
        if hir_ty::generics::generics(db, it.into()).iter(true).next().is_some() {
            return None;
        }
        db.const_eval(it, GenericArgs::empty(), None).ok()
    }

    /// Why the MIR [`Cx::body`] reads of `id` failed to lower: the first error of its body, or of
    /// a closure or coroutine written in it, in the order `body` meets them. For the error a
    /// build stops with at [`Missed::NoMir`]; `None` when each lowers (the MIR that failed is
    /// elsewhere in the item, an anonymous constant of a type).
    pub(crate) fn mir_error(&self, id: ItemId) -> Option<String> {
        let db = self.db;
        let def = self.body_owner(id)?;
        let body = match db.mir_body(InferBodyId::DefWithBodyId(def)) {
            Ok(body) => body,
            // alloc: the error a build stops with, once
            Err(error) => return Some(format!("{error:?}")),
        };
        let mut scratch = Form::quiet();
        let mut nested = Nests::default();
        self.mir(&mut scratch, body, &mut nested);
        let mut at = 0;
        while at < nested.order.len() {
            let next = nested.order[at];
            at += 1;
            let (what, body) = match next {
                Nested::Closure(it) => ("a closure", db.mir_body_for_closure(it)),
                Nested::Coroutine(it) => ("a coroutine", db.mir_body_for_coroutine(it)),
            };
            match body {
                Ok(body) => self.mir(&mut scratch, body, &mut nested),
                // alloc: as above
                Err(error) => return Some(format!("{what} in it: {error:?}")),
            }
        }
        None
    }

    /// The body `id` has, if it has one.
    fn body_owner(&self, id: ItemId) -> Option<DefWithBodyId> {
        let db = self.db;
        Some(match id {
            ItemId::Function(it) if FunctionSignature::of(db, it).has_body() => {
                DefWithBodyId::from(it)
            }
            ItemId::Const(it) if ConstSignature::of(db, it).has_body() => it.into(),
            ItemId::Static(it)
                if StaticSignature::of(db, it).flags.contains(StaticFlags::HAS_BODY) =>
            {
                it.into()
            }
            ItemId::Function(_)
            | ItemId::Const(_)
            | ItemId::Static(_)
            | ItemId::Adt(_)
            | ItemId::Trait(_)
            | ItemId::Impl(_)
            | ItemId::Alias(_)
            | ItemId::Macro(_) => return None,
        })
    }

    /// One closure's code alone, for it to be built alone.
    pub(crate) fn closure_code(&self, closure: InternedClosureId<'db>) -> Code {
        let mut f = Form::new("closure code");
        match self.db.mir_body_for_closure(closure) {
            Ok(body) => self.mir(&mut f, body, &mut Nests::default()),
            Err(_) => f.error(Missed::NoMir),
        }
        f.code()
    }

    /// One coroutine's code alone.
    pub(crate) fn coroutine_code(&self, coroutine: InternedCoroutineId<'db>) -> Code {
        let mut f = Form::new("coroutine code");
        match self.db.mir_body_for_coroutine(coroutine) {
            Ok(body) => self.mir(&mut f, body, &mut Nests::default()),
            Err(_) => f.error(Missed::NoMir),
        }
        f.code()
    }

    /// What a closure captures and how, and the signature inference gave it.
    pub(crate) fn captures(&self, closure: InternedClosureId<'db>) -> Sub {
        self.captures_of(closure.loc(self.db)).sub()
    }

    /// For each auto trait of the crates the owner's crate loads, in the order of their keys,
    /// whether the hidden type of the owner's `index`th opaque type implements it where the
    /// owner is written: what a caller learns of an `impl Trait` beyond its bounds.
    pub(crate) fn auto_traits(&self, owner: ItemId, index: u32) -> Sub {
        self.auto_traits_form(owner, index).sub()
    }

    /// The hidden type of each `impl Trait` of a function's return type or of a type alias, in
    /// order, `0` for one its defining bodies gave none: what `rpit_hidden_types` and
    /// `tait_hidden_types` give. Asked of a [`Cx::leak_free`], so that writing it infers no
    /// body but the defining ones.
    pub(crate) fn hidden_types(&self, owner: ItemId) -> Sub {
        debug_assert!(self.leak_free, "hidden types are written by a leak-free `Cx`");
        let db = self.db;
        let count = match owner {
            ItemId::Function(it) => SelfAndAssocBounds::return_type_impl_traits(db, it).len(),
            ItemId::Alias(it) => SelfAndAssocBounds::type_alias_opaque_count(db, it),
            ItemId::Adt(_)
            | ItemId::Trait(_)
            | ItemId::Impl(_)
            | ItemId::Const(_)
            | ItemId::Static(_)
            | ItemId::Macro(_) => {
                panic!("blaze-items: hidden types asked of {owner:?}, which has no opaque types")
            }
        };
        let mut f = Form::new("hidden types");
        f.len(count);
        for index in 0..count as u32 {
            // A caller that sees through the opaque then reads impls and methods by the hidden
            // type's items' keys, so the hidden type is by name as well as by content.
            match self.hidden(owner, index) {
                Some(hidden) => {
                    f.u8(1);
                    self.ty_in(&mut f, hidden);
                    self.names_of_ty(&mut f, hidden);
                }
                None => f.u8(0),
            }
        }
        f.sub()
    }

    fn auto_traits_form(&self, owner: ItemId, index: u32) -> Form {
        let db = self.db;
        let def = match owner {
            ItemId::Function(it) => GenericDefId::from(it),
            ItemId::Alias(it) => GenericDefId::from(it),
            ItemId::Adt(_)
            | ItemId::Trait(_)
            | ItemId::Impl(_)
            | ItemId::Const(_)
            | ItemId::Static(_)
            | ItemId::Macro(_) => {
                panic!("blaze-items: auto traits asked of {owner:?}, which has no opaque types")
            }
        };
        let mut f = Form::new("auto traits");
        let Some(hidden) = self.hidden(owner, index) else {
            f.error(Missed::ErrorType);
            return f;
        };
        // Where the opaque has a type or const parameter, whether an auto trait holds for it at
        // given arguments (`Vec<T>` or `Rc<T>` at `T = u8`) is more than whether it holds at the
        // parameters: the hidden type itself is the fact, by name as well as by content, as an
        // auto trait's impls are of a type by its key. Lifetimes change no answer, as an auto
        // trait's impls are fully generic.
        let generic = hir_ty::generics::generics(db, def)
            .iter(false)
            .any(|(id, _)| !matches!(id, GenericParamId::LifetimeParamId(_)));
        f.bool(generic);
        if generic {
            self.ty_in(&mut f, hidden);
            self.names_of_ty(&mut f, hidden);
        }
        let krate = owner.krate(db);
        let traits = loaded_auto_traits(db, krate);
        f.len(traits.len());
        let env = ParamEnvAndCrate { param_env: db.trait_environment(def), krate };
        implements_traits_unique(hidden, db, env, traits, |it, holds| {
            f.item(ItemId::Trait(it));
            f.bool(holds);
        });
        f
    }

    /// A field: its type, default, visibility and attributes.
    pub(crate) fn field(&self, variant: VariantId, field: LocalFieldId) -> Sub {
        self.field_form(variant, field).sub()
    }

    fn field_form(&self, variant: VariantId, field: LocalFieldId) -> Form {
        let db = self.db;
        let mut form = Form::new("field");
        let f = &mut form;
        match db.field_types(variant).get(field) {
            Some(it) => {
                self.ty_in(f, it.ty().skip_binder());
                match it.default() {
                    Some(default) => {
                        f.u8(1);
                        self.konst(f, default.skip_binder());
                    }
                    None => f.u8(0),
                }
            }
            None => f.error(Missed::ErrorType),
        }
        f.bool(variant.fields(db).fields()[field].is_unsafe);
        match VariantFields::field_visibilities(db, variant).get(field) {
            Some(&vis) => self.visibility(f, variant_module(db, variant), vis),
            None => f.error(Missed::ErrorType),
        }
        let flags = AttrFlags::query_field(db, FieldId { parent: variant, local_id: field });
        f.u128(flags.difference(unobserved()).bits());
        form
    }

    /// A type: its digest and slots are the same wherever it is written.
    pub(crate) fn ty(&self, ty: Ty<'db>) -> &'db Sub {
        &self.ty_code(ty).sub
    }

    fn ty_code(&self, ty: Ty<'db>) -> &'db Code {
        let entry = &self.seen[FixedState.hash_one(ty) as usize % SEEN];
        if let Some((seen, code)) = entry.get()
            && seen == ty
        {
            return code;
        }
        let key = FormTy::new(self.db, ty.store());
        let code = if self.leak_free {
            signature_ty_form(self.db, key)
        } else {
            ty_form(self.db, key)
        };
        entry.set(Some((ty, code)));
        code
    }

    fn ty_in(&self, f: &mut Form, ty: Ty<'db>) {
        if f.quiet {
            return;
        }
        #[cfg(test)]
        if self.reference.is_some() {
            let code = self.reference_ty(ty);
            return f.merge(&code.sub.digest, &code.spans, &code.sub.refs, code.sub.missed);
        }
        let code = self.ty_code(ty);
        f.merge(&code.sub.digest, &code.spans, &code.sub.refs, code.sub.missed);
    }

    fn function(&self, f: &mut Form, func: FunctionId) {
        let db = self.db;
        let loc = func.lookup(db);
        self.container(f, loc.container);
        self.visibility(f, func.module(db), AssocItemId::FunctionId(func).assoc_visibility(db));
        self.generics(f, func.into());
        let sig = db.callable_item_signature(func.into()).skip_binder();
        f.len(sig.bound_vars().len());
        self.fn_sig(f, sig.skip_binder());
        let data = FunctionSignature::of(db, func);
        f.u64(u64::from(data.flags.bits()));
        let flags = self.attrs(f, func.into());
        let features = AttrFlags::target_feature_list(db, func);
        f.len(features.len());
        for feature in &features {
            f.str(feature.as_str());
        }
        // Inherited from the impl, module and crate, so it changes with no change to the
        // function's own attributes.
        f.u8(hir_def::attrs::disabled_sanitizers(db, func));
        f.bool(flags.contains(AttrFlags::RUSTC_NOUNWIND));
        // An intrinsic's name is what calling it does, and codegen expands it into its
        // callers' code: `simd_lt` and `simd_ge` share a signature.
        let named = flags.intersects(AttrFlags::NO_MANGLE | AttrFlags::LINKED_BY_NAME)
            || matches!(loc.container, ItemContainerId::ExternBlockId(_))
            || FunctionSignature::is_intrinsic(db, func);
        self.linked_name(f, named, data.name.as_str());
        // A `const fn`'s body is what its callers' constants evaluate.
        if data.flags.contains(FnFlags::CONST) && data.has_body() {
            if self.leak_free {
                f.u8(LEAK);
            } else {
                f.u8(1);
                self.nest(f, self.code_form(InferBodyId::DefWithBodyId(func.into())));
            }
        } else {
            f.u8(0);
        }
        let count = SelfAndAssocBounds::return_type_impl_traits(db, func).len();
        f.len(count);
        for index in 0..count as u32 {
            let idx = ImplTraitIdx::from_raw(index.into());
            let opaque = InternedOpaqueTyId::new(db, ImplTraitId::ReturnTypeImplTrait(func, idx));
            self.opaque(f, ItemId::Function(func), index, opaque);
        }
    }

    fn adt(&self, f: &mut Form, adt: AdtId) {
        let db = self.db;
        self.visibility(f, adt.module(db), item_visibility(db, ItemId::Adt(adt)));
        self.generics(f, adt.into());
        self.attrs(f, adt.into());
        match AttrFlags::repr(db, adt) {
            Some(ReprOptions { int, align, pack, flags, scalable, field_shuffle_seed: _ }) => {
                f.u8(1);
                f.debug(&int);
                f.debug(&align);
                f.debug(&pack);
                f.debug(&flags);
                f.debug(&scalable);
            }
            None => f.u8(0),
        }
        let range = AttrFlags::rustc_layout_scalar_valid_range(db, adt);
        for bound in [range.start, range.end] {
            match bound {
                Some(it) => {
                    f.u8(1);
                    f.u128(it);
                }
                None => f.u8(0),
            }
        }
        match adt {
            AdtId::StructId(it) => {
                f.u8(0);
                f.u64(u64::from(StructSignature::of(db, it).flags.bits()));
                self.fields(f, it.into());
            }
            AdtId::UnionId(it) => {
                f.u8(1);
                f.u64(u64::from(UnionSignature::of(db, it).flags.bits()));
                self.fields(f, it.into());
            }
            AdtId::EnumId(it) => {
                f.u8(2);
                f.u64(u64::from(EnumSignature::of(db, it).flags.bits()));
                let variants = it.enum_variants(db);
                f.len(variants.variants.len());
                for (variant, _, explicit) in variants.with_explicit_discriminant() {
                    self.attrs(f, variant.into());
                    self.fields(f, variant.into());
                    // An explicit discriminant is a body; a variant without one has none.
                    if explicit && self.leak_free {
                        f.u8(LEAK);
                    } else if explicit {
                        f.u8(1);
                        self.code_in(f, InferBodyId::DefWithBodyId(variant.into()));
                    } else {
                        f.u8(0);
                    }
                }
            }
        }
    }

    fn fields(&self, f: &mut Form, variant: VariantId) {
        let fields = variant.fields(self.db);
        match fields.shape {
            FieldsShape::Record => f.u8(0),
            FieldsShape::Tuple => f.u8(1),
            FieldsShape::Unit => f.u8(2),
        }
        f.len(fields.len());
        for (field, _) in fields.fields().iter() {
            self.nest(f, self.field_form(variant, field));
        }
    }

    fn trait_(&self, f: &mut Form, it: TraitId) {
        let db = self.db;
        self.visibility(f, it.module(db), item_visibility(db, ItemId::Trait(it)));
        self.generics(f, it.into());
        f.u64(u64::from(TraitSignature::of(db, it).flags.bits()));
        self.attrs(f, it.into());
        // A trait's items are named by what implements and calls them: by their place.
        let items = &it.trait_items(db).items;
        f.len(items.len());
        for &(_, item) in items.iter() {
            f.item(item.into());
        }
    }

    fn impl_(&self, f: &mut Form, it: hir_def::ImplId) {
        let db = self.db;
        self.generics(f, it.into());
        f.u64(u64::from(ImplSignature::of(db, it).flags.bits()));
        self.attrs(f, it.into());
        self.ty_in(f, db.impl_self_ty(it).skip_binder());
        let items = &it.impl_items(db).items;
        match db.impl_trait(it) {
            Some(trait_ref) => {
                f.u8(1);
                let trait_ref = trait_ref.skip_binder();
                self.trait_ref(f, trait_ref);
                // Each item of the trait, in its order, with what implements it here.
                let trait_items = &trait_ref.def_id.0.trait_items(db).items;
                // The impl items used, a bit each, inline up to 64 of them.
                let mut used: Bits<SmallVec<[u64; 1]>> = Bits::new(items.len());
                f.len(trait_items.len());
                for (name, of_trait) in trait_items.iter() {
                    f.item((*of_trait).into());
                    let found = items.iter().position(|(other, of_impl)| {
                        other == name && same_kind(*of_impl, *of_trait)
                    });
                    match found {
                        Some(at) => {
                            used.insert(at);
                            f.u8(1);
                            f.item(items[at].1.into());
                        }
                        None => f.u8(0),
                    }
                }
                if used.count() != items.len() {
                    f.error(Missed::ImplItemNotInTrait);
                }
            }
            None => {
                f.u8(0);
                f.len(items.len());
                for &(_, item) in items.iter() {
                    f.item(item.into());
                }
            }
        }
    }

    fn konst_item(&self, f: &mut Form, it: hir_def::ConstId) {
        let db = self.db;
        self.container(f, it.lookup(db).container);
        self.visibility(f, it.module(db), AssocItemId::ConstId(it).assoc_visibility(db));
        self.generics(f, it.into());
        self.ty_in(f, db.type_for_const(it).skip_binder());
        let data = ConstSignature::of(db, it);
        f.u64(u64::from(data.flags.bits()));
        self.attrs(f, it.into());
        // A constant's value is what its users see: it is in their code. One without generics
        // that evaluates is its value, so an edit of a `const fn` it calls that keeps the value
        // reaches no user (E4 in docs/v2/plan/lanes/firewall/notes.md); else its body, from
        // which each use evaluates it.
        if data.has_body() && self.leak_free {
            f.u8(LEAK);
        } else if data.has_body()
            && let Some(value) = self.const_value(it)
        {
            f.u8(2);
            self.allocation(f, value);
        } else if data.has_body() {
            f.u8(1);
            self.nest(f, self.code_form(InferBodyId::DefWithBodyId(it.into())));
        } else {
            f.u8(0);
        }
    }

    fn static_(&self, f: &mut Form, it: hir_def::StaticId) {
        let db = self.db;
        let loc = it.lookup(db);
        self.container(f, loc.container);
        self.visibility(f, it.module(db), item_visibility(db, ItemId::Static(it)));
        self.ty_in(f, db.type_for_static(it).skip_binder());
        let data = StaticSignature::of(db, it);
        f.u64(u64::from(data.flags.bits()));
        let flags = self.attrs(f, it.into());
        let linked = flags.intersects(AttrFlags::NO_MANGLE | AttrFlags::LINKED_BY_NAME)
            || matches!(loc.container, ItemContainerId::ExternBlockId(_));
        self.linked_name(f, linked, data.name.as_str());
        // What an immutable static holds is read where it is used.
        let holds = !data.flags.contains(StaticFlags::MUTABLE)
            && data.flags.contains(StaticFlags::HAS_BODY);
        if holds && self.leak_free {
            f.u8(LEAK);
        } else if holds {
            f.u8(1);
            self.nest(f, self.code_form(InferBodyId::DefWithBodyId(it.into())));
        } else {
            f.u8(0);
        }
    }

    fn alias(&self, f: &mut Form, it: hir_def::TypeAliasId) {
        let db = self.db;
        self.container(f, it.lookup(db).container);
        self.visibility(f, it.module(db), AssocItemId::TypeAliasId(it).assoc_visibility(db));
        self.generics(f, it.into());
        let data = TypeAliasSignature::of(db, it);
        f.u64(u64::from(data.flags.bits()));
        self.attrs(f, it.into());
        // A trait's associated type without a default has no type of its own.
        if data.ty.is_some() {
            f.u8(1);
            self.ty_in(f, db.ty(TyDefId::TypeAliasId(it)).skip_binder());
        } else {
            f.u8(0);
        }
        self.clauses(f, db.type_alias_bounds(it).skip_binder().iter().copied());
        let count = SelfAndAssocBounds::type_alias_opaque_count(db, it);
        f.len(count);
        for index in 0..count as u32 {
            let idx = ImplTraitIdx::from_raw(index.into());
            let opaque = InternedOpaqueTyId::new(db, ImplTraitId::TypeAliasImplTrait(it, idx));
            self.opaque(f, ItemId::Alias(it), index, opaque);
        }
    }

    /// An opaque type of an item's: its bounds, and what is hidden behind it, which callers
    /// see through to the auto traits it implements and its layout.
    fn opaque(&self, f: &mut Form, owner: ItemId, index: u32, opaque: InternedOpaqueTyId<'db>) {
        self.clauses(f, opaque.predicates(self.db).skip_binder().iter().copied());
        if self.leak_free {
            return f.u8(LEAK);
        }
        match self.hidden(owner, index) {
            Some(hidden) => {
                self.ty_in(f, hidden);
                self.nest(f, self.auto_traits_form(owner, index));
            }
            None => f.error(Missed::ErrorType),
        }
    }

    fn hidden(&self, owner: ItemId, index: u32) -> Option<Ty<'db>> {
        let db = self.db;
        let idx = ImplTraitIdx::from_raw(index.into());
        match owner {
            ItemId::Function(it) => {
                rpit_hidden_types(db, it).get(idx).map(|it| it.get().skip_binder())
            }
            ItemId::Alias(it) => {
                let opaque = InternedOpaqueTyId::new(db, ImplTraitId::TypeAliasImplTrait(it, idx));
                let interner = DbInterner::new_no_crate(db);
                Some(interner.type_of_opaque_hir_typeck(OpaqueTyIdWrapper(opaque)).skip_binder())
            }
            ItemId::Adt(_)
            | ItemId::Trait(_)
            | ItemId::Impl(_)
            | ItemId::Const(_)
            | ItemId::Static(_)
            | ItemId::Macro(_) => None,
        }
    }

    /// A macro by its rules: tokens with their text and whether each is joint with the next,
    /// without trivia and without its name.
    fn macro_(&self, f: &mut Form, id: MacroId) {
        let db = self.db;
        self.visibility(f, id.module(db), item_visibility(db, ItemId::Macro(id)));
        self.attrs(f, id.into());
        let (trees, count): ([Option<ast::TokenTree<'_>>; 2], usize) = match id {
            MacroId::MacroRulesId(it) => {
                let loc = it.lookup(db);
                f.u8(0);
                f.debug(&loc.expander);
                f.u8(loc.flags.bits());
                f.debug(&loc.edition);
                ([loc.id.to_view(db).token_tree(), None], 1)
            }
            MacroId::Macro2Id(it) => {
                let loc = it.lookup(db);
                f.u8(1);
                f.debug(&loc.expander);
                f.bool(loc.allow_internal_unsafe);
                f.debug(&loc.edition);
                let node = loc.id.to_view(db);
                match node.args() {
                    Some(args) => ([Some(args), node.body()], 2),
                    None => ([node.body(), None], 1),
                }
            }
            MacroId::ProcMacroId(it) => {
                let loc = it.lookup(db);
                f.u8(2);
                match loc.kind {
                    ProcMacroKind::CustomDerive => f.u8(0),
                    ProcMacroKind::Bang => f.u8(1),
                    ProcMacroKind::Attr => f.u8(2),
                }
                f.debug(&loc.edition);
                let krate = it.krate(db);
                f.update(&key::crate_key(db, krate).0);
                match crate_def_map(db, krate).proc_macro_as_fn(it) {
                    Some(func) => f.item(ItemId::Function(func)),
                    None => f.error(Missed::ProcMacroWithoutFn),
                }
                return;
            }
        };
        f.len(count);
        for tree in trees.into_iter().take(count) {
            let Some(tree) = tree else { return f.error(Missed::NoRules) };
            let at = f.len_at();
            let mut solid = 0;
            let mut tokens =
                tree.syntax().descendants_with_tokens().filter_map(|it| it.into_token()).peekable();
            while let Some(token) = tokens.next() {
                if token.kind().is_trivia() {
                    continue;
                }
                f.str(token.text());
                f.bool(tokens.peek().is_some_and(|next| !next.kind().is_trivia()));
                solid += 1;
            }
            f.set_len(at, solid);
        }
    }

    fn container(&self, f: &mut Form, container: ItemContainerId) {
        let db = self.db;
        match container {
            ItemContainerId::ModuleId(_) => f.u8(0),
            ItemContainerId::ImplId(it) => {
                f.u8(1);
                f.item(ItemId::Impl(it));
            }
            ItemContainerId::TraitId(it) => {
                f.u8(2);
                f.item(ItemId::Trait(it));
            }
            ItemContainerId::ExternBlockId(it) => {
                f.u8(3);
                f.debug(&it.abi(db));
                let libs = AttrFlags::native_libs(db, it);
                f.len(libs.len());
                for lib in libs.iter() {
                    f.str(lib.name.as_str());
                    f.debug(&lib.kind);
                    f.bool(lib.verbatim);
                    f.bool(lib.whole_archive);
                    f.bool(lib.bundle);
                    f.debug(&lib.as_needed);
                    f.symbol(lib.import_name_type);
                }
                f.symbol(AttrFlags::wasm_import_module(db, it));
            }
        }
    }

    /// The name an item is linked by, where it is one the object file has: `#[no_mangle]`,
    /// an item another crate's declaration links to, or a foreign item.
    fn linked_name(&self, f: &mut Form, linked: bool, name: &str) {
        f.bool(linked);
        if linked {
            f.str(name);
        }
    }

    /// Public, the crate, or a module some levels up from the item's own.
    fn visibility(&self, f: &mut Form, module: ModuleId, vis: Visibility) {
        match vis {
            Visibility::Public => f.u8(0),
            Visibility::PubCrate(_) => f.u8(1),
            Visibility::Module(to, _) => {
                let mut at: ModuleIdLt<'db> = module;
                let mut up = 0u32;
                while at != to {
                    match at.containing_module(self.db) {
                        Some(parent) => {
                            at = parent;
                            up += 1;
                        }
                        None => return f.error(Missed::VisibilityOutside),
                    }
                }
                f.u8(2);
                f.u32(up);
            }
        }
    }

    /// The flags are what the attribute pass found: a symbol query whose flag is not set
    /// finds nothing, and asking it would make one memo per item for nothing.
    fn attrs(&self, f: &mut Form, owner: AttrDefId) -> AttrFlags {
        let db = self.db;
        let flags = AttrFlags::query(db, owner);
        f.u128(flags.difference(unobserved()).bits());
        f.symbol(flags.lang_item_with_attrs(db, owner));
        f.symbol(flags.unstable_feature(db, owner));
        f.symbol(if flags.contains(AttrFlags::HAS_EXPORT_NAME) {
            AttrFlags::export_name(db, owner)
        } else {
            None
        });
        f.symbol(if flags.contains(AttrFlags::HAS_LINK_NAME) {
            AttrFlags::link_name(db, owner)
        } else {
            None
        });
        f.symbol(if flags.contains(AttrFlags::HAS_LINK_SECTION) {
            AttrFlags::link_section(db, owner)
        } else {
            None
        });
        f.symbol(if flags.contains(AttrFlags::HAS_LINKAGE) {
            AttrFlags::linkage(db, owner)
        } else {
            None
        });
        flags
    }

    fn generics(&self, f: &mut Form, def: GenericDefId) {
        let db = self.db;
        let generics = hir_ty::generics::generics(db, def);
        let defaults = db.generic_defaults(def);
        let len_at = f.len_at();
        let mut count = 0;
        for (at, (id, data)) in generics.iter(false).enumerate() {
            count += 1;
            match data {
                GenericParamDataRef::TypeParamData(it) => {
                    f.u8(0);
                    f.debug(&it.provenance);
                }
                GenericParamDataRef::ConstParamData(_) => {
                    f.u8(1);
                    match id {
                        GenericParamId::ConstParamId(it) => self.ty_in(f, db.const_param_ty(it)),
                        GenericParamId::TypeParamId(_) | GenericParamId::LifetimeParamId(_) => {
                            f.error(Missed::ErrorConst)
                        }
                    }
                }
                GenericParamDataRef::LifetimeParamData(it) => {
                    f.u8(2);
                    f.debug(&it.bound_type);
                }
            }
            match defaults.get(at) {
                Some(default) => {
                    f.u8(1);
                    self.arg(f, default.skip_binder());
                }
                None => f.u8(0),
            }
        }
        f.set_len(len_at, count);
        self.clauses(f, GenericPredicates::query_all(db, def).skip_binder());
    }

    fn fn_sig(&self, f: &mut Form, sig: FnSig<'db>) {
        f.len(sig.inputs_and_output.len());
        for ty in sig.inputs_and_output.iter() {
            self.ty_in(f, ty);
        }
        f.debug(&sig.abi());
        f.debug(&sig.safety());
        f.bool(sig.c_variadic());
    }

    fn spell(&self, f: &mut Form, ty: Ty<'db>) {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => f.u8(0),
            TyKind::Char => f.u8(1),
            TyKind::Int(it) => {
                f.u8(2);
                f.str(it.name_str());
            }
            TyKind::Uint(it) => {
                f.u8(3);
                f.str(it.name_str());
            }
            TyKind::Float(it) => {
                f.u8(4);
                f.str(it.name_str());
            }
            TyKind::Adt(adt, args) => {
                f.u8(5);
                f.item(ItemId::Adt(adt.def_id()));
                self.args(f, args);
            }
            TyKind::Foreign(it) => {
                f.u8(6);
                f.item(ItemId::Alias(it.0));
            }
            TyKind::Str => f.u8(7),
            TyKind::Array(element, len) => {
                f.u8(8);
                self.ty_in(f, element);
                self.konst(f, len);
            }
            TyKind::Pat(base, pattern) => {
                f.u8(9);
                self.ty_in(f, base);
                self.pattern(f, pattern);
            }
            TyKind::Slice(element) => {
                f.u8(10);
                self.ty_in(f, element);
            }
            TyKind::RawPtr(pointee, mutability) => {
                f.u8(11);
                f.bool(mutability.is_mut());
                self.ty_in(f, pointee);
            }
            TyKind::Ref(region, pointee, mutability) => {
                f.u8(12);
                self.region(f, region);
                f.bool(mutability.is_mut());
                self.ty_in(f, pointee);
            }
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                f.u8(13);
                self.callable(f, def.0);
                self.args(f, args);
                // `type_name` and `type_id` give what the program sees of a type's name.
                if let CallableDefId::FunctionId(func) = def.0
                    && observes_names(db, func)
                {
                    self.names(f, args);
                }
            }
            TyKind::FnPtr(tys, header) => {
                f.u8(14);
                f.len(tys.bound_vars().len());
                let tys = tys.skip_binder().inputs_and_output;
                f.len(tys.len());
                for ty in tys.iter() {
                    self.ty_in(f, ty);
                }
                f.debug(&header.abi());
                f.debug(&header.safety());
                f.bool(header.c_variadic());
            }
            TyKind::UnsafeBinder(inner) => {
                f.u8(15);
                let binder: Binder<'db, Ty<'db>> = inner.into();
                f.len(binder.bound_vars().len());
                self.ty_in(f, binder.skip_binder());
            }
            TyKind::Dynamic(predicates, region) => {
                f.u8(16);
                let forms = predicates.iter().map(|predicate| {
                    let mut g = Form::new("existential");
                    g.len(predicate.bound_vars().len());
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            g.u8(0);
                            g.item(ItemId::Trait(it.def_id.0));
                            self.args(&mut g, it.args);
                        }
                        ExistentialPredicate::Projection(it) => {
                            g.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => g.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => g.item(ItemId::Const(konst)),
                            }
                            self.args(&mut g, it.args);
                            self.term(&mut g, it.term);
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            g.u8(2);
                            g.item(ItemId::Trait(it.0));
                        }
                    }
                    g
                });
                self.set(f, forms);
                self.region(f, region);
            }
            TyKind::Closure(id, args) => {
                f.u8(17);
                let loc = id.0.loc(db);
                self.closure_at(f, loc);
                self.captures_in(f, loc);
                self.args(f, args);
            }
            TyKind::CoroutineClosure(id, args) => {
                f.u8(18);
                let loc = id.0.loc(db);
                self.closure_at(f, loc);
                self.captures_in(f, loc);
                self.args(f, args);
            }
            TyKind::Coroutine(id, args) => {
                f.u8(19);
                let loc = id.0.loc(db);
                self.closure_at(f, loc);
                self.captures_in(f, loc);
                self.args(f, args);
                self.held(f, id.0, args);
            }
            TyKind::CoroutineWitness(id, args) => {
                f.u8(20);
                self.closure_at(f, id.0.loc(db));
                self.args(f, args);
                self.held(f, id.0, args);
            }
            TyKind::Never => f.u8(21),
            TyKind::Tuple(tys) => {
                f.u8(22);
                f.len(tys.len());
                for ty in tys.iter() {
                    self.ty_in(f, ty);
                }
            }
            TyKind::Alias(is_rigid, alias) => {
                f.u8(23);
                f.u8(is_rigid as u8);
                match alias.kind {
                    AliasTyKind::Projection { def_id } => {
                        f.u8(0);
                        f.item(ItemId::Alias(def_id.0));
                    }
                    AliasTyKind::Inherent { def_id } => {
                        f.u8(1);
                        f.item(ItemId::Alias(def_id.0));
                    }
                    AliasTyKind::Opaque { def_id } => {
                        f.u8(2);
                        self.opaque_at(f, def_id.0);
                    }
                    AliasTyKind::Free { def_id } => {
                        f.u8(3);
                        f.item(ItemId::Alias(def_id.0));
                    }
                }
                self.args(f, alias.args);
            }
            TyKind::Param(param) => {
                f.u8(24);
                f.u32(param.index);
            }
            TyKind::Bound(index, bound) => {
                f.u8(25);
                bound_index(f, index);
                f.u32(bound.var.as_u32());
                match bound.kind {
                    BoundTyKind::Anon => f.u8(0),
                    BoundTyKind::Param(_) => f.u8(1),
                }
            }
            TyKind::Placeholder(placeholder) => {
                f.u8(26);
                f.u32(placeholder.universe.as_u32());
                f.u32(placeholder.bound.var.as_u32());
            }
            TyKind::Infer(infer) => {
                f.u8(27);
                match infer {
                    InferTy::TyVar(it) => {
                        f.u8(0);
                        f.u32(it.as_u32());
                    }
                    InferTy::IntVar(it) => {
                        f.u8(1);
                        f.u32(it.as_u32());
                    }
                    InferTy::FloatVar(it) => {
                        f.u8(2);
                        f.u32(it.as_u32());
                    }
                    InferTy::FreshTy(it) => {
                        f.u8(3);
                        f.u32(it);
                    }
                    InferTy::FreshIntTy(it) => {
                        f.u8(4);
                        f.u32(it);
                    }
                    InferTy::FreshFloatTy(it) => {
                        f.u8(5);
                        f.u32(it);
                    }
                }
            }
            TyKind::Error(_) => f.error(Missed::ErrorType),
        }
    }

    fn pattern(&self, f: &mut Form, pattern: Pattern<'db>) {
        match pattern.kind() {
            PatternKind::Range { start, end } => {
                f.u8(0);
                self.konst(f, start);
                self.konst(f, end);
            }
            PatternKind::Or(patterns) => {
                f.u8(1);
                f.len(patterns.len());
                for it in patterns.iter() {
                    self.pattern(f, it);
                }
            }
            PatternKind::NotNull => f.u8(2),
        }
    }

    fn callable(&self, f: &mut Form, def: CallableDefId) {
        match def {
            CallableDefId::FunctionId(it) => {
                f.u8(0);
                f.item(ItemId::Function(it));
            }
            CallableDefId::StructId(it) => {
                f.u8(1);
                f.item(ItemId::Adt(it.into()));
            }
            CallableDefId::EnumVariantId(it) => {
                f.u8(2);
                self.variant(f, it.into());
            }
        }
    }

    fn opaque_at(&self, f: &mut Form, opaque: InternedOpaqueTyId<'db>) {
        match opaque.loc(self.db) {
            ImplTraitId::ReturnTypeImplTrait(of, index) => {
                f.u8(0);
                f.item(ItemId::Function(of));
                f.u32(index.into_raw().into_u32());
            }
            ImplTraitId::TypeAliasImplTrait(of, index) => {
                f.u8(1);
                f.item(ItemId::Alias(of));
                f.u32(index.into_raw().into_u32());
            }
        }
    }

    /// A closure or coroutine by the store it is written in and its place among the store's
    /// closures, with its kind.
    fn closure_at(&self, f: &mut Form, loc: InternedClosure<'db>) {
        let db = self.db;
        self.store(f, loc.owner.expression_store_owner(db));
        f.u32(ClosureKey::nth(db, loc));
        match loc.kind {
            ClosureKind::Closure => f.u8(0),
            ClosureKind::OldCoroutine(movability) => {
                f.u8(1);
                f.debug(&movability);
            }
            ClosureKind::Coroutine { kind, source } => {
                f.u8(2);
                f.debug(&kind);
                f.debug(&source);
            }
            ClosureKind::CoroutineClosure(kind) => {
                f.u8(3);
                f.debug(&kind);
            }
        }
    }

    fn store(&self, f: &mut Form, store: ExpressionStoreOwnerId) {
        match store {
            ExpressionStoreOwnerId::Signature(def) => {
                f.u8(0);
                f.item(generic_def_item(def));
            }
            ExpressionStoreOwnerId::Body(DefWithBodyId::VariantId(it)) => {
                f.u8(1);
                self.variant(f, it.into());
            }
            ExpressionStoreOwnerId::Body(body) => {
                f.u8(2);
                f.item(ItemId::of_body(self.db, body));
            }
            ExpressionStoreOwnerId::VariantFields(it) => {
                f.u8(3);
                self.variant(f, it);
            }
        }
    }

    /// A closure's captures written into `f`, or [`LEAK`] in a signature form.
    fn captures_in(&self, f: &mut Form, loc: InternedClosure<'db>) {
        if self.leak_free {
            f.u8(LEAK);
        } else {
            self.nest(f, self.captures_of(loc));
        }
    }

    fn captures_of(&self, loc: InternedClosure<'db>) -> Form {
        let mut form = Form::new("captures");
        let f = &mut form;
        let infer = InferenceResult::of(self.db, loc.owner);
        match infer.closures_data.get(&loc.expr) {
            Some(data) => {
                self.fn_sig(f, data.liberated_sig.get());
                f.len(data.min_captures.len());
                for (binding, places) in &data.min_captures {
                    f.u32(binding.into_raw().into_u32());
                    f.len(places.len());
                    for place in places {
                        self.captured(f, place);
                    }
                }
            }
            None => f.error(Missed::NoMir),
        }
        form
    }

    fn captured(&self, f: &mut Form, captured: &CapturedPlace) {
        let place = &captured.place;
        match place.base {
            PlaceBase::Rvalue => f.u8(0),
            PlaceBase::StaticItem => f.u8(1),
            PlaceBase::Local(binding) => {
                f.u8(2);
                f.u32(binding.into_raw().into_u32());
            }
            PlaceBase::Upvar { closure, var_id } => {
                f.u8(3);
                f.u32(closure.into_raw().into_u32());
                f.u32(var_id.into_raw().into_u32());
            }
        }
        self.ty_in(f, place.base_ty.as_ref());
        f.len(place.projections.len());
        for projection in &place.projections {
            match projection.kind {
                ProjectionKind::Deref => f.u8(0),
                ProjectionKind::Field { field_idx, variant_idx } => {
                    f.u8(1);
                    f.u32(field_idx);
                    f.u32(variant_idx);
                }
                ProjectionKind::Index => f.u8(2),
                ProjectionKind::Subslice => f.u8(3),
                ProjectionKind::UnwrapUnsafeBinder => f.u8(4),
            }
            self.ty_in(f, projection.ty.as_ref());
        }
        match captured.info.capture_kind {
            UpvarCapture::ByRef(kind) => {
                f.u8(0);
                f.debug(&kind);
            }
            UpvarCapture::ByUse => f.u8(1),
            UpvarCapture::ByValue => f.u8(2),
        }
        f.bool(captured.mutability.is_mut());
        self.ty_in(f, captured.captured_ty(self.db));
    }

    /// What a coroutine keeps across its yields, which its layout is of: the types of the
    /// locals held, which of them each yield keeps, and which may not share storage.
    fn held(&self, f: &mut Form, coroutine: InternedCoroutineId<'db>, args: GenericArgs<'db>) {
        if self.leak_free {
            return f.u8(LEAK);
        }
        let db = self.db;
        let by_move = is_by_move_coroutine(db, coroutine, args.as_coroutine());
        f.bool(by_move);
        let body = if by_move {
            db.runtime_mir_body_for_coroutine_by_move(coroutine)
        } else {
            db.runtime_mir_body_for_coroutine(coroutine)
        };
        let Ok(body) = body else { return f.error(Missed::NoMir) };
        let held = body.locals_held_across_yields();
        f.len(held.locals.len());
        for &local in &held.locals {
            self.ty_in(f, body.locals[local].ty.as_ref());
        }
        f.len(held.yields.len());
        for point in &held.yields {
            f.len(point.kept.len());
            for &kept in &point.kept {
                f.u64(kept as u64);
            }
        }
        for a in 0..held.locals.len() {
            for b in 0..held.locals.len() {
                f.bool(held.conflict(a, b));
            }
        }
    }

    /// A variant by its type and its place in it: a variant's name is in what derives write.
    fn variant(&self, f: &mut Form, variant: VariantId) {
        if f.quiet {
            return;
        }
        match variant {
            VariantId::EnumVariantId(it) => {
                f.item(ItemId::Adt(it.lookup(self.db).parent.into()));
                f.u32(it.index(self.db) as u32);
            }
            VariantId::StructId(it) => f.item(ItemId::Adt(it.into())),
            VariantId::UnionId(it) => f.item(ItemId::Adt(it.into())),
        }
    }

    fn region(&self, f: &mut Form, region: Region<'db>) {
        match region.kind() {
            RegionKind::ReEarlyParam(param) => {
                f.u8(0);
                f.u32(param.index);
            }
            RegionKind::ReBound(index, bound) => {
                f.u8(1);
                bound_index(f, index);
                bound_region(f, bound);
            }
            RegionKind::ReLateParam(param) => {
                f.u8(2);
                bound_region(f, param.kind);
            }
            RegionKind::ReStatic => f.u8(3),
            RegionKind::ReVar(it) => {
                f.u8(4);
                f.u32(it.as_u32());
            }
            RegionKind::RePlaceholder(placeholder) => {
                f.u8(5);
                f.u32(placeholder.universe.as_u32());
                bound_region(f, placeholder.bound);
            }
            RegionKind::ReErased => f.u8(6),
            // hir-ty's lowering gives each lifetime a signature leaves out (`fn f(&self)`, `&S`)
            // an error region, bound only where the fn is reified (`bind_elided_lifetimes`);
            // which lifetime it is follows from where it is, which the form spells. A lifetime
            // that is an error of the program (an undeclared `'b`) is a lowering diagnostic,
            // which fails the check.
            RegionKind::ReError(_) => f.u8(7),
        }
    }

    fn konst(&self, f: &mut Form, konst: Const<'db>) {
        if f.quiet {
            return;
        }
        match konst.kind() {
            ConstKind::Param(param) => {
                f.u8(0);
                f.u32(param.index);
            }
            ConstKind::Infer(InferConst::Var(it)) => {
                f.u8(1);
                f.u32(it.as_u32());
            }
            ConstKind::Infer(InferConst::Fresh(it)) => {
                f.u8(2);
                f.u32(it);
            }
            ConstKind::Bound(index, bound) => {
                f.u8(3);
                bound_index(f, index);
                f.u32(bound.var.as_u32());
            }
            ConstKind::Placeholder(placeholder) => {
                f.u8(4);
                f.u32(placeholder.universe.as_u32());
                f.u32(placeholder.bound.var.as_u32());
            }
            ConstKind::Alias(is_rigid, alias) => {
                f.u8(5);
                f.u8(is_rigid as u8);
                self.alias_const(f, alias.kind);
                self.args(f, alias.args);
            }
            ConstKind::Value(value) => {
                f.u8(6);
                self.ty_in(f, value.ty);
                self.valtree(f, value.value);
            }
            ConstKind::Error(_) => f.error(Missed::ErrorConst),
            ConstKind::Expr(_) => f.u8(7),
        }
    }

    fn alias_const(&self, f: &mut Form, kind: AliasConstKind<'db>) {
        match kind {
            AliasConstKind::Projection { def_id } => {
                f.u8(0);
                f.item(ItemId::Const(def_id.0));
            }
            // The two forms of an inherent const take different args.
            AliasConstKind::InherentSelf { def_id } => {
                f.u8(1);
                f.item(ItemId::Const(def_id.0));
            }
            AliasConstKind::InherentImpl { def_id } => {
                f.u8(2);
                f.item(ItemId::Const(def_id.0));
            }
            AliasConstKind::Free { def_id } => {
                f.u8(3);
                f.item(ItemId::Const(def_id.0));
            }
            // Written where it is used, it is as much a part of it as a closure is.
            AliasConstKind::Anon { def_id } => {
                f.u8(4);
                self.code_in(f, InferBodyId::AnonConstId(def_id.0));
            }
        }
    }

    fn valtree(&self, f: &mut Form, value: ValTree<'db>) {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                f.u8(0);
                f.u64(scalar.size().bytes());
                f.u128(scalar.to_bits_unchecked());
            }
            ValTreeKind::Branch(consts) => {
                f.u8(1);
                f.len(consts.len());
                for it in consts.iter() {
                    self.konst(f, it);
                }
            }
        }
    }

    fn term(&self, f: &mut Form, term: Term<'db>) {
        match term.kind() {
            TermKind::Ty(ty) => {
                f.u8(0);
                self.ty_in(f, ty);
            }
            TermKind::Const(konst) => {
                f.u8(1);
                self.konst(f, konst);
            }
        }
    }

    fn arg(&self, f: &mut Form, arg: hir_ty::next_solver::GenericArg<'db>) {
        match arg.kind() {
            GenericArgKind::Lifetime(region) => {
                f.u8(0);
                self.region(f, region);
            }
            GenericArgKind::Type(ty) => {
                f.u8(1);
                self.ty_in(f, ty);
            }
            GenericArgKind::Const(konst) => {
                f.u8(2);
                self.konst(f, konst);
            }
        }
    }

    fn args(&self, f: &mut Form, args: GenericArgs<'db>) {
        if f.quiet {
            return;
        }
        f.len(args.len());
        for arg in args.iter() {
            self.arg(f, arg);
        }
    }

    fn trait_ref(&self, f: &mut Form, trait_ref: TraitRef<'db>) {
        f.item(ItemId::Trait(trait_ref.def_id.0));
        self.args(f, trait_ref.args);
    }

    /// The key and path of every nominal type in `args`: what `type_name` prints of them and
    /// what `type_id` hashes.
    fn names(&self, f: &mut Form, args: GenericArgs<'db>) {
        // alloc: spelling nests as types do, deeper than frames; most args name few types.
        let mut found = Vec::new();
        for arg in args.iter() {
            for inner in arg.walk() {
                if let Some(ty) = inner.ty() {
                    self.nominal(ty, &mut found);
                }
            }
        }
        self.spell_names(f, found);
    }

    fn names_of_ty(&self, f: &mut Form, ty: Ty<'db>) {
        // alloc: as in `names`.
        let mut found = Vec::new();
        for inner in ty.walk() {
            if let Some(ty) = inner.ty() {
                self.nominal(ty, &mut found);
            }
        }
        self.spell_names(f, found);
    }

    fn spell_names(&self, f: &mut Form, mut found: Vec<Named>) {
        let db = self.db;
        let mut seen = HashSet::default();
        found.retain(|it| seen.insert(*it));
        f.len(found.len());
        for named in found {
            match named {
                Named::Item(id) => {
                    f.u8(0);
                    f.update(&item_key(db, id).0.0);
                    key::path_part(db, id, &mut |it| f.update(it));
                }
                Named::Variant(it) => {
                    f.u8(1);
                    f.update(&key::variant_key(db, it).0.0);
                    let parent = it.lookup(db).parent;
                    key::path_part(db, ItemId::Adt(parent.into()), &mut |it| f.update(it));
                    let variants = &parent.enum_variants(db).variants;
                    match variants.get_index(it.index(db)) {
                        Some((name, _)) => f.str(name.as_str()),
                        None => f.error(Missed::ErrorType),
                    }
                }
            }
        }
    }

    /// The items with a name the type is printed with, at its outermost layer: what the walk
    /// of its arguments does not reach.
    fn nominal(&self, ty: Ty<'db>, found: &mut Vec<Named>) {
        let db = self.db;
        match ty.kind() {
            TyKind::Adt(adt, _) => found.push(Named::Item(ItemId::Adt(adt.def_id()))),
            TyKind::Foreign(it) => found.push(Named::Item(ItemId::Alias(it.0))),
            TyKind::FnDef(def, _) => match def.0 {
                CallableDefId::FunctionId(it) => found.push(Named::Item(ItemId::Function(it))),
                CallableDefId::StructId(it) => found.push(Named::Item(ItemId::Adt(it.into()))),
                CallableDefId::EnumVariantId(it) => found.push(Named::Variant(it)),
            },
            TyKind::Closure(id, _) => found.push(Named::Item(store_item(db, id.0.loc(db)))),
            TyKind::CoroutineClosure(id, _) => {
                found.push(Named::Item(store_item(db, id.0.loc(db))))
            }
            TyKind::Coroutine(id, _) | TyKind::CoroutineWitness(id, _) => {
                found.push(Named::Item(store_item(db, id.0.loc(db))))
            }
            TyKind::Dynamic(predicates, _) => {
                for predicate in predicates.iter() {
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            found.push(Named::Item(ItemId::Trait(it.def_id.0)))
                        }
                        ExistentialPredicate::Projection(it) => match it.def_id.0 {
                            TermId::TypeAliasId(alias) => {
                                found.push(Named::Item(ItemId::Alias(alias)))
                            }
                            TermId::ConstId(konst) => found.push(Named::Item(ItemId::Const(konst))),
                        },
                        ExistentialPredicate::AutoTrait(it) => {
                            found.push(Named::Item(ItemId::Trait(it.0)))
                        }
                    }
                }
            }
            TyKind::Alias(_, alias) => match alias.kind {
                AliasTyKind::Projection { def_id } => {
                    found.push(Named::Item(ItemId::Alias(def_id.0)))
                }
                AliasTyKind::Inherent { def_id } => {
                    found.push(Named::Item(ItemId::Alias(def_id.0)))
                }
                AliasTyKind::Free { def_id } => found.push(Named::Item(ItemId::Alias(def_id.0))),
                AliasTyKind::Opaque { def_id } => match def_id.0.loc(db) {
                    ImplTraitId::ReturnTypeImplTrait(of, _) => {
                        found.push(Named::Item(ItemId::Function(of)))
                    }
                    ImplTraitId::TypeAliasImplTrait(of, _) => {
                        found.push(Named::Item(ItemId::Alias(of)))
                    }
                },
            },
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Array(..)
            | TyKind::Pat(..)
            | TyKind::Slice(_)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnPtr(..)
            | TyKind::UnsafeBinder(_)
            | TyKind::Never
            | TyKind::Tuple(_)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Error(_) => {}
        }
    }

    /// Where clauses as a set: reordering them changes nothing.
    fn clauses(&self, f: &mut Form, clauses: impl Iterator<Item = Clause<'db>>) {
        let forms = clauses.map(|clause| {
            let mut g = Form::new("clause");
            self.clause(&mut g, clause);
            g
        });
        self.set(f, forms);
    }

    fn clause(&self, f: &mut Form, clause: Clause<'db>) {
        let kind = clause.kind();
        f.len(kind.bound_vars().len());
        match kind.skip_binder() {
            ClauseKind::Trait(it) => {
                f.u8(0);
                self.trait_ref(f, it.trait_ref);
                f.debug(&it.polarity);
            }
            ClauseKind::RegionOutlives(OutlivesClause(a, b)) => {
                f.u8(1);
                self.region(f, a);
                self.region(f, b);
            }
            ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {
                f.u8(2);
                self.ty_in(f, ty);
                self.region(f, region);
            }
            ClauseKind::Projection(it) => {
                f.u8(3);
                match it.projection_term.kind {
                    AliasTermKind::ProjectionTy { def_id } => {
                        f.u8(0);
                        f.item(ItemId::Alias(def_id.0));
                    }
                    AliasTermKind::InherentTy { def_id } => {
                        f.u8(1);
                        f.item(ItemId::Alias(def_id.0));
                    }
                    AliasTermKind::OpaqueTy { def_id } => {
                        f.u8(2);
                        self.opaque_at(f, def_id.0);
                    }
                    AliasTermKind::FreeTy { def_id } => {
                        f.u8(3);
                        f.item(ItemId::Alias(def_id.0));
                    }
                    AliasTermKind::AnonConst { def_id } => {
                        f.u8(4);
                        self.code_in(f, InferBodyId::AnonConstId(def_id.0));
                    }
                    AliasTermKind::ProjectionConst { def_id } => {
                        f.u8(5);
                        f.item(ItemId::Const(def_id.0));
                    }
                    AliasTermKind::FreeConst { def_id } => {
                        f.u8(6);
                        f.item(ItemId::Const(def_id.0));
                    }
                    AliasTermKind::InherentConstSelf { def_id } => {
                        f.u8(7);
                        f.item(ItemId::Const(def_id.0));
                    }
                    AliasTermKind::InherentConstImpl { def_id } => {
                        f.u8(8);
                        f.item(ItemId::Const(def_id.0));
                    }
                }
                self.args(f, it.projection_term.args);
                self.term(f, it.term);
            }
            ClauseKind::ConstArgHasType(konst, ty) => {
                f.u8(4);
                self.konst(f, konst);
                self.ty_in(f, ty);
            }
            ClauseKind::WellFormed(term) => {
                f.u8(5);
                self.term(f, term);
            }
            ClauseKind::ConstEvaluatable(konst) => {
                f.u8(6);
                self.konst(f, konst);
            }
            ClauseKind::HostEffect(it) => {
                f.u8(7);
                self.trait_ref(f, it.trait_ref);
                f.debug(&it.constness);
            }
            ClauseKind::UnstableFeature(feature) => {
                f.u8(8);
                f.str(feature.0.as_str());
            }
        }
    }

    fn code_form(&self, owner: InferBodyId<'db>) -> Form {
        let mut f = Form::new("body");
        self.code_in(&mut f, owner);
        f
    }

    /// A body, then each closure and coroutine written in it, transitively, in the order the
    /// walks meet them.
    fn code_in(&self, f: &mut Form, owner: InferBodyId<'db>) {
        let db = self.db;
        let Ok(body) = db.mir_body(owner) else { return f.error(Missed::NoMir) };
        let mut nested = Nests::default();
        self.mir(f, body, &mut nested);
        let mut at = 0;
        while at < nested.order.len() {
            let next = nested.order[at];
            at += 1;
            let body = match next {
                Nested::Closure(it) => {
                    f.u8(0);
                    db.mir_body_for_closure(it)
                }
                Nested::Coroutine(it) => {
                    f.u8(1);
                    db.mir_body_for_coroutine(it)
                }
            };
            match body {
                Ok(body) => self.mir(f, body, &mut nested),
                Err(_) => f.error(Missed::NoMir),
            }
        }
        f.len(nested.order.len());
    }

    /// A body as the code generator is given it: no names, no spans, blocks and locals by
    /// number in the order a walk from the start block reaches them. The closures and
    /// coroutines it makes go to `nested`.
    fn mir(&self, f: &mut Form, body: &MirBody<'db>, nested: &mut Nests<'db>) {
        let canon = Canon::new(body);
        self.walk(&mut Form::quiet(), &canon);
        canon.collecting.set(false);
        self.walk(f, &canon);
        for it in canon.nested.take() {
            nested.push(it);
        }
        for &it in &body.closures {
            nested.push(Nested::Closure(it));
        }
    }

    fn walk(&self, f: &mut Form, c: &Canon<'_, 'db>) {
        let body = c.body;
        self.local(f, c, LocalId::from_raw(0u32.into()));
        f.len(body.param_locals.len());
        for &local in &body.param_locals {
            self.local(f, c, local);
        }
        f.bool(body.inherits_overflow_checks);
        f.debug(&body.phase);
        match body.coroutine {
            Some(coroutine) => {
                f.u8(1);
                self.local(f, c, coroutine.poll);
                self.local(f, c, coroutine.context);
            }
            None => f.u8(0),
        }
        f.len(c.c.order.len());
        let mut unwinds = 0;
        for (number, &id) in c.c.order.iter().enumerate() {
            let block = &body.basic_blocks[id];
            f.bool(block.is_cleanup);
            for (at, statement) in block.statements.iter().enumerate() {
                if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                    statement.kind
                    && !c.used(local)
                {
                    continue;
                }
                if let StatementKind::Nop = statement.kind {
                    continue;
                }
                f.span(statement.span);
                self.statement(f, c, &statement.kind);
                if let Some(unwind) = c.unwind(&mut unwinds, number as u32, at) {
                    f.u8(0xfd);
                    self.block(f, c, Some(unwind));
                }
            }
            match &block.terminator {
                Some(terminator) => {
                    f.u8(1);
                    f.span(terminator.span);
                    self.terminator(f, c, &terminator.kind);
                }
                None => f.u8(0),
            }
        }
        // What lowering keeps beside the blocks, which the code generator reads: the order
        // scopes drop their locals in, the temporaries promoted to constants, the captures of an
        // `async` body used in place, and the blocks no test leads to.
        self.used_locals(f, c, &body.scope_ends);
        self.used_locals(f, c, &body.promoted_constants);
        f.len(body.promoted_locals.len());
        for &(local, expr) in &body.promoted_locals {
            self.local(f, c, local);
            f.u32(expr.into_raw().into_u32());
        }
        f.len(body.upvars_in_place.len());
        for &(local, field) in &body.upvars_in_place {
            self.local(f, c, local);
            f.u32(field);
        }
        let at = f.len_at();
        let mut count = 0;
        for &block in &body.unreached {
            if let Some(number) = c.number_of(block) {
                f.u32(number);
                count += 1;
            }
        }
        f.set_len(at, count);
    }

    /// Those of `locals` the first walk found used, counted.
    fn used_locals(&self, f: &mut Form, c: &Canon<'_, 'db>, locals: &[LocalId]) {
        let at = f.len_at();
        let mut count = 0;
        for &local in locals {
            if c.used(local) {
                self.local(f, c, local);
                count += 1;
            }
        }
        f.set_len(at, count);
    }

    /// A local by the order the walk first meets it, with its type the first time: what
    /// lowering numbers it does not matter.
    fn local(&self, f: &mut Form, c: &Canon<'_, 'db>, local: LocalId) {
        if c.collecting.get() {
            return c.mark_used(local);
        }
        match c.number(local) {
            (_, true) => {
                f.u8(0);
                self.ty_in(f, c.body.locals[local].ty.as_ref());
            }
            (number, false) => {
                f.u8(1);
                f.u32(number);
            }
        }
    }

    fn place(&self, f: &mut Form, c: &Canon<'_, 'db>, place: &StoredPlace) {
        self.local(f, c, place.local);
        let projection = place.projection.as_slice();
        f.len(projection.len());
        for elem in projection {
            match *elem {
                ProjectionElem::Deref => f.u8(0),
                ProjectionElem::Field(field) => {
                    f.u8(1);
                    f.u32(field.0);
                }
                ProjectionElem::Index(local) => {
                    f.u8(2);
                    self.local(f, c, local);
                }
                ProjectionElem::ConstantIndex { offset, min_length, from_end } => {
                    f.u8(3);
                    f.u64(offset);
                    f.u64(min_length);
                    f.bool(from_end);
                }
                ProjectionElem::Subslice { from, to } => {
                    f.u8(4);
                    f.u64(from);
                    f.u64(to);
                }
                ProjectionElem::Downcast(variant) => {
                    f.u8(5);
                    self.variant(f, variant);
                }
            }
        }
    }

    fn operand(&self, f: &mut Form, c: &Canon<'_, 'db>, operand: &Operand) {
        f.operand_span(operand.span);
        match &operand.kind {
            OperandKind::Copy(place) => {
                f.u8(0);
                self.place(f, c, place);
            }
            OperandKind::Move(place) => {
                f.u8(1);
                self.place(f, c, place);
            }
            OperandKind::Constant { konst, ty } => {
                f.u8(2);
                self.konst(f, konst.as_ref());
                self.ty_in(f, ty.as_ref());
            }
            OperandKind::Allocation { allocation } => {
                f.u8(3);
                self.allocation(f, allocation.as_ref());
            }
            OperandKind::Static(id) => {
                f.u8(4);
                f.item(ItemId::Static(*id));
            }
        }
    }

    /// A constant's bytes with what is in them beside bytes: the pieces of memory its
    /// pointers point to, the types of its vtables, the statics it points into and the types
    /// of the `TypeId`s it holds.
    fn allocation(&self, f: &mut Form, allocation: Allocation<'db>) {
        if f.quiet {
            return;
        }
        self.ty_in(f, allocation.ty);
        f.bytes(&allocation.memory);
        match &allocation.memory_map {
            MemoryMap::Empty => f.u8(0),
            MemoryMap::Simple(bytes) => {
                f.u8(1);
                f.bytes(bytes);
            }
            MemoryMap::Complex(map) => {
                f.u8(2);
                // alloc: a complex constant's pieces, sorted to spell; spelling nests as types do.
                let mut pieces: Vec<_> = map.memory.iter().collect();
                radix::sort_copy_by_key(&mut pieces, |it| *it.0);
                f.len(pieces.len());
                for (&at, memory) in pieces {
                    f.u64(at as u64);
                    f.bytes(memory);
                    match allocation.memory_map.align_at(at) {
                        Some(align) => {
                            f.u8(1);
                            f.u64(align as u64);
                        }
                        None => f.u8(0),
                    }
                }
                f.len(map.vtable.tys().len());
                for ty in map.vtable.tys() {
                    self.ty_in(f, ty);
                }
                f.len(map.statics.len());
                for it in &map.statics {
                    f.item(ItemId::Static(it.id));
                    f.u64(it.start as u64);
                    f.u64(it.size as u64);
                }
                f.len(map.type_ids.len());
                for it in &map.type_ids {
                    match *it {
                        Some(ty) => {
                            f.u8(1);
                            self.ty_in(f, ty);
                            self.names_of_ty(f, ty);
                        }
                        None => f.u8(0),
                    }
                }
            }
        }
    }

    fn operands(&self, f: &mut Form, c: &Canon<'_, 'db>, operands: &[Operand]) {
        f.len(operands.len());
        for operand in operands {
            self.operand(f, c, operand);
        }
    }

    fn rvalue(&self, f: &mut Form, c: &Canon<'_, 'db>, rvalue: &Rvalue) {
        match rvalue {
            Rvalue::Use(operand) => {
                f.u8(0);
                self.operand(f, c, operand);
            }
            Rvalue::Repeat(operand, len) => {
                f.u8(1);
                self.operand(f, c, operand);
                self.konst(f, len.as_ref());
            }
            Rvalue::Ref(kind, place) => {
                f.u8(2);
                f.debug(kind);
                self.place(f, c, place);
            }
            Rvalue::Len(place) => {
                f.u8(3);
                self.place(f, c, place);
            }
            Rvalue::Cast(kind, operand, ty) => {
                f.u8(4);
                f.debug(kind);
                self.operand(f, c, operand);
                self.ty_in(f, ty.as_ref());
            }
            Rvalue::BinaryOp(op, a, b) => {
                f.u8(5);
                f.debug(op);
                self.operand(f, c, a);
                self.operand(f, c, b);
            }
            Rvalue::CheckedBinaryOp(op, a, b) => {
                f.u8(6);
                f.debug(op);
                self.operand(f, c, a);
                self.operand(f, c, b);
            }
            Rvalue::UnaryOp(op, operand) => {
                f.u8(7);
                f.debug(op);
                self.operand(f, c, operand);
            }
            Rvalue::Discriminant(place) => {
                f.u8(8);
                self.place(f, c, place);
            }
            Rvalue::Aggregate(kind, operands) => {
                f.u8(9);
                match kind {
                    AggregateKind::Array(ty) => {
                        f.u8(0);
                        self.ty_in(f, ty.as_ref());
                    }
                    AggregateKind::Tuple(ty) => {
                        f.u8(1);
                        self.ty_in(f, ty.as_ref());
                    }
                    AggregateKind::Adt(variant, args) => {
                        f.u8(2);
                        self.variant(f, *variant);
                        self.args(f, args.as_ref());
                    }
                    AggregateKind::Union(union, field) => {
                        f.u8(3);
                        f.item(ItemId::Adt((*union).into()));
                        f.u32(field.local_id.into_raw().into_u32());
                    }
                    AggregateKind::Closure(ty) => {
                        f.u8(4);
                        let ty = ty.as_ref();
                        self.ty_in(f, ty);
                        if !c.collecting.get()
                            && let Some(made) = self.nested_of(ty)
                        {
                            c.nested.borrow_mut().push(made);
                        }
                    }
                }
                self.operands(f, c, operands);
            }
            Rvalue::CopyForDeref(place) => {
                f.u8(10);
                self.place(f, c, place);
            }
            Rvalue::ThreadLocalRef(never) | Rvalue::AddressOf(never) | Rvalue::NullaryOp(never) => {
                match *never {}
            }
        }
    }

    /// The body an aggregate of this type makes: a closure's or a coroutine's.
    fn nested_of(&self, ty: Ty<'db>) -> Option<Nested<'db>> {
        match ty.kind() {
            TyKind::Closure(id, _) => Some(Nested::Closure(id.0)),
            TyKind::CoroutineClosure(id, _) => Some(Nested::Closure(id.0.as_closure(self.db))),
            TyKind::Coroutine(id, _) => Some(Nested::Coroutine(id.0)),
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Adt(..)
            | TyKind::Foreign(_)
            | TyKind::Str
            | TyKind::Array(..)
            | TyKind::Pat(..)
            | TyKind::Slice(_)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::UnsafeBinder(_)
            | TyKind::Dynamic(..)
            | TyKind::CoroutineWitness(..)
            | TyKind::Never
            | TyKind::Tuple(_)
            | TyKind::Alias(_, _)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Error(_) => None,
        }
    }

    fn statement(&self, f: &mut Form, c: &Canon<'_, 'db>, statement: &StatementKind) {
        match statement {
            StatementKind::Assign(place, rvalue) => {
                f.u8(0);
                self.place(f, c, place);
                self.rvalue(f, c, rvalue);
            }
            StatementKind::FakeRead(place) => {
                f.u8(1);
                self.place(f, c, place);
            }
            StatementKind::Deinit(place) => {
                f.u8(2);
                self.place(f, c, place);
            }
            StatementKind::StorageLive(local) => {
                f.u8(3);
                self.local(f, c, *local);
            }
            StatementKind::StorageDead(local) => {
                f.u8(4);
                self.local(f, c, *local);
            }
            StatementKind::InlineAsm(asm) => {
                f.u8(5);
                f.debug(&asm.template);
                f.u64(u64::from(asm.options.bits()));
                f.bool(asm.naked);
                f.bool(asm.clobber_abi);
                f.len(asm.operands.len());
                for operand in &asm.operands {
                    match operand {
                        AsmOperand::Reg { reg, input, output, written } => {
                            f.u8(0);
                            f.debug(reg);
                            f.bool(*written);
                            match input {
                                Some(input) => {
                                    f.u8(1);
                                    self.operand(f, c, input);
                                }
                                None => f.u8(0),
                            }
                            match output {
                                Some(output) => {
                                    f.u8(1);
                                    self.place(f, c, output);
                                }
                                None => f.u8(0),
                            }
                        }
                        AsmOperand::Const(operand) => {
                            f.u8(1);
                            self.operand(f, c, operand);
                        }
                        AsmOperand::Label => f.u8(2),
                        AsmOperand::Sym(operand) => {
                            f.u8(3);
                            self.operand(f, c, operand);
                        }
                    }
                }
                match &asm.taken {
                    Some(taken) => {
                        f.u8(1);
                        self.place(f, c, taken);
                    }
                    None => f.u8(0),
                }
            }
            StatementKind::Nop => f.u8(6),
        }
    }

    fn block(&self, f: &mut Form, c: &Canon<'_, 'db>, block: Option<BasicBlockId>) {
        match block {
            Some(it) => {
                f.u8(1);
                f.u32(c.number_of(it).expect("a block a reached block leads to is reached"));
            }
            None => f.u8(0),
        }
    }

    fn terminator(&self, f: &mut Form, c: &Canon<'_, 'db>, terminator: &TerminatorKind) {
        match terminator {
            TerminatorKind::Goto { target } => {
                f.u8(0);
                self.block(f, c, Some(*target));
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                f.u8(1);
                self.operand(f, c, discr);
                f.len(targets.iter().count());
                for (value, target) in targets.iter() {
                    f.u128(value);
                    self.block(f, c, Some(target));
                }
                self.block(f, c, Some(targets.otherwise()));
            }
            TerminatorKind::UnwindResume => f.u8(2),
            TerminatorKind::Abort => f.u8(3),
            TerminatorKind::Return => f.u8(4),
            TerminatorKind::Unreachable => f.u8(5),
            TerminatorKind::Drop { place, target, unwind } => {
                f.u8(6);
                self.place(f, c, place);
                self.block(f, c, Some(*target));
                self.block(f, c, *unwind);
            }
            TerminatorKind::DropAndReplace { place, value, target, unwind } => {
                f.u8(7);
                self.place(f, c, place);
                self.operand(f, c, value);
                self.block(f, c, Some(*target));
                self.block(f, c, *unwind);
            }
            // `intrinsic` is what `func` names, which is hashed.
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
                cleanup,
                from_hir_call,
                intrinsic: _,
            } => {
                f.u8(8);
                self.operand(f, c, func);
                self.operands(f, c, args);
                self.place(f, c, destination);
                self.block(f, c, *target);
                self.block(f, c, *cleanup);
                f.bool(*from_hir_call);
            }
            TerminatorKind::Assert { cond, expected, msg, target, cleanup } => {
                f.u8(9);
                self.operand(f, c, cond);
                f.bool(*expected);
                match msg {
                    AssertKind::BoundsCheck { len, index } => {
                        f.u8(0);
                        self.operand(f, c, len);
                        self.operand(f, c, index);
                    }
                    AssertKind::Overflow(op, a, b) => {
                        f.u8(1);
                        f.debug(op);
                        self.operand(f, c, a);
                        self.operand(f, c, b);
                    }
                    AssertKind::OverflowNeg(it) => {
                        f.u8(2);
                        self.operand(f, c, it);
                    }
                    AssertKind::DivisionByZero(it) => {
                        f.u8(3);
                        self.operand(f, c, it);
                    }
                    AssertKind::RemainderByZero(it) => {
                        f.u8(4);
                        self.operand(f, c, it);
                    }
                    AssertKind::MisalignedPointerDereference { required, found } => {
                        f.u8(5);
                        self.operand(f, c, required);
                        self.operand(f, c, found);
                    }
                    AssertKind::NullPointerDereference => f.u8(6),
                    AssertKind::NullReferenceConstructed => f.u8(7),
                    AssertKind::InvalidEnumConstruction(it) => {
                        f.u8(8);
                        self.operand(f, c, it);
                    }
                }
                self.block(f, c, Some(*target));
                self.block(f, c, *cleanup);
            }
            TerminatorKind::Yield { value, resume, resume_arg, drop } => {
                f.u8(10);
                self.operand(f, c, value);
                self.block(f, c, Some(*resume));
                self.place(f, c, resume_arg);
                self.block(f, c, *drop);
            }
            TerminatorKind::CoroutineDrop => f.u8(11),
            TerminatorKind::FalseEdge { real_target, imaginary_target } => {
                f.u8(12);
                self.block(f, c, Some(*real_target));
                self.block(f, c, Some(*imaginary_target));
            }
            TerminatorKind::FalseUnwind { real_target, unwind } => {
                f.u8(13);
                self.block(f, c, Some(*real_target));
                self.block(f, c, *unwind);
            }
            TerminatorKind::TailCall { func, args, fn_span } => {
                f.u8(14);
                self.operand(f, c, func);
                self.operands(f, c, args);
                f.span(*fn_span);
            }
        }
    }
}

/// What a type's printed name is made of: an item's path, or an enum's and a variant's name.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Named {
    Item(ItemId),
    Variant(hir_def::EnumVariantId),
}

/// Whether calling the function shows the program the name or identity of the types it is
/// called with. Only the intrinsics do; a generic fn that calls one with its parameters is
/// an instance that does, and codegen keys such an instance on the [`crate::Fact::Name`] of
/// the nominal types in its arguments.
pub fn observes_names(db: &Db, func: FunctionId) -> bool {
    // A call of an unknown intrinsic is refused when it is lowered; keying it on the names
    // as well misses no input.
    matches!(
        intrinsic_of(db, func),
        Ok(Some(Intrinsic::TypeName | Intrinsic::TypeId)) | Err(_)
    )
}

fn same_kind(a: AssocItemId, b: AssocItemId) -> bool {
    matches!(
        (a, b),
        (AssocItemId::FunctionId(_), AssocItemId::FunctionId(_))
            | (AssocItemId::ConstId(_), AssocItemId::ConstId(_))
            | (AssocItemId::TypeAliasId(_), AssocItemId::TypeAliasId(_))
    )
}

/// The item whose store a closure is written in.
fn store_item(db: &Db, loc: InternedClosure<'_>) -> ItemId {
    match loc.owner.expression_store_owner(db) {
        ExpressionStoreOwnerId::Signature(def) => generic_def_item(def),
        ExpressionStoreOwnerId::Body(body) => ItemId::of_body(db, body),
        ExpressionStoreOwnerId::VariantFields(VariantId::EnumVariantId(it)) => {
            ItemId::Adt(it.lookup(db).parent.into())
        }
        ExpressionStoreOwnerId::VariantFields(VariantId::StructId(it)) => ItemId::Adt(it.into()),
        ExpressionStoreOwnerId::VariantFields(VariantId::UnionId(it)) => ItemId::Adt(it.into()),
    }
}

fn variant_module(db: &Db, variant: VariantId) -> ModuleId {
    match variant {
        VariantId::EnumVariantId(it) => it.lookup(db).parent.module(db),
        VariantId::StructId(it) => it.module(db),
        VariantId::UnionId(it) => it.module(db),
    }
}

/// The visibility of an item that is not associated: a function, constant or type alias has
/// [`AssocItemId::assoc_visibility`] wherever it is.
fn item_visibility_uncached(db: &Db, id: ItemId) -> Visibility {
    match id {
        ItemId::Function(it) => AssocItemId::FunctionId(it).assoc_visibility(db),
        ItemId::Const(it) => AssocItemId::ConstId(it).assoc_visibility(db),
        ItemId::Alias(it) => AssocItemId::TypeAliasId(it).assoc_visibility(db),
        ItemId::Static(it) => {
            written_visibility(db, it, it.lookup(db).id)
        }
        ItemId::Adt(AdtId::StructId(it)) => {
            written_visibility(db, it, it.lookup(db).id)
        }
        ItemId::Adt(AdtId::EnumId(it)) => {
            written_visibility(db, it, it.lookup(db).id)
        }
        ItemId::Adt(AdtId::UnionId(it)) => {
            written_visibility(db, it, it.lookup(db).id)
        }
        ItemId::Trait(it) => {
            written_visibility(db, it, it.lookup(db).id)
        }
        // An impl is wherever its types and trait are.
        ItemId::Impl(_) => Visibility::Public,
        // A `macro_rules!` is in scope by text below it, or everywhere once exported.
        ItemId::Macro(MacroId::MacroRulesId(it)) => {
            if AttrFlags::query(db, it.into()).contains(AttrFlags::IS_MACRO_EXPORT) {
                Visibility::Public
            } else {
                Visibility::PubCrate(it.krate(db))
            }
        }
        ItemId::Macro(MacroId::Macro2Id(it)) => {
            written_visibility(db, it, it.lookup(db).id)
        }
        ItemId::Macro(MacroId::ProcMacroId(_)) => Visibility::Public,
    }
}

fn bound_index(f: &mut Form, index: BoundVarIndexKind) {
    match index {
        BoundVarIndexKind::Bound(depth) => {
            f.u8(0);
            f.u32(depth.as_u32());
        }
        BoundVarIndexKind::Canonical => f.u8(1),
    }
}

fn bound_region(f: &mut Form, bound: BoundRegion<'_>) {
    f.u32(bound.var.as_u32());
    match bound.kind {
        BoundRegionKind::Anon => f.u8(0),
        BoundRegionKind::NamedForPrinting(_) => f.u8(1),
        BoundRegionKind::Named(_) => f.u8(2),
        BoundRegionKind::ClosureEnv => f.u8(3),
    }
}

/// The order a body is written in. Blocks no walk from the start reaches, and the storage
/// marks of a local nothing else uses (`return 5;` has one where `return 5` has none), change
/// nothing the code does and are left out.
struct Canon<'b, 'db> {
    body: &'b MirBody<'db>,
    c: CanonBuffers,
    /// The first walk only finds which locals are used.
    collecting: Cell<bool>,
    /// The number the next local the second walk meets gets.
    next: Cell<u32>,
    /// The closures and coroutines the second walk meets being made, in its order.
    nested: RefCell<Vec<Nested<'db>>>,
}

/// A walk's columns, indexed by arena index: 4 bytes a block and a local and a bit a local,
/// one load a lookup. Kept by the worker between bodies ([`SPARE`]).
#[derive(Default)]
struct CanonBuffers {
    /// The blocks a walk from the start reaches, in the order it reaches them.
    order: Vec<BasicBlockId>,
    /// Each block's place in `order`, [`UNREACHED`] for one no walk reaches.
    blocks: Vec<u32>,
    stack: Vec<BasicBlockId>,
    /// `(place of the block, statement, cleanup block)` of each statement that unwinds,
    /// sorted: the walk reads it in order with a cursor.
    unwinds: Vec<(u32, usize, BasicBlockId)>,
    /// A bit for each local the first walk found used, in cells rather than a
    /// `blaze_ids::bits::BitSet`: the walk marks them through `&self`.
    uses: Vec<Cell<u64>>,
    /// Each local's number in the order the second walk meets it, [`UNNUMBERED`] before.
    numbers: Vec<Cell<u32>>,
}

const UNREACHED: u32 = u32::MAX;
const UNNUMBERED: u32 = u32::MAX;

fn block_index(id: BasicBlockId) -> usize {
    id.into_raw().into_u32() as usize
}

fn local_index(id: LocalId) -> usize {
    id.into_raw().into_u32() as usize
}

impl<'b, 'db> Canon<'b, 'db> {
    fn new(body: &'b MirBody<'db>) -> Self {
        let mut c = SPARE
            .try_with(|spare| spare.try_borrow_mut().ok().and_then(|mut it| it.canons.pop()))
            .ok()
            .flatten()
            .unwrap_or_default();
        c.blocks.resize(body.basic_blocks.len(), UNREACHED);
        c.stack.push(body.start_block);
        while let Some(id) = c.stack.pop() {
            let at = block_index(id);
            if c.blocks[at] != UNREACHED {
                continue;
            }
            c.blocks[at] = c.order.len() as u32;
            c.order.push(id);
            if let Some(terminator) = &body.basic_blocks[id].terminator {
                c.stack.extend(terminator.kind.all_successors().into_iter().rev());
            }
        }
        let blocks = &c.blocks;
        c.unwinds.extend(body.panic_unwinds.iter().filter_map(|&(block, at, to)| {
            let number = blocks[block_index(block)];
            (number != UNREACHED && blocks[block_index(to)] != UNREACHED).then_some((
                number, at, to,
            ))
        }));
        radix::sort_copy_by_key(&mut c.unwinds, |&(number, at, _)| {
            radix::pack_64(u64::from(number), at as u64)
        });
        c.uses.resize_with(bits::words_for(body.locals.len()), || Cell::new(0));
        c.numbers.resize_with(body.locals.len(), || Cell::new(UNNUMBERED));
        Canon {
            body,
            c,
            collecting: Cell::new(true),
            next: Cell::new(0),
            nested: RefCell::default(),
        }
    }

    /// The place of `block` in the walk's order, if the walk reaches it.
    fn number_of(&self, block: BasicBlockId) -> Option<u32> {
        match self.c.blocks[block_index(block)] {
            UNREACHED => None,
            number => Some(number),
        }
    }

    /// The cleanup block the `at`th statement of the `block`th block unwinds to. The walk asks
    /// in order, so `cursor` only moves forward; of two entries for one statement the later
    /// one holds, as it did in a map built from the list.
    fn unwind(&self, cursor: &mut usize, block: u32, at: usize) -> Option<BasicBlockId> {
        let unwinds = &self.c.unwinds;
        while *cursor < unwinds.len() && (unwinds[*cursor].0, unwinds[*cursor].1) < (block, at) {
            *cursor += 1;
        }
        let mut found = None;
        while *cursor < unwinds.len() && (unwinds[*cursor].0, unwinds[*cursor].1) == (block, at) {
            found = Some(unwinds[*cursor].2);
            *cursor += 1;
        }
        found
    }

    fn used(&self, local: LocalId) -> bool {
        let (word, mask) = bits::locate(local_index(local));
        self.c.uses[word].get() & mask != 0
    }

    fn mark_used(&self, local: LocalId) {
        let (word, mask) = bits::locate(local_index(local));
        let word = &self.c.uses[word];
        word.set(word.get() | mask);
    }

    /// The local's number, and whether the walk meets it here first.
    fn number(&self, local: LocalId) -> (u32, bool) {
        let cell = &self.c.numbers[local_index(local)];
        match cell.get() {
            UNNUMBERED => {
                let number = self.next.get();
                self.next.set(number + 1);
                cell.set(number);
                (number, true)
            }
            number => (number, false),
        }
    }
}

impl Drop for Canon<'_, '_> {
    fn drop(&mut self) {
        let mut c = mem::take(&mut self.c);
        if (c.blocks.capacity() + c.numbers.capacity()) * size_of::<u32>() > SPARE_BYTES {
            return;
        }
        c.order.clear();
        c.blocks.clear();
        c.stack.clear();
        c.unwinds.clear();
        c.uses.clear();
        c.numbers.clear();
        let _ = SPARE.try_with(|spare| {
            if let Ok(mut spare) = spare.try_borrow_mut()
                && spare.canons.len() < SPARE_COUNT
            {
                spare.canons.push(c);
            }
        });
    }
}

/// What the tests compare the writer's columns and buffers against: each answer here is read
/// from the new layout, and the tests build the old one beside it.
#[cfg(test)]
pub(crate) mod check {
    use std::fmt::Debug;

    use base_db::Db;
    use hir_ty::mir::{BasicBlockId, MirBody};

    use super::{Canon, Cx, Form, local_index};
    use crate::ItemId;

    /// A walk's order, each reached block's place in it, and the cleanup block each statement
    /// of a reached block unwinds to, as [`Canon`] keeps them.
    pub(crate) struct Walk {
        pub(crate) order: Vec<BasicBlockId>,
        pub(crate) numbers: Vec<(BasicBlockId, u32)>,
        pub(crate) unwinds: Vec<((BasicBlockId, usize), BasicBlockId)>,
    }

    pub(crate) fn walk(body: &MirBody<'_>) -> Walk {
        let canon = Canon::new(body);
        let order = canon.c.order.clone();
        let numbers = body
            .basic_blocks
            .iter()
            .filter_map(|(id, _)| canon.number_of(id).map(|number| (id, number)))
            .collect();
        let mut unwinds = Vec::new();
        let mut cursor = 0;
        for (number, &id) in order.iter().enumerate() {
            for at in 0..body.basic_blocks[id].statements.len() {
                if let Some(to) = canon.unwind(&mut cursor, number as u32, at) {
                    unwinds.push(((id, at), to));
                }
            }
        }
        Walk { order, numbers, unwinds }
    }

    /// The locals the first walk finds used, by index: with the quiet form it walks with, and
    /// with a form that writes everything, as it walked before.
    pub(crate) fn uses<'db>(
        db: &'db Db,
        body: &MirBody<'db>,
    ) -> (Vec<usize>, Vec<usize>) {
        let used = |mut form: Form| {
            let cx = Cx::new(db);
            let canon = Canon::new(body);
            cx.walk(&mut form, &canon);
            body.locals
                .iter()
                .map(|(local, _)| local)
                .filter(|&local| canon.used(local))
                .map(local_index)
                .collect::<Vec<usize>>()
        };
        (used(Form::quiet()), used(Form::new("")))
    }

    /// The bytes [`Form::debug`] writes, and those of the `format!` it replaced.
    pub(crate) fn debug<T: Debug + ?Sized>(value: &T) -> (Vec<u8>, Vec<u8>) {
        let mut new = Form::new("");
        new.debug(value);
        let mut old = Form::new("");
        old.str(&format!("{value:?}"));
        (new.digest.clone(), old.digest.clone())
    }

    /// `bytes` counted by a length written after them, and by one written before.
    pub(crate) fn counted(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut new = Form::new("");
        let at = new.len_at();
        for &byte in bytes {
            new.u8(byte);
        }
        new.set_len(at, bytes.len());
        let mut old = Form::new("");
        old.len(bytes.len());
        for &byte in bytes {
            old.u8(byte);
        }
        (new.digest.clone(), old.digest.clone())
    }

    /// The bytes and refs of `ids` written as slots, and those of a map of every slot, as
    /// [`Form::item`] kept before it scanned.
    pub(crate) fn slots(ids: &[ItemId]) -> ((Vec<u8>, Vec<ItemId>), (Vec<u8>, Vec<ItemId>)) {
        let mut new = Form::new("");
        for &id in ids {
            new.item(id);
        }
        let mut bytes = Form::new("").digest.clone();
        let mut refs = Vec::new();
        let mut slots = blaze_hash::HashMap::default();
        for &id in ids {
            let next = refs.len() as u32;
            let slot = *slots.entry(id).or_insert(next);
            if slot == next {
                refs.push(id);
            }
            bytes.extend_from_slice(&slot.to_le_bytes());
        }
        ((new.digest.clone(), new.refs.clone()), (bytes, refs))
    }

    /// A form written with `ids` and dropped, then the next one: its digest, spans, refs and
    /// slots, which must hold only its tag.
    pub(crate) fn reused(ids: &[ItemId]) -> (Vec<u8>, Vec<u8>, Vec<ItemId>, usize) {
        {
            let mut form = Form::new("a form with much in it");
            for &id in ids {
                form.item(id);
            }
            form.spans.extend_from_slice(&[1; 64]);
            form.error(super::Missed::ErrorType);
        }
        let next = Form::new("");
        assert_eq!(next.missed, None);
        (next.digest.clone(), next.spans.clone(), next.refs.clone(), next.slots.len())
    }
}
