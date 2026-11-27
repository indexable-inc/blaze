//! Layout-Canonicalized Monomorphization (Polymorphization) Engine.
//!
//! Attacks monomorphization blowup (Wave 26 measured `781,591` monomorphized functions compiled
//! from `70,925` source bodies — an 11x expansion) while preserving 100% Rust soundness
//! (Rule 4 `superset-of-rust`, Rule 21 `keys-cover-every-input`, Rule 25 `deterministic-output`).
//!
//! # Architecture (sans-IO, `#![no_std]`-clean, explicit `Alloc`)
//!
//! 1. **ADT parameter usage analysis ([`analyze_adt_fields`], [`analyze_adt_graph`])**:
//!    Determines whether an ADT `S<T>` ignores `T` (`PhantomData<T>` -> [`AdtParamUse::Unused`]),
//!    only stores `T` behind thin pointers (`NonNull<T>`, `Unique<T>`, `RawVec<T>`, `Vec<T>` ->
//!    [`AdtParamUse::OpaquePtr`]), embeds `T` by value (`Option<T>`, `Result<T, E>` ->
//!    [`AdtParamUse::ByValue`]), or projects/aliases `T` ([`AdtParamUse::Concrete`]).
//! 2. **Per-function body usage analysis ([`analyze_body`])**:
//!    Walks a `blaze_body::BodyRef` (locals, places, statements, terminators, drops, intrinsics,
//!    casts, constants, callees) iteratively with zero recursion and produces exact [`ParamUse`]
//!    bitflags per generic parameter plus direct callee/drop edges.
//! 3. **Transitive call-graph fixed-point propagation ([`propagate_call_graph`])**:
//!    Propagates [`ParamUse`] across direct function calls, drop edges, and mutually recursive
//!    SCCs until fixpoint. Any unanalyzed callee, trait method dispatch, inline `const { .. }`
//!    block, inline `asm!`, unsizing coercion, non-Rust ABI, or reflection intrinsic conservatively
//!    forces [`CanonLevel::Concrete`].
//! 4. **Canonical substitution & deterministic instance identity ([`canonicalize_args`],
//!    [`encode_canonical_instance`], [`canonical_fn_symbol`])**:
//!    Maps each concrete ground argument to [`CanonTy::Unused`], [`CanonTy::OpaqueThin`],
//!    [`CanonTy::Layout`]`(`[`LayoutShape`]`)`, or [`CanonTy::Concrete`]`(PTy)` and encodes a
//!    table-index-independent canonical forest & BLAKE3 [`Symbol`] (Rule 25).
//!
//! # Soundness invariants
//!
//! - [`CanonLevel::Unused`] -> [`CanonTy::Unused`] (unit `()`): only when `T_i` never appears
//!   (or only inside `PhantomData<T_i>`) and no callee/const-block/asm can observe `T_i`.
//! - [`CanonLevel::OpaquePtr`] -> [`CanonTy::OpaqueThin`]: only when `T_i` appears exclusively
//!   behind thin pointers/references/`NonNull`/`PhantomData` (never dereferenced by value, never
//!   sized/aligned/offset, never dropped, never trait-dispatched) **and** the concrete argument
//!   is a thin-pointer pointee (`Tail::Sized` or `PNode::Foreign`). Unsized DSTs stay concrete.
//! - [`CanonLevel::LayoutOnly`] -> [`CanonTy::Layout`]`(`[`LayoutShape`]`)`: only when `T_i` is
//!   used purely by layout/move/copy/index/size/align/niche with no trait dispatch or reflection,
//!   **and** the concrete argument is sized, `!needs_drop`, and carries a pure Rust ABI
//!   [`LayoutShape`] capturing `(size, align, abi_class, niche, uninhabited)`.
//!   - `f64` (`Float64`) never collapses with `u64` (`Int64`).
//!   - `i8` (`Int8Sext`) never collapses with `u8` (`Int8Zext`).
//!   - Non-transparent `struct S(u64)` (`Indirect`) never collapses with `u64` (`Int64`).
//!   - `Option<&Foo>` (`niche: NonNull`) never collapses with `Option<u64>` (`niche: None`).
//!   - `Option<bool>` (`0..=1`) never collapses with `Option<NonZero<u8>>` (`1..=255`).
//!   - `T: Drop` (`needs_drop == true`) never collapses with non-Drop types.
//! - [`CanonLevel::Concrete`] -> [`CanonTy::Concrete`]`(arg.ty)`: default whenever uncertain.

use core::fmt;

use blaze_body::{
    BadRow, BinOp, BodyRef, Callee, CastKind, ConstKind, Local, Operand, Place, Proj, Stmt, StmtKind, TyError, UnOp,
    Value,
};
use blaze_build::abi::Pass;
use blaze_build::instance::each_child;
use blaze_build::intrinsics::Which;
use blaze_build::layout::{Layout, Niche, Tail};
use blaze_code::record::Symbol;
use blaze_ids::{ContentId, Hasher};
use blaze_lir::Ty as LirTy;
use blaze_lir::func::Ext;
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::{Abi, FloatTy, IntTy, ItemKey, List, Mutbl, PNode, PTy, PTys, PortableTys, UintTy};

use crate::symbols::{NAME_MOST_BYTES, NAME_MOST_NODES};

/// BLAKE3 domain for canonicalized function instance symbols (distinct from `symbols::FN_DOMAIN`
/// so canonical symbols never collide with uncanonicalized symbols; Rule 21).
const CANON_FN_DOMAIN: &str = "blaze-ladder poly-canon fn instance symbol 1";

/// Dedicated synthetic `ItemKey` for [`CanonTy::OpaqueThin`] when represented as a `PNode::Foreign`
/// in a ground [`PortableTys`] table.
///
/// Using `PNode::Foreign` guarantees that:
/// - `*const T`, `&T`, `NonNull<T>` are 8-byte thin pointers;
/// - `needs_drop` is `false`;
/// - any accidental `size_of` or by-value layout fails loudly (`Refusal::NoSize`) instead of
///   silently miscompiling.
pub const OPAQUE_THIN_FOREIGN_KEY: ItemKey = ItemKey(0xB1A2_E000_504F_4C59_0000_0000_0000_0001);

/// Maximum distinct type nodes visited in one iterative type walk (Rule 18).
pub const MAX_TYPE_WALK_NODES: usize = 1 << 16;

/// Maximum place projection chain depth walked (Rule 18).
pub const MAX_PLACE_DEPTH: u32 = 256;

/// Maximum fixed-point iterations for call-graph or ADT-graph propagation (Rule 18).
pub const MAX_FIXED_POINT_ITERS: u32 = 1024;

/// Maximum generic parameters analyzed on a single item (Rule 18).
pub const MAX_GENERIC_PARAMS: u32 = 4096;

/// Typed errors from polymorphization analysis, propagation, or canonical encoding (Rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolyError {
    /// A body row was out of range or malformed.
    Body(BadRow),
    /// A body type handle failed to resolve against the type table and overlay.
    Ty(TyError),
    /// A ground type table lookup failed or met a non-ground type node.
    NotGround { what: &'static str },
    /// The caller passed a different number of arguments than generic parameters.
    ArgCountMismatch { expected: usize, got: usize },
    /// A generic parameter index exceeded `param_count`.
    ParamOutOfRange { index: u32, param_count: u32 },
    /// An item declared more than [`MAX_GENERIC_PARAMS`] generic parameters.
    TooManyParams { count: u32 },
    /// An iterative type walk exceeded [`MAX_TYPE_WALK_NODES`].
    TypeWalkCap { cap: usize },
    /// A place projection chain exceeded [`MAX_PLACE_DEPTH`].
    PlaceDepthCap { cap: u32 },
    /// Fixed-point propagation did not converge within [`MAX_FIXED_POINT_ITERS`].
    IterationCap { cap: u32 },
    /// Canonical forest encoding exceeded [`NAME_MOST_NODES`] or [`NAME_MOST_BYTES`].
    ForestTooLarge { nodes: u32, bytes: usize },
    /// The ground [`PortableTys`] table was full when interning a canonical representative.
    TableFull,
}

impl From<BadRow> for PolyError {
    fn from(e: BadRow) -> Self {
        PolyError::Body(e)
    }
}

impl From<TyError> for PolyError {
    fn from(e: TyError) -> Self {
        PolyError::Ty(e)
    }
}

impl fmt::Display for PolyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            PolyError::Body(e) => write!(f, "poly_canon: malformed body row {e:?}"),
            PolyError::Ty(e) => write!(f, "poly_canon: malformed type handle {e:?}"),
            PolyError::NotGround { what } => write!(f, "poly_canon: non-ground type in canonical instance: {what}"),
            PolyError::ArgCountMismatch { expected, got } => {
                write!(f, "poly_canon: argument count mismatch (expected {expected}, got {got})")
            }
            PolyError::ParamOutOfRange { index, param_count } => {
                write!(f, "poly_canon: generic param index {index} out of range (param_count = {param_count})")
            }
            PolyError::TooManyParams { count } => {
                write!(f, "poly_canon: item has {count} generic params, past cap {MAX_GENERIC_PARAMS}")
            }
            PolyError::TypeWalkCap { cap } => write!(f, "poly_canon: type walk exceeded cap of {cap} nodes"),
            PolyError::PlaceDepthCap { cap } => write!(f, "poly_canon: place projection chain exceeded cap of {cap}"),
            PolyError::IterationCap { cap } => write!(f, "poly_canon: fixed-point propagation exceeded {cap} iterations"),
            PolyError::ForestTooLarge { nodes, bytes } => {
                write!(f, "poly_canon: canonical instance forest reached {nodes} nodes and {bytes} bytes, past cap")
            }
            PolyError::TableFull => write!(f, "poly_canon: ground PortableTys table is full"),
        }
    }
}

impl core::error::Error for PolyError {}

// ---------------------------------------------------------------------------
// ParamUse bitflags & CanonLevel lattice
// ---------------------------------------------------------------------------

/// Four-tier canonicalization lattice derived from [`ParamUse`].
///
/// Ordered from most canonicalizable (`Unused`) to least (`Concrete`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum CanonLevel {
    /// Parameter is never used (or only inside `PhantomData`). Canonicalizes to `()`.
    Unused = 0,
    /// Parameter only appears behind thin pointers/refs/`NonNull`/`PhantomData` (never
    /// dereferenced by value, never sized/aligned/offset, never dropped, never trait-dispatched).
    /// Canonicalizes thin pointees to [`CanonTy::OpaqueThin`].
    OpaquePtr = 1,
    /// Parameter is used only for layout/move/copy/index/size/align/niche with no trait dispatch
    /// or reflection. Canonicalizes sized `!needs_drop` types to [`CanonTy::Layout`].
    LayoutOnly = 2,
    /// Parameter must remain concrete (trait dispatch, reflection, const generic, non-Rust ABI,
    /// unsized metadata, unanalyzed callee, inline asm, or anything uncertain).
    Concrete = 3,
}

/// Bitflags recording every observed way a generic parameter `T_i` is used by a function
/// (and transitively by its callees).
///
/// Forms a finite join-semilattice under bitwise OR (`|`), guaranteeing rapid monotone
/// convergence in call-graph fixed-point propagation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ParamUse(u16);

impl ParamUse {
    /// Parameter has not been observed anywhere (`CanonLevel::Unused`).
    pub const NONE: ParamUse = ParamUse(0);
    /// Parameter appeared inside `PhantomData<T>` (`CanonLevel::Unused`).
    pub const PHANTOM: ParamUse = ParamUse(1 << 0);
    /// Parameter appeared behind a thin pointer, reference, `NonNull`, slice len, or pointer-only
    /// ADT (`CanonLevel::OpaquePtr`).
    pub const OPAQUE_PTR: ParamUse = ParamUse(1 << 1);
    /// Parameter's `size_of`, `align_of`, or pointer/slice stride was read (`CanonLevel::LayoutOnly`).
    pub const SIZE_ALIGN: ParamUse = ParamUse(1 << 2);
    /// Parameter was moved, copied, stored, passed/returned by value, or embedded in a field/enum
    /// layout (`CanonLevel::LayoutOnly`).
    pub const BY_VALUE: ParamUse = ParamUse(1 << 3);
    /// Parameter may require drop glue or `needs_drop` check (`CanonLevel::LayoutOnly` when
    /// `!arg.needs_drop`, forced concrete when `arg.needs_drop`).
    pub const DROP_GLUE: ParamUse = ParamUse(1 << 4);
    /// Parameter participated in trait dispatch, associated type projection, or opaque type
    /// (`CanonLevel::Concrete`).
    pub const TRAIT_DISPATCH: ParamUse = ParamUse(1 << 5);
    /// Parameter passed to `TypeId::of`, `type_name`, `type_info`, `variant_count`, or `blaze_*`
    /// reflection intrinsics (`CanonLevel::Concrete`).
    pub const REFLECTION: ParamUse = ParamUse(1 << 6);
    /// Parameter used in a type-specific primitive/atomic/SIMD/validity/discriminant op
    /// (`CanonLevel::Concrete`).
    pub const VALUE_OP: ParamUse = ParamUse(1 << 7);
    /// Parameter is a used const generic parameter (`CanonLevel::Concrete`).
    pub const CONST_PARAM: ParamUse = ParamUse(1 << 8);
    /// Parameter passed to an unanalyzed callee, fn pointer, inline `const`, inline `asm!`,
    /// closure, coroutine, or unknown ADT (`CanonLevel::Concrete`).
    pub const UNANALYZED_CALLEE: ParamUse = ParamUse(1 << 9);
    /// Parameter appeared in a non-Rust ABI signature (e.g. `extern "C"`, where AAPCS64 HFA rules
    /// inspect struct fields; `CanonLevel::Concrete`).
    pub const NON_RUST_ABI: ParamUse = ParamUse(1 << 10);
    /// Parameter participated in unsized coercion (`&T as &dyn Tr` vtable creation) or pointer
    /// metadata extraction (`CanonLevel::Concrete`).
    pub const UNSIZED_META: ParamUse = ParamUse(1 << 11);

    /// Bitmask of all flags that require [`CanonLevel::LayoutOnly`].
    pub const LAYOUT_MASK: ParamUse = ParamUse(Self::SIZE_ALIGN.0 | Self::BY_VALUE.0 | Self::DROP_GLUE.0);

    /// Bitmask of all flags that force [`CanonLevel::Concrete`].
    pub const CONCRETE_MASK: ParamUse = ParamUse(
        Self::TRAIT_DISPATCH.0
            | Self::REFLECTION.0
            | Self::VALUE_OP.0
            | Self::CONST_PARAM.0
            | Self::UNANALYZED_CALLEE.0
            | Self::NON_RUST_ABI.0
            | Self::UNSIZED_META.0,
    );

    #[inline]
    pub const fn bits(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn from_bits(bits: u16) -> ParamUse {
        ParamUse(bits)
    }

    #[inline]
    pub const fn union(self, other: ParamUse) -> ParamUse {
        ParamUse(self.0 | other.0)
    }

    #[inline]
    pub const fn contains(self, other: ParamUse) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub const fn intersects(self, other: ParamUse) -> bool {
        (self.0 & other.0) != 0
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Joins `other` into `self` and returns `true` iff any new bit was set.
    #[inline]
    pub fn join_assign(&mut self, other: ParamUse) -> bool {
        let next = self.0 | other.0;
        let changed = next != self.0;
        self.0 = next;
        changed
    }

    /// Classifies these bitflags into the 4-level [`CanonLevel`] lattice.
    #[inline]
    pub const fn level(self) -> CanonLevel {
        if self.intersects(Self::CONCRETE_MASK) {
            CanonLevel::Concrete
        } else if self.intersects(Self::LAYOUT_MASK) {
            CanonLevel::LayoutOnly
        } else if self.contains(Self::OPAQUE_PTR) {
            CanonLevel::OpaquePtr
        } else {
            CanonLevel::Unused
        }
    }

    /// Returns `true` iff the parameter is effectively unused (never referenced, or only inside
    /// `PhantomData`).
    #[inline]
    pub const fn is_unused(self) -> bool {
        matches!(self.level(), CanonLevel::Unused)
    }

    /// Returns `true` iff the parameter can be canonicalized at least as `OPAQUE_PTR`.
    #[inline]
    pub const fn is_opaque_ptr(self) -> bool {
        matches!(self.level(), CanonLevel::OpaquePtr)
    }

    /// Returns `true` iff the parameter is layout-only canonicalizable.
    #[inline]
    pub const fn is_layout_only(self) -> bool {
        matches!(self.level(), CanonLevel::LayoutOnly)
    }

    /// Returns `true` iff the parameter must remain concrete.
    #[inline]
    pub const fn is_concrete(self) -> bool {
        matches!(self.level(), CanonLevel::Concrete)
    }
}

impl core::ops::BitOr for ParamUse {
    type Output = ParamUse;
    #[inline]
    fn bitor(self, rhs: ParamUse) -> ParamUse {
        self.union(rhs)
    }
}

impl core::ops::BitOrAssign for ParamUse {
    #[inline]
    fn bitor_assign(&mut self, rhs: ParamUse) {
        self.0 |= rhs.0;
    }
}

impl fmt::Debug for ParamUse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("ParamUse(NONE)");
        }
        f.write_str("ParamUse(")?;
        let flags: [(ParamUse, &str); 12] = [
            (Self::PHANTOM, "PHANTOM"),
            (Self::OPAQUE_PTR, "OPAQUE_PTR"),
            (Self::SIZE_ALIGN, "SIZE_ALIGN"),
            (Self::BY_VALUE, "BY_VALUE"),
            (Self::DROP_GLUE, "DROP_GLUE"),
            (Self::TRAIT_DISPATCH, "TRAIT_DISPATCH"),
            (Self::REFLECTION, "REFLECTION"),
            (Self::VALUE_OP, "VALUE_OP"),
            (Self::CONST_PARAM, "CONST_PARAM"),
            (Self::UNANALYZED_CALLEE, "UNANALYZED_CALLEE"),
            (Self::NON_RUST_ABI, "NON_RUST_ABI"),
            (Self::UNSIZED_META, "UNSIZED_META"),
        ];
        let mut first = true;
        for (bit, name) in flags {
            if self.contains(bit) {
                if !first {
                    f.write_str(" | ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        f.write_str(")")
    }
}

// ---------------------------------------------------------------------------
// AbiClass, NicheShape, LayoutShape, CanonTy, ArgInfo
// ---------------------------------------------------------------------------

/// How a sized value crosses the Rust function-call ABI boundary on 64-bit targets.
///
/// Derived directly from [`blaze_build::layout::Layout::rust_pass`] and [`Pass`]. Two types with
/// different [`AbiClass`]es NEVER share a [`CanonTy::Layout`] (e.g. `f64` in FPR vs `u64` in GPR,
/// `i8` sign-extended vs `u8` zero-extended, non-transparent `struct S(u64)` indirect vs `u64`
/// scalar).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum AbiClass {
    /// Zero-sized (`Pass::Ignore`).
    Zst = 0,
    /// 8-bit integer, zero-extended (`u8`, `bool`).
    Int8Zext = 1,
    /// 8-bit integer, sign-extended (`i8`).
    Int8Sext = 2,
    /// 16-bit integer, zero-extended (`u16`).
    Int16Zext = 3,
    /// 16-bit integer, sign-extended (`i16`).
    Int16Sext = 4,
    /// 32-bit GPR (`u32`, `i32`, `char`).
    Int32 = 5,
    /// 64-bit GPR (`u64`, `i64`, `usize`, `isize`, thin `*const T`/`*mut T`/`&T`/`&mut T`/`fn()`).
    Int64 = 6,
    /// 128-bit integer in two GPRs (`u128`, `i128`).
    Int128 = 7,
    /// 16-bit float in FP/SIMD register (`f16`).
    Float16 = 8,
    /// 32-bit float in FP/SIMD register (`f32`).
    Float32 = 9,
    /// 64-bit float in FP/SIMD register (`f64`).
    Float64 = 10,
    /// 128-bit quad float in FP/SIMD register (`f128`).
    Float128 = 11,
    /// Two-register wide pointer with `usize` length metadata (`&[T]`, `&str`, `*const [T]`).
    WideSlice = 12,
    /// Two-register wide pointer with vtable pointer metadata (`&dyn Tr`, `*const dyn Tr`).
    WideDyn = 13,
    /// Passed indirectly by pointer in blaze's Rust ABI (`Pass::Ref`: tuples, arrays,
    /// non-transparent structs/enums, unions).
    Indirect = 14,
}

impl AbiClass {
    /// Maps a Rust-ABI [`Pass`] from `blaze_build::layout::Layout::rust_pass` to its
    /// canonical [`AbiClass`]. Returns `None` for non-Rust C-ABI passes or unsupported vector
    /// registers (conservatively keeping such types concrete).
    pub const fn from_pass(pass: Pass) -> Option<AbiClass> {
        match pass {
            Pass::Ignore => Some(AbiClass::Zst),
            Pass::Scalar { ty, ext, agg: _ } => match (ty, ext) {
                (LirTy::I8, Ext::Sext) => Some(AbiClass::Int8Sext),
                (LirTy::I8, Ext::Zext | Ext::None) => Some(AbiClass::Int8Zext),
                (LirTy::I16, Ext::Sext) => Some(AbiClass::Int16Sext),
                (LirTy::I16, Ext::Zext | Ext::None) => Some(AbiClass::Int16Zext),
                (LirTy::I32, _) => Some(AbiClass::Int32),
                (LirTy::I64 | LirTy::PTR, _) => Some(AbiClass::Int64),
                (LirTy::I128, _) => Some(AbiClass::Int128),
                (LirTy::F16, _) => Some(AbiClass::Float16),
                (LirTy::F32, _) => Some(AbiClass::Float32),
                (LirTy::F64, _) => Some(AbiClass::Float64),
                (LirTy::F128, _) => Some(AbiClass::Float128),
                _ => None,
            },
            Pass::Wide { metadata: LirTy::I64 } => Some(AbiClass::WideSlice),
            Pass::Wide { metadata: LirTy::PTR } => Some(AbiClass::WideDyn),
            Pass::Wide { .. } => None,
            Pass::Ref => Some(AbiClass::Indirect),
            Pass::Memory { .. } | Pass::Words { .. } => None,
        }
    }
}

/// Canonical description of a type's primary niche, derived from [`blaze_build::layout::Niche`].
///
/// Included in [`LayoutShape`] so that enum niche optimization (`Option<&Foo>` vs `Option<u64>`,
/// `Option<bool>` vs `Option<NonZero<u8>>`) never collides across incompatible validity ranges.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct NicheShape {
    /// Byte offset of the niche scalar inside the type.
    pub offset: u64,
    /// Size in bytes of the niche scalar (`1`, `2`, `4`, `8`, or `16`).
    pub scalar_bytes: u8,
    /// Inclusive wrapping valid-value range start (`WrappingRange::start`).
    pub valid_start: u128,
    /// Inclusive wrapping valid-value range end (`WrappingRange::end`).
    pub valid_end: u128,
}

impl NicheShape {
    /// Extracts a canonical [`NicheShape`] from a [`blaze_build::layout::Niche`].
    pub const fn from_niche(n: Niche) -> NicheShape {
        NicheShape {
            offset: n.offset,
            scalar_bytes: n.scalar.prim.size() as u8,
            valid_start: n.scalar.valid.start,
            valid_end: n.scalar.valid.end,
        }
    }

    /// Standard non-null 8-byte pointer niche (`offset = 0`, `1..=u64::MAX`), shared by `&T`,
    /// `&mut T`, `NonNull<T>`, `Box<T>`, `fn()`, and `NonZero<u64>` / `NonZero<usize>`.
    pub const NON_NULL_PTR: NicheShape =
        NicheShape { offset: 0, scalar_bytes: 8, valid_start: 1, valid_end: u64::MAX as u128 };

    /// Standard `bool` niche (`offset = 0`, `0..=1`, `None` encoded as `2`).
    pub const BOOL: NicheShape = NicheShape { offset: 0, scalar_bytes: 1, valid_start: 0, valid_end: 1 };

    /// Standard `char` niche (`offset = 0`, `0..=0x10FFFF`).
    pub const CHAR: NicheShape = NicheShape { offset: 0, scalar_bytes: 4, valid_start: 0, valid_end: 0x10_FFFF };
}

/// Complete, deterministic layout + ABI fingerprint of a sized, non-Drop ground type.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LayoutShape {
    /// Byte size (`Layout::size`).
    pub size: u64,
    /// Alignment in bytes (`Layout::align_bytes`, always a power of two).
    pub align: u64,
    /// Rust calling-convention class (`AbiClass`).
    pub abi: AbiClass,
    /// Primary niche, if any (controls `Option<T>` / enum discriminant placement).
    pub niche: Option<NicheShape>,
    /// Whether the type is uninhabited (e.g. `!`, `Infallible`).
    pub uninhabited: bool,
}

impl LayoutShape {
    /// Extracts a [`LayoutShape`] from a [`blaze_build::layout::Layout`] in `ground`.
    ///
    /// Returns `None` if the type is unsized (`layout.tail != Tail::Sized`) or has an unsupported
    /// calling-convention pass.
    pub fn from_layout(layout: &Layout, ground: PTys<'_>) -> Option<LayoutShape> {
        if layout.tail != Tail::Sized {
            return None;
        }
        let pass = layout.rust_pass(ground).ok()?;
        let abi = AbiClass::from_pass(pass)?;
        Some(LayoutShape {
            size: layout.size,
            align: layout.align_bytes(),
            abi,
            niche: layout.niche.map(NicheShape::from_niche),
            uninhabited: layout.uninhabited,
        })
    }

    /// Convenience constructor for plain inhabited types without niches (e.g. integers, raw
    /// pointers, floats, plain byte-bag aggregates).
    pub const fn plain(size: u64, align: u64, abi: AbiClass) -> LayoutShape {
        LayoutShape { size, align, abi, niche: None, uninhabited: false }
    }

    /// Convenience constructor for inhabited types with a known primary niche (e.g. `&T`, `bool`,
    /// `NonZero<T>`, `NonNull<T>`).
    pub const fn with_niche(size: u64, align: u64, abi: AbiClass, niche: NicheShape) -> LayoutShape {
        LayoutShape { size, align, abi, niche: Some(niche), uninhabited: false }
    }

    /// Unit `()` layout shape: size 0, align 1, ZST, inhabited, no niche.
    pub const UNIT: LayoutShape = LayoutShape::plain(0, 1, AbiClass::Zst);

    /// Never `!` / `Infallible` layout shape: size 0, align 1, ZST, uninhabited.
    pub const NEVER: LayoutShape =
        LayoutShape { size: 0, align: 1, abi: AbiClass::Zst, niche: None, uninhabited: true };

    /// 64-bit GPR scalar without niche (`u64`, `i64`, `usize`, `isize`, `*const T`, `*mut T`).
    pub const INT64: LayoutShape = LayoutShape::plain(8, 8, AbiClass::Int64);

    /// 64-bit non-null thin pointer/reference (`&T`, `&mut T`, `NonNull<T>`, `fn()`).
    pub const NON_NULL_PTR: LayoutShape = LayoutShape::with_niche(8, 8, AbiClass::Int64, NicheShape::NON_NULL_PTR);

    /// 64-bit IEEE float (`f64`).
    pub const FLOAT64: LayoutShape = LayoutShape::plain(8, 8, AbiClass::Float64);
}

/// Whether a generic argument is a type argument or a const generic argument.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum ParamKind {
    Type = 0,
    Const = 1,
}

/// Concrete facts about one ground generic argument at an instantiation site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArgInfo {
    /// Concrete ground type (or const value node) in the ground [`PortableTys`].
    pub ty: PTy,
    /// Whether this argument fills a type parameter or a const parameter.
    pub kind: ParamKind,
    /// `true` iff a pointer/reference to this type is thin (8 bytes, no metadata): i.e. `Tail::Sized`
    /// or `PNode::Foreign`. `false` for unsized DSTs (`str`, `[T]`, `dyn Tr`, DST structs) and
    /// const arguments.
    pub thin_pointee: bool,
    /// `true` iff `core::mem::needs_drop::<Self>()` is `true` (has custom `Drop` glue or drops a
    /// field/element that needs drop).
    pub needs_drop: bool,
    /// Layout + ABI fingerprint if the type is sized and has a pure Rust ABI; `None` if unsized,
    /// const, or non-canonicalizable.
    pub layout: Option<LayoutShape>,
}

impl ArgInfo {
    /// Builds [`ArgInfo`] directly from a [`blaze_build::layout::Layout`] in `ground` and its
    /// `needs_drop` result (`blaze_build::drop_facts::needs_drop`).
    pub fn from_layout(ty: PTy, layout: &Layout, needs_drop: bool, ground: PTys<'_>) -> ArgInfo {
        let is_foreign = matches!(ground.node(ty), Ok(PNode::Foreign(_)));
        let thin_pointee = layout.tail == Tail::Sized || is_foreign;
        let shape = LayoutShape::from_layout(layout, ground);
        ArgInfo { ty, kind: ParamKind::Type, thin_pointee, needs_drop, layout: shape }
    }

    /// Builds [`ArgInfo`] for a `PNode::Foreign` extern type (thin pointer, no size, no drop).
    pub const fn foreign(ty: PTy) -> ArgInfo {
        ArgInfo { ty, kind: ParamKind::Type, thin_pointee: true, needs_drop: false, layout: None }
    }

    /// Builds [`ArgInfo`] for an unsized DST (`str`, `[T]`, `dyn Tr`, DST struct).
    pub const fn unsized_ty(ty: PTy, needs_drop: bool) -> ArgInfo {
        ArgInfo { ty, kind: ParamKind::Type, thin_pointee: false, needs_drop, layout: None }
    }

    /// Builds [`ArgInfo`] for a const generic argument (`PNode::ConstValue`).
    pub const fn const_arg(ty: PTy) -> ArgInfo {
        ArgInfo { ty, kind: ParamKind::Const, thin_pointee: false, needs_drop: false, layout: None }
    }

    /// Builds [`ArgInfo`] for a sized type with known [`LayoutShape`] and `needs_drop`.
    pub const fn sized(ty: PTy, shape: LayoutShape, needs_drop: bool) -> ArgInfo {
        ArgInfo { ty, kind: ParamKind::Type, thin_pointee: true, needs_drop, layout: Some(shape) }
    }

    /// Lifts a [`CanonTy`] back into an [`ArgInfo`] (using `placeholder_ty` for non-concrete
    /// variants). Used to prove and exercise idempotency:
    /// `canonicalize_args(u, lift(canonicalize_args(u, args))) == canonicalize_args(u, args)`.
    pub const fn from_canon(ct: CanonTy, placeholder_ty: PTy) -> ArgInfo {
        match ct {
            CanonTy::Unused => {
                ArgInfo { ty: placeholder_ty, kind: ParamKind::Type, thin_pointee: true, needs_drop: false, layout: Some(LayoutShape::UNIT) }
            }
            CanonTy::OpaqueThin => {
                ArgInfo { ty: placeholder_ty, kind: ParamKind::Type, thin_pointee: true, needs_drop: false, layout: None }
            }
            CanonTy::Layout(shape) => {
                ArgInfo { ty: placeholder_ty, kind: ParamKind::Type, thin_pointee: true, needs_drop: false, layout: Some(shape) }
            }
            CanonTy::Concrete(ty) => {
                // Concrete stays concrete on re-canonicalization: mark non-thin, needs_drop, no layout.
                ArgInfo { ty, kind: ParamKind::Type, thin_pointee: false, needs_drop: true, layout: None }
            }
        }
    }
}

/// One canonicalized generic argument.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CanonTy {
    /// Generic parameter was `UNUSED` (never referenced, or only inside `PhantomData`).
    /// Canonicalized to unit `()`.
    Unused,
    /// Generic parameter was `OPAQUE_PTR` and concrete argument is a thin-pointer pointee.
    OpaqueThin,
    /// Generic parameter was `LAYOUT_ONLY`, concrete argument is sized, `!needs_drop`, and has
    /// pure Rust ABI [`LayoutShape`].
    Layout(LayoutShape),
    /// Generic parameter kept concrete (either `ParamUse` is `Concrete`, or concrete argument is
    /// unsized / `needs_drop` / non-canonicalizable ABI / const param).
    Concrete(PTy),
}

impl CanonTy {
    /// Returns `true` iff this argument was collapsed (not [`CanonTy::Concrete`]).
    #[inline]
    pub const fn is_canonicalized(self) -> bool {
        !matches!(self, CanonTy::Concrete(_))
    }
}

/// Result of canonicalizing an instance's generic arguments.
#[derive(Debug, PartialEq, Eq)]
pub struct CanonicalArgs<'a> {
    args: AVec<'a, CanonTy>,
    canonicalized_count: usize,
}

impl<'a> CanonicalArgs<'a> {
    /// The canonicalized generic arguments, in parameter order.
    #[inline]
    pub fn as_slice(&self) -> &[CanonTy] {
        self.args.as_slice()
    }

    /// Number of generic parameters that were canonicalized (collapsed away from concrete).
    #[inline]
    pub fn canonicalized_count(&self) -> usize {
        self.canonicalized_count
    }

    /// `true` iff at least one parameter was canonicalized.
    #[inline]
    pub fn is_changed(&self) -> bool {
        self.canonicalized_count > 0
    }

    /// Total number of generic arguments.
    #[inline]
    pub fn len(&self) -> usize {
        self.args.len()
    }

    /// `true` iff there are zero generic arguments.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.args.is_empty()
    }
}

/// Canonicalizes concrete instantiation arguments `args` against a function's propagated
/// `usage` ([`ParamUse`] per generic parameter).
///
/// Soundness guarantees:
/// - `Unused` -> `CanonTy::Unused` (even for `Drop` or unsized types, because the parameter is
///   never observed).
/// - `OpaquePtr` -> `CanonTy::OpaqueThin` iff `arg.kind == ParamKind::Type && arg.thin_pointee`,
///   else `CanonTy::Concrete(arg.ty)`.
/// - `LayoutOnly` -> `CanonTy::Layout(shape)` iff `arg.kind == ParamKind::Type && !arg.needs_drop`
///   and `arg.layout == Some(shape)`, else `CanonTy::Concrete(arg.ty)`.
/// - `Concrete` -> always `CanonTy::Concrete(arg.ty)`.
pub fn canonicalize_args<'a>(
    usage: &[ParamUse],
    args: &[ArgInfo],
    alloc: Alloc<'a>,
) -> Result<CanonicalArgs<'a>, PolyError> {
    if usage.len() != args.len() {
        return Err(PolyError::ArgCountMismatch { expected: usage.len(), got: args.len() });
    }
    let mut out: AVec<'a, CanonTy> = Vec::with_capacity_in(args.len(), alloc);
    let mut canonicalized_count = 0usize;
    for (&u, &arg) in usage.iter().zip(args.iter()) {
        let ct = match u.level() {
            CanonLevel::Unused => CanonTy::Unused,
            CanonLevel::OpaquePtr => {
                if arg.kind == ParamKind::Type && arg.thin_pointee {
                    CanonTy::OpaqueThin
                } else {
                    CanonTy::Concrete(arg.ty)
                }
            }
            CanonLevel::LayoutOnly => {
                if arg.kind == ParamKind::Type && !arg.needs_drop {
                    match arg.layout {
                        Some(shape) => CanonTy::Layout(shape),
                        None => CanonTy::Concrete(arg.ty),
                    }
                } else {
                    CanonTy::Concrete(arg.ty)
                }
            }
            CanonLevel::Concrete => CanonTy::Concrete(arg.ty),
        };
        if ct.is_canonicalized() {
            canonicalized_count += 1;
        }
        out.push(ct);
    }
    Ok(CanonicalArgs { args: out, canonicalized_count })
}

/// Re-canonicalizes an already canonicalized argument slice under `usage`.
///
/// Satisfies strict idempotency: for any `usage` and `args`,
/// `canonicalize_canon(usage, canonicalize_args(usage, args)?.as_slice()) == canonicalize_args(usage, args)?`.
pub fn canonicalize_canon<'a>(
    usage: &[ParamUse],
    canon: &[CanonTy],
    alloc: Alloc<'a>,
) -> Result<CanonicalArgs<'a>, PolyError> {
    if usage.len() != canon.len() {
        return Err(PolyError::ArgCountMismatch { expected: usage.len(), got: canon.len() });
    }
    let mut out: AVec<'a, CanonTy> = Vec::with_capacity_in(canon.len(), alloc);
    let mut canonicalized_count = 0usize;
    for (&u, &ct) in usage.iter().zip(canon.iter()) {
        let next = match (u.level(), ct) {
            (CanonLevel::Unused, _) => CanonTy::Unused,
            (CanonLevel::OpaquePtr, CanonTy::OpaqueThin | CanonTy::Unused | CanonTy::Layout(_)) => {
                CanonTy::OpaqueThin
            }
            (CanonLevel::OpaquePtr, CanonTy::Concrete(ty)) => CanonTy::Concrete(ty),
            (CanonLevel::LayoutOnly, CanonTy::Layout(shape)) => CanonTy::Layout(shape),
            (CanonLevel::LayoutOnly, CanonTy::Unused) => CanonTy::Layout(LayoutShape::UNIT),
            (CanonLevel::LayoutOnly, CanonTy::OpaqueThin) => CanonTy::OpaqueThin,
            (CanonLevel::LayoutOnly, CanonTy::Concrete(ty)) => CanonTy::Concrete(ty),
            (CanonLevel::Concrete, other) => other,
        };
        if next.is_canonicalized() {
            canonicalized_count += 1;
        }
        out.push(next);
    }
    Ok(CanonicalArgs { args: out, canonicalized_count })
}

// ---------------------------------------------------------------------------
// ADT Parameter Usage Analysis
// ---------------------------------------------------------------------------

/// How an ADT `S<P_0, ..., P_{n-1}>`'s memory layout uses its generic parameter `P_i`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum AdtParamUse {
    /// Parameter is not stored in any field (or only inside `PhantomData`).
    Unused = 0,
    /// Parameter only appears behind thin pointers/refs/`NonNull` (e.g. `NonNull<T>`,
    /// `Unique<T>`, `RawVec<T>`, `Vec<T>`).
    OpaquePtr = 1,
    /// Parameter is embedded by value in fields (e.g. `Option<T>`, `Result<T, E>`, `MaybeUninit<T>`).
    ByValue = 2,
    /// Parameter appears in an associated type projection (`<T as Tr>::Assoc`), opaque type,
    /// `dyn`, const length, or unknown ADT — layout depends on concrete `T`.
    Concrete = 3,
}

impl AdtParamUse {
    #[inline]
    pub const fn join(self, other: AdtParamUse) -> AdtParamUse {
        if (self as u8) >= (other as u8) { self } else { other }
    }

    #[inline]
    pub fn join_assign(&mut self, other: AdtParamUse) -> bool {
        let next = self.join(other);
        let changed = next != *self;
        *self = next;
        changed
    }
}

/// Read-only facts supplied to the sans-IO polymorphization analyzer (Rule 14, Rule 75).
pub trait AnalysisFacts {
    /// Well-known `core::marker::PhantomData` ADT key, if present.
    fn phantom_data(&self) -> Option<ItemKey> {
        None
    }

    /// Well-known `core::ptr::NonNull` ADT key, if present.
    fn non_null(&self) -> Option<ItemKey> {
        None
    }

    /// Well-known `core::mem::ManuallyDrop` ADT key, if present.
    fn manually_drop(&self) -> Option<ItemKey> {
        None
    }

    /// Well-known `alloc::boxed::Box` ADT key, if present (needed only for `Proj::DerefBox`
    /// type peeling).
    fn own_box(&self) -> Option<ItemKey> {
        None
    }

    /// Returns `(param_layout_uses, has_drop_impl)` for ADT `adt`, if known.
    ///
    /// Returning `None` is always sound: unknown ADTs are conservatively treated as
    /// `AdtParamUse::Concrete` with `has_drop_impl = true`.
    fn adt_summary(&self, adt: ItemKey) -> Option<(&[AdtParamUse], bool)> {
        let _ = adt;
        None
    }

    /// Returns the `<Adt as Drop>::drop` function item key for `adt`, if `adt` implements `Drop`
    /// and its drop function is known (so drop calls participate in call-graph propagation).
    fn adt_drop_fn(&self, adt: ItemKey) -> Option<ItemKey> {
        let _ = adt;
        None
    }

    /// Classifies an intrinsic item key as a `blaze_build::intrinsics::Which`, if known.
    ///
    /// Returning `None` is always sound: unknown intrinsics conservatively force `Concrete`.
    fn intrinsic_which(&self, item: ItemKey) -> Option<Which> {
        let _ = item;
        None
    }

    /// Returns `true` iff `item` is a trait method declaration whose concrete callee depends on
    /// trait impl selection (forcing `ParamUse::TRAIT_DISPATCH`).
    ///
    /// Returning `false` is still sound when trait methods are absent from `analyses` (unanalyzed
    /// callees conservatively force `UNANALYZED_CALLEE` = `Concrete`), but returning `true` tags
    /// `TRAIT_DISPATCH` immediately and avoids chasing trait method edges.
    fn is_trait_method(&self, item: ItemKey) -> bool {
        let _ = item;
        false
    }
}

/// Empty facts implementation: 100% conservative default (every unknown ADT/intrinsic forces
/// `Concrete`).
pub struct NoFacts;

impl AnalysisFacts for NoFacts {}

/// Input describing one ADT's fields for [`analyze_adt_fields`] / [`analyze_adt_graph`].
#[derive(Clone, Copy)]
pub struct AdtDefInput<'a> {
    pub key: ItemKey,
    pub param_count: u32,
    pub field_tys: &'a [PTy],
    pub has_drop_impl: bool,
    pub tys: PTys<'a>,
}

/// Flat CSR result of [`analyze_adt_graph`].
pub struct AdtGraphSummary<'a> {
    keys: AVec<'a, (ItemKey, u32, u32, bool)>,
    uses: AVec<'a, AdtParamUse>,
}

impl<'a> AdtGraphSummary<'a> {
    /// Looks up `(param_uses, has_drop_impl)` for `adt` in `O(log N)`.
    pub fn get(&self, adt: ItemKey) -> Option<(&[AdtParamUse], bool)> {
        let idx = self.keys.binary_search_by_key(&adt, |row| row.0).ok()?;
        let (_, start, len, has_drop) = self.keys[idx];
        Some((&self.uses[start as usize..(start + len) as usize], has_drop))
    }
}

/// Analyzes one ADT's field types to determine its [`AdtParamUse`] per generic parameter, using
/// `facts` for nested ADTs.
pub fn analyze_adt_fields<'a>(
    input: AdtDefInput<'_>,
    facts: &dyn AnalysisFacts,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, AdtParamUse>, PolyError> {
    if input.param_count > MAX_GENERIC_PARAMS {
        return Err(PolyError::TooManyParams { count: input.param_count });
    }
    let mut out: AVec<'a, AdtParamUse> = Vec::with_capacity_in(input.param_count as usize, alloc);
    out.resize(input.param_count as usize, AdtParamUse::Unused);
    if facts.phantom_data() == Some(input.key) {
        return Ok(out);
    }
    if facts.non_null() == Some(input.key) {
        if let Some(slot) = out.first_mut() {
            *slot = AdtParamUse::OpaquePtr;
        }
        return Ok(out);
    }
    let mut stack: AVec<'_, (PTy, WalkCtx)> = Vec::new_in(alloc);
    for &field_ty in input.field_tys {
        walk_adt_field_ty(
            field_ty,
            WalkCtx::ByValue,
            input.tys,
            facts,
            None,
            &mut out,
            &mut stack,
        )?;
    }
    Ok(out)
}

/// Computes the fixed-point [`AdtGraphSummary`] for a set of mutually referencing ADTs
/// (e.g. `Vec<T>` -> `RawVec<T>` -> `Unique<T>` -> `NonNull<T>` + `PhantomData<T>`).
pub fn analyze_adt_graph<'a>(
    adts: &[AdtDefInput<'_>],
    external_facts: &dyn AnalysisFacts,
    alloc: Alloc<'a>,
) -> Result<AdtGraphSummary<'a>, PolyError> {
    let mut keys: AVec<'a, (ItemKey, u32, u32, bool)> = Vec::with_capacity_in(adts.len(), alloc);
    let mut total_params: usize = 0;
    for a in adts {
        if a.param_count > MAX_GENERIC_PARAMS {
            return Err(PolyError::TooManyParams { count: a.param_count });
        }
        total_params += a.param_count as usize;
    }
    let mut uses: AVec<'a, AdtParamUse> = Vec::with_capacity_in(total_params, alloc);
    uses.resize(total_params, AdtParamUse::Unused);

    // Build deterministic index sorted by ItemKey so lookups are O(log N) and order-independent.
    let mut sorted_order: AVec<'_, (ItemKey, usize)> = Vec::with_capacity_in(adts.len(), alloc);
    for (idx, a) in adts.iter().enumerate() {
        sorted_order.push((a.key, idx));
    }
    sorted_order.sort_unstable_by_key(|row| row.0);

    let mut offset: u32 = 0;
    for &(key, orig_idx) in sorted_order.iter() {
        let a = &adts[orig_idx];
        keys.push((key, offset, a.param_count, a.has_drop_impl));
        if external_facts.non_null() == Some(key) && a.param_count > 0 {
            uses[offset as usize] = AdtParamUse::OpaquePtr;
        }
        offset += a.param_count;
    }

    let mut stack: AVec<'_, (PTy, WalkCtx)> = Vec::new_in(alloc);
    let mut scratch_out: AVec<'_, AdtParamUse> = Vec::new_in(alloc);

    for _iter in 0..MAX_FIXED_POINT_ITERS {
        let mut changed = false;
        for (pos, &(key, orig_idx)) in sorted_order.iter().enumerate() {
            if external_facts.phantom_data() == Some(key) || external_facts.non_null() == Some(key) {
                continue;
            }
            let a = &adts[orig_idx];
            let (_, start, len, _) = keys[pos];
            let start = start as usize;
            let len = len as usize;
            scratch_out.clear();
            scratch_out.extend_from_slice(&uses[start..start + len]);
            let graph_lookup = AdtGraphView { keys: keys.as_slice(), uses: uses.as_slice() };
            for &field_ty in a.field_tys {
                walk_adt_field_ty(
                    field_ty,
                    WalkCtx::ByValue,
                    a.tys,
                    external_facts,
                    Some(&graph_lookup),
                    &mut scratch_out,
                    &mut stack,
                )?;
            }
            for i in 0..len {
                if uses[start + i].join_assign(scratch_out[i]) {
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(AdtGraphSummary { keys, uses });
        }
    }
    Err(PolyError::IterationCap { cap: MAX_FIXED_POINT_ITERS })
}

struct AdtGraphView<'a> {
    keys: &'a [(ItemKey, u32, u32, bool)],
    uses: &'a [AdtParamUse],
}

impl AdtGraphView<'_> {
    fn get(&self, adt: ItemKey) -> Option<(&[AdtParamUse], bool)> {
        let idx = self.keys.binary_search_by_key(&adt, |row| row.0).ok()?;
        let (_, start, len, has_drop) = self.keys[idx];
        Some((&self.uses[start as usize..(start + len) as usize], has_drop))
    }
}

fn walk_adt_field_ty(
    root: PTy,
    root_ctx: WalkCtx,
    tys: PTys<'_>,
    facts: &dyn AnalysisFacts,
    graph: Option<&AdtGraphView<'_>>,
    out: &mut [AdtParamUse],
    stack: &mut AVec<'_, (PTy, WalkCtx)>,
) -> Result<(), PolyError> {
    stack.clear();
    stack.push((root, root_ctx));
    let mut visited: usize = 0;
    while let Some((ty, ctx)) = stack.pop() {
        visited += 1;
        if visited > MAX_TYPE_WALK_NODES {
            return Err(PolyError::TypeWalkCap { cap: MAX_TYPE_WALK_NODES });
        }
        let node = tys.node(ty).map_err(|_| PolyError::Ty(TyError::NotInRecord(ty)))?;
        match node {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Foreign(_) => {}
            PNode::Param(i) => {
                let idx = i as usize;
                let Some(slot) = out.get_mut(idx) else {
                    return Err(PolyError::ParamOutOfRange { index: i, param_count: out.len() as u32 });
                };
                slot.join_assign(ctx.to_adt_use());
            }
            PNode::ConstParam(i) => {
                let idx = i as usize;
                let Some(slot) = out.get_mut(idx) else {
                    return Err(PolyError::ParamOutOfRange { index: i, param_count: out.len() as u32 });
                };
                if ctx != WalkCtx::Phantom {
                    slot.join_assign(AdtParamUse::Concrete);
                }
            }
            PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
                let next_ctx = match ctx {
                    WalkCtx::Phantom => WalkCtx::Phantom,
                    WalkCtx::Concrete => WalkCtx::Concrete,
                    WalkCtx::BehindThinPtr | WalkCtx::ByValue => WalkCtx::BehindThinPtr,
                };
                // `&[T]` / `*const [T]` is a 16-byte wide pointer independent of `T`'s layout.
                let inner_node = tys.node(inner).map_err(|_| PolyError::Ty(TyError::NotInRecord(inner)))?;
                if let PNode::Slice(elem) = inner_node {
                    stack.push((elem, next_ctx));
                } else {
                    stack.push((inner, next_ctx));
                }
            }
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                let items = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                for &child in items {
                    stack.push((child, ctx));
                }
            }
            PNode::Array(elem, len) => {
                stack.push((elem, ctx));
                let len_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                stack.push((len, len_ctx));
            }
            PNode::Slice(elem) => stack.push((elem, ctx)),
            PNode::Pat(base, ranges) => {
                stack.push((base, ctx));
                let items = tys.list(ranges).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(ranges)))?;
                for &r in items {
                    stack.push((r, WalkCtx::Concrete));
                }
            }
            PNode::ConstValue { ty: cty, .. } => {
                let c_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                stack.push((cty, c_ctx));
            }
            PNode::FnPtr(l, header) => {
                let items = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                let next_ctx = if ctx == WalkCtx::Phantom {
                    WalkCtx::Phantom
                } else if header.abi != Abi::Rust || header.c_variadic {
                    WalkCtx::Concrete
                } else {
                    WalkCtx::ByValue
                };
                for &child in items {
                    stack.push((child, next_ctx));
                }
            }
            PNode::Adt(k, l) => {
                let args = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                if ctx == WalkCtx::Phantom || facts.phantom_data() == Some(k) {
                    for &arg in args {
                        stack.push((arg, WalkCtx::Phantom));
                    }
                } else if facts.non_null() == Some(k) {
                    let next_ctx = if ctx == WalkCtx::Concrete { WalkCtx::Concrete } else { WalkCtx::BehindThinPtr };
                    for &arg in args {
                        stack.push((arg, next_ctx));
                    }
                } else if let Some((param_uses, _)) = graph.and_then(|g| g.get(k)).or_else(|| facts.adt_summary(k)) {
                    if param_uses.len() == args.len() {
                        for (&arg, &pu) in args.iter().zip(param_uses.iter()) {
                            stack.push((arg, combine_adt_ctx(ctx, pu)));
                        }
                    } else {
                        for &arg in args {
                            stack.push((arg, WalkCtx::Concrete));
                        }
                    }
                } else {
                    for &arg in args {
                        stack.push((arg, WalkCtx::Concrete));
                    }
                }
            }
            // Projections, opaques, trait objects, closures, coroutines, fn items, holes, errors:
            // conservatively concrete.
            PNode::FnDef(_, l)
            | PNode::Alias { args: l, .. }
            | PNode::Opaque(_, l)
            | PNode::Closure { args: l, .. }
            | PNode::Coroutine { args: l, .. }
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => {
                let items = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                for &child in items {
                    stack.push((child, WalkCtx::Concrete));
                }
            }
            PNode::ExProj { args, term, .. } => {
                let items = tys.list(args).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(args)))?;
                for &child in items {
                    stack.push((child, WalkCtx::Concrete));
                }
                stack.push((term, WalkCtx::Concrete));
            }
            PNode::Future(_, a, b) => {
                stack.push((a, WalkCtx::Concrete));
                stack.push((b, WalkCtx::Concrete));
            }
            PNode::ExAuto(_) | PNode::Infer | PNode::Error => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Intrinsic Classification
// ---------------------------------------------------------------------------

/// How a compiler intrinsic uses its generic type arguments.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IntrinsicClass {
    /// No type-parameter dependence (e.g. `likely`, `assume`, `abort_immediate`, `ub_checks`).
    NoTypeDependence,
    /// Only inspects raw thin pointer bits (`ptr_guaranteed_cmp`, `ptr_mask`).
    OpaquePtr,
    /// Depends only on `size_of` / `align_of` / `!needs_drop` / raw memory copy/move
    /// (`size_of`, `align_of`, `read_via_copy`, `write_via_move`, `copy_nonoverlapping`,
    /// `offset`, `needs_drop`, `forget`, `black_box`, ...).
    LayoutOnly,
    /// Identity or compile-time reflection (`type_id`, `type_name`, `type_info`, `variant_count`,
    /// `blaze_*`). Forces [`ParamUse::REFLECTION`] (`Concrete`).
    Reflection,
    /// Type-specific value, arithmetic, atomic, SIMD, validity, or discriminant operation.
    /// Forces [`ParamUse::VALUE_OP`] (`Concrete`).
    ValueOp,
    /// Unsized pointer metadata manipulation (`ptr_metadata`, `aggregate_raw_ptr`).
    /// Forces [`ParamUse::UNSIZED_META`] (`Concrete`).
    UnsizedMeta,
}

impl IntrinsicClass {
    /// Exhaustively classifies every [`blaze_build::intrinsics::Which`] intrinsic.
    ///
    /// No wildcard arm: adding a new intrinsic forces explicit soundness classification (Rule 73).
    pub const fn of(which: Which) -> IntrinsicClass {
        match which {
            Which::Likely
            | Which::Unlikely
            | Which::Assume
            | Which::ColdPath
            | Which::AllocShimMarker
            | Which::Unreachable
            | Which::AbortImmediate
            | Which::AtomicFence
            | Which::AtomicSingleThreadFence
            | Which::CallerLocation
            | Which::UbChecks
            | Which::OverflowChecks
            | Which::CompareBytes => IntrinsicClass::NoTypeDependence,

            Which::PtrGuaranteedCmp | Which::PtrMask => IntrinsicClass::OpaquePtr,

            Which::SizeOf
            | Which::AlignOf
            | Which::SizeOfVal
            | Which::AlignOfVal
            | Which::NeedsDrop
            | Which::Forget
            | Which::BlackBox
            | Which::Copy
            | Which::CopyNonoverlapping
            | Which::WriteBytes
            | Which::VolatileCopyMemory
            | Which::VolatileCopyNonoverlappingMemory
            | Which::VolatileSetMemory
            | Which::ReadViaCopy
            | Which::WriteViaMove
            | Which::WriteBoxViaMove
            | Which::VolatileLoad
            | Which::VolatileStore
            | Which::UnalignedVolatileLoad
            | Which::UnalignedVolatileStore
            | Which::Offset
            | Which::ArithOffset
            | Which::PtrOffsetFrom
            | Which::PtrOffsetFromUnsigned
            | Which::SliceGetUnchecked => IntrinsicClass::LayoutOnly,

            Which::TypeName
            | Which::TypeId
            | Which::TypeIdTypeOf
            | Which::TypeIdVariants
            | Which::TypeIdFields
            | Which::TypeIdFieldRepresentingType
            | Which::FieldRepresentingTypeActualTypeId
            | Which::FieldRepresentingTypeOffset
            | Which::TypeIdIsSigned
            | Which::TypeIdArrayLen
            | Which::TypeIdElementTy
            | Which::TypeIdSizeOf
            | Which::TypeIdPointsTo
            | Which::TypeIdPointsMutably
            | Which::VariantCount
            | Which::BlazeFieldCount
            | Which::BlazeIsEnum
            | Which::BlazeIsUnitEnum
            | Which::BlazeRecordSchemaId
            | Which::BlazeRecordsDisjoint
            | Which::BlazeVariantCount
            | Which::BlazeVariantName => IntrinsicClass::Reflection,

            Which::PtrMetadata | Which::AggregateRawPtr => IntrinsicClass::UnsizedMeta,

            Which::Transmute
            | Which::TransmuteUnchecked
            | Which::WrappingAdd
            | Which::WrappingSub
            | Which::WrappingMul
            | Which::AddWithOverflow
            | Which::SubWithOverflow
            | Which::MulWithOverflow
            | Which::SaturatingAdd
            | Which::SaturatingSub
            | Which::UncheckedAdd
            | Which::UncheckedSub
            | Which::UncheckedMul
            | Which::UncheckedDiv
            | Which::UncheckedRem
            | Which::ExactDiv
            | Which::UncheckedShl
            | Which::UncheckedShr
            | Which::Ctpop
            | Which::Ctlz
            | Which::CtlzNonzero
            | Which::Cttz
            | Which::CttzNonzero
            | Which::Bswap
            | Which::Bitreverse
            | Which::RotateLeft
            | Which::RotateRight
            | Which::AssertInhabited
            | Which::AssertZeroValid
            | Which::AssertMemUninitializedValid
            | Which::DiscriminantValue
            | Which::AtomicLoad
            | Which::AtomicStore
            | Which::AtomicXchg
            | Which::AtomicXadd
            | Which::AtomicXsub
            | Which::AtomicAnd
            | Which::AtomicNand
            | Which::AtomicOr
            | Which::AtomicXor
            | Which::AtomicMax
            | Which::AtomicMin
            | Which::AtomicUmax
            | Which::AtomicUmin
            | Which::AtomicCxchg
            | Which::AtomicCxchgWeak
            | Which::ConstEvalSelect
            | Which::CatchUnwind
            | Which::ThreeWayCompare
            | Which::FloatToIntUnchecked
            | Which::VaArg
            | Which::RawEq
            | Which::BlazeIntCast
            | Which::Simd(_) => IntrinsicClass::ValueOp,
        }
    }
}

// ---------------------------------------------------------------------------
// Per-Function Body Analysis (`analyze_body`)
// ---------------------------------------------------------------------------

/// Direct call or drop edge from an analyzed function to a callee item with generic arguments.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallEdge {
    pub callee: ItemKey,
    pub args_start: u32,
    pub args_len: u32,
    /// `true` when this edge comes from dropping an ADT with a custom `Drop` impl.
    pub is_drop_edge: bool,
}

/// Local parameter-usage analysis of one function body before interprocedural propagation.
pub struct FnAnalysis<'a> {
    pub item: ItemKey,
    pub param_count: u32,
    /// Local [`ParamUse`] bitflags per generic parameter `0..param_count`.
    pub local_use: AVec<'a, ParamUse>,
    /// Direct function-item calls, reified fn items, and custom `Drop` impl calls.
    pub call_edges: AVec<'a, CallEdge>,
    /// Flat arena of callee generic argument [`PTy`]s (indexed by [`CallEdge::args_start`] and
    /// [`CallEdge::args_len`]).
    pub edge_args: AVec<'a, PTy>,
}

impl<'a> FnAnalysis<'a> {
    /// Generic arguments passed on `edge`.
    #[inline]
    pub fn edge_arg_slice(&self, edge: &CallEdge) -> &[PTy] {
        let start = edge.args_start as usize;
        let end = start + edge.args_len as usize;
        &self.edge_args[start..end]
    }
}

/// Context during a type tree walk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WalkCtx {
    Phantom,
    BehindThinPtr,
    ByValue,
    Concrete,
}

impl WalkCtx {
    #[inline]
    const fn to_adt_use(self) -> AdtParamUse {
        match self {
            WalkCtx::Phantom => AdtParamUse::Unused,
            WalkCtx::BehindThinPtr => AdtParamUse::OpaquePtr,
            WalkCtx::ByValue => AdtParamUse::ByValue,
            WalkCtx::Concrete => AdtParamUse::Concrete,
        }
    }

    #[inline]
    const fn to_param_use(self, layout_flag: ParamUse, concrete_flag: ParamUse) -> ParamUse {
        match self {
            WalkCtx::Phantom => ParamUse::PHANTOM,
            WalkCtx::BehindThinPtr => ParamUse::OPAQUE_PTR,
            WalkCtx::ByValue => layout_flag,
            WalkCtx::Concrete => concrete_flag,
        }
    }
}

#[inline]
const fn combine_adt_ctx(outer: WalkCtx, adt_use: AdtParamUse) -> WalkCtx {
    match (outer, adt_use) {
        (WalkCtx::Phantom, _) | (_, AdtParamUse::Unused) => WalkCtx::Phantom,
        (WalkCtx::Concrete, _) | (_, AdtParamUse::Concrete) => WalkCtx::Concrete,
        (_, AdtParamUse::OpaquePtr) => WalkCtx::BehindThinPtr,
        (WalkCtx::BehindThinPtr, AdtParamUse::ByValue) => WalkCtx::BehindThinPtr,
        (WalkCtx::ByValue, AdtParamUse::ByValue) => WalkCtx::ByValue,
    }
}

/// Analyzes one `blaze_body::BodyRef` to compute local [`ParamUse`] bitflags per generic
/// parameter and collect all transitive callee/drop edges.
///
/// Works directly over `BodyRef` (including overlay types via `BodyRef::ty` / `BodyRef::ty_list`)
/// with zero recursion and explicit `Alloc`.
pub fn analyze_body<'a>(
    item: ItemKey,
    param_count: u32,
    abi: Abi,
    body: BodyRef<'_>,
    tys: PTys<'_>,
    facts: &dyn AnalysisFacts,
    alloc: Alloc<'a>,
) -> Result<FnAnalysis<'a>, PolyError> {
    if param_count > MAX_GENERIC_PARAMS {
        return Err(PolyError::TooManyParams { count: param_count });
    }
    let mut local_use: AVec<'a, ParamUse> = Vec::with_capacity_in(param_count as usize, alloc);
    local_use.resize(param_count as usize, ParamUse::NONE);
    let mut call_edges: AVec<'a, CallEdge> = Vec::new_in(alloc);
    let mut edge_args: AVec<'a, PTy> = Vec::new_in(alloc);
    let mut stack: AVec<'_, (PTy, WalkCtx)> = Vec::new_in(alloc);
    let mut proj_buf: AVec<'_, Proj> = Vec::new_in(alloc);

    let mut cx = BodyWalkCx {
        body,
        tys,
        facts,
        local_use: local_use.as_mut_slice(),
        call_edges: &mut call_edges,
        edge_args: &mut edge_args,
        stack: &mut stack,
        proj_buf: &mut proj_buf,
    };

    // 1. Walk all locals (return place, arguments, bindings, temporaries).
    let arg_count = body.args();
    for l_idx in 0..body.locals() {
        let decl = body.local(Local(l_idx))?;
        let is_sig_local = l_idx <= arg_count;
        if is_sig_local && abi != Abi::Rust {
            // Non-Rust ABI (e.g. extern "C" AAPCS64 HFA) depends on concrete struct fields.
            cx.walk_ty(decl.ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::NON_RUST_ABI)?;
        } else {
            cx.walk_ty(decl.ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
        }
    }

    // 2. Walk all constants declared in the body.
    for c_idx in 0..body.consts() {
        let decl = body.konst(blaze_body::Const(c_idx))?;
        match decl.kind {
            ConstKind::Scalar { .. } | ConstKind::Bytes { .. } | ConstKind::StaticAddr(_) => {
                cx.walk_ty(decl.ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
            }
            ConstKind::Param(p_idx) => {
                cx.mark_param(p_idx, ParamUse::CONST_PARAM)?;
            }
            ConstKind::Item { key: _, args } => {
                // Generic const item (e.g. `<T as Trait>::CONST`): value depends on concrete args.
                cx.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
            }
            ConstKind::FnItem { key, args } => {
                cx.record_edge(key, args, false)?;
            }
            ConstKind::Promoted(_) | ConstKind::Anon(_) => {
                // CRITICAL SOUNDNESS GUARD: inline `const { ... }` blocks (e.g. core's
                // `TypeId::of::<T>()` and `type_name::<T>()`) and promoted constants may mention
                // ANY generic parameter of the enclosing item even when `decl.ty` does not!
                cx.mark_all_params(ParamUse::UNANALYZED_CALLEE);
            }
        }
    }

    // 3. Walk all statements and terminators.
    for s_idx in 0..body.stmts() {
        let row = body.stmt(Stmt(s_idx))?;
        match row.kind {
            StmtKind::Assign { dest, value } => {
                cx.visit_place(dest, PlaceUse::ByValue)?;
                cx.visit_value(value)?;
            }
            StmtKind::StorageLive(_) | StmtKind::StorageDead(_) | StmtKind::Activate(_) => {}
            StmtKind::FakeRead(place) | StmtKind::Deinit(place) => {
                cx.visit_place(place, PlaceUse::ProjOnly)?;
            }
            StmtKind::SetDiscriminant { place, .. } => {
                let pty = cx.visit_place(place, PlaceUse::ProjOnly)?;
                cx.walk_ty(pty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
            }
            StmtKind::Goto(_)
            | StmtKind::Return
            | StmtKind::Unreachable
            | StmtKind::Resume
            | StmtKind::Terminate
            | StmtKind::Cancelled
            | StmtKind::Suspend { .. } => {}
            StmtKind::Switch { on, .. } => {
                cx.visit_operand(on)?;
            }
            StmtKind::Call { callee, args, dest, .. } => {
                cx.visit_callee(callee)?;
                for op_idx in args.start..args.end {
                    let op = body.operand(op_idx)?;
                    cx.visit_operand(op)?;
                }
                if let Some(d) = dest {
                    cx.visit_place(d, PlaceUse::ByValue)?;
                }
            }
            StmtKind::TailCall { callee, args } => {
                cx.visit_callee(callee)?;
                for op_idx in args.start..args.end {
                    let op = body.operand(op_idx)?;
                    cx.visit_operand(op)?;
                }
            }
            StmtKind::Drop { place, .. } => {
                let pty = cx.visit_place(place, PlaceUse::ProjOnly)?;
                cx.walk_drop_ty(pty)?;
            }
            StmtKind::Assert { cond, msg, .. } => {
                cx.visit_operand(cond)?;
                for op_idx in msg.start..msg.end {
                    let op = body.operand(op_idx)?;
                    cx.visit_operand(op)?;
                }
            }
            StmtKind::InlineAsm { .. } => {
                // Inline asm templates/constraints can reference const params or specific registers.
                cx.mark_all_params(ParamUse::UNANALYZED_CALLEE);
            }
        }
    }

    Ok(FnAnalysis { item, param_count, local_use, call_edges, edge_args })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlaceUse {
    /// Place value is loaded/stored by value.
    ByValue,
    /// Place is only projected (e.g. borrow, len, drop, discriminant).
    ProjOnly,
}

struct BodyWalkCx<'b, 'r, 'f, 's, 'e> {
    body: BodyRef<'b>,
    tys: PTys<'r>,
    facts: &'f dyn AnalysisFacts,
    local_use: &'s mut [ParamUse],
    call_edges: &'s mut AVec<'e, CallEdge>,
    edge_args: &'s mut AVec<'e, PTy>,
    stack: &'s mut AVec<'e, (PTy, WalkCtx)>,
    proj_buf: &'s mut AVec<'e, Proj>,
}

impl<'e> BodyWalkCx<'_, '_, '_, '_, 'e> {
    fn mark_param(&mut self, index: u32, flag: ParamUse) -> Result<(), PolyError> {
        let Some(slot) = self.local_use.get_mut(index as usize) else {
            return Err(PolyError::ParamOutOfRange { index, param_count: self.local_use.len() as u32 });
        };
        *slot |= flag;
        Ok(())
    }

    fn mark_all_params(&mut self, flag: ParamUse) {
        for slot in self.local_use.iter_mut() {
            *slot |= flag;
        }
    }

    fn record_edge(&mut self, callee: ItemKey, args: List, is_drop_edge: bool) -> Result<(), PolyError> {
        let list = self.body.ty_list(&self.tys, args)?;
        let start = self.edge_args.len() as u32;
        for arg in list.iter() {
            self.edge_args.push(arg);
        }
        let len = self.edge_args.len() as u32 - start;
        self.call_edges.push(CallEdge { callee, args_start: start, args_len: len, is_drop_edge });
        Ok(())
    }

    fn walk_list(
        &mut self,
        list: List,
        ctx: WalkCtx,
        layout_flag: ParamUse,
        concrete_flag: ParamUse,
    ) -> Result<(), PolyError> {
        let items = self.body.ty_list(&self.tys, list)?;
        for t in items.iter() {
            self.walk_ty(t, ctx, layout_flag, concrete_flag)?;
        }
        Ok(())
    }

    fn walk_ty(
        &mut self,
        root: PTy,
        root_ctx: WalkCtx,
        layout_flag: ParamUse,
        concrete_flag: ParamUse,
    ) -> Result<(), PolyError> {
        self.stack.clear();
        self.stack.push((root, root_ctx));
        let mut visited: usize = 0;
        while let Some((ty, ctx)) = self.stack.pop() {
            visited += 1;
            if visited > MAX_TYPE_WALK_NODES {
                return Err(PolyError::TypeWalkCap { cap: MAX_TYPE_WALK_NODES });
            }
            let node = self.body.ty(&self.tys, ty)?;
            match node {
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Str
                | PNode::Never
                | PNode::Foreign(_) => {}
                PNode::Param(i) => {
                    let flag = ctx.to_param_use(layout_flag, concrete_flag);
                    self.mark_param(i, flag)?;
                }
                PNode::ConstParam(i) => {
                    if ctx != WalkCtx::Phantom {
                        self.mark_param(i, ParamUse::CONST_PARAM)?;
                    }
                }
                PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
                    let next_ctx = match ctx {
                        WalkCtx::Phantom => WalkCtx::Phantom,
                        WalkCtx::Concrete => WalkCtx::Concrete,
                        WalkCtx::BehindThinPtr | WalkCtx::ByValue => WalkCtx::BehindThinPtr,
                    };
                    let inner_node = self.body.ty(&self.tys, inner)?;
                    if let PNode::Slice(elem) = inner_node {
                        self.stack.push((elem, next_ctx));
                    } else {
                        self.stack.push((inner, next_ctx));
                    }
                }
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                    let items = self.body.ty_list(&self.tys, l)?;
                    for child in items.iter() {
                        self.stack.push((child, ctx));
                    }
                }
                PNode::Array(elem, len) => {
                    self.stack.push((elem, ctx));
                    let len_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                    self.stack.push((len, len_ctx));
                }
                PNode::Slice(elem) => self.stack.push((elem, ctx)),
                PNode::Pat(base, ranges) => {
                    self.stack.push((base, ctx));
                    let items = self.body.ty_list(&self.tys, ranges)?;
                    for r in items.iter() {
                        self.stack.push((r, WalkCtx::Concrete));
                    }
                }
                PNode::ConstValue { ty: cty, .. } => {
                    let c_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                    self.stack.push((cty, c_ctx));
                }
                PNode::FnPtr(l, header) => {
                    let items = self.body.ty_list(&self.tys, l)?;
                    let next_ctx = if ctx == WalkCtx::Phantom {
                        WalkCtx::Phantom
                    } else if header.abi != Abi::Rust || header.c_variadic {
                        WalkCtx::Concrete
                    } else {
                        WalkCtx::ByValue
                    };
                    for child in items.iter() {
                        self.stack.push((child, next_ctx));
                    }
                }
                PNode::Adt(k, l) => {
                    let args = self.body.ty_list(&self.tys, l)?;
                    if ctx == WalkCtx::Phantom || self.facts.phantom_data() == Some(k) {
                        for arg in args.iter() {
                            self.stack.push((arg, WalkCtx::Phantom));
                        }
                    } else if self.facts.non_null() == Some(k) {
                        let next_ctx =
                            if ctx == WalkCtx::Concrete { WalkCtx::Concrete } else { WalkCtx::BehindThinPtr };
                        for arg in args.iter() {
                            self.stack.push((arg, next_ctx));
                        }
                    } else if let Some((param_uses, _has_drop)) = self.facts.adt_summary(k) {
                        if param_uses.len() == args.len() {
                            for (i, arg) in args.iter().enumerate() {
                                self.stack.push((arg, combine_adt_ctx(ctx, param_uses[i])));
                            }
                        } else {
                            for arg in args.iter() {
                                self.stack.push((arg, WalkCtx::Concrete));
                            }
                        }
                    } else {
                        for arg in args.iter() {
                            self.stack.push((arg, WalkCtx::Concrete));
                        }
                    }
                }
                PNode::FnDef(k, l) => {
                    if ctx == WalkCtx::Phantom {
                        let items = self.body.ty_list(&self.tys, l)?;
                        for child in items.iter() {
                            self.stack.push((child, WalkCtx::Phantom));
                        }
                    } else {
                        self.record_edge(k, l, false)?;
                    }
                }
                PNode::Alias { args: l, .. } | PNode::Opaque(_, l) | PNode::Dynamic(l) | PNode::ExTrait(_, l) => {
                    let items = self.body.ty_list(&self.tys, l)?;
                    for child in items.iter() {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                    // Also tag trait-dispatch specifically on params under aliases/dyn.
                    let _ = concrete_flag;
                }
                PNode::Closure { args: l, .. } | PNode::Coroutine { args: l, .. } => {
                    let items = self.body.ty_list(&self.tys, l)?;
                    for child in items.iter() {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                }
                PNode::ExProj { args, term, .. } => {
                    let items = self.body.ty_list(&self.tys, args)?;
                    for child in items.iter() {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                    self.stack.push((term, WalkCtx::Concrete));
                }
                PNode::Future(_, a, b) => {
                    self.stack.push((a, WalkCtx::Concrete));
                    self.stack.push((b, WalkCtx::Concrete));
                }
                PNode::ExAuto(_) | PNode::Infer | PNode::Error => {}
            }
        }
        Ok(())
    }

    /// Walks a type that is being dropped (`StmtKind::Drop { place }`), accurately tracking:
    /// - references/raw pointers/fn pointers/primitives: no drop glue;
    /// - `PhantomData` / `ManuallyDrop`: no drop glue;
    /// - `Param(i)`: sets `ParamUse::DROP_GLUE` (canonicalizable iff `!arg.needs_drop`);
    /// - ADTs with `has_drop_impl`: records a drop call edge to `<Adt as Drop>::drop` (or forces
    ///   `Concrete` if unknown);
    /// - ADTs without `has_drop_impl`: drops only `ByValue` params (e.g. `Option<T>` drops `T`,
    ///   `NonNull<T>` does not).
    fn walk_drop_ty(&mut self, root: PTy) -> Result<(), PolyError> {
        self.stack.clear();
        self.stack.push((root, WalkCtx::ByValue));
        let mut visited: usize = 0;
        while let Some((ty, ctx)) = self.stack.pop() {
            visited += 1;
            if visited > MAX_TYPE_WALK_NODES {
                return Err(PolyError::TypeWalkCap { cap: MAX_TYPE_WALK_NODES });
            }
            let node = self.body.ty(&self.tys, ty)?;
            match node {
                // Dropping a primitive, reference, raw pointer, fn item, fn ptr, or extern type is a
                // no-op and never drops any pointee.
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Str
                | PNode::Never
                | PNode::Foreign(_)
                | PNode::Ref(..)
                | PNode::Ptr(..)
                | PNode::FnDef(..)
                | PNode::FnPtr(..)
                | PNode::Union(_)
                | PNode::ConstValue { .. }
                | PNode::ExAuto(_) => {}
                PNode::Param(i) => {
                    let flag = if ctx == WalkCtx::Concrete {
                        ParamUse::UNANALYZED_CALLEE | ParamUse::DROP_GLUE
                    } else {
                        ParamUse::DROP_GLUE
                    };
                    self.mark_param(i, flag)?;
                }
                PNode::ConstParam(i) => {
                    self.mark_param(i, ParamUse::CONST_PARAM)?;
                }
                PNode::Tuple(l) | PNode::Record(_, l) => {
                    let items = self.body.ty_list(&self.tys, l)?;
                    for child in items.iter() {
                        self.stack.push((child, ctx));
                    }
                }
                PNode::Array(elem, len) => {
                    // `[T; 0]` has no drop, but if `len` is a `ConstParam` it depends on `N`.
                    match self.body.ty(&self.tys, len)? {
                        PNode::ConstValue { bits: 0, .. } => {}
                        PNode::ConstParam(n) => {
                            self.mark_param(n, ParamUse::CONST_PARAM)?;
                            self.stack.push((elem, ctx));
                        }
                        _ => self.stack.push((elem, ctx)),
                    }
                }
                PNode::Slice(elem) | PNode::Pat(elem, _) => self.stack.push((elem, ctx)),
                PNode::Adt(k, l) => {
                    if self.facts.phantom_data() == Some(k)
                        || self.facts.manually_drop() == Some(k)
                        || self.facts.non_null() == Some(k)
                    {
                        continue;
                    }
                    let args = self.body.ty_list(&self.tys, l)?;
                    if let Some((param_uses, has_drop_impl)) = self.facts.adt_summary(k) {
                        if has_drop_impl {
                            if let Some(drop_fn) = self.facts.adt_drop_fn(k) {
                                // Copy args into a small stack/heap slice via edge_args directly.
                                let start = self.edge_args.len() as u32;
                                for arg in args.iter() {
                                    self.edge_args.push(arg);
                                }
                                let len = self.edge_args.len() as u32 - start;
                                self.call_edges.push(CallEdge {
                                    callee: drop_fn,
                                    args_start: start,
                                    args_len: len,
                                    is_drop_edge: true,
                                });
                            } else {
                                // Custom Drop impl with unknown body: conservatively concrete.
                                for arg in args.iter() {
                                    self.stack.push((arg, WalkCtx::Concrete));
                                }
                            }
                        }
                        // Structural field drops still execute after custom `Drop::drop`!
                        if param_uses.len() == args.len() {
                            for (i, arg) in args.iter().enumerate() {
                                match param_uses[i] {
                                    AdtParamUse::Unused | AdtParamUse::OpaquePtr => {}
                                    AdtParamUse::ByValue => self.stack.push((arg, ctx)),
                                    AdtParamUse::Concrete => self.stack.push((arg, WalkCtx::Concrete)),
                                }
                            }
                        } else {
                            for arg in args.iter() {
                                self.stack.push((arg, WalkCtx::Concrete));
                            }
                        }
                    } else {
                        // Unknown ADT: might have custom Drop impl or structural drop.
                        for arg in args.iter() {
                            self.stack.push((arg, WalkCtx::Concrete));
                        }
                    }
                }
                PNode::Dynamic(l)
                | PNode::ExTrait(_, l)
                | PNode::Alias { args: l, .. }
                | PNode::Opaque(_, l)
                | PNode::Closure { args: l, .. }
                | PNode::Coroutine { args: l, .. } => {
                    let items = self.body.ty_list(&self.tys, l)?;
                    for child in items.iter() {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                }
                PNode::ExProj { args, term, .. } => {
                    let items = self.body.ty_list(&self.tys, args)?;
                    for child in items.iter() {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                    self.stack.push((term, WalkCtx::Concrete));
                }
                PNode::Future(_, a, b) => {
                    self.stack.push((a, WalkCtx::Concrete));
                    self.stack.push((b, WalkCtx::Concrete));
                }
                PNode::Infer | PNode::Error => {}
            }
        }
        Ok(())
    }

    /// Resolves `place`'s projection chain, records any layout/stride usage caused by projections
    /// (such as `Proj::Index`, `Proj::Field`, `Proj::Downcast`) and by-value access, and returns
    /// the resulting `PTy` of `place`.
    fn visit_place(&mut self, place: Place, mode: PlaceUse) -> Result<PTy, PolyError> {
        self.proj_buf.clear();
        let mut cur = place;
        let mut depth: u32 = 0;
        let base_local = loop {
            let row = self.body.place(cur)?;
            match (row.parent, row.proj) {
                (Some(parent), Some(proj)) => {
                    depth += 1;
                    if depth > MAX_PLACE_DEPTH {
                        return Err(PolyError::PlaceDepthCap { cap: MAX_PLACE_DEPTH });
                    }
                    self.proj_buf.push(proj);
                    cur = parent;
                }
                _ => break row.local,
            }
        };

        let mut ty = self.body.local(base_local)?.ty;
        // Walk projections from outermost (base local) to innermost.
        let proj_count = self.proj_buf.len();
        for idx in (0..proj_count).rev() {
            let proj = self.proj_buf[idx];
            match proj {
                Proj::Field { ty: field_ty, .. } => {
                    // Computing field offset depends on the enclosing aggregate's layout.
                    self.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    ty = field_ty;
                }
                Proj::Downcast(_) => {
                    // Downcasting an enum depends on the enum's layout & niche.
                    self.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                }
                Proj::Deref => {
                    ty = match self.body.ty(&self.tys, ty)? {
                        PNode::Ref(_, inner) | PNode::Ptr(_, inner) => inner,
                        _ => {
                            self.walk_ty(ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                            ty
                        }
                    };
                }
                Proj::DerefBox => {
                    ty = match self.body.ty(&self.tys, ty)? {
                        PNode::Adt(_, args) => {
                            let list = self.body.ty_list(&self.tys, args)?;
                            if let Some(inner) = list.get(0) {
                                inner
                            } else {
                                ty
                            }
                        }
                        PNode::Ref(_, inner) | PNode::Ptr(_, inner) => inner,
                        _ => {
                            self.walk_ty(ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                            ty
                        }
                    };
                }
                Proj::Index(_) | Proj::ConstIndex { .. } | Proj::Subslice { .. } => {
                    // Indexing/subslicing multiplies by `size_of::<elem>()`.
                    let elem = match self.body.ty(&self.tys, ty)? {
                        PNode::Slice(e) | PNode::Array(e, _) => e,
                        _ => {
                            self.walk_ty(ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                            ty
                        }
                    };
                    self.walk_ty(elem, WalkCtx::ByValue, ParamUse::SIZE_ALIGN, ParamUse::UNANALYZED_CALLEE)?;
                    if !matches!(proj, Proj::Subslice { .. }) {
                        ty = elem;
                    }
                }
            }
        }

        if mode == PlaceUse::ByValue {
            self.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
        }
        Ok(ty)
    }

    fn visit_operand(&mut self, op: Operand) -> Result<PTy, PolyError> {
        match op {
            Operand::Copy(p) | Operand::Move(p) => self.visit_place(p, PlaceUse::ByValue),
            Operand::Const(c) => Ok(self.body.konst(c)?.ty),
        }
    }

    fn visit_value(&mut self, value: Value) -> Result<(), PolyError> {
        match value {
            Value::Use(op) => {
                let _ = self.visit_operand(op)?;
            }
            Value::Borrow { place, .. } | Value::RawAddr { place, .. } => {
                // Reborrowing `*p` produces a pointer without loading the pointee by value.
                let pty = self.visit_place(place, PlaceUse::ProjOnly)?;
                self.walk_ty(pty, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::UNANALYZED_CALLEE)?;
            }
            Value::Binary { op, lhs, rhs } | Value::CheckedBinary { op, lhs, rhs } => {
                let lhs_ty = self.visit_operand(lhs)?;
                let _ = self.visit_operand(rhs)?;
                match op {
                    BinOp::Offset => {
                        // Pointer offset adds `delta * size_of::<pointee>()`.
                        if let PNode::Ptr(_, pointee) | PNode::Ref(_, pointee) = self.body.ty(&self.tys, lhs_ty)? {
                            self.walk_ty(pointee, WalkCtx::ByValue, ParamUse::SIZE_ALIGN, ParamUse::UNANALYZED_CALLEE)?;
                        } else {
                            self.walk_ty(lhs_ty, WalkCtx::Concrete, ParamUse::SIZE_ALIGN, ParamUse::VALUE_OP)?;
                        }
                    }
                    // Pointer equality/ordering on thin pointers compares addresses only.
                    BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        match self.body.ty(&self.tys, lhs_ty)? {
                            PNode::Ptr(_, pointee) | PNode::Ref(_, pointee) => {
                                self.walk_ty(pointee, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::VALUE_OP)?;
                            }
                            _ => {
                                self.walk_ty(lhs_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                            }
                        }
                    }
                    BinOp::Add
                    | BinOp::Sub
                    | BinOp::Mul
                    | BinOp::Div
                    | BinOp::Rem
                    | BinOp::BitXor
                    | BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::Shl
                    | BinOp::Shr
                    | BinOp::Cmp => {
                        self.walk_ty(lhs_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                    }
                }
            }
            Value::Unary { op, operand } => {
                let op_ty = self.visit_operand(operand)?;
                match op {
                    UnOp::PtrMetadata => {
                        self.walk_ty(op_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                    }
                    UnOp::Not | UnOp::Neg => {
                        self.walk_ty(op_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                    }
                }
            }
            Value::Cast { kind, operand, ty: target_ty } => {
                let src_ty = self.visit_operand(operand)?;
                match kind {
                    CastKind::PtrToPtr
                    | CastKind::MutToConstPointer
                    | CastKind::ExposeAddr
                    | CastKind::FromExposedAddr
                    | CastKind::FnPtrToPtr
                    | CastKind::UnsafeFnPointer
                    | CastKind::ArrayToPointer => {
                        self.walk_ty(src_ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                        self.walk_ty(target_ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    CastKind::Unsize => {
                        // Unsizing `&T -> &dyn Tr` builds a vtable for concrete `T`;
                        // `&[T; N] -> &[T]` embeds `N`.
                        self.walk_ty(src_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                        self.walk_ty(target_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                    }
                    CastKind::ReifyFnPointer => {
                        // Reifying a `FnDef` exports its concrete address; record as callee edge
                        // (or concrete if not a direct FnDef).
                        if let PNode::FnDef(k, args) = self.body.ty(&self.tys, src_ty)? {
                            self.record_edge(k, args, false)?;
                        } else {
                            self.walk_ty(src_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                        }
                        self.walk_ty(target_ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    CastKind::ClosureFnPointer => {
                        self.walk_ty(src_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                        self.walk_ty(target_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    CastKind::Transmute => {
                        self.walk_ty(src_ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                        self.walk_ty(target_ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    CastKind::IntToInt | CastKind::IntToFloat | CastKind::FloatToInt | CastKind::FloatToFloat => {
                        self.walk_ty(src_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                        self.walk_ty(target_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                    }
                }
            }
            Value::Aggregate { kind: _, ty, operands } => {
                self.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                for op_idx in operands.start..operands.end {
                    let op = self.body.operand(op_idx)?;
                    let _ = self.visit_operand(op)?;
                }
            }
            Value::Repeat { operand, count } => {
                let _ = self.visit_operand(operand)?;
                let cdecl = self.body.konst(count)?;
                if let ConstKind::Param(p) = cdecl.kind {
                    self.mark_param(p, ParamUse::CONST_PARAM)?;
                }
            }
            Value::Len(place) => {
                let pty = self.visit_place(place, PlaceUse::ProjOnly)?;
                match self.body.ty(&self.tys, pty)? {
                    PNode::Slice(elem) => {
                        // Reading the length word of `&[T]` never inspects `T`'s layout!
                        self.walk_ty(elem, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    PNode::Array(elem, len) => {
                        // Reading `[T; N].len()` reads `N` (const param -> concrete) but not `T`'s layout.
                        self.walk_ty(elem, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::UNANALYZED_CALLEE)?;
                        self.walk_ty(len, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::CONST_PARAM)?;
                    }
                    _ => {
                        self.walk_ty(pty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                }
            }
            Value::Discriminant(place) => {
                let pty = self.visit_place(place, PlaceUse::ProjOnly)?;
                self.walk_ty(pty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
            }
            Value::SizeOf(ty) | Value::AlignOf(ty) => {
                self.walk_ty(ty, WalkCtx::ByValue, ParamUse::SIZE_ALIGN, ParamUse::UNANALYZED_CALLEE)?;
            }
            Value::ThreadLocalAddr(_) => {}
        }
        Ok(())
    }

    fn visit_callee(&mut self, callee_id: blaze_body::CalleeId) -> Result<(), PolyError> {
        match self.body.callee(callee_id)? {
            Callee::FnItem { key, args } => {
                self.record_edge(key, args, false)?;
            }
            Callee::Method { key: _, args } => {
                // Trait method dispatch: impl selection depends on concrete Self and trait args!
                self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
            }
            Callee::Value(op) => {
                let fn_ty = self.visit_operand(op)?;
                // Indirect call through fn pointer or closure: callee body is opaque here.
                self.walk_ty(fn_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
            }
            Callee::Intrinsic { key, args } => match self.facts.intrinsic_which(key) {
                Some(which) => match IntrinsicClass::of(which) {
                    IntrinsicClass::NoTypeDependence => {
                        self.walk_list(args, WalkCtx::Phantom, ParamUse::PHANTOM, ParamUse::VALUE_OP)?;
                    }
                    IntrinsicClass::OpaquePtr => {
                        self.walk_list(args, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    IntrinsicClass::LayoutOnly => {
                        let layout_flag = match which {
                            Which::NeedsDrop => ParamUse::DROP_GLUE,
                            Which::SizeOf | Which::AlignOf | Which::SizeOfVal | Which::AlignOfVal => {
                                ParamUse::SIZE_ALIGN
                            }
                            _ => ParamUse::SIZE_ALIGN | ParamUse::BY_VALUE,
                        };
                        self.walk_list(args, WalkCtx::ByValue, layout_flag, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    IntrinsicClass::Reflection => {
                        self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::REFLECTION)?;
                    }
                    IntrinsicClass::ValueOp => {
                        self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                    }
                    IntrinsicClass::UnsizedMeta => {
                        self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                    }
                },
                None => {
                    // Unknown intrinsic: conservatively concrete.
                    self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                }
            },
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Transitive Call-Graph Fixed-Point Propagation
// ---------------------------------------------------------------------------

/// Flat CSR result of [`propagate_call_graph`]: final transitive [`ParamUse`] per function.
pub struct PropagatedUsage<'a> {
    /// Sorted by `ItemKey`: `(item, start, len)`.
    index: AVec<'a, (ItemKey, u32, u32)>,
    /// Per-input-function slice range `(start, len)` in `usage`, matching the `analyses` slice order.
    by_order: AVec<'a, (u32, u32)>,
    /// Flat array of converged [`ParamUse`] values.
    usage: AVec<'a, ParamUse>,
    /// Iterations taken to reach fixpoint.
    pub iterations: u32,
}

impl<'a> PropagatedUsage<'a> {
    /// Final [`ParamUse`] slice for function `fn_idx` (same index as `analyses[fn_idx]`).
    #[inline]
    pub fn get_index(&self, fn_idx: usize) -> &[ParamUse] {
        let (start, len) = self.by_order[fn_idx];
        &self.usage[start as usize..(start + len) as usize]
    }

    /// Final [`ParamUse`] slice for function `item` in `O(log N)`.
    #[inline]
    pub fn get(&self, item: ItemKey) -> Option<&[ParamUse]> {
        let pos = self.index.binary_search_by_key(&item, |row| row.0).ok()?;
        let (_, start, len) = self.index[pos];
        Some(&self.usage[start as usize..(start + len) as usize])
    }
}

/// Runs monotone fixed-point propagation of [`ParamUse`] across a call graph of analyzed
/// functions.
///
/// Soundness invariant: canonicalizing `F<A>` and `F<B>` to `F<Canon>` is ONLY sound if every
/// callee `G` called by `F` also canonicalizes its corresponding generic arguments compatibly.
/// - If `G` is not present in `analyses` (external / unanalyzed callee), every parameter appearing
///   in `G`'s generic arguments is marked [`ParamUse::UNANALYZED_CALLEE`] (`Concrete`).
/// - If `G` uses its parameter `j` at [`CanonLevel::LayoutOnly`] and `F` passes an ADT `S<T>`
///   where `S` has a custom `Drop` impl (`has_drop_impl == true`), `G<S<T>>` will not canonicalize
///   `S<T>` (since `needs_drop(S<T>) == true`), so `T` in `F` is promoted to `Concrete`!
/// - Handles mutual recursion (`A -> B -> A`) cleanly and terminates within [`MAX_FIXED_POINT_ITERS`].
pub fn propagate_call_graph<'a, 't>(
    analyses: &[FnAnalysis<'_>],
    tys_of: &dyn Fn(usize) -> PTys<'t>,
    facts: &dyn AnalysisFacts,
    alloc: Alloc<'a>,
) -> Result<PropagatedUsage<'a>, PolyError> {
    let mut total_params: usize = 0;
    for a in analyses {
        total_params += a.local_use.len();
    }
    let mut usage: AVec<'a, ParamUse> = Vec::with_capacity_in(total_params, alloc);
    let mut by_order: AVec<'a, (u32, u32)> = Vec::with_capacity_in(analyses.len(), alloc);
    let mut index: AVec<'a, (ItemKey, u32, u32)> = Vec::with_capacity_in(analyses.len(), alloc);

    for a in analyses {
        let start = usage.len() as u32;
        let len = a.local_use.len() as u32;
        usage.extend_from_slice(a.local_use.as_slice());
        by_order.push((start, len));
        index.push((a.item, start, len));
    }
    index.sort_unstable_by_key(|row| row.0);

    let mut stack: AVec<'_, (PTy, WalkCtx, bool)> = Vec::new_in(alloc);
    let mut next_caller: AVec<'_, ParamUse> = Vec::new_in(alloc);

    for iter in 0..MAX_FIXED_POINT_ITERS {
        let mut changed = false;
        for (caller_idx, caller) in analyses.iter().enumerate() {
            if caller.call_edges.is_empty() {
                continue;
            }
            let (c_start, c_len) = by_order[caller_idx];
            let c_start = c_start as usize;
            let c_len = c_len as usize;
            next_caller.clear();
            next_caller.extend_from_slice(&usage[c_start..c_start + c_len]);
            let caller_tys = tys_of(caller_idx);

            for edge in caller.call_edges.iter() {
                let edge_args = caller.edge_arg_slice(edge);
                let callee_slice = index
                    .binary_search_by_key(&edge.callee, |row| row.0)
                    .ok()
                    .map(|pos| {
                        let (_, s, l) = index[pos];
                        &usage[s as usize..(s + l) as usize]
                    });

                match callee_slice {
                    None => {
                        // External / unanalyzed callee: every param in its args must stay Concrete.
                        for &arg_ty in edge_args {
                            propagate_arg_ty(
                                arg_ty,
                                WalkCtx::Concrete,
                                false,
                                ParamUse::BY_VALUE,
                                ParamUse::UNANALYZED_CALLEE,
                                caller_tys,
                                facts,
                                &mut next_caller,
                                &mut stack,
                            )?;
                        }
                    }
                    Some(callee_uses) => {
                        if callee_uses.len() != edge_args.len() {
                            for &arg_ty in edge_args {
                                propagate_arg_ty(
                                    arg_ty,
                                    WalkCtx::Concrete,
                                    false,
                                    ParamUse::BY_VALUE,
                                    ParamUse::UNANALYZED_CALLEE,
                                    caller_tys,
                                    facts,
                                    &mut next_caller,
                                    &mut stack,
                                )?;
                            }
                            continue;
                        }
                        for (&arg_ty, &callee_u) in edge_args.iter().zip(callee_uses.iter()) {
                            match callee_u.level() {
                                CanonLevel::Unused => {
                                    // Callee ignores this arg (e.g. PhantomData): does not escalate caller.
                                }
                                CanonLevel::OpaquePtr => {
                                    propagate_arg_ty(
                                        arg_ty,
                                        WalkCtx::BehindThinPtr,
                                        false,
                                        ParamUse::OPAQUE_PTR,
                                        ParamUse::UNANALYZED_CALLEE,
                                        caller_tys,
                                        facts,
                                        &mut next_caller,
                                        &mut stack,
                                    )?;
                                }
                                CanonLevel::LayoutOnly => {
                                    // Callee requires `!needs_drop(arg_ty)` to canonicalize.
                                    // Therefore any ADT with `has_drop_impl == true` inside `arg_ty`
                                    // (outside pointers) will keep the callee concrete -> promote
                                    // its params to Concrete!
                                    let layout_bits =
                                        ParamUse::from_bits(callee_u.bits() & ParamUse::LAYOUT_MASK.bits());
                                    propagate_arg_ty(
                                        arg_ty,
                                        WalkCtx::ByValue,
                                        true,
                                        if layout_bits.is_empty() { ParamUse::BY_VALUE } else { layout_bits },
                                        ParamUse::UNANALYZED_CALLEE,
                                        caller_tys,
                                        facts,
                                        &mut next_caller,
                                        &mut stack,
                                    )?;
                                }
                                CanonLevel::Concrete => {
                                    let concrete_bits =
                                        ParamUse::from_bits(callee_u.bits() & ParamUse::CONCRETE_MASK.bits());
                                    let flag = if concrete_bits.is_empty() {
                                        ParamUse::UNANALYZED_CALLEE
                                    } else {
                                        concrete_bits
                                    };
                                    propagate_arg_ty(
                                        arg_ty,
                                        WalkCtx::Concrete,
                                        false,
                                        ParamUse::BY_VALUE,
                                        flag,
                                        caller_tys,
                                        facts,
                                        &mut next_caller,
                                        &mut stack,
                                    )?;
                                }
                            }
                        }
                    }
                }
            }

            for i in 0..c_len {
                if usage[c_start + i].join_assign(next_caller[i]) {
                    changed = true;
                }
            }
        }
        if !changed {
            return Ok(PropagatedUsage { index, by_order, usage, iterations: iter + 1 });
        }
    }
    Err(PolyError::IterationCap { cap: MAX_FIXED_POINT_ITERS })
}

#[allow(clippy::too_many_arguments)]
fn propagate_arg_ty(
    root: PTy,
    root_ctx: WalkCtx,
    check_drop_adts: bool,
    layout_flag: ParamUse,
    concrete_flag: ParamUse,
    tys: PTys<'_>,
    facts: &dyn AnalysisFacts,
    out: &mut [ParamUse],
    stack: &mut AVec<'_, (PTy, WalkCtx, bool)>,
) -> Result<(), PolyError> {
    stack.clear();
    stack.push((root, root_ctx, check_drop_adts));
    let mut visited: usize = 0;
    while let Some((ty, ctx, check_drop)) = stack.pop() {
        visited += 1;
        if visited > MAX_TYPE_WALK_NODES {
            return Err(PolyError::TypeWalkCap { cap: MAX_TYPE_WALK_NODES });
        }
        let node = tys.node(ty).map_err(|_| PolyError::Ty(TyError::NotInRecord(ty)))?;
        match node {
            PNode::Bool
            | PNode::Char
            | PNode::Int(_)
            | PNode::Uint(_)
            | PNode::Float(_)
            | PNode::Str
            | PNode::Never
            | PNode::Foreign(_) => {}
            PNode::Param(i) => {
                let Some(slot) = out.get_mut(i as usize) else {
                    return Err(PolyError::ParamOutOfRange { index: i, param_count: out.len() as u32 });
                };
                slot.join_assign(ctx.to_param_use(layout_flag, concrete_flag));
            }
            PNode::ConstParam(i) => {
                if ctx != WalkCtx::Phantom {
                    let Some(slot) = out.get_mut(i as usize) else {
                        return Err(PolyError::ParamOutOfRange { index: i, param_count: out.len() as u32 });
                    };
                    slot.join_assign(ParamUse::CONST_PARAM);
                }
            }
            PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
                // Behind a reference/pointer, `needs_drop(&T)` is always `false`.
                let next_ctx = match ctx {
                    WalkCtx::Phantom => WalkCtx::Phantom,
                    WalkCtx::Concrete => WalkCtx::Concrete,
                    WalkCtx::BehindThinPtr | WalkCtx::ByValue => WalkCtx::BehindThinPtr,
                };
                let inner_node = tys.node(inner).map_err(|_| PolyError::Ty(TyError::NotInRecord(inner)))?;
                if let PNode::Slice(elem) = inner_node {
                    stack.push((elem, next_ctx, false));
                } else {
                    stack.push((inner, next_ctx, false));
                }
            }
            PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                let items = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                let child_drop = check_drop && !matches!(node, PNode::Union(_));
                for &child in items {
                    stack.push((child, ctx, child_drop));
                }
            }
            PNode::Array(elem, len) => {
                stack.push((elem, ctx, check_drop));
                let len_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                stack.push((len, len_ctx, false));
            }
            PNode::Slice(elem) => stack.push((elem, ctx, check_drop)),
            PNode::Pat(base, ranges) => {
                stack.push((base, ctx, check_drop));
                let items = tys.list(ranges).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(ranges)))?;
                for &r in items {
                    stack.push((r, WalkCtx::Concrete, false));
                }
            }
            PNode::ConstValue { ty: cty, .. } => {
                let c_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                stack.push((cty, c_ctx, false));
            }
            PNode::FnPtr(l, header) => {
                let items = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                let next_ctx = if ctx == WalkCtx::Phantom {
                    WalkCtx::Phantom
                } else if header.abi != Abi::Rust || header.c_variadic {
                    WalkCtx::Concrete
                } else {
                    WalkCtx::ByValue
                };
                for &child in items {
                    stack.push((child, next_ctx, false));
                }
            }
            PNode::Adt(k, l) => {
                let args = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                if ctx == WalkCtx::Phantom || facts.phantom_data() == Some(k) {
                    for &arg in args {
                        stack.push((arg, WalkCtx::Phantom, false));
                    }
                } else if facts.non_null() == Some(k) {
                    let next_ctx = if ctx == WalkCtx::Concrete { WalkCtx::Concrete } else { WalkCtx::BehindThinPtr };
                    for &arg in args {
                        stack.push((arg, next_ctx, false));
                    }
                } else if facts.manually_drop() == Some(k) {
                    for &arg in args {
                        stack.push((arg, ctx, false));
                    }
                } else if let Some((param_uses, has_drop_impl)) = facts.adt_summary(k) {
                    if check_drop && has_drop_impl {
                        // Passing a Drop-implementing ADT (e.g. `Vec<T>`, `Box<T>`) by value to a
                        // `LayoutOnly` callee keeps the callee concrete (`needs_drop == true`).
                        for &arg in args {
                            stack.push((arg, WalkCtx::Concrete, false));
                        }
                    } else if param_uses.len() == args.len() {
                        for (&arg, &pu) in args.iter().zip(param_uses.iter()) {
                            let child_drop = check_drop && pu == AdtParamUse::ByValue;
                            stack.push((arg, combine_adt_ctx(ctx, pu), child_drop));
                        }
                    } else {
                        for &arg in args {
                            stack.push((arg, WalkCtx::Concrete, false));
                        }
                    }
                } else {
                    for &arg in args {
                        stack.push((arg, WalkCtx::Concrete, false));
                    }
                }
            }
            PNode::FnDef(_, l)
            | PNode::Alias { args: l, .. }
            | PNode::Opaque(_, l)
            | PNode::Closure { args: l, .. }
            | PNode::Coroutine { args: l, .. }
            | PNode::Dynamic(l)
            | PNode::ExTrait(_, l) => {
                let items = tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                for &child in items {
                    stack.push((child, WalkCtx::Concrete, false));
                }
            }
            PNode::ExProj { args, term, .. } => {
                let items = tys.list(args).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(args)))?;
                for &child in items {
                    stack.push((child, WalkCtx::Concrete, false));
                }
                stack.push((term, WalkCtx::Concrete, false));
            }
            PNode::Future(_, a, b) => {
                stack.push((a, WalkCtx::Concrete, false));
                stack.push((b, WalkCtx::Concrete, false));
            }
            PNode::ExAuto(_) | PNode::Infer | PNode::Error => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deterministic Canonical Encoding, Symbol Derivation & Ground Representatives
// ---------------------------------------------------------------------------

/// Appends the canonical, machine-independent, table-index-independent byte representation of
/// `(item, canon)` to `out` (Rule 21, Rule 25).
///
/// Any [`CanonTy::Concrete`]`(pty)` arguments are encoded in a single shared post-order forest
/// over `ground` (matching `symbols.rs` form 2: linear in distinct type nodes, zero dependence on
/// `PTy(u32)` table indices).
pub fn encode_canonical_instance(
    item: ItemKey,
    canon: &[CanonTy],
    ground: PTys<'_>,
    scratch: Alloc<'_>,
    out: &mut AVec<'_, u8>,
) -> Result<(), PolyError> {
    out.extend_from_slice(&item.0.to_le_bytes());
    out.extend_from_slice(&(canon.len() as u32).to_le_bytes());

    // Collect concrete PTys in argument order to share one forest across all concrete args.
    let mut concrete_tys: AVec<'_, PTy> = Vec::new_in(scratch);
    for &ct in canon {
        match ct {
            CanonTy::Unused => out.push(0),
            CanonTy::OpaqueThin => out.push(1),
            CanonTy::Layout(shape) => {
                out.push(2);
                out.extend_from_slice(&shape.size.to_le_bytes());
                out.extend_from_slice(&shape.align.to_le_bytes());
                out.push(shape.abi as u8);
                out.push(u8::from(shape.uninhabited));
                match shape.niche {
                    None => out.push(0),
                    Some(n) => {
                        out.push(1);
                        out.extend_from_slice(&n.offset.to_le_bytes());
                        out.push(n.scalar_bytes);
                        out.extend_from_slice(&n.valid_start.to_le_bytes());
                        out.extend_from_slice(&n.valid_end.to_le_bytes());
                    }
                }
            }
            CanonTy::Concrete(pty) => {
                out.push(3);
                let idx = concrete_tys.len() as u32;
                out.extend_from_slice(&idx.to_le_bytes());
                concrete_tys.push(pty);
            }
        }
    }

    if !concrete_tys.is_empty() {
        encode_ground_forest(concrete_tys.as_slice(), ground, scratch, out)?;
    }
    Ok(())
}

/// Computes the deterministic canonical [`Symbol`] for `(item, canon)` (Rule 21, Rule 25).
pub fn canonical_fn_symbol(
    item: ItemKey,
    canon: &[CanonTy],
    ground: PTys<'_>,
    scratch: Alloc<'_>,
) -> Result<Symbol, PolyError> {
    let mut bytes: AVec<'_, u8> = Vec::new_in(scratch);
    encode_canonical_instance(item, canon, ground, scratch, &mut bytes)?;
    Ok(canonical_symbol_of_bytes(bytes.as_slice()))
}

/// Computes the [`Symbol`] directly from [`encode_canonical_instance`]'s bytes.
#[inline]
pub fn canonical_symbol_of_bytes(bytes: &[u8]) -> Symbol {
    Symbol(ContentId::of(CANON_FN_DOMAIN, bytes).0)
}

/// Computes a deterministic [`ContentId`] digest of a function's [`ParamUse`] summary.
pub fn usage_digest(item: ItemKey, usage: &[ParamUse]) -> ContentId {
    let mut h = Hasher::new("blaze-ladder poly-canon param-use 1");
    h.update(&item.0.to_le_bytes());
    h.update(&(usage.len() as u32).to_le_bytes());
    for &u in usage {
        h.update(&u.bits().to_le_bytes());
    }
    h.finish()
}

/// Interns a concrete representative ground [`PTy`] for a [`CanonTy`] in `ground`, when one
/// exists purely from built-in `PNode`s (requiring no user ADTs):
///
/// - `Unused` -> `()`
/// - `OpaqueThin` -> `PNode::Foreign(OPAQUE_THIN_FOREIGN_KEY)` (thin pointer, no size, no drop)
/// - `Layout(shape)`:
///   - `UNIT` -> `()`, `NEVER` -> `!`
///   - plain scalars -> `u8`/`i8`/`u16`/`i16`/`u32`/`u64`/`u128`/`f16`/`f32`/`f64`/`f128`
///   - `bool`, `char`, non-null thin pointer -> `bool`, `char`, `&()`
///   - wide slice pointers -> `*const [u8]` (no niche) or `&[u8]` (non-null niche)
///   - plain `Indirect` byte-bags with align in `{1, 2, 4, 8, 16}` -> `[u8/u16/u32/u64/u128; N]`
///   - exotic shapes (align > 16, interior niches at offset > 0, `WideDyn`) -> `Ok(None)`
/// - `Concrete(ty)` -> `Ok(Some(ty))`
pub fn canonical_representative_pty(
    ct: CanonTy,
    ground: &mut PortableTys<'_>,
) -> Result<Option<PTy>, PolyError> {
    let full = |_| PolyError::TableFull;
    match ct {
        CanonTy::Concrete(ty) => Ok(Some(ty)),
        CanonTy::Unused => Ok(Some(ground.intern(PNode::Tuple(List::EMPTY)).map_err(full)?)),
        CanonTy::OpaqueThin => Ok(Some(ground.intern(PNode::Foreign(OPAQUE_THIN_FOREIGN_KEY)).map_err(full)?)),
        CanonTy::Layout(shape) => {
            if shape.uninhabited {
                return if shape.size == 0 && shape.align == 1 && shape.abi == AbiClass::Zst && shape.niche.is_none() {
                    Ok(Some(ground.intern(PNode::Never).map_err(full)?))
                } else {
                    Ok(None)
                };
            }
            match (shape.abi, shape.size, shape.align, shape.niche) {
                (AbiClass::Zst, 0, 1, None) => Ok(Some(ground.intern(PNode::Tuple(List::EMPTY)).map_err(full)?)),
                (AbiClass::Int8Zext, 1, 1, None) => Ok(Some(ground.intern(PNode::Uint(UintTy::U8)).map_err(full)?)),
                (AbiClass::Int8Zext, 1, 1, Some(n)) if n == NicheShape::BOOL => {
                    Ok(Some(ground.intern(PNode::Bool).map_err(full)?))
                }
                (AbiClass::Int8Sext, 1, 1, None) => Ok(Some(ground.intern(PNode::Int(IntTy::I8)).map_err(full)?)),
                (AbiClass::Int16Zext, 2, 2, None) => Ok(Some(ground.intern(PNode::Uint(UintTy::U16)).map_err(full)?)),
                (AbiClass::Int16Sext, 2, 2, None) => Ok(Some(ground.intern(PNode::Int(IntTy::I16)).map_err(full)?)),
                (AbiClass::Int32, 4, 4, None) => Ok(Some(ground.intern(PNode::Uint(UintTy::U32)).map_err(full)?)),
                (AbiClass::Int32, 4, 4, Some(n)) if n == NicheShape::CHAR => {
                    Ok(Some(ground.intern(PNode::Char).map_err(full)?))
                }
                (AbiClass::Int64, 8, 8, None) => Ok(Some(ground.intern(PNode::Uint(UintTy::U64)).map_err(full)?)),
                (AbiClass::Int64, 8, 8, Some(n)) if n == NicheShape::NON_NULL_PTR => {
                    let unit = ground.intern(PNode::Tuple(List::EMPTY)).map_err(full)?;
                    Ok(Some(ground.intern(PNode::Ref(Mutbl::Not, unit)).map_err(full)?))
                }
                (AbiClass::Int128, 16, 16, None) => Ok(Some(ground.intern(PNode::Uint(UintTy::U128)).map_err(full)?)),
                (AbiClass::Float16, 2, 2, None) => Ok(Some(ground.intern(PNode::Float(FloatTy::F16)).map_err(full)?)),
                (AbiClass::Float32, 4, 4, None) => Ok(Some(ground.intern(PNode::Float(FloatTy::F32)).map_err(full)?)),
                (AbiClass::Float64, 8, 8, None) => Ok(Some(ground.intern(PNode::Float(FloatTy::F64)).map_err(full)?)),
                (AbiClass::Float128, 16, 16, None) => Ok(Some(ground.intern(PNode::Float(FloatTy::F128)).map_err(full)?)),
                (AbiClass::WideSlice, 16, 8, None) => {
                    let u8_ty = ground.intern(PNode::Uint(UintTy::U8)).map_err(full)?;
                    let slice = ground.intern(PNode::Slice(u8_ty)).map_err(full)?;
                    Ok(Some(ground.intern(PNode::Ptr(Mutbl::Not, slice)).map_err(full)?))
                }
                (AbiClass::WideSlice, 16, 8, Some(n)) if n == NicheShape::NON_NULL_PTR => {
                    let u8_ty = ground.intern(PNode::Uint(UintTy::U8)).map_err(full)?;
                    let slice = ground.intern(PNode::Slice(u8_ty)).map_err(full)?;
                    Ok(Some(ground.intern(PNode::Ref(Mutbl::Not, slice)).map_err(full)?))
                }
                (AbiClass::Indirect, size, align, None) if matches!(align, 1 | 2 | 4 | 8 | 16) && size % align == 0 => {
                    let elem_node = match align {
                        1 => PNode::Uint(UintTy::U8),
                        2 => PNode::Uint(UintTy::U16),
                        4 => PNode::Uint(UintTy::U32),
                        8 => PNode::Uint(UintTy::U64),
                        16 => PNode::Uint(UintTy::U128),
                        _ => unreachable!(),
                    };
                    let elem = ground.intern(elem_node).map_err(full)?;
                    let usize_ty = ground.intern(PNode::Uint(UintTy::Usize)).map_err(full)?;
                    let count = ground
                        .intern(PNode::ConstValue { ty: usize_ty, bits: u128::from(size / align) })
                        .map_err(full)?;
                    Ok(Some(ground.intern(PNode::Array(elem, count)).map_err(full)?))
                }
                _ => Ok(None),
            }
        }
    }
}

/// Canonicalizes a concrete ground instantiation `(item, args)` against `usage` and materializes
/// canonical ground representative [`PTy`]s in `ground` using [`canonical_representative_pty`].
///
/// Returns:
/// - `Ok(None)` if no argument was canonicalized (or every canonicalized argument had an exotic
///   [`LayoutShape`] with no built-in ground representative, or every representative already equals
///   the original `arg.ty`);
/// - `Ok(Some((canon_tys, rep_list)))` when at least one argument strictly changed to its
///   canonical ground representative, ready to be encoded as the canonical `Instance { item, args: rep_list }`.
///
/// Soundness: preserves every guard of [`canonicalize_args`] and [`canonical_representative_pty`]
/// (Rule 4 `superset-of-rust`, Rule 21 `keys-cover-every-input`, Rule 25 `deterministic-output`).
pub fn canonicalize_instance_to_ground<'a>(
    usage: &[ParamUse],
    args: &[ArgInfo],
    ground: &mut PortableTys<'_>,
    scratch: Alloc<'_>,
    alloc: Alloc<'a>,
) -> Result<Option<(AVec<'a, CanonTy>, List)>, PolyError> {
    let canon = canonicalize_args(usage, args, scratch)?;
    if !canon.is_changed() {
        return Ok(None);
    }
    let mut reps: AVec<'_, PTy> = Vec::with_capacity_in(args.len(), scratch);
    let mut out_canon: AVec<'a, CanonTy> = Vec::with_capacity_in(args.len(), alloc);
    let mut changed_any = false;
    for (&ct, &orig) in canon.as_slice().iter().zip(args.iter()) {
        if orig.kind == ParamKind::Const {
            // Never replace a const generic argument (`PNode::ConstValue`) with a type node.
            reps.push(orig.ty);
            out_canon.push(CanonTy::Concrete(orig.ty));
            continue;
        }
        match canonical_representative_pty(ct, ground)? {
            Some(rep) => {
                if rep != orig.ty {
                    changed_any = true;
                }
                reps.push(rep);
                out_canon.push(ct);
            }
            None => {
                // Exotic layout shape (e.g. align > 16 or interior niche): conservatively keep concrete.
                reps.push(orig.ty);
                out_canon.push(CanonTy::Concrete(orig.ty));
            }
        }
    }
    if !changed_any {
        return Ok(None);
    }
    let rep_list = ground.intern_list(reps.as_slice()).map_err(|_| PolyError::TableFull)?;
    Ok(Some((out_canon, rep_list)))
}

const POLY_TEXT_DOMAIN: &str = "blaze-ladder poly-canon body text sig 1";

/// Computes a deterministic, trivia-free, table-index-independent [`ContentId`] digest covering a
/// generic function's item text (without whitespace or comments), its resolved signature
/// (`FnSig`), and its declared [`Abi`].
///
/// Used by `ReadKind::PolyBodyText` (Rule 21 `keys-cover-every-input`) so a caller that emitted a
/// call to a canonicalized callee verifies in O(|text|) from the callee's head row alone (zero
/// `check_one` body checks on warm cache hits) and invalidates immediately if the callee's body or
/// signature changes.
pub fn poly_body_text_digest(
    text: &str,
    sig: &blaze_typeck::FnSig<'_>,
    abi: blaze_build::Abi,
    scratch: Alloc<'_>,
) -> ContentId {
    use parser::{Edition, LexedStr};

    let mut h = Hasher::new(POLY_TEXT_DOMAIN);
    let abi_tag: u8 = match abi {
        blaze_build::Abi::Rust => 0,
        blaze_build::Abi::C => 1,
        blaze_build::Abi::RustCall => 2,
    };
    h.update(&[abi_tag]);
    h.update(&sig.parent_count.to_le_bytes());
    h.update(&sig.own_count.to_le_bytes());
    h.update(&sig.declared_count.to_le_bytes());
    h.update(&[u8::from(sig.self_param), sig.header.bits(), u8::from(sig.c_variadic)]);
    h.update(&(sig.legacy_const_generics.len() as u32).to_le_bytes());
    for &pos in sig.legacy_const_generics {
        h.update(&pos.to_le_bytes());
    }
    // Digest signature input & output types structurally (table-index-independent, Rule 25).
    let mut sig_tys: AVec<'_, PTy> = Vec::with_capacity_in(sig.inputs.len() + 1, scratch);
    sig_tys.extend_from_slice(sig.inputs);
    sig_tys.push(sig.output);
    let mut sig_bytes: AVec<'_, u8> = Vec::new_in(scratch);
    encode_sig_types_structural(sig_tys.as_slice(), sig.types, scratch, &mut sig_bytes);
    h.part(sig_bytes.as_slice());

    let lexed = LexedStr::new(Edition::Edition2024, text);
    let mut tokens: u64 = 0;
    for i in 0..lexed.len() {
        if !lexed.kind(i).is_trivia() {
            h.part(lexed.text(i).as_bytes());
            tokens += 1;
        }
    }
    h.update(&tokens.to_le_bytes());
    h.finish()
}

/// Appends a deterministic, table-index-independent structural pre-order encoding of `roots` in
/// `tys` (including generic `Param`/`ConstParam`/`Alias`/`Opaque` nodes) to `out`.
fn encode_sig_types_structural(
    roots: &[PTy],
    tys: PTys<'_>,
    scratch: Alloc<'_>,
    out: &mut AVec<'_, u8>,
) {
    out.extend_from_slice(&(roots.len() as u32).to_le_bytes());
    let mut stack: AVec<'_, PTy> = Vec::with_capacity_in(roots.len(), scratch);
    for &r in roots.iter().rev() {
        stack.push(r);
    }
    let mut visited: usize = 0;
    while let Some(ty) = stack.pop() {
        visited += 1;
        if visited > MAX_TYPE_WALK_NODES {
            out.push(0xFF);
            return;
        }
        let Ok(node) = tys.node(ty) else {
            out.push(0xFE);
            continue;
        };
        match node {
            PNode::Bool => out.push(0),
            PNode::Char => out.push(1),
            PNode::Int(i) => out.extend_from_slice(&[2, i as u8]),
            PNode::Uint(u) => out.extend_from_slice(&[3, u as u8]),
            PNode::Float(x) => out.extend_from_slice(&[4, x as u8]),
            PNode::Str => out.push(5),
            PNode::Never => out.push(6),
            PNode::Tuple(l) => {
                out.push(7);
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::Ref(m, inner) => {
                out.extend_from_slice(&[8, m as u8]);
                stack.push(inner);
            }
            PNode::Ptr(m, inner) => {
                out.extend_from_slice(&[9, m as u8]);
                stack.push(inner);
            }
            PNode::Array(elem, len) => {
                out.push(10);
                stack.push(len);
                stack.push(elem);
            }
            PNode::Slice(elem) => {
                out.push(11);
                stack.push(elem);
            }
            PNode::Adt(k, l) => {
                out.push(12);
                out.extend_from_slice(&k.0.to_le_bytes());
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::FnDef(k, l) => {
                out.push(13);
                out.extend_from_slice(&k.0.to_le_bytes());
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::ConstValue { ty: cty, bits } => {
                out.push(14);
                out.extend_from_slice(&bits.to_le_bytes());
                stack.push(cty);
            }
            PNode::Pat(base, l) => {
                out.push(15);
                push_sig_list(l, tys, &mut stack, out);
                stack.push(base);
            }
            PNode::Dynamic(l) => {
                out.push(16);
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::ExTrait(k, l) => {
                out.push(17);
                out.extend_from_slice(&k.0.to_le_bytes());
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::ExProj { trait_, name, args, term } => {
                out.push(18);
                out.extend_from_slice(&trait_.0.to_le_bytes());
                out.extend_from_slice(&name.0.to_le_bytes());
                stack.push(term);
                push_sig_list(args, tys, &mut stack, out);
            }
            PNode::ExAuto(k) => {
                out.push(19);
                out.extend_from_slice(&k.0.to_le_bytes());
            }
            PNode::Closure { owner, ordinal, args } => {
                out.push(20);
                out.extend_from_slice(&owner.0.to_le_bytes());
                out.extend_from_slice(&ordinal.to_le_bytes());
                push_sig_list(args, tys, &mut stack, out);
            }
            PNode::FnPtr(l, header) => {
                out.extend_from_slice(&[21, header.bits()]);
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::Future(expr, a, b) => {
                out.push(22);
                out.extend_from_slice(&expr.0.to_le_bytes());
                stack.push(b);
                stack.push(a);
            }
            PNode::Coroutine { owner, ordinal, args } => {
                out.push(23);
                out.extend_from_slice(&owner.0.to_le_bytes());
                out.extend_from_slice(&ordinal.to_le_bytes());
                push_sig_list(args, tys, &mut stack, out);
            }
            PNode::Foreign(k) => {
                out.push(24);
                out.extend_from_slice(&k.0.to_le_bytes());
            }
            PNode::Record(s, l) => {
                out.push(25);
                out.extend_from_slice(&s.0.to_le_bytes());
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::Union(l) => {
                out.push(26);
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::Param(i) => {
                out.push(27);
                out.extend_from_slice(&i.to_le_bytes());
            }
            PNode::ConstParam(i) => {
                out.push(28);
                out.extend_from_slice(&i.to_le_bytes());
            }
            PNode::Alias { trait_, name, args } => {
                out.push(29);
                out.extend_from_slice(&trait_.0.to_le_bytes());
                out.extend_from_slice(&name.0.to_le_bytes());
                push_sig_list(args, tys, &mut stack, out);
            }
            PNode::Opaque(k, l) => {
                out.push(30);
                out.extend_from_slice(&k.0.to_le_bytes());
                push_sig_list(l, tys, &mut stack, out);
            }
            PNode::Infer => out.push(31),
            PNode::Error => out.push(32),
        }
    }
}

fn push_sig_list(l: List, tys: PTys<'_>, stack: &mut AVec<'_, PTy>, out: &mut AVec<'_, u8>) {
    match tys.list(l) {
        Ok(items) => {
            out.extend_from_slice(&(items.len() as u32).to_le_bytes());
            for &child in items.iter().rev() {
                stack.push(child);
            }
        }
        Err(_) => out.extend_from_slice(&u32::MAX.to_le_bytes()),
    }
}

// ---------------------------------------------------------------------------
// Per-Function Checked-HIR Body Analysis (`analyze_checked_body`)
// ---------------------------------------------------------------------------

/// Analyzes a type-checked HIR body (`blaze_typeck::BodyView` + `blaze_typeck::Checked` +
/// `blaze_typeck::FnSig`), the exact representation lowered by `blaze_build::build`, to compute
/// local [`ParamUse`] bitflags per generic parameter and collect direct generic call/drop edges
/// for [`propagate_call_graph`].
///
/// Iterative with zero recursion and explicit `Alloc` (Rules 11, 14, 18).
///
/// Every soundness invariant of [`analyze_body`] is preserved:
/// - Non-Rust ABI, C-variadic, legacy const generics, `inline for`, `async fn`, closures,
///   promoteds, inline `const { .. }` blocks, inline `asm!`, `format_args!`, default field values,
///   and `impl Trait` opaque definers conservatively mark all generic parameters concrete.
/// - `Unsize` coercions (`&T -> &dyn Tr`, `&[T; N] -> &[T]`), `ReifyFnPointer`,
///   `ClosureFnPointer`, overloaded `Deref`, trait method calls, associated type projections,
///   generic const items, arithmetic/bitwise value ops, and reflection intrinsics (`TypeId::of`,
///   `type_name`, `blaze_*`) force `CanonLevel::Concrete`.
/// - Every local, parameter, return type, and expression type is walked for implicit drop glue
///   (`ParamUse::DROP_GLUE`), so `T: Drop` stays concrete while `!needs_drop` types canonicalize
///   cleanly and `&T`/`*const T`/`&[T]` stay `OpaquePtr`.
pub fn analyze_checked_body<'a>(
    item: ItemKey,
    sig: &blaze_typeck::FnSig<'_>,
    abi: blaze_build::Abi,
    view: &blaze_typeck::BodyView<'_>,
    checked: &blaze_typeck::Checked<'_>,
    has_inline_for: bool,
    facts: &dyn AnalysisFacts,
    alloc: Alloc<'a>,
) -> Result<FnAnalysis<'a>, PolyError> {
    let param_count = sig.parent_count.saturating_add(sig.own_count);
    if param_count > MAX_GENERIC_PARAMS {
        return Err(PolyError::TooManyParams { count: param_count });
    }
    let mut local_use: AVec<'a, ParamUse> = Vec::with_capacity_in(param_count as usize, alloc);
    local_use.resize(param_count as usize, ParamUse::NONE);
    let mut call_edges: AVec<'a, CallEdge> = Vec::new_in(alloc);
    let mut edge_args: AVec<'a, PTy> = Vec::new_in(alloc);

    if param_count == 0 {
        return Ok(FnAnalysis { item, param_count, local_use, call_edges, edge_args });
    }

    // Fast conservative whole-body soundness guards.
    if abi != blaze_build::Abi::Rust || sig.c_variadic || !sig.legacy_const_generics.is_empty() {
        for slot in local_use.iter_mut() {
            *slot |= ParamUse::NON_RUST_ABI;
        }
        return Ok(FnAnalysis { item, param_count, local_use, call_edges, edge_args });
    }
    if has_inline_for {
        for slot in local_use.iter_mut() {
            *slot |= ParamUse::REFLECTION;
        }
        return Ok(FnAnalysis { item, param_count, local_use, call_edges, edge_args });
    }
    if view.coroutine.is_some()
        || !checked.closures.is_empty()
        || !checked.hidden.is_empty()
        || !checked.awaited.is_empty()
        || !checked.promoted.is_empty()
        || !checked.default_fields.is_empty()
    {
        for slot in local_use.iter_mut() {
            *slot |= ParamUse::UNANALYZED_CALLEE;
        }
        return Ok(FnAnalysis { item, param_count, local_use, call_edges, edge_args });
    }

    let mut stack: AVec<'_, (PTy, WalkCtx)> = Vec::new_in(alloc);
    let checked_tys = checked.types.view();

    // 1. Walk signature inputs & output (in `sig.types`) for layout and implicit drop glue.
    {
        let mut sig_cx = CheckedWalkCx {
            tys: sig.types,
            facts,
            local_use: local_use.as_mut_slice(),
            call_edges: &mut call_edges,
            edge_args: &mut edge_args,
            stack: &mut stack,
            record_edges: false,
        };
        for &inp in sig.inputs {
            sig_cx.walk_ty(inp, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
            sig_cx.walk_drop_ty(inp)?;
        }
        sig_cx.walk_ty(sig.output, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
        sig_cx.walk_drop_ty(sig.output)?;
    }

    let mut cx = CheckedWalkCx {
        tys: checked_tys,
        facts,
        local_use: local_use.as_mut_slice(),
        call_edges: &mut call_edges,
        edge_args: &mut edge_args,
        stack: &mut stack,
        record_edges: true,
    };

    // 2. Walk every local binding, pattern, and expression type (layout + implicit drop glue).
    for &ty in checked.local_ty.iter() {
        cx.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
        cx.walk_drop_ty(ty)?;
    }
    for &ty in checked.pat_ty.iter() {
        cx.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
        cx.walk_drop_ty(ty)?;
    }
    for &ty in checked.expr_ty.iter() {
        cx.walk_ty(ty, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
        cx.walk_drop_ty(ty)?;
    }

    // 3. Walk every HIR expression for value ops, casts, indexing, offset_of, const blocks, asm, etc.
    for (idx, &expr) in view.exprs.iter().enumerate() {
        let expr_ty = checked.expr_ty.get(idx).copied();
        match expr {
            blaze_typeck::Expr::ConstBlock { .. }
            | blaze_typeck::Expr::Asm { .. }
            | blaze_typeck::Expr::Closure { .. }
            | blaze_typeck::Expr::AsyncBlock { .. }
            | blaze_typeck::Expr::Await(_) => {
                cx.mark_all_params(ParamUse::UNANALYZED_CALLEE);
            }
            blaze_typeck::Expr::FormatArgs { .. } => {
                cx.mark_all_params(ParamUse::TRAIT_DISPATCH);
            }
            blaze_typeck::Expr::Binary { op, lhs, .. } | blaze_typeck::Expr::AssignOp { op, lhs, .. } => {
                if let Some(&lhs_ty) = checked.expr_ty.get(lhs.ix()) {
                    let is_ptr_cmp = matches!(
                        op,
                        blaze_typeck::BinOp::Eq
                            | blaze_typeck::BinOp::Ne
                            | blaze_typeck::BinOp::Lt
                            | blaze_typeck::BinOp::Le
                            | blaze_typeck::BinOp::Gt
                            | blaze_typeck::BinOp::Ge
                    ) && matches!(checked_tys.node(lhs_ty), Ok(PNode::Ptr(..) | PNode::Ref(..)));
                    if is_ptr_cmp {
                        if let Ok(PNode::Ptr(_, pointee) | PNode::Ref(_, pointee)) = checked_tys.node(lhs_ty) {
                            cx.walk_ty(pointee, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::VALUE_OP)?;
                        }
                    } else {
                        cx.walk_ty(lhs_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                    }
                }
            }
            blaze_typeck::Expr::Unary { op, operand } => {
                if let Some(&op_ty) = checked.expr_ty.get(operand.ix()) {
                    match op {
                        blaze_typeck::UnOp::Neg | blaze_typeck::UnOp::Not => {
                            cx.walk_ty(op_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                        }
                        blaze_typeck::UnOp::Deref => {
                            if !matches!(checked_tys.node(op_ty), Ok(PNode::Ptr(..) | PNode::Ref(..))) {
                                cx.walk_ty(op_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
                            }
                        }
                    }
                }
            }
            blaze_typeck::Expr::Cast { operand, .. } => {
                let src_ty = checked.expr_ty.get(operand.ix()).copied();
                let is_ptr_or_addr = |t: PTy| {
                    matches!(
                        checked_tys.node(t),
                        Ok(PNode::Ptr(..) | PNode::Ref(..) | PNode::FnPtr(..) | PNode::Uint(UintTy::Usize) | PNode::Int(IntTy::Isize))
                    )
                };
                if let (Some(src), Some(dst)) = (src_ty, expr_ty) {
                    if is_ptr_or_addr(src) && is_ptr_or_addr(dst) {
                        cx.walk_ty(src, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                        cx.walk_ty(dst, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    } else {
                        cx.walk_ty(src, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                        cx.walk_ty(dst, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                    }
                }
            }
            blaze_typeck::Expr::Index { base, .. } => {
                if let Some(&base_ty) = checked.expr_ty.get(base.ix()) {
                    match checked_tys.node(base_ty) {
                        Ok(PNode::Slice(elem) | PNode::Array(elem, _)) => {
                            cx.walk_ty(elem, WalkCtx::ByValue, ParamUse::SIZE_ALIGN, ParamUse::UNANALYZED_CALLEE)?;
                        }
                        _ => {
                            cx.walk_ty(base_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
                        }
                    }
                }
            }
            blaze_typeck::Expr::OffsetOf { .. } => {
                cx.mark_all_params(ParamUse::UNANALYZED_CALLEE);
            }
            blaze_typeck::Expr::Repeat { len, .. } => {
                if let Ok(PNode::ConstParam(p)) = view.types.node(len) {
                    cx.mark_param(p, ParamUse::CONST_PARAM)?;
                } else if !matches!(view.types.node(len), Ok(PNode::ConstValue { .. })) {
                    cx.mark_all_params(ParamUse::CONST_PARAM);
                }
            }
            blaze_typeck::Expr::Call { callee, .. } => {
                if let Some(&callee_ty) = checked.expr_ty.get(callee.ix())
                    && !matches!(checked_tys.node(callee_ty), Ok(PNode::FnDef(..)))
                {
                    cx.walk_ty(callee_ty, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                }
            }
            blaze_typeck::Expr::Missing
            | blaze_typeck::Expr::Lit(_)
            | blaze_typeck::Expr::Local(_)
            | blaze_typeck::Expr::Path(_)
            | blaze_typeck::Expr::Paren(_)
            | blaze_typeck::Expr::Block { .. }
            | blaze_typeck::Expr::If { .. }
            | blaze_typeck::Expr::Let { .. }
            | blaze_typeck::Expr::Match { .. }
            | blaze_typeck::Expr::Loop { .. }
            | blaze_typeck::Expr::Break { .. }
            | blaze_typeck::Expr::Continue { .. }
            | blaze_typeck::Expr::Return { .. }
            | blaze_typeck::Expr::Become { .. }
            | blaze_typeck::Expr::MethodCall { .. }
            | blaze_typeck::Expr::Field { .. }
            | blaze_typeck::Expr::TupleField { .. }
            | blaze_typeck::Expr::Assign { .. }
            | blaze_typeck::Expr::Ref { .. }
            | blaze_typeck::Expr::RawRef { .. }
            | blaze_typeck::Expr::Tuple { .. }
            | blaze_typeck::Expr::Array { .. }
            | blaze_typeck::Expr::Struct { .. }
            | blaze_typeck::Expr::Record { .. }
            | blaze_typeck::Expr::Underscore => {}
        }
    }

    // 4. Walk adjustments (`Unsize`, overloaded `Deref`, fn-ptr reification, etc.).
    {
        let adj_exprs = checked.adjusts.column::<0>();
        let adj_kinds = checked.adjusts.column::<1>();
        let adj_tys = checked.adjusts.column::<2>();
        for row in 0..checked.adjusts.len() {
            let e = adj_exprs[row];
            let kind = adj_kinds[row];
            let dst = adj_tys[row];
            match kind {
                blaze_typeck::AdjustKind::Unsize => {
                    if let Some(&src) = checked.expr_ty.get(e.ix()) {
                        cx.walk_ty(src, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                    }
                    cx.walk_ty(dst, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                }
                blaze_typeck::AdjustKind::Deref { overloaded: Some(_) } => {
                    if let Some(&src) = checked.expr_ty.get(e.ix()) {
                        cx.walk_ty(src, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
                    }
                    cx.walk_ty(dst, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
                }
                blaze_typeck::AdjustKind::ReifyFnPointer | blaze_typeck::AdjustKind::ClosureFnPointer => {
                    if let Some(&src) = checked.expr_ty.get(e.ix()) {
                        cx.walk_ty(src, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                    cx.walk_ty(dst, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                }
                blaze_typeck::AdjustKind::Deref { overloaded: None } => {
                    // Builtin `&T -> *const T` / `&mut T -> &T` coercions lower as `Deref(None)`
                    // immediately followed by `Borrow` on the same expression: a pure pointer
                    // reinterpretation that never loads `T` by value.
                    let followed_by_borrow = row + 1 < checked.adjusts.len()
                        && adj_exprs[row + 1] == e
                        && matches!(adj_kinds[row + 1], blaze_typeck::AdjustKind::Borrow { .. });
                    if followed_by_borrow {
                        cx.walk_ty(dst, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::UNANALYZED_CALLEE)?;
                    } else {
                        cx.walk_ty(dst, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                    }
                }
                blaze_typeck::AdjustKind::NeverToAny
                | blaze_typeck::AdjustKind::Borrow { .. }
                | blaze_typeck::AdjustKind::UnsafeFnPointer
                | blaze_typeck::AdjustKind::MutToConstPointer
                | blaze_typeck::AdjustKind::UnionInject { .. }
                | blaze_typeck::AdjustKind::UnionWiden
                | blaze_typeck::AdjustKind::RecordProject => {
                    cx.walk_ty(dst, WalkCtx::ByValue, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
                }
            }
        }
    }

    // 5. Walk associated type projections and non-Sized trait obligations (`checked.selected`).
    for proj in checked.selected.iter() {
        if proj.name.is_some() || !proj.global_env {
            cx.walk_list(proj.args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
        }
    }

    // 6. Walk generic const arguments (`checked.const_args`, `checked.pat_const_args`).
    for &list in checked.const_args.column::<1>() {
        cx.walk_list(list, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
    }
    for &list in checked.pat_const_args.column::<1>() {
        cx.walk_list(list, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
    }

    // 7. Walk resolved callees (`checked.callees`): classify intrinsics, flag trait methods,
    //    record direct generic calls for interprocedural propagation.
    {
        let items = checked.callees.column::<1>();
        let arg_lists = checked.callees.column::<2>();
        for row in 0..checked.callees.len() {
            cx.visit_callee_item(items[row], arg_lists[row])?;
        }
    }

    Ok(FnAnalysis { item, param_count, local_use, call_edges, edge_args })
}

struct CheckedWalkCx<'r, 'f, 's, 'e> {
    tys: PTys<'r>,
    facts: &'f dyn AnalysisFacts,
    local_use: &'s mut [ParamUse],
    call_edges: &'s mut AVec<'e, CallEdge>,
    edge_args: &'s mut AVec<'e, PTy>,
    stack: &'s mut AVec<'e, (PTy, WalkCtx)>,
    record_edges: bool,
}

impl<'e> CheckedWalkCx<'_, '_, '_, 'e> {
    fn mark_param(&mut self, index: u32, flag: ParamUse) -> Result<(), PolyError> {
        let Some(slot) = self.local_use.get_mut(index as usize) else {
            return Err(PolyError::ParamOutOfRange { index, param_count: self.local_use.len() as u32 });
        };
        *slot |= flag;
        Ok(())
    }

    fn mark_all_params(&mut self, flag: ParamUse) {
        for slot in self.local_use.iter_mut() {
            *slot |= flag;
        }
    }

    fn record_edge(&mut self, callee: ItemKey, args: List, is_drop_edge: bool) -> Result<(), PolyError> {
        let list = self.tys.list(args).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(args)))?;
        if list.is_empty() {
            return Ok(());
        }
        if !self.record_edges {
            // Signature-only walk (types from `sig.types`, not `checked.types`): conservatively
            // mark params concrete rather than mixing `PTys` tables in `edge_args`.
            for &arg in list {
                self.walk_ty(arg, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNANALYZED_CALLEE)?;
            }
            return Ok(());
        }
        // Avoid pushing identical duplicate edges from both `expr_ty(FnDef)` and `checked.callees`.
        for existing in self.call_edges.iter() {
            if existing.callee == callee
                && existing.is_drop_edge == is_drop_edge
                && existing.args_len as usize == list.len()
                && &self.edge_args[existing.args_start as usize..(existing.args_start + existing.args_len) as usize] == list
            {
                return Ok(());
            }
        }
        let start = self.edge_args.len() as u32;
        self.edge_args.extend_from_slice(list);
        let len = self.edge_args.len() as u32 - start;
        self.call_edges.push(CallEdge { callee, args_start: start, args_len: len, is_drop_edge });
        Ok(())
    }

    fn visit_callee_item(&mut self, key: ItemKey, args: List) -> Result<(), PolyError> {
        if let Some(which) = self.facts.intrinsic_which(key) {
            match IntrinsicClass::of(which) {
                IntrinsicClass::NoTypeDependence => {
                    self.walk_list(args, WalkCtx::Phantom, ParamUse::PHANTOM, ParamUse::VALUE_OP)?;
                }
                IntrinsicClass::OpaquePtr => {
                    self.walk_list(args, WalkCtx::BehindThinPtr, ParamUse::OPAQUE_PTR, ParamUse::UNANALYZED_CALLEE)?;
                }
                IntrinsicClass::LayoutOnly => {
                    let layout_flag = match which {
                        Which::NeedsDrop => ParamUse::DROP_GLUE,
                        Which::SizeOf | Which::AlignOf | Which::SizeOfVal | Which::AlignOfVal => ParamUse::SIZE_ALIGN,
                        _ => ParamUse::SIZE_ALIGN | ParamUse::BY_VALUE,
                    };
                    self.walk_list(args, WalkCtx::ByValue, layout_flag, ParamUse::UNANALYZED_CALLEE)?;
                }
                IntrinsicClass::Reflection => {
                    self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::REFLECTION)?;
                }
                IntrinsicClass::ValueOp => {
                    self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::VALUE_OP)?;
                }
                IntrinsicClass::UnsizedMeta => {
                    self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::UNSIZED_META)?;
                }
            }
        } else if self.facts.is_trait_method(key) {
            self.walk_list(args, WalkCtx::Concrete, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH)?;
        } else {
            self.record_edge(key, args, false)?;
        }
        Ok(())
    }

    fn walk_list(
        &mut self,
        list: List,
        ctx: WalkCtx,
        layout_flag: ParamUse,
        concrete_flag: ParamUse,
    ) -> Result<(), PolyError> {
        let items = self.tys.list(list).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(list)))?;
        for &t in items {
            self.walk_ty(t, ctx, layout_flag, concrete_flag)?;
        }
        Ok(())
    }

    fn walk_ty(
        &mut self,
        root: PTy,
        root_ctx: WalkCtx,
        layout_flag: ParamUse,
        concrete_flag: ParamUse,
    ) -> Result<(), PolyError> {
        self.stack.clear();
        self.stack.push((root, root_ctx));
        let mut visited: usize = 0;
        while let Some((ty, ctx)) = self.stack.pop() {
            visited += 1;
            if visited > MAX_TYPE_WALK_NODES {
                return Err(PolyError::TypeWalkCap { cap: MAX_TYPE_WALK_NODES });
            }
            let node = self.tys.node(ty).map_err(|_| PolyError::Ty(TyError::NotInRecord(ty)))?;
            match node {
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Str
                | PNode::Never
                | PNode::Foreign(_) => {}
                PNode::Param(i) => {
                    let flag = ctx.to_param_use(layout_flag, concrete_flag);
                    self.mark_param(i, flag)?;
                }
                PNode::ConstParam(i) => {
                    if ctx != WalkCtx::Phantom {
                        self.mark_param(i, ParamUse::CONST_PARAM)?;
                    }
                }
                PNode::Ref(_, inner) | PNode::Ptr(_, inner) => {
                    let next_ctx = match ctx {
                        WalkCtx::Phantom => WalkCtx::Phantom,
                        WalkCtx::Concrete => WalkCtx::Concrete,
                        WalkCtx::BehindThinPtr | WalkCtx::ByValue => WalkCtx::BehindThinPtr,
                    };
                    let inner_node = self.tys.node(inner).map_err(|_| PolyError::Ty(TyError::NotInRecord(inner)))?;
                    if let PNode::Slice(elem) = inner_node {
                        self.stack.push((elem, next_ctx));
                    } else {
                        self.stack.push((inner, next_ctx));
                    }
                }
                PNode::Tuple(l) | PNode::Record(_, l) | PNode::Union(l) => {
                    let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    for &child in items {
                        self.stack.push((child, ctx));
                    }
                }
                PNode::Array(elem, len) => {
                    self.stack.push((elem, ctx));
                    let len_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                    self.stack.push((len, len_ctx));
                }
                PNode::Slice(elem) => self.stack.push((elem, ctx)),
                PNode::Pat(base, ranges) => {
                    self.stack.push((base, ctx));
                    let items = self.tys.list(ranges).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(ranges)))?;
                    for &r in items {
                        self.stack.push((r, WalkCtx::Concrete));
                    }
                }
                PNode::ConstValue { ty: cty, .. } => {
                    let c_ctx = if ctx == WalkCtx::Phantom { WalkCtx::Phantom } else { WalkCtx::Concrete };
                    self.stack.push((cty, c_ctx));
                }
                PNode::FnPtr(l, header) => {
                    let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    let next_ctx = if ctx == WalkCtx::Phantom {
                        WalkCtx::Phantom
                    } else if header.abi != Abi::Rust || header.c_variadic {
                        WalkCtx::Concrete
                    } else {
                        WalkCtx::ByValue
                    };
                    for &child in items {
                        self.stack.push((child, next_ctx));
                    }
                }
                PNode::Adt(k, l) => {
                    let args = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    if ctx == WalkCtx::Phantom || self.facts.phantom_data() == Some(k) {
                        for &arg in args {
                            self.stack.push((arg, WalkCtx::Phantom));
                        }
                    } else if self.facts.non_null() == Some(k) {
                        let next_ctx =
                            if ctx == WalkCtx::Concrete { WalkCtx::Concrete } else { WalkCtx::BehindThinPtr };
                        for &arg in args {
                            self.stack.push((arg, next_ctx));
                        }
                    } else if let Some((param_uses, _has_drop)) = self.facts.adt_summary(k) {
                        if param_uses.len() == args.len() {
                            for (&arg, &pu) in args.iter().zip(param_uses.iter()) {
                                self.stack.push((arg, combine_adt_ctx(ctx, pu)));
                            }
                        } else {
                            for &arg in args {
                                self.stack.push((arg, WalkCtx::Concrete));
                            }
                        }
                    } else {
                        for &arg in args {
                            self.stack.push((arg, WalkCtx::Concrete));
                        }
                    }
                }
                PNode::FnDef(k, l) => {
                    if ctx == WalkCtx::Phantom {
                        let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                        for &child in items {
                            self.stack.push((child, WalkCtx::Phantom));
                        }
                    } else {
                        // Save outer stack while classifying callee (visit_callee_item reuses `self.stack`
                        // only via `walk_list`, so classify intrinsics/trait methods directly on `self.stack`
                        // to preserve outer traversal!).
                        let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                        if let Some(which) = self.facts.intrinsic_which(k) {
                            let child_ctx = match IntrinsicClass::of(which) {
                                IntrinsicClass::NoTypeDependence => WalkCtx::Phantom,
                                IntrinsicClass::OpaquePtr => WalkCtx::BehindThinPtr,
                                IntrinsicClass::LayoutOnly => WalkCtx::ByValue,
                                IntrinsicClass::Reflection
                                | IntrinsicClass::ValueOp
                                | IntrinsicClass::UnsizedMeta => WalkCtx::Concrete,
                            };
                            for &child in items {
                                self.stack.push((child, child_ctx));
                            }
                        } else if self.facts.is_trait_method(k) {
                            for &child in items {
                                self.stack.push((child, WalkCtx::Concrete));
                            }
                        } else if !items.is_empty() {
                            if self.record_edges {
                                self.record_edge(k, l, false)?;
                            } else {
                                for &child in items {
                                    self.stack.push((child, WalkCtx::Concrete));
                                }
                            }
                        }
                    }
                }
                PNode::Alias { args: l, .. }
                | PNode::Opaque(_, l)
                | PNode::Dynamic(l)
                | PNode::ExTrait(_, l)
                | PNode::Closure { args: l, .. }
                | PNode::Coroutine { args: l, .. } => {
                    let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    for &child in items {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                }
                PNode::ExProj { args, term, .. } => {
                    let items = self.tys.list(args).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(args)))?;
                    for &child in items {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                    self.stack.push((term, WalkCtx::Concrete));
                }
                PNode::Future(_, a, b) => {
                    self.stack.push((a, WalkCtx::Concrete));
                    self.stack.push((b, WalkCtx::Concrete));
                }
                PNode::ExAuto(_) | PNode::Infer | PNode::Error => {}
            }
        }
        Ok(())
    }

    fn walk_drop_ty(&mut self, root: PTy) -> Result<(), PolyError> {
        self.stack.clear();
        self.stack.push((root, WalkCtx::ByValue));
        let mut visited: usize = 0;
        while let Some((ty, ctx)) = self.stack.pop() {
            visited += 1;
            if visited > MAX_TYPE_WALK_NODES {
                return Err(PolyError::TypeWalkCap { cap: MAX_TYPE_WALK_NODES });
            }
            let node = self.tys.node(ty).map_err(|_| PolyError::Ty(TyError::NotInRecord(ty)))?;
            match node {
                PNode::Bool
                | PNode::Char
                | PNode::Int(_)
                | PNode::Uint(_)
                | PNode::Float(_)
                | PNode::Str
                | PNode::Never
                | PNode::Foreign(_)
                | PNode::Ref(..)
                | PNode::Ptr(..)
                | PNode::FnDef(..)
                | PNode::FnPtr(..)
                | PNode::Union(_)
                | PNode::ConstValue { .. }
                | PNode::ExAuto(_) => {}
                PNode::Param(i) => {
                    let flag = if ctx == WalkCtx::Concrete {
                        ParamUse::UNANALYZED_CALLEE | ParamUse::DROP_GLUE
                    } else {
                        ParamUse::DROP_GLUE
                    };
                    self.mark_param(i, flag)?;
                }
                PNode::ConstParam(i) => {
                    self.mark_param(i, ParamUse::CONST_PARAM)?;
                }
                PNode::Tuple(l) | PNode::Record(_, l) => {
                    let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    for &child in items {
                        self.stack.push((child, ctx));
                    }
                }
                PNode::Array(elem, len) => match self.tys.node(len).map_err(|_| PolyError::Ty(TyError::NotInRecord(len)))? {
                    PNode::ConstValue { bits: 0, .. } => {}
                    PNode::ConstParam(n) => {
                        self.mark_param(n, ParamUse::CONST_PARAM)?;
                        self.stack.push((elem, ctx));
                    }
                    _ => self.stack.push((elem, ctx)),
                },
                PNode::Slice(elem) | PNode::Pat(elem, _) => self.stack.push((elem, ctx)),
                PNode::Adt(k, l) => {
                    if self.facts.phantom_data() == Some(k)
                        || self.facts.manually_drop() == Some(k)
                        || self.facts.non_null() == Some(k)
                    {
                        continue;
                    }
                    let args = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    if let Some((param_uses, has_drop_impl)) = self.facts.adt_summary(k) {
                        if has_drop_impl {
                            if self.record_edges
                                && let Some(drop_fn) = self.facts.adt_drop_fn(k)
                            {
                                self.record_edge(drop_fn, l, true)?;
                            } else {
                                for &arg in args {
                                    self.stack.push((arg, WalkCtx::Concrete));
                                }
                            }
                        }
                        if param_uses.len() == args.len() {
                            for (&arg, &pu) in args.iter().zip(param_uses.iter()) {
                                match pu {
                                    AdtParamUse::Unused | AdtParamUse::OpaquePtr => {}
                                    AdtParamUse::ByValue => self.stack.push((arg, ctx)),
                                    AdtParamUse::Concrete => self.stack.push((arg, WalkCtx::Concrete)),
                                }
                            }
                        } else {
                            for &arg in args {
                                self.stack.push((arg, WalkCtx::Concrete));
                            }
                        }
                    } else {
                        for &arg in args {
                            self.stack.push((arg, WalkCtx::Concrete));
                        }
                    }
                }
                PNode::Dynamic(l)
                | PNode::ExTrait(_, l)
                | PNode::Alias { args: l, .. }
                | PNode::Opaque(_, l)
                | PNode::Closure { args: l, .. }
                | PNode::Coroutine { args: l, .. } => {
                    let items = self.tys.list(l).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(l)))?;
                    for &child in items {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                }
                PNode::ExProj { args, term, .. } => {
                    let items = self.tys.list(args).map_err(|_| PolyError::Ty(TyError::ListNotInRecord(args)))?;
                    for &child in items {
                        self.stack.push((child, WalkCtx::Concrete));
                    }
                    self.stack.push((term, WalkCtx::Concrete));
                }
                PNode::Future(_, a, b) => {
                    self.stack.push((a, WalkCtx::Concrete));
                    self.stack.push((b, WalkCtx::Concrete));
                }
                PNode::Infer | PNode::Error => {}
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Deduplication Table & Statistics
// ---------------------------------------------------------------------------

/// Summary statistics from deduplicating monomorphized instances via [`CanonTable`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CanonStats {
    /// Total concrete instances submitted.
    pub total_instances: usize,
    /// Distinct canonical instances remaining after polymorphization.
    pub unique_canonical: usize,
    /// Instances that hit an existing canonical instance (compile work saved!).
    pub deduplicated_instances: usize,
    /// Total generic arguments canonicalized away from concrete.
    pub collapsed_params: usize,
}

impl CanonStats {
    /// Deduplication ratio scaled by 100 (e.g. `400` = `4.00x` reduction, `100` = `1.00x`).
    pub const fn dedup_ratio_x100(&self) -> usize {
        if self.unique_canonical == 0 {
            100
        } else {
            (self.total_instances * 100) / self.unique_canonical
        }
    }
}

/// Sans-IO canonical instance deduplication table (explicit `Alloc`, deterministic open
/// addressing keyed by canonical [`Symbol`]).
pub struct CanonTable<'a> {
    alloc: Alloc<'a>,
    symbols: AVec<'a, Symbol>,
    /// Open-addressing slots over `symbols`: `0` empty, else `row + 1`.
    slots: AVec<'a, u32>,
    stats: CanonStats,
}

impl<'a> CanonTable<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> CanonTable<'a> {
        CanonTable { alloc, symbols: Vec::new_in(alloc), slots: Vec::new_in(alloc), stats: CanonStats::default() }
    }

    /// Canonicalizes `(item, args)` under `usage` and interns its canonical [`Symbol`].
    /// Returns `(symbol, is_new)` where `is_new == false` means this instance collapsed into a
    /// previously compiled canonical instance.
    pub fn intern(
        &mut self,
        item: ItemKey,
        usage: &[ParamUse],
        args: &[ArgInfo],
        ground: PTys<'_>,
        scratch: Alloc<'_>,
    ) -> Result<(Symbol, bool), PolyError> {
        let canon = canonicalize_args(usage, args, scratch)?;
        let sym = canonical_fn_symbol(item, canon.as_slice(), ground, scratch)?;
        self.stats.total_instances += 1;
        self.stats.collapsed_params += canon.canonicalized_count();
        let is_new = self.insert_symbol(sym);
        if is_new {
            self.stats.unique_canonical += 1;
        } else {
            self.stats.deduplicated_instances += 1;
        }
        Ok((sym, is_new))
    }

    #[inline]
    pub fn stats(&self) -> CanonStats {
        self.stats
    }

    #[inline]
    pub fn unique_count(&self) -> usize {
        self.symbols.len()
    }

    fn insert_symbol(&mut self, sym: Symbol) -> bool {
        if (self.symbols.len() + 1) * 2 > self.slots.len() {
            let new_len = (self.slots.len() * 2).max(16);
            self.slots.clear();
            self.slots.resize(new_len, 0);
            let mask = new_len - 1;
            for (row, s) in self.symbols.iter().enumerate() {
                let mut at = sym_slot(*s, mask);
                while self.slots[at] != 0 {
                    at = (at + 1) & mask;
                }
                self.slots[at] = row as u32 + 1;
            }
        }
        let mask = self.slots.len() - 1;
        let mut at = sym_slot(sym, mask);
        loop {
            match self.slots[at] {
                0 => {
                    let row = self.symbols.len() as u32;
                    self.symbols.push(sym);
                    self.slots[at] = row + 1;
                    return true;
                }
                n if self.symbols[n as usize - 1] == sym => return false,
                _ => at = (at + 1) & mask,
            }
        }
    }

    #[allow(dead_code)]
    fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }
}

#[inline]
fn sym_slot(s: Symbol, mask: usize) -> usize {
    let h = u64::from_le_bytes([s.0[0], s.0[1], s.0[2], s.0[3], s.0[4], s.0[5], s.0[6], s.0[7]]);
    (h as usize) & mask
}

// ---------------------------------------------------------------------------
// Internal Shared-Forest Encoder for Concrete PTy Arguments (Rule 25)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ForestFrame {
    ty: PTy,
    node: PNode,
    kids_at: u32,
    kids_len: u32,
    done_at: u32,
}

fn encode_ground_forest(
    top: &[PTy],
    ground: PTys<'_>,
    scratch: Alloc<'_>,
    out: &mut AVec<'_, u8>,
) -> Result<(), PolyError> {
    out.extend_from_slice(&(top.len() as u32).to_le_bytes());
    let count_at = out.len();
    out.extend_from_slice(&0u32.to_le_bytes());
    let base = out.len();

    let mut spans: AVec<'_, (u32, u32)> = Vec::new_in(scratch);
    let mut by_bytes: AVec<'_, u32> = Vec::new_in(scratch);
    let mut memo: AVec<'_, (PTy, u32)> = Vec::new_in(scratch);
    let mut by_ty: AVec<'_, u32> = Vec::new_in(scratch);
    let mut node_buf: AVec<'_, u8> = Vec::new_in(scratch);
    let mut frames: AVec<'_, ForestFrame> = Vec::new_in(scratch);
    let mut kids: AVec<'_, PTy> = Vec::new_in(scratch);
    let mut done: AVec<'_, u32> = Vec::new_in(scratch);
    let mut refs: AVec<'_, u32> = Vec::with_capacity_in(top.len(), scratch);

    for &root in top {
        if let Some(existing) = lookup_memo(&memo, &by_ty, root) {
            refs.push(existing);
            continue;
        }
        open_forest_node(root, ground, &mut node_buf, &mut frames, &mut kids, &done)?;
        let mut last = 0u32;
        while let Some(&f) = frames.last() {
            let have = done.len() - f.done_at as usize;
            if have < f.kids_len as usize {
                let child = kids[f.kids_at as usize + have];
                match lookup_memo(&memo, &by_ty, child) {
                    Some(id) => done.push(id),
                    None => open_forest_node(child, ground, &mut node_buf, &mut frames, &mut kids, &done)?,
                }
                continue;
            }
            node_buf.clear();
            encode_pnode_head(f.node, &mut node_buf)?;
            node_buf.extend_from_slice(&f.kids_len.to_le_bytes());
            for i in f.done_at as usize..done.len() {
                node_buf.extend_from_slice(&done[i].to_le_bytes());
            }
            let id = write_forest_node(&mut spans, &mut by_bytes, node_buf.as_slice(), out, base)?;
            insert_memo(&mut memo, &mut by_ty, f.ty, id);
            kids.truncate(f.kids_at as usize);
            done.truncate(f.done_at as usize);
            frames.pop();
            if !frames.is_empty() {
                done.push(id);
            }
            last = id;
        }
        refs.push(last);
    }

    let count = spans.len() as u32;
    out[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
    for &r in refs.iter() {
        out.extend_from_slice(&r.to_le_bytes());
    }
    Ok(())
}

fn open_forest_node(
    ty: PTy,
    ground: PTys<'_>,
    node_buf: &mut AVec<'_, u8>,
    frames: &mut AVec<'_, ForestFrame>,
    kids: &mut AVec<'_, PTy>,
    done: &AVec<'_, u32>,
) -> Result<(), PolyError> {
    let node = ground.node(ty).map_err(|_| PolyError::NotGround { what: "a type past the ground table" })?;
    node_buf.clear();
    encode_pnode_head(node, node_buf)?;
    let kids_at = kids.len();
    each_child(ground, node, kids).map_err(|_| PolyError::NotGround { what: "a type's list past the ground table" })?;
    let kids_len = kids.len() - kids_at;
    frames.push(ForestFrame {
        ty,
        node,
        kids_at: kids_at as u32,
        kids_len: kids_len as u32,
        done_at: done.len() as u32,
    });
    Ok(())
}

fn encode_pnode_head(node: PNode, out: &mut AVec<'_, u8>) -> Result<(), PolyError> {
    match node {
        PNode::Bool => out.push(0),
        PNode::Char => out.push(1),
        PNode::Int(i) => out.extend_from_slice(&[2, i as u8]),
        PNode::Uint(u) => out.extend_from_slice(&[3, u as u8]),
        PNode::Float(x) => out.extend_from_slice(&[4, x as u8]),
        PNode::Str => out.push(5),
        PNode::Never => out.push(6),
        PNode::Tuple(_) => out.push(7),
        PNode::Ref(m, _) => out.extend_from_slice(&[8, m as u8]),
        PNode::Ptr(m, _) => out.extend_from_slice(&[9, m as u8]),
        PNode::Array(..) => out.push(10),
        PNode::Slice(_) => out.push(11),
        PNode::Adt(k, _) => {
            out.push(12);
            out.extend_from_slice(&k.0.to_le_bytes());
        }
        PNode::FnDef(k, _) => {
            out.push(13);
            out.extend_from_slice(&k.0.to_le_bytes());
        }
        PNode::ConstValue { bits, .. } => {
            out.push(14);
            out.extend_from_slice(&bits.to_le_bytes());
        }
        PNode::Pat(..) => out.push(15),
        PNode::Dynamic(_) => out.push(16),
        PNode::ExTrait(k, _) => {
            out.push(17);
            out.extend_from_slice(&k.0.to_le_bytes());
        }
        PNode::ExProj { trait_, name, .. } => {
            out.push(18);
            out.extend_from_slice(&trait_.0.to_le_bytes());
            out.extend_from_slice(&name.0.to_le_bytes());
        }
        PNode::ExAuto(k) => {
            out.push(19);
            out.extend_from_slice(&k.0.to_le_bytes());
        }
        PNode::Closure { owner, ordinal, .. } => {
            out.push(20);
            out.extend_from_slice(&owner.0.to_le_bytes());
            out.extend_from_slice(&ordinal.to_le_bytes());
        }
        PNode::FnPtr(_, header) => out.extend_from_slice(&[21, header.bits()]),
        PNode::Coroutine { owner, ordinal, .. } => {
            out.push(23);
            out.extend_from_slice(&owner.0.to_le_bytes());
            out.extend_from_slice(&ordinal.to_le_bytes());
        }
        PNode::Foreign(k) => {
            out.push(24);
            out.extend_from_slice(&k.0.to_le_bytes());
        }
        PNode::Record(s, _) => {
            out.push(25);
            out.extend_from_slice(&s.0.to_le_bytes());
        }
        PNode::Union(_) => out.push(26),
        PNode::Future(..) => return Err(PolyError::NotGround { what: "async block future" }),
        PNode::Param(_) => return Err(PolyError::NotGround { what: "generic type parameter" }),
        PNode::ConstParam(_) => return Err(PolyError::NotGround { what: "const generic parameter" }),
        PNode::Alias { .. } => return Err(PolyError::NotGround { what: "unnormalized projection" }),
        PNode::Opaque(..) => return Err(PolyError::NotGround { what: "opaque type" }),
        PNode::Infer => return Err(PolyError::NotGround { what: "inference hole" }),
        PNode::Error => return Err(PolyError::NotGround { what: "error type" }),
    }
    Ok(())
}

#[inline]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[inline]
fn pty_hash(t: PTy) -> u64 {
    u64::from(t.0).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

#[inline]
fn hash_slot(h: u64, len: usize) -> usize {
    ((h ^ (h >> 31)) as usize) & (len - 1)
}

fn ensure_slots(slots: &mut AVec<'_, u32>, rows: usize, hash_of: impl Fn(usize) -> u64) {
    if (rows + 1) * 2 <= slots.len() {
        return;
    }
    let mut len = slots.len().max(16);
    while (rows + 1) * 2 > len {
        len *= 2;
    }
    slots.clear();
    slots.resize(len, 0);
    let mask = len - 1;
    for row in 0..rows {
        let mut at = hash_slot(hash_of(row), len);
        while slots[at] != 0 {
            at = (at + 1) & mask;
        }
        slots[at] = row as u32 + 1;
    }
}

fn lookup_memo(memo: &[(PTy, u32)], by_ty: &[u32], ty: PTy) -> Option<u32> {
    if by_ty.is_empty() {
        return None;
    }
    let mask = by_ty.len() - 1;
    let mut at = hash_slot(pty_hash(ty), by_ty.len());
    loop {
        match by_ty[at] {
            0 => return None,
            n if memo[n as usize - 1].0 == ty => return Some(memo[n as usize - 1].1),
            _ => at = (at + 1) & mask,
        }
    }
}

fn insert_memo(memo: &mut AVec<'_, (PTy, u32)>, by_ty: &mut AVec<'_, u32>, ty: PTy, id: u32) {
    ensure_slots(by_ty, memo.len(), |row| pty_hash(memo[row].0));
    let mask = by_ty.len() - 1;
    let mut at = hash_slot(pty_hash(ty), by_ty.len());
    loop {
        match by_ty[at] {
            0 => {
                by_ty[at] = memo.len() as u32 + 1;
                memo.push((ty, id));
                return;
            }
            n if memo[n as usize - 1].0 == ty => return,
            _ => at = (at + 1) & mask,
        }
    }
}

fn write_forest_node(
    spans: &mut AVec<'_, (u32, u32)>,
    by_bytes: &mut AVec<'_, u32>,
    node_bytes: &[u8],
    out: &mut AVec<'_, u8>,
    base: usize,
) -> Result<u32, PolyError> {
    let found = {
        let written = &out[base..];
        let rows = spans.as_slice();
        let at = move |row: usize| {
            let (s, l) = rows[row];
            &written[s as usize..(s + l) as usize]
        };
        ensure_slots(by_bytes, rows.len(), |row| fnv1a(at(row)));
        let mask = by_bytes.len() - 1;
        let mut slot = hash_slot(fnv1a(node_bytes), by_bytes.len());
        loop {
            match by_bytes[slot] {
                0 => break Err(slot),
                n if at(n as usize - 1) == node_bytes => break Ok(n - 1),
                _ => slot = (slot + 1) & mask,
            }
        }
    };
    match found {
        Ok(id) => Ok(id),
        Err(slot) => {
            let row = spans.len();
            let start = out.len() - base;
            if row as u32 >= NAME_MOST_NODES || start + node_bytes.len() > NAME_MOST_BYTES {
                return Err(PolyError::ForestTooLarge { nodes: row as u32 + 1, bytes: start + node_bytes.len() });
            }
            out.extend_from_slice(node_bytes);
            spans.push((start as u32, node_bytes.len() as u32));
            by_bytes[slot] = row as u32 + 1;
            Ok(row as u32)
        }
    }
}

// ---------------------------------------------------------------------------
// Thorough Unit Tests & Soundness Controls (Rule 23)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use blaze_body::{BlockKind, BodyBuilder, ConstDecl, LocalDecl, LocalKind, Span, Unwind};
    use blaze_build::layout::{LayoutCx, LayoutFacts, Layouts, Missing, ReprAttrs, ValidRange};
    use blaze_typeck::{Adt, ExprId, Name};

    const PHANTOM_DATA: ItemKey = ItemKey(0xA001);
    const NON_NULL: ItemKey = ItemKey(0xA002);
    const UNIQUE: ItemKey = ItemKey(0xA003);
    const RAW_VEC: ItemKey = ItemKey(0xA004);
    const VEC: ItemKey = ItemKey(0xA005);
    const OPTION: ItemKey = ItemKey(0xA006);
    const MANUALLY_DROP: ItemKey = ItemKey(0xA007);
    const WITH_ASSOC: ItemKey = ItemKey(0xA008);
    const LOG_ON_DROP: ItemKey = ItemKey(0xA009);
    const FOO: ItemKey = ItemKey(0xB001);
    const BAR: ItemKey = ItemKey(0xB002);
    const BAZ: ItemKey = ItemKey(0xB003);
    const STRING_ADT: ItemKey = ItemKey(0xB004);

    const TRAIT_DISPLAY_FMT: ItemKey = ItemKey(0xC001);
    const TRAIT_ITERATOR: ItemKey = ItemKey(0xC002);

    const INTRINSIC_TYPE_ID: ItemKey = ItemKey(0xD001);
    const INTRINSIC_TYPE_NAME: ItemKey = ItemKey(0xD002);
    const INTRINSIC_READ_VIA_COPY: ItemKey = ItemKey(0xD003);
    const INTRINSIC_WRITE_VIA_MOVE: ItemKey = ItemKey(0xD004);
    const INTRINSIC_SIZE_OF: ItemKey = ItemKey(0xD005);
    const INTRINSIC_NEEDS_DROP: ItemKey = ItemKey(0xD006);
    const INTRINSIC_PTR_MASK: ItemKey = ItemKey(0xD007);

    const FN_LOG_DROP: ItemKey = ItemKey(0xE001);
    const FN_VEC_DROP: ItemKey = ItemKey(0xE002);

    struct TestFacts<'a> {
        adt_graph: Option<&'a AdtGraphSummary<'a>>,
    }

    impl AnalysisFacts for TestFacts<'_> {
        fn phantom_data(&self) -> Option<ItemKey> {
            Some(PHANTOM_DATA)
        }
        fn non_null(&self) -> Option<ItemKey> {
            Some(NON_NULL)
        }
        fn manually_drop(&self) -> Option<ItemKey> {
            Some(MANUALLY_DROP)
        }
        fn adt_summary(&self, adt: ItemKey) -> Option<(&[AdtParamUse], bool)> {
            if let Some(g) = self.adt_graph {
                if let Some(res) = g.get(adt) {
                    return Some(res);
                }
            }
            match adt {
                PHANTOM_DATA => Some((&[AdtParamUse::Unused], false)),
                NON_NULL => Some((&[AdtParamUse::OpaquePtr], false)),
                OPTION => Some((&[AdtParamUse::ByValue], false)),
                MANUALLY_DROP => Some((&[AdtParamUse::ByValue], false)),
                VEC => Some((&[AdtParamUse::OpaquePtr], true)),
                LOG_ON_DROP => Some((&[AdtParamUse::ByValue], true)),
                WITH_ASSOC => Some((&[AdtParamUse::Concrete], false)),
                FOO | BAR | BAZ => Some((&[], false)),
                STRING_ADT => Some((&[], true)),
                _ => None,
            }
        }
        fn adt_drop_fn(&self, adt: ItemKey) -> Option<ItemKey> {
            match adt {
                LOG_ON_DROP => Some(FN_LOG_DROP),
                VEC => Some(FN_VEC_DROP),
                _ => None,
            }
        }
        fn intrinsic_which(&self, item: ItemKey) -> Option<Which> {
            match item {
                INTRINSIC_TYPE_ID => Some(Which::TypeId),
                INTRINSIC_TYPE_NAME => Some(Which::TypeName),
                INTRINSIC_READ_VIA_COPY => Some(Which::ReadViaCopy),
                INTRINSIC_WRITE_VIA_MOVE => Some(Which::WriteViaMove),
                INTRINSIC_SIZE_OF => Some(Which::SizeOf),
                INTRINSIC_NEEDS_DROP => Some(Which::NeedsDrop),
                INTRINSIC_PTR_MASK => Some(Which::PtrMask),
                _ => None,
            }
        }
    }

    fn test_frame() -> blaze_mem::Frame {
        blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("test frame")
    }

    fn zero_span() -> Span {
        Span { lo: 0, hi: 0 }
    }

    /// Verifies that the ADT fixed-point analyzer automatically discovers:
    /// - `PhantomData<T>` -> `Unused`
    /// - `NonNull<T>` -> `OpaquePtr`
    /// - `Unique<T>` (contains `NonNull<T>` + `PhantomData<T>`) -> `OpaquePtr`
    /// - `RawVec<T>` (contains `Unique<T>` + `usize`) -> `OpaquePtr`
    /// - `Vec<T>` (contains `RawVec<T>` + `usize`) -> `OpaquePtr` (with `has_drop_impl = true`)
    /// - `Option<T>` -> `ByValue`
    /// - `WithAssoc<T>` (contains `<T as Iterator>::Item`) -> `Concrete`!
    #[test]
    fn adt_graph_discovers_vec_opaque_ptr_and_assoc_concrete() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let usize_ty = tys.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let phantom_t = tys.intern(PNode::Adt(PHANTOM_DATA, t0_list)).unwrap();
        let non_null_t = tys.intern(PNode::Adt(NON_NULL, t0_list)).unwrap();
        let unique_t = tys.intern(PNode::Adt(UNIQUE, t0_list)).unwrap();
        let raw_vec_t = tys.intern(PNode::Adt(RAW_VEC, t0_list)).unwrap();
        let assoc_t = tys.intern(PNode::Alias { trait_: TRAIT_ITERATOR, name: Name(1), args: t0_list }).unwrap();

        let view = tys.view();
        let defs = [
            AdtDefInput { key: PHANTOM_DATA, param_count: 1, field_tys: &[], has_drop_impl: false, tys: view },
            AdtDefInput { key: NON_NULL, param_count: 1, field_tys: &[], has_drop_impl: false, tys: view },
            AdtDefInput {
                key: UNIQUE,
                param_count: 1,
                field_tys: &[non_null_t, phantom_t],
                has_drop_impl: false,
                tys: view,
            },
            AdtDefInput {
                key: RAW_VEC,
                param_count: 1,
                field_tys: &[unique_t, usize_ty],
                has_drop_impl: true,
                tys: view,
            },
            AdtDefInput {
                key: VEC,
                param_count: 1,
                field_tys: &[raw_vec_t, usize_ty],
                has_drop_impl: true,
                tys: view,
            },
            AdtDefInput { key: OPTION, param_count: 1, field_tys: &[t0], has_drop_impl: false, tys: view },
            AdtDefInput { key: WITH_ASSOC, param_count: 1, field_tys: &[assoc_t], has_drop_impl: false, tys: view },
        ];

        let facts = TestFacts { adt_graph: None };
        let summary = analyze_adt_graph(&defs, &facts, alloc).unwrap();

        assert_eq!(summary.get(PHANTOM_DATA), Some((&[AdtParamUse::Unused][..], false)));
        assert_eq!(summary.get(NON_NULL), Some((&[AdtParamUse::OpaquePtr][..], false)));
        assert_eq!(summary.get(UNIQUE), Some((&[AdtParamUse::OpaquePtr][..], false)));
        assert_eq!(summary.get(RAW_VEC), Some((&[AdtParamUse::OpaquePtr][..], true)));
        assert_eq!(summary.get(VEC), Some((&[AdtParamUse::OpaquePtr][..], true)));
        assert_eq!(summary.get(OPTION), Some((&[AdtParamUse::ByValue][..], false)));
        assert_eq!(summary.get(WITH_ASSOC), Some((&[AdtParamUse::Concrete][..], false)));
    }

    /// Positive control 1: `PhantomData<T>` unused marker param collapses to `CanonTy::Unused`
    /// even for `Drop` or unsized types!
    #[test]
    fn positive_phantom_data_unused_param_collapses() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let usize_ty = tys.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let phantom_t = tys.intern(PNode::Adt(PHANTOM_DATA, t0_list)).unwrap();

        // fn marker_id<T>(_p: PhantomData<T>) -> usize { 42 }
        let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
        let ret = b.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let _arg = b.new_local(LocalDecl { ty: phantom_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let c42 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 42 }, ty: usize_ty });
        let bb0 = b.new_block(BlockKind::Normal);
        b.start(bb0);
        let ret_p = b.local_place(ret);
        b.push(StmtKind::Assign { dest: ret_p, value: Value::Use(Operand::Const(c42)) }, zero_span());
        b.terminate(StmtKind::Return, zero_span());
        let body = b.finish().unwrap();

        let fn_key = ItemKey(0x1001);
        let facts = TestFacts { adt_graph: None };
        let analysis = analyze_body(fn_key, 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
        assert_eq!(analysis.local_use[0].level(), CanonLevel::Unused);
        assert!(analysis.local_use[0].contains(ParamUse::PHANTOM));

        // Ground instantiations: u64, String (Drop!), str (unsized!) ALL collapse to ONE symbol!
        let mut ground = PortableTys::new_in(alloc);
        let g_u64 = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
        let g_string = ground.intern(PNode::Adt(STRING_ADT, List::EMPTY)).unwrap();
        let g_str = ground.intern(PNode::Str).unwrap();

        let c_u64 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_u64, LayoutShape::INT64, false)], alloc).unwrap();
        let c_string = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_string, LayoutShape::plain(24, 8, AbiClass::Indirect), true)], alloc).unwrap();
        let c_str = canonicalize_args(&analysis.local_use, &[ArgInfo::unsized_ty(g_str, false)], alloc).unwrap();

        assert_eq!(c_u64.as_slice(), &[CanonTy::Unused]);
        assert_eq!(c_string.as_slice(), &[CanonTy::Unused]);
        assert_eq!(c_str.as_slice(), &[CanonTy::Unused]);

        let s_u64 = canonical_fn_symbol(fn_key, c_u64.as_slice(), ground.view(), alloc).unwrap();
        let s_string = canonical_fn_symbol(fn_key, c_string.as_slice(), ground.view(), alloc).unwrap();
        let s_str = canonical_fn_symbol(fn_key, c_str.as_slice(), ground.view(), alloc).unwrap();
        assert_eq!(s_u64, s_string);
        assert_eq!(s_u64, s_str);
    }

    /// Positive control 2: `Option<&Foo>::is_some` == `Option<&Bar>::is_some`, AND negative
    /// control: `Option<u64>::is_some` (no niche!) MUST NOT collide with `Option<&Foo>::is_some`!
    #[test]
    fn positive_option_ref_is_some_and_negative_niche_control() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let bool_ty = tys.intern(PNode::Bool).unwrap();
        let u8_ty = tys.intern(PNode::Uint(UintTy::U8)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let opt_t = tys.intern(PNode::Adt(OPTION, t0_list)).unwrap();
        let ref_opt_t = tys.intern(PNode::Ref(Mutbl::Not, opt_t)).unwrap();

        // fn is_some<T>(self: &Option<T>) -> bool { discriminant(*self) == 1 }
        let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
        let ret = b.new_local(LocalDecl { ty: bool_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let self_arg = b.new_local(LocalDecl { ty: ref_opt_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let discr = b.new_local(LocalDecl { ty: u8_ty, kind: LocalKind::Temp(ExprId(0), blaze_body::Role::Value), mutbl: Mutbl::Mut });
        let self_p = b.local_place(self_arg);
        let deref_self = b.project(self_p, Proj::Deref);
        let discr_p = b.local_place(discr);
        let ret_p = b.local_place(ret);
        let c1 = b.new_const(ConstDecl { kind: ConstKind::Scalar { bits: 1 }, ty: u8_ty });

        let bb0 = b.new_block(BlockKind::Normal);
        b.start(bb0);
        b.push(StmtKind::Assign { dest: discr_p, value: Value::Discriminant(deref_self) }, zero_span());
        b.push(
            StmtKind::Assign {
                dest: ret_p,
                value: Value::Binary { op: BinOp::Eq, lhs: Operand::Copy(discr_p), rhs: Operand::Const(c1) },
            },
            zero_span(),
        );
        b.terminate(StmtKind::Return, zero_span());
        let body = b.finish().unwrap();

        let fn_key = ItemKey(0x1002);
        let facts = TestFacts { adt_graph: None };
        let analysis = analyze_body(fn_key, 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
        assert_eq!(analysis.local_use[0].level(), CanonLevel::LayoutOnly);

        let mut ground = PortableTys::new_in(alloc);
        let g_foo = ground.intern(PNode::Adt(FOO, List::EMPTY)).unwrap();
        let g_bar = ground.intern(PNode::Adt(BAR, List::EMPTY)).unwrap();
        let g_ref_foo = ground.intern(PNode::Ref(Mutbl::Not, g_foo)).unwrap();
        let g_ref_bar = ground.intern(PNode::Ref(Mutbl::Not, g_bar)).unwrap();
        let g_u64 = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
        let g_bool = ground.intern(PNode::Bool).unwrap();
        let g_u8 = ground.intern(PNode::Uint(UintTy::U8)).unwrap();
        let g_never = ground.intern(PNode::Never).unwrap();
        let g_unit = ground.intern(PNode::Tuple(List::EMPTY)).unwrap();

        let c_ref_foo = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_ref_foo, LayoutShape::NON_NULL_PTR, false)], alloc).unwrap();
        let c_ref_bar = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_ref_bar, LayoutShape::NON_NULL_PTR, false)], alloc).unwrap();
        let c_u64 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_u64, LayoutShape::INT64, false)], alloc).unwrap();
        let c_bool = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_bool, LayoutShape::with_niche(1, 1, AbiClass::Int8Zext, NicheShape::BOOL), false)], alloc).unwrap();
        let nz_u8_niche = NicheShape { offset: 0, scalar_bytes: 1, valid_start: 1, valid_end: 255 };
        let c_nz_u8 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_u8, LayoutShape::with_niche(1, 1, AbiClass::Int8Zext, nz_u8_niche), false)], alloc).unwrap();
        let c_never = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_never, LayoutShape::NEVER, false)], alloc).unwrap();
        let c_unit = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_unit, LayoutShape::UNIT, false)], alloc).unwrap();

        let sym = |c: &CanonicalArgs<'_>| canonical_fn_symbol(fn_key, c.as_slice(), ground.view(), alloc).unwrap();
        // Positive: Option<&Foo>::is_some == Option<&Bar>::is_some!
        assert_eq!(sym(&c_ref_foo), sym(&c_ref_bar));
        // Negative controls:
        // 1. Niche vs no-niche (`&Foo` vs `u64`): Option<&Foo> is 8B, Option<u64> is 16B!
        assert_ne!(sym(&c_ref_foo), sym(&c_u64));
        // 2. Incompatible niches (`bool` None=2 vs `NonZero<u8>` None=0)!
        assert_ne!(sym(&c_bool), sym(&c_nz_u8));
        // 3. Uninhabited `!` vs inhabited `()`!
        assert_ne!(sym(&c_never), sym(&c_unit));
    }

    /// Positive control 3: `Vec<*const A>::len` == `Vec<*const B>::len` == `Vec<u64>::len`,
    /// AND `Vec<*const Foo>::as_ptr` == `Vec<*const Bar>::as_ptr` == `Vec<u64>::as_ptr`!
    #[test]
    fn positive_vec_len_and_as_ptr_collapse_to_opaque_thin() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let usize_ty = tys.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let ptr_t = tys.intern(PNode::Ptr(Mutbl::Not, t0)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let vec_t = tys.intern(PNode::Adt(VEC, t0_list)).unwrap();
        let ref_vec_t = tys.intern(PNode::Ref(Mutbl::Not, vec_t)).unwrap();

        // fn Vec::len<T>(self: &Vec<T>) -> usize { (*self).1 }
        let mut b_len = BodyBuilder::new_with_types_in(tys.view(), alloc);
        let ret = b_len.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let self_arg = b_len.new_local(LocalDecl { ty: ref_vec_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let self_p = b_len.local_place(self_arg);
        let deref_self = b_len.project(self_p, Proj::Deref);
        let len_field = b_len.project(deref_self, Proj::Field { index: 1, ty: usize_ty });
        let ret_p = b_len.local_place(ret);
        let bb0 = b_len.new_block(BlockKind::Normal);
        b_len.start(bb0);
        b_len.push(StmtKind::Assign { dest: ret_p, value: Value::Use(Operand::Copy(len_field)) }, zero_span());
        b_len.terminate(StmtKind::Return, zero_span());
        let body_len = b_len.finish().unwrap();

        // fn Vec::as_ptr<T>(self: &Vec<T>) -> *const T { (*self).0 }
        let mut b_ptr = BodyBuilder::new_with_types_in(tys.view(), alloc);
        let ret2 = b_ptr.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let self_arg2 = b_ptr.new_local(LocalDecl { ty: ref_vec_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let self_p2 = b_ptr.local_place(self_arg2);
        let deref_self2 = b_ptr.project(self_p2, Proj::Deref);
        let ptr_field = b_ptr.project(deref_self2, Proj::Field { index: 0, ty: ptr_t });
        let ret_p2 = b_ptr.local_place(ret2);
        let bb0_2 = b_ptr.new_block(BlockKind::Normal);
        b_ptr.start(bb0_2);
        b_ptr.push(StmtKind::Assign { dest: ret_p2, value: Value::Use(Operand::Copy(ptr_field)) }, zero_span());
        b_ptr.terminate(StmtKind::Return, zero_span());
        let body_ptr = b_ptr.finish().unwrap();

        let fn_len = ItemKey(0x1003);
        let fn_as_ptr = ItemKey(0x1004);
        let facts = TestFacts { adt_graph: None };

        let a_len = analyze_body(fn_len, 1, Abi::Rust, body_len.view(), tys.view(), &facts, alloc).unwrap();
        let a_ptr = analyze_body(fn_as_ptr, 1, Abi::Rust, body_ptr.view(), tys.view(), &facts, alloc).unwrap();
        assert_eq!(a_len.local_use[0].level(), CanonLevel::OpaquePtr);
        assert_eq!(a_ptr.local_use[0].level(), CanonLevel::OpaquePtr);

        let mut ground = PortableTys::new_in(alloc);
        let g_foo = ground.intern(PNode::Adt(FOO, List::EMPTY)).unwrap();
        let g_bar = ground.intern(PNode::Adt(BAR, List::EMPTY)).unwrap();
        let g_ptr_foo = ground.intern(PNode::Ptr(Mutbl::Not, g_foo)).unwrap();
        let g_ptr_bar = ground.intern(PNode::Ptr(Mutbl::Not, g_bar)).unwrap();
        let g_u64 = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
        let g_str = ground.intern(PNode::Str).unwrap();

        let args_ptr_foo = [ArgInfo::sized(g_ptr_foo, LayoutShape::INT64, false)];
        let args_ptr_bar = [ArgInfo::sized(g_ptr_bar, LayoutShape::INT64, false)];
        let args_u64 = [ArgInfo::sized(g_u64, LayoutShape::INT64, false)];
        let args_str = [ArgInfo::unsized_ty(g_str, false)];

        for (fn_k, usage) in [(fn_len, &a_len.local_use), (fn_as_ptr, &a_ptr.local_use)] {
            let c1 = canonicalize_args(usage, &args_ptr_foo, alloc).unwrap();
            let c2 = canonicalize_args(usage, &args_ptr_bar, alloc).unwrap();
            let c3 = canonicalize_args(usage, &args_u64, alloc).unwrap();
            let c_unsized = canonicalize_args(usage, &args_str, alloc).unwrap();

            assert_eq!(c1.as_slice(), &[CanonTy::OpaqueThin]);
            assert_eq!(c2.as_slice(), &[CanonTy::OpaqueThin]);
            assert_eq!(c3.as_slice(), &[CanonTy::OpaqueThin]);
            // Negative control: unsized `str` MUST NOT become `OpaqueThin`!
            assert_eq!(c_unsized.as_slice(), &[CanonTy::Concrete(g_str)]);

            let s1 = canonical_fn_symbol(fn_k, c1.as_slice(), ground.view(), alloc).unwrap();
            let s2 = canonical_fn_symbol(fn_k, c2.as_slice(), ground.view(), alloc).unwrap();
            let s3 = canonical_fn_symbol(fn_k, c3.as_slice(), ground.view(), alloc).unwrap();
            let s_unsized = canonical_fn_symbol(fn_k, c_unsized.as_slice(), ground.view(), alloc).unwrap();
            assert_eq!(s1, s2);
            assert_eq!(s1, s3);
            assert_ne!(s1, s_unsized);
        }
    }

    /// Positive control 4: `swap::<u64>` == `swap::<*const u8>` (same `(8, 8, Int64)`, no Drop),
    /// AND negative controls:
    /// - `swap::<T: Drop>` MUST NOT collapse with `u64`!
    /// - `swap::<f64>` (float register ABI) MUST NOT collapse with `swap::<u64>` (GPR ABI)!
    /// - `swap::<i8>` (Sext) MUST NOT collapse with `swap::<u8>` (Zext)!
    /// - `swap::<NonTransparentStruct(u64)>` (Indirect) MUST NOT collapse with `swap::<u64>` (Int64)!
    #[test]
    fn positive_swap_u64_ptr_and_negative_drop_and_abi_controls() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let unit = tys.intern(PNode::Tuple(List::EMPTY)).unwrap();
        let mut_ref_t = tys.intern(PNode::Ref(Mutbl::Mut, t0)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();

        // fn swap<T>(x: &mut T, y: &mut T) { let tmp = read_via_copy::<T>(x); write_via_move::<T>(x, read_via_copy::<T>(y)); write_via_move::<T>(y, tmp); }
        let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
        let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let x = b.new_local(LocalDecl { ty: mut_ref_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let y = b.new_local(LocalDecl { ty: mut_ref_t, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
        let tmp = b.new_local(LocalDecl { ty: t0, kind: LocalKind::Temp(ExprId(0), blaze_body::Role::Value), mutbl: Mutbl::Mut });

        let read_c = b.callee(Callee::Intrinsic { key: INTRINSIC_READ_VIA_COPY, args: t0_list });
        let write_c = b.callee(Callee::Intrinsic { key: INTRINSIC_WRITE_VIA_MOVE, args: t0_list });
        let xp = b.local_place(x);
        let yp = b.local_place(y);
        let tmpp = b.local_place(tmp);
        let read_ops = b.operands(&[Operand::Copy(xp)]);
        let write_ops = b.operands(&[Operand::Copy(yp), Operand::Move(tmpp)]);

        let bb0 = b.new_block(BlockKind::Normal);
        let bb1 = b.new_block(BlockKind::Normal);
        let bb2 = b.new_block(BlockKind::Normal);
        b.start(bb0);
        b.terminate(
            StmtKind::Call { callee: read_c, args: read_ops, dest: Some(tmpp), target: Some(bb1), unwind: Unwind::Unreachable },
            zero_span(),
        );
        b.start(bb1);
        b.terminate(
            StmtKind::Call { callee: write_c, args: write_ops, dest: None, target: Some(bb2), unwind: Unwind::Unreachable },
            zero_span(),
        );
        b.start(bb2);
        b.terminate(StmtKind::Return, zero_span());
        let body = b.finish().unwrap();

        let fn_swap = ItemKey(0x1005);
        let facts = TestFacts { adt_graph: None };
        let analysis = analyze_body(fn_swap, 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
        assert_eq!(analysis.local_use[0].level(), CanonLevel::LayoutOnly);

        let mut ground = PortableTys::new_in(alloc);
        let g_u64 = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
        let g_u8 = ground.intern(PNode::Uint(UintTy::U8)).unwrap();
        let g_i8 = ground.intern(PNode::Int(IntTy::I8)).unwrap();
        let g_ptr_u8 = ground.intern(PNode::Ptr(Mutbl::Not, g_u8)).unwrap();
        let g_f64 = ground.intern(PNode::Float(FloatTy::F64)).unwrap();
        let g_drop_u64 = ground.intern(PNode::Adt(STRING_ADT, List::EMPTY)).unwrap();
        let g_struct_u64 = ground.intern(PNode::Adt(FOO, List::EMPTY)).unwrap();

        let c_u64 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_u64, LayoutShape::INT64, false)], alloc).unwrap();
        let c_ptr = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_ptr_u8, LayoutShape::INT64, false)], alloc).unwrap();
        let c_f64 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_f64, LayoutShape::FLOAT64, false)], alloc).unwrap();
        let c_drop = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_drop_u64, LayoutShape::INT64, true)], alloc).unwrap();
        let c_i8 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_i8, LayoutShape::plain(1, 1, AbiClass::Int8Sext), false)], alloc).unwrap();
        let c_u8 = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_u8, LayoutShape::plain(1, 1, AbiClass::Int8Zext), false)], alloc).unwrap();
        let c_struct = canonicalize_args(&analysis.local_use, &[ArgInfo::sized(g_struct_u64, LayoutShape::plain(8, 8, AbiClass::Indirect), false)], alloc).unwrap();

        let sym = |c: &CanonicalArgs<'_>| canonical_fn_symbol(fn_swap, c.as_slice(), ground.view(), alloc).unwrap();

        // Positive: swap::<u64> == swap::<*const u8>!
        assert_eq!(c_u64.as_slice(), &[CanonTy::Layout(LayoutShape::INT64)]);
        assert_eq!(c_ptr.as_slice(), &[CanonTy::Layout(LayoutShape::INT64)]);
        assert_eq!(sym(&c_u64), sym(&c_ptr));

        // Negative control 1: T: Drop MUST stay Concrete and NOT collapse with non-Drop u64!
        assert_eq!(c_drop.as_slice(), &[CanonTy::Concrete(g_drop_u64)]);
        assert_ne!(sym(&c_drop), sym(&c_u64));

        // Negative control 2: f64 (FPR ABI) MUST NOT collapse with u64 (GPR ABI)!
        assert_eq!(c_f64.as_slice(), &[CanonTy::Layout(LayoutShape::FLOAT64)]);
        assert_ne!(sym(&c_f64), sym(&c_u64));

        // Negative control 3: i8 (Sext) MUST NOT collapse with u8 (Zext)!
        assert_ne!(sym(&c_i8), sym(&c_u8));

        // Negative control 4: ordinary struct(u64) (Indirect) MUST NOT collapse with u64 (Int64)!
        assert_ne!(sym(&c_struct), sym(&c_u64));
    }

    /// Negative controls: `TypeId::of::<T>()`, `type_name::<T>()`, inline `const { ... }` block,
    /// `T: Display` trait dispatch, `&T as &dyn Tr` unsizing, `extern "C"` ABI, and `asm!`
    /// ALL MUST force `CanonLevel::Concrete` and NEVER collapse!
    #[test]
    fn negative_soundness_controls_force_concrete() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let unit = tys.intern(PNode::Tuple(List::EMPTY)).unwrap();
        let u128_ty = tys.intern(PNode::Uint(UintTy::U128)).unwrap();
        let ref_t = tys.intern(PNode::Ref(Mutbl::Not, t0)).unwrap();
        let str_ty = tys.intern(PNode::Str).unwrap();
        let ref_str = tys.intern(PNode::Ref(Mutbl::Not, str_ty)).unwrap();
        let dyn_pred = tys.intern(PNode::ExTrait(TRAIT_DISPLAY_FMT, List::EMPTY)).unwrap();
        let dyn_list = tys.intern_list(&[dyn_pred]).unwrap();
        let dyn_ty = tys.intern(PNode::Dynamic(dyn_list)).unwrap();
        let ref_dyn = tys.intern(PNode::Ref(Mutbl::Not, dyn_ty)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();

        let facts = TestFacts { adt_graph: None };

        // 1. Direct `intrinsics::type_id::<T>()`
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: u128_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let c = b.callee(Callee::Intrinsic { key: INTRINSIC_TYPE_ID, args: t0_list });
            let ops = b.operands(&[]);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x2001), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
            assert!(a.local_use[0].contains(ParamUse::REFLECTION));
        }

        // 2. Direct `intrinsics::type_name::<T>()`
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: ref_str, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let c = b.callee(Callee::Intrinsic { key: INTRINSIC_TYPE_NAME, args: t0_list });
            let ops = b.operands(&[]);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x2002), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
            assert!(a.local_use[0].contains(ParamUse::REFLECTION));
        }

        // 3. Inline `const { type_id::<T>() }` (ConstKind::Anon whose type does NOT mention T!)
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: u128_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let anon_c = b.new_const(ConstDecl { kind: ConstKind::Anon(ExprId(7)), ty: u128_ty });
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.push(StmtKind::Assign { dest: ret_p, value: Value::Use(Operand::Const(anon_c)) }, zero_span());
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x2003), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
        }

        // 4. `T: Display` trait method dispatch (`Callee::Method`)
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let x = b.new_local(LocalDecl { ty: ref_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let c = b.callee(Callee::Method { key: TRAIT_DISPLAY_FMT, args: t0_list });
            let xp = b.local_place(x);
            let ops = b.operands(&[Operand::Copy(xp)]);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: None, target: Some(bb1), unwind: Unwind::Continue }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x2004), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
            assert!(a.local_use[0].contains(ParamUse::TRAIT_DISPATCH));
        }

        // 5. Unsizing coercion `&T as &dyn Display` (builds concrete vtable!)
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: ref_dyn, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let x = b.new_local(LocalDecl { ty: ref_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let xp = b.local_place(x);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.push(
                StmtKind::Assign { dest: ret_p, value: Value::Cast { kind: CastKind::Unsize, operand: Operand::Copy(xp), ty: ref_dyn } },
                zero_span(),
            );
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x2005), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
            assert!(a.local_use[0].contains(ParamUse::UNSIZED_META));
        }

        // 6. Non-Rust ABI (`extern "C" fn f<T>(x: T) -> T { x }`) -> Concrete (HFA safety!)
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: t0, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let x = b.new_local(LocalDecl { ty: t0, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let xp = b.local_place(x);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.push(StmtKind::Assign { dest: ret_p, value: Value::Use(Operand::Move(xp)) }, zero_span());
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x2006), 1, Abi::C, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
            assert!(a.local_use[0].contains(ParamUse::NON_RUST_ABI));
        }
    }

    /// Transitive callee propagation tests:
    /// 1. Mutually recursive `OPAQUE_PTR` functions stay `OPAQUE_PTR`!
    /// 2. Caller of a function that transitively calls `TypeId::of::<T>()` becomes `Concrete`!
    /// 3. Caller dropping `LogOnDrop<T>` (whose `Drop::drop` dispatches `Display`) becomes `Concrete`!
    /// 4. Caller of an unanalyzed external callee becomes `Concrete`!
    #[test]
    fn transitive_call_graph_propagation_and_drop_edge_soundness() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let ptr_t = tys.intern(PNode::Ptr(Mutbl::Not, t0)).unwrap();
        let ref_t = tys.intern(PNode::Ref(Mutbl::Not, t0)).unwrap();
        let unit = tys.intern(PNode::Tuple(List::EMPTY)).unwrap();
        let u128_ty = tys.intern(PNode::Uint(UintTy::U128)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let log_drop_t = tys.intern(PNode::Adt(LOG_ON_DROP, t0_list)).unwrap();
        let ref_log_drop_t = tys.intern(PNode::Ref(Mutbl::Mut, log_drop_t)).unwrap();

        let fn_rec_a = ItemKey(0x3001);
        let fn_rec_b = ItemKey(0x3002);
        let fn_caller_id = ItemKey(0x3003);
        let fn_callee_id = ItemKey(0x3004);
        let fn_drop_caller = ItemKey(0x3005);
        let fn_ext_caller = ItemKey(0x3006);
        let fn_ext_unknown = ItemKey(0x3099);

        let facts = TestFacts { adt_graph: None };

        // Helper: build `fn f<T>(p: *const T) -> *const T { g::<T>(p) }`
        let make_ptr_forward = |callee: ItemKey| {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let arg = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let c = b.callee(Callee::FnItem { key: callee, args: t0_list });
            let arg_p = b.local_place(arg);
            let ret_p = b.local_place(ret);
            let ops = b.operands(&[Operand::Copy(arg_p)]);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        let body_rec_a = make_ptr_forward(fn_rec_b);
        let body_rec_b = make_ptr_forward(fn_rec_a);
        let body_ext_caller = make_ptr_forward(fn_ext_unknown);

        // fn caller_id<T>(p: *const T) -> u128 { callee_id::<T>() }
        let body_caller_id = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: u128_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let _arg = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let c = b.callee(Callee::FnItem { key: fn_callee_id, args: t0_list });
            let ops = b.operands(&[]);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        // fn callee_id<T>() -> u128 { type_id::<T>() }
        let body_callee_id = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: u128_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let c = b.callee(Callee::Intrinsic { key: INTRINSIC_TYPE_ID, args: t0_list });
            let ops = b.operands(&[]);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        // fn drop_caller<T>(x: LogOnDrop<T>) { drop(x); }
        let body_drop_caller = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let x = b.new_local(LocalDecl { ty: log_drop_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let xp = b.local_place(x);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Drop { place: xp, target: bb1, unwind: Unwind::Continue }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        // impl<T: Display> Drop for LogOnDrop<T> { fn drop(&mut self) { Display::fmt(&self.0); } }
        let body_log_drop = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let _s = b.new_local(LocalDecl { ty: ref_log_drop_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let _r = b.new_local(LocalDecl { ty: ref_t, kind: LocalKind::Temp(ExprId(0), blaze_body::Role::Value), mutbl: Mutbl::Not });
            let c = b.callee(Callee::Method { key: TRAIT_DISPLAY_FMT, args: t0_list });
            let ops = b.operands(&[]);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: None, target: Some(bb1), unwind: Unwind::Continue }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        let analyses = [
            analyze_body(fn_rec_a, 1, Abi::Rust, body_rec_a.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(fn_rec_b, 1, Abi::Rust, body_rec_b.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(fn_caller_id, 1, Abi::Rust, body_caller_id.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(fn_callee_id, 1, Abi::Rust, body_callee_id.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(fn_drop_caller, 1, Abi::Rust, body_drop_caller.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(FN_LOG_DROP, 1, Abi::Rust, body_log_drop.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(fn_ext_caller, 1, Abi::Rust, body_ext_caller.view(), tys.view(), &facts, alloc).unwrap(),
        ];

        // Before propagation, `fn_caller_id` and `fn_drop_caller` look non-concrete locally:
        assert_eq!(analyses[2].local_use[0].level(), CanonLevel::OpaquePtr);
        assert_eq!(analyses[4].local_use[0].level(), CanonLevel::LayoutOnly);

        let prop = propagate_call_graph(&analyses, &|_| tys.view(), &facts, alloc).unwrap();

        // 1. Mutually recursive OpaquePtr stays OpaquePtr!
        assert_eq!(prop.get(fn_rec_a).unwrap()[0].level(), CanonLevel::OpaquePtr);
        assert_eq!(prop.get(fn_rec_b).unwrap()[0].level(), CanonLevel::OpaquePtr);
        // 2. Transitive TypeId taints caller to Concrete!
        assert_eq!(prop.get(fn_caller_id).unwrap()[0].level(), CanonLevel::Concrete);
        assert!(prop.get(fn_caller_id).unwrap()[0].contains(ParamUse::REFLECTION));
        // 3. Dropping LogOnDrop<T> follows Drop::drop edge and taints caller to Concrete!
        assert_eq!(prop.get(fn_drop_caller).unwrap()[0].level(), CanonLevel::Concrete);
        assert!(prop.get(fn_drop_caller).unwrap()[0].contains(ParamUse::TRAIT_DISPATCH));
        // 4. Unanalyzed external callee taints caller to Concrete!
        assert_eq!(prop.get(fn_ext_caller).unwrap()[0].level(), CanonLevel::Concrete);
        assert!(prop.get(fn_ext_caller).unwrap()[0].contains(ParamUse::UNANALYZED_CALLEE));
    }

    /// Determinism & idempotency tests:
    /// 1. `canonicalize(canonicalize(x)) == canonicalize(x)` for all 4 lattice levels & concrete args.
    /// 2. Two `PortableTys` tables with completely different `PTy(u32)` indices produce byte-identical
    ///    canonical encodings and identical canonical `Symbol`s (Rule 25).
    /// 3. `LayoutShape::from_layout` integrates natively with `blaze_build::layout::Layouts`!
    #[test]
    fn determinism_idempotency_and_native_blaze_layout_integration() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        // Build two ground tables with different insertion order (different PTy ids).
        let mut g1 = PortableTys::new_in(alloc);
        let u64_1 = g1.intern(PNode::Uint(UintTy::U64)).unwrap();
        let foo_1 = g1.intern(PNode::Adt(FOO, List::EMPTY)).unwrap();
        let pair_1_list = g1.intern_list(&[foo_1, u64_1]).unwrap();
        let pair_1 = g1.intern(PNode::Tuple(pair_1_list)).unwrap();

        let mut g2 = PortableTys::new_in(alloc);
        // Insert unrelated noise types first so every PTy index differs!
        let _ = g2.intern(PNode::Bool).unwrap();
        let _ = g2.intern(PNode::Char).unwrap();
        let _ = g2.intern(PNode::Float(FloatTy::F32)).unwrap();
        let _ = g2.intern(PNode::Str).unwrap();
        let foo_2 = g2.intern(PNode::Adt(FOO, List::EMPTY)).unwrap();
        let u64_2 = g2.intern(PNode::Uint(UintTy::U64)).unwrap();
        let pair_2_list = g2.intern_list(&[foo_2, u64_2]).unwrap();
        let pair_2 = g2.intern(PNode::Tuple(pair_2_list)).unwrap();
        assert_ne!(pair_1, pair_2);

        let usage = [ParamUse::NONE, ParamUse::OPAQUE_PTR, ParamUse::BY_VALUE, ParamUse::TRAIT_DISPATCH];
        let args_1 = [
            ArgInfo::sized(u64_1, LayoutShape::INT64, false),
            ArgInfo::sized(foo_1, LayoutShape::plain(16, 8, AbiClass::Indirect), false),
            ArgInfo::sized(u64_1, LayoutShape::INT64, false),
            ArgInfo::sized(pair_1, LayoutShape::plain(24, 8, AbiClass::Indirect), false),
        ];
        let args_2 = [
            ArgInfo::sized(u64_2, LayoutShape::INT64, false),
            ArgInfo::sized(foo_2, LayoutShape::plain(16, 8, AbiClass::Indirect), false),
            ArgInfo::sized(u64_2, LayoutShape::INT64, false),
            ArgInfo::sized(pair_2, LayoutShape::plain(24, 8, AbiClass::Indirect), false),
        ];

        let fn_key = ItemKey(0x4001);
        let c1 = canonicalize_args(&usage, &args_1, alloc).unwrap();
        let c2 = canonicalize_args(&usage, &args_2, alloc).unwrap();

        // 1. Determinism across distinct PortableTys tables (Rule 25).
        let mut bytes1: AVec<'_, u8> = Vec::new_in(alloc);
        let mut bytes2: AVec<'_, u8> = Vec::new_in(alloc);
        encode_canonical_instance(fn_key, c1.as_slice(), g1.view(), alloc, &mut bytes1).unwrap();
        encode_canonical_instance(fn_key, c2.as_slice(), g2.view(), alloc, &mut bytes2).unwrap();
        assert_eq!(bytes1.as_slice(), bytes2.as_slice());
        assert_eq!(canonical_symbol_of_bytes(&bytes1), canonical_symbol_of_bytes(&bytes2));

        // 2. Idempotency: canonicalize_canon(u, canonicalize_args(u, args)) == canonicalize_args(u, args)
        let c1_again = canonicalize_canon(&usage, c1.as_slice(), alloc).unwrap();
        assert_eq!(c1.as_slice(), c1_again.as_slice());

        // AND idempotency via ArgInfo::from_canon round-trip!
        let lifted: [ArgInfo; 4] = core::array::from_fn(|i| ArgInfo::from_canon(c1.as_slice()[i], u64_1));
        let c1_lifted = canonicalize_args(&usage, &lifted, alloc).unwrap();
        assert_eq!(c1.as_slice(), c1_lifted.as_slice());

        // 3. Native integration with `blaze_build::layout::Layouts::of` + `canonical_representative_pty`!
        struct MiniLayoutFacts;
        impl LayoutFacts for MiniLayoutFacts {
            fn adt(&self, _key: ItemKey) -> Option<Adt<'_>> {
                None
            }
            fn repr(&self, key: ItemKey) -> Result<ReprAttrs, Missing> {
                Err(Missing { item: key })
            }
            fn discriminant(&self, key: ItemKey, _variant: u32) -> Result<Option<i128>, Missing> {
                Err(Missing { item: key })
            }
            fn valid_range(&self, key: ItemKey) -> Result<ValidRange, Missing> {
                Err(Missing { item: key })
            }
        }
        let mut layouts = Layouts::new_in(alloc);
        let mut g3 = PortableTys::new_in(alloc);
        let l_u64 = g3.intern(PNode::Uint(UintTy::U64)).unwrap();
        let l_f64 = g3.intern(PNode::Float(FloatTy::F64)).unwrap();
        let l_ptr = g3.intern(PNode::Ptr(Mutbl::Not, l_u64)).unwrap();
        let l_ref = g3.intern(PNode::Ref(Mutbl::Not, l_u64)).unwrap();
        let l_str = g3.intern(PNode::Str).unwrap();
        let l_ref_str = g3.intern(PNode::Ref(Mutbl::Not, l_str)).unwrap();

        let lf = MiniLayoutFacts;
        let shape_of = |g: &mut PortableTys<'_>, ls: &mut Layouts<Alloc<'_>>, ty: PTy| {
            let mut cx = LayoutCx { ground: g, facts: &lf, scratch: alloc };
            let id = ls.of(ty, &mut cx).unwrap();
            LayoutShape::from_layout(ls.get(id), cx.ground.view())
        };

        let sh_u64 = shape_of(&mut g3, &mut layouts, l_u64).unwrap();
        let sh_ptr = shape_of(&mut g3, &mut layouts, l_ptr).unwrap();
        let sh_f64 = shape_of(&mut g3, &mut layouts, l_f64).unwrap();
        let sh_ref = shape_of(&mut g3, &mut layouts, l_ref).unwrap();
        let sh_ref_str = shape_of(&mut g3, &mut layouts, l_ref_str).unwrap();

        assert_eq!(sh_u64, LayoutShape::INT64);
        assert_eq!(sh_ptr, LayoutShape::INT64);
        assert_eq!(sh_f64, LayoutShape::FLOAT64);
        assert_eq!(sh_ref, LayoutShape::NON_NULL_PTR);
        assert_eq!(sh_ref_str.abi, AbiClass::WideSlice);
        assert_eq!(sh_ref_str.niche, Some(NicheShape::NON_NULL_PTR));

        // Representative PTy round-trips back to the exact same LayoutShape!
        for sh in [sh_u64, sh_f64, sh_ref, sh_ref_str, LayoutShape::plain(24, 8, AbiClass::Indirect)] {
            let rep = canonical_representative_pty(CanonTy::Layout(sh), &mut g3).unwrap().unwrap();
            let rep_sh = shape_of(&mut g3, &mut layouts, rep).unwrap();
            assert_eq!(rep_sh, sh, "canonical representative PTy must preserve LayoutShape identically");
        }
    }

    /// Wave 26 realistic deduplication measurement: simulates a typical generic std/collection
    /// workload (`Vec::len`, `Vec::as_ptr`, `Option::is_some`, `swap`, `slice::len`,
    /// `PhantomData` marker, plus concrete `Display::fmt` & `Drop` types) across 50 distinct
    /// user structs/pointers and measures exact deduplication ratio via `CanonTable`.
    #[test]
    fn wave26_dedup_table_collapses_monomorphization_blowup() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut ground = PortableTys::new_in(alloc);
        let mut table = CanonTable::new_in(alloc);

        let fn_vec_len = ItemKey(0x5001);
        let fn_vec_as_ptr = ItemKey(0x5002);
        let fn_opt_is_some = ItemKey(0x5003);
        let fn_swap = ItemKey(0x5004);
        let fn_slice_len = ItemKey(0x5005);
        let fn_marker = ItemKey(0x5006);
        let fn_display = ItemKey(0x5007);

        let n_types: u128 = 50;
        for i in 0..n_types {
            let adt = ground.intern(PNode::Adt(ItemKey(0x9000 + i), List::EMPTY)).unwrap();
            let ptr = ground.intern(PNode::Ptr(Mutbl::Not, adt)).unwrap();
            let rf = ground.intern(PNode::Ref(Mutbl::Not, adt)).unwrap();
            let info_ptr = ArgInfo::sized(ptr, LayoutShape::INT64, false);
            let info_ref = ArgInfo::sized(rf, LayoutShape::NON_NULL_PTR, false);
            let info_val = ArgInfo::sized(adt, LayoutShape::plain(24, 8, AbiClass::Indirect), false);

            // 50 instantiations each of 6 polymorphizable functions + 1 concrete trait-dispatched fn:
            table.intern(fn_vec_len, &[ParamUse::OPAQUE_PTR], &[info_val], ground.view(), alloc).unwrap();
            table.intern(fn_vec_as_ptr, &[ParamUse::OPAQUE_PTR], &[info_ptr], ground.view(), alloc).unwrap();
            table.intern(fn_opt_is_some, &[ParamUse::BY_VALUE], &[info_ref], ground.view(), alloc).unwrap();
            table.intern(fn_swap, &[ParamUse::BY_VALUE], &[info_ptr], ground.view(), alloc).unwrap();
            table.intern(fn_slice_len, &[ParamUse::OPAQUE_PTR], &[info_val], ground.view(), alloc).unwrap();
            table.intern(fn_marker, &[ParamUse::PHANTOM], &[info_val], ground.view(), alloc).unwrap();
            table.intern(fn_display, &[ParamUse::TRAIT_DISPATCH], &[info_val], ground.view(), alloc).unwrap();
        }

        let st = table.stats();
        assert_eq!(st.total_instances, 350);
        // 6 canonicalized functions collapse to 1 each (6 total) + 50 concrete Display::fmt = 56!
        assert_eq!(st.unique_canonical, 56);
        assert_eq!(st.deduplicated_instances, 294);
        assert!(st.dedup_ratio_x100() >= 600, "expected >= 6.0x dedup ratio, got {}", st.dedup_ratio_x100());
    }

    /// Soundness test for `slice::len` (`OPAQUE_PTR`) vs `slice::index` (`LAYOUT_ONLY`):
    /// `slice::len::<T>` collapses for all sized `T`, while `slice::index::<T>` computes stride
    /// `size_of::<T>()` so `u8` and `u64` MUST NOT collapse!
    #[test]
    fn slice_len_is_opaque_ptr_while_slice_index_is_layout_only() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let usize_ty = tys.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let slice_t = tys.intern(PNode::Slice(t0)).unwrap();
        let ref_slice_t = tys.intern(PNode::Ref(Mutbl::Not, slice_t)).unwrap();
        let ref_t = tys.intern(PNode::Ref(Mutbl::Not, t0)).unwrap();

        // fn slice_len<T>(s: &[T]) -> usize { len(*s) }
        let body_len = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let s = b.new_local(LocalDecl { ty: ref_slice_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let sp = b.local_place(s);
            let deref_s = b.project(sp, Proj::Deref);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.push(StmtKind::Assign { dest: ret_p, value: Value::Len(deref_s) }, zero_span());
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        // fn slice_index<T>(s: &[T], i: usize) -> &T { &(*s)[i] }
        let body_index = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: ref_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let s = b.new_local(LocalDecl { ty: ref_slice_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let idx = b.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
            let sp = b.local_place(s);
            let deref_s = b.project(sp, Proj::Deref);
            let elem_p = b.project(deref_s, Proj::Index(idx));
            let ret_p = b.local_place(ret);
            let orig = b.origin(false);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.borrow(ret_p, blaze_body::BorrowKind::Shared, elem_p, orig, zero_span());
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        let facts = TestFacts { adt_graph: None };
        let a_len = analyze_body(ItemKey(0x6001), 1, Abi::Rust, body_len.view(), tys.view(), &facts, alloc).unwrap();
        let a_idx = analyze_body(ItemKey(0x6002), 1, Abi::Rust, body_index.view(), tys.view(), &facts, alloc).unwrap();

        assert_eq!(a_len.local_use[0].level(), CanonLevel::OpaquePtr);
        assert_eq!(a_idx.local_use[0].level(), CanonLevel::LayoutOnly);
        assert!(a_idx.local_use[0].contains(ParamUse::SIZE_ALIGN));

        let mut ground = PortableTys::new_in(alloc);
        let g_u8 = ground.intern(PNode::Uint(UintTy::U8)).unwrap();
        let g_u64 = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
        let info_u8 = ArgInfo::sized(g_u8, LayoutShape::plain(1, 1, AbiClass::Int8Zext), false);
        let info_u64 = ArgInfo::sized(g_u64, LayoutShape::INT64, false);

        // slice_len::<u8> == slice_len::<u64>!
        let c_len_u8 = canonicalize_args(&a_len.local_use, &[info_u8], alloc).unwrap();
        let c_len_u64 = canonicalize_args(&a_len.local_use, &[info_u64], alloc).unwrap();
        assert_eq!(c_len_u8.as_slice(), c_len_u64.as_slice());

        // slice_index::<u8> != slice_index::<u64> (different stride: 1 vs 8)!
        let c_idx_u8 = canonicalize_args(&a_idx.local_use, &[info_u8], alloc).unwrap();
        let c_idx_u64 = canonicalize_args(&a_idx.local_use, &[info_u64], alloc).unwrap();
        assert_ne!(c_idx_u8.as_slice(), c_idx_u64.as_slice());
    }

    /// Soundness test: passing a `Drop`-implementing ADT (`Vec<T>`) by value to a `LayoutOnly`
    /// callee MUST promote caller's `T` to `Concrete` (because `needs_drop(Vec<T>) == true`, so
    /// the callee cannot canonicalize `Vec<T>`).
    #[test]
    fn passing_drop_adt_to_layout_only_callee_promotes_to_concrete() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let unit = tys.intern(PNode::Tuple(List::EMPTY)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let vec_t = tys.intern(PNode::Adt(VEC, t0_list)).unwrap();
        let vec_t_list = tys.intern_list(&[vec_t]).unwrap();

        let fn_caller = ItemKey(0x7001);
        let fn_drop_u = ItemKey(0x7002);

        // fn drop_u<U>(u: U) { drop(u); } -> LayoutOnly (DROP_GLUE)
        let body_drop_u = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let u = b.new_local(LocalDecl { ty: t0, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let up = b.local_place(u);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Drop { place: up, target: bb1, unwind: Unwind::Continue }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        // fn caller<T>(v: Vec<T>) { drop_u::<Vec<T>>(v); }
        let body_caller = {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let v = b.new_local(LocalDecl { ty: vec_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let vp = b.local_place(v);
            let c = b.callee(Callee::FnItem { key: fn_drop_u, args: vec_t_list });
            let ops = b.operands(&[Operand::Move(vp)]);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: None, target: Some(bb1), unwind: Unwind::Continue }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            b.finish().unwrap()
        };

        let facts = TestFacts { adt_graph: None };
        let analyses = [
            analyze_body(fn_caller, 1, Abi::Rust, body_caller.view(), tys.view(), &facts, alloc).unwrap(),
            analyze_body(fn_drop_u, 1, Abi::Rust, body_drop_u.view(), tys.view(), &facts, alloc).unwrap(),
        ];
        // Locally, `fn_caller` only holds `Vec<T>` (OpaquePtr layout) and passes it to `fn_drop_u`.
        assert_eq!(analyses[0].local_use[0].level(), CanonLevel::OpaquePtr);
        assert_eq!(analyses[1].local_use[0].level(), CanonLevel::LayoutOnly);

        // After propagation: `fn_drop_u` is `LayoutOnly`, and `Vec<T>` has `has_drop_impl = true`,
        // so `fn_caller`'s `T` MUST be promoted to `Concrete`!
        let prop = propagate_call_graph(&analyses, &|_| tys.view(), &facts, alloc).unwrap();
        assert_eq!(prop.get(fn_caller).unwrap()[0].level(), CanonLevel::Concrete);
    }

    /// Tests `PtrMask` (`OpaquePtr`), `SizeOf` (`LayoutOnly`), `NeedsDrop` (`LayoutOnly` +
    /// `DROP_GLUE`), and `ConstParam` (`Concrete`).
    #[test]
    fn intrinsic_and_const_param_classification() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let c1 = tys.intern(PNode::ConstParam(1)).unwrap();
        let usize_ty = tys.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let bool_ty = tys.intern(PNode::Bool).unwrap();
        let ptr_t = tys.intern(PNode::Ptr(Mutbl::Not, t0)).unwrap();
        let arr_t_n = tys.intern(PNode::Array(t0, c1)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let facts = TestFacts { adt_graph: None };

        // 1. `ptr_mask::<T>` -> OpaquePtr
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let p = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let m = b.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
            let c = b.callee(Callee::Intrinsic { key: INTRINSIC_PTR_MASK, args: t0_list });
            let pp = b.local_place(p);
            let mp = b.local_place(m);
            let ret_p = b.local_place(ret);
            let ops = b.operands(&[Operand::Copy(pp), Operand::Copy(mp)]);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x8001), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::OpaquePtr);
        }

        // 2. `size_of::<T>()` -> LayoutOnly (SIZE_ALIGN)
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: usize_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let c = b.callee(Callee::Intrinsic { key: INTRINSIC_SIZE_OF, args: t0_list });
            let ops = b.operands(&[]);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x8002), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::LayoutOnly);
            assert!(a.local_use[0].contains(ParamUse::SIZE_ALIGN));
        }

        // 3. `needs_drop::<T>()` -> LayoutOnly (DROP_GLUE)
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: bool_ty, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let c = b.callee(Callee::Intrinsic { key: INTRINSIC_NEEDS_DROP, args: t0_list });
            let ops = b.operands(&[]);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::Call { callee: c, args: ops, dest: Some(ret_p), target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x8003), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::LayoutOnly);
            assert!(a.local_use[0].contains(ParamUse::DROP_GLUE));
        }

        // 4. `fn arr_id<T, const N: usize>(x: [T; N]) -> [T; N] { x }`: T is LayoutOnly, N is Concrete!
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: arr_t_n, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let x = b.new_local(LocalDecl { ty: arr_t_n, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let xp = b.local_place(x);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.push(StmtKind::Assign { dest: ret_p, value: Value::Use(Operand::Move(xp)) }, zero_span());
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x8004), 2, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::LayoutOnly);
            assert_eq!(a.local_use[1].level(), CanonLevel::Concrete);
            assert!(a.local_use[1].contains(ParamUse::CONST_PARAM));
        }
    }

    /// Soundness test: `BinOp::Offset` on `*const T` depends on `size_of::<T>()`, so it MUST
    /// upgrade `T` from `OpaquePtr` to `LayoutOnly` (`SIZE_ALIGN`): `u8` and `u64` MUST NOT
    /// collapse, while `u64` and `*const u8` DO collapse!
    #[test]
    fn pointer_offset_upgrades_opaque_ptr_to_layout_only() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let isize_ty = tys.intern(PNode::Int(IntTy::Isize)).unwrap();
        let ptr_t = tys.intern(PNode::Ptr(Mutbl::Not, t0)).unwrap();

        let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
        let ret = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Return, mutbl: Mutbl::Mut });
        let p = b.new_local(LocalDecl { ty: ptr_t, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
        let n = b.new_local(LocalDecl { ty: isize_ty, kind: LocalKind::Arg(1), mutbl: Mutbl::Not });
        let pp = b.local_place(p);
        let np = b.local_place(n);
        let ret_p = b.local_place(ret);
        let bb0 = b.new_block(BlockKind::Normal);
        b.start(bb0);
        b.push(
            StmtKind::Assign {
                dest: ret_p,
                value: Value::Binary { op: BinOp::Offset, lhs: Operand::Copy(pp), rhs: Operand::Copy(np) },
            },
            zero_span(),
        );
        b.terminate(StmtKind::Return, zero_span());
        let body = b.finish().unwrap();

        let facts = TestFacts { adt_graph: None };
        let fn_key = ItemKey(0x8101);
        let a = analyze_body(fn_key, 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
        assert_eq!(a.local_use[0].level(), CanonLevel::LayoutOnly);
        assert!(a.local_use[0].contains(ParamUse::SIZE_ALIGN));

        let mut ground = PortableTys::new_in(alloc);
        let g_u8 = ground.intern(PNode::Uint(UintTy::U8)).unwrap();
        let g_u64 = ground.intern(PNode::Uint(UintTy::U64)).unwrap();
        let g_ptr_u8 = ground.intern(PNode::Ptr(Mutbl::Not, g_u8)).unwrap();

        let c_u8 = canonicalize_args(&a.local_use, &[ArgInfo::sized(g_u8, LayoutShape::plain(1, 1, AbiClass::Int8Zext), false)], alloc).unwrap();
        let c_u64 = canonicalize_args(&a.local_use, &[ArgInfo::sized(g_u64, LayoutShape::INT64, false)], alloc).unwrap();
        let c_ptr = canonicalize_args(&a.local_use, &[ArgInfo::sized(g_ptr_u8, LayoutShape::INT64, false)], alloc).unwrap();

        assert_ne!(c_u8.as_slice(), c_u64.as_slice());
        assert_eq!(c_u64.as_slice(), c_ptr.as_slice());
    }

    /// Soundness test: `StmtKind::InlineAsm` and an ADT with an associated-type field
    /// (`WithAssoc<T>`) both conservatively force `CanonLevel::Concrete`!
    #[test]
    fn inline_asm_and_with_assoc_adt_force_concrete() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut tys = PortableTys::new_in(alloc);
        let t0 = tys.intern(PNode::Param(0)).unwrap();
        let unit = tys.intern(PNode::Tuple(List::EMPTY)).unwrap();
        let t0_list = tys.intern_list(&[t0]).unwrap();
        let with_assoc_t = tys.intern(PNode::Adt(WITH_ASSOC, t0_list)).unwrap();
        let ref_with_assoc = tys.intern(PNode::Ref(Mutbl::Not, with_assoc_t)).unwrap();
        let facts = TestFacts { adt_graph: None };

        // 1. Inline asm conservatively marks all params Concrete.
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let _ret = b.new_local(LocalDecl { ty: unit, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let asm_id = b.new_asm(blaze_body::AsmArch::Aarch64, 0, "", &[], &[]);
            let ops = b.operands(&[]);
            let bb0 = b.new_block(BlockKind::Normal);
            let bb1 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.terminate(StmtKind::InlineAsm { asm: asm_id, operands: ops, target: Some(bb1), unwind: Unwind::Unreachable }, zero_span());
            b.start(bb1);
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x8201), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
        }

        // 2. Even behind `&WithAssoc<T>`, an associated-type field may be unsized depending on `T`
        //    -> MUST force `Concrete`!
        {
            let mut b = BodyBuilder::new_with_types_in(tys.view(), alloc);
            let ret = b.new_local(LocalDecl { ty: ref_with_assoc, kind: LocalKind::Return, mutbl: Mutbl::Mut });
            let x = b.new_local(LocalDecl { ty: ref_with_assoc, kind: LocalKind::Arg(0), mutbl: Mutbl::Not });
            let xp = b.local_place(x);
            let ret_p = b.local_place(ret);
            let bb0 = b.new_block(BlockKind::Normal);
            b.start(bb0);
            b.push(StmtKind::Assign { dest: ret_p, value: Value::Use(Operand::Copy(xp)) }, zero_span());
            b.terminate(StmtKind::Return, zero_span());
            let body = b.finish().unwrap();
            let a = analyze_body(ItemKey(0x8202), 1, Abi::Rust, body.view(), tys.view(), &facts, alloc).unwrap();
            assert_eq!(a.local_use[0].level(), CanonLevel::Concrete);
        }
    }

    /// Scalability & determinism test: a doubling DAG `(t, t)` nested 40 times (2^41 - 1 tree
    /// nodes!) encodes linearly in ~41 shared forest nodes and produces byte-identical canonical
    /// keys across two differently-ordered `PortableTys` tables (Rule 25).
    #[test]
    fn doubling_dag_concrete_type_encodes_linearly() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let build_dag = |g: &mut PortableTys<'_>, depth: u32, noise: bool| {
            if noise {
                let _ = g.intern(PNode::Bool).unwrap();
                let _ = g.intern(PNode::Char).unwrap();
                let _ = g.intern(PNode::Str).unwrap();
            }
            let mut t = g.intern(PNode::Uint(UintTy::U8)).unwrap();
            for _ in 0..depth {
                let pair = g.intern_list(&[t, t]).unwrap();
                t = g.intern(PNode::Tuple(pair)).unwrap();
            }
            t
        };

        let mut g1 = PortableTys::new_in(alloc);
        let mut g2 = PortableTys::new_in(alloc);
        let d40_a = build_dag(&mut g1, 40, false);
        let d40_b = build_dag(&mut g2, 40, true);
        assert_ne!(d40_a, d40_b);

        let fn_key = ItemKey(0x8301);
        let mut out_a: AVec<'_, u8> = Vec::new_in(alloc);
        let mut out_b: AVec<'_, u8> = Vec::new_in(alloc);
        encode_canonical_instance(fn_key, &[CanonTy::Concrete(d40_a)], g1.view(), alloc, &mut out_a).unwrap();
        encode_canonical_instance(fn_key, &[CanonTy::Concrete(d40_b)], g2.view(), alloc, &mut out_b).unwrap();

        assert_eq!(out_a.as_slice(), out_b.as_slice());
        assert_eq!(canonical_symbol_of_bytes(&out_a), canonical_symbol_of_bytes(&out_b));
        // 41 nodes * ~13 bytes + small header << 1 KiB (vs 2^41 tree nodes!).
        assert!(out_a.len() < 1024, "expected linear shared-forest encoding, got {} bytes", out_a.len());
    }

    #[test]
    fn canonicalize_instance_to_ground_refs_and_guards() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut g = PortableTys::new_in(alloc);
        let t_u8 = g.intern(PNode::Uint(UintTy::U8)).unwrap();
        let t_u32 = g.intern(PNode::Uint(UintTy::U32)).unwrap();
        let t_u64 = g.intern(PNode::Uint(UintTy::U64)).unwrap();
        let t_usize = g.intern(PNode::Uint(UintTy::Usize)).unwrap();
        let ref_u32 = g.intern(PNode::Ref(Mutbl::Not, t_u32)).unwrap();
        let ref_u64 = g.intern(PNode::Ref(Mutbl::Not, t_u64)).unwrap();
        let const_7 = g.intern(PNode::ConstValue { ty: t_usize, bits: 7 }).unwrap();

        let layout_use = [ParamUse::BY_VALUE];
        let opaque_use = [ParamUse::OPAQUE_PTR];
        let unused_use = [ParamUse::NONE];

        // `&u32` and `&u64` share identical canonical ground representative `&()`.
        let (c1, l1) = canonicalize_instance_to_ground(
            &layout_use,
            &[ArgInfo::sized(ref_u32, LayoutShape::NON_NULL_PTR, false)],
            &mut g,
            alloc,
            alloc,
        )
        .unwrap()
        .expect("&u32 should canonicalize to &()");
        let (c2, l2) = canonicalize_instance_to_ground(
            &layout_use,
            &[ArgInfo::sized(ref_u64, LayoutShape::NON_NULL_PTR, false)],
            &mut g,
            alloc,
            alloc,
        )
        .unwrap()
        .expect("&u64 should canonicalize to &()");
        assert_eq!(c1.as_slice(), c2.as_slice());
        assert_eq!(l1, l2);

        // `u32` and `u64` are already canonical representatives (no change, distinct).
        assert!(
            canonicalize_instance_to_ground(
                &layout_use,
                &[ArgInfo::sized(t_u32, LayoutShape::plain(4, 4, AbiClass::Int32), false)],
                &mut g,
                alloc,
                alloc,
            )
            .unwrap()
            .is_none()
        );
        assert!(
            canonicalize_instance_to_ground(
                &layout_use,
                &[ArgInfo::sized(t_u64, LayoutShape::INT64, false)],
                &mut g,
                alloc,
                alloc,
            )
            .unwrap()
            .is_none()
        );

        // `Drop` type stays concrete.
        assert!(
            canonicalize_instance_to_ground(
                &layout_use,
                &[ArgInfo::sized(ref_u32, LayoutShape::NON_NULL_PTR, true)],
                &mut g,
                alloc,
                alloc,
            )
            .unwrap()
            .is_none()
        );

        // Exotic alignment (`align = 32`) stays concrete.
        assert!(
            canonicalize_instance_to_ground(
                &layout_use,
                &[ArgInfo::sized(t_u64, LayoutShape::plain(32, 32, AbiClass::Indirect), false)],
                &mut g,
                alloc,
                alloc,
            )
            .unwrap()
            .is_none()
        );

        // Const generic arg is never replaced with a type node even when `Unused`.
        assert!(
            canonicalize_instance_to_ground(&unused_use, &[ArgInfo::const_arg(const_7)], &mut g, alloc, alloc)
                .unwrap()
                .is_none()
        );

        // `OpaquePtr` maps both `u8` and `u64` to the single `OpaqueThin` foreign marker.
        let (_, op1) = canonicalize_instance_to_ground(
            &opaque_use,
            &[ArgInfo::sized(t_u8, LayoutShape::plain(1, 1, AbiClass::Int8Zext), false)],
            &mut g,
            alloc,
            alloc,
        )
        .unwrap()
        .expect("u8 behind thin ptr canonicalizes to OpaqueThin");
        let (_, op2) = canonicalize_instance_to_ground(
            &opaque_use,
            &[ArgInfo::sized(t_u64, LayoutShape::INT64, false)],
            &mut g,
            alloc,
            alloc,
        )
        .unwrap()
        .expect("u64 behind thin ptr canonicalizes to OpaqueThin");
        assert_eq!(op1, op2);
        let &[rep_ty] = g.view().list(op1).unwrap() else { panic!("single arg") };
        assert_eq!(g.view().node(rep_ty).unwrap(), PNode::Foreign(OPAQUE_THIN_FOREIGN_KEY));
    }

    #[test]
    fn poly_body_text_digest_trivia_free_and_table_independent() {
        let frame = test_frame();
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();

        let mut t1 = PortableTys::new_in(alloc);
        let p0_a = t1.intern(PNode::Param(0)).unwrap();
        let sig_a = blaze_typeck::FnSig {
            types: t1.view(),
            parent_count: 0,
            own_count: 1,
            declared_count: 1,
            self_param: false,
            inputs: &[p0_a],
            output: p0_a,
            header: blaze_typeck::FnPtrHeader::RUST,
            c_variadic: false,
            legacy_const_generics: &[],
        };

        // Second PortableTys with extra noise nodes so Param(0) gets a different PTy index.
        let mut t2 = PortableTys::new_in(alloc);
        let _ = t2.intern(PNode::Bool).unwrap();
        let _ = t2.intern(PNode::Char).unwrap();
        let p0_b = t2.intern(PNode::Param(0)).unwrap();
        let u64_b = t2.intern(PNode::Uint(UintTy::U64)).unwrap();
        assert_ne!(p0_a, p0_b);
        let sig_b = blaze_typeck::FnSig {
            types: t2.view(),
            parent_count: 0,
            own_count: 1,
            declared_count: 1,
            self_param: false,
            inputs: &[p0_b],
            output: p0_b,
            header: blaze_typeck::FnPtrHeader::RUST,
            c_variadic: false,
            legacy_const_generics: &[],
        };
        let sig_diff = blaze_typeck::FnSig { output: u64_b, ..sig_b };

        let d1 = poly_body_text_digest("fn id<T>(x: T) -> T { x }", &sig_a, blaze_build::Abi::Rust, alloc);
        let d2 = poly_body_text_digest("fn id<T>(x: T) -> T {\n    // comment!\n    x\n}", &sig_b, blaze_build::Abi::Rust, alloc);
        let d_body_edit = poly_body_text_digest("fn id<T>(x: T) -> T { let _ = type_name::<T>(); x }", &sig_a, blaze_build::Abi::Rust, alloc);
        let d_sig_edit = poly_body_text_digest("fn id<T>(x: T) -> T { x }", &sig_diff, blaze_build::Abi::Rust, alloc);
        let d_abi_edit = poly_body_text_digest("fn id<T>(x: T) -> T { x }", &sig_a, blaze_build::Abi::C, alloc);

        assert_eq!(d1, d2, "trivia & PTy table order must not change digest");
        assert_ne!(d1, d_body_edit, "body token edit must change digest");
        assert_ne!(d1, d_sig_edit, "signature type edit must change digest");
        assert_ne!(d1, d_abi_edit, "ABI edit must change digest");
    }
}
