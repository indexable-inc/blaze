//! The type system. We currently use this to infer types for completion, hover
//! information and various assists.

#![feature(allocator_api, allocator_ext)]
// It's useful to refer to code that is private in doc comments.
#![allow(rustdoc::private_intra_doc_links)]

extern crate ra_ap_rustc_ast_ir as rustc_ast_ir;
extern crate ra_ap_rustc_index as rustc_index;
extern crate rustc_abi;
extern crate rustc_next_trait_solver;
extern crate rustc_type_ir;

extern crate self as hir_ty;

pub mod builtin_derive;
pub mod comptime;
pub mod comptime_env;
pub mod generics;
pub mod infer;
pub mod inhabitedness;
pub mod intrinsic;
mod lower;
pub mod next_solver;
mod opaques;
mod representability;
pub mod rpitit;
mod specialization;
mod target_feature;
pub mod utils;
mod variance;

pub mod arbitrary_ints;
pub mod autoderef;
pub mod body_store;
pub mod check_cache;
pub mod const_param_ty;
pub mod const_valtree;
pub mod consteval;
pub mod coroutine_obligations;
pub mod db;
pub mod diagnostics;
pub mod display;
pub mod drop;
pub mod dropck;
pub mod dyn_compatibility;
pub mod effects;
pub mod evict;
pub mod facts;
pub mod fn_cache;
pub mod lang_items;
pub mod layout;
pub mod method_resolution;
pub mod mir;
pub mod primitive;
pub mod providers;
pub mod record;
pub mod solver_errors;
pub mod traits;
pub mod used_imports;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use std::{hash::Hash, ops::ControlFlow};

use base_db::Db;
use blaze_hash::{FixedState, HashMap, HashSet};
use blaze_mem::Frame;
use hir_def::{
    CallableDefId, ConstId, DefWithBodyId, EnumVariantId, ExpressionStoreOwnerId, FunctionId,
    GenericDefId, HasModule, LifetimeParamId, ModuleId, StaticId, TypeAliasId, TypeOrConstParamId,
    TypeParamId,
    expr_store::{Body, ExpressionStore},
    hir::{BindingId, ExprId, ExprOrPatId, ExprOrPatIdPacked, PatId},
    resolver::{HasResolver, Resolver, TypeNs},
    type_ref::{Rawness, TypeRefId},
};
use hir_expand::name::Name;
use macros::GenericTypeVisitable;
use mir::{MirEvalError, VTableMap};
use rustc_abi::ExternAbi;
use rustc_type_ir::{
    BoundVarIndexKind, TypeSuperVisitable, TypeVisitableExt,
    inherent::{IntoKind, Ty as _},
};
use std::alloc::Global;
use stdx::impl_from;
use traits::FnTrait;

use crate::{
    db::{AnonConstId, HirDatabase},
    lower::SupertraitsInfo,
    next_solver::{
        AliasTy, Binder, BoundConst, BoundRegion, BoundRegionKind, BoundTy, BoundTyKind, Canonical,
        CanonicalVarKind, CanonicalVarKinds, ClauseKind, Const, ConstExt, ConstKind, DbInterner,
        GenericArgs, IsRigid, PolyFnSig, Region, RegionExt, RegionKind, TraitRef, Ty, TyKind,
        TypingMode,
        abi::Safety,
        infer::{
            DbInternerInferExt,
            traits::{Obligation, ObligationCause},
        },
        obligation_ctxt::ObligationCtxt,
    },
};

pub use autoderef::autoderef;
pub use infer::{
    Adjust, Adjustment, AutoBorrow, BindingMode, ByRef, CallArguments, ExplicitDropMethodUseKind,
    InferenceDiagnostic, InferenceResult, OverloadedDeref, PointerCast, RecordProblem, ReturnKind,
    StructPathDef,
    StructPathFound, Unreachable, UnreachableAt,
    cast::{CastError, CastPointerKind, CastPointers},
    could_coerce, could_unify, could_unify_deeply, infer_query_with_inspect,
    prelude_lints::{EditionLint, ReceiverAdjustment},
};
pub use lower::{
    ElidedGroups, ElidedLifetimes, ElidedOutput, FieldType, FieldTypes, GenericDefaults,
    GenericDefaultsRef, GenericPredicates, LifetimeElisionKind, LifetimeLoweringMode, Lowered,
    LoweringMode, PredicateSource, SelfAndAssocBounds, TyDefId, TyLoweringContext,
    TyLoweringResult, ValueTyDefId,
    batch::{
        Batch, CrateFieldTypes, CrateImplPredicates, CrateImplSelfTys, CrateImplTraits, SideTable,
    },
    diagnostics::*,
    elided_groups, elided_lifetimes, hir_mutability, hybrid_param_env, lower_mutability,
    persist::{BatchKind, ColdBatch, ColdBatchRef, PredicatesRec},
    predicate_sources,
    record::{
        RecordParamDefault, record_defaults_owner, record_param_default, record_param_defaults,
    },
    solver_fn_sig, supertraits_cycle, with_elided_regions,
};
pub use opaques::{rpit_hidden_types, tait_hidden_types};
pub use specialization::{
    SpecializationMode, impl_unconstrained_params, specialization_mode, specialized_ancestor,
};
pub use target_feature::{
    TargetFeature, TargetFeatureSet, TargetFeatures, crate_target_features, multiversion_plan,
};
pub use traits::{
    ParamEnvAndCrate, SealedOpen, check_orphan_rules, declares_sealed, param_is_sized,
    relaxed_uncovered_input, sealed_bounds, sealed_closure, sealed_in_graph, sealed_local_bound,
    sealed_relaxation,
};
pub use used_imports::used_imports_with_traits;
pub use utils::{
    TargetFeatureIsSafeInTarget, Unsafety, all_super_traits, direct_super_traits,
    is_fn_unsafe_to_call, target_feature_is_safe_in_target,
};
pub use variance::{adt_unused_params, ctor_variances};

pub mod closure_analysis {
    pub use crate::infer::{
        CaptureInfo, CaptureSourceStack, CapturedPlace, ClosureData, MinCaptures, MinCapturesIter,
        UpvarCapture,
        closure::analysis::{
            BorrowKind,
            expr_use_visitor::{FakeReadCause, Place, PlaceBase, Projection, ProjectionKind},
        },
    };
}

/// A map of a query run's temporaries, in its frame (docs/v2/plan/design/memory.md, R3).
pub type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FixedState, &'f Frame>;
/// A set of a query run's temporaries, in its frame.
pub type FrameSet<'f, T> = hashbrown::HashSet<T, FixedState, &'f Frame>;

pub fn frame_map<K, V>(frame: &Frame) -> FrameMap<'_, K, V> {
    hashbrown::HashMap::with_hasher_in(FixedState, frame)
}

pub fn frame_set<T>(frame: &Frame) -> FrameSet<'_, T> {
    hashbrown::HashSet::with_hasher_in(FixedState, frame)
}

/// A constant can have reference to other things. Memory map job is holding
/// the necessary bits of memory of the const eval session to keep the constant
/// meaningful.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash, GenericTypeVisitable)]
pub enum MemoryMap<'db> {
    #[default]
    Empty,
    Simple(Box<[u8]>),
    Complex(Box<ComplexMemoryMap<'db>>),
}

#[derive(Debug, Default, Clone, PartialEq, Eq, GenericTypeVisitable)]
pub struct ComplexMemoryMap<'db> {
    pub memory: MemoryPieces,
    pub vtable: VTableMap<'db>,
    /// The address ranges that are the memory of a static, as start and size.
    pub statics: Vec<StaticMemory>,
    /// What the address of a piece of `memory` was a multiple of where it was made, for
    /// those where that is known: address and alignment, sorted by address.
    aligns: Vec<(usize, usize)>,
    /// The types of the `TypeId`s the constant may hold, by the ids the evaluation gave them:
    /// those are not rustc's hashes, which whoever emits the constant puts in their place.
    /// A field's id is its field representing type's, as in rustc, so the evaluator makes no
    /// `None`; it stays for maps decoded from stores written before (FIXME(comptime): narrow
    /// to `Vec<Ty>` in one port with its encoders).
    pub type_ids: Vec<Option<Ty<'db>>>,
    /// The offsets into the evaluated value's bytes of each word that is a pointer with
    /// provenance, sorted. A word not listed is an integer, even when it is typed as a pointer.
    value_pointers: Vec<usize>,
    /// The addresses, in this map's address space, of each word inside `memory`'s pieces that
    /// is a pointer with provenance, sorted.
    memory_pointers: Vec<usize>,
}

impl Hash for ComplexMemoryMap<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash_content(state);
        let ComplexMemoryMap { vtable, statics, type_ids, .. } = self;
        // `VTableMap`'s ids follow from the order of its entries, each a type and the `dyn`
        // type it is a vtable for.
        state.write_usize(vtable.tys().len());
        vtable.entries().for_each(|entry| entry.hash(state));
        statics.iter().for_each(|it| it.id.hash(state));
        type_ids.hash(state);
    }
}

impl ComplexMemoryMap<'_> {
    /// Feeds `state` every field of the map that is bytes and addresses, which is what the code
    /// generator reads of it beside types and statics: each piece's address and bytes, where
    /// each static's memory is and its size, each piece's alignment (its data object's), and
    /// which words of the value and of the pieces are pointers with provenance (each one it
    /// relocates). Left out, for the caller to hash by identity, are what names a type or an
    /// item by the database's ids: the table of functions and vtables, the types of the
    /// `TypeId`s and which static each range of memory is. The one place that says which fields
    /// are content, so a field added is hashed by every identity that reads the map (rule 21).
    /// Every number is written by `write_usize`, each column after its length, so a hasher
    /// that writes integers at a fixed width (`blaze_hash::Blake3Hasher`) gets the same bytes
    /// on every target.
    pub fn hash_content<H: std::hash::Hasher>(&self, state: &mut H) {
        let ComplexMemoryMap {
            memory,
            vtable: _,
            statics,
            aligns,
            type_ids: _,
            value_pointers,
            memory_pointers,
        } = self;
        state.write_usize(memory.len());
        for (address, bytes) in memory.iter() {
            state.write_usize(*address);
            state.write_usize(bytes.len());
            state.write(bytes);
        }
        state.write_usize(statics.len());
        for StaticMemory { start, size, id: _ } in statics {
            state.write_usize(*start);
            state.write_usize(*size);
        }
        state.write_usize(aligns.len());
        for (address, align) in aligns {
            state.write_usize(*address);
            state.write_usize(*align);
        }
        for column in [value_pointers, memory_pointers] {
            state.write_usize(column.len());
            column.iter().for_each(|it| state.write_usize(*it));
        }
    }
}

/// Where a word of an evaluated constant is: at an offset into its value's bytes, or at an
/// address inside its memory map's pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerAt {
    Value(usize),
    Memory(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StaticMemory {
    pub start: usize,
    pub size: usize,
    pub id: hir_def::StaticId,
}

// SAFETY: visits every field: it has three, a start, a size and a `StaticId`, none of which
// holds a type.
unsafe impl<V> rustc_type_ir::GenericTypeVisitable<V> for StaticMemory {
    fn generic_visit_with(&self, _visitor: &mut V) {}
}

/// The pieces of memory a constant points to, as columns: their bytes one after another in one
/// buffer, their addresses sorted, and where each one's bytes are. Pieces may overlap: a
/// reference into a value another piece holds is a piece of its own.
///
/// Cost: 24 bytes a piece beside its bytes (an address, 8, in the column searched; its extent,
/// 16, in a column read only once found). A lookup by address is a binary search of the
/// address column; one inside a piece also scans the pieces that start at most the longest
/// piece's length below it. Inserting in address order appends; out of order it moves the
/// columns' tails.
#[derive(Default, Clone)]
pub struct MemoryPieces {
    bytes: Vec<u8>,
    addresses: Vec<usize>,
    extents: Vec<Extent>,
    /// The length of the longest piece: no piece starting further below an address holds it.
    longest: usize,
}

/// Where the bytes of one piece of [`MemoryPieces`] are in its buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Extent {
    pub(crate) start: usize,
    pub(crate) len: usize,
}

const _: () = assert!(size_of::<Extent>() == 2 * size_of::<usize>());

impl MemoryPieces {
    /// Pieces from their columns: `addresses` strictly increasing, each extent in `bytes`.
    pub(crate) fn from_columns(
        bytes: Vec<u8>,
        addresses: Vec<usize>,
        extents: Vec<Extent>,
    ) -> Self {
        debug_assert!(addresses.is_sorted_by(|a, b| a < b) && addresses.len() == extents.len());
        debug_assert!(extents.iter().all(|it| it.start + it.len <= bytes.len()));
        let longest = extents.iter().map(|it| it.len).max().unwrap_or(0);
        MemoryPieces { bytes, addresses, extents, longest }
    }

    pub fn len(&self) -> usize {
        self.addresses.len()
    }

    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// Whether `bytes` holds each piece once, in address order, and nothing else.
    fn is_compact(&self) -> bool {
        let mut at = 0;
        self.extents.iter().all(|it| {
            let next = it.start == at;
            at += it.len;
            next
        }) && at == self.bytes.len()
    }

    /// The bytes of the piece `index` in the columns.
    fn piece(&self, index: usize) -> &[u8] {
        let Extent { start, len } = self.extents[index];
        &self.bytes[start..start + len]
    }

    fn position(&self, address: usize) -> Result<usize, usize> {
        self.addresses.binary_search(&address)
    }

    /// The piece at `address`.
    pub fn get(&self, address: &usize) -> Option<&[u8]> {
        let at = self.position(*address).ok()?;
        Some(self.piece(at))
    }

    pub fn contains_key(&self, address: &usize) -> bool {
        self.position(*address).is_ok()
    }

    /// Each piece with its address, lowest address first.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&usize, &[u8])> + DoubleEndedIterator {
        self.addresses.iter().enumerate().map(|(index, address)| (address, self.piece(index)))
    }

    /// The piece that holds `address`, with the address it starts at: the piece at `address`,
    /// or else the lowest one that holds it.
    fn holding(&self, address: usize) -> Option<(usize, &[u8])> {
        let end = match self.position(address) {
            Ok(at) => return Some((address, self.piece(at))),
            Err(end) => end,
        };
        let first =
            self.addresses[..end].partition_point(|&it| it.saturating_add(self.longest) <= address);
        (first..end)
            .find(|&index| address - self.addresses[index] < self.extents[index].len)
            .map(|index| (self.addresses[index], self.piece(index)))
    }

    /// The lowest piece with bytes that holds `address` or that `address` is one past the end
    /// of, with the address it starts at. A reference to nothing (an empty slice) keeps an
    /// empty piece at its address, which may be inside or at the end of the memory it points
    /// into; this finds that memory.
    fn around(&self, address: usize) -> Option<(usize, &[u8])> {
        let end = self.addresses.partition_point(|&it| it <= address);
        let first =
            self.addresses[..end].partition_point(|&it| it.saturating_add(self.longest) < address);
        (first..end)
            .find(|&index| {
                let len = self.extents[index].len;
                len > 0 && address - self.addresses[index] <= len
            })
            .map(|index| (self.addresses[index], self.piece(index)))
    }

    /// Keeps `bytes` at `address`, unless the piece already there is as long or longer.
    pub fn insert(&mut self, address: usize, bytes: &[u8]) {
        match self.position(address) {
            Ok(at) => {
                let extent = &mut self.extents[at];
                if extent.len >= bytes.len() {
                    return;
                }
                if extent.start + extent.len == self.bytes.len() {
                    self.bytes.truncate(extent.start);
                } else {
                    extent.start = self.bytes.len();
                }
                extent.len = bytes.len();
            }
            Err(at) => {
                self.addresses.insert(at, address);
                self.extents.insert(at, Extent { start: self.bytes.len(), len: bytes.len() });
            }
        }
        self.longest = self.longest.max(bytes.len());
        self.bytes.extend_from_slice(bytes);
    }

    /// The bytes of every piece, lowest address first, and nothing else: what a piece that
    /// grew left behind is dropped.
    pub fn shrink_to_fit(&mut self) {
        if !self.is_compact() {
            let mut bytes = Vec::with_capacity(self.extents.iter().map(|it| it.len).sum());
            for extent in &mut self.extents {
                let start = bytes.len();
                bytes.extend_from_slice(&self.bytes[extent.start..extent.start + extent.len]);
                extent.start = start;
            }
            self.bytes = bytes;
        }
        self.bytes.shrink_to_fit();
        self.addresses.shrink_to_fit();
        self.extents.shrink_to_fit();
    }

    /// The buffer when it holds each piece once in address order, as [`Self::shrink_to_fit`]
    /// leaves it: what a writer copies in one go.
    pub(crate) fn compact_bytes(&self) -> Option<&[u8]> {
        self.is_compact().then_some(&*self.bytes)
    }

    pub(crate) fn addresses(&self) -> &[usize] {
        &self.addresses
    }
}

impl PartialEq for MemoryPieces {
    fn eq(&self, other: &Self) -> bool {
        self.addresses == other.addresses
            && (0..self.len()).all(|index| self.piece(index) == other.piece(index))
    }
}

impl Eq for MemoryPieces {}

impl Hash for MemoryPieces {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addresses.hash(state);
        (0..self.len()).for_each(|index| self.piece(index).hash(state));
    }
}

impl std::fmt::Debug for MemoryPieces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

// SAFETY: visits every field: its columns hold bytes, addresses and extents, none of which is
// a type.
unsafe impl<V> rustc_type_ir::GenericTypeVisitable<V> for MemoryPieces {
    fn generic_visit_with(&self, _visitor: &mut V) {}
}

impl ComplexMemoryMap<'_> {
    pub fn insert(&mut self, addr: usize, val: &[u8]) {
        self.memory.insert(addr, val);
    }

    pub fn note_align(&mut self, addr: usize, align: usize) {
        match self.aligns.binary_search_by_key(&addr, |it| it.0) {
            Ok(at) => self.aligns[at].1 = align,
            Err(at) => self.aligns.insert(at, (addr, align)),
        }
    }

    /// Records that the word at `at` is a pointer with provenance.
    pub fn note_pointer(&mut self, at: PointerAt) {
        let (column, at) = match at {
            PointerAt::Value(offset) => (&mut self.value_pointers, offset),
            PointerAt::Memory(addr) => (&mut self.memory_pointers, addr),
        };
        if let Err(index) = column.binary_search(&at) {
            column.insert(index, at);
        }
    }
}

impl<'db> MemoryMap<'db> {
    /// The static whose memory holds `addr`, or that `addr` is one past the end of, and the
    /// offset of `addr` in it. Const eval leaves a gap after each allocation, so one past the
    /// end of one is never the start of another.
    pub fn static_at(&self, addr: usize) -> Option<(hir_def::StaticId, usize)> {
        let MemoryMap::Complex(map) = self else { return None };
        map.statics.iter().find_map(|it| {
            let offset = addr.checked_sub(it.start)?;
            (offset <= it.size).then_some((it.id, offset))
        })
    }

    /// The offsets of the words of the value that are pointers with provenance, in order.
    pub fn value_pointers(&self) -> &[usize] {
        match self {
            MemoryMap::Complex(map) => &map.value_pointers,
            MemoryMap::Empty | MemoryMap::Simple(_) => &[],
        }
    }

    /// The memory that holds `addr`, with the address it starts at.
    pub fn memory_at(&self, addr: usize) -> Option<(usize, &[u8])> {
        match self {
            MemoryMap::Empty => None,
            MemoryMap::Simple(memory) => (addr == 0).then_some((0, &**memory)),
            MemoryMap::Complex(map) => map.memory.holding(addr),
        }
    }

    /// The memory with bytes that holds `addr`, or that `addr` is one past the end of, with
    /// the address it starts at, passing over the empty pieces that references to nothing keep.
    pub fn kept_around(&self, addr: usize) -> Option<(usize, &[u8])> {
        match self {
            MemoryMap::Complex(map) => map.memory.around(addr),
            MemoryMap::Empty | MemoryMap::Simple(_) => None,
        }
    }

    /// What the piece of memory at `start` was aligned to where it was made, if that was
    /// recorded.
    pub fn align_at(&self, start: usize) -> Option<usize> {
        match self {
            MemoryMap::Empty | MemoryMap::Simple(_) => None,
            MemoryMap::Complex(map) => {
                let at = map.aligns.binary_search_by_key(&start, |it| it.0).ok()?;
                Some(map.aligns[at].1)
            }
        }
    }

    /// Whether the word at `at` is a pointer with provenance. An `Empty` or `Simple` map holds
    /// no such word: a `Simple` map is only made for a literal's reference.
    pub fn has_provenance(&self, at: PointerAt) -> bool {
        let MemoryMap::Complex(map) = self else { return false };
        match at {
            PointerAt::Value(offset) => map.value_pointers.binary_search(&offset).is_ok(),
            PointerAt::Memory(addr) => map.memory_pointers.binary_search(&addr).is_ok(),
        }
    }

    /// The addresses of the words in `range` of memory that are pointers with provenance,
    /// in order. An `Empty` or `Simple` map holds none.
    pub fn pointers_in(&self, range: std::ops::Range<usize>) -> &[usize] {
        let MemoryMap::Complex(map) = self else { return &[] };
        let from = map.memory_pointers.partition_point(|&it| it < range.start);
        let to = map.memory_pointers.partition_point(|&it| it < range.end);
        &map.memory_pointers[from..to.max(from)]
    }

    pub fn vtable_ty(&self, id: usize) -> Result<Ty<'db>, MirEvalError<'db>> {
        match self {
            MemoryMap::Empty | MemoryMap::Simple(_) => Err(MirEvalError::InvalidVTableId(id)),
            MemoryMap::Complex(cm) => cm.vtable.ty(id),
        }
    }

    /// The concrete type and the `dyn` type of the vtable whose id is `id`, if it is one:
    /// rustc's `GlobalAlloc::VTable(ty, dyn_ty)`. A word holding it is the vtable only where
    /// it has provenance ([`Self::has_provenance`]).
    pub fn vtable_of(&self, id: usize) -> Option<(Ty<'db>, Ty<'db>)> {
        let MemoryMap::Complex(map) = self else { return None };
        map.vtable.vtable(id)
    }

    /// The function (a fn item or a closure type) whose id is `id`, if it is one and not a
    /// vtable's.
    pub fn function_of(&self, id: usize) -> Option<Ty<'db>> {
        let MemoryMap::Complex(map) = self else { return None };
        match map.vtable.entry(id)? {
            (ty, None) if matches!(ty.kind(), TyKind::FnDef(..) | TyKind::Closure(..)) => Some(ty),
            _ => None,
        }
    }

    /// The type of the `TypeId` whose bytes, as an integer, are `id`.
    pub fn type_id_ty(&self, id: u128) -> Option<Ty<'db>> {
        let MemoryMap::Complex(map) = self else { return None };
        *map.type_ids.get(usize::try_from(id.checked_sub(1)?).ok()?)?
    }

    /// Whether the constant may hold a `TypeId`, which only [`Self::type_id_ty`] reads.
    pub fn has_type_ids(&self) -> bool {
        matches!(self, MemoryMap::Complex(map) if !map.type_ids.is_empty())
    }

    pub fn simple(v: Box<[u8]>) -> Self {
        MemoryMap::Simple(v)
    }

    /// This functions convert each address by a function `f` which gets the byte intervals and assign an address
    /// to them. It is useful when you want to load a constant with a memory map in a new memory. You can pass an
    /// allocator function as `f` and it will return a mapping of old addresses to new addresses.
    ///
    /// Each piece is given the alignment its evaluation recorded; a literal's map (`Simple`) is
    /// the bytes of a string or byte string, of alignment 1. A piece of a `Complex` map with no
    /// alignment recorded is an error naming `what` (the constant): a guess could under-align a
    /// pointee that needs more.
    pub fn transform_addresses(
        &self,
        what: impl Fn() -> String,
        mut f: impl FnMut(&[u8], usize) -> Result<usize, MirEvalError<'db>>,
    ) -> Result<HashMap<usize, usize, Global>, MirEvalError<'db>> {
        let mut transform = |(addr, val): (&usize, &[u8])| {
            let addr = *addr;
            let align = match self {
                MemoryMap::Simple(_) => 1,
                MemoryMap::Empty | MemoryMap::Complex(_) => match self.align_at(addr) {
                    Some(align) => align,
                    None => {
                        return Err(MirEvalError::NotSupported(format!(
                            // alloc: error path
                            "{}: its memory at {addr:#x} ({} bytes) has no alignment its evaluation \
                             recorded, and const eval does not guess one, which could under-align it",
                            what(),
                            val.len()
                        )));
                    }
                },
            };
            f(val, align).map(|it| (addr, it))
        };
        match self {
            MemoryMap::Empty => Ok(Default::default()),
            MemoryMap::Simple(m) => transform((&0, m)).map(|(addr, val)| {
                let mut map = HashMap::with_capacity_in(1, Global);
                map.insert(addr, val);
                map
            }),
            MemoryMap::Complex(cm) => {
                let mut map = HashMap::with_capacity_in(cm.memory.len(), Global);
                for (addr, val) in cm.memory.iter() {
                    let (addr, new) = transform((addr, val))?;
                    map.insert(addr, new);
                }
                Ok(map)
            }
        }
    }

    pub fn get(&self, addr: usize, size: usize) -> Option<&[u8]> {
        if size == 0 {
            Some(&[])
        } else {
            match self {
                MemoryMap::Empty => Some(&[]),
                MemoryMap::Simple(m) if addr == 0 => m.get(0..size),
                MemoryMap::Simple(_) => None,
                MemoryMap::Complex(cm) => cm.memory.get(&addr)?.get(0..size),
            }
        }
    }
}

#[cfg(test)]
mod memory_map_tests {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};

    use super::*;

    fn hash<T: Hash>(value: &T) -> u64 {
        BuildHasherDefault::<DefaultHasher>::default().hash_one(value)
    }

    fn pieces(of: &[(usize, &[u8])]) -> MemoryPieces {
        let mut pieces = MemoryPieces::default();
        for &(address, bytes) in of {
            pieces.insert(address, bytes);
        }
        pieces
    }

    #[test]
    fn pieces_are_sorted_by_address_whatever_order_they_came_in() {
        let a = pieces(&[(32, &[3, 3]), (8, &[1]), (16, &[2, 2, 2])]);
        let b = pieces(&[(8, &[1]), (16, &[2, 2, 2]), (32, &[3, 3])]);
        let seen: Vec<_> = a.iter().map(|(&at, bytes)| (at, bytes.to_vec())).collect();
        assert_eq!(seen, [(8, vec![1]), (16, vec![2, 2, 2]), (32, vec![3, 3])]);
        assert_eq!(a, b);
        assert_eq!(hash(&a), hash(&b));
        assert_eq!(a.get(&16), Some(&[2, 2, 2][..]));
        assert_eq!(a.get(&17), None);
        assert!(a.contains_key(&32) && !a.contains_key(&33));
        assert_eq!(a.len(), 3);
    }

    #[test]
    fn a_longer_piece_at_the_same_address_replaces_a_shorter_one_only() {
        let mut a = pieces(&[(8, &[1, 1]), (16, &[2])]);
        a.insert(8, &[9]);
        assert_eq!(a.get(&8), Some(&[1, 1][..]));
        a.insert(8, &[4, 4, 4]);
        a.insert(16, &[5, 5]);
        assert_eq!(a.get(&8), Some(&[4, 4, 4][..]));
        assert_eq!(a.get(&16), Some(&[5, 5][..]));
        let fresh = pieces(&[(16, &[5, 5]), (8, &[4, 4, 4])]);
        assert_eq!(a, fresh);
        assert_eq!(hash(&a), hash(&fresh));
        a.shrink_to_fit();
        assert_eq!(a.bytes, [4, 4, 4, 5, 5]);
        assert_eq!(a, fresh);
    }

    #[test]
    fn different_pieces_differ() {
        let a = pieces(&[(8, &[1, 2])]);
        for other in [pieces(&[(8, &[1, 3])]), pieces(&[(9, &[1, 2])]), pieces(&[(8, &[1])])] {
            assert_ne!(a, other);
            assert_ne!(hash(&a), hash(&other));
        }
        // The same bytes split differently are other pieces.
        assert_ne!(
            hash(&pieces(&[(0, &[1]), (1, &[2, 3])])),
            hash(&pieces(&[(0, &[1, 2]), (1, &[3])]))
        );
    }

    #[test]
    fn memory_at_finds_the_piece_at_an_address_or_the_lowest_holding_it() {
        let mut map = ComplexMemoryMap::default();
        map.insert(104, &[7u8; 4]);
        map.insert(100, &[6u8; 16]);
        map.insert(200, &[0u8; 0]);
        let map = MemoryMap::Complex(Box::new(map));
        assert_eq!(map.memory_at(104), Some((104, &[7; 4][..])));
        assert_eq!(map.memory_at(106), Some((100, &[6; 16][..])));
        assert_eq!(map.memory_at(115), Some((100, &[6; 16][..])));
        assert_eq!(map.memory_at(116), None);
        assert_eq!(map.memory_at(200), Some((200, &[][..])));
        assert_eq!(map.memory_at(99), None);
        assert_eq!(map.get(100, 2), Some(&[6, 6][..]));
        assert_eq!(map.get(101, 2), None);
        assert_eq!(map.get(104, 5), None);
    }

    #[test]
    fn an_empty_piece_inside_memory_is_passed_over_for_that_memory() {
        let mut map = ComplexMemoryMap::default();
        map.insert(100, &[6u8; 3]);
        map.insert(101, &[0u8; 0]);
        map.insert(103, &[0u8; 0]);
        map.insert(200, &[0u8; 0]);
        let map = MemoryMap::Complex(Box::new(map));
        assert_eq!(map.memory_at(101), Some((101, &[][..])));
        assert_eq!(map.kept_around(101), Some((100, &[6; 3][..])));
        assert_eq!(map.kept_around(103), Some((100, &[6; 3][..])));
        assert_eq!(map.kept_around(104), None);
        assert_eq!(map.kept_around(200), None);
    }

    #[test]
    fn alignments_are_kept_by_address_and_overwritten() {
        let mut map = ComplexMemoryMap::default();
        map.insert(64, &[0u8; 8]);
        map.note_align(64, 4);
        map.note_align(8, 2);
        map.note_align(64, 8);
        let mut other = ComplexMemoryMap::default();
        other.note_align(8, 2);
        other.note_align(64, 8);
        other.insert(64, &[0u8; 8]);
        let (map, other) = (MemoryMap::Complex(Box::new(map)), MemoryMap::Complex(Box::new(other)));
        assert_eq!(map.align_at(64), Some(8));
        assert_eq!(map.align_at(8), Some(2));
        assert_eq!(map.align_at(16), None);
        assert_eq!(map, other);
        assert_eq!(hash(&map), hash(&other));
        let addresses: HashMap<usize, usize, Global> = map
            .transform_addresses(
                || "the test's map".to_owned(),
                |bytes, align| Ok(bytes.len() * 1000 + align),
            )
            .unwrap();
        assert_eq!(addresses.get(&64), Some(&8008));
    }

    #[test]
    fn statics_and_kinds_of_map_are_hashed() {
        let mut with_static = ComplexMemoryMap::default();
        with_static.statics.push(StaticMemory {
            start: 8,
            size: 4,
            id: blaze_query::db::FromId::from_id(blaze_query::db::Id::from_u32(1)),
        });
        let empty = MemoryMap::Complex(Box::default());
        let with_static = MemoryMap::Complex(Box::new(with_static));
        assert_ne!(empty, with_static);
        assert_ne!(hash(&empty), hash(&with_static));
        assert_ne!(hash(&MemoryMap::Empty), hash(&MemoryMap::simple(Box::new([0u8; 0]))));
        assert_ne!(
            hash(&MemoryMap::simple(Box::new([1u8]))),
            hash(&MemoryMap::simple(Box::new([2u8])))
        );
    }

    #[test]
    fn pointers_with_provenance_are_kept_by_column_and_hashed() {
        let mut map = ComplexMemoryMap::default();
        map.note_pointer(PointerAt::Memory(24));
        map.note_pointer(PointerAt::Value(8));
        map.note_pointer(PointerAt::Memory(16));
        map.note_pointer(PointerAt::Memory(24));
        let mut other = ComplexMemoryMap::default();
        other.note_pointer(PointerAt::Memory(16));
        other.note_pointer(PointerAt::Memory(24));
        other.note_pointer(PointerAt::Value(8));
        assert_eq!(map.memory_pointers, [16, 24]);
        let (map, other) = (MemoryMap::Complex(Box::new(map)), MemoryMap::Complex(Box::new(other)));
        assert_eq!(map, other);
        assert_eq!(hash(&map), hash(&other));
        assert!(map.has_provenance(PointerAt::Value(8)));
        assert!(map.has_provenance(PointerAt::Memory(16)));
        assert!(!map.has_provenance(PointerAt::Memory(8)));
        assert!(!map.has_provenance(PointerAt::Value(16)));
        assert!(!MemoryMap::Empty.has_provenance(PointerAt::Value(0)));
        assert!(!MemoryMap::simple(Box::new([0u8; 8])).has_provenance(PointerAt::Value(0)));
        for at in [PointerAt::Value(16), PointerAt::Memory(8)] {
            let mut moved = ComplexMemoryMap::default();
            moved.note_pointer(PointerAt::Memory(16));
            moved.note_pointer(PointerAt::Memory(24));
            moved.note_pointer(at);
            let moved = MemoryMap::Complex(Box::new(moved));
            assert_ne!(map, moved);
            assert_ne!(hash(&map), hash(&moved));
        }
    }
}

/// Returns the index of a parameter in the generic type parameter list by its id.
pub fn type_or_const_param_idx(db: &Db, id: TypeOrConstParamId) -> u32 {
    generics::generics(db, id.parent).type_or_const_param_idx(id)
}

pub fn lifetime_param_idx(db: &Db, id: LifetimeParamId) -> u32 {
    generics::generics(db, id.parent).lifetime_param_idx(id, false).0
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub enum ImplTraitId {
    ReturnTypeImplTrait(hir_def::FunctionId, next_solver::ImplTraitIdx),
    TypeAliasImplTrait(hir_def::TypeAliasId, next_solver::ImplTraitIdx),
}

/// 'Canonicalizes' the `t` by replacing any errors with new variables. Also
/// ensures there are no unbound variables or inference variables anywhere in
/// the `t`.
pub fn replace_errors_with_variables<'db, T>(interner: DbInterner<'db>, t: &T) -> Canonical<'db, T>
where
    T: rustc_type_ir::TypeFoldable<DbInterner<'db>> + Clone,
{
    use rustc_type_ir::{FallibleTypeFolder, TypeSuperFoldable};
    struct ErrorReplacer<'db> {
        interner: DbInterner<'db>,
        vars: Vec<CanonicalVarKind<'db>>,
        binder: rustc_type_ir::DebruijnIndex,
    }
    impl<'db> FallibleTypeFolder<DbInterner<'db>> for ErrorReplacer<'db> {
        #[cfg(debug_assertions)]
        type Error = ();
        #[cfg(not(debug_assertions))]
        type Error = std::convert::Infallible;

        fn cx(&self) -> DbInterner<'db> {
            self.interner
        }

        fn try_fold_binder<T>(&mut self, t: Binder<'db, T>) -> Result<Binder<'db, T>, Self::Error>
        where
            T: rustc_type_ir::TypeFoldable<DbInterner<'db>>,
        {
            self.binder.shift_in(1);
            let result = t.try_super_fold_with(self);
            self.binder.shift_out(1);
            result
        }

        fn try_fold_ty(&mut self, t: Ty<'db>) -> Result<Ty<'db>, Self::Error> {
            if !t.has_type_flags(
                rustc_type_ir::TypeFlags::HAS_ERROR
                    | rustc_type_ir::TypeFlags::HAS_TY_INFER
                    | rustc_type_ir::TypeFlags::HAS_CT_INFER
                    | rustc_type_ir::TypeFlags::HAS_RE_INFER,
            ) {
                return Ok(t);
            }

            #[cfg(debug_assertions)]
            let error = || Err(());
            #[cfg(not(debug_assertions))]
            let error = || Ok(Ty::new_error(self.interner, crate::next_solver::ErrorGuaranteed));

            match t.kind() {
                TyKind::Error(_) => {
                    let var = rustc_type_ir::BoundVar::from_usize(self.vars.len());
                    self.vars.push(CanonicalVarKind::Ty {
                        ui: rustc_type_ir::UniverseIndex::ZERO,
                        sub_root: var,
                    });
                    Ok(Ty::new_bound(
                        self.interner,
                        self.binder,
                        BoundTy { var, kind: BoundTyKind::Anon },
                    ))
                }
                TyKind::Infer(_) => error(),
                TyKind::Bound(BoundVarIndexKind::Bound(index), _) if index > self.binder => error(),
                _ => t.try_super_fold_with(self),
            }
        }

        fn try_fold_const(&mut self, ct: Const<'db>) -> Result<Const<'db>, Self::Error> {
            if !ct.has_type_flags(
                rustc_type_ir::TypeFlags::HAS_ERROR
                    | rustc_type_ir::TypeFlags::HAS_TY_INFER
                    | rustc_type_ir::TypeFlags::HAS_CT_INFER
                    | rustc_type_ir::TypeFlags::HAS_RE_INFER,
            ) {
                return Ok(ct);
            }

            #[cfg(debug_assertions)]
            let error = || Err(());
            #[cfg(not(debug_assertions))]
            let error = || Ok(Const::error(self.interner));

            match ct.kind() {
                ConstKind::Error(_) => {
                    let var = rustc_type_ir::BoundVar::from_usize(self.vars.len());
                    self.vars.push(CanonicalVarKind::Const(rustc_type_ir::UniverseIndex::ZERO));
                    Ok(Const::new_bound(self.interner, self.binder, BoundConst::new(var)))
                }
                ConstKind::Infer(_) => error(),
                ConstKind::Bound(BoundVarIndexKind::Bound(index), _) if index > self.binder => {
                    error()
                }
                _ => ct.try_super_fold_with(self),
            }
        }

        fn try_fold_region(&mut self, region: Region<'db>) -> Result<Region<'db>, Self::Error> {
            #[cfg(debug_assertions)]
            let error = || Err(());
            #[cfg(not(debug_assertions))]
            let error = || Ok(Region::error(self.interner));

            match region.kind() {
                RegionKind::ReError(_) => {
                    let var = rustc_type_ir::BoundVar::from_usize(self.vars.len());
                    self.vars.push(CanonicalVarKind::Region(rustc_type_ir::UniverseIndex::ZERO));
                    Ok(Region::new_bound(
                        self.interner,
                        self.binder,
                        BoundRegion { var, kind: BoundRegionKind::Anon },
                    ))
                }
                RegionKind::ReVar(_) => error(),
                RegionKind::ReBound(BoundVarIndexKind::Bound(index), _) if index > self.binder => {
                    error()
                }
                _ => Ok(region),
            }
        }
    }

    // alloc: the `hir` API's edge, and pushed only for error types, regions and consts
    let vars = Vec::new();
    let mut error_replacer =
        ErrorReplacer { vars, binder: rustc_type_ir::DebruijnIndex::ZERO, interner };
    // clone: the `hir` API's edge, folding takes `T` by value and callers pass interned types
    let value = match t.clone().try_fold_with(&mut error_replacer) {
        Ok(t) => t,
        Err(_) => panic!("Encountered unbound or inference vars in {t:?}"),
    };
    Canonical {
        value,
        max_universe: rustc_type_ir::UniverseIndex::ZERO,
        var_kinds: CanonicalVarKinds::new_from_slice(&error_replacer.vars),
    }
}

/// To be used from `hir` only.
pub fn associated_type_shorthand_candidates(
    db: &Db,
    def: GenericDefId,
    res: TypeNs,
    mut cb: impl FnMut(&Name, TypeAliasId) -> bool,
) -> Option<TypeAliasId> {
    let interner = DbInterner::new_no_crate(db);
    let (def, param) = match res {
        TypeNs::GenericParam(param) => (def, param),
        TypeNs::SelfType(impl_) => {
            let impl_trait = db.impl_trait(impl_)?.skip_binder().def_id.0;
            let param = TypeParamId::trait_self(impl_trait);
            (impl_trait.into(), param)
        }
        _ => return None,
    };

    let mut dedup_map = HashSet::new_in(Global);
    let param_ty = Ty::new_param(interner, param, type_or_const_param_idx(db, param.into()));
    // We use the ParamEnv and not the predicates because the ParamEnv elaborates bounds.
    let param_env = db.trait_environment(def);
    for clause in param_env.clauses {
        let ClauseKind::Trait(trait_clause) = clause.kind().skip_binder() else { continue };
        if trait_clause.self_ty() != param_ty {
            continue;
        }
        let trait_id = trait_clause.def_id().0;
        dedup_map.extend(
            SupertraitsInfo::query(db, trait_id)
                .defined_assoc_types
                .iter()
                .map(|(name, id)| (name, *id)),
        );
    }

    dedup_map
        .into_iter()
        .try_for_each(
            |(name, id)| {
                if cb(name, id) { ControlFlow::Break(id) } else { ControlFlow::Continue(()) }
            },
        )
        .break_value()
}

/// To be used from `hir` only.
pub fn callable_sig_from_fn_trait<'db>(
    self_ty: Ty<'db>,
    param_env: ParamEnvAndCrate<'db>,
    db: &'db Db,
) -> Option<(FnTrait, PolyFnSig<'db>)> {
    let ParamEnvAndCrate { param_env, krate } = param_env;
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let lang_items = interner.lang_items();
    let cause = ObligationCause::dummy();

    let impls_trait = |trait_: FnTrait| {
        let mut ocx = ObligationCtxt::new(&infcx);
        let tupled_args = infcx.next_ty_var(Span::Dummy);
        let args = GenericArgs::new_from_slice(&[self_ty.into(), tupled_args.into()]);
        let trait_id = trait_.get_id(lang_items)?;
        let trait_ref = TraitRef::new_from_args(interner, trait_id.into(), args);
        let obligation = Obligation::new(interner, cause, param_env, trait_ref);
        ocx.register_obligation(obligation);
        if !ocx.try_evaluate_obligations().is_empty() {
            return None;
        }
        let tupled_args =
            infcx.deeply_resolve_ignoring_regions(tupled_args).replace_infer_with_error(interner);
        if tupled_args.is_tuple() { Some(tupled_args) } else { None }
    };

    let (trait_, args) = 'find_trait: {
        for trait_ in [FnTrait::Fn, FnTrait::FnMut, FnTrait::FnOnce] {
            if let Some(args) = impls_trait(trait_) {
                break 'find_trait (trait_, args);
            }
        }
        return None;
    };

    let output_assoc_type = lang_items.FnOnceOutput?;
    let output_projection = Ty::new_alias(
        interner,
        IsRigid::No,
        AliasTy::new(
            interner,
            rustc_type_ir::Projection { def_id: output_assoc_type.into() },
            [self_ty, args],
        ),
    );
    let mut ocx = ObligationCtxt::new(&infcx);
    let ret = ocx.structurally_normalize_ty(&cause, param_env, output_projection).ok()?;
    let ret = ret.replace_infer_with_error(interner);

    let sig = Binder::dummy(interner.mk_fn_sig(
        args.tuple_fields(),
        ret,
        false,
        // FIXME(splat): handle splatted arguments
        Safety::Safe,
        ExternAbi::Rust,
    ));
    Some((trait_, sig))
}

struct ParamCollector {
    params: HashSet<TypeOrConstParamId, Global>,
}

impl<'db> rustc_type_ir::TypeVisitor<DbInterner<'db>> for ParamCollector {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) -> Self::Result {
        if let TyKind::Param(param) = ty.kind() {
            self.params.insert(param.id.into());
        }

        ty.super_visit_with(self);
    }

    fn visit_const(&mut self, konst: Const<'db>) -> Self::Result {
        if let ConstKind::Param(param) = konst.kind() {
            self.params.insert(param.id.into());
        }

        konst.super_visit_with(self);
    }
}

/// Returns unique params for types and consts contained in `value`.
pub fn collect_params<'db, T>(value: &T) -> Vec<TypeOrConstParamId>
where
    T: ?Sized + rustc_type_ir::TypeVisitable<DbInterner<'db>>,
{
    let mut collector = ParamCollector { params: HashSet::new_in(Global) };
    value.visit_with(&mut collector);
    Vec::from_iter(collector.params)
}

/// A `Span` represents some location in lowered code - a type, expression or pattern.
///
/// It has no meaning outside its body therefore it should not exit the pass it was created in
/// (e.g. inference). It is usually associated with a solver obligation or an infer var, which
/// should also not cross the pass they were created in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Span {
    ExprId(ExprId),
    PatId(PatId),
    BindingId(BindingId),
    TypeRefId(TypeRefId),
    /// An unimportant location. Errors on this will be suppressed.
    Dummy,
}
impl_from!(ExprId, PatId, BindingId, TypeRefId for Span);

impl From<ExprOrPatIdPacked> for Span {
    fn from(value: ExprOrPatIdPacked) -> Self {
        match value.unpack() {
            ExprOrPatId::ExprId(idx) => idx.into(),
            ExprOrPatId::PatId(idx) => idx.into(),
        }
    }
}

impl Span {
    pub(crate) fn pick_best(a: Span, b: Span) -> Span {
        // We prefer dummy spans to minimize the risk of false errors.
        if b.is_dummy() { b } else { a }
    }

    #[inline]
    pub fn is_dummy(&self) -> bool {
        matches!(self, Self::Dummy)
    }
}

/// A [`DefWithBodyId`], or an anon const.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    blaze_query::Supertype,
    blaze_query::Value,
)]
pub enum InferBodyId<'db> {
    DefWithBodyId(DefWithBodyId),
    AnonConstId(AnonConstId<'db>),
}
impl_from!(
    impl<'db>
    DefWithBodyId(FunctionId, ConstId, StaticId),
    AnonConstId<'db>
    for InferBodyId<'db>
);
impl<'db> From<EnumVariantId> for InferBodyId<'db> {
    fn from(id: EnumVariantId) -> Self {
        InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(id))
    }
}

impl HasModule for InferBodyId<'_> {
    fn module(&self, db: &Db) -> ModuleId {
        match self {
            InferBodyId::DefWithBodyId(id) => id.module(db),
            InferBodyId::AnonConstId(id) => id.module(db),
        }
    }
}

impl HasResolver for InferBodyId<'_> {
    fn resolver(self, db: &Db) -> Resolver<'_> {
        match self {
            InferBodyId::DefWithBodyId(id) => id.resolver(db),
            InferBodyId::AnonConstId(id) => id.resolver(db),
        }
    }
}

impl InferBodyId<'_> {
    pub fn expression_store_owner(self, db: &Db) -> ExpressionStoreOwnerId {
        match self {
            InferBodyId::DefWithBodyId(id) => id.into(),
            InferBodyId::AnonConstId(id) => id.loc(db).owner,
        }
    }

    pub fn generic_def(self, db: &Db) -> GenericDefId {
        match self {
            InferBodyId::DefWithBodyId(id) => id.generic_def(db),
            InferBodyId::AnonConstId(id) => id.loc(db).owner.generic_def(db),
        }
    }

    #[inline]
    pub fn as_function(self) -> Option<FunctionId> {
        match self {
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => Some(it),
            _ => None,
        }
    }

    #[inline]
    pub fn as_variant(self) -> Option<EnumVariantId> {
        match self {
            InferBodyId::DefWithBodyId(DefWithBodyId::VariantId(it)) => Some(it),
            _ => None,
        }
    }

    pub fn store_and_root_expr(self, db: &Db) -> (&ExpressionStore, ExprId) {
        match self {
            InferBodyId::DefWithBodyId(id) => {
                let body = Body::of(db, id);
                (body, body.root_expr())
            }
            InferBodyId::AnonConstId(id) => {
                let loc = id.loc(db);
                let store = ExpressionStore::of(db, loc.owner);
                (store, loc.expr)
            }
        }
    }
}

pub fn setup_tracing() -> Option<tracing::subscriber::DefaultGuard> {
    use std::env;
    use tracing_subscriber::{Registry, layer::SubscriberExt};
    use tracing_tree::HierarchicalLayer;

    let filter: tracing_subscriber::filter::Targets =
        env::var("SOLVER_DEBUG").ok().and_then(|it| it.parse().ok()).unwrap_or_default();
    let layer = HierarchicalLayer::default()
        .with_indent_lines(true)
        .with_ansi(false)
        .with_indent_amount(2)
        .with_writer(std::io::stderr);
    let subscriber = Registry::default().with(filter).with(layer);
    Some(tracing::subscriber::set_default(subscriber))
}
