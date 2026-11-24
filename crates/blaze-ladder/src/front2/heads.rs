//! Step 2's store: the lowered heads of structs, enums, unions, traits, impls and fns, filled
//! on demand by a bounded work list. An item asked for is lowered from the names library's
//! text of it (`lower_signature`, one item at a time, under its crate's cfg) and its types are
//! built by [`Builder`]; a [`Why::Need`] lowers that key's head first and tries the item again.
//! A cycle of needs is [`HeadError::Cycle`]. Trait and impl members are read from the item's
//! outline, each lowered alone, a member its crate's cfg turns off left out.
//!
//! Where-clauses are built by [`super::preds`] into [`Store::preds`]; a trait's predicates and
//! fn signatures are [`Want::Members`], apart from its head; an impl's fn's signature is its
//! own [`Want::MemberFn`], made when that fn is used ([`member_fn`]). Not here yet: `where const`
//! clauses (collected, not keyed), attributes (`#[repr]`, ABI, `no_mangle`), consts, statics.

use std::alloc::Global;
use std::format;
use std::vec::Vec;

use blaze_hash::HashMap;
use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_lower::{CfgAnswer, Lowered, View, form as lf, head};
use blaze_own::Alloc;
use blaze_scope::crate_roots::TextMarks;
use blaze_surface::{ItemDetail, Range};
use blaze_typeck as tc;
use blaze_typeck::{ItemKey, List, Name, PClause, PNode, PTy, PortableTys};

use super::dyn_ty::Member as DynMember;
use super::head_record::{Head, HeadAt};
use super::heads_regions::{self as hreg, Laid, RegionsRefusal, Scope};
use super::item_site::MarksLost;
use super::tables::recorder::{ReadsFull, Recorder, TableRead};
use super::keys::{KeyClash, Keys, MemberKind, checker_of};
use super::preds::Preds;
use super::source::NamesSource;
use super::type_miss::{HeadMiss, MissOf, TypeMiss};
use super::types::{AliasGap, AliasRegions, AliasSlot, Bounds, Builder, Captures, ContainerParamBound, Env, Heads, HeadsArity, NotLaidWhy, ParamBound, RegionRefusal, RegionSink, SelfGiven, SigPart, SelfTy, Slot, SlotKind, TraitAliasHead, Used, Why, intern, intern_list, used_lifetimes};
use blaze_typeck::region_facts::HeadRegions;
use blaze_typeck::regions::{LifetimeParams, Numbering, Region, RegionCol};

/// A heads store's table: in the allocator [`Store::new`] is handed (front2 rows.rs says whose).
type Rows<K, V> = super::rows::Rows<K, V, Alloc<'static>>;
use crate::resolve::Paths;

mod member_fn;
pub use member_fn::{MemberFrame, member_fn_want, member_head_want};

/// The most steps one [`Store::ensure`] takes: past it the needs are a cycle.
const MOST_STEPS: usize = 1 << 16;

/// Why the head of a fn or static declared in a body is refused when it has no body and no
/// `extern` block of that body declares it (`Store::fn_head`): never asked of the names source,
/// which holds no row for an item its body's lowering keys (front2/locals.rs).
const LOCAL_NO_BODY: &str = "a fn or static declared in a body with no body, and no `extern` block of that body declares it (the Reference, [items.fn.body], [items.static.init])";

/// A crate's cfg as blaze-lower asks it: an atom is on when the crate's set holds it.
pub struct CrateCfg<'c>(pub &'c [(&'c str, Option<&'c str>)]);

/// Signature parser invocations over one worker's records. A Bodies stretch reads its own
/// count, including unsuccessful lowerings and a seed that froze a head on that worker.
#[derive(Default)]
pub struct HeadLowerings(core::cell::Cell<u64>);

impl HeadLowerings {
    pub fn total(&self) -> u64 {
        self.0.get()
    }

    pub(crate) fn starting<'n>(names: impl NamesSource<'n>) {
        if let Some(count) = names.head_lowerings() {
            count.0.set(count.total().saturating_add(1));
        }
    }
}

impl blaze_lower::Cfg for CrateCfg<'_> {
    fn atom(&mut self, name: &str, value: Option<&str>) -> CfgAnswer {
        if self.0.iter().any(|&(n, v)| n == name && v == value) { CfgAnswer::Yes } else { CfgAnswer::No }
    }
}

/// Why an item's head could not be made.
#[derive(Debug)]
pub enum HeadError {
    /// The names library holds no text for the item.
    NoText(LibKey),
    /// Its lowering asked for cfg atoms the crate's set does not answer.
    Cfg(LibKey),
    /// The item is no struct, enum, union, trait, impl or fn.
    NotAHead(LibKey, &'static str),
    /// Each item needs the next's head and the last the first's.
    Cycle(Vec<LibKey, Global>),
    /// `by` needs `need`, which was just made, yet its lookup still fails: a lookup asking for
    /// a want that does not make what it reads.
    NeedMade { need: Want, by: Want },
    /// A member's key clashed.
    Key(KeyClash),
    /// The item's types: what and where in its text.
    Item(LibKey, Why),
    /// `by` needs `need`, whose cell the seed ([`Store::ensure_seeded`]) was to file from its
    /// frozen record and could not: the seed's caller holds why (front2/tables/head.rs
    /// `freeze_seeded` returns that error). Never lowered here instead (rule 3).
    Seed { need: Want, by: Want },
    /// Head `at` of cell `want`'s record could not be filed ([`Store::file_heads`]): `what`.
    Filed { want: Want, at: HeadAt, what: &'static str },
}

/// What a seed ([`Store::ensure_seeded`]) did with a need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Seeded {
    /// The need's cell was filed from its frozen record: the need is made.
    Filed,
    /// The need is of the cell being made (its own item), or of one being made up the
    /// seeding stack (two cells whose items need each other's heads): lowered in this store.
    Lower,
}

/// A seed could not file a need's cell; the seed keeps why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeedFailed;

impl core::fmt::Display for HeadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HeadError::NoText(k) => write!(f, "the names library holds no text for item {k:?}"),
            HeadError::Cfg(k) => write!(f, "item {k:?} asks for cfg atoms its crate's set does not answer"),
            HeadError::NotAHead(k, what) => write!(f, "item {k:?} is {what}: no head is made for it under that want"),
            // Each key whole: a key's short form is its crate's part, so two items of one
            // crate printed short read as one item needing itself.
            HeadError::Cycle(ks) => {
                write!(f, "these items need each other's heads in a cycle: [")?;
                for (i, k) in ks.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k:x}")?;
                }
                write!(f, "]")
            }
            HeadError::NeedMade { need, by } => write!(f, "{by:?} needs {need:?}, which is made, yet asks for it again (its lookup reads what that want does not make)"),
            HeadError::Key(c) => write!(f, "{c}"),
            HeadError::Item(k, Why::NeedConst(value, at)) => write!(f, "item {k:x} needs constant {value:x} at {at:?}"),
            HeadError::Item(k, why) => write!(f, "item {k:?}: {why:?}"),
            HeadError::Seed { need, by } => write!(f, "{by:?} needs {need:?}, whose frozen head could not be filed (the seed holds why)"),
            HeadError::Filed { want, at, what } => write!(f, "the frozen head of {want:?}, head {at:?}: {what}"),
        }
    }
}

/// A trait's head: its generics (`Self` first), its type parameters' defaults (after `Self`),
/// its associated types and its fns by name.
pub struct TraitHead {
    pub generics: u32,
    pub all_self_defaults: bool,
    /// The marks on the principal trait of a trait object's inherent impl.
    pub marks: CoherenceMarks,
    pub defaults: Vec<Option<PTy>, Global>,
    pub assoc: Vec<Name, Global>,
    pub fns: Vec<(Name, ItemKey), Global>,
    /// The checker's flags read from the header: `auto` (which is coinductive too), `unsafe`.
    pub flags: tc::TraitFlags,
    /// Its members' dyn-compatibility facts, handed to the checker as `TraitDef.members`
    /// (the checker decides: `blaze_typeck::dyn_compat`).
    pub members: Vec<tc::dyn_compat::MemberFacts, Global>,
}

/// Whether every parameter after a trait's implicit `Self` defaults to `Self`.
pub(crate) fn all_self_defaults(defaults: &[Option<PTy>], tys: tc::PTys<'_>) -> bool {
    defaults.iter().all(|d| d.is_some_and(|p| matches!(tys.get(p), Some(PNode::Param(0)))))
}

/// The heads [`Builder`] reads.
pub struct Maps {
    /// Each struct's, enum's and union's type parameter count and their defaults (over its
    /// own parameters, `None` where none is written).
    pub generics: Rows<LibKey, (u32, Vec<Option<PTy>, Global>)>,
    pub traits: Rows<LibKey, TraitHead>,
    /// Each trait's associated type's count of its OWN type and const parameters (`type
    /// MapStorage<V>`: one), by the trait and the name, filed with the trait's members
    /// ([`Heads::assoc_generics`]): what `<K as Key>::MapStorage<V>` is checked against.
    pub assoc_generics: Rows<(LibKey, Name), u32>,
    /// The outlives where clauses of each trait's associated type that writes one (`type
    /// Iter<'a> where Self: 'a;`), by the trait and the name: read and kept, not checked
    /// (gat.rs [`super::gat::OUTLIVES_NOT_CHECKED`]).
    pub assoc_where: Rows<(LibKey, Name), super::gat::AssocWhere>,
    /// Each trait's associated type's own const parameters (own position, declared type), by
    /// the trait and the name, filed with the trait's members ([`Heads::assoc_const_params`]):
    /// the type a constant written in `Tr<Buf<3>: Bound>` is made at (gat_bounds.rs).
    pub assoc_consts: Rows<(LibKey, Name), Vec<(u32, PTy), Global>>,
    /// Each trait's supertraits written as bounds on `Self` (see [`Heads::trait_supers`]):
    /// made with its members ([`Want::Members`]), since a supertrait's head may need the
    /// trait's (`FromResidual<R = <Self as Try>::Residual>`).
    pub supers: Rows<LibKey, Vec<(LibKey, List), Global>>,
    /// Each type alias's generic count and aliased type ([`Heads::alias`]); `None` where its
    /// head was refused, with the why in `alias_gaps`: an alias no path uses never fails a
    /// check, and one that is used fails only the item that names it.
    pub aliases: Rows<LibKey, Option<(u32, PTy)>>,
    /// Each type alias's parameter defaults (over its own parameters, `None` where none is
    /// written), made with its head: `type Result<T, E = Error>`.
    pub alias_defaults: Rows<LibKey, Vec<Option<PTy>, Global>>,
    /// Each type alias's type laid over its own lifetimes ([`Heads::alias_slots`]), made with
    /// its head (`alias_slots`) or filed from its record; absent where its head laid none.
    pub alias_slots: Rows<LibKey, AliasRegions>,
    pub alias_gaps: Rows<LibKey, AliasGap>,
    /// Each trait alias's head ([`Heads::trait_alias`]; made by `Store::trait_alias`).
    pub trait_aliases: Rows<LibKey, TraitAliasHead>,
    /// Each `usize` constant item a head's array length names, by its value
    /// ([`Why::NeedConst`]): filled by the driver that has the constant evaluator.
    pub const_lens: HashMap<LibKey, u64, Global>,
    /// Each evaluated constant a head names, by its value's bits in its OWN type's width (an
    /// `i8` of -8 is `0xf8`; a `u128` or `i128` whole), as blaze-consteval writes them and as
    /// `PNode::ConstValue` holds them: the reader knows the type and extends by its sign.
    /// Keyed by a constant item's key or an anonymous constant's need key
    /// ([`super::anon::need_key`]); filled by `check::eval_len` beside `const_lens`, which
    /// holds only those that fit 64 bits.
    pub const_bits: HashMap<LibKey, u128, Global>,
    /// The current body's defining-text reads. A store shared across body tasks
    /// also keeps the source reads of values it can hand to a subsequent task.
    pub(super) const_text_reads: Option<Recorder<'static>>,
    pub(super) const_proofs: Option<ConstProofs>,
    /// The anonymous constants the type builder asked for and no head holds yet
    /// ([`Heads::ask_anon`]): filed as heads by [`super::anon::file_asked`], which
    /// `check::eval_len` calls before it evaluates. In asking order; a repeat is skipped there.
    pub anon_asked: core::cell::RefCell<Vec<super::anon::Ask, Global>>,
    /// Each anonymous constant the evaluator refused, by its need key, with the refusal as
    /// printed ([`Heads::anon_failed`]): the head that wrote it is refused naming both.
    pub anon_failed: HashMap<LibKey, std::boxed::Box<str, Global>, Global>,
    /// The qualified constant leaves the type builder asked for (`<B4 as Spec>::BITS`:
    /// [`Heads::ask_qpath`]) and each answered one's type; answered by
    /// [`super::const_qpath::eval_asked`], which `check::eval_len` calls. A value is in
    /// `const_bits` and a refusal in `anon_failed`, under the leaf's need key.
    pub qpaths: core::cell::RefCell<super::const_qpath::Table>,
    /// The inherent constant leaves the type builder asked for (`Self::MAX_SIZE`,
    /// `InlineExtension::MAX`: [`Heads::inherent_leaves`]), each one's state and type;
    /// answered by [`super::const_inherent::eval_asked`] (`check::eval_len`) or
    /// [`super::const_inherent::first`] (the evaluator's work list). A value is in
    /// `const_bits` and a refusal in `anon_failed`, under the leaf's need key.
    pub inherents: core::cell::RefCell<super::const_inherent::Table>,
    /// Each item's const generic parameters (index, declared type), sorted by index: a
    /// struct's, enum's, union's, trait's and alias's own (made with its head, what
    /// [`Heads::const_params`] reads), an impl's own, and a fn's with its container's first
    /// (the checker's `const_params`). An item with none has an empty row once made.
    pub const_params: Rows<LibKey, Vec<(u32, PTy), Global>>,
    /// Each enum with no generic parameters whose every variant is a unit variant, by its
    /// checker key, with its variant count: what a const parameter's type may be past the
    /// integers, `char` and `bool` ([`super::const_enum`]). Filed by `Store::adt_head` with
    /// the enum's generics, so before any path to the enum has a type.
    pub fieldless_enums: Rows<ItemKey, u32>,
    /// How many lowerings of a written bound's arguments are nested right now
    /// ([`Heads::written_depth`]): zero between any two heads.
    pub written_depth: core::cell::Cell<u32>,
    /// How many constant evaluations are running now (consts.rs `Consts::value_at` counts
    /// itself in and out): zero where no evaluator is at work. `Records::consts` is `None`
    /// both while one runs and for a caller that has no evaluator at all; this tells the two
    /// apart (check.rs `make`: only the second refuses a head by name).
    pub evaluating: core::cell::Cell<u32>,
    /// What the one region counter (`regions::region_arity`) reads that a portable node does
    /// not hold, by checker key (D148): each ADT's and trait's declared lifetime count, each
    /// trait's associated type's own lifetime parameters, each opaque's captured lifetimes.
    /// Filed with the head that declares them; one not filed is `Count::NotKnown`, never 0.
    pub adt_lifetimes: Rows<ItemKey, u32>,
    pub trait_lifetimes: Rows<ItemKey, u32>,
    pub assoc_lifetimes: Rows<(ItemKey, Name), u32>,
    pub opaque_lifetimes: Rows<ItemKey, u32>,
    /// Canonical source spellings of structural record fields lowered into this store
    /// (`Name -> text`), used by structural record type spread (`{ ..Base, extra: T }`) to sort
    /// merged fields alphabetically by source spelling rather than by `Name` hash.
    pub record_field_names: Rows<Name, blaze_own::String<Alloc<'static>>>,
    /// The tables' allocator ([`Store::new`]'s): what an asked anonymous constant's marks
    /// are copied into ([`Heads::mark_alloc`]).
    alloc: Alloc<'static>,
}

impl Maps {
    /// Empty maps; the tables in `alloc` ([`Store::new`]'s).
    pub fn new_in(alloc: Alloc<'static>) -> Maps {
        Maps {
            alloc,
            generics: Rows::new_in(alloc),
            traits: Rows::new_in(alloc),
            assoc_generics: Rows::new_in(alloc),
            assoc_where: Rows::new_in(alloc),
            assoc_consts: Rows::new_in(alloc),
            supers: Rows::new_in(alloc),
            aliases: Rows::new_in(alloc),
            alias_defaults: Rows::new_in(alloc),
            alias_slots: Rows::new_in(alloc),
            alias_gaps: Rows::new_in(alloc),
            trait_aliases: Rows::new_in(alloc),
            const_lens: Default::default(),
            const_bits: Default::default(),
            const_text_reads: None,
            const_proofs: None,
            anon_asked: Default::default(),
            anon_failed: Default::default(),
            qpaths: Default::default(),
            inherents: core::cell::RefCell::new(super::const_inherent::Table::new_in(alloc)),
            const_params: Rows::new_in(alloc),
            fieldless_enums: Rows::new_in(alloc),
            written_depth: Default::default(),
            evaluating: Default::default(),
            adt_lifetimes: Rows::new_in(alloc),
            trait_lifetimes: Rows::new_in(alloc),
            assoc_lifetimes: Rows::new_in(alloc),
            opaque_lifetimes: Rows::new_in(alloc),
            record_field_names: Rows::new_in(alloc),
        }
    }
}

pub(super) type ConstInputs = Result<blaze_own::AVec<'static, TableRead>, ReadsFull>;

pub(super) struct ConstCapture(Option<Recorder<'static>>);

pub(super) struct ConstProofs {
    rows: blaze_own::AVec<'static, ConstInputs>,
    by_key: HashMap<LibKey, usize, Alloc<'static>>,
    pub(super) filled: blaze_own::AVec<'static, LibKey>,
}

impl ConstProofs {
    pub(super) fn new(alloc: Alloc<'static>) -> Self {
        Self { rows: blaze_own::AVec::new_in(alloc), by_key: HashMap::new_in(alloc), filled: blaze_own::AVec::new_in(alloc) }
    }

    /// Batched values share the proof of the evaluation that produced them.
    pub(super) fn finish(&mut self, start: usize, key: LibKey, inputs: ConstInputs) {
        let at = self.rows.len();
        self.rows.push(inputs);
        self.by_key.insert(key, at);
        for derived in self.filled.drain(start..) {
            self.by_key.insert(derived, at);
        }
    }
}

impl Maps {
    pub(super) fn capture_const(&mut self, alloc: Alloc<'static>) -> Option<ConstCapture> {
        self.const_proofs.as_ref()?;
        let recorder = Recorder::new(super::tables::recorder::RecorderOpts { alloc });
        Some(ConstCapture(self.const_text_reads.replace(recorder)))
    }

    pub(super) fn finish_capture(&mut self, capture: Option<ConstCapture>) -> Option<ConstInputs> {
        let outer = capture?.0;
        let inner = core::mem::replace(&mut self.const_text_reads, outer).expect("the captured constant's recorder");
        let inputs = inner.distinct().map(|(reads, _)| reads);
        self.read_inputs(Some(&inputs));
        Some(inputs)
    }

    pub(super) fn read_inputs(&self, inputs: Option<&ConstInputs>) {
        if let (Some(recorder), Some(inputs)) = (&self.const_text_reads, inputs) {
            recorder.replay(inputs.as_deref().map_err(|e| *e));
        }
    }

    pub(super) fn noted_const_result(&mut self, key: LibKey) {
        if let Some(proofs) = &mut self.const_proofs {
            proofs.filled.push(key);
        }
    }

    pub(super) fn file_bits(&mut self, key: LibKey, value: u128) {
        self.noted_const_result(key);
        self.const_bits.insert(key, value);
    }

    fn replay_const(&self, key: LibKey) {
        if let (Some(reads), Some(proofs)) = (&self.const_text_reads, &self.const_proofs)
            && let Some(&at) = proofs.by_key.get(&key)
        {
            reads.replay(proofs.rows[at].as_deref().map_err(|e| *e));
        }
    }
}

impl Heads for Maps {
    fn written_depth(&self) -> Option<&core::cell::Cell<u32>> {
        Some(&self.written_depth)
    }
    fn adt_lifetimes(&self, adt: ItemKey) -> Option<u32> {
        self.adt_lifetimes.get(&adt).copied()
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> Option<u32> {
        self.trait_lifetimes.get(&trait_).copied()
    }
    fn assoc_lifetimes(&self, trait_: ItemKey, name: Name) -> Option<u32> {
        self.assoc_lifetimes.get(&(trait_, name)).copied()
    }
    fn opaque_lifetimes(&self, opaque: ItemKey) -> Option<u32> {
        self.opaque_lifetimes.get(&opaque).copied()
    }
    fn note_record_field_name(&self, name: Name, text: &str) {
        if !self.record_field_names.contains_key(&name) {
            let mut owned = blaze_own::String::with_capacity_in(text.len(), self.alloc);
            owned.push_str(text);
            self.record_field_names.insert_absent(name, owned);
        }
    }
    fn record_field_name(&self, name: Name) -> Option<&str> {
        self.record_field_names.get(&name).map(|s| s.as_str())
    }
    fn const_len(&self, key: LibKey) -> Option<u64> {
        self.replay_const(key);
        self.const_lens.get(&key).copied()
    }
    fn const_bits(&self, key: LibKey) -> Option<u128> {
        self.replay_const(key);
        self.const_bits.get(&key).copied().or_else(|| self.const_lens.get(&key).map(|&v| u128::from(v)))
    }
    fn anon_failed(&self, key: LibKey) -> Option<&str> {
        let why = self.anon_failed.get(&key)?;
        self.replay_const(key);
        Some(why)
    }
    fn mark_alloc(&self) -> Option<Alloc<'static>> {
        Some(self.alloc)
    }
    /// Kept for `check::eval_len` to file; one asked twice before then is kept once.
    fn ask_anon(&self, ask: super::anon::Ask) -> bool {
        let mut asked = self.anon_asked.borrow_mut();
        if !asked.iter().any(|a| a.need == ask.need) {
            asked.push(ask);
        }
        true
    }
    /// Kept for `check::eval_len` to answer ([`super::const_qpath::eval_asked`]).
    fn ask_qpath(&self, ask: super::const_qpath::QAsk) -> bool {
        self.qpaths.borrow_mut().ask(ask);
        true
    }
    fn qpath_ty(&self, need: LibKey) -> Option<PNode> {
        self.qpaths.borrow().ty(need)
    }
    fn inherent_leaves(&self) -> Option<&core::cell::RefCell<super::const_inherent::Table>> {
        Some(&self.inherents)
    }
    fn adt_generics(&self, adt: LibKey) -> Option<(u32, &[Option<PTy>])> {
        self.generics.get(&adt).map(|(g, d)| (*g, &d[..]))
    }
    fn trait_head(&self, trait_: LibKey) -> Option<(u32, &[Option<PTy>])> {
        self.traits.get(&trait_).map(|t| (t.generics, &t.defaults[..]))
    }
    fn alias(&self, alias: LibKey) -> Option<Option<(u32, PTy)>> {
        self.aliases.get(&alias).copied()
    }
    fn alias_defaults(&self, alias: LibKey) -> &[Option<PTy>] {
        self.alias_defaults.get(&alias).map_or(&[], |d| &d[..])
    }
    fn alias_slots(&self, alias: LibKey) -> Option<&AliasRegions> {
        self.alias_slots.get(&alias)
    }
    fn alias_gap(&self, alias: LibKey) -> Option<&AliasGap> {
        self.alias_gaps.get(&alias)
    }
    fn trait_alias(&self, alias: LibKey) -> Option<&TraitAliasHead> {
        self.trait_aliases.get(&alias)
    }
    fn trait_supers(&self, trait_: LibKey) -> Option<&[(LibKey, List)]> {
        self.supers.get(&trait_).map(|s| &s[..])
    }
    fn trait_has_assoc(&self, trait_: LibKey, name: Name) -> Option<bool> {
        self.traits.get(&trait_).map(|t| t.assoc.contains(&name))
    }
    fn assoc_generics(&self, trait_: LibKey, name: Name) -> Option<u32> {
        self.assoc_generics.get(&(trait_, name)).copied()
    }
    fn assoc_const_params(&self, trait_: LibKey, name: Name) -> Option<&[(u32, PTy)]> {
        self.assoc_consts.get(&(trait_, name)).map(|c| &c[..])
    }
    fn trait_auto(&self, trait_: LibKey) -> Option<bool> {
        self.traits.get(&trait_).map(|t| t.flags.has(tc::TraitFlags::AUTO))
    }
    fn trait_self_static(&self, trait_: LibKey) -> Option<bool> {
        self.traits.get(&trait_).map(|t| t.flags.has(tc::TraitFlags::SELF_STATIC))
    }
    fn checker(&self, key: LibKey) -> ItemKey {
        checker_of(key)
    }
    fn const_params(&self, item: LibKey) -> Option<&[(u32, PTy)]> {
        self.const_params.get(&item).map(|c| &c[..])
    }
}

/// Whether a lowered struct view `v` of item `key` (in `module`) is eligible for Blaze-edition
/// automatic `Copy`/`Clone` (`AdtFlags::AUTO_COPY_PUB`):
/// - defined in a crate on Blaze edition (`StdIs::Bstd(_)`),
/// - `AdtKind::Struct`,
/// - if it has fields: every field is `pub` (`lf::Vis::Pub`),
/// - if it has no fields (unit/empty struct): the struct itself is `pub` (`lf::Vis::Pub`).
pub(crate) fn is_auto_copy_pub_view<'n, S: NamesSource<'n>>(
    names: S,
    key: LibKey,
    module: LibKey,
    kind: tc::AdtKind,
    v: &View<'_>,
) -> Result<bool, Why> {
    if kind != tc::AdtKind::Struct {
        return Ok(false);
    }
    let Some(krate) = names.crate_of(key).or_else(|| names.crate_of(module)) else {
        return Ok(false);
    };
    if !matches!(names.std_is(krate), Some(blaze_unit_graph::StdIs::Bstd(_))) {
        return Ok(false);
    }
    let field_rows = v.rows(v.list(head::FIELDS))?;
    if field_rows.is_empty() {
        return Ok(v.vis() == lf::Vis::Pub);
    }
    for &n in field_rows {
        let f = lf::Field::decode(n)?;
        if f.vis != lf::Vis::Pub {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a field default (`a: u8 = 1`, RFC 3681 `default_field_values`) may be written on
/// a field of `adt`'s variant of constructor `ctor`, at `r`:
/// - only on a named field of a struct or an enum's struct variant: a tuple struct's or tuple
///   variant's field and a union's field are refused, each by name (rustc's "default fields
///   are not supported in tuple structs" and "unions cannot have default field values");
/// - in a blaze edition (the crate's `std` is bstd) the feature is on unasked (D126); in a
///   Rust edition it needs `#![feature(default_field_values)]` at the crate root (the crate
///   root record's `features`, `NamesSource::has_feature`), refused naming the feature
///   without it.
pub(crate) fn field_default_allowed<'n, S: NamesSource<'n>>(names: S, adt: LibKey, kind: tc::AdtKind, ctor: tc::CtorKind, r: Range) -> Result<(), Why> {
    if kind == tc::AdtKind::Union {
        return Err(Why::Program(Box::from("a union's field may not have a default value (RFC 3681: unions cannot have default field values)"), r));
    }
    if ctor != tc::CtorKind::Braces {
        return Err(Why::Program(Box::from("a tuple struct's or tuple variant's field may not have a default value (RFC 3681: only a named field has one)"), r));
    }
    let Some(krate) = names.crate_of(adt) else {
        return Err(Why::Program(Box::from("a field default: the item's crate is not answered"), r));
    };
    match (names.std_is(krate), names.has_feature(krate, "default_field_values")) {
        (Some(blaze_unit_graph::StdIs::Bstd(_)), _) | (Some(blaze_unit_graph::StdIs::Rust), Some(true)) => Ok(()),
        (Some(blaze_unit_graph::StdIs::Rust), Some(false)) => Err(Why::Program(
            Box::from("a field default needs `#![feature(default_field_values)]` at the crate root in a Rust edition (on unasked in a blaze edition)"),
            r,
        )),
        (Some(blaze_unit_graph::StdIs::Rust), None) => Err(Why::Program(Box::from("a field default: the crate root's features are not answered"), r)),
        (None, _) => Err(Why::Program(Box::from("a field default: the crate's edition (which `std` it names) is not answered"), r)),
    }
}

/// An ADT head's own syntax errors (its lowering's `Syntax` findings) refuse it at their place
/// (rule 3: a head is never made from text that does not parse). The parser takes `= value`
/// only after a NAMED field (RFC 3681), so a tuple field's `u8 = 1` is a syntax error whose
/// rest begins with `=`: refused by name as [`field_default_allowed`] refuses one.
fn adt_syntax(v: &View<'_>) -> Result<(), Why> {
    let Some(d) = v.diags().next() else { return Ok(()) };
    let d = d?;
    let r = v.site(d.range)?;
    // The field the error is in: from the `(` or `,` before it to the `,` or `)` after it (the
    // parser stops at the `=` or past its value, by how it recovers).
    let text = v.item();
    let at = (r.start as usize).min(text.len());
    let from = text.get(..at).and_then(|t| t.rfind(['(', ','])).map_or(0, |i| i + 1);
    let to = text.get(at..).and_then(|t| t.find([',', ')'])).map_or(text.len(), |i| at + i);
    if let Some(eq) = text.get(from..to).and_then(|piece| piece.find('=')) {
        let start = (from + eq) as u32;
        return Err(Why::Program(
            Box::from("a tuple struct's or tuple variant's field may not have a default value (RFC 3681: only a named field has one)"),
            Range::new(start, to as u32),
        ));
    }
    let near = text.get(from..to).unwrap_or("");
    Err(Why::Program(format!("the item's text does not parse here ({:?}, in `{near}`)", d.kind).into_boxed_str(), r))
}

/// Item attributes used by the orphan and inherent-impl rules.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoherenceMarks {
    pub fundamental: bool,
    pub incoherent: bool,
}

impl CoherenceMarks {
    fn attribute(&mut self, path: &str) {
        match path {
            "fundamental" => self.fundamental = true,
            "rustc_has_incoherent_inherent_impls" => self.incoherent = true,
            _ => {}
        }
    }
}

/// An item's `#[unstable(feature = "..", implied_by = "..")]`, as written: the library feature
/// that a crate turns on with `#![feature(..)]` to use the item (library/std/src/collections/
/// hash/map.rs: `#[unstable(feature = "map_try_insert", issue = "82766")] pub fn try_insert`;
/// library/alloc/src/vec/mod.rs: `#[unstable(feature = "allocator_ext", issue = "163177",
/// implied_by = "allocator_api")] pub fn try_with_capacity_in`). Method lookup reads it through
/// `World::member_unstable` (rustc_hir_typeck method/probe.rs `consider_candidates`: a
/// candidate whose `eval_stability` is `Deny` is dropped before the pick, and only when nothing
/// else is picked is the search run again with it). `issue`, `reason`, `soft` and `old_name`
/// change no pick and are not kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unstable {
    pub feature: Box<str>,
    /// rustc_middle stability.rs `eval_stability_allow_unstable`: "If this item was previously
    /// part of a now-stabilized feature which is still enabled .. then allow use of this item."
    pub implied_by: Option<Box<str>>,
}

/// An item's stability as its own attributes write it (rustc_passes stability.rs
/// `lookup_stability`: an item's own `#[stable]`/`#[unstable]`, else its parent's when that
/// is unstable).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Marked {
    Unmarked,
    Stable,
    Unstable(Unstable),
}

impl Marked {
    /// The stability attributes of the item `v` is the view of, read from its attribute rows
    /// (the path's text, then the arguments' tokens: never a scan of the item's text). Two of
    /// them on one item is rustc's E0544, refused by name.
    pub(super) fn of(v: &View<'_>) -> Result<Marked, Why> {
        let mut marked = Marked::Unmarked;
        for &row in v.rows(v.list(head::ATTRS))? {
            let attr = v.attr(row)?;
            let this = match v.text_at(attr.path)?.trim() {
                "stable" => Marked::Stable,
                "unstable" => Marked::Unstable(Unstable::of_args(v.text_at(attr.args)?)?),
                _ => continue,
            };
            if marked != Marked::Unmarked {
                return Err(Why::Program(Box::from("an item with two stability attributes (rustc E0544, \"multiple stability levels\")"), Range::new(0, 0)));
            }
            marked = this;
        }
        Ok(marked)
    }

    /// The stability of an item marked `self` whose parent is marked `parent`: its own, else
    /// the parent's when that is unstable (rustc_passes stability.rs `lookup_stability`:
    /// `if parent.is_unstable() .. { return Some(parent) }`). `None`: not unstable.
    pub(super) fn unstable_under(&self, parent: &Marked) -> Option<Unstable> {
        match (self, parent) {
            (Marked::Unstable(u), _) | (Marked::Unmarked, Marked::Unstable(u)) => Some(u.clone()),
            _ => None,
        }
    }
}

impl Unstable {
    /// `(feature = "x", issue = "n", implied_by = "y", soft)` (or the same without the
    /// parentheses), from its tokens: each meta item an identifier, then `= "value"` or
    /// nothing, separated by commas. A `feature` is required (rustc's E0546 "missing
    /// 'feature'"); a value with an escape or a raw string is refused as unsupported (std's
    /// feature names are plain identifiers).
    fn of_args(args: &str) -> Result<Unstable, Why> {
        use parser::SyntaxKind as K;
        let at = Range::new(0, 0);
        let bad = |what: &'static str| Why::Program(format!("an `#[unstable(..)]` attribute: {what}").into_boxed_str(), at);
        let lexed = parser::LexedStr::new(parser::Edition::Edition2024, args);
        let mut i = 0usize;
        let mut next = || {
            while i < lexed.len() {
                i += 1;
                if !lexed.kind(i - 1).is_trivia() {
                    return Some(i - 1);
                }
            }
            None
        };
        let (mut feature, mut implied_by) = (None, None);
        let mut tok = next();
        let parens = tok.is_some_and(|t| lexed.kind(t) == K::L_PAREN);
        if parens {
            tok = next();
        }
        loop {
            let Some(t) = tok else {
                if parens {
                    return Err(bad("no `)` closes its arguments"));
                }
                break;
            };
            match lexed.kind(t) {
                K::R_PAREN if parens => {
                    if next().is_some() {
                        return Err(bad("tokens after its `)`"));
                    }
                    break;
                }
                K::IDENT => {}
                _ => return Err(bad("a meta item that is not a name")),
            }
            if lexed.error(t).is_some() {
                return Err(bad("a token that does not lex"));
            }
            let key = lexed.text(t);
            tok = next();
            let mut value = None;
            if tok.is_some_and(|e| lexed.kind(e) == K::EQ) {
                let Some(s) = next().filter(|&s| lexed.kind(s) == K::STRING && lexed.error(s).is_none()) else {
                    return Err(bad("a `=` not followed by a string"));
                };
                value = Some(lexed.text(s));
                tok = next();
            }
            // Only a feature's name is read: a `reason` may be raw or hold escapes (a line
            // continuation), and is not kept.
            let name = |text: &str| -> Result<Box<str>, Why> {
                let inner = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')).ok_or_else(|| bad("a feature name written as a raw string"))?;
                if inner.contains(['\\', '\r']) {
                    return Err(Why::Unsupported("an `#[unstable(..)]` feature name with an escape", at));
                }
                Ok(Box::from(inner))
            };
            match (key, value) {
                ("feature", Some(v)) => feature = Some(name(v)?),
                ("implied_by", Some(v)) => implied_by = Some(name(v)?),
                ("feature" | "implied_by", None) => return Err(bad("`feature` or `implied_by` with no value")),
                _ => {}
            }
            match tok {
                Some(c) if lexed.kind(c) == K::COMMA => tok = next(),
                Some(c) if parens && lexed.kind(c) == K::R_PAREN => {}
                None if !parens => {}
                _ => return Err(bad("meta items not separated by commas")),
            }
        }
        let feature = feature.ok_or_else(|| bad("no `feature` (rustc E0546)"))?;
        Ok(Unstable { feature, implied_by })
    }
}

/// Facts of the nominal declaration that are already known before any field type is built.
#[derive(Clone, Copy)]
pub struct AdtDeclaration {
    pub lifetimes: u32,
    pub marks: CoherenceMarks,
}

/// A struct, enum or union: the checker's `Adt` parts.
pub struct AdtRow {
    pub kind: tc::AdtKind,
    pub generics: u32,
    pub marks: CoherenceMarks,
    /// Declared lifetime parameters, in declaration order, separate from type and const parameters.
    pub lifetimes: u32,
    /// One region row per field in variant order, shared with the borrow checker.
    pub field_regions: super::borrows::AdtFieldRegions,
    pub variants: Vec<tc::Variant, Global>,
    pub fields: Vec<tc::Field, Global>,
    /// Its `#[repr(..)]`: what layout reads.
    pub repr: blaze_build::layout::ReprAttrs,
    /// Its `#[rustc_layout_scalar_valid_range_start(s)]` and `..._end(e)`, as written (each
    /// `None` when absent): the ONE fact of a narrowed valid range (rule 30) that layout's
    /// `valid_range` answers (blaze-build layout.rs narrows the struct's one scalar by it; a
    /// pattern type's range is its type's, `PNode::Pat`, never here). Read by [`valid_range_of`].
    pub valid: blaze_build::layout::ValidRange,
    /// An enum's discriminant by variant, explicit or implicit, in its discriminant type
    /// ([`discr_ty`]); empty for a struct or union. Layout's `discriminant` answers from it.
    pub discrs: Vec<i128, Global>,
    /// Marked `#[blaze::args]` (rule 83, D126): an options struct, a call's named arguments and
    /// nothing else. The world answers it as [`tc::TypeAttrs::ARGS`] (world_items.rs `attrs`),
    /// which the body check's `args-inline` reads (blaze-typeck rules/args_inline.rs); the head
    /// record keeps it, so an ADT of any crate is known.
    pub args: bool,
}

/// Reads one `#[rustc_layout_scalar_valid_range_start(n)]` or `..._end(n)` (its path `path` as
/// written, its argument text `args`, `(n)` or `n`) of an ADT of `kind` into `out`. rustc's rule
/// (rustc_middle `layout_scalar_valid_range`): the attribute takes one integer literal, sits on a
/// struct, and bounds the struct's one scalar; the end is inclusive. The literal is any radix
/// with `_`s and a suffix, never negative; a repeated bound, another suffix of the path, a union
/// or enum, or a value past `u128` is refused by name (layout refuses a struct that is not one
/// scalar or one wide pointer).
pub(crate) fn valid_range_of(path: &str, args: &str, kind: tc::AdtKind, out: &mut blaze_build::layout::ValidRange) -> Result<(), &'static str> {
    let which = path.strip_prefix("rustc_layout_scalar_valid_range_").map(str::trim);
    if kind != tc::AdtKind::Struct {
        return Err("a `rustc_layout_scalar_valid_range_*` attribute on an enum or union (rustc takes it on a struct only)");
    }
    let args = args.trim();
    let inner = args.strip_prefix('(').and_then(|a| a.strip_suffix(')')).unwrap_or(args).trim();
    if inner.starts_with('-') || inner.contains(',') {
        return Err("a `rustc_layout_scalar_valid_range_*` bound that is not one non-negative integer literal");
    }
    let value = crate::front::int_value(inner).ok_or("a `rustc_layout_scalar_valid_range_*` bound that is not one integer literal")?;
    let slot = match which {
        Some("start") => &mut out.start,
        Some("end") => &mut out.end,
        _ => return Err("a `rustc_layout_scalar_valid_range_*` attribute other than `_start` and `_end`"),
    };
    if slot.is_some() {
        return Err("a `rustc_layout_scalar_valid_range_*` bound written twice");
    }
    *slot = Some(value);
    Ok(())
}

/// Whether an attribute's path, as written, is `blaze::args` (spaces around `::` allowed).
fn is_args_attr(path: &str) -> bool {
    path.split_once("::").is_some_and(|(tool, rest)| tool.trim() == "blaze" && rest.trim() == "args")
}

/// An enum's discriminant type (the Reference, "Enumerations", "Discriminants": "Under the
/// default representation, the discriminant is interpreted as an `isize` value"; a primitive
/// representation `#[repr(u8)]` makes it that integer type), as the checker's `DiscrRepr`.
pub fn discr_ty(repr: blaze_build::layout::ReprAttrs) -> tc::DiscrRepr {
    match repr.int {
        Some(blaze_build::layout::IntRepr::Signed(i)) => tc::DiscrRepr::Int(i),
        Some(blaze_build::layout::IntRepr::Unsigned(u)) => tc::DiscrRepr::Uint(u),
        None => tc::DiscrRepr::Int(tc::IntTy::Isize),
    }
}

/// Each variant's discriminant (the Reference, "Enumerations", "Discriminants"): a written
/// one is a constant expression of the discriminant type ([`Builder::const_term`]: a literal or
/// a constant item; another expression is [`Why::ConstPath`] naming it); an implicit one is
/// the previous variant's plus one (the first's is 0). Two variants with one value are the
/// language's error (E0081), as is one past the type's range.
fn discriminants<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, repr: blaze_build::layout::ReprAttrs, written: &[(Option<lf::ConstIdx>, Range)]) -> Result<Vec<i128, Global>, Why> {
    let mut out: Vec<i128, Global> = Vec::with_capacity_in(written.len(), Global);
    if written.is_empty() {
        return Ok(out);
    }
    let (node, signed) = match discr_ty(repr) {
        tc::DiscrRepr::Int(i) => (PNode::Int(i), true),
        tc::DiscrRepr::Uint(u) => (PNode::Uint(u), false),
        tc::DiscrRepr::Unread => return Err(Why::Program("an enum with no discriminant type".into(), written[0].1)),
    };
    let ty = intern(tys, node)?;
    let Some(&(shown, _)) = super::types::PRIMS.iter().find(|(_, n)| *n == node) else {
        return Err(Why::Program("a discriminant type with no name".into(), written[0].1));
    };
    let bits: u32 = match shown {
        "i8" | "u8" => 8,
        "i16" | "u16" => 16,
        "i32" | "u32" => 32,
        "i128" | "u128" => 128,
        _ => 64,
    };
    let (least, most): (i128, i128) = match (signed, bits) {
        (true, 128) => (i128::MIN, i128::MAX),
        (true, w) => (-(1i128 << (w - 1)), (1i128 << (w - 1)) - 1),
        // A `u128` past `i128::MAX` is refused below by name (`i128` is layout's question).
        (false, 128) => (0, i128::MAX),
        (false, w) => (0, (1i128 << w) - 1),
    };
    // A written discriminant that is a constant not evaluated yet (a constant item, or an
    // expression `1 << 0`: an anonymous constant, front2/anon.rs) is a need; EVERY written one
    // is asked before the first need is returned, so the driver evaluates them in one round.
    let mut need: Option<Why> = None;
    for (k, &(c, r)) in written.iter().enumerate() {
        let value = match c {
            Some(c) => {
                let at = v.const_range(c)?;
                let cr = v.site(at)?;
                let made = match b.const_term(tys, env, v.text_at(at)?, cr, &|_| Ok(ty)) {
                    Ok(made) => made,
                    Err(why @ Why::NeedConst(..)) => {
                        need.get_or_insert(why);
                        continue;
                    }
                    Err(why) => return Err(why),
                };
                if need.is_some() {
                    continue;
                }
                let Some(&PNode::ConstValue { bits: raw, .. }) = tys.view().get(made) else {
                    return Err(Why::ConstPath(Box::from(v.text_at(at)?.trim()), cr));
                };
                // The bits are the value's in the type's width: sign-extend a signed one.
                if signed && bits == 128 {
                    raw as i128
                } else if signed && raw >> (bits - 1) & 1 == 1 {
                    (raw | !((1u128 << bits) - 1)) as i128
                } else {
                    i128::try_from(raw).map_err(|_| Why::Unsupported("a `u128` discriminant past `i128::MAX`", cr))?
                }
            }
            // After a need the values so far are not all known: nothing is judged this pass.
            None if need.is_some() => continue,
            None => match out.last() {
                None => 0,
                Some(&prev) if prev < most => prev + 1,
                Some(_) => return Err(Why::Program(format!("enum discriminant overflowed: the variant after the value {most} has no value of `{shown}` (E0370)").into_boxed_str(), r)),
            },
        };
        if value < least || value > most {
            return Err(Why::Program(format!("the discriminant {value} is not a `{shown}`").into_boxed_str(), r));
        }
        if let Some(j) = out.iter().position(|&o| o == value) {
            return Err(Why::Program(format!("discriminant value `{value}` assigned more than once (E0081): variants {j} and {k}").into_boxed_str(), r));
        }
        out.push(value);
    }
    if let Some(why) = need {
        return Err(why);
    }
    Ok(out)
}

/// `#[rustc_legacy_const_generics(..)]`'s arguments (`args`, the parenthesized list): the
/// unsuffixed decimal positions, comma separated, appended to `out` in written order (the
/// checker binds them to the own const parameters in that order). Any other form is refused.
fn legacy_positions(args: &str, r: Range, out: &mut Vec<u32, Global>) -> Result<(), Why> {
    let inner = args.trim().strip_prefix('(').and_then(|a| a.strip_suffix(')')).ok_or(Why::Unsupported("a `rustc_legacy_const_generics` attribute without a parenthesized list", r))?;
    for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Why::Unsupported("a `rustc_legacy_const_generics` position that is not an unsuffixed decimal integer", r));
        }
        let k = part.parse::<u32>().map_err(|_| Why::Unsupported("a `rustc_legacy_const_generics` position past `u32::MAX`", r))?;
        out.push(k);
    }
    Ok(())
}

/// The `const` qualifier and the cfg-expanded attributes used when typing and binding calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FnQualifiers(u8);

impl FnQualifiers {
    // Bit zero is free in the function subset of TypeAttrs.
    pub const CONST: Self = Self(1);
    const ATTRS: u32 = tc::TypeAttrs::PROMOTABLE | tc::TypeAttrs::INTRINSIC | tc::TypeAttrs::TARGET_FEATURE;
    const INTERNAL_SYMBOL: u8 = 1 << 1;
    const RENAMED_INTERNAL: u8 = 1 << 3;
    const WRITES_LIFETIMES: u8 = 1 << 6;

    fn of_flags(flags: u32) -> Self {
        Self(u8::from(flags & sig::CONST != 0))
    }

    /// Whether the declaration writes a lifetime (including `'_`). This is the borrow
    /// checker's callee flag. The original spelling matters even when lifetimes are erased.
    pub(super) fn note_lifetimes(mut self, text: &str) -> Self {
        let text = super::head_text::after_outer_trivia(text);
        let head = text.split('{').next().unwrap_or(text).as_bytes();
        if (0..head.len()).any(|i| head[i] == b'\'' && head.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') && head.get(i + 2) != Some(&b'\'')) {
            self.0 |= Self::WRITES_LIFETIMES;
        }
        self
    }

    pub fn writes_lifetimes(self) -> bool {
        self.0 & Self::WRITES_LIFETIMES != 0
    }

    fn attribute(&mut self, path: &str) {
        use super::fn_attrs::{INTRINSIC, PROMOTABLE, TARGET_FEATURE};
        self.0 |= match path {
            PROMOTABLE => tc::TypeAttrs::PROMOTABLE as u8,
            INTRINSIC => tc::TypeAttrs::INTRINSIC as u8,
            TARGET_FEATURE => tc::TypeAttrs::TARGET_FEATURE as u8,
            super::internal_symbol::ATTR => Self::INTERNAL_SYMBOL,
            _ => 0,
        };
    }

    fn of(v: &View<'_>) -> Result<Self, Why> {
        let mut qualifiers = Self::of_flags(v.head[head::FLAGS]).note_lifetimes(v.item());
        for &row in v.rows(v.list(head::ATTRS))? {
            qualifiers.attribute(v.text_at(v.attr(row)?.path)?.trim());
        }
        Ok(qualifiers)
    }

    pub(super) fn from_byte(byte: u8) -> Option<Self> {
        let allowed = Self::CONST.0 | Self::ATTRS as u8 | Self::INTERNAL_SYMBOL | Self::RENAMED_INTERNAL | Self::WRITES_LIFETIMES;
        let valid_rename = byte & Self::RENAMED_INTERNAL == 0 || byte & Self::INTERNAL_SYMBOL != 0;
        (byte & !allowed == 0 && valid_rename).then_some(Self(byte))
    }

    pub(super) fn byte(self) -> u8 {
        self.0
    }

    pub fn typing(self) -> tc::TypeAttrs {
        tc::TypeAttrs(u32::from(self.0) & Self::ATTRS)
    }

    pub fn internal_symbol(self) -> bool {
        self.0 & Self::INTERNAL_SYMBOL != 0
    }

    pub fn renamed_internal(self) -> bool {
        self.0 & Self::RENAMED_INTERNAL != 0
    }

    fn is_const(self) -> bool {
        self.0 & Self::CONST.0 != 0
    }
}

/// A fn's signature: generics `0..parent` are its trait's or impl's, `parent..parent + own` its
/// own; `text` is the item's text its body is lowered from.
pub struct FnHead<'n> {
    pub lib: LibKey,
    pub text: &'n str,
    /// Bytes before the body's opening brace, or `None` when the head checks the whole item.
    pub text_prefix: Option<u32>,
    pub container: Option<ItemKey>,
    pub parent: u32,
    pub own: u32,
    /// The first `declared` of the `own` parameters are written in `<..>`; the rest are the
    /// anonymous parameters of its `impl Trait` arguments (`own - anon.len()`).
    pub declared: u32,
    pub self_param: bool,
    /// Its ABI and safety: the header of the fn pointer it reifies to.
    pub header: tc::FnPtrHeader,
    /// A C-variadic foreign fn (`...` last): `inputs` are the declared parameters only.
    pub c_variadic: bool,
    pub qualifiers: FnQualifiers,
    /// `#[rustc_legacy_const_generics(i, ..)]`'s positions, as written: the call argument at
    /// each is the next own const generic argument (blaze-typeck rules/legacy_const.rs). Empty
    /// for every other fn.
    pub legacy_const_generics: Vec<u32, Global>,
    pub inputs: Vec<PTy, Global>,
    pub output: PTy,
    pub has_body: bool,
    /// The names its body's types are read with, as its signature was: the module, every
    /// generic parameter in scope (the container's first, then its own), `Self` and the
    /// container's associated types. One home: a body is lowered in the env its head was.
    pub module: LibKey,
    pub params: Vec<(Name, u32), Global>,
    /// Which of those are const parameters, with their types ([`Env::consts`]).
    pub consts: Vec<(u32, PTy), Global>,
    /// The trait bounds on those parameters (the container's, then its own).
    pub bounds: Vec<ParamBound, Global>,
    pub self_: SelfTy,
    pub self_assoc: Vec<(Name, PTy), Global>,
}

/// The environment in which a free item body is lowered. A local item's body belongs to
/// its enclosing module, even when a block module supplied its signature's names.
pub(super) struct FreeBodyHead<'s, 'n> {
    pub head: &'s FnHead<'n>,
    pub module: LibKey,
    pub marks: Option<TextMarks<'n>>,
}

impl FnHead<'_> {
    /// The constant evaluator asks this after resolving the callee. The intrinsic table and
    /// lang-item index supply its identity. Its qualifiers and body flag come from the head.
    pub(super) fn const_call<'n>(&self, names: impl NamesSource<'n>, index: &super::index::Index<'n>) -> blaze_consteval::FnKind {
        use blaze_consteval::{Fact, FnKind, Intrinsic};
        if index.lang_name_of(self.lib).is_some_and(super::consts::is_panic_lang) {
            return FnKind::Panic;
        }
        if self.qualifiers.typing().0 & tc::TypeAttrs::INTRINSIC != 0 {
            let Some(name) = super::consts::written_name(names, self.lib) else {
                return FnKind::Missing(Fact::IntrinsicName);
            };
            match Intrinsic::named(name) {
                Ok(intrinsic) => return FnKind::Intrinsic(intrinsic),
                Err(_) if !self.has_body => return FnKind::UnknownIntrinsic,
                // An intrinsic with a Rust definition uses that definition when the
                // evaluator has no built-in implementation of the named intrinsic.
                Err(_) => {}
            }
        }
        if self.qualifiers.is_const() { FnKind::Const } else { FnKind::NotConst }
    }
}

/// A tuple struct's or tuple variant's constructor fn (the Reference, "Struct expressions",
/// "Enumerated types"): its name in the value namespace is a fn from its fields, in order, to
/// the type, generic over the struct's or enum's parameters (`Ok::<T, E>: fn(T) -> Result<T,
/// E>`); its where-clauses are the type's. A unit struct or variant is a constant of the type
/// and a braced one has no value-namespace name: neither has a row.
pub struct CtorSig {
    pub adt: ItemKey,
    pub generics: u32,
    pub inputs: Vec<PTy, Global>,
    pub output: PTy,
}

/// An impl member fn's visibility as written (the Reference, "Visibility and privacy"), with
/// the module it is relative to left to the reader: the impl's module (its members' frame,
/// `MemberFrame::module`). Kept as written, so the head is a function of the impl's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberVis {
    /// `pub`: anyone.
    Public,
    /// `pub(crate)`: the modules of the impl's crate.
    Crate,
    /// No visibility written, or `pub(self)`: the impl's module and the modules under it.
    SelfMod,
    /// `pub(super)`: the impl's module's parent and the modules under it.
    Super,
    /// `pub(in crate::a::b)`: the module `depth` normal modules below the crate's root on the
    /// impl's module's own chain (the Reference, "Visibility and privacy": "`pub(in path)`
    /// makes an item visible within the provided path. `path` must be an ancestor module of
    /// the item whose visibility is being declared"; rustc E0742 otherwise). Being an
    /// ancestor, the module is told by its depth alone, no name resolved (tempo
    /// `pub(in crate::follow::driver) fn identity`, depth 2).
    InCrate { depth: u8 },
    /// `pub(in super::super)`: the impl's module's `up`th normal ancestor (`pub(in self)` is
    /// [`MemberVis::SelfMod`], `pub(in super)` [`MemberVis::Super`]).
    Up { up: u8 },
    /// Any other `pub(in path)` (edition 2015's crate-relative `pub(in a::b)`, a path deeper
    /// than [`MOST_VIS_DEPTH`]): not told here. A body outside the impl's crate cannot see it;
    /// one inside is the world's gap naming the fn, never a guess.
    InPath,
}

/// The deepest `pub(in ..)` module a head record tells by depth: the bytes after
/// [`MemberVis::byte`]'s five plain forms (rule 18).
pub const MOST_VIS_DEPTH: u8 = 100;

impl MemberVis {
    /// The written form of a member's visibility, its `pub(in path)` read in `v`, the
    /// member's own lowering: `crate` and then names is [`MemberVis::InCrate`], `self` or
    /// `super`s alone [`MemberVis::Up`]. Read by the path's segment kinds only: the path must
    /// name an ancestor (above), so its names are not resolved (a wrong one is rustc's
    /// E0742, not this reader's to find).
    pub fn of_in(v: &View<'_>, vis: lf::Vis) -> Result<MemberVis, Why> {
        let p = match vis {
            lf::Vis::Pub => return Ok(MemberVis::Public),
            lf::Vis::Crate => return Ok(MemberVis::Crate),
            lf::Vis::Private | lf::Vis::SelfMod => return Ok(MemberVis::SelfMod),
            lf::Vis::Super => return Ok(MemberVis::Super),
            lf::Vis::In(p) => p,
        };
        let path = v.path(p)?;
        if path.absolute {
            return Ok(MemberVis::InPath);
        }
        let segments = v.segments(path)?;
        let Some((&first, rest)) = segments.split_first() else { return Ok(MemberVis::InPath) };
        let kind_of = |n| lf::Segment::decode(n).map(|s| s.kind);
        match kind_of(first)? {
            lf::SegKind::Crate => {
                for &n in rest {
                    if kind_of(n)? != lf::SegKind::Ident {
                        return Ok(MemberVis::InPath);
                    }
                }
                Ok(match u8::try_from(rest.len()) {
                    Ok(0) => MemberVis::Crate,
                    Ok(depth) if depth <= MOST_VIS_DEPTH => MemberVis::InCrate { depth },
                    _ => MemberVis::InPath,
                })
            }
            lf::SegKind::SelfMod if rest.is_empty() => Ok(MemberVis::SelfMod),
            lf::SegKind::Super => {
                for &n in rest {
                    if kind_of(n)? != lf::SegKind::Super {
                        return Ok(MemberVis::InPath);
                    }
                }
                Ok(match u8::try_from(segments.len()) {
                    Ok(1) => MemberVis::Super,
                    Ok(up) if up <= MOST_VIS_DEPTH => MemberVis::Up { up },
                    _ => MemberVis::InPath,
                })
            }
            _ => Ok(MemberVis::InPath),
        }
    }

    /// Its byte in a head record (head_record.rs, form 20): the five plain forms `0..=4`, then
    /// [`MemberVis::InCrate`] at `5 + depth`, then [`MemberVis::Up`] at `6 + MOST_VIS_DEPTH +
    /// up` (both at most [`MOST_VIS_DEPTH`], so the byte is at most 207).
    pub fn byte(self) -> u8 {
        match self {
            MemberVis::Public => 0,
            MemberVis::Crate => 1,
            MemberVis::SelfMod => 2,
            MemberVis::Super => 3,
            MemberVis::InPath => 4,
            MemberVis::InCrate { depth } => 5 + depth.min(MOST_VIS_DEPTH),
            MemberVis::Up { up } => 6 + MOST_VIS_DEPTH + up.min(MOST_VIS_DEPTH),
        }
    }

    /// What [`MemberVis::byte`] wrote; `None` for a byte that is no visibility.
    pub fn of_byte(byte: u8) -> Option<MemberVis> {
        const IN_CRATE_LAST: u8 = 5 + MOST_VIS_DEPTH;
        const UP_FIRST: u8 = 6 + MOST_VIS_DEPTH;
        const UP_LAST: u8 = 6 + 2 * MOST_VIS_DEPTH;
        Some(match byte {
            0 => MemberVis::Public,
            1 => MemberVis::Crate,
            2 => MemberVis::SelfMod,
            3 => MemberVis::Super,
            4 => MemberVis::InPath,
            5..=IN_CRATE_LAST => MemberVis::InCrate { depth: byte - 5 },
            UP_FIRST..=UP_LAST => MemberVis::Up { up: byte - UP_FIRST },
            _ => return None,
        })
    }
}

/// An impl's header: its self type, its trait and arguments, its associated types and fns.
pub struct ImplHead {
    pub generics: u32,
    pub self_ty: PTy,
    pub trait_: Option<ItemKey>,
    pub trait_args: tc::ArgsAfterSelf,
    pub assoc: Vec<(Name, PTy), Global>,
    /// Its fns by name and key. No signature is made with the head: each is its own
    /// [`Want::MemberFn`], made when the fn is used (work at item grain).
    pub fns: Vec<(Name, ItemKey), Global>,
    /// For each of `fns`, in its order: whether its first parameter is `self`, read from its
    /// text ([`member_fn::takes_self`]): what answering a name needs, without a signature.
    pub fn_self: Vec<bool, Global>,
    /// For each of `fns`, in its order: its visibility as written ([`MemberVis`], read from
    /// its text like `fn_self`): what method lookup asks before it takes an inherent fn
    /// (`World::member_visible`). A trait impl's members are `Public` (they are as visible as
    /// the trait, which lookup reaches only when it is in scope).
    pub fn_vis: Vec<MemberVis, Global>,
    /// An inherent impl's fns and constants that are unstable ([`Unstable`]: their own
    /// `#[unstable]`, or the impl's when they write no stability attribute), by name in
    /// member order; empty for a trait impl (lookup reaches a trait's member through the
    /// trait). What method and path lookup ask before taking an inherent candidate
    /// (`World::member_unstable`).
    pub unstable: Vec<(Name, Unstable), Global>,
    /// Its associated constants (the Reference, "Associated constants"), each with a row in
    /// `Store::fns` as the checker reads one: no inputs, its type the output, and its body
    /// where its value is written (`has_body`: what the constant evaluator lowers).
    pub consts: Vec<(Name, ItemKey), Global>,
    /// `impl !Trait for T` is negative (the Reference does not list negative impls: rustc's
    /// `negative_impls`, which core and alloc use); `#[rustc_reservation_impl]` reserves.
    pub polarity: tc::ImplPolarity,
}

/// What [`Store::ensure`] is asked to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// An ADT's generic count or a trait's head: what [`Builder`] reads.
    Head(LibKey),
    Adt(LibKey),
    Impl(LibKey),
    Fn(LibKey),
    /// A trait's predicates (supertraits, where clauses) and its fns' signatures: apart from
    /// its head, which they may need (`where Self::Item: Clone`).
    Members(LibKey),
    /// The signature of fn `.1` of impl `.0`: its own want, made after the impl's head and
    /// apart from it ([`member_fn`]), so naming a member never makes a signature.
    MemberFn(LibKey, ItemKey),
}

impl Want {
    /// The library key it is made from: an impl's fn's is its impl's.
    pub fn key(self) -> LibKey {
        match self {
            Want::Head(k) | Want::Adt(k) | Want::Impl(k) | Want::Fn(k) | Want::Members(k) | Want::MemberFn(k, _) => k,
        }
    }
}

/// Why a made head has no regions filed (`Store::region_gaps`, rule 19).
#[derive(Debug)]
pub enum RegionGap {
    /// A region position the producer does not lay, named (types.rs [`RegionRefusal`]), and
    /// the part of the signature it was met in.
    Laid(RegionRefusal, SigPart),
    /// The laid regions the record's form refused ([`super::head_record::FiledRegions::new`]).
    Record(super::head_record::HeadRecordError),
    /// The clauses', fields', header's or captures' columns refused (heads_regions.rs, piece
    /// L1b), by the language's error or a construct by name.
    Clauses(RegionsRefusal),
}

impl RegionGap {
    /// The making store's detail of the gap (rule 19), for a note: what was compared and where.
    /// A path written with another lifetime count (E0107) names the path, the count it declares
    /// and the count written; any other gap its refusal as made. `text` is the item's text its
    /// ranges are relative to (rule 71: the positions are printed only here). The cross-store
    /// answer stays the kind ([`super::head_record::Unlaid`]): this detail lives in the
    /// making store's [`Store::region_gaps`] only.
    pub fn say(&self, text: &str, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        let at_text = |at: Range| text.get(at.start as usize..at.end as usize);
        let compared = |out: &mut dyn core::fmt::Write, at: Range, want: u32, got: u32| match at_text(at) {
            Some(path) => write!(out, "`{path}` declares {want} lifetime parameter(s) and is written with {got} (E0107)"),
            None => write!(out, "the path at {}..{} of the item's text declares {want} lifetime parameter(s) and is written with {got} (E0107)", at.start, at.end),
        };
        match self {
            RegionGap::Laid(RegionRefusal::LifetimeArgs { at, want, got }, _) => compared(out, *at, *want, *got),
            RegionGap::Clauses(RegionsRefusal::LifetimeArgs { at, want, got }) => compared(out, *at, *want, *got),
            // The cause in Rust's words, in which part, and the text it is at (rule 19).
            RegionGap::Laid(RegionRefusal::NotLaid { why, at }, part) => match at_text(*at).filter(|t| !t.is_empty()) {
                Some(t) => write!(out, "{}, in {part}: `{t}`", why.text()),
                None => write!(out, "{}, in {part}", why.text()),
            },
            RegionGap::Laid(r, part) => write!(out, "{r}, in {part}"),
            other => write!(out, "{other:?}"),
        }
    }

    /// The gap of `why`, a refusal of the signature laid in `sink`: placed in the part the
    /// sink met its first refusal in when `why` is that one; an elided output lifetime the
    /// signature's own rule refused is the output's; any other check across the parts is the
    /// signature's as a whole.
    fn placed(sink: &RegionSink<'_>, why: RegionRefusal) -> RegionGap {
        let part = match (sink.refused(), why) {
            (Some(first), _) if first == why => sink.refused_part(),
            (_, RegionRefusal::NotLaid { why: NotLaidWhy::ElidedOutput | NotLaidWhy::OpaqueElided, .. }) => SigPart::Output,
            _ => SigPart::Whole,
        };
        RegionGap::Laid(why, part)
    }
}

/// A trait's or impl's lifetimes as its members' fns number them: the container group of
/// `regions::Numbering`, `Param(0..count)`. A trait's are its declared lifetimes; an impl's
/// are its declared ones, then one per lifetime its header elides (`impl Tr for &u8`, `impl
/// S<'_>`), in the self type's slot order. `self_regions` is the self type's column's slots
/// in those params (an impl's; a trait's `Self` is a type parameter and has none).
#[derive(Debug)]
pub struct ContainerRegions {
    pub names: Vec<Name, Alloc<'static>>,
    pub count: u32,
    pub self_regions: Option<Vec<Region, Alloc<'static>>>,
    /// The binder nodes of the self type (what a `self` input shifts the inputs after it by).
    pub self_binders: u32,
    /// A trait impl's trait reference's column in the same params (its trait's lifetimes as
    /// the header writes them first: what a member's `Self::Name` reads, types.rs
    /// `projection_slots`); `None` for a trait, an inherent impl, or before the header's
    /// regions are laid.
    pub trait_regions: Option<Vec<Region, Alloc<'static>>>,
    /// Non-higher-ranked trait bounds on the container's own type parameters (`Param(p): Trait<..>`),
    /// extracted from the container's filed clause columns (numbered in `Param(0..count)`).
    pub param_bounds: Vec<ContainerParamBound, Alloc<'static>>,
}

/// Extract non-higher-ranked trait bounds on bare type parameters (`PNode::Param(p)`) from a
/// container's laid clause columns (`HeadRegions::clauses`).
fn extract_container_param_bounds<A: core::alloc::Allocator>(
    tys: tc::PTys<'_>,
    clauses: &[tc::region_facts::ClauseRegions<A>],
    alloc: Alloc<'static>,
) -> Vec<ContainerParamBound, Alloc<'static>> {
    let mut out = Vec::new_in(alloc);
    for c in clauses {
        if c.binders != 0 {
            continue;
        }
        let Ok(PNode::ExTrait(trait_, args)) = tys.node(c.col.ty()) else { continue };
        let Ok(list) = tys.list(args) else { continue };
        let Some(&first) = list.first() else { continue };
        let Ok(PNode::Param(param)) = tys.node(first) else { continue };
        let mut regions = Vec::with_capacity_in(c.col.as_slice().len(), alloc);
        regions.extend_from_slice(c.col.as_slice());
        out.push(ContainerParamBound { param, trait_, regions });
    }
    out
}

impl ContainerRegions {
    /// The declared lifetimes of the header `v` (its GENERICS rows), in declaration order.
    pub fn of_header(v: &View<'_>, self_regions: Option<Vec<Region, Alloc<'static>>>, alloc: Alloc<'static>) -> Result<ContainerRegions, Why> {
        let mut names = Vec::new_in(alloc);
        lifetime_names(v, &mut names)?;
        let count = names.len() as u32;
        Ok(ContainerRegions { names, count, self_regions, self_binders: 0, trait_regions: None, param_bounds: Vec::new_in(alloc) })
    }

    /// What a member walk in `env` is given ([`SelfGiven`]): in an impl's member, `Self`'s
    /// regions, the trait column, and the container's parameter bounds; in a trait's member,
    /// the container's parameter bounds; elsewhere nothing.
    pub fn given<'c>(this: Option<&'c ContainerRegions>, env: &Env<'_>) -> SelfGiven<'c> {
        match (env.self_, this) {
            (SelfTy::Ty(..), Some(c)) => SelfGiven {
                self_: c.self_regions.as_deref(),
                impl_trait: c.trait_regions.as_deref(),
                param_bounds: &c.param_bounds,
            },
            (_, Some(c)) => SelfGiven { self_: None, impl_trait: None, param_bounds: &c.param_bounds },
            _ => SelfGiven::NONE,
        }
    }

    /// The container param a lifetime written by name is, if the container declares it.
    pub fn named(&self, n: Name) -> Option<Region> {
        self.names.iter().position(|&x| x == n).map(|i| Region::Param(i as u32))
    }
}

/// An impl's container group from its self type's slots (the one fragment `sink` holds): its
/// declared lifetimes, then one fresh parameter per elided lifetime of the self type in slot
/// order (the Reference, "Lifetime elision", "Impl headers"); a trait object's default at
/// the self type's root is `'static`. A fn pointer's or trait object's bound variable in the
/// self type is not laid yet (refused by name).
fn impl_container(v: &View<'_>, sink: &RegionSink<'_>, alloc: Alloc<'static>) -> Result<Result<ContainerRegions, RegionRefusal>, Why> {
    if let Some(why) = sink.refused() {
        return Ok(Err(why));
    }
    let mut c = ContainerRegions::of_header(v, None, alloc)?;
    let Some(&f) = sink.frags().last() else {
        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::Stack, Range::new(0, 0))));
    };
    let mut elided: Vec<u32, Alloc<'static>> = Vec::new_in(alloc);
    let mut regions = Vec::with_capacity_in(f.len as usize, alloc);
    let declared = c.names.len() as u32;
    for s in sink.slots_of(f) {
        let r = match s.kind {
            SlotKind::Static | SlotKind::ObjectDefault(_) => Region::Static,
            SlotKind::Named(n, at) => match c.named(n) {
                Some(r) => r,
                None => return Ok(Err(RegionRefusal::Undeclared { at })),
            },
            SlotKind::Elided(id) => {
                let j = match elided.iter().position(|&x| x == id) {
                    Some(j) => j as u32,
                    None => {
                        elided.push(id);
                        elided.len() as u32 - 1
                    }
                };
                Region::Param(declared + j)
            }
            SlotKind::Given(r) => r,
            SlotKind::Late { .. } | SlotKind::Bound(_) => {
                return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::BoundVariable, Range::new(0, 0))));
            }
        };
        regions.push(r);
    }
    c.count = declared + elided.len() as u32;
    c.self_regions = Some(regions);
    c.self_binders = f.binders;
    Ok(Ok(c))
}

/// D148's join: the written types `types` (of the view `v`) walked again by `b` in `env`, each
/// with a sink of its own given `Self`'s regions and the impl's trait column (`given`) and the
/// item's opaques' captures, for the clause, field and header producers (heads_regions.rs
/// [`Laid`]). A need is passed up; any other error of the walk (which the head's own walk of
/// the same type did not meet) is that type's refusal, read by name only if a producer reads
/// the type.
#[expect(clippy::too_many_arguments, reason = "the walk's builder, env and view, and what each sink is given")]
fn lay_written<'n, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    env: &Env<'_>,
    v: &View<'_>,
    types: &[lf::TypeId],
    given: SelfGiven<'_>,
    opaques: Captures<'_>,
    alloc: Alloc<'static>,
) -> Result<Laid, Why> {
    let mut laid = Laid::new_in(alloc);
    for &t in types {
        if laid.has(t) {
            continue;
        }
        let mut sink = RegionSink::new_in(alloc);
        sink.set_given(given);
        sink.set_opaques(opaques);
        match b.ty_regions(tys, env, v, t, &mut sink) {
            // A type whose slots pass the table's u32 count is that type's refusal, not the
            // head's (its walk refused nothing).
            Ok(made) => {
                if laid.add(t, made, &sink).is_err() {
                    laid.refuse(t, RegionRefusal::not_laid(NotLaidWhy::TooMany, Range::new(0, 0)));
                }
            }
            Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
            Err(_) => laid.refuse(t, RegionRefusal::not_laid(NotLaidWhy::Resolve, Range::new(0, 0))),
        }
    }
    Ok(laid)
}

/// A type alias's type `t` (of its view `v`) laid over the alias's own lifetimes (D148,
/// [`Heads::alias_slots`]): each slot in `each_slot_node`'s order, a name the alias declares
/// its index among them. `None` (the regions of a use refused by name, never guessed) where
/// the walk refused them, or a slot is one no alias's type may hold: an elided lifetime
/// ("Lifetime elision" gives a type alias's type no rule; rustc's E0106), a trait object's
/// default left to its context, or a name the alias does not declare (E0261). A need is
/// passed up; any other error of this second walk is the alias's regions', never its head's
/// (the head's own walk of the same type met none).
fn alias_slots<'n, H: Heads, S: NamesSource<'n>>(b: &Builder<'_, H, S>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, alloc: Alloc<'static>) -> Result<Option<AliasRegions>, Why> {
    let mut sink = RegionSink::new_in(alloc);
    match b.ty_regions(tys, env, v, t, &mut sink) {
        Ok(_) => {}
        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
        Err(_) => return Ok(None),
    }
    let ([f], None) = (sink.frags(), sink.refused()) else { return Ok(None) };
    let mut names = blaze_own::Vec::new_in(alloc);
    for &n in v.rows(v.list(head::GENERICS))? {
        if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(n)? {
            names.push(Name(v.name(name)?));
        }
    }
    let Ok(lifetimes) = u32::try_from(names.len()) else { return Ok(None) };
    let laid = sink.slots_of(*f);
    let mut slots = blaze_own::Vec::with_capacity_in(laid.len(), alloc);
    for s in laid {
        slots.push(match s.kind {
            SlotKind::Static => AliasSlot::Static,
            SlotKind::Late { binder, var } => AliasSlot::Late { binder, var },
            SlotKind::ObjectDefault(_) => AliasSlot::ObjectDefault,
            SlotKind::Named(n, _) => match names.iter().position(|&m| m == n).map(u32::try_from) {
                Some(Ok(i)) => AliasSlot::Lifetime(i),
                _ => return Ok(None),
            },
            SlotKind::Elided(_) | SlotKind::Bound(_) | SlotKind::Given(_) => return Ok(None),
        });
    }
    Ok(Some(AliasRegions { lifetimes, slots }))
}

/// An opaque type's regions (D148): `me` its type in the fn's head, `count` the fn's lifetimes
/// (its clause columns are in the fn's numbering), `captured` its captured lifetimes as the fn's
/// signature column lays them ([`Captures`]: `Param(0..count)` under the every-lifetime rule,
/// the listed ones under `use<..>` or a pre-2024 RPIT's named rule), `rule` and `elided` the
/// rule and the output rule's `'_` they were taken by. heads_regions.rs `captures` must say the
/// same where they meet; its own clauses are its bounds with it as `Self` (`op`), read in the
/// view `v` they are written in.
#[expect(clippy::too_many_arguments, reason = "the opaque's parts and the walk that reads its bounds")]
fn opaque_filed<'n, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    heads: &H,
    env: &Env<'_>,
    v: &View<'_>,
    (op, written_bounds, me): (&Preds, Option<lf::TypeId>, PTy),
    (count, captured, captures, rule, elided): (u32, &[Region], Captures<'_>, hreg::Capture, Option<Region>),
    (self_regions, scope): (SelfGiven<'_>, &Scope),
    alloc: Alloc<'static>,
) -> Result<Result<super::head_record::FiledRegions, RegionGap>, Why> {
    let laid = lay_listed(b, tys, env, v, (op, &[]), self_regions, captures, alloc)?;
    let arity = HeadsArity { heads, opaques: captures.counts, names: b.resolver.names };
    let mut cols: blaze_own::Vec<Region, Alloc<'static>> = blaze_own::Vec::with_capacity_in(captured.len(), alloc);
    cols.extend_from_slice(captured);
    let made = [(me, &cols[..])];
    let cx = hreg::Cx { v, facts: &arity, written: &laid, scope, made: &made, alloc };
    // Where the two producers meet: the captured lifetimes by the opaque's rule and its
    // `use<..>`, read again from its text.
    let mut by_rule: blaze_own::Vec<Region, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    if let Err(why) = hreg::captures(v, scope, alloc, written_bounds, rule, count, elided, &mut by_rule) {
        return Ok(Err(RegionGap::Clauses(why)));
    }
    if by_rule.as_slice() != cols.as_slice() {
        return Ok(Err(RegionGap::Clauses(RegionsRefusal::Disagree { what: "an opaque type's captured lifetimes" })));
    }
    let head = match hreg::opaque_regions(&cx, tys, op, count, &cols) {
        Ok(h) => h,
        Err(why) => return Ok(Err(RegionGap::Clauses(why))),
    };
    let view = tys.view();
    Ok(super::head_record::FiledRegions::new(head, &arity, view, alloc).map_err(RegionGap::Record))
}

/// The regions of an `async fn`'s future (opaque 0): its captures `captured` (every lifetime
/// in scope, `Param(0..count)`), and its derived clauses (`Future`, `Output = T`, `Sized`),
/// which have no column. Unlike [`opaque_filed`] there is no written bound or `use<..>` to read
/// the captures from again: the Reference's rule for an `async fn` is all of them.
#[expect(clippy::too_many_arguments, reason = "opaque_filed's, less its rule")]
fn async_future_filed<'n, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    heads: &H,
    env: &Env<'_>,
    v: &View<'_>,
    (op, me): (&Preds, PTy),
    (count, captured, captures): (u32, &[Region], Captures<'_>),
    (self_regions, scope): (SelfGiven<'_>, &Scope),
    alloc: Alloc<'static>,
) -> Result<Result<super::head_record::FiledRegions, RegionGap>, Why> {
    let laid = lay_listed(b, tys, env, v, (op, &[]), self_regions, captures, alloc)?;
    let arity = HeadsArity { heads, opaques: captures.counts, names: b.resolver.names };
    let mut cols: blaze_own::Vec<Region, Alloc<'static>> = blaze_own::Vec::with_capacity_in(captured.len(), alloc);
    cols.extend_from_slice(captured);
    let made = [(me, &cols[..])];
    let cx = hreg::Cx { v, facts: &arity, written: &laid, scope, made: &made, alloc };
    let head = match hreg::opaque_regions(&cx, tys, op, count, &cols) {
        Ok(h) => h,
        Err(why) => return Ok(Err(RegionGap::Clauses(why))),
    };
    let view = tys.view();
    Ok(super::head_record::FiledRegions::new(head, &arity, view, alloc).map_err(RegionGap::Record))
}

/// The written types of `more` and `preds`, for [`lay_written`], and why the listing stopped
/// short when a bound's path could not be read (a regions refusal, never the head's: the
/// head's own walk read the same bound).
fn written_of(v: &View<'_>, preds: &Preds, more: &[lf::TypeId], alloc: Alloc<'static>) -> (blaze_own::Vec<lf::TypeId, Alloc<'static>>, Option<RegionsRefusal>) {
    let mut out = blaze_own::Vec::new_in(alloc);
    for &t in more {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    let unlisted = hreg::written_types(v, preds, &mut out).err();
    (out, unlisted)
}

/// [`written_of`] then [`lay_written`]: each type listed is laid, and a type the listing
/// stopped before is refused with its reason when a producer reads it (the regions refused,
/// the head made).
#[expect(clippy::too_many_arguments, reason = "lay_written's, and what is listed")]
fn lay_listed<'n, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    env: &Env<'_>,
    v: &View<'_>,
    (preds, more): (&Preds, &[lf::TypeId]),
    given: SelfGiven<'_>,
    opaques: Captures<'_>,
    alloc: Alloc<'static>,
) -> Result<Laid, Why> {
    let (types, unlisted) = written_of(v, preds, more, alloc);
    let mut laid = lay_written(b, tys, env, v, &types, given, opaques, alloc)?;
    if let Some(why) = unlisted {
        laid.unlisted(why);
    }
    Ok(laid)
}

/// An associated constant's type and its regions (D148), from one walk of its written type
/// `t`: laid as a fn's with no inputs, over `FnPtr((), T)` ([`fn_regions`] with
/// `FnSig::item_fn` false, so an elided lifetime is `'static`, "Static lifetime elision"),
/// numbered in its container's group (`container`, or the container whose group is not filed),
/// its clauses (all inherited: a constant has no generics of its own) given their container's
/// columns as filed (`container_heads`, heads_regions.rs `fill_clauses`). The head record's Fn
/// form carries a constant's regions as it carries a fn's; without this producer every
/// associated constant's record had none (head_tests.rs
/// `a_frozen_head_carries_the_store_s_side_tables`). A refusal of the regions is the gap
/// returned beside the type, never the head's.
#[expect(clippy::too_many_arguments, reason = "the constant's parts and its container's")]
fn const_regions<'n, H: Heads, S: NamesSource<'n>>(
    b: &Builder<'_, H, S>,
    tys: &mut PortableTys<'_>,
    env: &Env<'_>,
    v: &View<'_>,
    t: lf::TypeId,
    (container, scope, counts): (Result<&ContainerRegions, ItemKey>, &Scope, Option<&dyn tc::regions::Arity>),
    (preds, container_heads): (&Preds, Option<&HeadRegions<Alloc<'static>>>),
    alloc: Alloc<'static>,
) -> Result<(PTy, Result<super::head_record::FiledRegions, RegionGap>), Why> {
    let whole = v.site(v.type_range(t)?)?;
    let mut sink = RegionSink::new_in(alloc);
    let container = match container {
        Ok(c) => Some(c),
        Err(c) => {
            sink.refuse(RegionRefusal::NoContainer { container: c });
            None
        }
    };
    // `Self`'s regions are its self type's and `Self::Name`'s trait lifetimes the impl's trait
    // column, as a member fn's walk is given them (`impl<'de> Tr<'de> for X { const C:
    // Self::Value = ..; }`).
    let given = ContainerRegions::given(container, env);
    sink.set_given(given);
    // A constant's type is its signature's output (`FnPtr(() -> T)` below).
    sink.set_part(SigPart::Output);
    let output = b.ty_regions(tys, env, v, t, &mut sink)?;
    sink.set_part(SigPart::Whole);
    if let Some(c) = container
        && c.count != scope.params()
    {
        sink.refuse(RegionRefusal::not_laid(NotLaidWhy::ContainerDisagree, whole));
    }
    let numbering = Numbering { container: container.map_or(0, |c| c.count), own: 0, elided: 0 };
    let sig = FnSig { numbering, own: &[], elided: &[], container, inputs_laid: 0, receiver: false, item_fn: false, is_async: false, whole };
    let io = intern_list(tys, &[output])?;
    let sig_ty = intern(tys, PNode::FnPtr(io, tc::FnPtrHeader::RUST))?;
    let filed = match fn_regions(v, &sink, &sig, alloc)? {
        Err(why) => Err(RegionGap::placed(&sink, why)),
        Ok((_, _, Some(_))) => Err(RegionGap::Laid(RegionRefusal::not_laid(NotLaidWhy::OutOfStep, whole), SigPart::Output)),
        Ok((params, sig_slots, None)) => {
            let arity = MemberArity { container: counts, heads: HeadsArity { heads: b.heads, opaques: &[], names: b.resolver.names } };
            match RegionCol::lay(tys.view(), sig_ty, &arity, &sig_slots, alloc) {
                Err(e) => Err(RegionGap::Laid(RegionRefusal::Regions(e), SigPart::Output)),
                Ok(sig_col) => {
                    let mut hr = HeadRegions::new_in(params, alloc);
                    hr.sig = Some(sig_col);
                    let laid = lay_listed(b, tys, env, v, (preds, &[]), given, Captures::NONE, alloc)?;
                    let cx = hreg::Cx { v, facts: &arity, written: &laid, scope, made: &[], alloc };
                    match hreg::fill_clauses(&cx, tys, preds, container_heads, &mut hr) {
                        Ok(()) => super::head_record::FiledRegions::new(hr, &arity, tys.view(), alloc).map_err(RegionGap::Record),
                        Err(why) => Err(RegionGap::Clauses(why)),
                    }
                }
            }
        }
    };
    Ok((output, filed))
}

/// The one region counter's facts for a member of a container whose regions are filed (D148):
/// every DECLARED count (an ADT's, a trait's, an associated type's) from the names (D155,
/// [`HeadsArity`]: its crate's items record, the same in every store); an opaque's captures
/// as the container's own record counted them (`FiledRegions::arity`) first, then as the
/// heads filed them. Before D155 the declared counts were asked of the container's record,
/// then of the heads this store had made, and a store that filed an item from its record
/// counted what a lowering store did not (the derive's `fn clone(&self) -> Self` refused
/// `Counts` in a fresh store; `a_seeded_freeze_lowers_each_item_once`,
/// `a_member_counts_a_restored_trait_s_lifetimes`).
struct MemberArity<'h, H: Heads + ?Sized, S> {
    container: Option<&'h dyn tc::regions::Arity>,
    heads: HeadsArity<'h, H, S>,
}

impl<'n, H: Heads + ?Sized, S: NamesSource<'n>> tc::regions::Arity for MemberArity<'_, H, S> {
    fn adt_lifetimes(&self, adt: ItemKey) -> tc::regions::Count {
        tc::regions::Arity::adt_lifetimes(&self.heads, adt)
    }
    fn opaque_captures(&self, opaque: ItemKey) -> tc::regions::Count {
        if let Some(c) = self.container
            && let tc::regions::Count::Known(n) = c.opaque_captures(opaque)
        {
            return tc::regions::Count::Known(n);
        }
        tc::regions::Arity::opaque_captures(&self.heads, opaque)
    }
    fn alias_lifetimes(&self, trait_: ItemKey, name: Name) -> tc::regions::Count {
        tc::regions::Arity::alias_lifetimes(&self.heads, trait_, name)
    }
    fn trait_lifetimes(&self, trait_: ItemKey) -> tc::regions::Count {
        tc::regions::Arity::trait_lifetimes(&self.heads, trait_)
    }
}

/// What a head's regions are filed as: made, or its gap.
fn file_regions(store: &Store<'_>, key: ItemKey, filed: Result<super::head_record::FiledRegions, RegionGap>) {
    match filed {
        Ok(f) => {
            store.region_gaps.remove(&key);
            store.regions.file(key, f);
        }
        // A soft refusal: the head is filed whole, its regions refused by the gap's kind, by
        // name (not `HeadRefused`: the head is not), the same answer its record carries to
        // another store (tables/head.rs form 11).
        Err(gap) => {
            let kind = gap.unlaid();
            store.region_gaps.insert(key, gap);
            store.regions.file_unlaid(key, kind, kind.name());
        }
    }
}

impl RegionGap {
    /// The kind a reader is told ([`super::head_record::Unlaid`]).
    pub fn unlaid(&self) -> super::head_record::Unlaid {
        use super::head_record::Unlaid;
        match self {
            RegionGap::Laid(r, part) => match *r {
                RegionRefusal::NotLaid { why, .. } => Unlaid::NotLaid { why, part: *part },
                RegionRefusal::Regions(_) => Unlaid::Counts,
                RegionRefusal::NoContainer { .. } => Unlaid::NoContainer,
                RegionRefusal::Undeclared { .. } => Unlaid::Undeclared,
                RegionRefusal::LifetimeArgs { .. } => Unlaid::LifetimeArgs,
                RegionRefusal::ObjectDefault { .. } => Unlaid::ObjectDefault,
                RegionRefusal::NoType { .. } => Unlaid::NoType,
            },
            RegionGap::Record(_) => Unlaid::Form,
            RegionGap::Clauses(_) => Unlaid::Columns,
        }
    }
}

/// A `self` input's fragment: `own` (a `&self`'s reference slot), then `Self`'s regions.
fn push_self_slots(sink: &mut RegionSink<'_>, env: &Env<'_>, own: Option<SlotKind>, given: &[Region], binders: u32, alloc: Alloc<'static>) {
    let mut slots: Vec<Slot, Alloc<'static>> = Vec::with_capacity_in(given.len() + 1, alloc);
    if let Some(o) = own {
        slots.push(Slot::written(o));
    }
    if let SelfTy::Ty(..) = env.self_ {
        slots.extend(given.iter().map(|&g| Slot::quiet(SlotKind::Given(g))));
    }
    let binders = if let SelfTy::Ty(..) = env.self_ { binders } else { 0 };
    sink.push(&slots, binders);
}

/// The slot a lifetime written by name gives (`'_`: elided; `'static`; a name).
fn written_lifetime(v: &View<'_>, n: lf::NameIdx, sink: &mut RegionSink<'_>) -> Result<SlotKind, Why> {
    let text = v.name_text(n)?;
    let at = v.site(v.name_range(n)?)?;
    Ok(match text.strip_prefix('\'').unwrap_or(text) {
        "_" => sink.new_elided(at),
        "static" => SlotKind::Static,
        _ => SlotKind::Named(Name(v.name(n)?), at),
    })
}

/// What [`fn_regions`] numbers a fn's laid slots by.
struct FnSig<'s> {
    numbering: Numbering,
    /// The fn's own lifetimes as declared: each name and its text without the `'`.
    own: &'s [(Name, &'s str)],
    /// The elided input lifetimes' ids, in input slot order (the elided group).
    elided: &'s [u32],
    container: Option<&'s ContainerRegions>,
    /// How many fragments on the sink are inputs (the one after them is the return type's).
    inputs_laid: usize,
    /// Input 0 is `&self`, `&mut self` or `self: &Self` (elision's receiver rule).
    receiver: bool,
    /// A fn (a constant's or static's type elides to `'static`).
    item_fn: bool,
    /// An `async fn`: its output is its future (the captures), its written return type a
    /// column of its own.
    is_async: bool,
    whole: Range,
}

impl FnSig<'_> {
    /// A written or elided lifetime of the signature as a region: a name by its container's or
    /// own group, an elided input lifetime by the elided group (`'static` for a constant's or
    /// static's type).
    fn resolve(&self, sink: &RegionSink<'_>, k: SlotKind) -> Result<Region, RegionRefusal> {
        let n = self.numbering;
        Ok(match k {
            SlotKind::Static | SlotKind::ObjectDefault(_) => Region::Static,
            SlotKind::Named(nm, at) => match self.container.and_then(|c| c.named(nm)) {
                Some(r) => r,
                None => match self.own.iter().position(|&(x, _)| x == nm) {
                    Some(i) => n.own(i as u32)?,
                    None => return Err(RegionRefusal::Undeclared { at }),
                },
            },
            SlotKind::Elided(_) if !self.item_fn => Region::Static,
            SlotKind::Elided(id) => match self.elided.iter().position(|&x| x == id) {
                Some(j) => n.elided(j as u32)?,
                None => return Err(RegionRefusal::not_laid(NotLaidWhy::ElidedOutput, sink.elided_at(id).unwrap_or(self.whole))),
            },
            SlotKind::Late { binder, var } => Region::Late { binder, var },
            SlotKind::Given(r) => r,
            SlotKind::Bound(_) => return Err(RegionRefusal::not_laid(NotLaidWhy::BoundVariable, self.whole)),
        })
    }

    /// The output rule (the Reference, "Lifetime elision in functions"): an elided output
    /// lifetime is the receiver's (`&self`, `&mut self`, `self: &Self`) if there is one, else
    /// the one lifetime the inputs `inputs` use; [`Used::None`] or [`Used::Many`] when neither.
    fn output_rule(&self, sink: &RegionSink<'_>, inputs: &[Slot]) -> Used {
        let frags = sink.frags();
        let receiver = match (self.receiver, frags.first()) {
            (true, Some(&f)) => sink.slots_of(f).first().map(|s| s.kind),
            _ => None,
        };
        match receiver {
            Some(k) => Used::One(k),
            None => used_lifetimes(inputs, |_| false),
        }
    }

    /// What `'_` in a return opaque's bound names (`-> impl Iterator<Item = &u8> + '_`): the
    /// output rule's region. With no single candidate the opaque's regions are refused by name
    /// (soft: the head is made, rule 3 holds by the name), never E0106 ([`missing_lifetime`]
    /// is for a plain elided reference or path of the return type alone).
    fn opaque_elided(&self, sink: &RegionSink<'_>, alloc: Alloc<'static>) -> Result<Region, RegionRefusal> {
        if let Some(why) = sink.refused() {
            return Err(why);
        }
        let frags = sink.frags();
        let inputs = frags.get(..self.inputs_laid).ok_or(RegionRefusal::not_laid(NotLaidWhy::Stack, self.whole))?;
        let mut input_slots: Vec<Slot, Alloc<'static>> = Vec::new_in(alloc);
        for f in inputs {
            input_slots.extend_from_slice(sink.slots_of(*f));
        }
        match self.output_rule(sink, &input_slots) {
            Used::One(k) => self.resolve(sink, k),
            Used::None | Used::Many => Err(RegionRefusal::not_laid(NotLaidWhy::OpaqueElided, self.whole)),
        }
    }
}

/// E0106, the one place it is the PROGRAM's error (a hard [`Why::Program`], the head refused):
/// a plain elided reference or path lifetime (`&T`, `&'_ T`, `Foo<'_>`, `Foo` with a hidden
/// lifetime) in a fn's return type, where the output rule ([`FnSig::output_rule`]) finds no
/// candidate (`Used::None`) or several and no receiver (`Used::Many`). Every other `'_` or
/// elided lifetime the regions cannot place (an opaque's or a `dyn`'s bound, a clause, a
/// field the walk refused) is a regions refusal by name, the head made.
fn missing_lifetime(one: &Used, at: Range) -> Why {
    let why = if matches!(one, Used::None) {
        "missing lifetime specifier: this fn's return type elides a lifetime, and it has no input lifetime to take it from (E0106)"
    } else {
        "missing lifetime specifier: this fn's return type elides a lifetime, and its inputs have more than one lifetime and no `&self` (E0106)"
    };
    Why::Program(why.into(), at)
}

/// A fn's lifetime parameters and its signature's slots as regions: over `FnPtr(inputs..,
/// output)` (the root fn pointer is binder 0, so every input's bound variable is shifted past
/// it), and an `async fn`'s written return type's on its own. Elision (the Reference,
/// "Lifetime elision in functions"): each elided input lifetime is a parameter of its own; an
/// elided output lifetime is the receiver's (`&self`) if there is one, else the one lifetime
/// the inputs use, else E0106 (a program error, the head's). A constant's or static's elided
/// lifetime is `'static` ("Static lifetime elision"). Late-bound ("Generic parameters",
/// rustc's rule): an own lifetime its inputs use that no bound or where clause names, and every
/// elided input lifetime.
fn fn_regions(v: &View<'_>, sink: &RegionSink<'_>, sig: &FnSig<'_>, alloc: Alloc<'static>) -> Result<Result<(LifetimeParams<Alloc<'static>>, Vec<Region, Alloc<'static>>, Option<Vec<Region, Alloc<'static>>>), RegionRefusal>, Why> {
    macro_rules! soft {
        ($e:expr) => {
            match $e {
                Ok(x) => x,
                Err(why) => return Ok(Err(RegionRefusal::from(why))),
            }
        };
    }
    if let Some(why) = sink.refused() {
        return Ok(Err(why));
    }
    let frags = sink.frags();
    if frags.len() != sig.inputs_laid + 1 {
        return Ok(Err(RegionRefusal::not_laid(NotLaidWhy::Stack, sig.whole)));
    }
    let n = sig.numbering;
    let count = soft!(n.count());
    // A written or elided lifetime as a region (an output's elided ones resolved first).
    let resolve = |k: SlotKind| sig.resolve(sink, k);
    // The output's elided lifetimes: the receiver's, else the inputs' one lifetime, else E0106.
    let mut input_slots: Vec<Slot, Alloc<'static>> = Vec::new_in(alloc);
    for f in &frags[..sig.inputs_laid] {
        input_slots.extend_from_slice(sink.slots_of(*f));
    }
    let out_frag = frags[sig.inputs_laid];
    let mut out_slots: Vec<Slot, Alloc<'static>> = Vec::new_in(alloc);
    out_slots.extend_from_slice(sink.slots_of(out_frag));
    if sig.item_fn {
        let one = sig.output_rule(sink, &input_slots);
        for s in out_slots.iter_mut() {
            if let SlotKind::Elided(id) = s.kind {
                match one {
                    Used::One(k) => s.kind = k,
                    Used::None | Used::Many => return Err(missing_lifetime(&one, sink.elided_at(id).unwrap_or(sig.whole))),
                }
            }
        }
    }
    // The signature column: inputs shifted past the root, then the output (an `async fn`'s
    // future: its captures).
    let mut sig_slots = Vec::with_capacity_in(input_slots.len() + out_slots.len(), alloc);
    let mut shift = 1u32;
    for f in &frags[..sig.inputs_laid] {
        for s in sink.slots_of(*f) {
            let k = match s.kind {
                SlotKind::Late { binder, var } => SlotKind::Late { binder: binder + shift, var },
                k => k,
            };
            sig_slots.push(soft!(resolve(k)));
        }
        shift += f.binders;
    }
    let written = if sig.is_async {
        for i in 0..count {
            sig_slots.push(Region::Param(i));
        }
        let mut w = Vec::with_capacity_in(out_slots.len(), alloc);
        for s in out_slots.iter() {
            w.push(soft!(resolve(s.kind)));
        }
        Some(w)
    } else {
        for s in out_slots.iter() {
            let k = match s.kind {
                SlotKind::Late { binder, var } => SlotKind::Late { binder: binder + shift, var },
                k => k,
            };
            sig_slots.push(soft!(resolve(k)));
        }
        None
    };
    // Late-bound: an own lifetime the inputs use and no bound names, and each elided input.
    let mut params = LifetimeParams::new_in(count, alloc);
    let bounded = bounded_lifetimes(v, sig.own, alloc)?;
    for (i, &(nm, _)) in sig.own.iter().enumerate() {
        let used = input_slots.iter().any(|s| matches!(s.kind, SlotKind::Named(x, _) if x == nm));
        if used && !bounded.contains(&(i as u32)) {
            soft!(params.set_late(n.container + i as u32));
        }
    }
    for j in 0..n.elided {
        soft!(params.set_late(n.container + n.own + j));
    }
    Ok(Ok((params, sig_slots, written)))
}

/// The own lifetimes (by index in `own`) that a bound or where clause of `v` names: a
/// lifetime's own bounds (`'a: 'b` names both), a where clause `'a: 'b`, and a type
/// parameter's bound or a type's where clause whose text writes `'a`.
fn bounded_lifetimes(v: &View<'_>, own: &[(Name, &str)], alloc: Alloc<'static>) -> Result<Vec<u32, Alloc<'static>>, Why> {
    let mut out = Vec::new_in(alloc);
    let mark = |nm: Name, out: &mut Vec<u32, Alloc<'static>>| {
        if let Some(i) = own.iter().position(|&(x, _)| x == nm)
            && !out.contains(&(i as u32))
        {
            out.push(i as u32);
        }
    };
    let by_text = |text: &str, out: &mut Vec<u32, Alloc<'static>>| {
        for (i, &(_, t)) in own.iter().enumerate() {
            if mentions_lifetime(text, t) && !out.contains(&(i as u32)) {
                out.push(i as u32);
            }
        }
    };
    let generics = v.list(head::GENERICS);
    for (k, &row) in v.rows(generics)?.iter().enumerate() {
        match lf::GenericParam::decode(row)? {
            lf::GenericParam::Lifetime { name, bounds } => {
                for &b in v.rows(bounds)? {
                    if let lf::Bound::Lifetime(other) = lf::Bound::decode(b)? {
                        mark(Name(v.name(name)?), &mut out);
                        mark(Name(v.name(other)?), &mut out);
                    }
                }
            }
            lf::GenericParam::Type { .. } | lf::GenericParam::Const { .. } => by_text(v.text_at(v.row_range(generics.start + k as u32)?)?, &mut out),
        }
    }
    let wheres = v.list(head::WHERE);
    for (k, &row) in v.rows(wheres)?.iter().enumerate() {
        match lf::WherePred::decode(row)? {
            lf::WherePred::Lifetime { name, bounds } => {
                mark(Name(v.name(name)?), &mut out);
                for &b in v.rows(bounds)? {
                    if let lf::Bound::Lifetime(other) = lf::Bound::decode(b)? {
                        mark(Name(v.name(other)?), &mut out);
                    }
                }
            }
            lf::WherePred::Type { .. } | lf::WherePred::ForFields { .. } => by_text(v.text_at(v.row_range(wheres.start + k as u32)?)?, &mut out),
            lf::WherePred::Const { .. } => {}
        }
    }
    Ok(out)
}

/// Whether `text` writes the lifetime `'name` (a whole token).
fn mentions_lifetime(text: &str, name: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(p) = text[i..].find('\'') {
        let at = i + p + 1;
        let end = at + name.len();
        if text.get(at..end) == Some(name) && b.get(end).is_none_or(|&c| !(c.is_ascii_alphanumeric() || c == b'_')) {
            return true;
        }
        i = at;
    }
    false
}

/// The lifetimes the header `v` declares (its GENERICS rows), in declaration order.
fn lifetime_names(v: &View<'_>, out: &mut Vec<Name, Alloc<'static>>) -> Result<(), Why> {
    for &row in v.rows(v.list(head::GENERICS))? {
        if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
            out.push(Name(v.name(name)?));
        }
    }
    Ok(())
}

/// The lowered heads, by key, and the type table they are in.
///
/// The tables a body's check borrows its answers from (`adts`, `field_defaults`, `impls`,
/// `fns`, `preds`, `trait_defs`, `ctors`) are [`Rows`]: each row filed once at an address that
/// holds until the store drops, filed through `&self`, so a row filed during a check moves no
/// row an earlier answer of that check holds (front2 rows.rs says why that is sound).
/// An impl constant whose row waits on a constant leaf being evaluated ([`Store::impl_`]): its
/// impl, its name, and the leaf's need key (front2/const_inherent.rs `need_key`).
///
/// Why (turso's divan, `impl KnownCounterKind { const COUNT: usize = 4; const ALL: [Self;
/// Self::COUNT] = ..; }`): finding the leaf `KnownCounterKind::COUNT` makes the impl's header,
/// which made every constant's row, and `ALL`'s type needs `COUNT`'s value, which was being
/// evaluated: the impl was refused as a cycle, and every head naming `COUNT` with it. The
/// Reference, "Associated Items": a path to an associated item is the type's path and the
/// item's name, so finding `COUNT` reads the impls' names, never `ALL`'s type; "Constant
/// evaluation" refuses only a constant that needs ITSELF.
#[derive(Clone, Copy, Debug)]
pub struct ConstWait {
    pub impl_: LibKey,
    pub name: Name,
    pub need: LibKey,
}

/// A trait impl whose header (`generics`, `self_ty`, `trait_`, `trait_args`, `preds`, `fns`,
/// `consts`, `polarity`) has been lowered while one or more of its associated types
/// (`type Name = ..;`) is waiting on a constant (`Why::NeedConst`).
///
/// Kept apart from [`Store::impls`] (never frozen or encoded as a head record) and visible
/// only while constant evaluation is running (`Maps::evaluating > 0`), so evaluating the
/// constant can prove `T: Trait` (e.g. `itoa`'s `Integer: private::Sealed` where `impl Sealed
/// for i128` writes `type Buffer = [MaybeUninit<u8>; Self::MAX_STR_LEN]`, or a same-trait
/// `impl Tr for T` with `type Buf = [u8; Self::N]`) without re-entering the pending
/// associated type. Removed as soon as `Store::impl_` finishes or fails the impl, or when the
/// waited-on constant finishes.
pub struct ProvisionalImpl {
    pub head: ImplHead,
    pub pending_assoc: blaze_own::Vec<Name, Alloc<'static>>,
    pub waiting_const: LibKey,
}

pub struct Store<'n> {
    pub tys: PortableTys<'static>,
    pub maps: Maps,
    pub adts: Rows<ItemKey, AdtRow>,
    /// Each ADT's Blaze-edition automatic `Copy`/`Clone` eligibility (`AdtFlags::AUTO_COPY_PUB`),
    /// kept in a side table so `AdtRow` and `head_record::VERSION` stay untouched.
    pub auto_copy_pub: Rows<ItemKey, bool>,
    /// Made by `Want::Head`, including when a field type needs its own ADT's inherent constant.
    pub adt_declarations: Rows<ItemKey, AdtDeclaration>,
    /// Each ADT's field defaults (`struct S { a: u8 = 1 }`, RFC 3681), made with its head in
    /// `adts`: one per field of `AdtRow::fields`, the key of the default's head in `fns` (an
    /// anonymous constant of the field's type over the ADT's generics), or `None`; empty when
    /// no field has one. A head record carries them (head_record.rs, form 8); a row filed
    /// without its entry is the world's gap `field_defaults`, never "no defaults".
    pub field_defaults: Rows<ItemKey, Vec<Option<ItemKey>, Global>>,
    pub impls: Rows<ItemKey, ImplHead>,
    /// Trait impls whose header is lowered while one of their associated types waits on a
    /// constant ([`ProvisionalImpl`]). Visible only while `maps.evaluating > 0`.
    pub provisional_impls: Rows<ItemKey, ProvisionalImpl>,
    /// Provisional impl keys read during the current body check or projection normalization
    /// (`BodyWorld::impl_header_once`, `BodyWorld::assoc_item_once`).
    pub(super) provisional_used: core::cell::RefCell<blaze_own::Vec<ItemKey, Alloc<'static>>>,
    pub fns: Rows<ItemKey, FnHead<'n>>,
    /// Each head's regions (D148), by [`super::head_record::regions_key`]: a fn's made by
    /// [`Store::fn_head`] in the walk that makes its types, or filed from its record.
    pub regions: super::head_record::RegionTable,
    /// Why a head that was made has no regions (rule 19): a region position this producer
    /// does not lay, named; the erased head stands, and a reader of its regions is told.
    pub region_gaps: Rows<ItemKey, RegionGap>,
    /// Each trait's and impl's lifetimes as its members' fns number them (the container
    /// group of `regions::Numbering`), by its checker key: filed by `trait_head` and `impl_`.
    pub container_regions: Rows<ItemKey, ContainerRegions>,
    /// Each trait's and impl's lifetimes by name, as its members' clauses name them
    /// (heads_regions.rs [`Scope`]): its declared ones `Param(0..)`, numbering
    /// `container_regions`' count. Filed for every trait and impl made or read from its record,
    /// even one whose regions are refused, so a member's `where T: 'a` still reads `'a`.
    pub container_scopes: Rows<ItemKey, Scope>,
    /// Each trait's clauses' columns as its members inherit them (heads_regions.rs
    /// `trait_inherited`: `Self: Trait<params>` first, in `Store::trait_members`' `inherited`
    /// order), by the trait's checker key. An impl's members read the impl's own filed regions.
    pub trait_inherits: Rows<ItemKey, HeadRegions<Alloc<'static>>>,
    /// Each ADT's, trait's, impl's and fn's where-clauses, own and inherited (a fn's from its
    /// trait or impl), with its `where const` conditions.
    pub preds: Rows<ItemKey, Preds>,
    /// The `sized` lang trait: each own type parameter's implied bound.
    pub sized: Option<LibKey>,
    /// Cached `LangItem::From` trait key (`None` = not yet looked up, `Some(k)` = looked up).
    pub(super) from_trait: core::cell::Cell<Option<Option<LibKey>>>,
    /// Cached `alloc_shim::bound` targets by `Shim` ordinal (`None` = not yet looked up).
    pub(super) alloc_shims: core::cell::Cell<[Option<Option<ItemKey>>; 6]>,
    /// The library's `Box`, `Rc`, `Arc` and `Pin`, a trait method's typed receiver is read
    /// against for dyn compatibility ([`super::receiver::Pointers::find`]; set by the maker of
    /// the store: [`Store::new`] leaves them not looked up, so every such receiver is a gap).
    pub pointers: super::receiver::Pointers,
    /// Each trait's parts of the checker's `TraitDef` that need its members lowered.
    pub trait_defs: Rows<ItemKey, TraitExtra>,
    /// The member fns whose signature could not be made, by why: their container's head is
    /// made without them, and a body that uses one gets [`super::world_items::Gap::FnHead`]
    /// (a trait's head does not depend on its fns' signatures: `FnMut` and `extern "rust-call"`).
    /// Filed from a frozen record too ([`Store::file_heads`], head_record.rs form 13), through
    /// `&self` as the other rows are.
    pub fn_gaps: Rows<ItemKey, Why>,
    /// Each generic associated type's const parameters as the checker sees them, keyed by the
    /// associated type item. The stored GAT declaration row numbers its own parameters from zero.
    /// This derived row numbers them after `Self` and all trait parameters, as `ConstParams`
    /// requires.
    pub gat_const_params: Rows<ItemKey, Vec<(u32, PTy), Alloc<'static>>>,
    /// Each impl's frame its fns' signatures are made in ([`member_fn::MemberFrame`]), kept
    /// when its head is made, by the impl's checker key.
    member_frames: Rows<ItemKey, member_fn::MemberFrame>,
    /// The impl constants whose row waits on a constant leaf being evaluated now, by the
    /// constant's key ([`ConstWait`], [`Store::impl_`]); each is made again when its leaf is
    /// answered ([`Store::remake_waiting`]).
    const_waits: Rows<ItemKey, ConstWait>,
    /// Each tuple struct's and tuple variant's constructor fn, by the variant's key: made with
    /// its ADT's head.
    pub ctors: Rows<ItemKey, CtorSig>,
    /// Each static whose head is made (its type is its [`FnHead`] of no inputs, as a const's),
    /// with its mutability (`static mut`): the checker's `Def::Static`.
    pub statics: Rows<ItemKey, tc::Mutbl>,
    /// Each opaque type a fn's head made (an `impl Trait` in its return type, an `async fn`'s
    /// `impl Future`), by its key ([`super::keys::opaque_key`]): its fn and its index. Its
    /// bounds are in `preds` under the same key, with the opaque type as `Self`.
    pub opaques: Rows<ItemKey, (ItemKey, u32)>,
    /// Each trait's associated types with a synthetic one per `impl Trait` at a place of a fn's
    /// return type ([`super::rpitit`], Rust's model of `impl Trait` in traits), made on a
    /// world's first ask from heads alone (a trait with none has an empty row once scanned).
    pub rpitit_traits: Rows<ItemKey, super::rpitit::TraitRows>,
    /// The default of each such type of a provided fn, by its key ([`super::rpitit::Twin`]):
    /// an opaque no head made, filed with its trait's rows; its hidden type is its source's.
    pub rpitit_defaults: Rows<ItemKey, super::rpitit::Twin>,
    /// Each impl of such a trait: its associated types with the synthetic ones' values (its
    /// fns' return types), made with the first ask of its header ([`super::rpitit::impl_assoc`]).
    pub rpitit_impls: Rows<ItemKey, blaze_own::Vec<(Name, PTy), Alloc<'static>>>,
    /// The hidden types of the opaques defined by the bodies checked clean in this command,
    /// and the running check's leak list ([`super::opaque_auto`]).
    pub hidden: super::opaque_auto::HiddenTable<'static>,
    /// The items declared inside the bodies lowered so far, kept in the command's frame: a
    /// local item's head is made from its row ([`super::locals`]).
    pub locals: super::locals::Locals<'n>,
    /// `anon::file_asked` files the column beside the synthetic head for `body::lower_member`
    /// to read by that head's key. Both borrow the store's frame. Each pooled body check
    /// creates its own store, so its columns are visible only within that check.
    pub(super) anon_marks: super::anon::Columns<'n>,
    /// Each head's lifetime part, made when first asked for ([`super::lifetimes`]:
    /// `Store::fn_life`, `Store::adt_life`, `Store::container_life`): what the erased types
    /// above leave out and the borrow check reads.
    pub lives: super::lifetimes::Lives,
    /// The run's sources a body's macro calls expand with ([`super::body_host::BodyHost`]:
    /// the crate's environment, its texts, the proc-macro runner), the same item-level
    /// expansion reads. `None` (as [`Store::new`] makes it): every `env!`, include and proc
    /// macro in a body is refused by name. The command sets it once, before any body.
    pub sources: Option<&'n dyn super::body_host::BodySources>,
    /// Each body checked clean in this run: its read list's digest, by owner, filed where the
    /// check finished (front2 body_reads.rs); the list itself is dropped there.
    pub reads: super::body_reads::Digests,
    /// Each closure of each body checked clean in this run (its kind and signature, in `tys`),
    /// filed beside `reads`: what the world answers a closure of ANOTHER body from
    /// ([`super::foreign_closures`]).
    pub closures: super::foreign_closures::ClosureRows<Global>,
    /// Each fn whose signature writes a type that names nothing (E0412, [`HeadMiss`]), by the
    /// fn's checker key: the type is the error type in its head, so the fn's callers are checked
    /// against the rest of it; the fn's own body check says the error (front2 check.rs), in
    /// the store the head was made in or one filed from its record (`file_heads`: the record
    /// carries them; the engine's body stores hold no head made from text, so a miss kept only
    /// here was said by nothing once blaze-cli body_types/heads.rs went). Replaced whenever the
    /// head is made or filed again.
    pub type_misses: Rows<ItemKey, blaze_own::Vec<HeadMiss, Alloc<'static>>>,
    /// The body task's meter (front2 tables/cost.rs), when one measures: a measurement only.
    pub cost: Option<&'n super::tables::cost::Cost<'n>>,
    /// The request's table of trait goals shared by every body (front2 tables/goals.rs,
    /// blaze-solve share.rs), when the body task shares them; `None` solves every goal here.
    pub goals: Option<&'n dyn blaze_solve::share::GoalShare>,
    /// The shared goals the last check round of the body took (blaze-solve `GoalHit`): the
    /// one read each names, for the body's result (tables/task.rs `BodyOut::goal_hits`).
    pub goal_hits: Vec<blaze_solve::share::GoalHit, Global>,
    /// A measurement for tests, off unless set: the trait goals each check round's solver
    /// traces (blaze-solve witness.rs, "the trait-goal trace"), and the last round's trace.
    pub trace: Option<blaze_solve::witness::TraceAsk>,
    pub traced: Option<blaze_solve::witness::Trace<'static>>,
    /// The needs a step met beside the one it stopped at (`trait_members`), taken by the
    /// next step of [`Store::ensure_noting`].
    more_needs: Vec<LibKey, Global>,
    /// Resolved concrete return types (in `self.tys`) for functions whose written return
    /// signature contained one or more `_` placeholders (`-> _`, `-> Option<_>`, `-> Result<T, _>`),
    /// populated once each function's body has been checked.
    pub inferred_outputs: HashMap<ItemKey, PTy, Alloc<'static>>,
    /// Active worklist stack of functions whose return-type `_` placeholders are currently being
    /// inferred (used to detect mutual recursion cycles without native recursion).
    pub(crate) inferring_output_stack: blaze_own::Vec<ItemKey, Alloc<'static>>,
    /// Next unresolved `_`-returning callee discovered while inferring a function on the worklist.
    pub(crate) inferred_output_wanted: Option<ItemKey>,
    /// Mutual recursion cycle `(caller, callee)` discovered while inferring `_`-returning functions.
    pub(crate) inferred_output_cycle: Option<(ItemKey, ItemKey)>,
    /// True during the pre-unroll `inline for` probe check (allows downstream diagnostics that
    /// depend on the unexpanded loop body and skips filing `inferred_outputs`).
    pub(crate) inline_for_probe: bool,
    /// How a trait's members' needs are met: the seam the cost test proves equal.
    pub needs: NeedsMet,
    /// A body task's frozen heads (front2 tables/seeds.rs): with one, every head
    /// [`Store::ensure`] needs, the asked one first, is filed from its frozen record, never
    /// lowered; `None` lowers (today's path, and a store that freezes).
    pub seeds: Option<&'n dyn super::tables::seeds::HeadSeed<'n>>,
    /// Why the last seed failed ([`HeadError::Seed`] names the need; this, why).
    pub seed_error: Option<super::tables::seeds::SeedError>,
    /// The wants this store made by lowering (a step that made its want: `Store::step`),
    /// not counting those filed from frozen records ([`Store::file_heads`]): the measure a
    /// seeded freeze is held to (front2/tables/head_tests.rs).
    pub lowered: u32,
    /// The cells a body's world imported while a check ran (front2 world_items.rs
    /// `BodyWorld::store_want`, a store with `seeds`): a question that met one is asked
    /// again at once, never in a round more (`BodyWorld::again`).
    /// LISTED FOR DELETION with `AsksMet::Again` (front2 asks.rs, D161 step 3).
    pub imported: core::cell::Cell<u64>,
    /// How the store's world meets a head it lacks (front2 asks.rs [`AsksMet`](super::asks::AsksMet),
    /// the seam the oracle runs both ways): `Again` here, `Once` with its board when set.
    pub asks: super::asks::Asks,
    /// What the one ask counted in this store (blaze-ask `Counts`): under `Once`, its `made`
    /// is the count `Store::imports` watches in place of `imported`.
    pub asked: core::cell::RefCell<blaze_ask::Counts>,
    /// The gate of the body being checked (front2 asks.rs [`Gate`](super::asks::Gate)).
    pub gate: core::cell::RefCell<super::asks::Gate>,
}

/// How [`Store::ensure`] meets the needs of a trait's members (front2_tables_cost.rs runs
/// every body both ways and asserts equal bytes and read lists).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NeedsMet {
    /// Every member's need met before the trait is made again.
    All,
    /// The trait made again after each need (quadratic; the default again since clap-root's
    /// cycle under `All`, see `Store::new`).
    First,
}

/// A trait's implied clauses (on `Self`, and its header's associated type bounds:
/// `Preds::predicates_implying`) and its associated types with their keys and defaults (in
/// the store's table); made with its members ([`Want::Members`]).
pub struct TraitExtra {
    pub supertraits: Vec<PClause, Global>,
    pub assoc: Vec<tc::AssocTyDef, Global>,
    /// Its associated constants, declared or defaulted (the Reference, "Associated
    /// Constants"), in the order written: each with a row in `Store::fns` as an impl's has
    /// ([`ImplHead::consts`]); the row's `has_body` says a default is written.
    pub consts: Vec<(Name, ItemKey), Global>,
}

/// The step's result: done, the head it needs first, or the want it needs first (an impl's
/// head before one of its fns' signatures).
enum Step {
    Done,
    Need(LibKey),
    First(Want),
}

/// A want parked on [`Store::ensure_seeded`]'s stack (typeck.md "Head making waits"): the want
/// and, once a step reached its text, that text's lowering, kept until the want is made.
struct HeadFrame<'n> {
    want: Want,
    lowered: Option<FrameLowering<'n>>,
}

/// What a frame's first step read before it made anything: the item's detail, text and names
/// module, and the text's lowering. Every later step of the frame reads these, never the text.
struct FrameLowering<'n> {
    detail: ItemDetail,
    text: &'n str,
    module: LibKey,
    low: Lowered<'static, 'n>,
}

fn cycle(stack: &[HeadFrame<'_>]) -> Vec<LibKey, Global> {
    let mut out = Vec::with_capacity_in(stack.len(), Global);
    out.extend(stack.iter().map(|f| f.want.key()));
    out
}

fn bad(key: LibKey) -> impl Fn(Why) -> HeadError {
    move |why| HeadError::Item(key, why)
}


/// The fn's anonymous type parameters (the Reference, "Impl trait", "Anonymous type
/// parameters"): each `impl Trait` in a parameter's type, numbered from `base` (after the
/// declared generics) in order of appearance in the text, an outer one before those in its
/// bounds' type arguments (`impl Into<impl Show>`: the `Into` one first), and then those in its
/// bounds' associated type bindings (`impl IntoIterator<Item = impl Into<u64>>`: the
/// `IntoIterator` one, then the `Into<u64>` one; clap's `ArgGroup::args(self, ns: impl
/// IntoIterator<Item = impl Into<Id>>)`). The Reference, "Impl trait": "`impl Trait` can only
/// appear as a parameter or return type of a non-extern function": the inner one is in the
/// parameter's type, so it is an anonymous type parameter too; its bounds are filed by
/// `Preds::impl_bounds` from this list and the binding lowers to its `Param(i)` through
/// `env.anon`. A binding's type follows the path's type arguments, which is text order: a
/// binding is written after every argument of its segment, and only on a trait path's last
/// segment. Not walked to, so refused where made ("an `impl Trait` type outside a fn's
/// parameters and return type"): an `impl` in a parenthesized `Fn(..) -> ..` bound, in a
/// binding's own arguments (`Out<impl X> = Y`) or under an associated type bound
/// (`Item: Into<impl X>`). Note: `impl Into<impl Show>` is accepted here and refused by rustc
/// (E0666); left as it is. A work list, no recursion.
pub(super) fn anon_params(v: &View<'_>, base: u32, out: &mut Vec<(lf::TypeId, u32), Global>) -> Result<(), Why> {
    let mut todo: Vec<lf::TypeId, Global> = Vec::new_in(Global);
    let mut kids: Vec<lf::TypeId, Global> = Vec::new_in(Global);
    for &n in v.rows(v.list(head::PARAMS))?.iter().rev() {
        todo.extend(lf::Param::decode(n)?.ty);
    }
    while let Some(t) = todo.pop() {
        kids.clear();
        if let lf::TypeRef::Impl(bounds) = v.ty(t)? {
            out.push((t, base + out.len() as u32));
            for &b in v.rows(bounds)? {
                if let lf::Bound::Trait { ty, .. } = lf::Bound::decode(b)? {
                    // The path's type arguments, then its bindings' types; an associated type
                    // bound (`Error: Into<E>`) holds no type of the path: its bounds are
                    // clauses lowered by `Builder::bound` (bound_kids.rs). A bound that is no
                    // path is walked as a type.
                    if !super::bound_kids::bound_path_kids(v, ty, &mut kids)? {
                        kids.push(ty);
                    }
                }
            }
        } else {
            super::types::children(v, t, &mut kids)?;
        }
        todo.extend(kids.iter().rev());
    }
    Ok(())
}

/// What `param_names` does with a const generic parameter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Consts {
    /// Leave its name out (its index is still taken): the caller asks only which names are
    /// type parameters (filing an impl by its header, where a const parameter is never a type).
    Skip,
    /// Name it with its index, as a type parameter is ([`own_generics`] says which are const).
    Keep,
}

/// The generic type parameters of `v`'s item from index `base`, by name; their count.
pub(super) fn param_names(v: &View<'_>, base: u32, out: &mut Vec<(Name, u32), Global>, consts: Consts) -> Result<u32, Why> {
    let mut i = base;
    let list = v.list(head::GENERICS);
    for &n in v.rows(list)? {
        match lf::GenericParam::decode(n)? {
            lf::GenericParam::Lifetime { .. } => {}
            lf::GenericParam::Type { name, .. } => {
                out.push((Name(v.name(name)?), i));
                i += 1;
            }
            lf::GenericParam::Const { name, .. } => {
                if consts == Consts::Keep {
                    out.push((Name(v.name(name)?), i));
                }
                i += 1;
            }
        }
    }
    Ok(i - base)
}

/// A fn's written ABI: none is Rust's, a bare `extern` is C's; any other than "C", "C-unwind",
/// "system", "system-unwind" and "Rust" is refused.
fn abi_of(v: &View<'_>, whole: Range) -> Result<tc::Abi, Why> {
    if v.head[head::ABI] == u32::MAX {
        return Ok(tc::Abi::Rust);
    }
    let Some(r) = v.abi() else { return Ok(tc::Abi::C) };
    super::types::fn_ptr_abi(v.text_at(r)?, whole)
}

/// `Param(from)..Param(to)` as one list, a const parameter's as `ConstParam`.
fn params(tys: &mut PortableTys<'_>, from: u32, to: u32, consts: &[(u32, PTy)]) -> Result<List, Why> {
    let mut l = Vec::with_capacity_in((to - from) as usize, Global);
    for i in from..to {
        let n = if consts.iter().any(|(c, _)| *c == i) { PNode::ConstParam(i) } else { PNode::Param(i) };
        l.push(intern(tys, n)?);
    }
    intern_list(tys, &l)
}

/// The generic parameters of `v`'s item from index `base` (the Reference, "Generic
/// parameters"): each type and const parameter by name into `ps`, and each const parameter's
/// index and declared type into `consts` (its type is read in the item's module with no
/// parameters in scope: a const parameter's type may not name one, E0770). The type is any
/// the pinned core's `ConstParamTy_` impls can cover (integers, `char`, `bool`, a struct or
/// enum, tuples and arrays of those, `&T`, `&str`, `&[T]`: [`super::const_param_ty::admits`],
/// which refuses every other type by name). Their count.
pub(super) fn own_generics<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, module: LibKey, v: &View<'_>, base: u32, ps: &mut Vec<(Name, u32), Global>, consts: &mut Vec<(u32, PTy), Global>) -> Result<u32, Why> {
    let n = param_names(v, base, ps, Consts::Keep)?;
    let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[], body: None };
    let mut i = base;
    let list = v.list(head::GENERICS);
    for (k, &row) in v.rows(list)?.iter().enumerate() {
        match lf::GenericParam::decode(row)? {
            lf::GenericParam::Lifetime { .. } => {}
            lf::GenericParam::Type { .. } => i += 1,
            lf::GenericParam::Const { ty, .. } => {
                let t = b.ty(tys, &env, v, ty)?;
                // By the type's shape (const_param_ty.rs): `const ASSUME: Assume` (a struct,
                // core's `TransmuteFrom`), `const ORD: AtomicOrdering` (a fieldless enum),
                // `const FIELDS: &'static str`. A VALUE of a non-scalar type is refused where it
                // is read (types.rs `const_term`), never here.
                let scratch = tys.alloc();
                if let Err(no) = super::const_param_ty::admits(tys, t, scratch) {
                    let r = v.site(v.row_range(list.start + k as u32)?)?;
                    return Err(Why::Unsupported(no.text(), r));
                }
                consts.push((i, t));
                i += 1;
            }
        }
    }
    Ok(n)
}

/// The type `t` of a signature, or, where its path names nothing and the names library decided
/// so (front2 type_miss.rs), the error type, its miss pushed on `misses`: the head is made, its
/// callers checked against the rest of it, and the fn's `Signature` task says the E0412. `text`
/// is the text `v`'s ranges are relative to.
/// Its region slots go on `sink` as one fragment (types.rs `Builder::ty_regions`); a missed
/// type's fragment is empty and the sink refuses the head's regions by its site.
#[expect(clippy::too_many_arguments, reason = "one signature type: its builder, text, misses and region sink")]
fn ty_or_miss<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, t: lf::TypeId, text: &str, misses: &mut Vec<TypeMiss, Global>, sink: &mut RegionSink<'_>) -> Result<PTy, Why> {
    let mark = sink.mark();
    match b.ty_regions(tys, env, v, t, sink) {
        Err(Why::Resolve(crate::resolve::ResolveError::Path(e), r)) => match TypeMiss::of(b.resolver.names, text, e, r) {
            Some(miss) => {
                misses.push(miss);
                sink.reset(mark);
                sink.refuse(RegionRefusal::NoType { at: r });
                sink.push(&[], 0);
                intern(tys, PNode::Error)
            }
            None => Err(Why::Resolve(crate::resolve::ResolveError::Path(e), r)),
        },
        other => other,
    }
}

/// Each own generic parameter's default from index `base`, in order (the Reference, "Generic
/// parameters": a type parameter's is a type, a const parameter's a constant of its type), read
/// in `env` (the parameters before it in scope); `None` where none is written.
fn own_defaults<'n, S: NamesSource<'n>>(b: &Builder<'_, Maps, S>, tys: &mut PortableTys<'_>, env: &Env<'_>, v: &View<'_>, base: u32, out: &mut Vec<Option<PTy>, Global>) -> Result<(), Why> {
    let mut i = base;
    for &row in v.rows(v.list(head::GENERICS))? {
        match lf::GenericParam::decode(row)? {
            lf::GenericParam::Lifetime { .. } => continue,
            lf::GenericParam::Type { default, .. } => out.push(match default {
                Some(t) => Some(b.ty(tys, env, v, t)?),
                None => None,
            }),
            lf::GenericParam::Const { default, .. } => out.push(match default {
                Some(c) => {
                    let at = v.const_range(c)?;
                    let r = v.site(at)?;
                    let want = env.consts.iter().find(|(c, _)| *c == i).map(|&(_, t)| t);
                    let want = want.ok_or(Why::Program("a const parameter with no type".into(), r))?;
                    Some(b.const_term(tys, env, v.text_at(at)?, r, &|_| Ok(want))?)
                }
                None => None,
            }),
        }
        i += 1;
    }
    Ok(())
}

/// One item's signature, lowered from `text` under its crate's cfg.
pub(super) fn lower<'n, S: super::source::NamesSource<'n>>(names: S, key: LibKey, text: &'n str) -> Result<Lowered<'static, 'n>, HeadError> {
    let krate = names.crate_of(key).ok_or(HeadError::NoText(key))?;
    // Over records a crate with no items record has no atoms and no edition: the source filed
    // why (`source::SourceError::NoRecord`), and the item has no text to lower.
    let mut cfg = CrateCfg(names.cfg_atoms(krate).ok_or(HeadError::NoText(key))?);
    let edition = names.edition_of(krate).ok_or(HeadError::NoText(key))?;
    let extensions = names.blaze_extensions(krate);
    HeadLowerings::starting(names);
    blaze_lower::lower_signature_with_extensions(text, edition, extensions, &mut cfg, Alloc::global()).map_err(|_| HeadError::Cfg(key))
}

/// A member of a trait or an impl: its name, kind and text, as the names library lists it.
struct Member<'n> {
    lib: LibKey,
    name: &'n str,
    detail: ItemDetail,
    text: &'n str,
    /// The macro call in the container's body whose expansion it is in (its tokens' marks are
    /// that expansion's: front2 item_site.rs `member_marks`).
    written_by: Option<LibKey>,
}

/// The members of trait or impl `key` from `Names::members`: those written in its body and
/// those a macro call there expands to (core's `impl u32 { uint_impl! { .. } }`), one list.
fn members<'n, S: super::source::NamesSource<'n>>(names: S, key: LibKey) -> Result<Vec<Member<'n>, Global>, HeadError> {
    names.crate_of(key).ok_or(HeadError::NoText(key))?;
    if !names.has_text(key) {
        return Err(HeadError::NoText(key));
    }
    let mut out = Vec::new_in(Global);
    // A member of a kind whose detail the source cannot give (over records: one that holds
    // ranges of its text; the source filed the gap).
    let mut no_detail = false;
    // A container with no member has no row in the library's members table.
    names.each_member(key, &mut |m| {
        // The call itself: its expansion's members are listed beside it (`written_by`).
        if m.kind == blaze_surface::ItemKind::MacroCall {
            return;
        }
        match m.detail {
            Some(detail) => out.push(Member { lib: m.key, name: m.name.unwrap_or(""), detail, text: m.text, written_by: m.written_by }),
            None => {
                names.gap("a member's outline detail beyond its kind (heads.rs: members)", Some(m.key));
                no_detail = true;
            }
        }
    });
    if no_detail {
        return Err(HeadError::NoText(key));
    }
    Ok(out)
}

impl<'n> Store<'n> {
    /// An empty store; `sized` is the program's `#[lang = "sized"]` trait (none in a
    /// `no_core` rung that declares none).
    /// `frame` is the command's ([`super::Front2::frame`]), where bodies' local items are kept.
    /// `alloc` is the check's allocator, handed down from the program's entry: the tables are
    /// in it, and `tys.alloc()` gives it to everything the store keeps for the check.
    pub fn new(sized: Option<LibKey>, frame: &'n blaze_mem::Frame, alloc: Alloc<'static>) -> Store<'n> {
        Store {
            tys: match super::rows::stable_tys(alloc) {
                Ok(tys) => tys,
                // `new_stable` refuses only columns that already hold rows; these are new.
                Err(error) => panic!("the heads store's type table over fresh columns was refused: {error:?}"),
            },
            maps: Maps::new_in(alloc),
            adts: Rows::new_in(alloc),
            auto_copy_pub: Rows::new_in(alloc),
            adt_declarations: Rows::new_in(alloc),
            field_defaults: Rows::new_in(alloc),
            impls: Rows::new_in(alloc),
            provisional_impls: Rows::new_in(alloc),
            provisional_used: core::cell::RefCell::new(blaze_own::Vec::new_in(alloc)),
            fns: Rows::new_in(alloc),
            regions: super::head_record::RegionTable::new_in(alloc),
            region_gaps: Rows::new_in(alloc),
            container_regions: Rows::new_in(alloc),
            container_scopes: Rows::new_in(alloc),
            trait_inherits: Rows::new_in(alloc),
            preds: Rows::new_in(alloc),
            sized,
            from_trait: core::cell::Cell::new(None),
            alloc_shims: core::cell::Cell::new([None; 6]),
            pointers: super::receiver::Pointers::NOT_LOOKED_UP,
            trait_defs: Rows::new_in(alloc),
            fn_gaps: Rows::new_in(alloc),
            gat_const_params: Rows::new_in(alloc),
            member_frames: Rows::new_in(alloc),
            const_waits: Rows::new_in(alloc),
            ctors: Rows::new_in(alloc),
            statics: Rows::new_in(alloc),
            opaques: Rows::new_in(alloc),
            rpitit_traits: Rows::new_in(alloc),
            rpitit_defaults: Rows::new_in(alloc),
            rpitit_impls: Rows::new_in(alloc),
            hidden: super::opaque_auto::HiddenTable::new_in(alloc),
            locals: super::locals::Locals::new(frame),
            anon_marks: HashMap::new_in(frame),
            lives: super::lifetimes::Lives::new_in(alloc),
            sources: None,
            reads: super::body_reads::Digests::new(),
            closures: super::foreign_closures::ClosureRows::new_in(Global),
            type_misses: Rows::new_in(alloc),
            cost: None,
            goals: None,
            goal_hits: Vec::new_in(Global),
            trace: None,
            traced: None,
            more_needs: Vec::new_in(Global),
            inferred_outputs: HashMap::new_in(alloc),
            inferring_output_stack: blaze_own::Vec::new_in(alloc),
            inferred_output_wanted: None,
            inferred_output_cycle: None,
            inline_for_probe: false,
            seeds: None,
            seed_error: None,
            // `First` until `All` is proven on clap-root: under `All`, clap's `Cmd` was refused
            // with a cycle among core's `Iterator`, `Rev`, `DoubleEndedIterator`,
            // `ExactSizeIterator`, `FnMut`, `TrustedRandomAccessNoCoerce` (lane FL, 2026-11-17;
            // not reproduced by head_tests.rs `traits_naming_each_other_are_made_under_both_
            // strategies` nor by the cost test's real core).
            needs: NeedsMet::First,
            lowered: 0,
            imported: core::cell::Cell::new(0),
            asks: super::asks::Asks::Again,
            asked: core::cell::RefCell::new(blaze_ask::Counts::default()),
            gate: core::cell::RefCell::new(super::asks::Gate::Off),
        }
    }

    /// The frame impl `ck`'s member fns are made in, kept when its head was made (head
    /// records carry it, form 9).
    pub fn member_frame(&self, ck: ItemKey) -> Option<&MemberFrame> {
        self.member_frames.get(&ck)
    }

    /// Whether ADT `item` is eligible for Blaze-edition automatic `Copy`/`Clone`
    /// (`AdtFlags::AUTO_COPY_PUB`). Cached in `self.auto_copy_pub` so `AdtRow` and
    /// `head_record::VERSION` stay untouched; for an ADT loaded from a frozen head record,
    /// Rust-edition crates immediately answer and cache `false` without lowering text.
    pub(crate) fn is_auto_copy_pub<S: NamesSource<'n>>(&self, names: S, keys: &Keys, item: ItemKey) -> bool {
        if let Some(&v) = self.auto_copy_pub.get(&item) {
            return v;
        }
        let eligible = self.adts.get(&item).is_some_and(|a| a.kind == tc::AdtKind::Struct)
            && keys.library_in(names, item).is_some_and(|lib| {
                let Some(krate) = names.crate_of(lib) else { return false };
                if !matches!(names.std_is(krate), Some(blaze_unit_graph::StdIs::Bstd(_))) {
                    return false;
                }
                let Some(text) = names.signature_text_of(lib) else { return false };
                let Ok(low) = lower(names, lib, text.text) else { return false };
                let view = low.view();
                is_auto_copy_pub_view(names, lib, lib, tc::AdtKind::Struct, &view).is_ok_and(|x| x)
            });
        self.auto_copy_pub.insert_absent(item, eligible);
        eligible
    }

    /// The resolved return type of `key` in `self.tys`: for a function whose signature wrote `_`
    /// (`-> _`, `-> Option<_>`, `-> Result<T, _>`) and whose body has been checked, the concrete
    /// inferred return type; otherwise the head's written `output`.
    pub fn fn_output(&self, key: ItemKey) -> Option<PTy> {
        if let Some(&out) = self.inferred_outputs.get(&key) {
            return Some(out);
        }
        self.fns.get(&key).map(|f| f.output)
    }

    /// The concrete inferred return type (in `self.tys`) for a function whose return signature
    /// contained `_`, once its body has been checked (for callers and `blaze inspect` inlay hints).
    pub fn inferred_return_type(&self, key: ItemKey) -> Option<PTy> {
        self.inferred_outputs.get(&key).copied()
    }

    /// The inferred error type (in `self.tys`) for a `Result<T, _>` function, once its
    /// body has been checked (for `blaze inspect` inlay hints).
    pub fn inferred_error_set(&self, key: ItemKey) -> Option<PTy> {
        let out = self.inferred_outputs.get(&key).copied()?;
        let view = self.tys.view();
        match view.node(out).ok()? {
            PNode::Adt(_, args) | PNode::Union(args) => {
                let list = view.list(args).ok()?;
                (list.len() == 2).then_some(list[1])
            }
            _ => None,
        }
    }

    /// Whether `key` has a return-type `_` placeholder whose concrete type has not been inferred yet.
    pub fn has_unresolved_inferred_output(&self, key: ItemKey) -> bool {
        if self.inferred_outputs.contains_key(&key) {
            return false;
        }
        self.fns.get(&key).is_some_and(|f| has_infer_hole(self.tys.view(), f.output, self.tys.alloc()))
    }

    /// Copies a checked body's finalized return type `root` from `from` into `self.tys` and
    /// records it under `owner` in `self.inferred_outputs`. Returns `None` if `root` cannot be
    /// copied cleanly (unresolved hole/error type or full table).
    pub(crate) fn file_inferred_output(&mut self, owner: ItemKey, from: tc::PTys<'_>, root: PTy) -> Option<PTy> {
        let alloc = self.tys.alloc();
        let n = from.len();
        let mut seen: blaze_own::Vec<bool, Alloc<'static>> = blaze_own::Vec::with_capacity_in(n, alloc);
        seen.resize(n, false);
        let mut work: blaze_own::Vec<PTy, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        work.push(root);
        let mut steps = 0u32;
        while let Some(t) = work.pop() {
            steps += 1;
            if steps > 1 << 16 {
                return None;
            }
            let at = usize::try_from(t.0).ok()?;
            let slot = seen.get_mut(at)?;
            if *slot {
                continue;
            }
            *slot = true;
            let node = from.node(t).ok()?;
            from.children_of(node, &mut work).ok()?;
        }
        let mut mapped: blaze_own::Vec<Option<PTy>, Alloc<'static>> = blaze_own::Vec::with_capacity_in(n, alloc);
        mapped.resize(n, None);
        let mut kids: blaze_own::Vec<PTy, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        for i in 0..n {
            if !seen.get(i).copied().unwrap_or(false) {
                continue;
            }
            let idx = u32::try_from(i).ok()?;
            let node = from.node(PTy(idx)).ok()?;
            let m = |c: PTy, mapped: &[Option<PTy>]| -> Option<PTy> {
                usize::try_from(c.0)
                    .ok()
                    .and_then(|at| mapped.get(at).copied().flatten())
            };
            let mut l = |list: List, mapped: &[Option<PTy>], out: &mut PortableTys<'static>| -> Option<List> {
                kids.clear();
                for &c in from.list(list).ok()? {
                    kids.push(m(c, mapped)?);
                }
                out.intern_list(&kids).ok()
            };
            let copied = match node {
                PNode::Infer | PNode::Error | PNode::Future(..) => return None,
                p @ (PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Str
                | PNode::Never
                | PNode::Param(_)
                | PNode::ConstParam(_)
                | PNode::ExAuto(_)
                | PNode::Foreign(_)) => p,
                PNode::ConstValue { ty, bits } => PNode::ConstValue { ty: m(ty, &mapped)?, bits },
                PNode::Tuple(a) => PNode::Tuple(l(a, &mapped, &mut self.tys)?),
                PNode::Record(s, a) => {
                    if let Some(names) = from.record_names(s) {
                        self.tys.register_record_schema(s, names).ok()?;
                    }
                    PNode::Record(s, l(a, &mapped, &mut self.tys)?)
                }
                PNode::Union(a) => PNode::Union(l(a, &mapped, &mut self.tys)?),
                PNode::Ref(mu, a) => PNode::Ref(mu, m(a, &mapped)?),
                PNode::Ptr(mu, a) => PNode::Ptr(mu, m(a, &mapped)?),
                PNode::Array(a, len) => PNode::Array(m(a, &mapped)?, m(len, &mapped)?),
                PNode::Slice(a) => PNode::Slice(m(a, &mapped)?),
                PNode::Pat(a, r) => PNode::Pat(m(a, &mapped)?, l(r, &mapped, &mut self.tys)?),
                PNode::Adt(k, a) => PNode::Adt(k, l(a, &mapped, &mut self.tys)?),
                PNode::FnDef(k, a) => PNode::FnDef(k, l(a, &mapped, &mut self.tys)?),
                PNode::FnPtr(a, h) => PNode::FnPtr(l(a, &mapped, &mut self.tys)?, h),
                PNode::Closure { owner: o, ordinal, args } => PNode::Closure { owner: o, ordinal, args: l(args, &mapped, &mut self.tys)? },
                PNode::Coroutine { owner: o, ordinal, args } => PNode::Coroutine { owner: o, ordinal, args: l(args, &mapped, &mut self.tys)? },
                PNode::Alias { trait_, name, args } => PNode::Alias { trait_, name, args: l(args, &mapped, &mut self.tys)? },
                PNode::Opaque(k, a) => PNode::Opaque(k, l(a, &mapped, &mut self.tys)?),
                PNode::Dynamic(a) => PNode::Dynamic(l(a, &mapped, &mut self.tys)?),
                PNode::ExTrait(k, a) => PNode::ExTrait(k, l(a, &mapped, &mut self.tys)?),
                PNode::ExProj { trait_, name, args, term } => {
                    PNode::ExProj { trait_, name, args: l(args, &mapped, &mut self.tys)?, term: m(term, &mapped)? }
                }
            };
            if let Some(slot) = mapped.get_mut(i) {
                *slot = Some(self.tys.intern(copied).ok()?);
            }
        }
        let copied = usize::try_from(root.0)
            .ok()
            .and_then(|at| mapped.get(at).copied().flatten())?;
        self.inferred_outputs.insert(owner, copied);
        Some(copied)
    }

    /// What a [`Why::Need`] of `k` asks for: its head, or, for a trait whose head is made,
    /// its members (where its supertraits are filed).
    pub fn need(&self, k: LibKey) -> Want {
        if self.maps.traits.contains_key(&k) { Want::Members(k) } else { Want::Head(k) }
    }

    /// The marks the tokens of `key`'s text `text` keep, which its head's paths resolve by
    /// (front2 item_site.rs, token-mark-F1). An item declared in a body keeps its enclosing
    /// body's marks when a macro wrote that body (locals.rs `LocalHead::marks`: bstd's `main!`
    /// writes `static MANIFEST: $crate::grant::Manifest` in the user's `main`), read at its
    /// block's module.
    fn marks<S: NamesSource<'n>>(&self, names: S, key: LibKey, text: &str) -> Result<Option<TextMarks<'n>>, Why> {
        if let Some(local) = self.locals.head(checker_of(key)) {
            return Ok(local.marks);
        }
        super::item_site::item_marks(names, key, text).map_err(MarksLost::why)
    }

    pub(super) fn free_body_head<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, item: LibKey) -> Result<FreeBodyHead<'_, 'n>, HeadError> {
        let want = Want::Fn(item);
        self.ensure(names, keys, want)?;
        let key = checker_of(item);
        let head = self.fns.get(&key).ok_or(HeadError::Filed { want, at: HeadAt::Fn(key), what: "the free item's body needs its function head" })?;
        let module = self.locals.head(key).map_or(head.module, |local| local.module);
        let marks = self.marks(names, item, head.text).map_err(|why| HeadError::Item(item, why))?;
        Ok(FreeBodyHead { head, module, marks })
    }

    /// Makes `want` and every head it needs, by a work list.
    pub fn ensure<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, want: Want) -> Result<(), HeadError> {
        self.ensure_noting(names, keys, want, &mut |_| {})
    }

    /// [`Store::ensure`], handing `noted` every other want it is made to make on the way (each
    /// need, at the moment it is met, before it is made). Over a store that held nothing, that
    /// is every head `want`'s making read beside its own (front2/tables/head.rs `freeze`).
    pub fn ensure_noting<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, want: Want, noted: &mut dyn FnMut(Want)) -> Result<(), HeadError> {
        let Some(seeds) = self.seeds else {
            return self.ensure_seeded(names, keys, want, noted, &mut |_, _, _| Ok(Seeded::Lower));
        };
        // Seeded: the asked want first (`ensure_seeded` seeds only what a step needs), then
        // every need its steps meet. Its record files it whole; the steps below find it made.
        let scratch = self.tys.alloc();
        let mut failed = None;
        let made = {
            let mut seed = |store: &mut Store<'n>, keys: &mut Keys, need: Want| {
                seeds.seed(store, keys, need, scratch).map_err(|e| {
                    failed = Some(e);
                    SeedFailed
                })
            };
            match if self.made(want) { Ok(Seeded::Filed) } else { seed(self, keys, want) } {
                Err(SeedFailed) => Err(HeadError::Seed { need: want, by: want }),
                Ok(_) => self.ensure_seeded(names, keys, want, noted, &mut seed),
            }
        };
        if failed.is_some() {
            self.seed_error = failed;
        }
        made
    }

    /// [`Store::ensure_noting`], asking `seed` first of each need as it is met: [`Seeded::Filed`]
    /// (its cell filed from a frozen record, [`Store::file_heads`]) makes it without a
    /// lowering; [`Seeded::Lower`] lowers it here. A seed that fails is
    /// [`HeadError::Seed`]: the need is never lowered instead (rule 3).
    pub fn ensure_seeded<S: NamesSource<'n>>(
        &mut self,
        names: S,
        keys: &mut Keys,
        want: Want,
        noted: &mut dyn FnMut(Want),
        seed: &mut dyn FnMut(&mut Store<'n>, &mut Keys, Want) -> Result<Seeded, SeedFailed>,
    ) -> Result<(), HeadError> {
        let _cost = super::tables::cost::Cost::enter_want(self.cost, want);
        // One parked frame per want (typeck.md "Head making waits"): its lowering is kept
        // across its steps, so a step after a need never lowers the text again.
        let mut stack: Vec<HeadFrame<'n>, Alloc<'static>> = Vec::new_in(self.tys.alloc());
        stack.push(HeadFrame { want, lowered: None });
        // The want made by the step before: asked again at once, it was made and its lookup
        // still fails (no progress), which is named, never looped until `MOST_STEPS`.
        let mut just_made = None;
        for _ in 0..MOST_STEPS {
            let stepped = super::tables::cost::Cost::step_start(self.cost);
            self.more_needs.clear();
            let (top, step) = {
                let Some(frame) = stack.last_mut() else { return Ok(()) };
                let top = frame.want;
                (top, self.step(names, keys, top, &mut frame.lowered)?)
            };
            super::tables::cost::Cost::step_end(self.cost, stepped, matches!(step, Step::Need(_)));
            let more = core::mem::replace(&mut self.more_needs, Vec::new_in(Global));
            let need = match step {
                Step::Done => {
                    just_made = stack.pop().map(|f| f.want);
                    continue;
                }
                Step::Need(k) => self.need(k),
                Step::First(w) => w,
            };
            if just_made == Some(need) {
                return Err(HeadError::NeedMade { need, by: top });
            }
            if stack.iter().any(|f| f.want == need) {
                return Err(HeadError::Cycle(cycle(&stack)));
            }
            just_made = None;
            // The step's other needs, made after `need` and before `top` steps again; one
            // already made, or on the stack (a cycle: met by name when `top` steps again and
            // stops at it alone), is left to that step.
            if matches!(step, Step::Need(_)) {
                for k in more.iter().rev() {
                    let w = self.need(*k);
                    if w != need && !self.made(w) && !stack.iter().any(|s| s.want == w) {
                        noted(w);
                        match seed(self, keys, w) {
                            Ok(Seeded::Filed) => {}
                            Ok(Seeded::Lower) => stack.push(HeadFrame { want: w, lowered: None }),
                            Err(SeedFailed) => return Err(HeadError::Seed { need: w, by: top }),
                        }
                    }
                }
            }
            noted(need);
            match seed(self, keys, need) {
                Ok(Seeded::Filed) => {
                    // Asked again at once by `top`, the filed record did not make what it reads.
                    just_made = Some(need);
                    continue;
                }
                Ok(Seeded::Lower) => {}
                Err(SeedFailed) => return Err(HeadError::Seed { need, by: top }),
            }
            stack.push(HeadFrame { want: need, lowered: None });
        }
        Err(HeadError::Cycle(cycle(&stack)))
    }

    /// One step of `want`'s making. `lowered` is its frame's lowering: made on the first step
    /// that reaches the text, then read by every later step of the same frame (a pure function
    /// of the text, the crate's cfg and edition, which no step changes).
    /// `key`'s text lowered into `lowered`, in a frame of its own: [`Store::step`]'s frame
    /// stays on the stack under every head the step makes (blaze-int notes t29
    /// overflow-bt4.txt: 31,104 bytes), and a debug frame gives the lowering, its `Result` and
    /// the row it is moved into a slot each.
    #[inline(never)]
    fn lower_frame<'f, S: NamesSource<'n>>(&self, names: S, key: LibKey, lowered: &'f mut Option<FrameLowering<'n>>) -> Result<&'f mut FrameLowering<'n>, HeadError> {
        let resolver = Paths { names };
        // An item declared in a body: its row's text and its block's module
        // (`Locals::head_of`).
        let (detail, text, module) = match self.locals.head_of(key) {
            Some(local) => local?,
            None => {
                let detail = resolver.detail(key).ok_or(HeadError::NoText(key))?;
                // One row over records: the text and the module its names are read in.
                let it = names.signature_text_of(key).ok_or(HeadError::NoText(key))?;
                (detail, it.text, it.module)
            }
        };
        let low = lower(names, key, text)?;
        Ok(lowered.insert(FrameLowering { detail, text, module, low }))
    }

    fn step<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, want: Want, lowered: &mut Option<FrameLowering<'n>>) -> Result<Step, HeadError> {
        let key = want.key();
        let ck = checker_of(key);
        // The one test the world's answers share (front2 by_origin.rs `Store::made`).
        let done = self.made(want);
        if done {
            return Ok(Step::Done);
        }
        if !matches!(keys.origin(ck), Some(super::keys::Origin::Item(lib)) if *lib == key) {
            keys.item(key).map_err(HeadError::Key)?;
        }
        // An impl's fn: its impl's head first (its frame, associated types and clauses), then
        // its own signature alone.
        if let Want::MemberFn(impl_, f) = want {
            if !self.impls.contains_key(&ck) {
                return Ok(Step::First(Want::Impl(impl_)));
            }
            // An impl filed from a record whose lifetime part was never made (lifetime parts
            // are made on demand: `Store::make_lives`) has no members' scope yet: made now from
            // its header's text, the names `impl_` would have filed.
            if !self.container_scopes.contains_key(&ck) {
                self.impl_scope_from_text(names, impl_)?;
            }
            return match self.member_fn(names, keys, impl_, f) {
                Ok(()) => {
                    self.lowered += 1;
                    Ok(Step::Done)
                }
                Err(Why::Need(k)) => Ok(Step::Need(k)),
                Err(why) => Err(HeadError::Item(impl_, why)),
            };
        }
        let frame = match lowered {
            Some(frame) => frame,
            None => self.lower_frame(names, key, lowered)?,
        };
        let (detail, text, module) = (frame.detail, frame.text, frame.module);
        let v = frame.low.view();
        let got = match (want, detail) {
            (Want::Head(_), ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => {
                self.adt_head(names, key, module, &v)
            }
            (Want::Head(_), ItemDetail::Trait) => self.trait_head(names, keys, key, module, &v, text),
            (Want::Head(_), ItemDetail::TraitAlias) => self.trait_alias(names, keys, key, module, &v),
            (Want::Head(_), ItemDetail::TypeAlias) => match self.alias(names, keys, key, module, &v) {
                Err(why @ (Why::Need(_) | Why::NeedConst(..))) => Err(why),
                Err(why) => {
                    self.maps.aliases.insert(key, None);
                    self.maps.alias_gaps.insert(key, AliasGap::made(why));
                    Ok(())
                }
                Ok(()) => Ok(()),
            },
            (Want::Adt(_), ItemDetail::Struct | ItemDetail::Enum | ItemDetail::Union) => {
                if !self.maps.generics.contains_key(&key) {
                    return Ok(Step::Need(key));
                }
                self.adt(names, keys, key, module, &v, detail)
            }
            (Want::Members(_), ItemDetail::Trait) => {
                if !self.maps.traits.contains_key(&key) {
                    return Ok(Step::Need(key));
                }
                self.trait_members(names, keys, key, module, &v)
            }
            (Want::Impl(_), ItemDetail::Impl) => self.impl_(names, keys, key, module, &v),
            // A free constant's head is its type as a fn of no inputs (the checker reads a
            // `Def::Const` through `fn_sig`); its body is lowered by `lower_fn` like a fn's.
            (Want::Fn(_), ItemDetail::Fn { .. } | ItemDetail::Const { .. }) => {
                let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[], body: None };
                let marks = self.marks(names, key, text).map_err(|why| HeadError::Item(key, why))?;
                self.fn_head(names, keys, key, ck, None, &env, 0, &[], &v, text, detail, marks)
            }
            // A static's head is its type, as a const's, and its mutability (the Reference,
            // "Static items": "Mutable statics", `static mut`).
            (Want::Fn(_), ItemDetail::Static { .. }) => {
                let env = Env { module, params: &[], bounds: Bounds::Done(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &[], body: None };
                let marks = self.marks(names, key, text).map_err(|why| HeadError::Item(key, why))?;
                let made = self.fn_head(names, keys, key, ck, None, &env, 0, &[], &v, text, detail, marks);
                if made.is_ok() {
                    let m = if v.head[head::FLAGS] & sig::MUT != 0 { tc::Mutbl::Mut } else { tc::Mutbl::Not };
                    self.statics.insert(ck, m);
                }
                made
            }
            // Rule 19: every detail named (the shared names say "this item" for the kinds the
            // old front end made, which here are a head asked for under another want).
            (w, d) => {
                let what = match (w, d) {
                    (Want::Head(_), ItemDetail::Fn { .. }) => "a fn (asked for as a type's or trait's head: a `Why::Need` of a fn's key)",
                    (Want::Adt(_), ItemDetail::Fn { .. }) => "a fn (asked for as an ADT)",
                    (Want::Impl(_), ItemDetail::Fn { .. }) => "a fn (asked for as an impl)",
                    (Want::Members(_), ItemDetail::Fn { .. }) => "a fn (asked for as a trait's members)",
                    (Want::MemberFn(..), ItemDetail::Fn { .. }) => "a fn (asked for as an impl's member fn)",
                    (Want::Head(_), ItemDetail::Impl) => "an impl (asked for as a type's or trait's head: `Store::need` of an impl's key; an impl's head is `Want::Impl`, its fn's `Want::MemberFn`)",
                    (Want::Members(_), ItemDetail::Impl) => "an impl (asked for as a trait's members: an impl's head is `Want::Impl`, its fn's `Want::MemberFn`)",
                    (Want::Adt(_), ItemDetail::Impl) => "an impl (asked for as an ADT)",
                    (Want::Fn(_), ItemDetail::Impl) => "an impl (asked for as a fn)",
                    (_, d) => match d {
                    ItemDetail::Struct => "a struct (asked for as other than an ADT)",
                    ItemDetail::Enum => "an enum (asked for as other than an ADT)",
                    ItemDetail::Trait => "a trait (asked for as other than a trait)",
                    ItemDetail::Fn { .. } => "a fn (asked for as other than a fn)",
                    ItemDetail::Mod { .. } => "a module",
                    ItemDetail::Use { .. } => "a `use`",
                    ItemDetail::ExternBlock { .. } => "an `extern` block",
                    d => crate::front::detail_name(d),
                    },
                };
                return Err(HeadError::NotAHead(key, what));
            }
        };
        match got {
            Ok(()) => {
                self.lowered += 1;
                Ok(Step::Done)
            }
            Err(Why::Need(k)) => Ok(Step::Need(k)),
            Err(why) => Err(HeadError::Item(key, self.named_local(names, keys, ck, why))),
        }
    }

    /// `why`, refusing the head of item `ck`, with the item named when it is declared in a body
    /// (rule 19): its key is derived at its body's lowering and names nothing a person can
    /// find (std hello's stop printed "item ItemKey(1ef15ca1d9b8)"). A [`Why::Unsupported`]
    /// becomes [`Why::UnsupportedIn`]: its kind, its name as written, and the body it is
    /// declared in by that item's path ("a static in a block `__RUST_STD_INTERNAL_VAL`,
    /// declared in the body of std::io::stdio::OUTPUT_CAPTURE (key ..)"). Every other `why`,
    /// and every item with a names row, is returned as it is.
    fn named_local<S: NamesSource<'n>>(&self, names: S, keys: &Keys, ck: ItemKey, why: Why) -> Why {
        let Why::Unsupported(what, at) = why else { return why };
        let Some(h) = self.locals.head(ck) else { return Why::Unsupported(what, at) };
        let name = self.locals.body(h.enclosing).and_then(|b| b.rows.iter().find(|r| r.key == h.lib)).map_or("<a name its body's rows do not hold>", |r| r.name);
        let body = super::item_shown::ItemShown::of(names, keys, h.enclosing);
        Why::UnsupportedIn(what, format!("{} `{name}`, declared in the body of {body}", h.kind.form()).into_boxed_str(), at)
    }

    /// Record the definition the evaluator consumes. The module of a synthetic
    /// constant is its resolution context, not the source of its expression.
    pub(super) fn read_const_body<S: NamesSource<'n>>(&self, names: S, keys: &Keys, mut item: ItemKey) -> Result<(), super::body::BodyError> {
        use super::tables::recorder::{ReadSink, TableRead};
        let Some(reads) = &self.maps.const_text_reads else { return Ok(()) };
        loop {
            if let Some(local) = self.locals.head(item) {
                item = local.enclosing;
            } else if let Some(super::keys::Origin::Member { container, kind, name }) = keys.origin(item) {
                if self.locals.head(*container).is_some() {
                    item = *container;
                    continue;
                }
                if *kind == MemberKind::Anon {
                    // Its expression is part of the requesting body or signature.
                    // The definitions it calls are recorded individually.
                    return Ok(());
                }
                if let Some(head) = self.fns.get(&item) {
                    let (_, member) = keys.source_member(names, item, head.text).ok_or(super::body::BodyError::NoHead(item))?;
                    reads.read(TableRead::Text { key: member.key });
                } else if let Some(owner) = keys.library_in(names, *container) {
                    // A refused member has no usable head. Its written declarations
                    // still supply the source of the refusal, including duplicates.
                    let written = match *kind {
                        MemberKind::Fn => blaze_surface::ItemKind::Function,
                        MemberKind::Const => blaze_surface::ItemKind::Const,
                        _ => return Ok(()),
                    };
                    names.each_member(owner, &mut |member| {
                        if member.kind == written && member.name == Some(name.as_ref()) {
                            reads.read(TableRead::Text { key: member.key });
                        }
                    });
                }
                return Ok(());
            } else {
                let key = keys.library_in(names, item).ok_or(super::body::BodyError::NoHead(item))?;
                reads.read(TableRead::Text { key });
                return Ok(());
            }
        }
    }

    /// The heads of cell `want` read from its frozen record (front2/tables/head.rs `import`,
    /// into this store's `tys`), filed where `Store::step` files them: the inverse of
    /// head_record.rs `from_store`, so each want the cell makes is [`Store::made`] with no
    /// lowering. An impl's frame's const parameters are the impl's own, filed under `want`'s
    /// library key as `Store::impl_` files them. Not in the form, so never filed: lifetime parts
    /// (made from the text when asked), the job's key origins (the caller files them).
    /// Through `&self`: every table it files into is [`Rows`], so a head imported while a
    /// body's check holds answers from this store moves none of them.
    pub fn file_heads<I: IntoIterator<Item = (HeadAt, Head<'n>)>>(&self, want: Want, heads: I) -> Result<(), HeadError> {
        let lib = want.key();
        for (at, head) in heads {
            match head {
                Head::Fn { key, head, preds, static_, regions, misses } => {
                    if let Some(r) = regions {
                        self.regions.file(key, r);
                    }
                    // The signature's E0412s, as the store that made the head kept them: the
                    // fn's own body check says them (front2 check.rs).
                    if misses.is_empty() {
                        self.type_misses.remove(&key);
                    } else {
                        self.type_misses.insert(key, misses);
                    }
                    if let Some(p) = preds {
                        self.preds.insert(key, blaze_own::Box::into_inner(p));
                    }
                    if let Some(m) = static_ {
                        self.statics.insert(key, m);
                    }
                    // A free function's const parameters are read through `Maps` while a
                    // body converts its generic arguments.  Imported fn heads carry that
                    // row only in `FnHead::consts`, so publish it under the library key here.
                    // Members keep their container's parameters in this field and must stay
                    // out of the free-item map.
                    if head.container.is_none() {
                        self.maps.const_params.insert(head.lib, head.consts.to_vec_in(*head.consts.allocator()));
                    }
                    self.fns.insert(key, head);
                }
                Head::Adt { key, row, defaults, const_params, field_defaults, preds, life, fieldless, regions } => {
                    if let Some(l) = life {
                        self.file_adt_life(key, l);
                    }
                    let ck = checker_of(key);
                    if let Some(r) = regions {
                        self.regions.file(ck, r);
                    }
                    // The walk reads a library ADT's count from the names (D155, types.rs
                    // `declared_lifetimes`), and this map only for a BODY-LOCAL one (no item in
                    // the names world), whose row is the one home of its count.
                    self.maps.adt_lifetimes.insert(ck, row.lifetimes);
                    if let Some(n) = fieldless {
                        self.maps.fieldless_enums.insert(ck, n);
                    }
                    self.maps.generics.insert(key, (row.generics, defaults.iter().copied().collect()));
                    self.adt_declarations.insert(ck, AdtDeclaration { lifetimes: row.lifetimes, marks: row.marks });
                    self.maps.const_params.insert(key, const_params.iter().copied().collect());
                    self.field_defaults.insert(ck, field_defaults.iter().copied().collect());
                    if let Some(p) = preds {
                        self.preds.insert(ck, blaze_own::Box::into_inner(p));
                    }
                    self.adts.insert(ck, row);
                }
                Head::Ctor { key, sig, regions } => {
                    if let Some(r) = regions {
                        self.regions.file(key, r);
                    }
                    self.ctors.insert(key, sig);
                }
                Head::Trait { key, head, extra, supers, const_params, preds, assoc_side, life, regions } => {
                    let ck = checker_of(key);
                    // Its declared counts are the names' (D155, types.rs `declared_lifetimes`):
                    // the record carries none and nothing is filed for them.
                    if let Some(Ok(l)) = &life {
                        // Its members' container group and names (D148), as `trait_head` files
                        // them: its declared lifetimes.
                        // A trait has no regions side here: its record's lifetime names that do
                        // not line up are the record's own error.
                        self.restore_container(ck, l, None).map_err(|what| HeadError::Filed { want, at, what })?;
                        if let Some(r) = regions.as_ref() {
                            let param_bounds = extract_container_param_bounds(self.tys.view(), &r.regions().clauses, self.tys.alloc());
                            if !param_bounds.is_empty()
                                && let Some(prev) = self.container_regions.get(&ck)
                            {
                                let mut names = Vec::with_capacity_in(prev.names.len(), self.tys.alloc());
                                names.extend_from_slice(&prev.names);
                                self.container_regions.insert(ck, ContainerRegions {
                                    names,
                                    count: prev.count,
                                    self_regions: None,
                                    self_binders: 0,
                                    trait_regions: None,
                                    param_bounds,
                                });
                            }
                        }
                    }
                    if let Some(l) = life {
                        self.file_container_life(ck, l);
                    }
                    if let Some(r) = regions {
                        self.regions.file(ck, r);
                    }
                    let trait_generics = head.generics;
                    for side in assoc_side {
                        let item = extra.as_ref().and_then(|x| x.assoc.iter().find(|a| a.name == side.name)).map(|a| a.item);
                        if let Some(n) = side.generics {
                            self.maps.assoc_generics.insert((key, side.name), n);
                        }
                        if let Some(w) = side.where_ {
                            self.maps.assoc_where.insert((key, side.name), w);
                        }
                        if let Some(c) = &side.consts {
                            self.maps.assoc_consts.insert((key, side.name), c.iter().copied().collect());
                        }
                        if let (Some(item), Some(generics)) = (item, side.generics)
                            && generics != 0
                        {
                            let mut checker = Vec::with_capacity_in(const_params.len() + side.consts.as_ref().map_or(0, |c| c.len()), self.tys.alloc());
                            checker.extend_from_slice(&const_params);
                            if let Some(own) = &side.consts {
                                checker.extend(own.iter().map(|&(i, t)| (trait_generics + i, t)));
                            }
                            self.gat_const_params.insert(item, checker);
                        }
                        // Its item bounds (form 11), under its key as `Store::trait_members`
                        // files them (the decoder refuses bounds with no members row).
                        if let Some(b) = side.bounds {
                            let item = extra.as_ref().and_then(|x| x.assoc.iter().find(|a| a.name == side.name)).map(|a| a.item);
                            let Some(item) = item else {
                                return Err(HeadError::Filed { want, at, what: "an associated type's item bounds with no members row" });
                            };
                            self.preds.insert(item, blaze_own::Box::into_inner(b));
                        }
                    }
                    // Filed with its members (`Store::trait_members`), as `trait_defs` is.
                    if extra.is_some() || !supers.is_empty() {
                        self.maps.supers.insert(key, supers.iter().copied().collect());
                    }
                    if let Some(x) = extra {
                        self.trait_defs.insert(ck, x);
                    }
                    if let Some(p) = preds {
                        self.preds.insert(ck, blaze_own::Box::into_inner(p));
                    }
                    self.maps.const_params.insert(key, const_params.iter().copied().collect());
                    self.maps.traits.insert(key, head);
                }
                Head::Impl { key, head, preds, frame, life, regions } => {
                    if key != checker_of(lib) {
                        return Err(HeadError::Filed { want, at, what: "an impl other than the cell's own" });
                    }
                    // Its members' container group and names (D148), as `impl_` files them: the
                    // group is its filed regions' count, when they were made.
                    // A disagreement there is the regions' and never the head's (D148: the
                    // erased head is whole): the head is filed, its regions refused by name
                    // (`RegionsUnread`), and its members' scope made from its header's text
                    // when a member is first made (`impl_scope_from_text`, over no regions).
                    let mut regions = regions;
                    if let Some(Ok(l)) = &life
                        && let Err(what) = self.restore_container(key, l, Some((regions.as_ref(), head.self_ty)))
                    {
                        regions = None;
                        self.container_regions.remove(&key);
                        self.container_scopes.remove(&key);
                        self.regions.file_unlaid(key, super::head_record::Unlaid::Disagree, what);
                    }
                    if let Some(l) = life {
                        self.file_container_life(key, l);
                    }
                    if let Some(r) = regions {
                        self.regions.file(key, r);
                    }
                    if let Some(f) = frame {
                        self.maps.const_params.insert(lib, f.consts.to_vec_in(*f.consts.allocator()));
                        self.member_frames.insert(key, f);
                    }
                    if let Some(p) = preds {
                        self.preds.insert(key, blaze_own::Box::into_inner(p));
                    }
                    self.impls.insert(key, head);
                }
                Head::Alias { key, generics, ty, defaults, const_params, slots } => {
                    match slots {
                        Some(laid) => {
                            self.maps.alias_slots.insert(key, laid);
                        }
                        None => {
                            self.maps.alias_slots.remove(&key);
                        }
                    }
                    self.maps.alias_defaults.insert(key, defaults.iter().copied().collect());
                    self.maps.const_params.insert(key, const_params.iter().copied().collect());
                    self.maps.aliases.insert(key, Some((generics, ty)));
                }
                Head::AliasRefused { key, why } => {
                    let text = core::str::from_utf8(&why).map_err(|_| HeadError::Filed { want, at, what: "a refused type alias whose why is not UTF-8" })?;
                    self.maps.aliases.insert(key, None);
                    self.maps.alias_gaps.insert(key, AliasGap { text: text.into(), why: None });
                }
                // A member refused alone: its gap again, its why the record's text (form 13).
                Head::FnRefused { key, why } => {
                    let text = core::str::from_utf8(&why).map_err(|_| HeadError::Filed { want, at, what: "a refused member fn whose why is not UTF-8" })?;
                    self.fn_gaps.insert(key, Why::Program(text.into(), Range::new(0, 0)));
                    self.regions.file_refused(key);
                }
                Head::TraitAlias { key, head } => {
                    self.maps.trait_aliases.insert(key, head);
                }
                Head::Opaque { key, fn_, index, preds, regions } => {
                    // Its captured lifetimes, for the one region counter (types.rs
                    // `HeadsArity`), as `fn_head` files them (D148).
                    if let Some(n) = regions.as_ref().and_then(|r| r.regions().opaque_captures) {
                        self.maps.opaque_lifetimes.insert(key, n);
                    }
                    if let Some(r) = regions {
                        self.regions.file(key, r);
                    }
                    if let Some(p) = preds {
                        self.preds.insert(key, blaze_own::Box::into_inner(p));
                    }
                    self.opaques.insert(key, (fn_, index));
                }
            }
        }
        Ok(())
    }

    /// A restored trait's or impl's `container_regions` and `container_scopes` (D148), from its
    /// record's lifetime part `life` and, for an impl (`impl_`), its filed regions and self
    /// type: what `trait_head` and `impl_` file when they make the head, so a member made fresh
    /// under a restored container is numbered as it would be under a fresh one. Where the two
    /// records meet (the lifetime part's names, the regions' group) they must agree; where they
    /// do not, the refusal is returned by name and nothing is filed: for an impl the caller
    /// refuses its regions softly (its erased head is whole and filed), for a trait it is the
    /// record's error.
    ///
    /// The self type is NOT compared position by position: the lifetime part counts in its own
    /// grain (lifetimes.rs `Reader::count`: one `Hidden` position per projection, opaque or
    /// trait object, one per ADT under `Grain::OnePerAdt`), the regions in the one region
    /// counter's (`regions::slots`: a projection `<T as AsULE>::ULE` has its trait's, its own
    /// and its arguments' slots, none here), so `impl<T: AsULE> AsRef<..> for &[T::ULE]` is two
    /// positions in one and one slot in the other, both right. A fresh impl (`impl_`) never
    /// compared them; the members' `Self` is the regions' self column alone.
    fn restore_container(&self, ck: ItemKey, life: &super::lifetimes::ContainerLife, impl_: Option<(Option<&super::head_record::FiledRegions>, PTy)>) -> Result<(), &'static str> {
        let alloc = self.tys.alloc();
        let bad = |what: &'static str| what;
        // Its declared lifetimes' names in declaration order.
        let mut names: Vec<Name, Alloc<'static>> = Vec::with_capacity_in(life.lifetimes as usize, alloc);
        names.resize(life.lifetimes as usize, Name(0));
        let mut seen: Vec<bool, Alloc<'static>> = Vec::with_capacity_in(life.lifetimes as usize, alloc);
        seen.resize(life.lifetimes as usize, false);
        for &(hash, i) in life.names.iter() {
            let slot = seen.get_mut(i as usize).ok_or(bad("a container's lifetime name past its declared count"))?;
            if *slot {
                return Err(bad("a container's lifetime index named twice"));
            }
            *slot = true;
            names[i as usize] = Name(hash);
        }
        if seen.iter().any(|s| !s) {
            return Err(bad("a container's declared lifetime with no name"));
        }
        // Where the lifetime part and the filed regions meet: the group holds the declared
        // and the self type's elided lifetimes (both count an elided header lifetime as one
        // anonymous parameter after the declared ones; the regions' group also counts those
        // the trait reference elides, so it may be larger).
        if let Some((Some(r), _)) = impl_ {
            let h = r.regions();
            if h.impl_header.is_none() {
                return Err(bad("an impl's filed regions with no header columns"));
            }
            let declared_and_self = life.lifetimes.checked_add(life.elided).ok_or(bad("an impl's lifetime counts past u32"))?;
            if h.params.count() < declared_and_self {
                return Err(bad("an impl whose lifetime part and filed regions disagree on its group (D148)"));
            }
        }
        self.file_container(ck, &names, impl_)
    }

    /// The `container_regions` and `container_scopes` of the trait or impl `ck` (D148) whose
    /// declared lifetimes are `names` (declaration order) and, for an impl (`impl_`), whose
    /// filed regions (none: refused) and self type are given: what `trait_head` and `impl_`
    /// file when they make the head. A trait's group is its declared lifetimes; an impl's, its
    /// filed regions' count (declared, then each lifetime elided in its header).
    fn file_container(&self, ck: ItemKey, names: &[Name], impl_: Option<(Option<&super::head_record::FiledRegions>, PTy)>) -> Result<(), &'static str> {
        let alloc = self.tys.alloc();
        let declared = u32::try_from(names.len()).map_err(|_| "a container with more lifetimes than a u32 counts")?;
        let mut owned: Vec<Name, Alloc<'static>> = Vec::with_capacity_in(names.len(), alloc);
        owned.extend_from_slice(names);
        let container = match impl_ {
            // A trait's group is its declared lifetimes (`ContainerRegions::of_header`).
            None => Some(ContainerRegions { names: owned, count: declared, self_regions: None, self_binders: 0, trait_regions: None, param_bounds: Vec::new_in(alloc) }),
            // An impl's whose regions were refused has none (its members' are refused by name).
            Some((None, _)) => None,
            Some((Some(r), self_ty)) => {
                let h = r.regions();
                let Some((self_col, trait_col)) = h.impl_header.as_ref() else {
                    return Err("an impl's filed regions with no header columns");
                };
                let count = h.params.count();
                if count < declared {
                    return Err("an impl whose filed regions number fewer lifetimes than it declares (D148)");
                }
                // Counted by the counts the record's own columns were laid by (the self column
                // is over the self type), never by the heads this store has made so far: a
                // restored impl's self type may name an ADT or trait not made in this run.
                if self_col.ty() != self_ty {
                    return Err("an impl's filed self column is not over its head's self type (D148)");
                }
                let mut binders = 0u32;
                tc::regions::each_slot_node(self.tys.view(), self_ty, r.arity(), alloc, &mut |_, node, _| {
                    binders += u32::from(super::types::is_binder(node));
                    Ok(())
                })
                .map_err(|_| "an impl's self type whose region slots its own record does not count (D148)")?;
                let mut self_regions: Vec<Region, Alloc<'static>> = Vec::with_capacity_in(self_col.as_slice().len(), alloc);
                self_regions.extend_from_slice(self_col.as_slice());
                let trait_regions = trait_col.as_ref().map(|t| {
                    let mut v: Vec<Region, Alloc<'static>> = Vec::with_capacity_in(t.as_slice().len(), alloc);
                    v.extend_from_slice(t.as_slice());
                    v
                });
                let param_bounds = extract_container_param_bounds(self.tys.view(), &h.clauses, alloc);
                Some(ContainerRegions { names: owned, count, self_regions: Some(self_regions), self_binders: binders, trait_regions, param_bounds })
            }
        };
        // The members' scope: declared names, then the whole group (as `impl_` files it).
        let group = container.as_ref().map_or(declared, |c| c.count);
        let scope = Scope::container(names, group, alloc).map_err(|_| "a container's lifetime names past its group")?;
        match container {
            Some(c) => {
                self.container_regions.insert(ck, c);
            }
            None => {
                self.container_regions.remove(&ck);
            }
        }
        self.container_scopes.insert(ck, scope);
        Ok(())
    }

    /// [`Store::file_container`] for the impl `impl_`, whose head is filed, from its header's
    /// text: its declared lifetimes' names (`lifetime_names`, as `impl_` reads them) beside its
    /// filed regions and self type. For an impl restored from a record that holds no lifetime
    /// part (made only when asked), before a member fn of it is made.
    fn impl_scope_from_text<S: NamesSource<'n>>(&self, names: S, impl_: LibKey) -> Result<(), HeadError> {
        let ck = checker_of(impl_);
        let (want, at) = (Want::Impl(impl_), HeadAt::Impl(ck));
        let text = match self.locals.head_of(impl_) {
            Some(local) => local?.1,
            None => names.signature_text_of(impl_).ok_or(HeadError::NoText(impl_))?.text,
        };
        let low = lower(names, impl_, text)?;
        let v = low.view();
        let mut life_names: Vec<Name, Alloc<'static>> = Vec::new_in(self.tys.alloc());
        lifetime_names(&v, &mut life_names).map_err(|why| HeadError::Item(impl_, why))?;
        let self_ty = self.impls.get(&ck).ok_or(HeadError::Filed { want, at, what: "an impl's members' scope asked before its head is filed" })?.self_ty;
        // Filed regions that do not fit the head are the regions' refusal, never the head's
        // (D148): they are refused by name and the scope is made over none (its members'
        // regions are then refused by name, `RegionRefusal::NoContainer`). Only a names-side
        // error (more names than the group) is the head's.
        if let Some(r) = self.regions.made(ck)
            && let Err(what) = self.file_container(ck, &life_names, Some((Some(r), self_ty)))
        {
            self.regions.file_unlaid(ck, super::head_record::Unlaid::Disagree, what);
        }
        if self.container_scopes.contains_key(&ck) {
            return Ok(());
        }
        self.file_container(ck, &life_names, Some((None, self_ty))).map_err(|what| HeadError::Filed { want, at, what })
    }

    /// A trait alias's head (`trait A<T> = B<T> + C where ..;`): its generics (`Self` first)
    /// and the clauses it stands for, read in its own module as a trait's supertraits and where
    /// clauses are, with `Self` = `Param(0)`. An alias named in them is made first
    /// ([`Why::Need`]), so its clauses arrive expanded; aliases naming each other in a cycle
    /// are [`Store::ensure`]'s `HeadError::Cycle`.
    fn trait_alias<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let own = own_generics(&b, &mut self.tys, module, v, 1, &mut ps, &mut cs)?;
        let me = intern(&mut self.tys, PNode::Param(0))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Ty(me, None), self_assoc: &[], anon: &[], consts: &cs, body: None };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 1, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let mut preds = Preds::new_in(Scope::item(v, None, 0, self.tys.alloc()).map_err(Why::Regions)?);
        // `sized` is not given: an alias adds no implied `Sized` on its own parameters (a use
        // stands for what is written).
        b.predicates(&mut self.tys, &env, v, 1, Some(me), None, &mut preds)?;
        if !preds.conds.is_empty() {
            return Err(Why::Unsupported("`where const` in a trait alias", whole));
        }
        let mut traits = Vec::new_in(Global);
        for c in preds.clauses.iter() {
            let PClause::Trait { trait_, args } = *c else { continue };
            let first = self.tys.view().list(args).map_err(|_| Why::Unsupported("a clause list outside the store's table", whole))?.first().copied();
            if first == Some(me) {
                let lib = keys.library_in(names, trait_).ok_or(Why::Program(format!("the trait {trait_:?} has no library key").into_boxed_str(), whole))?;
                traits.push((lib, args));
            }
        }
        self.maps.trait_aliases.insert(key, TraitAliasHead { generics: own + 1, clauses: preds.clauses, traits });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A free type alias's head: its generics and the aliased type, read in the alias's own
    /// module with its own parameters (a bound on an alias's parameter binds nothing).
    fn alias<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let generics = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        // A free `type T;` with no type is an extern type (the Reference, "Type aliases": a
        // free alias without a type is an error, so the only accepted form is a foreign item:
        // feature `extern_types`, core's `unsafe extern "C" { pub type Code; }`, the pointee of
        // `FnPtr::as_ptr`'s `NonNull<Code>`). It is its own nominal type with no size, no
        // fields and no generics (rustc refuses a generic one, E0044): `PNode::Foreign` of its
        // own key, which a path naming it reads as an alias of no parameters stands for.
        // GAP (named): the outline does not say whether the item is in an `extern` block, so
        // a typeless free alias outside one (which rustc refuses) is read the same way.
        let Some(t) = v.head_opt(head::TY) else {
            if generics != 0 {
                return Err(Why::Unsupported("an extern type with generic parameters (rustc: E0044)", Range::new(0, 0)));
            }
            let foreign = intern(&mut self.tys, PNode::Foreign(checker_of(key)))?;
            // No parameters: no defaults row (read as none) and no region slots.
            self.maps.alias_slots.remove(&key);
            self.maps.aliases.insert(key, Some((0, foreign)));
            self.maps.const_params.insert(key, cs);
            return Ok(());
        };
        // A bound on an alias's parameter is not checked where the alias is used, but it is
        // what `T::X` in the aliased type is read through (core's `ChangeOutputType`).
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs, body: None };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        // Its parameters' defaults (`type Result<T, E = Error>`): a path leaving them out takes
        // them, as a struct's (types.rs `with_defaults`).
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 0, &mut defaults)?;
        // Each `impl Trait` the alias's type writes is one opaque of the alias (super::tait).
        let alias_ck = checker_of(key);
        let a = self.tys.alloc();
        let opaques = super::tait::alias_opaques(&mut self.tys, v, lf::TypeId(t), alias_ck, generics, &cs, a)?;
        let env = Env { anon: &opaques, ..env };
        let ty = b.ty(&mut self.tys, &env, v, lf::TypeId(t))?;
        // Its type's regions over its own lifetimes (D148), read where a path names it.
        match alias_slots(&b, &mut self.tys, &env, v, lf::TypeId(t), a)? {
            Some(laid) => {
                self.maps.alias_slots.insert(key, laid);
            }
            None => {
                self.maps.alias_slots.remove(&key);
            }
        }
        let scope = Scope::item(v, None, 0, a).map_err(Why::Regions)?;
        for o in super::tait::alias_opaque_preds(&b, &mut self.tys, &env, v, alias_ck, &opaques, self.sized, &scope, a)? {
            self.preds.insert(o.key, o.preds);
            self.opaques.insert(o.key, (alias_ck, o.index));
        }
        self.register_record_defaults(names, keys, key, &env, marks, v)?;
        self.maps.alias_defaults.insert(key, defaults);
        self.maps.aliases.insert(key, Some((generics, ty)));
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A struct's, enum's or union's head: its type parameter count and each one's default,
    /// lowered over the parameters before it (`struct Vec<T, A: Allocator = Global>`).
    fn adt_head<S: NamesSource<'n>>(&mut self, names: S, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let g = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs, body: None };
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 0, &mut defaults)?;
        // A fieldless enum is filed before its generics are: a path to it has a type only
        // once `generics` holds it, and `own_generics` then finds it here.
        if let Some(variants) = super::const_enum::fieldless(names, key, v, g)? {
            self.maps.fieldless_enums.insert(checker_of(key), variants);
        }
        let mut coherence = CoherenceMarks::default();
        for &attr in v.rows(v.list(head::ATTRS))? {
            coherence.attribute(v.text_at(v.attr(attr)?.path)?.trim());
        }
        let lifetimes = super::borrows::declared_lifetimes(v)?;
        self.adt_declarations.insert(checker_of(key), AdtDeclaration { lifetimes, marks: coherence });
        // L1b: heads_regions call point (an ADT head: `adt_regions`, its fields' columns and
        // clauses, filed under `checker_of(key)`).
        self.maps.generics.insert(key, (g, defaults));
        self.maps.const_params.insert(key, cs);
        // The declared count of a BODY-LOCAL ADT only (one the names world holds no item for):
        // every other ADT's is the names' (D155, types.rs `declared_lifetimes`, which asks
        // the same question to choose). Last, after every refusal above.
        if names.key_beginning(checker_of(key).0.to_be_bytes()).is_none() {
            self.maps.adt_lifetimes.insert(checker_of(key), lifetimes);
        }
        Ok(())
    }

    fn trait_head<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>, _text: &'n str) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let written = v.head[head::FLAGS];
        if written & sig::TRAIT_ALIAS != 0 {
            return Err(Why::Unsupported("a trait alias", whole));
        }
        let mut flags = 0;
        let lifetimes = super::borrows::declared_lifetimes(v)?;
        if lifetimes != 0 {
            flags |= tc::TraitFlags::LIFETIME_PARAMS;
        }
        // Its members' fns number the trait's lifetimes first (`regions::Numbering`'s
        // container group); `Self` is a type parameter and has no region position.
        self.container_regions.insert(checker_of(key), ContainerRegions::of_header(v, None, self.tys.alloc())?);
        // The same names as its members' clauses read them (D148; heads_regions.rs `Scope`).
        self.container_scopes.insert(checker_of(key), Scope::item(v, None, 0, self.tys.alloc()).map_err(Why::Regions)?);
        if written & sig::AUTO != 0 {
            flags |= tc::TraitFlags::AUTO | tc::TraitFlags::COINDUCTIVE;
        }
        if written & sig::UNSAFE != 0 {
            flags |= tc::TraitFlags::UNSAFE;
        }
        // The Reference, "Default trait object lifetimes": a trait with a `Self: 'static` bound
        // (such as `core::any::Any`) gives `dyn Trait` a `'static` default even behind `&`.
        for &b in v.rows(v.list(head::BOUNDS))? {
            if let lf::Bound::Lifetime(nm) = lf::Bound::decode(b)?
                && v.name_text(nm)? == "'static"
            {
                flags |= tc::TraitFlags::SELF_STATIC;
            }
        }
        for &w in v.rows(v.list(head::WHERE))? {
            let lf::WherePred::Type { ty, bounds, .. } = lf::WherePred::decode(w)? else { continue };
            let lf::TypeRef::Path(p) = v.ty(ty)? else { continue };
            let (abs, texts, _, _) = super::types::path_texts(v, p)?;
            if abs || texts.as_slice() != ["Self"] {
                continue;
            }
            for &b in v.rows(bounds)? {
                if let lf::Bound::Lifetime(nm) = lf::Bound::decode(b)?
                    && v.name_text(nm)? == "'static"
                {
                    flags |= tc::TraitFlags::SELF_STATIC;
                }
            }
        }
        // `#[rustc_coinductive]` (core's `Sized`, `MetaSized`, `PointeeSized`): a cycle through
        // the trait's own steps holds (library/core/src/marker.rs: "`Sized` being coinductive,
        // despite having supertraits, is okay as there are no user-written impls").
        let mut coherence = CoherenceMarks::default();
        for &n in v.rows(v.list(head::ATTRS))? {
            let path = v.text_at(v.attr(n)?.path)?.trim();
            coherence.attribute(path);
            if path == "rustc_coinductive" {
                flags |= tc::TraitFlags::COINDUCTIVE;
            }
        }
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let own = own_generics(&b, &mut self.tys, module, v, 1, &mut ps, &mut cs)?;
        // The trait's bounds are lowered after its head (they may name the trait): a default
        // written with `T::X` is refused as not yet readable, never as not found.
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Trait(key), self_assoc: &[], anon: &[], consts: &cs, body: None };
        let mut defaults = Vec::new_in(Global);
        own_defaults(&b, &mut self.tys, &env, v, 1, &mut defaults)?;
        let ck = checker_of(key);
        let mut assoc = Vec::new_in(Global);
        let mut assoc_own: Vec<(Name, u32), Global> = Vec::new_in(Global);
        let mut assoc_lives: Vec<(Name, u32), Global> = Vec::new_in(Global);
        let mut assoc_where: Vec<(Name, super::gat::AssocWhere), Global> = Vec::new_in(Global);
        let mut fns = Vec::new_in(Global);
        let ms = members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        let mut members = Vec::new_in(Global);
        for m in &ms {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            if low.view().cfg_out() {
                continue;
            }
            let kind = match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    fns.push((Name::of(m.name), mk));
                    DynMember::Fn
                }
                ItemDetail::TypeAlias => {
                    assoc.push(Name::of(m.name));
                    // Its own type and const parameters, counted (a generic associated type:
                    // `type MapStorage<V>` has one; a lifetime is none): what arguments on
                    // its segment are checked against ([`Heads::assoc_generics`]). Filed with
                    // the head, as its name is: a path may name it before the members exist.
                    let mv = low.view();
                    let mut own_params = 0u32;
                    let mut own_lifetimes = 0u32;
                    for &row in mv.rows(mv.list(head::GENERICS))? {
                        if matches!(lf::GenericParam::decode(row)?, lf::GenericParam::Lifetime { .. }) {
                            own_lifetimes += 1;
                        } else {
                            own_params += 1;
                        }
                    }
                    // A GAT's own lifetimes: kept for a body-local trait only (below).
                    assoc_lives.push((Name::of(m.name), own_lifetimes));
                    assoc_own.push((Name::of(m.name), own_params));
                    // Its where clauses: an outlives clause is kept with the head, any
                    // other is refused by name (gat.rs).
                    let kept = super::gat::member_where(&mv, m.name)?;
                    if !kept.is_empty() {
                        assoc_where.push((Name::of(m.name), kept));
                    }
                    DynMember::Type
                }
                ItemDetail::Const { .. } => DynMember::Const,
                d => return Err(Why::Unsupported(crate::front::detail_name(d), whole)),
            };
            let shape = super::dyn_ty::member(&b.resolver, module, self.sized, &self.pointers, kind, &low.view())?;
            members.push(tc::dyn_compat::MemberFacts { name: Name::of(m.name), shape });
        }
        for (name, own_params) in assoc_own {
            self.maps.assoc_generics.insert((key, name), own_params);
        }
        for (name, kept) in assoc_where {
            self.maps.assoc_where.insert((key, name), kept);
        }
        // The declared counts of a BODY-LOCAL trait (one the names world holds no item for):
        // the region walk reads every other trait's from the names (D155, types.rs
        // `declared_lifetimes`, which asks the same question to choose). Filed last, after
        // every refusal above, so a refused head files none.
        if names.key_beginning(ck.0.to_be_bytes()).is_none() {
            self.maps.trait_lifetimes.insert(ck, lifetimes);
            for (name, n) in assoc_lives {
                self.maps.assoc_lifetimes.insert((ck, name), n);
            }
        }
        let all_self_defaults = all_self_defaults(&defaults, self.tys.view());
        self.maps.traits.insert(key, TraitHead { generics: own + 1, all_self_defaults, defaults, assoc, fns, flags: tc::TraitFlags(flags), members, marks: coherence });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// A trait's predicates (its supertraits and where clauses on its generics, `Self` =
    /// `Param(0)`) and its fns' signatures, each fn's clauses its trait's plus
    /// `Self: Trait<params>`.
    fn trait_members<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let ck = checker_of(key);
        let mut ps = Vec::new_in(Global);
        let own = param_names(v, 1, &mut ps, Consts::Keep)?;
        let cs = self.maps.const_params.get(&key).cloned().ok_or(Why::Need(key))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Trait(key), self_assoc: &[], anon: &[], consts: &cs, body: None };
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 1, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let me = intern(&mut self.tys, PNode::Param(0))?;
        let alloc = self.tys.alloc();
        // The trait's lifetimes by name (D148): what its clauses and its members' read.
        let scope = Scope::item(v, None, 0, alloc).map_err(Why::Regions)?;
        self.container_scopes.insert(ck, scope.clone_in(alloc));
        // Its members' container group, a fact of this header as the scope is: filed here too,
        // so a trait whose head was filed from a record with no lifetime part (`Head::Trait`'s
        // `life` `None` or refused: no `restore_container`) still numbers its members' regions
        // (D148), never `RegionRefusal::NoContainer` for a header that is here. One the record
        // or `trait_head` filed stands.
        self.container_regions.insert_absent(ck, ContainerRegions::of_header(v, None, alloc)?);
        let mut preds = Preds::new_in(scope.clone_in(alloc));
        // What elaborating `T: Trait` adds: its implied clauses (`Preds::predicates_implying`:
        // the header's bounds with their associated item constraints, `where Self: ..`).
        let mut supertraits = Vec::new_in(Global);
        b.predicates_implying(&mut self.tys, &env, v, 1, Some(me), self.sized, &mut preds, &mut |c| supertraits.push(c))?;
        let mut inherited = Vec::with_capacity_in(preds.clauses.len() + 1, Global);
        inherited.push(PClause::Trait { trait_: ck, args: params(&mut self.tys, 0, own + 1, &cs)? });
        inherited.extend_from_slice(&preds.clauses);
        // Before the members' signatures: `Self::Output` in `Fn::call` is a supertrait's. The
        // supertraits proper: the implied trait clauses on `Self` (an associated type's bound,
        // `<Self as Super>::A: B`, names no trait whose items `Self::` reaches).
        let mut supers = Vec::new_in(Global);
        for c in supertraits.iter() {
            if let PClause::Trait { trait_, args } = *c {
                if self.tys.view().list(args).map_err(|_| Why::Unsupported("a clause list outside the store's table", whole))?.first() != Some(&me) {
                    continue;
                }
                let lib = keys.library_in(names, trait_).ok_or(Why::Program(format!("the supertrait {trait_:?} has no library key").into_boxed_str(), whole))?;
                supers.push((lib, args));
            }
        }
        // A trait's `Self` is `MetaSized` unless its header states a sizedness (self_size.rs;
        // core's `Write::write_fmt` needs it for its block impl's `W: ?Sized` at `W = Self`): a
        // clause of every member's env, as `Self: Trait` is; not a supertrait (vtables).
        if let Some(meta) = super::self_size::implied_self_size(&self.maps, key, &supers, self.sized)? {
            let self_list = intern_list(&mut self.tys, &[me])?;
            inherited.push(PClause::Trait { trait_: self.maps.checker(meta), args: self_list });
        }
        // D148: the trait's clauses' columns (filed under its key with its clauses below), and
        // the columns its members inherit, in `inherited`'s order.
        let trait_regions = {
            let laid = lay_listed(&b, &mut self.tys, &env, v, (&preds, &[]), SelfGiven::NONE, Captures::NONE, alloc)?;
            let arity = HeadsArity { heads: &self.maps, opaques: &[], names };
            let cx = hreg::Cx { v, facts: &arity, written: &laid, scope: &scope, made: &[], alloc };
            let own = hreg::clause_regions(&cx, &mut self.tys, &preds, scope.params(), None);
            let members = match &own {
                Ok(h) => {
                    let own_clauses = u32::try_from(preds.clauses.len()).map_err(|_| RegionGap::Clauses(RegionsRefusal::Unaligned { nth: u32::MAX }));
                    own_clauses.and_then(|n| hreg::trait_inherited(&cx, &mut self.tys, inherited[0], h, n, &inherited[1 + preds.clauses.len()..]).map_err(RegionGap::Clauses))
                }
                Err(why) => Err(RegionGap::Clauses(*why)),
            };
            let view = self.tys.view();
            if let Ok(h) = &own {
                let param_bounds = extract_container_param_bounds(view, &h.clauses, alloc);
                if !param_bounds.is_empty()
                    && let Some(prev) = self.container_regions.get(&ck)
                {
                    let mut names = Vec::with_capacity_in(prev.names.len(), alloc);
                    names.extend_from_slice(&prev.names);
                    self.container_regions.insert(ck, ContainerRegions {
                        names,
                        count: prev.count,
                        self_regions: None,
                        self_binders: 0,
                        trait_regions: None,
                        param_bounds,
                    });
                }
            }
            let own = own.map_err(RegionGap::Clauses).and_then(|h| super::head_record::FiledRegions::new(h, &arity, view, alloc).map_err(RegionGap::Record));
            (own, members)
        };
        // A member reads the inherited columns or, with none filed, refuses its own by name
        // (heads_regions.rs `RegionsRefusal::Inherited`).
        match trait_regions.1 {
            Ok(h) => {
                self.trait_inherits.insert(ck, h);
            }
            Err(_) => {
                self.trait_inherits.remove(&ck);
            }
        }
        file_regions(self, ck, trait_regions.0);
        self.maps.supers.insert(key, supers);
        let mut assoc = Vec::new_in(Global);
        let mut consts: Vec<(Name, ItemKey), Global> = Vec::new_in(Global);
        // Each associated type's own const parameters (own position, type): filed after the
        // loop into `Maps::assoc_consts`.
        let mut assoc_consts: Vec<(Name, Vec<(u32, PTy), Global>), Global> = Vec::new_in(Global);
        let mut needs: Vec<LibKey, Global> = Vec::new_in(Global);
        for m in &members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))? {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let mv = low.view();
            if mv.cfg_out() {
                continue;
            }
            match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    // A need of one member's signature does not stop the others: every need
                    // of the trait's members is met before the trait is made again, so a trait
                    // of n members is made at most a few times, not once per need (Iterator's
                    // members: 62 restarts and 126 ms of 149 a body, front2_tables_cost.rs).
                    match self.member_fn_head(names, keys, key, mk, Some(ck), &env, own + 1, &inherited, &mv, m, marks) {
                        Ok(()) => {}
                        Err(Why::Need(k)) if self.needs == NeedsMet::All => needs.push(k),
                        Err(why) => return Err(why),
                    }
                }
                ItemDetail::TypeAlias => {
                    let item = keys.member(ck, MemberKind::Type, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
                    let default = match mv.head_opt(head::TY) {
                        Some(t) => Some(b.ty(&mut self.tys, &env, &mv, lf::TypeId(t))?),
                        None => None,
                    };
                    // Its item bounds (`type A: B;` and the implied `Sized`), filed as the
                    // predicates of its own key with `Self` the alias `<Self as Trait<params>>::A`
                    // ([`Builder::assoc_ty_bounds`]): what the checker gives the solver as the
                    // alias's bounds. As a member fn's signature ([`Store::member_fn_head`]): a
                    // need is passed up; any other refusal is filed in `fn_gaps` under the
                    // type's key and the trait's head goes on: a body whose solve reads the
                    // bounds then gets the gap, by name (`BodyWorld::predicates`).
                    // A generic associated type (`type Out<T>: B;`): its own type and const
                    // parameters follow the trait's, in the alias's list and in scope for
                    // its bounds (gat.rs; the checker's `AssocGenerics`).
                    let mut own_ps = Vec::new_in(Global);
                    own_ps.extend_from_slice(&ps);
                    let mut own_cs = Vec::new_in(Global);
                    own_cs.extend_from_slice(&cs);
                    let of_type = match own_generics(&b, &mut self.tys, module, &mv, own + 1, &mut own_ps, &mut own_cs) {
                        Ok(n) => n,
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(why) => {
                            self.preds.remove(&item);
                            self.fn_gaps.insert(item, why);
                            self.regions.file_refused(item);
                            assoc.push(tc::AssocTyDef { name: Name::of(m.name), item, default });
                            continue;
                        }
                    };
                    // Its own const parameters: those of `own_cs` past the trait's (indices
                    // `own + 1 ..`), at their position among its own parameters.
                    let mut mine = Vec::new_in(Global);
                    for &(i, t) in own_cs.iter().filter(|(i, _)| *i > own) {
                        mine.push((i - own - 1, t));
                    }
                    assoc_consts.push((Name::of(m.name), mine));
                    let own_env = Env { params: &own_ps, consts: &own_cs, ..env };
                    let alias_args = params(&mut self.tys, 0, own + 1 + of_type, &own_cs)?;
                    let alias = intern(&mut self.tys, PNode::Alias { trait_: ck, name: Name::of(m.name), args: alias_args })?;
                    // Its own lifetimes after the trait's (D148): its bounds' `'a` and its alias's
                    // identity column `Param(0..)` (`Alias`'s own slots: the trait's, then its own).
                    let assoc_scope = Scope::item(&mv, Some(&scope), scope.params(), alloc).map_err(Why::Regions)?;
                    let mut ap = Preds::new_in(assoc_scope.clone_in(alloc));
                    match b.assoc_ty_bounds(&mut self.tys, &own_env, &mv, alias, self.sized, &mut ap) {
                        Ok(()) => {
                            self.fn_gaps.remove(&item);
                            let filed = {
                                let laid = lay_listed(&b, &mut self.tys, &own_env, &mv, (&ap, &[]), SelfGiven::NONE, Captures::NONE, alloc)?;
                                let arity = HeadsArity { heads: &self.maps, opaques: &[], names };
                                let mut ident: blaze_own::Vec<Region, Alloc<'static>> = blaze_own::Vec::with_capacity_in(assoc_scope.params() as usize, alloc);
                                for i in 0..assoc_scope.params() {
                                    ident.push(Region::Param(i));
                                }
                                let made = [(alias, &ident[..])];
                                let cx = hreg::Cx { v: &mv, facts: &arity, written: &laid, scope: &assoc_scope, made: &made, alloc };
                                let head = hreg::clause_regions(&cx, &mut self.tys, &ap, assoc_scope.params(), None).map_err(RegionGap::Clauses);
                                let view = self.tys.view();
                                head.and_then(|h| super::head_record::FiledRegions::new(h, &arity, view, alloc).map_err(RegionGap::Record))
                            };
                            file_regions(self, item, filed);
                            self.preds.insert(item, ap);
                        }
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(why) => {
                            self.preds.remove(&item);
                            self.fn_gaps.insert(item, why);
                            self.regions.file_refused(item);
                        }
                    }
                    assoc.push(tc::AssocTyDef { name: Name::of(m.name), item, default });
                }
                // A trait's associated constant (the Reference, "Associated Items",
                // "Associated Constants": "An associated constant declaration declares a
                // signature for associated constant definitions. It is written as `const`,
                // then an identifier, then `:`, then a type, finished by a `;`"; "Associated
                // constants definitions define a constant associated with a type. It is
                // written the same as a constant item"): a row as an impl's constant has
                // ([`ImplHead::consts`]): no inputs, its type the output, generic over the
                // trait's parameters (`Self` = `Param(0)`), its clauses the trait's plus
                // `Self: Trait<params>`. `has_body` says whether a DEFAULT is written
                // (`const N: usize = 4;`), read from the text as the impl side reads it.
                ItemDetail::Const { .. } => {
                    let mk = keys.member(ck, MemberKind::Const, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    let at = Range::new(0, m.text.len() as u32);
                    let t = mv.head_opt(head::TY).ok_or(Why::Unsupported("an associated constant with no type", at))?;
                    consts.push((Name::of(m.name), mk));
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
                    // As a member fn's signature ([`Store::member_fn_head`]): a need is passed
                    // up; any other refusal of the constant's type is filed in `fn_gaps` and
                    // the trait's head goes on (core's `SizedTypeProperties` has six
                    // constants: one unread type must not hide `SIZE`); a body that names the
                    // constant then gets the gap, by name.
                    // An associated constant has no generics of its own (blaze-lower's grammar):
                    // the trait's lifetimes are all it names.
                    let mut cp = Preds::new_in(scope.clone_in(alloc));
                    cp.inherit(&inherited);
                    // Its type and regions from one walk ([`const_regions`]); its inherited
                    // clauses' columns are the trait's members' (`trait_inherits`).
                    let ra = self.tys.alloc();
                    let made = const_regions(&b, &mut self.tys, &env, &mv, lf::TypeId(t), (self.container_regions.get(&ck).ok_or(ck), &scope, self.regions.made(ck).map(super::head_record::FiledRegions::arity)), (&cp, self.trait_inherits.get(&ck)), ra);
                    let output = match made {
                        Ok((output, filed)) => {
                            file_regions(self, mk, filed);
                            output
                        }
                        Err(why @ (Why::Need(_) | Why::NeedConst(..))) => return Err(why),
                        Err(why) => {
                            self.fn_gaps.insert(mk, why);
                            self.regions.file_refused(mk);
                            continue;
                        }
                    };
                    self.fn_gaps.remove(&mk);
                    let mut own_bounds = Vec::with_capacity_in(bounds.len(), Global);
                    own_bounds.extend_from_slice(&bounds);
                    self.preds.insert(mk, cp);
                    let header = tc::FnPtrHeader::RUST;
                    let has_body = mv.head[head::FLAGS] & sig::HAS_BODY != 0;
                    let alloc = *ps.allocator();
                    let row = FnHead {
                        qualifiers: FnQualifiers::of(&mv)?,
                        lib: key, text: m.text, text_prefix: None, container: Some(ck), parent: own + 1, own: 0, declared: 0, self_param: false,
                        header, c_variadic: false, legacy_const_generics: Vec::new_in(alloc), inputs: Vec::new_in(alloc), output, has_body,
                        module, params: ps.to_vec_in(alloc), consts: cs.to_vec_in(alloc), bounds: own_bounds, self_: env.self_, self_assoc: Vec::new_in(alloc),
                    };
                    self.fns.insert(mk, row);
                }
                _ => {}
            }
        }
        // The members' needs: the first as a step's need, the rest made before it (`ensure_noting`).
        if let Some((&first, rest)) = needs.split_first() {
            self.more_needs.clear();
            self.more_needs.extend_from_slice(rest);
            return Err(Why::Need(first));
        }
        for (name, mine) in assoc_consts {
            let item = assoc.iter().find(|a| a.name == name).map(|a| a.item);
            let mut checker = Vec::with_capacity_in(cs.len() + mine.len(), self.tys.alloc());
            checker.extend_from_slice(&cs);
            checker.extend(mine.iter().map(|&(i, t)| (own + 1 + i, t)));
            self.maps.assoc_consts.insert((key, name), mine);
            if let Some(item) = item {
                self.gat_const_params.insert(item, checker);
            }
        }
        self.trait_defs.insert(ck, TraitExtra { supertraits, assoc, consts });
        self.preds.insert(ck, preds);
        Ok(())
    }

    #[expect(clippy::too_many_arguments, reason = "the item's parts, each read once by the caller")]
    fn adt<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>, detail: ItemDetail) -> Result<(), Why> {
        let ck = checker_of(key);
        adt_syntax(v)?;
        let mut ps = Vec::new_in(Global);
        let generics = param_names(v, 0, &mut ps, Consts::Keep)?;
        let cs = self.maps.const_params.get(&key).cloned().ok_or(Why::Need(key))?;
        let pl = params(&mut self.tys, 0, generics, &cs)?;
        let self_ty = intern(&mut self.tys, PNode::Adt(ck, pl))?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::Ty(self_ty, None), self_assoc: &[], anon: &[], consts: &cs, body: None };
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        let kind = match detail {
            ItemDetail::Enum => tc::AdtKind::Enum,
            ItemDetail::Union => tc::AdtKind::Union,
            _ => tc::AdtKind::Struct,
        };
        let ctor_of = |s: lf::Shape| match s {
            lf::Shape::Record => tc::CtorKind::Braces,
            lf::Shape::Tuple => tc::CtorKind::Tuple,
            lf::Shape::Unit => tc::CtorKind::Unit,
        };
        // Each variant: its key, name, ctor and field rows; a struct's one variant is itself.
        let mut shapes = Vec::new_in(Global);
        // Each variant's written discriminant (`A = 1`) and the variant's range.
        let mut written = Vec::new_in(Global);
        if kind == tc::AdtKind::Enum {
            names.crate_of(key).ok_or(Why::Need(key))?;
            let lib_vs = names.variants(key);
            for (i, &n) in v.rows(v.list(head::FIELDS))?.iter().enumerate() {
                let var = lf::Variant::decode(n)?;
                let r = v.site(v.row_range(v.list(head::FIELDS).start + i as u32)?)?;
                written.push((var.discriminant, r));
                let name = v.name_text(var.name)?;
                let lk = lib_vs.and_then(|vs| vs.iter().find(|x| x.name == name)).map(|x| x.key);
                let lk = lk.ok_or_else(|| Why::Program(format!("variant `{name}` is not in the names library").into_boxed_str(), r))?;
                shapes.push((checker_of(lk), Name::of(name), ctor_of(var.shape), var.fields));
            }
        } else {
            let ctor = match v.head[head::SHAPE] {
                0 => tc::CtorKind::Braces,
                1 => tc::CtorKind::Tuple,
                _ => tc::CtorKind::Unit,
            };
            let name = names.name_of(key).unwrap_or("");
            let vk = keys.member(ck, MemberKind::Ctor, "").map_err(|c| Why::Program(format!("{c}").into_boxed_str(), Range::new(0, 0)))?;
            shapes.push((vk, Name::of(name), ctor, v.list(head::FIELDS)));
        }
        let mut variants = Vec::new_in(Global);
        let mut fields = Vec::new_in(Global);
        // Each field's default (RFC 3681), one per field, and the text of each written one:
        // filed as an anonymous constant's head below, once the ADT's clauses are made.
        let mut defaults: Vec<Option<ItemKey>, Global> = Vec::new_in(Global);
        let mut default_heads: Vec<(ItemKey, &'n str, PTy), Global> = Vec::new_in(Global);
        // Each field where it is written and what it made: the fields' columns (D148).
        let alloc = self.tys.alloc();
        let mut field_at: blaze_own::Vec<hreg::FieldAt, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        for (vi, &(vk, name, ctor, rows)) in shapes.iter().enumerate() {
            let start = fields.len() as u32;
            for (j, &n) in v.rows(rows)?.iter().enumerate() {
                let f = lf::Field::decode(n)?;
                let ty = b.ty(&mut self.tys, &env, v, f.ty)?;
                field_at.push(hreg::FieldAt { variant: vi as u32, field: j as u32, written: f.ty, ty });
                let fname = match f.name {
                    Some(n) => Name(v.name(n)?),
                    None => Name::of_index(j as u32),
                };
                fields.push(tc::Field { name: fname, ty });
                let Some(c) = f.default else {
                    defaults.push(None);
                    continue;
                };
                let at = v.const_range(c)?;
                let r = v.site(at)?;
                field_default_allowed(names, key, kind, ctor, r)?;
                let ty_text = v.text_at(v.type_range(f.ty)?)?.trim();
                let value_text = v.text_at(at)?.trim();
                // Keyed by the ADT, the field's place and the default's text (rule 21: the
                // text is the value's input; the ADT's generics are its head's).
                let dk = keys
                    .member(ck, MemberKind::Anon, &format!("field default {vi}.{j}: {ty_text} = {value_text}"))
                    .map_err(|c| Why::Program(format!("{c}").into_boxed_str(), r))?;
                let text: &'n str = self.locals.keep_text(&format!("const _: {ty_text} = {value_text};"));
                defaults.push(Some(dk));
                default_heads.push((dk, text, ty));
            }
            variants.push(tc::Variant { key: vk, name, ctor, fields: List { start, len: fields.len() as u32 - start } });
            if ctor == tc::CtorKind::Tuple {
                let mut inputs = Vec::with_capacity_in(fields.len() - start as usize, Global);
                inputs.extend(fields[start as usize..].iter().map(|f| f.ty));
                self.ctors.insert(vk, CtorSig { adt: ck, generics, inputs, output: self_ty });
            }
        }
        let scope = Scope::item(v, None, 0, alloc).map_err(Why::Regions)?;
        let mut preds = Preds::new_in(scope.clone_in(alloc));
        b.predicates(&mut self.tys, &env, v, 0, None, self.sized, &mut preds)?;
        // D148: the ADT's regions (its own lifetimes, its clauses' columns, its fields'), filed
        // under its checker key ([`super::head_record::regions_key`]), plus a `HeadRegions::sig`
        // for each tuple struct / tuple variant constructor (`CtorSig`) so `FnDef` values like
        // `Ok`, `ControlFlow::Break`, `Asked::Run`, `OneOf` prove builtin `Fn*` bounds.
        let mut ctor_filed: blaze_own::Vec<(ItemKey, Result<super::head_record::FiledRegions, RegionGap>), Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        let filed = {
            let mut written: blaze_own::Vec<lf::TypeId, Alloc<'static>> = blaze_own::Vec::with_capacity_in(field_at.len(), alloc);
            written.extend(field_at.iter().map(|f| f.written));
            let laid = lay_listed(&b, &mut self.tys, &env, v, (&preds, &written), SelfGiven::NONE, Captures::NONE, alloc)?;
            let arity = HeadsArity { heads: &self.maps, opaques: &[], names };
            let cx = hreg::Cx { v, facts: &arity, written: &laid, scope: &scope, made: &[], alloc };
            let mut head = hreg::adt_regions(&cx, &mut self.tys, &preds, &field_at).map_err(RegionGap::Clauses);
            if let Ok(ref mut h) = head {
                for (vi, var) in variants.iter().enumerate() {
                    if var.ctor != tc::CtorKind::Tuple {
                        continue;
                    }
                    let fslice = fields.get(var.fields.range()).unwrap_or(&[]);
                    let mut io: blaze_own::Vec<PTy, Alloc<'static>> = blaze_own::Vec::with_capacity_in(fslice.len() + 1, alloc);
                    let mut sig_slots: blaze_own::Vec<Region, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
                    let mut ok = true;
                    for (fi, f) in fslice.iter().enumerate() {
                        io.push(f.ty);
                        if let Some((_, _, col)) = h.fields.iter().find(|&&(v_i, f_i, _)| v_i == vi as u32 && f_i == fi as u32) {
                            sig_slots.extend_from_slice(col.as_slice());
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    io.push(self_ty);
                    for li in 0..h.params.count() {
                        sig_slots.push(Region::Param(li));
                    }
                    if !ok {
                        continue;
                    }
                    if let Ok(io_list) = self.tys.intern_list(&io)
                        && let Ok(sig_ty) = self.tys.intern(PNode::FnPtr(io_list, tc::FnPtrHeader::RUST))
                    {
                        let arity2 = HeadsArity { heads: &self.maps, opaques: &[], names };
                        let view2 = self.tys.view();
                        if let Ok(sig_col) = RegionCol::lay(view2, sig_ty, &arity2, &sig_slots, alloc) {
                            if var.key == ck {
                                h.sig = Some(sig_col);
                            } else {
                                let mut cparams = tc::regions::LifetimeParams::new_in(h.params.count(), alloc);
                                for &(l, s) in h.params.outlives() {
                                    let _ = cparams.add_outlives(l, s);
                                }
                                let mut chr = HeadRegions::new_in(cparams, alloc);
                                chr.sig = Some(sig_col);
                                for c in h.clauses.iter() {
                                    if let Ok(ccol) = RegionCol::lay(view2, c.col.ty(), &arity2, c.col.as_slice(), alloc) {
                                        chr.clauses.push(tc::region_facts::ClauseRegions { nth: c.nth, binders: c.binders, col: ccol });
                                    }
                                }
                                chr.outlives.extend_from_slice(&h.outlives);
                                let cf = super::head_record::FiledRegions::new(chr, &arity2, view2, alloc).map_err(RegionGap::Record);
                                ctor_filed.push((var.key, cf));
                            }
                        }
                    }
                }
            }
            let view = self.tys.view();
            head.and_then(|h| super::head_record::FiledRegions::new(h, &arity, view, alloc).map_err(RegionGap::Record))
        };
        file_regions(self, ck, filed);
        for (vk, cf) in ctor_filed {
            file_regions(self, vk, cf);
        }
        // Each field default (RFC 3681): an anonymous constant of the field's type in a const
        // context, generic over the ADT's parameters with its clauses (it may name `N` or
        // `T`), evaluated at each use's ground arguments (the struct expression's type's).
        for &(dk, text, output) in &default_heads {
            let mut dp = Preds::new_in(scope.clone_in(alloc));
            dp.inherit(&preds.clauses);
            self.preds.insert(dk, dp);
            let mut own_bounds = Vec::with_capacity_in(bounds.len(), Global);
            own_bounds.extend_from_slice(&bounds);
            let row = FnHead {
                lib: key,
                text,
                qualifiers: FnQualifiers::CONST.note_lifetimes(text),
                text_prefix: None,
                container: None,
                parent: 0,
                own: generics,
                declared: 0,
                self_param: false,
                header: tc::FnPtrHeader::RUST,
                c_variadic: false,
                legacy_const_generics: Vec::new_in(Global),
                inputs: Vec::new_in(Global),
                output,
                has_body: true,
                module,
                params: ps.to_vec_in(*ps.allocator()),
                consts: cs.to_vec_in(*cs.allocator()),
                bounds: own_bounds,
                self_: SelfTy::Ty(self_ty, None),
                self_assoc: Vec::new_in(Global),
            };
            self.fns.insert(dk, row);
        }
        if defaults.iter().all(Option::is_none) {
            defaults.clear();
        }
        self.field_defaults.insert(ck, defaults);
        self.preds.insert(ck, preds);
        // Its `#[repr(..)]`, read by the reader the old front reads with, and its
        // `rustc_layout_scalar_valid_range_*` bounds (layout's `valid_range`).
        let mut repr = blaze_build::layout::ReprAttrs::RUST;
        let mut valid = blaze_build::layout::ValidRange::default();
        let mut args = false;
        let coherence = self.adt_declarations.get(&ck).ok_or(Why::Need(key))?.marks;
        let attrs = v.list(head::ATTRS);
        for (i, &n) in v.rows(attrs)?.iter().enumerate() {
            let a = v.attr(n)?;
            let path = v.text_at(a.path)?.trim();
            if path == "repr" {
                let r = v.site(v.row_range(attrs.start + i as u32)?)?;
                crate::front::repr_parts(v.text_at(a.args)?, &mut repr).map_err(|what| Why::Unsupported(what, r))?;
            }
            // `UnsafeCell` and `UnsafePinned` hide their field's niche (core/src/cell.rs: "the
            // niche of `T` is not visible", `UnsafeCell<NonZero<u8>>` has none) and are not
            // `Freeze`: what layout and a static's place read (layout.rs `interior_mut`).
            if path == "lang" && matches!(crate::front::unquote(v.text_at(a.args)?), "unsafe_cell" | "unsafe_pinned") {
                repr.hide_niche = true;
            }
            if path.starts_with("rustc_layout_scalar_valid_range") {
                let r = v.site(v.row_range(attrs.start + i as u32)?)?;
                valid_range_of(path, v.text_at(a.args)?, kind, &mut valid).map_err(|what| Why::Unsupported(what, r))?;
            }
            args |= is_args_attr(path);
        }
        let discrs = discriminants(&b, &mut self.tys, &env, v, repr, &written)?;
        self.register_record_defaults(names, keys, key, &env, marks, v)?;
        let (lifetimes, field_region_at, field_region_list) = super::borrows::freeze_field_regions(names, keys, self, ck, v, kind, &fields, self.tys.alloc())?;
        let field_regions = super::borrows::FieldRegions::new(field_region_at, field_region_list, self.tys.alloc());
        let auto_copy = is_auto_copy_pub_view(names, key, module, kind, v)?;
        self.auto_copy_pub.insert(ck, auto_copy);
        self.adts.insert(ck, AdtRow { kind, generics, variants, fields, repr, valid, discrs, args, marks: coherence, lifetimes, field_regions });
        let _ = self.adt_life(names, keys, key);
        Ok(())
    }

    /// The impl head of `ck` if complete, or its provisional header while constant evaluation
    /// is running (`maps.evaluating > 0`) and one of its associated types is waiting on a
    /// constant. Returns `(head, is_provisional)`.
    pub(super) fn impl_or_provisional(&self, ck: ItemKey) -> Option<(&ImplHead, bool)> {
        if let Some(h) = self.impls.get(&ck) {
            return Some((h, false));
        }
        if self.maps.evaluating.get() > 0 {
            if let Some(p) = self.provisional_impls.get(&ck) {
                return Some((&p.head, true));
            }
        }
        None
    }

    /// Records that the current check or projection read the provisional impl `ck`.
    pub(super) fn note_provisional_read(&self, ck: ItemKey) {
        let mut used = self.provisional_used.borrow_mut();
        if !used.iter().any(|&k| k == ck) {
            used.push(ck);
        }
    }

    /// Removes any provisional impl waiting on constant `need` now that `need` has been
    /// answered (or refused). Returns whether any entry was removed.
    pub(super) fn clear_provisional_for(&self, need: LibKey) -> bool {
        if self.provisional_impls.is_empty() {
            return false;
        }
        let mut drop_keys: blaze_own::Vec<ItemKey, Alloc<'static>> = blaze_own::Vec::new_in(self.tys.alloc());
        for (&ck, p) in self.provisional_impls.iter() {
            if p.waiting_const == need {
                drop_keys.push(ck);
            }
        }
        for &ck in drop_keys.iter() {
            self.provisional_impls.remove(&ck);
        }
        !drop_keys.is_empty()
    }

    /// Removes every provisional impl entry (called when constant evaluation returns to
    /// depth 0 so no provisional state can outlive an evaluation).
    pub(super) fn clear_all_provisional(&self) -> bool {
        if self.provisional_impls.is_empty() {
            return false;
        }
        let mut drop_keys: blaze_own::Vec<ItemKey, Alloc<'static>> = blaze_own::Vec::new_in(self.tys.alloc());
        for (&ck, _) in self.provisional_impls.iter() {
            drop_keys.push(ck);
        }
        for &ck in drop_keys.iter() {
            self.provisional_impls.remove(&ck);
        }
        !drop_keys.is_empty()
    }

    fn impl_<S: NamesSource<'n>>(&mut self, names: S, keys: &mut Keys, key: LibKey, module: LibKey, v: &View<'_>) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let mut polarity = if v.head[head::FLAGS] & sig::NEGATIVE != 0 { tc::ImplPolarity::Negative } else { tc::ImplPolarity::Positive };
        for &n in v.rows(v.list(head::ATTRS))? {
            if v.text_at(v.attr(n)?.path)?.trim() == "rustc_reservation_impl" {
                polarity = tc::ImplPolarity::Reservation;
            }
        }
        let ck = checker_of(key);
        self.provisional_impls.remove(&ck);
        let mut ps = Vec::new_in(Global);
        let mut cs = Vec::new_in(Global);
        let marks = self.marks(names, key, v.item())?;
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        let generics = own_generics(&b, &mut self.tys, module, v, 0, &mut ps, &mut cs)?;
        let env = Env { module, params: &ps, bounds: Bounds::Building(&[]), self_: SelfTy::None, self_assoc: &[], anon: &[], consts: &cs, body: None };
        // The self type FIRST, then the bounds with `Self` known: the Reference, "Paths",
        // "`Self`": "In an implementation, it refers to the type being implemented", the
        // impl's where clauses included (ruint's `impl<.., T> UintTryTo<T> for Uint<BITS,
        // LIMBS> where T: for<'a> TryFrom<&'a Self>`). A `T::X` in the self type is read
        // through the bounds of `T` as written in this header (`Bounds::Building`:
        // [`super::written`], by name), never through the lowered list, which needs `Self`.
        // The self type itself naming `Self` has nothing to refer to: refused by that name.
        // Its region slots in the same walk (D148): the container group its members' fns
        // number first ([`ContainerRegions`]).
        let alloc = self.tys.alloc();
        let mut sink = RegionSink::new_in(alloc);
        let self_ty = b.ty_regions(&mut self.tys, &env, v, lf::TypeId(v.head[head::SELF_TY]), &mut sink).map_err(|why| match why {
            Why::Unsupported(super::types::SELF_HERE, r) => Why::Unsupported("an impl's self type naming `Self`", r),
            other => other,
        })?;
        // Filed with the impl's regions below, once its header's columns say its count.
        let container = impl_container(v, &sink, alloc)?;
        // Its lifetimes by name (D148): its declared ones; an elided one has no name.
        let scope = Scope::item(v, None, 0, alloc).map_err(Why::Regions)?;
        let env = Env { self_: SelfTy::Ty(self_ty, None), ..env };
        // The trait reference BEFORE the where clauses: a clause's `Self::X` is
        // `<SelfTy as Trait<..>>::X` through it (the Reference, "Paths", "`Self`": "In an
        // implementation, it refers to the type being implemented", its where clauses
        // included; types.rs `SelfTy::Ty(_, Some(_))`): `impl<'de, 'b, T> Held<'de> for H<'b,
        // T> where T: From<Self::Value>`. Lowered after them, the clauses saw `SelfTy::Ty(_,
        // None)`, an inherent impl's `Self`, and refused the name. A `T::X` in the trait's
        // arguments is read through the bounds as WRITTEN (`Bounds::Building`, types.rs
        // `param_assoc` and [`super::written`]), as the self type's is.
        let (trait_, trait_args, written) = match v.head_opt(head::TRAIT) {
            Some(t) => {
                // `trait_ref` gives a clause's arguments, `Self` first; the checker's
                // `ImplHeader::trait_args` are those AFTER `Self` (the self type is `self_ty`).
                // With `Self` left in, no goal unified with the header (`Result<Out, Missing>:
                // Try` failed against `impl<T, E> Try for Result<T, E>`).
                let (tk, args) = b.trait_ref(&mut self.tys, &env, v, lf::TypeId(t), self_ty)?;
                let rest = tc::ArgsAfterSelf::split(self.tys.view(), args, self_ty).map_err(|e| {
                    Why::Program(format!("the impl's trait reference does not start with its self type {self_ty:?}: {e:?}").into_boxed_str(), whole)
                })?;
                (Some(checker_of(tk)), rest, Some((tk, args)))
            }
            None => (None, tc::ArgsAfterSelf::NONE, None),
        };
        // The where clauses' and the members' `Self::X` read the impl's trait (types.rs
        // `SelfTy::Ty`); an inherent impl's (`written` `None`) names none (types.rs: "names no
        // associated type of the impl").
        let env = Env { self_: SelfTy::Ty(self_ty, written), ..env };
        let mut bounds = Vec::new_in(Global);
        b.param_bounds(&mut self.tys, &env, v, 0, &mut bounds)?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        // D148: the impl's header columns FIRST (heads_regions.rs `impl_header_regions`): its
        // trait column is what `Self::Name` of a trait with lifetimes reads (types.rs
        // `projection_slots`, `Through::SelfImpl`: `impl<'de> Visitor<'de> for V { type X =
        // Self::Value; }`, `.. where Self::Value: Clone`), so the `type X = ..;` values below
        // and the header's own where clauses are walked after it and given it, as its members
        // are. The header's own walk lays that column, so it is given `Self`'s regions alone.
        let self_written = lf::TypeId(v.head[head::SELF_TY]);
        let trait_path = v.head_opt(head::TRAIT).map(lf::TypeId);
        let self_given = match &container {
            Ok(c) => c.self_regions.as_deref(),
            Err(_) => None,
        };
        let header_head = {
            let mut header: blaze_own::Vec<lf::TypeId, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
            // A header the regions' reader refuses refuses the regions, not the impl.
            let header_read = hreg::header_types(v, self_written, trait_path, &mut header);
            let laid = lay_written(&b, &mut self.tys, &env, v, &header, SelfGiven::from(self_given), Captures::NONE, alloc)?;
            let arity = HeadsArity { heads: &self.maps, opaques: &[], names };
            let cx = hreg::Cx { v, facts: &arity, written: &laid, scope: &scope, made: &[], alloc };
            let trait_ref = match (trait_path, written) {
                (Some(path), Some((tk, args))) => Some(hreg::TraitRef { path, trait_: checker_of(tk), args }),
                _ => None,
            };
            header_read.and_then(|()| hreg::impl_header_regions(&cx, &mut self.tys, hreg::ImplAt { self_written, self_ty, trait_ref }))
        };
        // The trait column the walks after the header are given: the header's, when its self
        // column is the container's (when not, the regions are refused below, `Disagree`, and
        // nothing is given: a `Self::Name` needing it is refused by name, `ImplTraitLifetimes`).
        let impl_trait: Option<Vec<Region, Alloc<'static>>> = match &header_head {
            Ok(h) => match h.impl_header.as_ref() {
                Some((s, Some(t))) if self_given == Some(s.as_slice()) => {
                    let mut col: Vec<Region, Alloc<'static>> = Vec::with_capacity_in(t.as_slice().len(), alloc);
                    col.extend_from_slice(t.as_slice());
                    Some(col)
                }
                _ => None,
            },
            Err(_) => None,
        };
        let given = SelfGiven { self_: self_given, impl_trait: impl_trait.as_deref(), param_bounds: &[] };
        let ms = members(names, key).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        let mut assoc = Vec::new_in(Global);
        let mut fns = Vec::new_in(Global);
        // Each fn's `self` from its text ([`member_fn::takes_self`]): its signature is its own
        // want ([`Want::MemberFn`]), made when it is used, never with this head (brief 14: a
        // signature naming `Self::BYTES` of a sibling impl made every lookup of a name in the
        // type need that constant).
        let mut fn_self = Vec::new_in(Global);
        // Each fn's visibility from its text too ([`MemberVis`]): a trait impl's members
        // write none and are the trait's (`Public` here).
        let mut fn_vis = Vec::new_in(Global);
        // An inherent impl's unstable members ([`Unstable`]), under the impl's own stability.
        let impl_marked = if trait_.is_none() { Marked::of(v)? } else { Marked::Unmarked };
        let mut unstable = Vec::new_in(Global);
        let mut const_texts = Vec::new_in(Global);
        // Each `type X = U;` value's column (D148, `HeadRegions::impl_assoc`), or why not: filed
        // with the impl's regions below, a refusal refusing them.
        let mut assoc_cols: blaze_own::Vec<Result<(Name, RegionCol<Alloc<'static>>), RegionsRefusal>, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        // For a trait impl, `Self::X` in where-clauses and members lowers to `<SelfTy as Trait>::X`
        // (types.rs `self_assoc_path`), never reading `env.self_assoc`. When an associated type's
        // value needs a constant (`Why::NeedConst`), we still lower the impl's header, where-clauses
        // and associated constants into `provisional_impls` before returning `NeedConst` so that
        // evaluating the constant can prove `T: Trait` without re-entering the pending associated type.
        let mut first_assoc_const: Option<(LibKey, Why)> = None;
        let mut pending_assoc: blaze_own::Vec<Name, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        for m in &ms {
            let low = lower(names, key, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            let mv = low.view();
            if mv.cfg_out() {
                continue;
            }
            if trait_.is_none()
                && matches!(m.detail, ItemDetail::Fn { .. } | ItemDetail::Const { .. })
                && let Some(u) = Marked::of(&mv)?.unstable_under(&impl_marked)
            {
                unstable.push((Name::of(m.name), u));
            }
            match m.detail {
                ItemDetail::Fn { .. } => {
                    let mk = keys.member(ck, MemberKind::Fn, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    fns.push((Name::of(m.name), mk));
                    fn_self.push(member_fn::takes_self(&mv)?);
                    fn_vis.push(if trait_.is_some() { MemberVis::Public } else { MemberVis::of_in(&mv, mv.vis())? });
                }
                ItemDetail::TypeAlias => {
                    let t = mv.head_opt(head::TY).ok_or(Why::Unsupported("an associated type with no type", whole))?;
                    // The member's text is read with its own tokens' marks (item_site.rs).
                    let marks = super::item_site::member_marks(names, key, m.written_by, m.text).map_err(MarksLost::why)?;
                    let mb = Builder { resolver: Paths { names }, heads: &self.maps, marks };
                    // `type Out<X> = ..;` (a generic associated type's value): its own type
                    // and const parameters are in scope after the impl's, at the indices
                    // `generics..` (the checker's `ImplAssoc`: "over the impl's generics then
                    // the item's own"; a lifetime is no parameter of a portable type). Its
                    // where clauses are read and refused or kept by gat.rs.
                    // D148: the value's region slots, by a second walk of the written type in
                    // the member's own view (`lay_written`: a walk the regions cannot take
                    // refuses the regions, never the impl), `Self` the impl's self type's and
                    // `Self::Name`'s trait lifetimes the header's trait column (`given`, laid
                    // above); the header's head files these values' columns below
                    // (`file_impl_assoc`).
                    let written_t = lf::TypeId(t);
                    // A GAT's own lifetimes are not in the impl's numbering (`RegionFacts::
                    // impl_assoc`: `Param(i)` the impl's): no column is laid for such a value,
                    // and a reader asking for it is refused by name (`NoHead`), never given one.
                    let mut own_lifetimes = false;
                    for &row in mv.rows(mv.list(head::GENERICS))? {
                        own_lifetimes |= matches!(lf::GenericParam::decode(row)?, lf::GenericParam::Lifetime { .. });
                    }
                    let lowered_assoc = if mv.rows(mv.list(head::GENERICS))?.is_empty() {
                        mb.ty(&mut self.tys, &env, &mv, written_t).and_then(|value| {
                            lay_written(&mb, &mut self.tys, &env, &mv, &[written_t], given, Captures::NONE, alloc).map(|laid| (value, laid))
                        })
                    } else {
                        let mut own_ps = Vec::new_in(Global);
                        own_ps.extend_from_slice(&ps);
                        let mut own_cs = Vec::new_in(Global);
                        own_cs.extend_from_slice(&cs);
                        own_generics(&mb, &mut self.tys, module, &mv, generics, &mut own_ps, &mut own_cs).and_then(|_| {
                            let own_env = Env { params: &own_ps, consts: &own_cs, ..env };
                            let value = mb.ty(&mut self.tys, &own_env, &mv, written_t)?;
                            lay_written(&mb, &mut self.tys, &own_env, &mv, &[written_t], given, Captures::NONE, alloc).map(|laid| (value, laid))
                        })
                    };
                    let (value, laid) = match lowered_assoc {
                        Ok(pair) => pair,
                        Err(why @ Why::NeedConst(need, _)) if trait_.is_some() => {
                            if first_assoc_const.is_none() {
                                first_assoc_const = Some((need, why));
                            }
                            pending_assoc.push(Name::of(m.name));
                            continue;
                        }
                        Err(why) => return Err(why),
                    };
                    super::gat::member_where(&mv, m.name)?;
                    let arity = HeadsArity { heads: &self.maps, opaques: &[], names };
                    if !own_lifetimes {
                        let cx = hreg::Cx { v: &mv, facts: &arity, written: &laid, scope: &scope, made: &[], alloc };
                        assoc_cols.push(hreg::impl_assoc(&cx, self.tys.view(), hreg::AssocAt { name: Name::of(m.name), written: written_t, ty: value }));
                    } else if let Ok(gat_scope) = scope.with_gat_lifetimes(&mv, alloc) {
                        let cx = hreg::Cx { v: &mv, facts: &arity, written: &laid, scope: &gat_scope, made: &[], alloc };
                        if let Ok(col) = hreg::impl_assoc(&cx, self.tys.view(), hreg::AssocAt { name: Name::of(m.name), written: written_t, ty: value }) {
                            assoc_cols.push(Ok(col));
                        }
                    }
                    assoc.push((Name::of(m.name), value));
                }
                ItemDetail::Const { .. } => {
                    let mk = keys.member(ck, MemberKind::Const, m.name).map_err(|c| Why::Program(format!("{c}").into_boxed_str(), whole))?;
                    const_texts.push((Name::of(m.name), mk, m.text, m.written_by));
                }
                d => return Err(Why::Unsupported(crate::front::detail_name(d), whole)),
            }
        }
        let env = Env { self_assoc: &assoc, ..env };
        let mut preds = Preds::new_in(scope.clone_in(alloc));
        b.predicates(&mut self.tys, &env, v, 0, None, self.sized, &mut preds)?;
        let mut consts = Vec::with_capacity_in(const_texts.len(), Global);
        let mut life_names = Vec::new_in(alloc);
        lifetime_names(v, &mut life_names)?;
        if let Some((waiting_const, need_const)) = first_assoc_const {
            self.container_scopes.insert(ck, Scope::container(&life_names, scope.params(), alloc).map_err(Why::Regions)?);
            for &(name, mk, text, written_by) in &const_texts {
                if self.impl_const(names, key, ck, mk, text, written_by, &env, (&preds.scope, preds.clauses.as_slice()), &bounds, generics).is_ok() {
                    self.fn_gaps.remove(&mk);
                    consts.push((name, mk));
                }
            }
            self.preds.insert(ck, preds);
            self.provisional_impls.insert(
                ck,
                ProvisionalImpl {
                    head: ImplHead { generics, self_ty, trait_, trait_args, assoc, fns, fn_self, fn_vis, unstable, consts, polarity },
                    pending_assoc,
                    waiting_const,
                },
            );
            return Err(need_const);
        }
        // D148: the impl's regions (its header's columns, laid above; its clauses'; its values'),
        // filed under its key, and its members' container group. L1a's group
        // (`impl_container`, from the self type's walk) and the header's (heads_regions.rs
        // `impl_header_regions`) meet here: the self type's column must be one, and the count
        // is the header's (it also counts the lifetimes the trait reference elides, which the
        // self type's walk does not see).
        let filed = {
            // The header's own where clauses, walked after its columns and given them, as the
            // values were (`given`).
            let laid = lay_listed(&b, &mut self.tys, &env, v, (&preds, &[]), given, Captures::NONE, alloc)?;
            let arity = HeadsArity { heads: &self.maps, opaques: &[], names };
            let cx = hreg::Cx { v, facts: &arity, written: &laid, scope: &scope, made: &[], alloc };
            let head = header_head.and_then(|mut h| {
                hreg::fill_clauses(&cx, &mut self.tys, &preds, None, &mut h)?;
                for col in assoc_cols {
                    let (name, col) = col?;
                    hreg::file_impl_assoc(&mut h, name, col)?;
                }
                Ok(h)
            });
            match (container, head) {
                (Ok(mut c), Ok(h)) => {
                    let self_col = h.impl_header.as_ref().map(|(s, _)| s.as_slice());
                    if c.self_regions.as_deref() != self_col {
                        self.container_regions.remove(&ck);
                        Err(RegionGap::Clauses(RegionsRefusal::Disagree { what: "an impl's self type column" }))
                    } else {
                        c.count = h.params.count();
                        // The trait column as filed (the record's reader restores the same,
                        // `file_container`): a member's `Self::Name` reads its trait's lifetimes.
                        // The self columns agree here, so it is the one the values and clauses
                        // were given (`impl_trait` above).
                        c.trait_regions = impl_trait;
                        c.param_bounds = extract_container_param_bounds(self.tys.view(), &h.clauses, alloc);
                        self.container_regions.insert(ck, c);
                        let view = self.tys.view();
                        super::head_record::FiledRegions::new(h, &arity, view, alloc).map_err(RegionGap::Record)
                    }
                }
                (Err(why), _) => {
                    self.container_regions.remove(&ck);
                    Err(RegionGap::Laid(why, SigPart::Header))
                }
                (Ok(_), Err(why)) => {
                    self.container_regions.remove(&ck);
                    Err(RegionGap::Clauses(why))
                }
            }
        };
        // The members' clauses name the impl's declared lifetimes; their own come after every
        // lifetime of the impl's group (its elided ones too), when that group is known.
        let group = self.container_regions.get(&ck).map_or(scope.params(), |c| c.count);
        self.container_scopes.insert(ck, Scope::container(&life_names, group, alloc).map_err(Why::Regions)?);
        file_regions(self, ck, filed);
        // Every constant's name and key, for the waits below (`ConstWait`).
        let mut declared_consts = Vec::with_capacity_in(const_texts.len(), Global);
        declared_consts.extend(const_texts.iter().map(|&(name, mk, ..)| (name, mk)));
        for &(name, mk, text, written_by) in &const_texts {
            match self.impl_const(names, key, ck, mk, text, written_by, &env, (&preds.scope, preds.clauses.as_slice()), &bounds, generics) {
                Ok(()) => {
                    self.fn_gaps.remove(&mk);
                }
                // A constant whose type names a sibling constant being evaluated now (`const
                // ALL: [Self; Self::COUNT]` while `COUNT` is: the leaf's evaluation made this
                // header to find `COUNT`) waits for it: the header is made without its row,
                // which is made when the leaf is answered ([`Store::remake_waiting`]). Any
                // other refusal (a need first) is the header's, as before.
                Err(why) => match self.waits_on_running(self_ty, &declared_consts, &why) {
                    Some(need) => {
                        self.const_waits.insert(mk, ConstWait { impl_: key, name, need });
                        self.fn_gaps.insert(mk, why);
                    }
                    None => return Err(why),
                },
            }
            consts.push((name, mk));
        }
        // The frame its fns' signatures are made in, each by its own want (`Store::member_fn`).
        let mut frame_bounds = Vec::with_capacity_in(bounds.len(), Global);
        frame_bounds.extend_from_slice(&bounds);
        let self_ = env.self_;
        self.member_frames.insert(ck, member_fn::MemberFrame { module, generics, params: ps, consts: cs.to_vec_in(*cs.allocator()), bounds: frame_bounds, self_ });
        self.preds.insert(ck, preds);
        self.impls.insert(ck, ImplHead { generics, self_ty, trait_, trait_args, assoc, fns, fn_self, fn_vis, unstable, consts, polarity });
        self.maps.const_params.insert(key, cs);
        Ok(())
    }

    /// The row of impl `key`'s associated constant `mk` (checker key of the impl `ck`), whose
    /// text is `text`: its type and regions walked in the impl's frame `env` (its parameters,
    /// bounds `bounds`, `Self`, associated types), its clauses the impl's (`scope`, `clauses`),
    /// filed in `fns` as the checker reads a constant (no inputs, its type the output). Made
    /// with its impl's head ([`Store::impl_`]), or after it when it waited on a leaf
    /// ([`Store::remake_waiting`]): one maker, so both rows are the same.
    #[expect(clippy::too_many_arguments, reason = "one constant's text and its impl's frame")]
    fn impl_const<S: NamesSource<'n>>(&mut self, names: S, key: LibKey, ck: ItemKey, mk: ItemKey, text: &'n str, written_by: Option<LibKey>, env: &Env<'_>, (scope, clauses): (&Scope, &[PClause]), bounds: &[ParamBound], generics: u32) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let low = lower(names, key, text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        let cv = low.view();
        let at = Range::new(0, text.len() as u32);
        let t = cv.head_opt(head::TY).ok_or(Why::Unsupported("an associated constant with no type", at))?;
        let marks = super::item_site::member_marks(names, key, written_by, text).map_err(MarksLost::why)?;
        let cb = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        // An associated constant has no generics of its own: the impl's lifetimes.
        let mut cp = Preds::new_in(scope.clone_in(scope.alloc()));
        cp.inherit(clauses);
        // Its type and regions from one walk ([`const_regions`]), in the members' group filed
        // with the impl's head; its inherited clauses' columns are the impl's filed regions'.
        let member_scope = self.container_scopes.get(&ck).ok_or(Why::Regions(RegionsRefusal::Disagree { what: "an impl's constant whose impl's lifetimes are not filed" }))?;
        let ra = self.tys.alloc();
        let (output, filed) = const_regions(&cb, &mut self.tys, env, &cv, lf::TypeId(t), (self.container_regions.get(&ck).ok_or(ck), member_scope, self.regions.made(ck).map(super::head_record::FiledRegions::arity)), (&cp, self.regions.made(ck).map(super::head_record::FiledRegions::regions)), ra)?;
        file_regions(self, mk, filed);
        let mut own_bounds = Vec::with_capacity_in(bounds.len(), Global);
        own_bounds.extend_from_slice(bounds);
        let mut self_assoc = Vec::with_capacity_in(env.self_assoc.len(), Global);
        self_assoc.extend_from_slice(env.self_assoc);
        let mut params = Vec::with_capacity_in(env.params.len(), Global);
        params.extend_from_slice(env.params);
        let mut consts = Vec::with_capacity_in(env.consts.len(), Global);
        consts.extend_from_slice(env.consts);
        let header = tc::FnPtrHeader::RUST;
        self.preds.insert(mk, cp);
        // An impl's constant has its value written (`pub const MAX: Self = !0;` inside `impl
        // usize`): its body is lowered by `lower_member` like a member fn's, which is what the
        // constant evaluator reads when another constant names it (`usize::MAX >> 1`). Read
        // from the text, never assumed.
        let has_body = cv.head[head::FLAGS] & sig::HAS_BODY != 0;
        let row = FnHead {
            qualifiers: FnQualifiers::of(&cv)?,
            lib: key, text, text_prefix: None, container: Some(ck), parent: generics, own: 0, declared: 0, self_param: false,
            header, c_variadic: false, legacy_const_generics: Vec::new_in(Global), inputs: Vec::new_in(Global), output, has_body,
            module: env.module, params, consts, bounds: own_bounds, self_: env.self_, self_assoc,
        };
        self.fns.insert(mk, row);
        Ok(())
    }

    /// Makes the rows of the impl constants that waited on the leaf `need` ([`ConstWait`]),
    /// now that it is answered or refused (front2/const_inherent.rs `file`): each in its
    /// impl's kept frame, by the maker its head used ([`Store::impl_const`]). A row refused now
    /// is that constant's own gap (`fn_gaps`), by why, never its impl's; one that waits on
    /// another leaf being evaluated waits on that one.
    pub(super) fn remake_waiting<S: NamesSource<'n>>(&mut self, names: S, need: LibKey) {
        let mut due: Vec<(ItemKey, ConstWait), Global> = Vec::new_in(Global);
        due.extend(self.const_waits.iter().filter(|(_, w)| w.need == need).map(|(k, w)| (*k, *w)));
        for (mk, w) in due {
            self.const_waits.remove(&mk);
            match self.remake_const(names, w, mk) {
                Ok(()) => {
                    self.fn_gaps.remove(&mk);
                }
                Err(why) => {
                    let ck = checker_of(w.impl_);
                    let again = match self.impls.get(&ck) {
                        Some(h) => self.waits_on_running(h.self_ty, &h.consts, &why),
                        None => None,
                    };
                    if let Some(next) = again {
                        self.const_waits.insert(mk, ConstWait { need: next, ..w });
                    }
                    self.fn_gaps.insert(mk, why);
                    self.regions.file_refused(mk);
                }
            }
        }
    }

    /// One waiting constant's row ([`Store::remake_waiting`]), in the frame its impl's head
    /// kept, as [`Store::member_fn`] makes a fn's signature.
    fn remake_const<S: NamesSource<'n>>(&mut self, names: S, w: ConstWait, mk: ItemKey) -> Result<(), Why> {
        let whole = Range::new(0, 0);
        let ck = checker_of(w.impl_);
        let lost = |what: &str| Why::Program(format!("impl {ck:?}: its constant {mk:?} waited on a leaf, and {what}").into_boxed_str(), whole);
        let frame = self.member_frames.get(&ck).ok_or_else(|| lost("its members' frame is not kept"))?;
        let (module, generics, self_) = (frame.module, frame.generics, frame.self_);
        let mut params = Vec::with_capacity_in(frame.params.len(), Global);
        params.extend_from_slice(&frame.params);
        let mut consts = Vec::with_capacity_in(frame.consts.len(), Global);
        consts.extend_from_slice(&frame.consts);
        let mut bounds = Vec::with_capacity_in(frame.bounds.len(), Global);
        bounds.extend_from_slice(&frame.bounds);
        let head = self.impls.get(&ck).ok_or_else(|| lost("its impl's head is not made"))?;
        let mut self_assoc = Vec::with_capacity_in(head.assoc.len(), Global);
        self_assoc.extend_from_slice(&head.assoc);
        let clauses = self.preds.get(&ck).ok_or_else(|| lost("its impl's clauses are not filed"))?;
        let scope = clauses.scope.clone_in(clauses.scope.alloc());
        let mut inherited: Vec<PClause, Global> = Vec::with_capacity_in(clauses.clauses.len(), Global);
        inherited.extend_from_slice(&clauses.clauses);
        let env = Env { module, params: &params, bounds: Bounds::Done(&bounds), self_, self_assoc: &self_assoc, anon: &[], consts: &consts, body: None };
        let ms = members(names, w.impl_).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
        for m in &ms {
            if !matches!(m.detail, ItemDetail::Const { .. }) || Name::of(m.name) != w.name {
                continue;
            }
            let low = lower(names, w.impl_, m.text).map_err(|e| Why::Program(format!("{e}").into_boxed_str(), whole))?;
            if low.view().cfg_out() {
                continue;
            }
            return self.impl_const(names, w.impl_, ck, mk, m.text, m.written_by, &env, (&scope, inherited.as_slice()), &bounds, generics);
        }
        Err(lost("its impl holds no constant of its name that its crate's cfg keeps"))
    }

    /// The leaf a constant's refused row waits on: `why` is a constant path's refusal and a
    /// leaf of the impl's own self type (`self_ty`, a ground ADT) whose name the impl declares
    /// (`consts`) is being evaluated now (front2/const_inherent.rs `running_sibling`). `None`:
    /// the refusal is the row's own.
    fn waits_on_running(&self, self_ty: PTy, consts: &[(Name, ItemKey)], why: &Why) -> Option<LibKey> {
        if !matches!(why, Why::ConstPath(..)) {
            return None;
        }
        let adt = match self.tys.view().get(self_ty).copied()? {
            PNode::Adt(k, args) if args.len == 0 => k,
            _ => return None,
        };
        super::const_inherent::running_sibling(&self.maps.inherents, adt, &|n| consts.iter().any(|&(c, _)| c == n))
    }

    /// A member fn's head: as [`Store::fn_head`], but a refusal other than a need is filed in
    /// `fn_gaps` and the container's head goes on (a need is passed up: the store makes the
    /// head it names and makes the container again).
    #[expect(clippy::too_many_arguments, reason = "the fn's parts and its container's")]
    fn member_fn_head<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey, key: ItemKey, container: Option<ItemKey>, outer: &Env<'_>, parent: u32, inherited: &[PClause], v: &View<'_>, member: &Member<'n>, marks: Option<TextMarks<'n>>) -> Result<(), Why> {
        names.read_member_signature(member.lib, member.written_by);
        match self.fn_head(names, keys, lib, key, container, outer, parent, inherited, v, member.text, member.detail, marks) {
            Err(why @ (Why::Need(_) | Why::NeedConst(..))) => Err(why),
            Err(why) => {
                self.fn_gaps.insert(key, why);
                self.regions.file_refused(key);
                Ok(())
            }
            Ok(()) => {
                self.fn_gaps.remove(&key);
                Ok(())
            }
        }
    }

    /// Fast check for whether `v` contains any structural record type (`TypeRef::Record`),
    /// avoiding allocating `Env` and entering [`Store::register_record_defaults`] on normal code.
    pub(super) fn view_has_record_type(v: &View<'_>) -> bool {
        for i in 0..v.types.len() {
            let Ok(ti) = u32::try_from(i) else { break };
            if matches!(v.ty(lf::TypeId(ti)), Ok(lf::TypeRef::Record(_))) {
                return true;
            }
        }
        false
    }

    /// Registers anonymous constant heads for any `TypeRef::Record` field defaults written in `v`
    /// (`{ width: u32 = 80, name: &str }`) or inherited via structural record spread
    /// (`{ ..Base, hp: u32 = 100 }`), keyed under `RecordSchema::defaults_key()` in
    /// `self.field_defaults` so `Expr::Record { .. }` resolves them via `World::field_defaults`.
    #[cold]
    #[inline(never)]
    pub(super) fn register_record_defaults<S: NamesSource<'n>>(
        &mut self,
        names: S,
        keys: &Keys,
        lib: LibKey,
        env: &Env<'_>,
        marks: Option<TextMarks<'n>>,
        v: &View<'_>,
    ) -> Result<(), Why> {
        #[derive(Clone, Copy)]
        enum DefaultEntry {
            None,
            Inherited(ItemKey),
            Written(lf::TypeId, lf::ConstIdx),
        }

        let alloc = self.tys.alloc();
        for i in 0..v.types.len() {
            let Ok(ti) = u32::try_from(i) else { break };
            let Ok(lf::TypeRef::Record(fields)) = v.ty(lf::TypeId(ti)) else { continue };
            let rows = v.rows(fields)?;
            let mut has_default = false;
            let mut has_spread = false;
            for &row in rows {
                let f = lf::Field::decode(row)?;
                if f.default.is_some() {
                    has_default = true;
                }
                if f.name.is_none() {
                    has_spread = true;
                }
            }
            if !has_default && !has_spread {
                continue;
            }
            let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
            let mut merged: blaze_own::Vec<(&str, Name, DefaultEntry), Alloc<'_>> = blaze_own::Vec::with_capacity_in(rows.len(), alloc);
            let mut inherited_any = false;
            let mut spread_ok = true;
            for &row in rows {
                let f = lf::Field::decode(row)?;
                match f.name {
                    Some(nm) => {
                        let text = v.name_text(nm)?;
                        let name = Name::of(text);
                        self.maps.note_record_field_name(name, text);
                        let entry = match f.default {
                            Some(c) => DefaultEntry::Written(f.ty, c),
                            None => DefaultEntry::None,
                        };
                        if let Some(slot) = merged.iter_mut().find(|(_, existing, _)| *existing == name) {
                            *slot = (text, name, entry);
                        } else {
                            merged.push((text, name, entry));
                        }
                    }
                    None => {
                        let base_ty = b.ty(&mut self.tys, env, v, f.ty)?;
                        let Some(PNode::Record(base_schema, _)) = self.tys.view().get(base_ty).copied() else {
                            spread_ok = false;
                            break;
                        };
                        let mut base_names: blaze_own::Vec<Name, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
                        if let Some(ns) = self.tys.view().record_names(base_schema) {
                            base_names.extend_from_slice(ns);
                        } else {
                            spread_ok = false;
                            break;
                        }
                        let base_defaults = self.field_defaults.get(&base_schema.defaults_key());
                        for (idx, &bnm) in base_names.iter().enumerate() {
                            let Some(btext) = self.maps.record_field_name(bnm) else {
                                spread_ok = false;
                                break;
                            };
                            let entry = match base_defaults.and_then(|ds| ds.get(idx).copied().flatten()) {
                                Some(dk) => {
                                    inherited_any = true;
                                    DefaultEntry::Inherited(dk)
                                }
                                None => DefaultEntry::None,
                            };
                            if let Some(slot) = merged.iter_mut().find(|(_, existing, _)| *existing == bnm) {
                                *slot = (btext, bnm, entry);
                            } else {
                                merged.push((btext, bnm, entry));
                            }
                        }
                        if !spread_ok {
                            break;
                        }
                    }
                }
            }
            if !spread_ok || (!has_default && !inherited_any) {
                continue;
            }
            merged.sort_by(|a, b| a.0.cmp(b.0));
            let mut sorted_names = blaze_own::Vec::with_capacity_in(merged.len(), alloc);
            for &(_, nm, _) in merged.iter() {
                sorted_names.push(nm);
            }
            let schema = tc::RecordSchema::of_sorted_names(&sorted_names);
            let rk = schema.defaults_key();
            let mut defaults = blaze_own::Vec::with_capacity_in(merged.len(), alloc);
            for (j, &(_, _, entry)) in merged.iter().enumerate() {
                let (fty, c) = match entry {
                    DefaultEntry::None => {
                        defaults.push(None);
                        continue;
                    }
                    DefaultEntry::Inherited(dk) => {
                        defaults.push(Some(dk));
                        continue;
                    }
                    DefaultEntry::Written(fty, c) => (fty, c),
                };
                let ty = b.ty(&mut self.tys, env, v, fty)?;
                let at = v.const_range(c)?;
                let r = v.site(at)?;
                let ty_text = v.text_at(v.type_range(fty)?)?.trim();
                let value_text = v.text_at(at)?.trim();
                let mut label = blaze_own::String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut label, format_args!("record default {j}: {ty_text} = {value_text}"));
                let dk = keys
                    .member(rk, MemberKind::Anon, &label)
                    .map_err(|e| {
                        let mut msg = blaze_own::String::new_in(alloc);
                        let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("{e}"));
                        Why::Program(msg.as_str().into(), r)
                    })?;
                let mut src = blaze_own::String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut src, format_args!("const _: {ty_text} = {value_text};"));
                let text: &'n str = self.locals.keep_text(&src);
                let scope = Scope::item(v, None, 0, alloc).map_err(Why::Regions)?;
                self.preds.insert(dk, Preds::new_in(scope));
                let row = FnHead {
                    lib,
                    text,
                    qualifiers: FnQualifiers::CONST.note_lifetimes(text),
                    text_prefix: None,
                    container: None,
                    parent: 0,
                    own: 0,
                    declared: 0,
                    self_param: false,
                    header: tc::FnPtrHeader::RUST,
                    c_variadic: false,
                    legacy_const_generics: Default::default(),
                    inputs: Default::default(),
                    output: ty,
                    has_body: true,
                    module: env.module,
                    params: Default::default(),
                    consts: Default::default(),
                    bounds: Default::default(),
                    self_: SelfTy::None,
                    self_assoc: Default::default(),
                };
                self.fns.insert(dk, row);
                defaults.push(Some(dk));
            }
            self.field_defaults.insert(rk, defaults.into_iter().collect());
        }
        Ok(())
    }

    /// A fn's signature and clauses: `env` is its container's (its params `0..parent`),
    /// `inherited` its container's clauses, `v` its own text's, `marks` its text's tokens'
    /// (front2 item_site.rs).
    #[expect(clippy::too_many_arguments, reason = "the fn's parts and its container's")]
    fn fn_head<S: NamesSource<'n>>(&mut self, names: S, keys: &Keys, lib: LibKey, key: ItemKey, container: Option<ItemKey>, outer: &Env<'_>, parent: u32, inherited: &[PClause], v: &View<'_>, text: &'n str, detail: ItemDetail, marks: Option<TextMarks<'n>>) -> Result<(), Why> {
        let mut ps = Vec::new_in(Global);
        ps.extend_from_slice(outer.params);
        let mut cs = Vec::new_in(Global);
        cs.extend_from_slice(outer.consts);
        let declared = {
            let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
            own_generics(&b, &mut self.tys, outer.module, v, parent, &mut ps, &mut cs)?
        };
        // Each `impl Trait` parameter type is one more own type parameter, after the declared
        // ones; it has no name, so it is in `anon`, never in `ps`.
        let mut anon_at = Vec::new_in(Global);
        anon_params(v, parent + declared, &mut anon_at)?;
        let own = declared + anon_at.len() as u32;
        let mut anon: Vec<(lf::TypeId, PTy), Global> = Vec::with_capacity_in(anon_at.len(), Global);
        for &(t, i) in &anon_at {
            anon.push((t, intern(&mut self.tys, PNode::Param(i))?));
        }
        let params_end = anon.len();
        // Each `impl Trait` in the return type is an opaque type (the Reference, "Impl trait",
        // "Abstract return types": the caller sees only its bounds), keyed by the fn's key and
        // its index ([`super::keys::opaque_key`], [`super::types::return_impls`]'s order; an
        // `async fn`'s `impl Future` is index 0 and the written ones follow). It captures every
        // generic parameter in scope, the fn's and its container's, `impl Trait` parameters
        // too ("Capturing", 2024 edition: "all in-scope generic parameters"; in earlier
        // editions all type and const parameters and only the named lifetimes: lifetimes are
        // erased here, so both editions give the same list): its args are their identity.
        let flags = v.head[head::FLAGS];
        let is_async = flags & sig::ASYNC != 0;
        let mut ident = Vec::with_capacity_in((parent + own) as usize, Global);
        for i in 0..parent + own {
            let node = if cs.iter().any(|&(c, _)| c == i) { PNode::ConstParam(i) } else { PNode::Param(i) };
            ident.push(intern(&mut self.tys, node)?);
        }
        let ident = intern_list(&mut self.tys, &ident)?;
        // The return type as the Reference reads it, after macro expansion: one that writes a
        // type macro anywhere (`-> fut!(u8)`, `-> Option<it!(u8)>`, alloy's `-> impl_future!(<Output
        // = T>)`) is lowered again from its own text with every macro expanded
        // ([`super::type_expand`]) and its opaques are the expansion's, numbered in the same
        // preorder; one with no macro is read in place. An `async fn`'s written return type is
        // walked the same way (`-> eyre::Result<impl Provider<N>>`: opaque 1, after the future's 0).
        let written_ret = v.head_opt(head::TY).map(lf::TypeId);
        let mut rets = Vec::new_in(Global);
        let has_macro = match written_ret {
            Some(t) => super::types::return_impls(v, t, &mut rets)?,
            None => false,
        };
        let expanded = match written_ret {
            Some(t) if has_macro => {
                rets.clear();
                let r = v.site(v.type_range(t)?)?;
                let ret_text = v.text_at(v.type_range(t)?)?;
                let x = super::type_expand::expand_type_text(names, outer.module, ret_text, marks, r, self.tys.alloc())?;
                x.impls(ret_text, r, &mut rets)?;
                Some(x)
            }
            _ => None,
        };
        // `rv` is the view the return type and its opaques' ids are in: the expansion's, or
        // the item's own. The return opaques are in their own list (`opaques`), never in
        // `anon` with the parameters' `impl`s: two views' type ids are not comparable.
        let (rv, ret) = match &expanded {
            Some(x) => (x.view(), Some(x.root)),
            None => (*v, written_ret),
        };
        let first = u32::from(is_async);
        let mut opaques: Vec<(lf::TypeId, PTy), Global> = Vec::with_capacity_in(rets.len(), Global);
        for (j, &t) in rets.iter().enumerate() {
            let k = super::keys::opaque_key(key, first + j as u32);
            opaques.push((t, intern(&mut self.tys, PNode::Opaque(k, ident))?));
        }
        let mut bounds = Vec::new_in(Global);
        let Bounds::Done(outer_bounds) = outer.bounds else {
            return Err(Why::Program("a fn's container's bounds were not lowered before the fn".into(), Range::new(0, 0)));
        };
        bounds.extend_from_slice(outer_bounds);
        let b = Builder { resolver: Paths { names }, heads: &self.maps, marks };
        // The return type's builder: an expanded return type is a text of its own, read at the
        // fn's module; the item's marks are not its tokens'.
        let rb = Builder { resolver: Paths { names }, heads: &self.maps, marks: if expanded.is_some() { None } else { marks } };
        // A LOCAL fn's, const's or static's signature stands in the block it is declared in
        // (the Reference, "Statements", "Item declarations": its scope is the block's), so a
        // written type's first segment is first asked of that block's `use` items: the body
        // scope of the local itself, from which `Locals::find` walks out to its declaration
        // (block_type_uses.rs `through_use`, its local-signature arm). Its head is read at
        // `LocalHead::names_at`, and a block of only `use` items and fns is no module of the
        // names library, so without this a block import named nothing there (alloc's
        // `custom_owner_from_box`: `use core::ptr::NonNull;` then `unsafe fn
        // drop_box_raw<T: ?Sized>(ptr: NonNull<T>)`: E0412; ladder rungs f3-dyn,
        // x0-ref-11-deref-unsized, x0-ref-12-wide-ptr-eq; tests/front2_block_use_types.rs).
        let body = self.locals.head(key).map(|_| super::block_type_uses::BodyScope { locals: &self.locals, owner: key, module: outer.module, alloc: self.tys.alloc() });
        let env = Env { module: outer.module, params: &ps, bounds: Bounds::Building(&[]), self_: outer.self_, self_assoc: outer.self_assoc, anon: &anon, consts: &cs, body };
        // A bound whose trait names nothing is the fn's own E0405 (kept below), not its head's
        // refusal: the head is made without it, and its callers are not blamed. Only a fn with
        // a body: its body's check is what says the finding (check.rs `signature_misses`); a
        // bodiless fn's such bound stays its head's error.
        let says_misses = flags & sig::HAS_BODY != 0;
        let mut bound_misses = blaze_own::Vec::new_in(self.tys.alloc());
        b.param_bounds_kept(&mut self.tys, &env, v, parent, &mut bounds, if says_misses { Some(&mut bound_misses) } else { None })?;
        let env = Env { bounds: Bounds::Done(&bounds), ..env };
        // The return type's env: its opaques by their ids in `rv`; the parameters' `impl`s are
        // not in it (none is written in a return type).
        let ret_env = Env { anon: &opaques, ..env };
        let self_ty = match env.self_ {
            SelfTy::Trait(_) => Some(intern(&mut self.tys, PNode::Param(0))?),
            SelfTy::Ty(t, _) => Some(t),
            SelfTy::None => None,
        };
        let mut inputs = Vec::new_in(Global);
        let mut self_param = false;
        // The signature's written types that name nothing (E0412): each the error type here.
        let mut misses: Vec<TypeMiss, Global> = Vec::new_in(Global);
        // The signature's region slots, laid in the walks that make its types (D148): one
        // fragment per input, then the return type's. The container group comes from the
        // container's head ([`ContainerRegions`]): `Self`'s regions are its self type's.
        let alloc = self.tys.alloc();
        let mut sink = RegionSink::new_in(alloc);
        let container_regions = match container {
            Some(c) => match self.container_regions.get(&c) {
                Some(cr) => Some(cr),
                None => {
                    sink.refuse(RegionRefusal::NoContainer { container: c });
                    None
                }
            },
            None => None,
        };
        let self_given: &[Region] = container_regions.and_then(|c| c.self_regions.as_deref()).unwrap_or(&[]);
        let self_binders = container_regions.map_or(0, |c| c.self_binders);
        match env.self_ {
            SelfTy::Ty(..) if container_regions.and_then(|c| c.self_regions.as_ref()).is_some() => sink.set_self(Some(self_given)),
            _ => sink.set_self(None),
        }
        // A trait impl's member: its trait's lifetimes as the header writes them, for
        // `Self::Name` (types.rs `projection_slots`, `Through::SelfImpl`).
        match env.self_ {
            SelfTy::Ty(..) => sink.set_impl_trait(container_regions.and_then(|c| c.trait_regions.as_deref())),
            _ => sink.set_impl_trait(None),
        }
        sink.set_param_bounds(container_regions.map_or(&[], |c| c.param_bounds.as_slice()));
        let list = v.list(head::PARAMS);
        for (k, &n) in v.rows(list)?.iter().enumerate() {
            let prm = lf::Param::decode(n)?;
            // `...` (last, blaze-lower's grammar): no input; `c_variadic` below records it.
            if prm.variadic {
                continue;
            }
            let r = v.site(v.row_range(list.start + k as u32)?)?;
            sink.set_part(SigPart::Input(inputs.len() as u32));
            let ty = match (prm.self_kind, self_ty) {
                (lf::SelfKind::No | lf::SelfKind::Typed, _) => ty_or_miss(&b, &mut self.tys, &env, v, prm.ty.ok_or(Why::Unsupported("a parameter with no type", r))?, text, &mut misses, &mut sink)?,
                (lf::SelfKind::Value, Some(s)) => {
                    push_self_slots(&mut sink, &env, None, self_given, self_binders, alloc);
                    s
                }
                (lf::SelfKind::Ref(m), Some(s)) => {
                    // `&'a self`, `&self`: the reference's slot, then `Self`'s.
                    let own = match prm.lifetime {
                        None => sink.new_elided(r),
                        Some(nm) => written_lifetime(v, nm, &mut sink)?,
                    };
                    push_self_slots(&mut sink, &env, Some(own), self_given, self_binders, alloc);
                    intern(&mut self.tys, PNode::Ref(m, s))?
                }
                (_, None) => return Err(Why::Program("a `self` parameter outside a trait or an impl".into(), r)),
            };
            self_param |= k == 0 && prm.self_kind != lf::SelfKind::No;
            inputs.push(ty);
        }
        // A check across the inputs and the container (below) is the signature's as a whole.
        sink.set_part(SigPart::Whole);
        // The item's lifetimes in `regions::Numbering`'s three groups: the container's, its
        // own as declared, one per distinct elided input lifetime in input slot order.
        let mut own_names: Vec<(Name, &str), Alloc<'static>> = Vec::new_in(alloc);
        for &row in v.rows(v.list(head::GENERICS))? {
            if let lf::GenericParam::Lifetime { name, .. } = lf::GenericParam::decode(row)? {
                let t = v.name_text(name)?;
                own_names.push((Name(v.name(name)?), t.strip_prefix('\'').unwrap_or(t)));
            }
        }
        let mut elided_ids: Vec<u32, Alloc<'static>> = Vec::new_in(alloc);
        for f in sink.frags() {
            for s in sink.slots_of(*f) {
                if let SlotKind::Elided(id) = s.kind
                    && !elided_ids.contains(&id)
                {
                    elided_ids.push(id);
                }
            }
        }
        let numbering = Numbering { container: container_regions.map_or(0, |c| c.count), own: own_names.len() as u32, elided: elided_ids.len() as u32 };
        // Every fragment so far is an input's; the next one is the return type's.
        let inputs_laid = sink.frags().len();
        let text_prefix = v.fn_text_prefix(detail)?;
        let whole = Range::new(0, text_prefix.unwrap_or(text.len() as u32));
        // The fn's lifetimes by name (D148): its container's, then its own, numbered after the
        // container's group (`Numbering`'s first two groups; its elided ones have no name).
        let container_scope = match container {
            Some(c) => Some(self.container_scopes.get(&c).ok_or(Why::Regions(RegionsRefusal::Disagree { what: "a member whose container's lifetimes are not filed (its trait or impl head was not made first)" }))?),
            None => None,
        };
        let fn_scope = Scope::item(v, container_scope, container_scope.map_or(0, Scope::params), alloc).map_err(Why::Regions)?;
        // Where the two numberings meet: the container group the signature is numbered with is
        // the one its clauses' names are.
        if let (Some(cr), Some(cs)) = (container_regions, container_scope)
            && cr.count != cs.params()
        {
            sink.refuse(RegionRefusal::not_laid(NotLaidWhy::ContainerDisagree, whole));
        }
        // How the signature elides (D148): numbered by `numbering`, the output rule over the
        // inputs' fragments. `fn_regions` below lays the signature by it, and a return opaque's
        // bounds take `'_` by it ([`FnSig::opaque_elided`]).
        let receiver = self_param && matches!((inputs.first().map(|&t| self.tys.view().node(t)), self_ty), (Some(Ok(PNode::Ref(_, p))), Some(s)) if p == s);
        let sig = FnSig { numbering, own: &own_names, elided: &elided_ids, container: container_regions, inputs_laid, receiver, item_fn: matches!(detail, ItemDetail::Fn { .. }), is_async, whole };
        // A return opaque's scope: the fn's names, and `'_` (`-> impl Trait + '_`,
        // `-> impl Iterator<Item = &u8>`) by the output rule, or why it gives none (soft).
        // Asked BEFORE the return type is walked: the output rule reads the inputs alone, and a
        // refusal of the return type's own slots (an opaque whose captures are not decided,
        // types.rs `RegionSink::opaque`) says nothing of them. Asked after the walk, that
        // refusal was `'_`'s answer, and `-> impl Tr + '_` lost its outlives clause
        // (region_tests `an_elided_bound_takes_the_output_rule`).
        let opaque_elided = sig.opaque_elided(&sink, alloc);
        let opaque_scope = fn_scope.with_elided(opaque_elided);
        // Each return opaque's captured lifetimes (the Reference, "Impl trait", "Capturing"):
        // every lifetime in scope, `Param(0..count)`, for an `async fn`'s future and in edition
        // 2024. Before 2024 an RPIT captures the lifetimes its bounds name (RFC 3498, "Rust 2021
        // and earlier"; `'_` the output rule's). Where the rule itself refuses (a `'_` with no
        // output lifetime), the opaque's slots are refused by name, never guessed.
        let in_scope = numbering.count().ok();
        let edition_2024 = names.crate_of(lib).and_then(|c| names.edition_of(c)).is_some_and(|e| matches!(e, blaze_surface::Edition::E2024));
        // A trait's method, and a trait impl's: its `-> impl Trait` captures every generic
        // parameter in scope, lifetimes too, in EVERY edition (the Reference, "Impl trait",
        // "Capturing": "return-position `impl Trait` in traits and trait implementations ...
        // automatically capture all in-scope generic parameters, including lifetime
        // parameters"; RFC 3425). Read as a free fn's pre-2024 rule, tokio-like `fn sleep(&self,
        // d: Duration) -> impl Future` captured nothing decided and every caller's borrow
        // check was refused "a region position the signature's walk does not lay" (tempo: 452,
        // first crates/zones/node/src/role.rs).
        //
        // A written `use<..>` decides the set exactly, under every rule (RFC 3617; in a trait
        // since Rust 1.87, `precise_capturing_in_traits`): commonware-runtime 2026.10.0's
        // `Clock::sleep(&self, d: Duration) -> impl Future<Output = ()> + Send + 'static +
        // use<Self>` captures `Self` and NO lifetime, `&self`'s elided one included. Skipped as
        // "not decided here" before, its every caller's borrow check was refused "a region
        // position the signature's walk does not lay" (tempo: 485, first
        // crates/contracts/src/precompiles/tip20_channel_reserve.rs). The set is now carried as
        // the regions it lists ([`Captures`]), so a proper subset (`use<'b>` of `'a, 'b`; a
        // pre-2024 RPIT naming one of two) is laid as well as the whole.
        let in_trait = matches!(env.self_, SelfTy::Trait(_) | SelfTy::Ty(_, Some(_)));
        let rule = if edition_2024 || in_trait { hreg::Capture::All } else { hreg::Capture::Named };
        let mut captures: Vec<(ItemKey, u32), Alloc<'static>> = Vec::new_in(alloc);
        let mut captured: blaze_own::Vec<Region, Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        if let Some(n) = in_scope {
            if is_async {
                captures.push((super::keys::opaque_key(key, 0), n));
                for i in 0..n {
                    captured.push(Region::Param(i));
                }
            }
            for (j, &(t, _)) in opaques.iter().enumerate() {
                // The rule's own refusal (a `'_` the output rule gives none for, `'static` in
                // `use<..>`) leaves the opaque undecided: its slots are refused by name.
                let lo = captured.len();
                match hreg::captures(&rv, &opaque_scope, alloc, Some(t), rule, n, opaque_elided.ok(), &mut captured) {
                    Ok(()) => match u32::try_from(captured.len() - lo) {
                        Ok(len) => captures.push((super::keys::opaque_key(key, first + j as u32), len)),
                        Err(_) => captured.truncate(lo),
                    },
                    Err(_) => captured.truncate(lo),
                }
            }
        }
        for &(k, n) in captures.iter() {
            self.maps.opaque_lifetimes.insert(k, n);
        }
        let all_captures = Captures { counts: &captures, regions: captured.as_slice() };
        sink.set_opaques(all_captures);
        sink.set_part(SigPart::Output);
        let written = match ret {
            // An expanded return type's ranges are in its expansion's text, not the fn's: a
            // miss there stays the refusal it was.
            Some(t) if expanded.is_none() => ty_or_miss(&rb, &mut self.tys, &ret_env, &rv, t, text, &mut misses, &mut sink)?,
            Some(t) => rb.ty_regions(&mut self.tys, &ret_env, &rv, t, &mut sink)?,
            None => {
                sink.push(&[], 0);
                intern(&mut self.tys, PNode::Tuple(List::EMPTY))?
            }
        };
        // Filed with the bounds' misses once the predicates are made (below); none is kept for
        // a head refused between here and there.
        self.type_misses.remove(&key);
        let mut kept = blaze_own::Vec::with_capacity_in(misses.len(), self.tys.alloc());
        kept.extend(misses.iter().map(TypeMiss::kept));
        if flags & sig::GEN != 0 {
            return Err(Why::Unsupported("a `gen fn`", whole));
        }
        let c_variadic = flags & sig::VARIADIC != 0;
        if c_variadic && flags & sig::HAS_BODY != 0 {
            return Err(Why::Unsupported("a C-variadic fn definition (`unsafe extern \"C\" fn f(x: i32, mut args: ...)`: the unstable `c_variadic` feature)", whole));
        }
        // Each opaque's bounds as written, with the opaque type as `Self`, are its own
        // predicates (the checker's `Predicates(k)`); the written ones' bounds are made after
        // the fn's own bounds are known (a bound may name `T::Item`).
        // `Self`'s regions and the impl's trait column as the signature's walk was given them
        // (`set_self` and `set_impl_trait` above): an opaque's bounds and the fn's clauses may
        // name `Self::Name` too (`-> impl Iterator<Item = Self::Value>`).
        let self_laid = SelfGiven {
            self_: match env.self_ {
                SelfTy::Ty(..) if container_regions.and_then(|c| c.self_regions.as_ref()).is_some() => Some(self_given),
                _ => None,
            },
            impl_trait: match env.self_ {
                SelfTy::Ty(..) => container_regions.and_then(|c| c.trait_regions.as_deref()),
                _ => None,
            },
            param_bounds: container_regions.map_or(&[], |c| c.param_bounds.as_slice()),
        };
        for (j, &(t, me)) in opaques.iter().enumerate() {
            let k = super::keys::opaque_key(key, first + j as u32);
            let mut op = Preds::new_in(opaque_scope.clone_in(alloc));
            rb.impl_bounds(&mut self.tys, &ret_env, &rv, &[(t, me)], self.sized, &mut op)?;
            // Its regions, when the signature's column lays its captures (`captures` above).
            let filed = match (in_scope, all_captures.of(k)) {
                (Some(count), Some(run)) => opaque_filed(&rb, &mut self.tys, &self.maps, &ret_env, &rv, (&op, Some(t), me), (count, run, all_captures, rule, opaque_elided.ok()), (self_laid, &opaque_scope), alloc)?,
                _ => Err(RegionGap::Laid(RegionRefusal::not_laid(NotLaidWhy::OpaqueCaptures, whole), SigPart::Output)),
            };
            file_regions(self, k, filed);
            self.preds.insert(k, op);
            self.opaques.insert(k, (key, first + j as u32));
        }
        // An `async fn f(..) -> T` is `fn f(..) -> impl Future<Output = T>` (the Reference,
        // "Async functions": "async fn example(x: &str) -> usize { x.len() }" is "roughly
        // equivalent to" "fn example<'a>(x: &'a str) -> impl Future<Output = usize> + 'a"):
        // its output is opaque 0 with the bounds `Future`, `Output = T` and `Sized`. Its body
        // is an async block (a coroutine): the checker's and the code builder's, not the head's.
        let output = if is_async {
            let fut = super::langs::future_trait(Paths { names }, self.sized.ok_or(Why::Unsupported("an `async fn` in a program with no `Sized` (no core)", whole))?)
                .map_err(|e| Why::Program(format!("the `Future` trait of an `async fn`: {e}").into_boxed_str(), whole))?
                .ok_or(Why::Unsupported("an `async fn` in a program whose core has no `future::Future`", whole))?;
            let k = super::keys::opaque_key(key, 0);
            let me = intern(&mut self.tys, PNode::Opaque(k, ident))?;
            let args = intern_list(&mut self.tys, &[me])?;
            // Its clauses are made here, not written (`Src::Derived`): its regions stay refused
            // until a producer lays the `Output = T` column from the return type's slots.
            let mut op = Preds::new_in(fn_scope.clone_in(alloc));
            op.push(PClause::Trait { trait_: checker_of(fut), args }, hreg::Src::Derived);
            op.push(PClause::Projection { trait_: checker_of(fut), args, name: Name::of("Output"), term: written }, hreg::Src::Derived);
            if let Some(s) = self.sized {
                op.push(PClause::Trait { trait_: checker_of(s), args }, hreg::Src::Derived);
            }
            // Its regions: the future captures every lifetime in scope (the Reference, "Async
            // functions": "+ 'a" of each; Rust 2024 "Capturing rules": all in-scope generic
            // parameters), laid above as `Param(0..n)` (`captures`); its clauses are derived,
            // so they have no column (heads_regions.rs `fill_clauses`), and `T`'s slots are
            // the fn's own `written_output` column. Refused only when the scope's count was.
            let filed = match (in_scope, all_captures.of(k)) {
                (Some(count), Some(run)) => async_future_filed(&rb, &mut self.tys, &self.maps, &ret_env, &rv, (&op, me), (count, run, all_captures), (self_laid, &fn_scope), alloc)?,
                _ => Err(RegionGap::Laid(RegionRefusal::not_laid(NotLaidWhy::AsyncCount, whole), SigPart::Output)),
            };
            file_regions(self, k, filed);
            self.preds.insert(k, op);
            self.opaques.insert(k, (key, 0));
            me
        } else {
            written
        };
        let header = tc::FnPtrHeader { abi: abi_of(v, whole)?, safety: if flags & sig::UNSAFE != 0 { tc::Safety::Unsafe } else { tc::Safety::Safe }, c_variadic: false };
        // A fn an `extern` block in a body declares ([`super::foreign_locals`]): its own text
        // carries no ABI, its block's does, and it "is implicitly unsafe unless the safe
        // function qualifier is present" (the Reference, "External blocks").
        // A foreign STATIC of such a block takes the same answer (a static's head is made here
        // too, `Want::Fn` of a static): its block's ABI and `unsafe` unless written `safe`
        // (the Reference, [items.extern.static.safety]), exactly as `module_foreign_header`
        // answers a module-level block's static. Before, only a `ForeignKind::Fn` was taken
        // here and a body's foreign static fell through to the names source, which holds no
        // row for an item declared in a body (its key is derived at lowering, locals.rs): "a fn
        // with no body whose `extern` block the names source did not answer".
        //
        // The answer comes from where the head's TEXT came from (rule 30): an item whose text
        // is the body lowering's (`Locals::head`) is answered by `Locals` alone, never by the
        // names source, which cannot know it; any other item by the names source's row.
        let local = self.locals.head(key).is_some();
        let header = match self.locals.foreign(key) {
            Some(f) => tc::FnPtrHeader { abi: super::types::fn_ptr_abi(f.abi, whole)?, safety: if f.safe { tc::Safety::Safe } else { tc::Safety::Unsafe }, c_variadic: false },
            // An item declared in a body outside every `extern` block: a fn or static with no
            // body there is no Rust ("Functions", [items.fn.body]; "Static items": a static
            // outside an extern block has an initializer), refused by name.
            None if local && flags & sig::HAS_BODY == 0 => return Err(Why::Unsupported(LOCAL_NO_BODY, whole)),
            None if local => header,
            // A fn a MODULE-LEVEL `extern` block declares: its block's ABI, and `unsafe` unless
            // written `safe` (super::foreign_head). Read as its own head before, it was a safe
            // Rust fn: aws-lc-rs's `EVP_aead_chacha20_poly1305` was no `unsafe extern "C" fn`
            // pointer (E0308) and libc's variadic `open` was refused as a `...` on a Rust fn.
            None => super::foreign_head::module_foreign_header(names, lib, container.is_some(), flags, header, whole)?,
        };
        if c_variadic && !header.abi.takes_c_variadic() {
            return Err(Why::Unsupported("`...` on a fn of an ABI other than \"C\" or \"system\" (the Reference, [items.extern.variadic.conventions])", whole));
        }
        let has_body = flags & sig::HAS_BODY != 0;
        if !has_body && has_infer_hole(self.tys.view(), written, alloc) {
            return Err(Why::Program("inferred return/error type `_` requires a function body".into(), whole));
        }
        let mut preds = Preds::new_in(fn_scope.clone_in(alloc));
        preds.inherit(inherited);
        if says_misses {
            preds.keep_trait_misses();
        }
        b.predicates(&mut self.tys, &env, v, parent, None, self.sized, &mut preds)?;
        b.impl_bounds(&mut self.tys, &env, v, &anon[..params_end], self.sized, &mut preds)?;
        // The bounds' E0405s: one per bound path (the parameter-bound walk and the predicates
        // walk each meet it), in text order. One the words cannot be made for (a path not in
        // the fn's own text) stays the head's error, as before.
        if let Some(found) = preds.trait_misses.take() {
            bound_misses.extend(found.iter().copied());
        }
        bound_misses.sort_by_key(|m| (m.1.start, m.1.end));
        bound_misses.dedup_by_key(|m| (m.1.start, m.1.end));
        for &(e, at) in bound_misses.iter() {
            let miss = TypeMiss::of_trait(names, text, e, at).ok_or(Why::Resolve(crate::resolve::ResolveError::Path(e), at))?;
            kept.push(miss.kept_as(MissOf::Trait));
        }
        kept.sort_by_key(|m| (m.at.start, m.at.end));
        if !kept.is_empty() {
            self.type_misses.insert(key, kept);
        }
        self.preds.insert(key, preds);
        let mut self_assoc = Vec::with_capacity_in(env.self_assoc.len(), Global);
        self_assoc.extend_from_slice(env.self_assoc);
        let (module, self_) = (env.module, env.self_);
        // A free fn's const parameters are filed under its own key. A member's `lib` is its
        // CONTAINER's key (a member has no library key): filing there replaced the trait's or
        // impl's row with the container's plus this member's, so after `Iterator::next_chunk<const
        // N: usize>` the trait's index 1 read as a const parameter, and `sum<S>`'s `S` (index 1
        // too) was refused "a const parameter in a type position" when the trait's members
        // were made again. A member's own are in its `FnHead::consts`.
        if container.is_none() {
            self.maps.const_params.insert(lib, cs.to_vec_in(*cs.allocator()));
        }
        // `#[rustc_legacy_const_generics(1)]` (stdarch's `vget_lane_u64(v, 0)`): the positions
        // of the call arguments that are the fn's own const generic arguments.
        let mut legacy_const_generics = Vec::new_in(Global);
        let mut qualifiers = FnQualifiers::of_flags(flags).note_lifetimes(text);
        let attrs = v.list(head::ATTRS);
        for (i, &n) in v.rows(attrs)?.iter().enumerate() {
            let a = v.attr(n)?;
            let path = v.text_at(a.path)?.trim();
            qualifiers.attribute(path);
            if path == "rustc_legacy_const_generics" {
                let r = v.site(v.row_range(attrs.start + i as u32)?)?;
                legacy_positions(v.text_at(a.args)?, r, &mut legacy_const_generics)?;
            }
        }
        // Only a bodyless free declaration can bind a std-internal name. Compare every
        // active link_name against the name the binding rule uses, including a local
        // declaration's spelling in its enclosing extern block.
        if qualifiers.internal_symbol() && !has_body && container.is_none() {
            let name = if self.locals.foreign(key).is_some() {
                super::panic_entry::local_name(self, key).map_err(|_| Why::Unsupported("a std-internal declaration absent from its enclosing body's rows", whole))?
            } else {
                names.name_of(lib)
            };
            if let Some(name) = name {
                // The value cooked as the native image reads it (links.rs `symbol_of`: a value
                // written as a macro call is the names' row of the item, as the link table's).
                let edition = names
                    .crate_of(lib)
                    .and_then(|c| names.edition_of(c))
                    .ok_or(Why::Unsupported("a std-internal declaration of a crate whose edition is not read", whole))?;
                for &n in v.rows(attrs)? {
                    let attr = v.attr(n)?;
                    if v.text_at(attr.path)?.trim() != "link_name" {
                        continue;
                    }
                    let values_at = super::links::values_key(names, self, lib);
                    let renamed = super::links::with_symbol_of(v.text_at(attr.args)?, Some(values_at), |item| names.attr_value(item, "link_name"), edition, |res| res.map(|symbol| symbol != name).map_err(|_| ()))
                        .map_err(|_| Why::Unsupported("a `#[link_name]` whose value names no symbol (front2 links.rs `symbol_of`)", whole))?;
                    if renamed {
                        qualifiers.0 |= FnQualifiers::RENAMED_INTERNAL;
                        break;
                    }
                }
            }
        }
        // The signature's regions (D148), from the slots the walks above laid: numbered,
        // elided by the Reference's rules, late-bound marked, laid over `FnPtr(inputs..,
        // output)` and filed in the form the record writes ([`super::head_record::FiledRegions`]).
        let mut sig_kids: Vec<PTy, Alloc<'static>> = Vec::with_capacity_in(inputs.len() + 1, alloc);
        sig_kids.extend_from_slice(&inputs);
        sig_kids.push(output);
        let sig_list = intern_list(&mut self.tys, &sig_kids)?;
        let sig_ty = intern(&mut self.tys, PNode::FnPtr(sig_list, header))?;
        // Every ADT's, trait's and associated type's count the column asks is read from the
        // names (D155: its crate's items record), whatever this store made before; no need is
        // raised for a count.
        let filed = match fn_regions(v, &sink, &sig, alloc)? {
            Err(why) => Err(RegionGap::placed(&sink, why)),
            Ok((params, sig_slots, written_slots)) => {
                // An opaque a member's container's record counted is counted as it counted it
                // ([`MemberArity`]); every declared count is the names' (D155).
                let counts = container.and_then(|c| self.regions.made(c)).map(super::head_record::FiledRegions::arity);
                let arity = MemberArity { container: counts, heads: HeadsArity { heads: &self.maps, opaques: sink.opaques(), names } };
                let view = self.tys.view();
                match (RegionCol::lay(view, sig_ty, &arity, &sig_slots, alloc), written_slots.map(|w| RegionCol::lay(view, written, &arity, &w, alloc)).transpose()) {
                    (Ok(sig_col), Ok(written_col)) => {
                        let mut hr = HeadRegions::new_in(params, alloc);
                        hr.sig = Some(sig_col);
                        hr.written_output = written_col;
                        // Its clauses' columns and outlives (heads_regions.rs `fill_clauses`), in
                        // the numbering `params` is: the container's group first, so an inherited
                        // clause's column is its container's as filed (a trait's members' from
                        // `trait_inherits`, an impl's from its filed regions).
                        let container_heads: Option<&HeadRegions<Alloc<'static>>> = match (container, self_) {
                            (Some(c), SelfTy::Trait(_)) => self.trait_inherits.get(&c),
                            (Some(c), _) => self.regions.made(c).map(super::head_record::FiledRegions::regions),
                            (None, _) => None,
                        };
                        let preds = self.preds.get(&key).ok_or(Why::Regions(RegionsRefusal::Disagree { what: "a fn whose clauses, filed above, are gone" }))?;
                        let laid = lay_listed(&b, &mut self.tys, &env, v, (preds, &[]), self_laid, sink.captures(), alloc)?;
                        let cx = hreg::Cx { v, facts: &arity, written: &laid, scope: &fn_scope, made: &[], alloc };
                        match hreg::fill_clauses(&cx, &mut self.tys, preds, container_heads, &mut hr) {
                            Ok(()) => {
                                let view = self.tys.view();
                                super::head_record::FiledRegions::new(hr, &arity, view, alloc).map_err(RegionGap::Record)
                            }
                            Err(why) => Err(RegionGap::Clauses(why)),
                        }
                    }
                    (Err(e), _) | (_, Err(e)) => Err(RegionGap::Laid(RegionRefusal::Regions(e), SigPart::Whole)),
                }
            }
        };
        file_regions(self, key, filed);
        if Self::view_has_record_type(v) {
            let rec_env = Env { module, params: &ps, bounds: Bounds::Done(&bounds), self_, self_assoc: &self_assoc, anon: &anon, consts: &cs, body: None };
            self.register_record_defaults(names, keys, lib, &rec_env, marks, v)?;
        }
        self.fns.insert(key, FnHead { lib, text, text_prefix, container, parent, own, declared, self_param, header, c_variadic, qualifiers, legacy_const_generics, inputs, output, has_body, module, params: ps, consts: cs, bounds, self_, self_assoc });
        Ok(())
    }
}

/// Whether any type node reachable from `root` in `tys` is `PNode::Infer` (`_`).
pub(crate) fn has_infer_hole(tys: tc::PTys<'_>, root: PTy, alloc: Alloc<'_>) -> bool {
    let mut work: blaze_own::Vec<PTy, Alloc<'_>> = blaze_own::Vec::new_in(alloc);
    work.push(root);
    let mut steps = 0u32;
    while let Some(t) = work.pop() {
        steps += 1;
        if steps > 1 << 16 {
            return false;
        }
        let Ok(node) = tys.node(t) else { return false };
        if matches!(node, PNode::Infer) {
            return true;
        }
        if tys.children_of(node, &mut work).is_err() {
            return false;
        }
    }
    false
}

/// A fn head's regions as the walk lays them (D148, piece L1a): numbering by declaration,
/// elision's three rules and E0106, a fn pointer's own variables. Each test has a planted
/// control that the property it checks would catch.
#[cfg(test)]
mod region_tests {
    use super::{Store, Want};
    use crate::front2::keys::{Keys, MemberKind, checker_of, member_key_of};
    use crate::front2::source::NamesSource;
    use crate::front2::tables::head_tests::{a, cells, world_of};
    use blaze_id::ItemKey as LibKey;
    use blaze_typeck::ItemKey;
    use blaze_typeck::regions::Region;
    use std::format;
    use std::string::String;
    use std::vec::Vec;

    const APP: &str = "pub fn f<'a, 'b>(y: &'b u8, x: &'a u8) -> &'a u8 { x }
pub struct S<'x> { r: &'x u8 }
impl<'x> S<'x> {
    pub fn g(&self, x: &u8) -> &u8 { x }
}
pub fn p(q: fn(&u8) -> &u8) {}
";

    const E0106: &str = "pub fn h(x: &u8, y: &u8) -> &u8 { x }\n";

    /// The free fn `name` of the app crate (crate 0 of `world_of`).
    fn item(names: crate::front2::tables::head_tests::Src, name: &str) -> LibKey {
        let mut found = None;
        NamesSource::each_item(names, 0, &mut |key, _| {
            if NamesSource::name_of(names, key) == Some(name) {
                found = Some(key);
            }
        });
        found.unwrap_or_else(|| panic!("`{name}` in the fixture"))
    }

    /// The fn of `want` made in a fresh store: its signature's regions and late-bound
    /// parameters, or why there are none.
    fn sig_of(app: &'static str, want: impl Fn(crate::front2::tables::head_tests::Src) -> (Want, ItemKey)) -> Result<(Vec<Region>, Vec<bool>), String> {
        let (names, sized, frame) = world_of(app);
        let (w, key) = want(names);
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        store.ensure(names, &mut keys, w).map_err(|e| format!("ensure: {e:?}"))?;
        if let Some(why) = store.fn_gaps.get(&key) {
            return Err(format!("{why:?}"));
        }
        let Some(filed) = store.regions.made(key) else {
            return Err(format!("no regions filed: {:?}", store.region_gaps.get(&key)));
        };
        let hr = filed.regions();
        let sig = hr.sig.as_ref().ok_or_else(|| String::from("no signature column"))?;
        let mut late = Vec::new();
        for i in 0..hr.params.count() {
            late.push(hr.params.is_late(i).map_err(|e| format!("{e:?}"))?);
        }
        Ok((sig.as_slice().to_vec(), late))
    }

    fn free(name: &'static str) -> impl Fn(crate::front2::tables::head_tests::Src) -> (Want, ItemKey) {
        move |names| {
            let lib = item(names, name);
            (Want::Fn(lib), checker_of(lib))
        }
    }

    /// The numbering by declaration a planted bug would replace: first appearance in the
    /// inputs (`'b` met first would be `Param(0)`).
    fn by_first_appearance(laid: &[Region]) -> Vec<Region> {
        let mut seen: Vec<Region> = Vec::new();
        laid.iter()
            .map(|&r| match r {
                Region::Param(_) => {
                    let i = match seen.iter().position(|&s| s == r) {
                        Some(i) => i,
                        None => {
                            seen.push(r);
                            seen.len() - 1
                        }
                    };
                    Region::Param(i as u32)
                }
                other => other,
            })
            .collect()
    }

    /// `fn f<'a, 'b>(y: &'b u8, x: &'a u8) -> &'a u8` lays `[Param(1), Param(0), Param(0)]`:
    /// `'a` is 0 and `'b` is 1 by declaration, wherever they are first written. Control: the
    /// first-appearance numbering (the planted bug) lays `[Param(0), Param(1), Param(1)]`,
    /// which this test refuses.
    #[test]
    fn a_fn_numbers_its_lifetimes_by_declaration() {
        let (sig, late) = sig_of(APP, free("f")).unwrap_or_else(|e| panic!("`f`'s regions: {e}"));
        let want = [Region::Param(1), Region::Param(0), Region::Param(0)];
        assert_eq!(sig, want, "ORACLE: `f`'s signature column");
        let planted = by_first_appearance(&sig);
        assert_eq!(planted, [Region::Param(0), Region::Param(1), Region::Param(1)], "CONTROL: the planted numbering");
        assert_ne!(planted[..], want[..], "CONTROL: first-appearance numbering passes the oracle");
        // Both are used in the inputs and named by no bound: late-bound.
        assert_eq!(late, [true, true], "`'a` and `'b` late-bound");
    }

    /// `impl<'x> S<'x> { fn g(&self, x: &u8) -> &u8 }` elides its output to `&self`'s
    /// lifetime: `'x` is the container's `Param(0)`, `&self`'s elided lifetime `Param(1)`,
    /// `x`'s `Param(2)`; the column over `FnPtr(&S<'x>, &u8, &u8)` is `[1, 0, 2, 1]`.
    /// Control: the output does not take `x`'s lifetime (the one-input rule would, were it
    /// asked first; with two input lifetimes and no receiver rule it would be E0106).
    #[test]
    fn a_method_elides_its_output_to_self() {
        let g = |names| {
            let mut found = None;
            for w in cells(names).iter() {
                if let Want::MemberFn(lib, k) = *w
                    && k == member_key_of(checker_of(lib), MemberKind::Fn, "g")
                {
                    found = Some((*w, k));
                }
            }
            found.expect("`S::g`'s cell")
        };
        let (sig, late) = sig_of(APP, g).unwrap_or_else(|e| panic!("`g`'s regions: {e}"));
        assert_eq!(sig, [Region::Param(1), Region::Param(0), Region::Param(2), Region::Param(1)], "ORACLE: `g`'s signature column");
        assert_ne!(sig[3], sig[2], "CONTROL: the output took `x`'s lifetime");
        // The container's `'x` is early-bound; the two elided inputs are late.
        assert_eq!(late, [false, true, true], "`g`'s late-bound parameters");
    }

    /// `fn h(x: &u8, y: &u8) -> &u8` is E0106: two input lifetimes, no receiver. Control: the
    /// same fn with one input (`fn h(x: &u8) -> &u8`) is laid, its output `x`'s.
    #[test]
    fn two_inputs_and_an_elided_output_is_e0106() {
        let refused = sig_of(E0106, free("h")).expect_err("ORACLE: `h` was laid");
        assert!(refused.contains("E0106"), "ORACLE: `h`'s refusal names E0106: {refused}");
        let (sig, _) = sig_of("pub fn h(x: &u8) -> &u8 { x }\n", free("h")).unwrap_or_else(|e| panic!("CONTROL: one input refused: {e}"));
        assert_eq!(sig, [Region::Param(0), Region::Param(0)], "CONTROL: one input's lifetime is the output's");
    }

    /// A fn pointer argument `fn(&u8) -> &u8` binds its own lifetime: both slots are
    /// `Late { binder: 1, var: 0 }` (the signature's root fn pointer is binder 0, the argument
    /// binder 1), and the fn has no lifetime parameter. Control: they are not the fn's
    /// parameters (a producer that numbered the inner elision in the fn's group would lay
    /// `Param(0)` and count one parameter).
    #[test]
    fn a_fn_pointer_argument_binds_its_own_lifetimes() {
        let (sig, late) = sig_of(APP, free("p")).unwrap_or_else(|e| panic!("`p`'s regions: {e}"));
        let inner = Region::Late { binder: 1, var: 0 };
        assert_eq!(sig, [inner, inner], "ORACLE: `p`'s signature column");
        assert!(late.is_empty(), "CONTROL: the fn pointer's lifetime became a parameter of `p`: {late:?}");
        assert!(!sig.contains(&Region::Param(0)), "CONTROL: the inner elision was numbered in `p`'s group");
    }

    /// D148 end to end: the signature walk's numbering (L1a) and the clauses' (L1b, through
    /// the container's and the fn's `Scope`) meet in one head. `g`'s lifetimes are the
    /// container's `'x` (`Param(0)`), its own `'a` (`Param(1)`), then `&self`'s elided one
    /// (`Param(2)`); its column over `FnPtr(&S<'x>, &'a T, &'a T)` is `[2, 0, 1, 1]`; its
    /// clauses give one `T: 'a` (`TypeOutlives` at `Param(1)`) and one `'x: 'a`
    /// (`RegionOutlives(Param(0), Param(1))`); and its head record opens to the same regions
    /// and writes the same bytes again (rule 26). Control (rule 23): without `where 'x: 'a`
    /// the region outlives goes, so it is read from the clause, and its swapped numbering
    /// (`'a` first, the planted bug of numbering the fn's own before its container's) is not
    /// what is filed.
    #[test]
    fn a_member_s_clauses_and_signature_share_one_numbering() {
        use crate::front2::head_record::{self, Head, HeadAt};
        use blaze_typeck::region_facts::Outlives;
        const FIXTURE: &str = "pub struct S<'x> { r: &'x u8 }
impl<'x> S<'x> {
    pub fn g<'a, T: 'a>(&self, x: &'a T) -> &'a T where 'x: 'a { x }
}
";
        const NO_WHERE: &str = "pub struct S<'x> { r: &'x u8 }
impl<'x> S<'x> {
    pub fn g<'a, T: 'a>(&self, x: &'a T) -> &'a T { x }
}
";
        let (p0, p1, p2) = (Region::Param(0), Region::Param(1), Region::Param(2));
        // The made store, `g`'s key, and its outlives as filed.
        let made = |app: &'static str| {
            let (names, sized, frame) = world_of(app);
            let mut found = None;
            for w in cells(names).iter() {
                if let Want::MemberFn(lib, k) = *w
                    && k == member_key_of(checker_of(lib), MemberKind::Fn, "g")
                {
                    found = Some((*w, k));
                }
            }
            let (w, key) = found.expect("`S::g`'s cell");
            let mut store = Store::new(sized, frame, a());
            let mut keys = Keys::new();
            store.ensure(names, &mut keys, w).unwrap_or_else(|e| panic!("ensure: {e:?}"));
            assert!(store.fn_gaps.get(&key).is_none(), "`g` refused: {:?}", store.fn_gaps.get(&key));
            (store, key)
        };
        let (store, key) = made(FIXTURE);
        let filed = store.regions.made(key).unwrap_or_else(|| panic!("ORACLE: no regions filed for `g`: {:?}", store.region_gaps.get(&key)));
        let hr = filed.regions();
        assert_eq!(hr.params.count(), 3, "ORACLE: `g`'s lifetimes are `'x`, `'a` and `&self`'s");
        let sig = hr.sig.as_ref().expect("`g`'s signature column");
        assert_eq!(sig.as_slice(), [p2, p0, p1, p1], "ORACLE: `g`'s signature column");
        let types: Vec<Region> = hr.outlives.iter().filter_map(|o| match *o { Outlives::Type { region, .. } => Some(region), Outlives::Region { .. } => None }).collect();
        let regions: Vec<(Region, Region)> = hr.outlives.iter().filter_map(|o| match *o { Outlives::Region { long, short } => Some((long, short)), Outlives::Type { .. } => None }).collect();
        assert_eq!(types, [p1], "ORACLE: one `T: 'a`, at `'a`'s number");
        assert_eq!(regions, [(p0, p1)], "ORACLE: one `'x: 'a`, the container's `'x` first");
        assert!(!regions.contains(&(p1, p0)), "CONTROL: the fn's own lifetimes were numbered before its container's");
        // The head record: opened, the same regions; written again, the same bytes.
        let text = store.fns.get(&key).expect("`g`'s head").text;
        let bytes = head_record::encode(head_record::from_store(&store, HeadAt::Fn(key)).unwrap_or_else(|e| panic!("from the store: {e}")), store.tys.view(), a()).unwrap_or_else(|e| panic!("encode: {e}"));
        let opened = head_record::decode(&bytes, Some(text), &store.tys, a()).unwrap_or_else(|e| panic!("decode: {e}"));
        let Head::Fn { regions: Some(back), .. } = &opened else {
            panic!("ORACLE: `g`'s record opened with no regions")
        };
        assert!(back.regions().same(hr), "ORACLE: `g`'s regions changed in the round trip");
        let again = head_record::encode(opened.as_ref(), store.tys.view(), a()).unwrap_or_else(|e| panic!("re-encode: {e}"));
        assert_eq!(&again[..], &bytes[..], "ORACLE: `g`'s record written again is other bytes (rule 26)");
        // CONTROL: the cause removed, the effect goes; the rest stays.
        let (bare, bare_key) = made(NO_WHERE);
        let bare_filed = bare.regions.made(bare_key).unwrap_or_else(|| panic!("CONTROL: no regions filed: {:?}", bare.region_gaps.get(&bare_key)));
        let bare_hr = bare_filed.regions();
        assert!(!bare_hr.outlives.iter().any(|o| matches!(o, Outlives::Region { .. })), "CONTROL: a region outlives with no `'x: 'a` written");
        assert!(bare_hr.outlives.iter().any(|o| matches!(*o, Outlives::Type { region, .. } if region == p1)), "CONTROL: `T: 'a` went with the where clause");
        assert!(!bare_hr.same(hr), "CONTROL: `same` cannot tell the two heads apart");
    }

    /// serde's shape: `deserialize_seq<V: Visitor<'de>>(self, v: V) -> V::Value` counts
    /// `Visitor`'s lifetimes for its bound and its projection, and those counts are facts of
    /// `Visitor`'s TEXT (D155: its crate's items record, read through the names), whichever way
    /// the store got `Visitor`'s head; the projection's slot is the bound's `'de` (types.rs
    /// `projection_slots`). Oracle: the member's regions are laid (one slot, the projection's
    /// `'de`) in a fresh store asked for the member first, and alike in a store that filed
    /// `Visitor`'s head from its record (which carries no count). Control (rule 23): with the
    /// names answering no count for `Visitor` (`Records::plant_no_lifetimes`) the member's
    /// regions are refused by name, in a fresh store, so the names are what counted it.
    #[test]
    fn a_member_counts_a_restored_trait_s_lifetimes() {
        use crate::front2::head_record::{self, Head, HeadAt};
        const SERDE: &str = "pub trait Visitor<'de> {
    type Value;
}
pub trait Deserializer<'de> {
    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> V::Value;
}
";
        let (names, sized, frame) = world_of(SERDE);
        let visitor = item(names, "Visitor");
        let deserializer = item(names, "Deserializer");
        let member = member_key_of(checker_of(deserializer), MemberKind::Fn, "deserialize_seq");
        let laid = |store: &Store<'_>| -> Result<Vec<Region>, String> {
            if let Some(why) = store.fn_gaps.get(&member) {
                return Err(format!("refused: {why:?}"));
            }
            let filed = store.regions.made(member).ok_or_else(|| format!("no regions filed: {:?}", store.region_gaps.get(&member)))?;
            let sig = filed.regions().sig.as_ref().ok_or_else(|| String::from("no signature column"))?;
            Ok(sig.as_slice().to_vec())
        };
        // A fresh store asked for the member first.
        let mut fresh = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        fresh.ensure(names, &mut keys, Want::Members(deserializer)).unwrap_or_else(|e| panic!("ensure the members: {e:?}"));
        let first = laid(&fresh).unwrap_or_else(|e| panic!("ORACLE: asked first, `deserialize_seq`'s regions: {e}"));
        // `self: Self` and `v: V` hold none; `V::Value`'s one is the bound's `'de`, the
        // container's first lifetime.
        assert_eq!(first, [Region::Param(0)], "ORACLE: `V::Value`'s one slot is `Visitor<'de>`'s `'de`");
        // `Visitor`'s head alone, as its own cell freezes it: its record.
        let mut own = Store::new(sized, frame, a());
        let mut own_keys = Keys::new();
        own.ensure(names, &mut own_keys, Want::Head(visitor)).unwrap_or_else(|e| panic!("ensure `Visitor`'s head: {e:?}"));
        let bytes = head_record::encode(head_record::from_store(&own, HeadAt::Trait(visitor)).unwrap_or_else(|e| panic!("from the store: {e}")), own.tys.view(), a())
            .unwrap_or_else(|e| panic!("encode: {e}"));
        // A store seeded with that record, then asked for the member.
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        let opened = head_record::decode(&bytes, None, &store.tys, a()).unwrap_or_else(|e| panic!("decode: {e}"));
        assert!(matches!(opened, Head::Trait { .. }), "`Visitor`'s record is not a trait's");
        store.file_heads(Want::Head(visitor), [(HeadAt::Trait(visitor), opened)]).unwrap_or_else(|e| panic!("file `Visitor`'s head: {e:?}"));
        assert!(store.maps.trait_lifetimes.get(&checker_of(visitor)).is_none(), "ORACLE: filing `Visitor`'s record filed a count for the walk (D155: the names hold it)");
        store.ensure(names, &mut keys, Want::Members(deserializer)).unwrap_or_else(|e| panic!("ensure the members over `Visitor`'s record: {e:?}"));
        let restored = laid(&store).unwrap_or_else(|e| panic!("ORACLE: `Visitor` filed from its record, `deserialize_seq`'s regions: {e}"));
        assert_eq!(restored, first, "ORACLE: the member's column depends on how `Visitor`'s head was got (rule 25)");
        // CONTROL: the names answer no count for `Visitor`; a fresh store refuses the column.
        names.plant_no_lifetimes(visitor);
        let mut planted = Store::new(sized, frame, a());
        let mut planted_keys = Keys::new();
        let refused = match planted.ensure(names, &mut planted_keys, Want::Members(deserializer)) {
            Ok(()) => laid(&planted).expect_err("CONTROL: with `Visitor`'s count unanswered the member's regions were laid"),
            Err(e) => format!("ensure the members: {e:?}"),
        };
        let kind = planted.region_gaps.get(&member).map(super::RegionGap::unlaid);
        assert_eq!(kind, Some(crate::front2::head_record::Unlaid::Counts), "CONTROL: the refusal is not `Counts`: {refused}");
        std::println!("deserialize_seq: {first:?} fresh and restored; without the count: {refused}");
    }

    /// The derive's shape (D155): an impl written by a macro expansion, whose member returns
    /// `Self` (`#[derive(Clone)]`'s `fn clone(&self) -> Self` over `Named<'a>`), asked for first
    /// in a FRESH store over records. `Self` is `Named<'a>`, and the walk counts `Named`'s
    /// lifetimes from its crate's items record through the names, with no head of `Named`
    /// made. Oracle: the member's `regions.sig` is laid, `'a` (the impl's first lifetime) at
    /// `Self`'s one slot, as a hand-written impl of the same text lays it. Control (rule 23):
    /// with the names answering no count for `Named` (`Records::plant_no_lifetimes`) the
    /// column is refused by name as `Unlaid::Counts`, so the names are what counted it.
    #[test]
    fn an_expansion_written_impl_returning_self_lays_its_regions() {
        const DERIVED: &str = "pub trait Dup: Sized {
    fn dup(&self) -> Self;
}
pub struct Named<'a> {
    name: &'a u8,
}
macro_rules! dup_by_derive {
    ($t:ident) => {
        impl<'a> Dup for $t<'a> {
            fn dup(&self) -> Self {
                $t { name: self.name }
            }
        }
    };
}
dup_by_derive!(Named);
";
        const BY_HAND: &str = "pub trait Dup: Sized {
    fn dup(&self) -> Self;
}
pub struct Named<'a> {
    name: &'a u8,
}
impl<'a> Dup for Named<'a> {
    fn dup(&self) -> Self {
        Named { name: self.name }
    }
}
";
        // The one impl of the app crate, and its `dup`.
        let the_impl = |names: crate::front2::tables::head_tests::Src| -> (Want, ItemKey) {
            let mut found = None;
            NamesSource::each_item(names, 0, &mut |key, _| {
                if NamesSource::text_of(names, key).is_some_and(|t| t.impl_header.is_some()) {
                    assert!(found.replace(key).is_none(), "the fixture has one impl");
                }
            });
            let lib = found.unwrap_or_else(|| panic!("the fixture's impl: the expansion's items were not served"));
            (Want::MemberFn(lib, member_key_of(checker_of(lib), MemberKind::Fn, "dup")), member_key_of(checker_of(lib), MemberKind::Fn, "dup"))
        };
        let derived = sig_of(DERIVED, the_impl).unwrap_or_else(|e| panic!("ORACLE: the expansion-written `dup`'s regions: {e}"));
        let by_hand = sig_of(BY_HAND, the_impl).unwrap_or_else(|e| panic!("ORACLE: the hand-written `dup`'s regions: {e}"));
        // `&self`: the reference's own slot, then `Self`'s `'a`; the output: `Self`'s `'a`.
        assert!(derived.0.contains(&Region::Param(0)), "ORACLE: `Self`'s `'a` is not the impl's first lifetime: {:?}", derived.0);
        assert_eq!(derived, by_hand, "ORACLE: the expansion-written impl lays another column than the hand-written one");
        // CONTROL: the names answer no count for `Named`; a fresh store refuses the column.
        let (names, sized, frame) = world_of(DERIVED);
        names.plant_no_lifetimes(item(names, "Named"));
        let (w, key) = the_impl(names);
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        let ensured = store.ensure(names, &mut keys, w);
        assert!(store.regions.made(key).is_none(), "CONTROL: with `Named`'s count unanswered `dup`'s column was laid ({ensured:?})");
        // The refusal is the count's: the member's own walk (`Counts`), or the impl's header
        // walk (`Named<'a>` as its self type: `Counts` on the impl) leaving the member no
        // container (`NoContainer`).
        use crate::front2::head_record::Unlaid;
        let Want::MemberFn(lib, _) = w else { unreachable!("`the_impl` asks for a member fn") };
        let kind = store.region_gaps.get(&key).map(super::RegionGap::unlaid);
        let impl_kind = store.region_gaps.get(&checker_of(lib)).map(super::RegionGap::unlaid);
        assert!(
            kind == Some(Unlaid::Counts) || (kind == Some(Unlaid::NoContainer) && impl_kind == Some(Unlaid::Counts)),
            "CONTROL: the refusal is not the count's: `dup` {:?}, the impl {:?} ({ensured:?})",
            store.region_gaps.get(&key),
            store.region_gaps.get(&checker_of(lib)),
        );
        std::println!("dup: {derived:?} expansion-written and by hand; without the count: {kind:?}, the impl {impl_kind:?}");
    }

    /// std's `Mutex::lock` shape (the real two-crate fixture's `std_calls`, refused
    /// `LifetimeArgs` before the fix): `MutexGuard<'a, T: ?Sized + 'a>` declares one lifetime,
    /// written behind its own outer attribute (`#[rustc_assert_variance(covariant)] 'a`,
    /// library/std/src/sync/poison/mutex.rs:277), and `LockResult<T>` is an alias with one type
    /// parameter and none of its own; `fn lock(&self) -> LockResult<MutexGuard<'_, T>>` writes
    /// the guard's one lifetime as `'_`. Oracle: the items record counts the guard's 1 and the
    /// alias's 0 (blaze-file-items `declared_lifetimes`: an attribute before a parameter kept
    /// the walk off the parameter's start, and counted 0), and `lock`'s regions are laid in a
    /// fresh store. Control (rule 23): the same method writing the guard with TWO lifetimes is
    /// refused `LifetimeArgs` by name, and the making store's detail names the path, the
    /// declared 1 and the written 2 (`RegionGap::say`, rule 19), so the comparison is the one
    /// the oracle passes.
    #[test]
    fn a_mutex_shaped_lock_lays_its_regions() {
        // `#![no_std]`: the prelude is core's, where the fixture's `Sized` lives (without it the
        // prelude is std's, absent from this world, and `?Sized` did not resolve).
        const MUTEX: &str = "#![no_std]
pub struct Mutex<T: ?Sized> {
    data: T,
}
pub struct MutexGuard<
    #[rustc_assert_variance(covariant)] 'a,
    #[rustc_assert_variance(invariant)] T: ?Sized + 'a,
> {
    lock: &'a Mutex<T>,
}
pub enum Res<T, E> {
    Good(T),
    Bad(E),
}
pub struct PoisonError<T> {
    data: T,
}
pub type LockResult<T> = Res<T, PoisonError<T>>;
impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        loop {}
    }
}
";
        const WRITTEN: &str = "LockResult<MutexGuard<'_, T>>";
        const TWO: &str = "LockResult<MutexGuard<'_, '_, T>>";
        // The one impl's `lock`.
        let the_lock = |names: crate::front2::tables::head_tests::Src| -> (Want, ItemKey) {
            let mut found = None;
            NamesSource::each_item(names, 0, &mut |key, _| {
                if NamesSource::text_of(names, key).is_some_and(|t| t.impl_header.is_some()) {
                    assert!(found.replace(key).is_none(), "the fixture has one impl");
                }
            });
            let lib = found.unwrap_or_else(|| panic!("the fixture's impl"));
            let mk = member_key_of(checker_of(lib), MemberKind::Fn, "lock");
            (Want::MemberFn(lib, mk), mk)
        };
        let (names, _, _) = world_of(MUTEX);
        assert_eq!(NamesSource::declared_lifetimes(names, item(names, "MutexGuard")), Some(1), "ORACLE: the record counts `MutexGuard`'s attributed `'a`");
        assert_eq!(NamesSource::declared_lifetimes(names, item(names, "LockResult")), Some(0), "ORACLE: `LockResult<T>` declares no lifetime");
        let laid = sig_of(MUTEX, the_lock).unwrap_or_else(|e| panic!("ORACLE: `Mutex::lock`'s regions: {e}"));
        assert!(!laid.0.is_empty(), "ORACLE: `lock`'s signature holds `&self`'s and the guard's slots: {laid:?}");
        // CONTROL: the guard written with two lifetimes.
        let two = MUTEX.replace(WRITTEN, TWO);
        let two: &'static str = std::boxed::Box::leak(two.into_boxed_str());
        let (names, sized, frame) = world_of(two);
        let (w, key) = the_lock(names);
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        let ensured = store.ensure(names, &mut keys, w);
        assert!(store.regions.made(key).is_none(), "CONTROL: `MutexGuard<'_, '_, T>` was laid ({ensured:?})");
        let gap = store.region_gaps.get(&key).unwrap_or_else(|| panic!("CONTROL: no gap filed for `lock` ({ensured:?})"));
        assert_eq!(gap.unlaid(), crate::front2::head_record::Unlaid::LifetimeArgs, "CONTROL: not refused `LifetimeArgs`: {gap:?}");
        let text = store.fns.get(&key).map(|f| f.text).unwrap_or_else(|| panic!("CONTROL: `lock`'s head was not filed whole"));
        let mut said = String::new();
        gap.say(text, &mut said).unwrap_or_else(|e| panic!("say: {e}"));
        assert!(said.contains("MutexGuard<'_, '_, T>") && said.contains("declares 1") && said.contains("written with 2"), "CONTROL: the detail does not name the path and both counts: {said}");
        std::println!("lock: {laid:?}; written with two: {said}");
    }

    /// zerovec's header (tempo, 1,076 bodies): `impl<T: AsULE> AsRef<ZeroSlice<T>> for
    /// &[T::ULE]`, an elided header lifetime over a projection. Its lifetime part counts the
    /// self type in its own grain (one `Hidden` position for the projection) and the regions in
    /// the region counter's (`T::ULE` has no slot), so the two self lengths differ and both are
    /// right. Oracle: the impl's regions made fresh and the ones a store files from its record
    /// are `same`, and the record's head is filed. Control (rule 23): the old restore rule (the
    /// two self lengths equal) refuses this record, so it was the cause; and a planted
    /// disagreement on the group (a lifetime part declaring more than the regions count) files
    /// the head whole and refuses only its regions, by name (`UnreadWhy::Refused`).
    #[test]
    fn an_elided_header_over_a_projection_restores_its_regions() {
        use crate::front2::head_record::{self, Head, HeadAt};
        use blaze_typeck::region_facts::UnreadWhy;
        const ZEROVEC: &str = "pub trait AsULE {
    type ULE;
}
pub struct ZeroSlice<T> { t: T }
pub trait AsRef2<T> {}
impl<T: AsULE> AsRef2<ZeroSlice<T>> for &[T::ULE] {}
";
        let (names, sized, frame) = world_of(ZEROVEC);
        let mut lib = None;
        for w in cells(names).iter() {
            if let Want::Impl(k) = *w {
                lib = Some(k);
            }
        }
        let lib = lib.expect("the impl's cell");
        let ck = checker_of(lib);
        let mut fresh = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        fresh.ensure(names, &mut keys, Want::Impl(lib)).unwrap_or_else(|e| panic!("ensure the impl: {e:?}"));
        fresh.make_lives(names, &keys, &[HeadAt::Impl(ck)]);
        let made = fresh.regions.made(ck).unwrap_or_else(|| panic!("ORACLE: the impl's regions were not made fresh: {:?}", fresh.region_gaps.get(&ck)));
        let bytes = head_record::encode(head_record::from_store(&fresh, HeadAt::Impl(ck)).unwrap_or_else(|e| panic!("from the store: {e}")), fresh.tys.view(), a())
            .unwrap_or_else(|e| panic!("encode: {e}"));
        let open = |store: &Store<'_>| head_record::decode(&bytes, None, &store.tys, a()).unwrap_or_else(|e| panic!("decode: {e}"));
        // CONTROL: the old restore rule compared the two self lengths, and they differ here.
        {
            let probe = Store::new(sized, frame, a());
            let Head::Impl { life: Some(Ok(l)), regions: Some(r), .. } = open(&probe) else {
                panic!("ORACLE: the impl's record holds no lifetime part or no regions")
            };
            let (self_col, _) = r.regions().impl_header.as_ref().expect("the impl's header columns");
            assert_ne!(self_col.as_slice().len(), l.self_regions.len(), "CONTROL: the two self lengths agree, so the old rule was not the cause");
        }
        // ORACLE: restored, the head is filed and its regions are the fresh ones.
        let restored = Store::new(sized, frame, a());
        let opened = open(&restored);
        restored.file_heads(Want::Impl(lib), [(HeadAt::Impl(ck), opened)]).unwrap_or_else(|e| panic!("ORACLE: the impl's record failed its head: {e:?}"));
        assert!(restored.impls.contains_key(&ck), "ORACLE: the impl's head was not filed");
        let back = restored.regions.made(ck).unwrap_or_else(|| panic!("ORACLE: restored, the impl's regions: {}", restored.regions.unread(ck)));
        assert_eq!(back.regions().params.count(), made.regions().params.count(), "ORACLE: the impl's group made fresh and restored differ");
        // Across two stores the encodings are compared (`same` is within one table): the
        // restored store writes the fresh store's bytes (rule 26).
        let again = head_record::encode(head_record::from_store(&restored, HeadAt::Impl(ck)).unwrap_or_else(|e| panic!("from the restored store: {e}")), restored.tys.view(), a())
            .unwrap_or_else(|e| panic!("re-encode: {e}"));
        assert_eq!(&again[..], &bytes[..], "ORACLE: the impl's record restored and written again is other bytes (rule 26)");
        // CONTROL: a disagreement planted on the group files the head and refuses the regions.
        let planted = Store::new(sized, frame, a());
        let mut opened = open(&planted);
        if let Head::Impl { life: Some(Ok(l)), .. } = &mut opened {
            l.lifetimes += 4;
        }
        planted.file_heads(Want::Impl(lib), [(HeadAt::Impl(ck), opened)]).unwrap_or_else(|e| panic!("CONTROL: a regions disagreement failed the head: {e:?}"));
        assert!(planted.impls.contains_key(&ck), "CONTROL: the planted impl's head was not filed");
        let why = planted.regions.unread(ck).why;
        assert!(matches!(why, UnreadWhy::Refused { .. }), "CONTROL: the planted disagreement was not refused by name: {why:?}");
    }

    const ELIDED_BOUNDS: &str = "pub trait Tr {}
pub fn o(x: &u8) -> impl Tr + '_ { loop {} }
pub fn n() -> impl Tr + '_ { loop {} }
pub fn w<T>(t: T) where T: '_ {}
pub fn d(x: &u8) -> &(dyn Tr + '_) { loop {} }
pub fn p<'a>(x: &u8) -> impl Tr + '_ { loop {} }
";

    /// `'_` in a return opaque's bound is the output rule's region (tempo tt_2d_pool.rs:736,
    /// `-> impl Iterator<Item = ..> + '_`): `o`'s opaque has `TypeOutlives(_, Param(0))`, `x`'s
    /// elided lifetime, and its regions are laid. CONTROL (the cause removed: no input lifetime
    /// for the rule to take): `n`'s head is made, its opaque's outlives clause is left out and
    /// its regions are refused by name, never the head's error. `'_` in a where clause (E0637)
    /// is the same soft refusal; `dyn Tr + '_` in a return type is elided by the output rule.
    #[test]
    fn an_elided_bound_takes_the_output_rule() {
        use blaze_typeck::PClause;
        let (names, sized, frame) = world_of(ELIDED_BOUNDS);
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        // A fn's return opaques are numbered from 0, from 1 in an `async fn` (opaque 0 is its
        // future): `fn_head`'s `first`. These fns are not async.
        let opaque_of = |name: &str| -> (ItemKey, ItemKey) {
            let k = checker_of(item(names, name));
            (k, crate::front2::keys::opaque_key(k, 0))
        };
        for name in ["o", "n", "w", "d", "p"] {
            store.ensure(names, &mut keys, Want::Fn(item(names, name))).unwrap_or_else(|e| panic!("ensure `{name}`: {e:?}"));
            let (fk, _) = opaque_of(name);
            assert!(store.fn_gaps.get(&fk).is_none(), "ORACLE: `{name}`'s head was refused: {:?}", store.fn_gaps.get(&fk));
        }
        let outlives = |k: ItemKey| -> Vec<Region> {
            let p = store.preds.get(&k).unwrap_or_else(|| panic!("the opaque's predicates"));
            p.clauses.iter().filter_map(|c| if let PClause::TypeOutlives { region, .. } = c { Some(*region) } else { None }).collect()
        };
        let (_, ok) = opaque_of("o");
        assert_eq!(outlives(ok), [Region::Param(0)], "ORACLE: `o`'s `+ '_` is not `x`'s lifetime");
        assert!(store.regions.made(ok).is_some(), "ORACLE: `o`'s opaque's regions: {}", store.regions.unread(ok));
        // CONTROL: no input lifetime, no region; the head stands and the regions say why.
        let (_, nk) = opaque_of("n");
        assert!(outlives(nk).is_empty(), "CONTROL: `n`'s `+ '_` was given a region");
        assert!(store.regions.made(nk).is_none(), "CONTROL: `n`'s opaque's regions were laid");
        let (wk, _) = opaque_of("w");
        assert!(store.regions.made(wk).is_none(), "CONTROL: `w`'s `T: '_` (E0637) was laid");
        // `p`'s opaque names one of its two lifetimes (the elided `Param(1)`, not `'a`): under
        // 2021 it captures that one alone (RFC 3498, "Rust 2021 and earlier"), a proper subset
        // now laid as the list it is (types.rs `Captures`), never `'a` with it. `'_` is still
        // `x`'s, and the clause is there (asked after the walk, it was dropped).
        let (_, pk) = opaque_of("p");
        assert_eq!(outlives(pk), [Region::Param(1)], "ORACLE: `p`'s `+ '_` lost `x`'s lifetime to its return type's own refusal");
        assert!(store.regions.made(pk).is_some(), "ORACLE: `p`'s opaque captures `x`'s lifetime alone and its regions are laid: {}", store.regions.unread(pk));
        assert_eq!(store.maps.opaque_lifetimes.get(&pk).copied(), Some(1), "ORACLE: `p`'s opaque captures one lifetime");
        let (psig, _) = sig_of(ELIDED_BOUNDS, free("p")).unwrap_or_else(|e| panic!("`p`'s regions: {e}"));
        assert!(!psig.contains(&Region::Param(0)), "CONTROL: `p`'s signature column holds `'a`, which nothing in it names or captures: {psig:?}");
        assert_eq!(psig.last().copied(), Some(Region::Param(1)), "ORACLE: `p`'s opaque slot is `x`'s lifetime: {psig:?}");
        // `-> &(dyn Tr + '_)`: the reference's and the object's lifetimes are both `x`'s.
        let (sig, _) = sig_of(ELIDED_BOUNDS, free("d")).unwrap_or_else(|e| panic!("`d`'s regions: {e}"));
        assert!(sig.len() >= 2 && sig.iter().all(|&r| r == Region::Param(0)), "ORACLE: `d`'s signature column: {sig:?}");
    }

    /// An impl header naming a raw identifier (`impl<T> Tr for r#return<T>`, and an inherent
    /// impl of it) is made: its path's segment is `return` (types.rs `path_texts_of`, as
    /// blaze-scope lower.rs `unraw` files the name). CONTROL: the raw text as a segment
    /// resolves to nothing, the path that "did not resolve" before.
    #[test]
    fn a_raw_identifier_in_an_impl_header_resolves() {
        use crate::resolve::{Ns, Paths};
        const RAW: &str = "pub trait Tr {}
pub struct r#return<T> { t: T }
impl<T> Tr for r#return<T> {}
impl<T> r#return<T> {
    pub fn get(&self) -> &T { &self.t }
}
";
        let (names, sized, frame) = world_of(RAW);
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        let mut impls = 0;
        for w in cells(names).iter() {
            if let Want::Impl(k) = *w {
                store.ensure(names, &mut keys, Want::Impl(k)).unwrap_or_else(|e| panic!("ORACLE: the impl over `r#return`: {e:?}"));
                assert!(store.impls.contains_key(&checker_of(k)), "ORACLE: the impl over `r#return` was not filed");
                impls += 1;
            }
        }
        assert_eq!(impls, 2, "ORACLE: the fixture's two impls");
        let root = NamesSource::root_module(names, 0).expect("the app crate's root");
        let paths = Paths { names };
        assert!(paths.path(root, false, &["return"], Ns::Type).is_ok(), "ORACLE: `return` names the struct");
        assert!(paths.path(root, false, &["r#return"], Ns::Type).is_err(), "CONTROL: the raw text resolved; the test does not see the cause");
    }

    #[test]
    fn record_sorting_union_flatten_dedup_and_fallible_lowering() {
        use crate::front2::buckets;
        use crate::front2::types::{Bounds, Builder, Env, SelfTy};
        use crate::resolve::Paths;
        use blaze_lower::{Cfg, CfgAnswer, form as lf, head};
        use blaze_typeck::{IntTy, Name, PNode, RecordSchema, Shape, UintTy};

        struct NoCfg;
        impl Cfg for NoCfg {
            fn atom(&mut self, name: &str, _value: Option<&str>) -> CfgAnswer {
                panic!("unexpected cfg atom `{name}`")
            }
        }

        let alloc = blaze_own::testing::heap().alloc();
        let exts = parser::Extensions::RECORDS.with(parser::Extensions::ERROR_UNIONS);

        const SRC: &str = "
pub struct A;
pub struct B;
";
        let (names, sized, frame) = world_of(SRC);
        let mut store = Store::new(sized, frame, a());
        let mut keys = Keys::new();
        for name in ["A", "B"] {
            let lib = item(names, name);
            store.ensure(names, &mut keys, Want::Adt(lib)).unwrap_or_else(|e| panic!("ensure `{name}`: {e:?}"));
        }
        let root = NamesSource::root_module(names, 0).expect("root module");
        let env = Env {
            module: root,
            params: &[],
            bounds: Bounds::Done(&[]),
            self_: SelfTy::None,
            self_assoc: &[],
            anon: &[],
            consts: &[],
            body: None,
        };

        let lower_param_ty = |store: &mut Store<'_>, sig_text: &str| -> blaze_typeck::PTy {
            let low = blaze_lower::lower_signature_with_extensions(sig_text, blaze_scope::Edition::E2024, exts, &mut NoCfg, alloc)
                .unwrap_or_else(|e| panic!("lower `{sig_text}`: {e:?}"));
            let v = low.view();
            assert!(v.diags.is_empty(), "diags for `{sig_text}`: {:?}", v.diags);
            let row = v.rows(v.list(head::PARAMS)).expect("params")[0];
            let p = lf::Param::decode(row).expect("param");
            let ty_id = p.ty.expect("param type");
            let b = Builder { resolver: Paths { names }, heads: &store.maps, marks: None };
            b.ty(&mut store.tys, &env, &v, ty_id).unwrap_or_else(|e| panic!("Builder::ty `{sig_text}`: {e:?}"))
        };

        // 1. `{ b: u8, a: i32 }` and `{ a: i32, b: u8 }` lower to identical `PNode::Record`.
        let rec_ba_ty = lower_param_ty(&mut store, "pub fn f(x: { b: u8, a: i32 });");
        let rec_ab_ty = lower_param_ty(&mut store, "pub fn f(x: { a: i32, b: u8 });");
        let rec_xy_ty = lower_param_ty(&mut store, "pub fn f(x: { x: i32, y: u8 });");
        let uni_ty = lower_param_ty(&mut store, "pub fn f(x: A | (B | A));");

        let tys = store.tys.view();
        assert_eq!(rec_ba_ty, rec_ab_ty, "`{{ b: u8, a: i32 }}` and `{{ a: i32, b: u8 }}` must lower to identical PTy");
        let expected_ab_schema = RecordSchema::of_sorted_names(&[Name::of("a"), Name::of("b")]);
        let PNode::Record(ab_schema, ab_fields) = tys.node(rec_ba_ty).expect("rec_ba node") else {
            panic!("expected PNode::Record, got {:?}", tys.node(rec_ba_ty));
        };
        assert_eq!(ab_schema, expected_ab_schema);
        let ab_list = tys.list(ab_fields).expect("ab fields");
        assert_eq!(ab_list.len(), 2);
        assert_eq!(tys.node(ab_list[0]).expect("a field"), PNode::Int(IntTy::I32));
        assert_eq!(tys.node(ab_list[1]).expect("b field"), PNode::Uint(UintTy::U8));
        assert_eq!(buckets::shape_of(tys, rec_ba_ty), Some(Shape::Record(expected_ab_schema.0, 2)));

        // 2. `{ x: i32, y: u8 }` and `{ a: i32, b: u8 }` lower to distinct `PNode::Record` (different RecordSchema).
        let PNode::Record(xy_schema, _) = tys.node(rec_xy_ty).expect("rec_xy node") else {
            panic!("expected PNode::Record for rec_xy");
        };
        assert_ne!(xy_schema, ab_schema, "distinct field names must yield distinct RecordSchema");
        assert_ne!(rec_xy_ty, rec_ba_ty, "`{{ x: i32, y: u8 }}` must be distinct from `{{ a: i32, b: u8 }}`");

        // 3. `A | (B | A)` flattens and deduplicates cleanly to `PNode::Union([A, B])`.
        let PNode::Union(uni_members) = tys.node(uni_ty).expect("uni node") else {
            panic!("expected PNode::Union, got {:?}", tys.node(uni_ty));
        };
        let members = tys.list(uni_members).expect("uni members");
        assert_eq!(members.len(), 2, "`A | (B | A)` must flatten and deduplicate to 2 members");
        let a_key = checker_of(item(names, "A"));
        let b_key = checker_of(item(names, "B"));
        assert!(matches!(tys.node(members[0]), Ok(PNode::Adt(k, _)) if k == a_key));
        assert!(matches!(tys.node(members[1]), Ok(PNode::Adt(k, _)) if k == b_key));
        assert_eq!(buckets::shape_of(tys, uni_ty), Some(Shape::Union(2)));
    }

    #[test]
    fn record_spread_canonical_schema_override_regions_and_error() {
        use blaze_id::{ContentId, CrateItemKey};
        use blaze_scope::graph::Sysroot;
        use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
        use blaze_scope::task::Serial;
        use blaze_surface::Edition;
        use blaze_typeck::{IntTy, Name, PNode, RecordSchema, UintTy};
        use blaze_unit_graph::{Implicit, Prelude, StdIs};

        const SRC: &str = "#![blaze::records]
#![feature(no_core, lang_items)]
#![no_core]

#[lang = \"sized\"]
trait Sized {}

pub type Transform = { x: i32, y: i32, scale: i32 };
pub type Combat = { atk: u32, hp: u8 };
pub type Actor = { ..Transform, hp: u32 };
pub type FlatActor = { hp: u32, scale: i32, x: i32, y: i32 };
pub type Boss = { ..Actor, ..Combat, hp: u32, shield: u32 };
pub type FlatBoss = { atk: u32, hp: u32, scale: i32, shield: u32, x: i32, y: i32 };
pub type Bad = { ..u32, hp: u32 };
pub type WithRef<'a> = { name: &'a str, x: i32 };
pub type NamedActor<'a> = { ..WithRef<'a>, hp: u32 };
pub fn use_named<'a>(a: NamedActor<'a>) -> &'a str { a.name }
";
        struct SrcText(&'static str);
        impl<'a> SourceTexts<'a> for SrcText {
            fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
                (krate == 0 && path == "/app/lib.rs").then_some(self.0)
            }
        }
        let g = a();
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let bump = frame.alloc(blaze_own::Erased::new(&frame)).bump();
        let crates = [CrateInput {
            name: "app",
            key: CrateItemKey(ContentId::of("record spread test", b"app")),
            edition: Edition::E2024,
            implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2024), proc_macro: false, entry: false },
            root: "/app/lib.rs",
            shown_from: "/",
            features: &[],
            deps: &[],
            cfg: &[],
            script: None,
        }];
        let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
        let solved = resolve_crates(&crates, sysroot, &SrcText(SRC), &Serial, bump).expect("resolve_crates");
        let names = &solved;
        let find = |wanted: &str| -> LibKey {
            let mut found = None;
            NamesSource::each_item(names, 0, &mut |key, _| {
                if NamesSource::name_of(names, key) == Some(wanted) {
                    found = Some(key);
                }
            });
            found.unwrap_or_else(|| panic!("`{wanted}` in fixture"))
        };
        let sized = Some(find("Sized"));
        let mut store = Store::new(sized, &frame, g);
        let mut keys = Keys::new();

        for name in ["Actor", "FlatActor", "Boss", "FlatBoss", "NamedActor"] {
            let lib = find(name);
            store.ensure(names, &mut keys, Want::Head(lib)).unwrap_or_else(|e| panic!("ensure `{name}`: {e:?}"));
        }
        let use_named_key = find("use_named");
        store.ensure(names, &mut keys, Want::Fn(use_named_key)).unwrap_or_else(|e| panic!("ensure `use_named`: {e:?}"));

        let actor_ty = store.maps.aliases.get(&find("Actor")).copied().flatten().expect("Actor alias").1;
        let flat_actor_ty = store.maps.aliases.get(&find("FlatActor")).copied().flatten().expect("FlatActor alias").1;
        assert_eq!(actor_ty, flat_actor_ty, "`{{ ..Transform, hp: u32 }}` must equal `{{ hp: u32, scale: i32, x: i32, y: i32 }}`");

        let tys = store.tys.view();
        let PNode::Record(actor_schema, actor_fields) = tys.node(actor_ty).expect("actor node") else {
            panic!("expected PNode::Record for Actor");
        };
        let expected_actor_schema = RecordSchema::of_sorted_names(&[Name::of("hp"), Name::of("scale"), Name::of("x"), Name::of("y")]);
        assert_eq!(actor_schema, expected_actor_schema);
        let af = tys.list(actor_fields).expect("actor fields");
        assert_eq!(af.len(), 4);
        assert_eq!(tys.node(af[0]).expect("hp"), PNode::Uint(UintTy::U32));
        assert_eq!(tys.node(af[1]).expect("scale"), PNode::Int(IntTy::I32));
        assert_eq!(tys.node(af[2]).expect("x"), PNode::Int(IntTy::I32));
        assert_eq!(tys.node(af[3]).expect("y"), PNode::Int(IntTy::I32));

        // Multi-spread + field override (`hp: u32` overrides `Combat`'s `hp: u8` without duplicating `hp`).
        let boss_ty = store.maps.aliases.get(&find("Boss")).copied().flatten().expect("Boss alias").1;
        let flat_boss_ty = store.maps.aliases.get(&find("FlatBoss")).copied().flatten().expect("FlatBoss alias").1;
        assert_eq!(boss_ty, flat_boss_ty, "`Boss` must canonically equal `FlatBoss`");

        // Lifetime-bearing spread lays regions cleanly without region gaps.
        assert!(store.maps.alias_slots.get(&find("NamedActor")).is_some(), "NamedActor<'a> must lay alias region slots");
        assert!(store.region_gaps.get(&checker_of(use_named_key)).is_none(), "use_named must have no region gap");

        // Non-record spread `..u32` is rejected cleanly in `alias_gaps`.
        let bad_key = find("Bad");
        store.ensure(names, &mut keys, Want::Head(bad_key)).unwrap_or_else(|e| panic!("ensure `Bad`: {e:?}"));
        assert!(store.maps.aliases.get(&bad_key).copied().flatten().is_none(), "`Bad` must be refused");
        let gap = store.maps.alias_gaps.get(&bad_key).expect("Bad alias gap");
        assert!(gap.text.contains("record type spread"), "unexpected gap message: {}", gap.text);
    }

    #[test]
    fn record_spread_defaults_and_nested_elision_end_to_end() {
        use crate::refusal::SourceFile;
        use crate::run::{FrontChoice, NewFront, run_with};

        const NEW: NewFront<'static> = NewFront {
            package: "ladder",
            edition: "blaze-2026.11",
            shelf: None,
            overflow: blaze_build::OverflowChecks::Off,
        };
        const HEAD: &str = "#![blaze::records]
#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for i32 {}
impl Copy for bool {}
";

        let run_src = |body: &str| -> Result<u8, blaze_own::String<blaze_own::Alloc<'static>>> {
            let alloc = blaze_own::testing::heap().alloc();
            let mut text = blaze_own::String::new_in(alloc);
            text.push_str(HEAD);
            text.push_str(body);
            let files = [SourceFile { path: "src/main.rs", text: text.as_str() }];
            run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| {
                let mut msg = blaze_own::String::new_in(alloc);
                let _ = core::fmt::Write::write_fmt(&mut msg, format_args!("{e}"));
                msg
            })
        };

        // 1. Structural record spread with inherited, overridden, and multi-spread defaults,
        //    plus flat structural compatibility and width-subtyping to base record.
        let spread_ok = run_src("
type Transform = { x: i32 = 0, y: i32 = 0, scale: i32 = 1 };
type Combat = { atk: u32 = 10, hp: u32 = 50 };
type Actor = { ..Transform, hp: u32 = 100 };
type Boss = { ..Actor, ..Combat, hp: u32 = 200, shield: u32 = 15 };

fn take_flat(r: { hp: u32, scale: i32, x: i32, y: i32 }) -> i32 {
    r.x + r.y + r.scale + r.hp as i32
}

fn take_transform(t: Transform) -> i32 {
    t.x + t.y + t.scale
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a: Actor = { x: 10 };
    let b: Boss = { y: 5 };
    // take_flat(a) = 10 + 0 + 1 + 100 = 111
    // take_transform(b) = 0 + 5 + 1 = 6
    // b.atk (10) + b.shield (15) = 25
    // b.hp (200) -> use delta (b.hp - 180 = 20) so total fits in exit status u8: 111 + 6 + 25 + 20 = 162
    take_flat(a) + take_transform(b) + (b.atk + b.shield + (b.hp - 180)) as i32
}
");
        assert_eq!(spread_ok, Ok(162));

        // 2. Re-declaring a field without a default clears the inherited default.
        let cleared_err = run_src("
type Transform = { x: i32 = 0, y: i32 = 0, scale: i32 = 1 };
type Actor = { ..Transform, hp: u32 = 100 };
type RequiredHp = { ..Actor, hp: u32, tag: u8 = 7 };

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: RequiredHp = { x: 1 };
    r.x
}
");
        assert!(cleared_err.is_err(), "omitting cleared-default field `hp` must fail typecheck");

        let cleared_ok = run_src("
type Transform = { x: i32 = 0, y: i32 = 0, scale: i32 = 1 };
type Actor = { ..Transform, hp: u32 = 100 };
type RequiredHp = { ..Actor, hp: u32, tag: u8 = 7 };

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: RequiredHp = { hp: 34 };
    r.x + r.y + r.scale + r.hp as i32 + r.tag as i32
}
");
        assert_eq!(cleared_ok, Ok(42));

        // 3. Nested partial record default elision:
        //    `{ transform: { x: 10 } }`, `{ transform: { .. }, hp: 20 }`, `{ .. }`,
        //    `{ transform: { x: 10, ..base.transform }, ..base }`, `{ transform: { x: 7 }, ..base }`.
        let nested_ok = run_src("
type Transform = { x: i32 = 0, y: i32 = 0, scale: i32 = 1 };
type Entity = { transform: Transform = { x: 0, y: 0, scale: 1 }, hp: u32 = 100 };

fn score(e: Entity) -> i32 {
    e.transform.x + e.transform.y + e.transform.scale + e.hp as i32
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let e1: Entity = { transform: { x: 10 } };
    let e2: Entity = { transform: { .. }, hp: 20 };
    let e3: Entity = { .. };
    let base: Entity = { transform: { x: 3, y: 4, scale: 2 }, hp: 10 };
    let e4: Entity = { transform: { x: 10, ..base.transform }, ..base };
    let e5: Entity = { transform: { x: 7 }, ..base };
    // score(e1) = 10 + 0 + 1 + 100 = 111
    // score(e2) = 0 + 0 + 1 + 20 = 21
    // score(e3) = 0 + 0 + 1 + 100 = 101
    // score(e4) = 10 + 4 + 2 + 10 = 26
    // score(e5) = 7 + 0 + 1 + 10 = 18
    // total - 200 = 277 - 200 = 77
    score(e1) + score(e2) + score(e3) + score(e4) + score(e5) - 200
}
");
        assert_eq!(nested_ok, Ok(77));
    }
}
