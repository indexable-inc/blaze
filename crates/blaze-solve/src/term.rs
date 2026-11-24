//! The solver's vocabulary: terms (types, regions and consts in one arena of 4-byte handles),
//! lists of them, definitions by key, predicates, certainty, and the simplified self shape impls
//! are bucketed by (solve.md, "The seams" and "Goals").
//!
//! `Ty`, `List`, `TyKind`, `Flags` and `Markers` are blaze-ty's; they are defined here until
//! that crate lands, then move there (one concept, one home: rule 30). The same `TyKind` is the
//! node of a portable answer (`facts::Portable`), where its handles index the answer's own
//! tables instead of the arena.

/// A term: a type, a region or a const, as a handle into the `Tys` arena (or, in a portable
/// answer, an index into its nodes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ty(pub u32);

impl Ty {
    /// No term: the sentinel of an optional slot in a column.
    pub const NONE: Ty = Ty(u32::MAX);
}

/// A list of terms. In a portable answer, `start..start + len` is a range of its `lists`. In
/// an arena, `start` is a handle the `Tys` made and only `len` has a meaning to the solver:
/// elements are read with `Tys::list_get`, never by arithmetic on `start` (blaze-ty's lists
/// are interned ids; the tests' arena happens to use ranges).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct List {
    pub start: u32,
    pub len: u32,
}

impl List {
    pub const EMPTY: List = List { start: 0, len: 0 };
}

/// A definition: the first 16 bytes of its `blaze_id::ItemKey` (blaze-typeck's `ItemKey` form):
/// portable, so canonical keys built from it are the same on every machine (rule 25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Def(pub u128);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UintTy {
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FloatTy {
    F16,
    F32,
    F64,
    F128,
}

/// The type of a const generic value: the Reference ("Const generics") allows only integer
/// types, `char` and `bool`. Two values are one term only when type and bits are both equal
/// (`4u8` and `4usize` are different arguments), as rustc's `ty::Value` carries its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValTy {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    /// A fieldless enum without generics, by its definition (`adt_const_params`: the pinned
    /// library's `const ORD: AtomicOrdering`, `AtomicOrdering` deriving `ConstParamTy`): the
    /// value's bits are its variant's index, as `Def::Ctor` counts.
    Enum(Def),
}

impl ValTy {
    /// `usize`, the type of an array length and of a closure's kind term.
    pub const USIZE: ValTy = ValTy::Uint(UintTy::Usize);

    /// One word for canonical forms: 0 `bool`, 1 `char`, 2..=7 the signed integers, 8..=13
    /// the unsigned ones, in declaration order.
    pub const fn code(self) -> u32 {
        match self {
            ValTy::Bool => 0,
            ValTy::Char => 1,
            ValTy::Int(i) => 2 + i as u32,
            ValTy::Uint(u) => 8 + u as u32,
            ValTy::Enum(_) => ValTy::ENUM_CODE,
        }
    }

    /// [`ValTy::code`] of an enum: its definition follows in four more words
    /// ([`Def`]'s bits, low first), which `from_code` does not read.
    pub const ENUM_CODE: u32 = 14;

    /// The type of `code`, `None` past the last.
    pub const fn from_code(code: u32) -> Option<ValTy> {
        const INTS: [IntTy; 6] = [IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128, IntTy::Isize];
        const UINTS: [UintTy; 6] = [UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128, UintTy::Usize];
        match code {
            0 => Some(ValTy::Bool),
            1 => Some(ValTy::Char),
            2..=7 => Some(ValTy::Int(INTS[(code - 2) as usize])),
            8..=13 => Some(ValTy::Uint(UINTS[(code - 8) as usize])),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mutbl {
    Not,
    Mut,
}

/// What an inference variable may become.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VarKind {
    /// Any type.
    Ty,
    /// An integer type (an unsuffixed integer literal).
    Int,
    /// A float type.
    Float,
    /// A region.
    Region,
    /// A const.
    Const,
}

/// An inference variable. `index` is unique across kinds in one arena, so one wake list per
/// index serves every kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Var {
    pub kind: VarKind,
    pub index: u32,
}

/// What an alias names (`rustc_type_ir::AliasTyKind`, and unevaluated consts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AliasKind {
    /// `<args[0] as Trait<args[1..n]>>::A<args[n..]>`; the def is the trait's associated item.
    Projection,
    /// An inherent associated type; the def is the impl's item.
    Inherent,
    /// `impl Trait`; the def is the opaque type.
    Opaque,
    /// A free (lazy) type alias.
    Free,
    /// An unevaluated const: an associated or anonymous const; the def is the const.
    Const,
}

/// Whether an alias is rigid (`rustc_type_ir::IsRigid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rigid {
    No,
    Yes,
}

/// A closure's kind as the term in its args (`ConstValue(0)` is `Fn`, 1 `FnMut`, 2 `FnOnce`),
/// or an inference variable while it is not known.
pub const CLOSURE_FN: u64 = 0;
pub const CLOSURE_FN_MUT: u64 = 1;
pub const CLOSURE_FN_ONCE: u64 = 2;

/// A fn pointer's calling convention. Each is its own type: `extern "C" fn()` and
/// `extern "C-unwind" fn()` relate to nothing but themselves (the reference, "Function pointer
/// types"; every ABI of the pinned nightly's `ExternAbi`, by discriminant: a type of any ABI the
/// nightly knows is carried, and what the builder cannot emit is refused there by name).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Abi {
    Rust = 0,
    C = 1,
    CUnwind = 2,
    System = 3,
    SystemUnwind = 4,
    /// `extern "rust-call"` (unstable `unboxed_closures`): the ABI of `FnOnce::call_once`,
    /// `FnMut::call_mut` and `Fn::call` in the pinned library (library/core/src/ops/function.rs).
    /// At the type level a fn of it takes its last parameter, a tuple, as one value; only the
    /// call's ABI spreads that tuple. Not `Fn*`-compatible (`ExternAbi::is_fn_trait_compatible`).
    RustCall = 5,
    /// `extern "llvm-intrinsic"` (unstable `link_llvm_intrinsics`): rustc_abi's `LlvmIntrinsic`,
    /// "Unstable ABI used to call LLVM intrinsics." The pinned library's `core::arch` declares
    /// every target intrinsic in such a block; a declaration in a body carries it as its fn
    /// header's ABI (blaze-ladder front2/heads.rs, a local foreign fn).
    LlvmIntrinsic = 6,
    Swift = 7,
    Aapcs = 8,
    AapcsUnwind = 9,
    AvrInterrupt = 10,
    AvrNonBlockingInterrupt = 11,
    Cdecl = 12,
    CdeclUnwind = 13,
    CmseNonsecureCall = 14,
    CmseNonsecureEntry = 15,
    Custom = 16,
    EfiApi = 17,
    Fastcall = 18,
    FastcallUnwind = 19,
    GpuKernel = 20,
    Msp430Interrupt = 21,
    PtxKernel = 22,
    RiscvInterruptM = 23,
    RiscvInterruptS = 24,
    RustCold = 25,
    RustInvalid = 26,
    RustPreserveNone = 27,
    Stdcall = 28,
    StdcallUnwind = 29,
    SysV64 = 30,
    SysV64Unwind = 31,
    RustTail = 32,
    Thiscall = 33,
    ThiscallUnwind = 34,
    Vectorcall = 35,
    VectorcallUnwind = 36,
    Win64 = 37,
    Win64Unwind = 38,
    X86Interrupt = 39,
}

impl Abi {
    /// Every ABI of the pinned nightly's `ExternAbi`, by discriminant.
    pub const ALL: [Abi; 40] = [
        Abi::Rust,
        Abi::C,
        Abi::CUnwind,
        Abi::System,
        Abi::SystemUnwind,
        Abi::RustCall,
        Abi::LlvmIntrinsic,
        Abi::Swift,
        Abi::Aapcs,
        Abi::AapcsUnwind,
        Abi::AvrInterrupt,
        Abi::AvrNonBlockingInterrupt,
        Abi::Cdecl,
        Abi::CdeclUnwind,
        Abi::CmseNonsecureCall,
        Abi::CmseNonsecureEntry,
        Abi::Custom,
        Abi::EfiApi,
        Abi::Fastcall,
        Abi::FastcallUnwind,
        Abi::GpuKernel,
        Abi::Msp430Interrupt,
        Abi::PtxKernel,
        Abi::RiscvInterruptM,
        Abi::RiscvInterruptS,
        Abi::RustCold,
        Abi::RustInvalid,
        Abi::RustPreserveNone,
        Abi::Stdcall,
        Abi::StdcallUnwind,
        Abi::SysV64,
        Abi::SysV64Unwind,
        Abi::RustTail,
        Abi::Thiscall,
        Abi::ThiscallUnwind,
        Abi::Vectorcall,
        Abi::VectorcallUnwind,
        Abi::Win64,
        Abi::Win64Unwind,
        Abi::X86Interrupt,
    ];

    /// The string each ABI is written with, by discriminant.
    const NAMES: [&'static str; 40] = [
        "Rust",
        "C",
        "C-unwind",
        "system",
        "system-unwind",
        "rust-call",
        "llvm-intrinsic",
        "Swift",
        "aapcs",
        "aapcs-unwind",
        "avr-interrupt",
        "avr-non-blocking-interrupt",
        "cdecl",
        "cdecl-unwind",
        "cmse-nonsecure-call",
        "cmse-nonsecure-entry",
        "custom",
        "efiapi",
        "fastcall",
        "fastcall-unwind",
        "gpu-kernel",
        "msp430-interrupt",
        "ptx-kernel",
        "riscv-interrupt-m",
        "riscv-interrupt-s",
        "rust-cold",
        "rust-invalid",
        "rust-preserve-none",
        "stdcall",
        "stdcall-unwind",
        "sysv64",
        "sysv64-unwind",
        "tail",
        "thiscall",
        "thiscall-unwind",
        "vectorcall",
        "vectorcall-unwind",
        "win64",
        "win64-unwind",
        "x86-interrupt",
    ];

    /// The string the ABI is written with.
    pub const fn name(self) -> &'static str {
        Abi::NAMES[self as usize]
    }

    /// The ABI written `s`; `None` for a string the nightly does not know (E0703).
    pub fn of_name(s: &str) -> Option<Abi> {
        Abi::NAMES.iter().position(|n| *n == s).map(|i| Abi::ALL[i])
    }

    /// Whether a fn of this ABI may be C-variadic (rustc_abi `supports_c_variadic`, plus
    /// "system" as before).
    pub const fn takes_c_variadic(self) -> bool {
        matches!(
            self,
            Abi::C
                | Abi::CUnwind
                | Abi::System
                | Abi::SystemUnwind
                | Abi::Cdecl
                | Abi::CdeclUnwind
                | Abi::Aapcs
                | Abi::AapcsUnwind
                | Abi::SysV64
                | Abi::SysV64Unwind
                | Abi::Win64
                | Abi::Win64Unwind
                | Abi::EfiApi
        )
    }
}

/// Whether calling through a fn pointer needs `unsafe`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Safety {
    Safe,
    Unsafe,
}

/// A fn pointer's header: its ABI, safety and whether it is C-variadic. Part of the type: two
/// fn pointers relate only when their headers are equal; a safe one coerces to the unsafe one
/// of the same ABI (`UnsafeFnPointer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FnPtrHeader {
    pub abi: Abi,
    pub safety: Safety,
    /// `...` as the last parameter (the Reference, "Function pointer types": "Variadic
    /// parameters can only be specified with extern function types with the "C" or "cdecl"
    /// calling convention"): `unsafe extern "C" fn(A, ...)` and `unsafe extern "C" fn(A)` are
    /// two types; the inputs list only the named parameters.
    pub c_variadic: bool,
}

impl FnPtrHeader {
    /// `fn(..)`: the safe Rust ABI.
    pub const RUST: FnPtrHeader = FnPtrHeader { abi: Abi::Rust, safety: Safety::Safe, c_variadic: false };

    /// One byte: the ABI in the low six bits, C-variadic in bit 6, `unsafe` in bit 7 (a key's
    /// and a canonical form's; blaze-typeck's `FnSig` hash already gives bit 6 that meaning).
    pub const fn bits(self) -> u8 {
        let abi = self.abi as u8;
        let abi = if self.c_variadic { abi | 0x40 } else { abi };
        match self.safety {
            Safety::Safe => abi,
            Safety::Unsafe => abi | 0x80,
        }
    }

    /// The inverse of [`FnPtrHeader::bits`]; `None` for a byte it never writes.
    pub const fn from_bits(b: u8) -> Option<FnPtrHeader> {
        let i = (b & 0x3f) as usize;
        if i >= Abi::ALL.len() {
            return None;
        }
        let abi = Abi::ALL[i];
        let safety = if b & 0x80 != 0 { Safety::Unsafe } else { Safety::Safe };
        Some(FnPtrHeader { abi, safety, c_variadic: b & 0x40 != 0 })
    }
}

/// One level of a term. Children are handles; a closure's args end with its kind (a const),
/// its signature (a `FnPtr`) and its upvars (a `Tuple`), after the parent's generics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TyKind {
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(List),
    /// A structural record: canonical schema hash of sorted field names, and sorted field types.
    Record(u64, List),
    /// A structural tagged union: canonical member types.
    Union(List),
    /// Region, pointee.
    Ref(Ty, Ty, Mutbl),
    RawPtr(Ty, Mutbl),
    /// Element, length (a const).
    Array(Ty, Ty),
    Slice(Ty),
    /// A pattern type `base is PAT` (core's `pattern_type!`; typeck.md "Pattern types"): the
    /// base, then its valid values as `ConstValue` terms in pairs, `start ..= end` each, sorted,
    /// merged, `start > end` meaning a range that wraps. Equal by structure; no relation to the
    /// base but a transmute.
    Pat(Ty, List),
    Adt(Def, List),
    Foreign(Def),
    FnDef(Def, List),
    /// Inputs, then the output last; the header.
    FnPtr(List, FnPtrHeader),
    Closure(Def, List),
    /// An async block's future (the Reference, "Async blocks"): `[output, interior]`, where
    /// `interior` is the tuple of what the future holds (its captures, by capture kind, and
    /// the values live across an `.await`): the constituents of its auto traits. The only
    /// coroutine kind built: async, so immovable (never `Unpin`, `Copy` or `Clone`).
    Coroutine(Def, List),
    /// Existential predicates (`ExTrait` principal first, then `ExProj`s, then `ExAuto`s,
    /// each sorted), then the region bound.
    Dynamic(List, Ty),
    /// `Alias(IsRigid, AliasTy)` of the reference: a rigid alias (one the solver failed to
    /// normalize further, marked by normalization) relates by structure like a type
    /// constructor; relating a non-rigid one emits a `Projection` goal.
    Alias(Rigid, AliasKind, Def, List),
    /// A generic parameter of the item in scope, by index (`Self` is 0 in a trait).
    Param(u32),
    /// A universally bound term: universe, index.
    Placeholder(u32, u32),
    /// A term bound by a binder: De Bruijn index, var.
    Bound(u32, u32),
    /// A variable of a canonical form: its index among the form's variables.
    Canonical(u32),
    Infer(Var),
    Error,
    // Regions.
    ReStatic,
    ReParam(u32),
    ReErased,
    // Consts.
    /// A value of a const generic: its type and its bits, little-endian as consteval writes them.
    ConstValue(ValTy, u128),
    ConstParam(u32),
    // Existential predicates, the elements of `Dynamic`'s list.
    /// `dyn Trait<args>`: the args after `Self`.
    ExTrait(Def, List),
    /// `Trait<.., A = term>`: the associated item, the args after `Self`, the term.
    ExProj(Def, List, Ty),
    /// `+ Send`.
    ExAuto(Def),
}

/// What a term contains, one load per term (blaze-ty's `flags` column).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Flags(pub u16);

impl Flags {
    /// A type, int, float or const inference variable.
    pub const HAS_INFER: u16 = 1 << 0;
    /// A region inference variable.
    pub const HAS_RE_INFER: u16 = 1 << 1;
    /// A generic parameter (type, const or region).
    pub const HAS_PARAM: u16 = 1 << 2;
    pub const HAS_PLACEHOLDER: u16 = 1 << 3;
    /// A projection, inherent, free or const alias.
    pub const HAS_ALIAS: u16 = 1 << 4;
    pub const HAS_OPAQUE: u16 = 1 << 5;
    pub const HAS_BOUND: u16 = 1 << 6;
    pub const HAS_ERROR: u16 = 1 << 7;
    /// A region other than `'static` (a parameter, an erased one, a variable).
    pub const HAS_FREE_REGION: u16 = 1 << 8;
    pub const HAS_CANONICAL: u16 = 1 << 9;
    /// An alias with `Rigid::No` (`HAS_TY_NON_RIGID_ALIAS` and friends in rustc_type_ir
    /// flags.rs): relating the term may emit a `Projection` goal.
    pub const HAS_NON_RIGID_ALIAS: u16 = 1 << 10;

    pub const fn has(self, bits: u16) -> bool {
        self.0 & bits != 0
    }

    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }

    /// No inference variable of any kind: the ground fast path applies.
    pub const fn no_infer(self) -> bool {
        self.0 & (Flags::HAS_INFER | Flags::HAS_RE_INFER) == 0
    }

    /// Nothing that depends on the environment or on inference: no variable, parameter,
    /// placeholder, alias, opaque or bound term. A marker answer for such a term is a fact
    /// of the term (solve.md (c)).
    pub const fn is_global_ground(self) -> bool {
        self.0
            & (Flags::HAS_INFER
                | Flags::HAS_RE_INFER
                | Flags::HAS_PARAM
                | Flags::HAS_PLACEHOLDER
                | Flags::HAS_ALIAS
                | Flags::HAS_OPAQUE
                | Flags::HAS_BOUND
                | Flags::HAS_CANONICAL)
            == 0
    }
}

/// The marker traits whose answers a term keeps in its `markers` byte (solve.md (c)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarkerTrait {
    Sized = 0,
    Copy = 1,
    Send = 2,
    Sync = 3,
}

impl MarkerTrait {
    pub const ALL: [MarkerTrait; 4] = [MarkerTrait::Sized, MarkerTrait::Copy, MarkerTrait::Send, MarkerTrait::Sync];
}

/// Two bits per marker trait: whether the answer is known, and whether it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Markers(pub u8);

impl Markers {
    /// `Some(holds)` when the answer for `m` is known.
    pub const fn get(self, m: MarkerTrait) -> Option<bool> {
        let shift = (m as u8) * 2;
        if self.0 & (1 << shift) == 0 { None } else { Some(self.0 & (2 << shift) != 0) }
    }

    /// The byte with `m`'s answer set to `holds`.
    pub const fn with(self, m: MarkerTrait, holds: bool) -> Markers {
        let shift = (m as u8) * 2;
        let cleared = self.0 & !(3 << shift);
        Markers(cleared | (1 << shift) | if holds { 2 << shift } else { 0 })
    }
}

/// A trait goal's polarity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Polarity {
    Positive,
    Negative,
}

/// A host-effect goal's constness (`~const` is `Maybe`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Constness {
    Const,
    Maybe,
}

/// A predicate: a goal, an obligation's goal or an assumption of the param env. A fixed-size
/// `Copy` row (32 bytes). Its terms are arena handles, or portable indices in an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pred {
    /// `args[0]: trait_<args[1..]>`.
    Trait { trait_: Def, args: List, polarity: Polarity },
    /// `alias == term`, `alias` an `Alias` term (`ClauseKind::Projection`, solved by
    /// project_goals/mod.rs): also the goal relating a non-rigid alias emits, with a fresh
    /// variable as `term` (rustc_type_ir relate/solver_relating.rs, the `IsRigid::No` arms).
    Projection { alias: Ty, term: Ty },
    /// The alias's value is `term`, which is unconstrained when asked
    /// (`PredicateKind::NormalizesTo`, normalizes_to.rs).
    NormalizesTo { alias: Ty, term: Ty },
    /// `ty: 'region`: recorded for the borrow checker.
    TypeOutlives { ty: Ty, region: Ty },
    /// `'a: 'b`: recorded for the borrow checker.
    RegionOutlives { a: Ty, b: Ty },
    WellFormed(Ty),
    ConstEvaluatable(Ty),
    /// `a <: b`; stalls while either side is an unresolved type variable.
    Subtype { a: Ty, b: Ty },
    /// `a` coerces to `b`; stalls as `Subtype`.
    Coerce { a: Ty, b: Ty },
    DynCompatible(Def),
    /// `args[0]: [const] trait_<args[1..]>`.
    HostEffect { trait_: Def, args: List, constness: Constness },
    /// `where const { COND }` (blaze's `blaze_comptime`): the condition `cond`, an anonymous
    /// constant over its item's generics, is `true` at `args`. Decided by the world once every
    /// argument is ground (`Facts::const_holds`); ambiguous while one is an inference variable;
    /// never decided over a generic parameter; never proves two impls disjoint (coherence).
    ConstHolds { cond: Def, args: List },
    /// `for<F in fields(container)> F: trait_<args_after_self...>`: every field type `F` of the
    /// product type `container` (struct ADT, structural record, or tuple) implements
    /// `trait_<args_after_self...>`, or the bound is assumed in the param env.
    ForFields { container: Ty, trait_: Def, args_after_self: List },
    /// The const `ct` has type `ty`.
    ConstArgHasType { ct: Ty, ty: Ty },
    /// Internal to a probe: unify `a` and `b` now (never canonicalized, never a query).
    Eq { a: Ty, b: Ty },
}

/// Why a nested goal was added (`rustc_type_ir::solve::GoalSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GoalSource {
    Misc,
    ImplWhereBound,
    AliasBoundConstCondition,
    InstantiateHigherRanked,
    AliasWellFormed,
    NormalizeGoal,
    TypeRelating,
}

/// How sure an answer is (`rustc_type_ir::solve::Certainty` with `MaybeCause`), as one byte
/// ordered so that `and` is the maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Certainty {
    Yes = 0,
    /// `Maybe(Ambiguity)`.
    Ambiguous = 1,
    /// `Maybe(Overflow { suggest_increasing_limit: false })`: a fixpoint that did not settle.
    Overflow = 2,
    /// `Maybe(Overflow { suggest_increasing_limit: true })`: the depth limit.
    OverflowLimit = 3,
}

impl Certainty {
    /// Both hold (`Certainty::and`): overflow dominates ambiguity (`MaybeCause::and`).
    pub fn and(self, other: Certainty) -> Certainty {
        if self >= other { self } else { other }
    }

    /// Either holds (`MaybeInfo::or`, as `bail_with_ambiguity` folds it): ambiguity dominates
    /// overflow; yes is the identity of the fold's caller.
    pub fn or(self, other: Certainty) -> Certainty {
        match (self, other) {
            (Certainty::Yes, x) | (x, Certainty::Yes) => x,
            (Certainty::Ambiguous, _) | (_, Certainty::Ambiguous) => Certainty::Ambiguous,
            (a, b) => {
                if a <= b {
                    a
                } else {
                    b
                }
            }
        }
    }

    pub fn is_overflow(self) -> bool {
        matches!(self, Certainty::Overflow | Certainty::OverflowLimit)
    }

    pub fn from_byte(b: u8) -> Certainty {
        match b {
            0 => Certainty::Yes,
            1 => Certainty::Ambiguous,
            2 => Certainty::Overflow,
            _ => Certainty::OverflowLimit,
        }
    }
}

/// The typing mode (`rustc_type_ir::TypingMode`). Which one decides what an opaque type is to
/// normalization (alias.rs `normalize_opaque_type`): in [`TypingMode::Analysis`] itself (rigid,
/// only its item bounds known), in [`TypingMode::PostAnalysis`] its hidden type. In every mode
/// an auto trait still sees through it (structural.rs, auto-trait leakage).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TypingMode {
    /// Type checking and analysis of a body. An opaque type outside its defining scope is
    /// rigid (the Reference, "Abstract return types": "a caller of the function is not
    /// permitted to know the concrete type"); the defining body has a variable for its own
    /// (blaze-typeck rules/opaque_def.rs), so the solver never defines one.
    Analysis,
    /// Coherence: the overlap check between impls, with unknowable candidates.
    Coherence,
    /// After analysis (layout, the borrow check's facts, codegen): every opaque type with a
    /// hidden type normalizes to it (`TypingMode::PostAnalysis`, opaque_types.rs).
    PostAnalysis,
}

/// The lang items the solver recognizes (`SolverTraitLangItem`, and the items next to them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {
    Sized,
    MetaSized,
    PointeeSized,
    Copy,
    Clone,
    Send,
    Sync,
    Unpin,
    Fn,
    FnMut,
    FnOnce,
    /// `FnOnce::Output`.
    FnOnceOutput,
    FnPtrTrait,
    Tuple,
    Pointee,
    /// `Pointee::Metadata`.
    Metadata,
    DynMetadata,
    DiscriminantKind,
    /// `DiscriminantKind::Discriminant`.
    Discriminant,
    Destruct,
    Drop,
    Unsize,
    CoerceUnsized,
    DispatchFromDyn,
    PhantomData,
    OwnedBox,
    ManuallyDrop,
    /// The trait `Future` (`future_trait`).
    Future,
    /// `Future::Output` (`future_output`).
    FutureOutput,
    /// The trait `AsyncFn` (`async_fn`; library/core/src/ops/async_function.rs). The six
    /// async-callable items are last, so the earlier items keep their indices.
    AsyncFn,
    /// The trait `AsyncFnMut` (`async_fn_mut`).
    AsyncFnMut,
    /// The trait `AsyncFnOnce` (`async_fn_once`).
    AsyncFnOnce,
    /// `AsyncFnOnce::Output` (`async_fn_once_output`).
    AsyncFnOnceOutput,
    /// `AsyncFnOnce::CallOnceFuture` (`call_once_future`).
    CallOnceFuture,
    /// `AsyncFnMut::CallRefFuture` (`call_ref_future`).
    CallRefFuture,
    /// The trait `From` (`from` / `core::convert::From`), for error-union conversions.
    From,
}

/// The shape of a self type impls are bucketed by (rustc's `SimplifiedType`, blaze-impls'
/// `SelfShape`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    /// A pattern type (`TyKind::Pat`): its impls are its own, not its base's.
    Pat,
    Bool,
    Char,
    Int(IntTy),
    Uint(UintTy),
    Float(FloatTy),
    Str,
    Never,
    Tuple(u32),
    Record(u64, u32),
    Union(u32),
    Ref(Mutbl),
    Ptr(Mutbl),
    Array,
    Slice,
    Adt(Def),
    Foreign(Def),
    FnDef(Def),
    FnPtr(u32),
    Closure(Def),
    Coroutine(Def),
    /// `dyn Trait`: the principal, or 0 for `dyn Send` and the like.
    Dynamic(Def),
    /// An integer variable: the impls for every integer shape.
    AnyInt,
    /// A float variable: the impls for every float shape.
    AnyFloat,
    /// A parameter, placeholder, alias or opaque: only blanket impls can apply by shape.
    Blanket,
}

/// The shape of a resolved term's kind; `None` for an unresolved type variable (whose goal is
/// ambiguous before any impl is read).
pub fn shape_of_kind(k: TyKind) -> Option<Shape> {
    Some(match k {
        TyKind::Bool => Shape::Bool,
        TyKind::Char => Shape::Char,
        TyKind::Int(i) => Shape::Int(i),
        TyKind::Uint(u) => Shape::Uint(u),
        TyKind::Float(f) => Shape::Float(f),
        TyKind::Str => Shape::Str,
        TyKind::Never => Shape::Never,
        TyKind::Tuple(l) => Shape::Tuple(l.len),
        TyKind::Record(s, l) => Shape::Record(s, l.len),
        TyKind::Union(l) => Shape::Union(l.len),
        TyKind::Ref(_, _, m) => Shape::Ref(m),
        TyKind::RawPtr(_, m) => Shape::Ptr(m),
        TyKind::Array(..) => Shape::Array,
        TyKind::Slice(_) => Shape::Slice,
        TyKind::Pat(..) => Shape::Pat,
        TyKind::Adt(d, _) => Shape::Adt(d),
        TyKind::Foreign(d) => Shape::Foreign(d),
        TyKind::FnDef(d, _) => Shape::FnDef(d),
        TyKind::FnPtr(l, _) => Shape::FnPtr(l.len.saturating_sub(1)),
        TyKind::Closure(d, _) => Shape::Closure(d),
        TyKind::Coroutine(d, _) => Shape::Coroutine(d),
        TyKind::Dynamic(..) => Shape::Dynamic(Def(0)),
        TyKind::Infer(Var { kind: VarKind::Int, .. }) => Shape::AnyInt,
        TyKind::Infer(Var { kind: VarKind::Float, .. }) => Shape::AnyFloat,
        TyKind::Infer(_) => return None,
        TyKind::Alias(..)
        | TyKind::Param(_)
        | TyKind::Placeholder(..)
        | TyKind::Bound(..)
        | TyKind::Canonical(_)
        | TyKind::Error
        | TyKind::ReStatic
        | TyKind::ReParam(_)
        | TyKind::ReErased
        | TyKind::ConstValue(..)
        | TyKind::ConstParam(_)
        | TyKind::ExTrait(..)
        | TyKind::ExProj(..)
        | TyKind::ExAuto(_) => Shape::Blanket,
    })
}

/// Whether a kind is a region.
pub fn is_region(k: TyKind) -> bool {
    matches!(
        k,
        TyKind::ReStatic | TyKind::ReParam(_) | TyKind::ReErased | TyKind::Infer(Var { kind: VarKind::Region, .. })
    )
}

/// The children of one level, in the canonical order every walk uses (the canonicalizer, the
/// importer, the fingerprint and the stall-variable walk): lists in order, then the trailing
/// handles. Written into `out` (at most `lists` entries plus 2), returned as how many.
///
/// A list child is reported as a `ChildRef::List`; the caller reads its elements from the arena
/// or the portable table it walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Children {
    None,
    One(Ty),
    Two(Ty, Ty),
    List(List),
    /// A list, then one more term.
    ListThen(List, Ty),
}

/// The children of `k` (see [`Children`]).
pub fn children(k: TyKind) -> Children {
    match k {
        TyKind::Tuple(l)
        | TyKind::Record(_, l)
        | TyKind::Union(l)
        | TyKind::Adt(_, l)
        | TyKind::FnDef(_, l)
        | TyKind::FnPtr(l, _)
        | TyKind::Closure(_, l)
        | TyKind::Coroutine(_, l)
        | TyKind::Alias(_, _, _, l)
        | TyKind::ExTrait(_, l) => Children::List(l),
        TyKind::Ref(r, t, _) => Children::Two(r, t),
        TyKind::Array(t, n) => Children::Two(t, n),
        TyKind::RawPtr(t, _) | TyKind::Slice(t) => Children::One(t),
        TyKind::Dynamic(l, r) => Children::ListThen(l, r),
        TyKind::Pat(base, ranges) => Children::ListThen(ranges, base),
        TyKind::ExProj(_, l, t) => Children::ListThen(l, t),
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never
        | TyKind::Foreign(_)
        | TyKind::Param(_)
        | TyKind::Placeholder(..)
        | TyKind::Bound(..)
        | TyKind::Canonical(_)
        | TyKind::Infer(_)
        | TyKind::Error
        | TyKind::ReStatic
        | TyKind::ReParam(_)
        | TyKind::ReErased
        | TyKind::ConstValue(..)
        | TyKind::ConstParam(_)
        | TyKind::ExAuto(_) => Children::None,
    }
}

impl Pred {
    /// The self type of a trait-shaped predicate, read from `args[0]` by the caller; the
    /// predicate's trait for `Trait` and `HostEffect`.
    pub fn trait_of(&self) -> Option<Def> {
        match *self {
            Pred::Trait { trait_, .. } | Pred::HostEffect { trait_, .. } => Some(trait_),
            _ => None,
        }
    }
}
