//! Kind codes and what each column of a kind holds: one table per class, read by every walk
//! (fold, relate, generalize, canonicalize, export, print), so a walk is written once over the
//! columns instead of once per kind.
//!
//! A row is `(kind: u8, a: u32, b: u32)`. A column's [`Role`] says what it holds: nothing
//! (0), a raw number (an index, a width, a header), an item ([`crate::Def`]), a child
//! ([`crate::Node`]: class in the low two bits), or a const's scalar (an index into the
//! scalar column). The kinds cover `rustc_type_ir::TyKind` (rustc_type_ir/src/ty_kind.rs),
//! `RegionKind` (region_kind.rs), `ConstKind` (const_kind.rs), `ExistentialPredicate` and
//! `PredicateKind`/`ClauseKind` (predicate_kind.rs).

use crate::flags::Flags;
use crate::ids::ix8;

/// What a column holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Always 0.
    None,
    /// A number that is part of the kind's identity.
    Raw,
    /// An item of the arena's item column.
    Def,
    /// A node.
    Child,
    /// A const value, by its index in the scalar column.
    Scalar,
}

/// One kind's columns.
#[derive(Clone, Copy, Debug)]
pub struct Desc {
    pub a: Role,
    pub b: Role,
    /// The children are under one more binder (a fn pointer's signature, an existential
    /// predicate, a predicate: each is a `Binder` in rustc).
    pub binder: bool,
    /// The flags of the kind itself, before its children's.
    pub flags: u16,
}

impl Desc {
    const BAD: Desc = Desc { a: Role::None, b: Role::None, binder: false, flags: 0 };

    const fn new(a: Role, b: Role, binder: bool, flags: u16) -> Desc {
        Desc { a, b, binder, flags }
    }
}

/// Type and predicate kinds (the type arena).
pub mod ty {
    pub const BOOL: u8 = 0;
    pub const CHAR: u8 = 1;
    /// `a`: an `IntTy` code.
    pub const INT: u8 = 2;
    /// `a`: a `UintTy` code.
    pub const UINT: u8 = 3;
    /// `a`: a `FloatTy` code.
    pub const FLOAT: u8 = 4;
    pub const STR: u8 = 5;
    pub const NEVER: u8 = 6;
    /// `a`: the ADT; `b`: its arguments (a list).
    pub const ADT: u8 = 7;
    /// `a`: the extern type.
    pub const FOREIGN: u8 = 8;
    /// `a`: the element type; `b`: the length (a const).
    pub const ARRAY: u8 = 9;
    /// `a`: the element type.
    pub const SLICE: u8 = 10;
    /// `*const a`.
    pub const PTR: u8 = 11;
    /// `*mut a`.
    pub const PTR_MUT: u8 = 12;
    /// `&b a`.
    pub const REF: u8 = 13;
    /// `&b mut a`.
    pub const REF_MUT: u8 = 14;
    /// `a`: the fn item; `b`: its arguments.
    pub const FN_DEF: u8 = 15;
    /// `a`: the signature (a list: inputs, then the output), under a binder; `b`: the header
    /// (`FnHeader::bits`).
    pub const FN_PTR: u8 = 16;
    /// `a`: the predicates (a list of `EX_*` rows, each a binder); `b`: the region.
    pub const DYNAMIC: u8 = 17;
    /// `a`: the closure; `b`: its arguments (rustc's closure args: parent's, kind, signature,
    /// upvars).
    pub const CLOSURE: u8 = 18;
    pub const CORO_CLOSURE: u8 = 19;
    pub const COROUTINE: u8 = 20;
    pub const CORO_WITNESS: u8 = 21;
    /// `a`: the fields (a list).
    pub const TUPLE: u8 = 22;
    /// Aliases not known to be rigid: `a` the associated item, impl item, opaque or type
    /// alias; `b` the arguments. Relating one registers a projection goal.
    pub const PROJECTION: u8 = 23;
    pub const INHERENT: u8 = 24;
    pub const OPAQUE: u8 = 25;
    pub const FREE: u8 = 26;
    /// The same four, known rigid (the fork's `IsRigid::Yes`): related structurally.
    pub const RIGID_PROJECTION: u8 = 27;
    pub const RIGID_INHERENT: u8 = 28;
    pub const RIGID_OPAQUE: u8 = 29;
    pub const RIGID_FREE: u8 = 30;
    /// `a`: the parameter's index in the item's generics (parents first).
    pub const PARAM: u8 = 31;
    /// `a`: the De Bruijn index; `b`: the variable.
    pub const BOUND: u8 = 32;
    /// `a`: the canonical variable.
    pub const CANONICAL: u8 = 33;
    /// `a`: the universe; `b`: the variable.
    pub const PLACEHOLDER: u8 = 34;
    /// `a`: the variable.
    pub const TY_VAR: u8 = 35;
    pub const INT_VAR: u8 = 36;
    pub const FLOAT_VAR: u8 = 37;
    pub const ERROR: u8 = 38;
    /// A pattern type `a is ..` (typeck.md "Pattern types"): `a` the base; `b` its valid
    /// values, a list of const values in pairs, `start ..= end` each, canonical (sorted,
    /// merged, `start > end` wrapping).
    pub const PAT: u8 = 39;

    /// `dyn` predicates. `a`: the trait; `b`: its arguments without `Self`.
    pub const EX_TRAIT: u8 = 40;
    /// `a`: the associated item; `b`: its arguments without `Self`, then the term last.
    pub const EX_PROJECTION: u8 = 41;
    /// `a`: the auto trait.
    pub const EX_AUTO: u8 = 42;
    /// A structural record: `a` the sorted field types (a list); `b` the schema hash (a scalar).
    pub const RECORD: u8 = 43;
    /// A structural tagged union: `a` the canonical member types (a list).
    pub const UNION: u8 = 44;

    /// Predicates. `a`: the trait; `b`: its arguments, `Self` first.
    pub const P_TRAIT: u8 = 48;
    /// A negative trait predicate (`T: !Trait`).
    pub const P_NOT_TRAIT: u8 = 49;
    /// `a`: the alias (a type); `b`: the term.
    pub const P_PROJECTION: u8 = 50;
    /// `a: b` of regions.
    pub const P_REGION_OUTLIVES: u8 = 51;
    /// `a: b`, a type and a region.
    pub const P_TYPE_OUTLIVES: u8 = 52;
    /// `a` (any argument) is well formed.
    pub const P_WELL_FORMED: u8 = 53;
    /// The const `a` has the type `b`.
    pub const P_CONST_HAS_TYPE: u8 = 54;
    pub const P_CONST_EVALUATABLE: u8 = 55;
    /// `a <: b`.
    pub const P_SUBTYPE: u8 = 56;
    /// `a` coerces to `b`.
    pub const P_COERCE: u8 = 57;
    /// `a == b` where one is an alias.
    pub const P_ALIAS_EQ: u8 = 58;
    /// `a <: b` where one is an alias.
    pub const P_ALIAS_SUB: u8 = 59;
    /// The trait `a` is dyn-compatible.
    pub const P_DYN_COMPATIBLE: u8 = 60;
    /// The alias `a` normalizes to the term `b`.
    pub const P_NORMALIZES_TO: u8 = 61;
    /// The consts `a` and `b` are equal (one unevaluated).
    pub const P_CONST_EQUATE: u8 = 62;
    pub const P_AMBIGUOUS: u8 = 63;

    pub const COUNT: usize = 64;
}

/// Region kinds.
pub mod re {
    /// `a`: the parameter's index.
    pub const EARLY_PARAM: u8 = 0;
    /// `a`: the De Bruijn index; `b`: the variable.
    pub const BOUND: u8 = 1;
    /// `a`: the item whose late-bound region it is; `b`: the variable.
    pub const LATE_PARAM: u8 = 2;
    pub const STATIC: u8 = 3;
    /// `a`: the variable.
    pub const VAR: u8 = 4;
    /// `a`: the universe; `b`: the variable.
    pub const PLACEHOLDER: u8 = 5;
    pub const ERASED: u8 = 6;
    pub const ERROR: u8 = 7;
    /// `a`: the canonical variable.
    pub const CANONICAL: u8 = 8;

    pub const COUNT: usize = 9;
}

/// Const kinds.
pub mod ct {
    /// `a`: the parameter's index.
    pub const PARAM: u8 = 0;
    /// `a`: the variable.
    pub const VAR: u8 = 1;
    /// `a`: the De Bruijn index; `b`: the variable.
    pub const BOUND: u8 = 2;
    /// `a`: the universe; `b`: the variable.
    pub const PLACEHOLDER: u8 = 3;
    /// `a`: the type; `b`: the value, in the scalar column.
    pub const VALUE: u8 = 4;
    /// `a`: the const item or anonymous const; `b`: its arguments.
    pub const UNEVALUATED: u8 = 5;
    pub const ERROR: u8 = 6;
    /// `a`: the canonical variable.
    pub const CANONICAL: u8 = 7;

    pub const COUNT: usize = 8;
}

const fn ty_desc(kind: u8) -> Desc {
    use Role::{Child, Def, None, Raw, Scalar};
    match kind {
        ty::BOOL | ty::CHAR | ty::STR | ty::NEVER => Desc::new(None, None, false, 0),
        ty::INT | ty::UINT | ty::FLOAT => Desc::new(Raw, None, false, 0),
        ty::ADT | ty::FN_DEF => Desc::new(Def, Child, false, 0),
        ty::FOREIGN => Desc::new(Def, None, false, 0),
        ty::ARRAY | ty::REF | ty::REF_MUT => Desc::new(Child, Child, false, 0),
        ty::SLICE | ty::PTR | ty::PTR_MUT | ty::TUPLE | ty::UNION => Desc::new(Child, None, false, 0),
        ty::RECORD => Desc::new(Child, Scalar, false, 0),
        ty::FN_PTR => Desc::new(Child, Raw, true, 0),
        ty::DYNAMIC | ty::PAT => Desc::new(Child, Child, false, 0),
        ty::CLOSURE => Desc::new(Def, Child, false, 0),
        ty::CORO_CLOSURE | ty::COROUTINE | ty::CORO_WITNESS => Desc::new(Def, Child, false, Flags::CORO),
        ty::PROJECTION | ty::INHERENT | ty::FREE => {
            Desc::new(Def, Child, false, Flags::ALIAS | Flags::NEEDS_NORMALIZE)
        }
        ty::OPAQUE => Desc::new(Def, Child, false, Flags::ALIAS | Flags::NEEDS_NORMALIZE | Flags::OPAQUE),
        ty::RIGID_PROJECTION | ty::RIGID_INHERENT | ty::RIGID_FREE => Desc::new(Def, Child, false, Flags::ALIAS),
        ty::RIGID_OPAQUE => Desc::new(Def, Child, false, Flags::ALIAS | Flags::OPAQUE),
        ty::PARAM => Desc::new(Raw, None, false, Flags::PARAM),
        ty::BOUND => Desc::new(Raw, Raw, false, 0),
        ty::CANONICAL => Desc::new(Raw, None, false, Flags::CANONICAL),
        ty::PLACEHOLDER => Desc::new(Raw, Raw, false, Flags::PLACEHOLDER),
        ty::TY_VAR | ty::INT_VAR | ty::FLOAT_VAR => Desc::new(Raw, None, false, Flags::TY_INFER),
        ty::ERROR => Desc::new(None, None, false, Flags::ERROR),
        ty::EX_TRAIT | ty::EX_PROJECTION => Desc::new(Def, Child, true, 0),
        ty::EX_AUTO => Desc::new(Def, None, true, 0),
        ty::P_TRAIT | ty::P_NOT_TRAIT => Desc::new(Def, Child, true, 0),
        ty::P_PROJECTION
        | ty::P_REGION_OUTLIVES
        | ty::P_TYPE_OUTLIVES
        | ty::P_CONST_HAS_TYPE
        | ty::P_SUBTYPE
        | ty::P_COERCE
        | ty::P_ALIAS_EQ
        | ty::P_ALIAS_SUB
        | ty::P_NORMALIZES_TO
        | ty::P_CONST_EQUATE => Desc::new(Child, Child, true, 0),
        ty::P_WELL_FORMED | ty::P_CONST_EVALUATABLE => Desc::new(Child, None, true, 0),
        ty::P_DYN_COMPATIBLE => Desc::new(Def, None, true, 0),
        ty::P_AMBIGUOUS => Desc::new(None, None, true, 0),
        _ => Desc::BAD,
    }
}

const fn re_desc(kind: u8) -> Desc {
    use Role::{Def, None, Raw};
    match kind {
        re::EARLY_PARAM => Desc::new(Raw, None, false, Flags::PARAM | Flags::FREE_REGIONS),
        re::BOUND => Desc::new(Raw, Raw, false, 0),
        re::LATE_PARAM => Desc::new(Def, Raw, false, Flags::FREE_REGIONS),
        re::STATIC => Desc::new(None, None, false, Flags::FREE_REGIONS),
        re::VAR => Desc::new(Raw, None, false, Flags::RE_INFER | Flags::FREE_REGIONS),
        re::PLACEHOLDER => Desc::new(Raw, Raw, false, Flags::PLACEHOLDER | Flags::FREE_REGIONS),
        re::ERASED => Desc::new(None, None, false, Flags::FREE_REGIONS),
        re::ERROR => Desc::new(None, None, false, Flags::ERROR | Flags::FREE_REGIONS),
        re::CANONICAL => Desc::new(Raw, None, false, Flags::CANONICAL),
        _ => Desc::BAD,
    }
}

const fn ct_desc(kind: u8) -> Desc {
    use Role::{Child, Def, None, Raw, Scalar};
    match kind {
        ct::PARAM => Desc::new(Raw, None, false, Flags::PARAM),
        ct::VAR => Desc::new(Raw, None, false, Flags::CT_INFER),
        ct::BOUND => Desc::new(Raw, Raw, false, 0),
        ct::PLACEHOLDER => Desc::new(Raw, Raw, false, Flags::PLACEHOLDER),
        ct::VALUE => Desc::new(Child, Scalar, false, 0),
        ct::UNEVALUATED => Desc::new(Def, Child, false, Flags::ALIAS | Flags::NEEDS_NORMALIZE),
        ct::ERROR => Desc::new(None, None, false, Flags::ERROR),
        ct::CANONICAL => Desc::new(Raw, None, false, Flags::CANONICAL),
        _ => Desc::BAD,
    }
}

macro_rules! table {
    ($name:ident, $f:ident, $n:expr) => {
        pub(crate) const $name: [Desc; $n] = {
            let mut t = [Desc::BAD; $n];
            let mut k: u8 = 0;
            while ix8(k) < $n {
                t[ix8(k)] = $f(k);
                k += 1;
            }
            t
        };
    };
}

table!(TY, ty_desc, ty::COUNT);
table!(RE, re_desc, re::COUNT);
table!(CT, ct_desc, ct::COUNT);

/// Whether `kind` is a kind of class `tag` (a list has none).
pub(crate) fn valid(tag: u32, kind: u8) -> bool {
    match tag {
        crate::ids::TAG_TY => {
            kind <= ty::PAT
                || (kind >= ty::EX_TRAIT && kind <= ty::UNION)
                || (kind >= ty::P_TRAIT && kind <= ty::P_AMBIGUOUS)
        }
        crate::ids::TAG_REGION => ix8(kind) < re::COUNT,
        crate::ids::TAG_CONST => ix8(kind) < ct::COUNT,
        _ => false,
    }
}

/// The classes a child column of a kind may hold, as a mask of `1 << tag`.
pub(crate) const fn child_classes(tag: u32, kind: u8, b: bool) -> u8 {
    use crate::ids::{TAG_CONST, TAG_LIST, TAG_REGION, TAG_TY};
    const T: u8 = 1 << TAG_TY;
    const R: u8 = 1 << TAG_REGION;
    const C: u8 = 1 << TAG_CONST;
    const L: u8 = 1 << TAG_LIST;
    const ARG: u8 = T | R | C;
    if tag == TAG_CONST {
        return if b { L } else { T };
    }
    if tag != TAG_TY {
        return 0;
    }
    match (kind, b) {
        (ty::ARRAY, false) | (ty::SLICE | ty::PTR | ty::PTR_MUT | ty::REF | ty::REF_MUT, false) => T,
        (ty::ARRAY, true) => C,
        (ty::PAT, false) => T,
        (ty::PAT, true) => L,
        (ty::REF | ty::REF_MUT | ty::DYNAMIC, true) => R,
        (ty::FN_PTR | ty::DYNAMIC | ty::TUPLE | ty::RECORD | ty::UNION, false) => L,
        (ty::P_PROJECTION | ty::P_ALIAS_EQ | ty::P_NORMALIZES_TO, _) => T | C,
        (ty::P_WELL_FORMED, false) => ARG,
        (ty::P_REGION_OUTLIVES, _) => R,
        (ty::P_TYPE_OUTLIVES, false) | (ty::P_SUBTYPE | ty::P_COERCE | ty::P_ALIAS_SUB, _) => T,
        (ty::P_TYPE_OUTLIVES, true) => R,
        (ty::P_CONST_HAS_TYPE, false) | (ty::P_CONST_EVALUATABLE, false) | (ty::P_CONST_EQUATE, _) => C,
        (ty::P_CONST_HAS_TYPE, true) => T,
        // Items' argument lists: ADTs, fn items, closures, coroutines, aliases, `dyn` and
        // trait predicates.
        (_, true) => L,
        _ => 0,
    }
}

/// Whether `kind` is a predicate kind.
pub const fn is_pred(kind: u8) -> bool {
    kind >= ty::P_TRAIT && kind <= ty::P_AMBIGUOUS
}

/// Whether `kind` is an existential predicate of a `dyn` (`ExTrait`, `ExProjection`, `ExAuto`).
pub const fn is_existential(kind: u8) -> bool {
    kind >= ty::EX_TRAIT && kind <= ty::EX_AUTO
}

/// Whether `kind` is an alias not known to be rigid.
pub const fn is_unnormalized_alias(kind: u8) -> bool {
    kind >= ty::PROJECTION && kind <= ty::FREE
}

/// Whether `kind` is an alias of any kind.
pub const fn is_alias(kind: u8) -> bool {
    kind >= ty::PROJECTION && kind <= ty::RIGID_FREE
}

/// Whether `kind` is an inference variable.
pub const fn is_infer(kind: u8) -> bool {
    kind >= ty::TY_VAR && kind <= ty::FLOAT_VAR
}

/// The descriptor of a type-arena kind (a bad kind reads as one with no columns).
#[inline(always)]
pub(crate) fn ty_d(kind: u8) -> Desc {
    match TY.get(ix8(kind)) {
        Some(d) => *d,
        None => Desc::BAD,
    }
}

#[inline(always)]
pub(crate) fn re_d(kind: u8) -> Desc {
    match RE.get(ix8(kind)) {
        Some(d) => *d,
        None => Desc::BAD,
    }
}

#[inline(always)]
pub(crate) fn ct_d(kind: u8) -> Desc {
    match CT.get(ix8(kind)) {
        Some(d) => *d,
        None => Desc::BAD,
    }
}
