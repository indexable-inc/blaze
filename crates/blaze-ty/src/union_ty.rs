//! Canonical Union Type / Anonymous Enum Algebra (`blaze-ty::union_ty`).
//!
//! First-class compiler representation for structural union types (`A | B | C`), implementing
//! Blaze's union-type rules:
//!
//! 1. **Order-independent, lifetime-erased identity**: members are sorted by their lifetime-erased
//!    [`Ty`] key ([`crate::type_set::erase_regions`]), so `A | B | C` and `C | A | B` are
//!    structurally identical, and `&'a str | &'b str` collapses to one member. Two members that
//!    might instantiate to the same type (e.g., generic parameters `T | U` or `T | u32`) are
//!    rejected unless an explicit `T != U` [`DisjointBound`] rules out overlap.
//! 2. **Small local-tag representation**: each distinct union gets a compact local discriminant
//!    (`0` bytes for `<= 1` member or niche-elided, `u8` for `<= 256` members, `u16` otherwise)
//!    plus optimal payload size/alignment and niche-reuse hints ([`UnionLayout`]).
//! 3. **By-value widening only**: `A | B` widens by value to `A | B | C` via an O(N + M)
//!    compile-time `source_tag -> target_tag` re-tag table ([`UnionTy::widen_tag_map`]). Reference
//!    widening (`&(A | B)` -> `&(A | B | C)`) is explicitly rejected ([`UnionError::RefCannotWiden`])
//!    because local tag encodings differ across unions.
//! 4. **Type-safe narrowing**: matching a member `M` removes it from `U`, yielding `U \ {M}` as
//!    [`NarrowResult::Empty`], [`NarrowResult::Single`], or [`NarrowResult::Union`].
//! 5. **Fixed auto-traits & zero user impls**: only the fixed allowlist ([`AUTO_TRAITS`]:
//!    `Debug`, `Display`, `Error`, `Clone`, `Copy`, `PartialEq`, `Eq`, `Hash`, `Send`, `Sync`,
//!    `Unpin`) is automatically lifted when every member implements the trait; user-written `impl`
//!    blocks on union types are forbidden ([`UnionError::ImplForbidden`]) to avoid coherence issues.
//! 6. **No implicit union inference**: unannotated heterogeneous branches (`if c { 1u8 } else { "x" }`)
//!    never infer a union type ([`UnionError::NoImplicitUnion`]).

use blaze_own::{Alloc, Vec};

use crate::cx::TyCx;
use crate::ids::{Mutability, Ty};
use crate::type_set::erase_regions;
use crate::tys::{ConstKind, TyKind, Tys};

/// One member of a canonical [`UnionTy`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnionMember {
    /// Lifetime-erased canonical key used for ordering, deduplication, lookup, and widening.
    pub key: Ty,
    /// Full member type preserving concrete lifetimes.
    pub ty: Ty,
}

/// An explicit disjointness bound `T != U` proving two type expressions cannot instantiate to the
/// same concrete type. Stored symmetrically with `lhs <= rhs` (lifetimes erased).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct DisjointBound {
    pub lhs: Ty,
    pub rhs: Ty,
}

impl DisjointBound {
    /// Constructs a canonicalized, symmetric `T != U` bound with lifetimes erased.
    pub fn new(cx: &mut TyCx<'_>, a: Ty, b: Ty, scratch: Alloc<'_>) -> Self {
        let ka = erase_regions(cx, a, scratch);
        let kb = erase_regions(cx, b, scratch);
        if ka <= kb {
            Self { lhs: ka, rhs: kb }
        } else {
            Self { lhs: kb, rhs: ka }
        }
    }

    /// Constructs a canonicalized `T != U` bound from already-erased keys.
    #[inline]
    pub const fn from_erased(a: Ty, b: Ty) -> Self {
        if a.index() <= b.index() {
            Self { lhs: a, rhs: b }
        } else {
            Self { lhs: b, rhs: a }
        }
    }
}

/// How identical lifetime-erased member keys (such as `&'a str | &'b str` or `A | A`) are handled
/// during union construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DuplicatePolicy {
    /// Deduplicate identical erased keys into a single member (standard algebraic union identity:
    /// `&'a str | &'b str` is one member).
    CollapseIdentical,
    /// Reject any duplicate erased key with [`UnionError::DuplicateMember`].
    RejectDuplicate,
}

/// How a union value is being passed when checking widening eligibility.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WidenMode {
    /// Owned by-value move (`A | B` -> `A | B | C`): allowed via local re-tagging.
    ByValue,
    /// Behind a shared or mutable reference (`&(A | B)` -> `&(A | B | C)`): forbidden when the
    /// target union differs from the source union.
    ByRef(Mutability),
}

/// Errors produced by union construction, widening, narrowing, impl checks, or branch inference.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnionError {
    /// Two members have the same lifetime-erased key under [`DuplicatePolicy::RejectDuplicate`].
    DuplicateMember { key: Ty },
    /// Two members (involving generic params, aliases, or infer vars) might instantiate to the
    /// same type and no `T != U` [`DisjointBound`] rules out overlap.
    ParamOverlap { lhs: Ty, rhs: Ty },
    /// Source union contains a member not present in the target union.
    NotSubset { missing_member: Ty },
    /// Widening behind a reference (`&(A | B)` -> `&(A | B | C)`) is forbidden because local tags
    /// differ between distinct unions.
    RefCannotWiden,
    /// Queried or narrowed member is not present in this union.
    UnknownMember { key: Ty },
    /// A `match` arm matched a union member that was already covered by an earlier arm.
    RedundantArm { key: Ty },
    /// User-written `impl` blocks (inherent or trait) on a union type are forbidden.
    ImplForbidden,
    /// Unannotated heterogeneous branches (`if c { 1u8 } else { "x" }`) do not implicitly infer a
    /// union type.
    NoImplicitUnion { lhs: Ty, rhs: Ty },
}

/// Niche availability of a single union member payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NicheKind {
    /// No unused bit patterns available for tag elision.
    None,
    /// `bool`-like niche: 1 byte, 2 valid values (`0..=1`), 254 niche states.
    Bool,
    /// Non-zero / non-null niche (e.g., `&T`, `NonZeroU32`, `fn()`): 1 unused state (`0`).
    NonZero,
    /// General niche with `invalid_values` unused states at byte `offset` within the payload.
    Range { offset: u32, invalid_values: u32 },
}

impl NicheKind {
    /// Maximum number of additional ZST variants that can be stored inside this niche.
    #[inline]
    pub const fn capacity(self) -> u32 {
        match self {
            NicheKind::None => 0,
            NicheKind::Bool => 254,
            NicheKind::NonZero => 1,
            NicheKind::Range { invalid_values, .. } => invalid_values,
        }
    }
}

/// Infers the built-in [`NicheKind`] of a primitive or reference [`Ty`].
pub fn niche_for_ty(tys: &Tys<'_>, ty: Ty) -> NicheKind {
    match tys.ty_kind(ty) {
        TyKind::Bool => NicheKind::Bool,
        TyKind::Ref(..) | TyKind::FnPtr(..) => NicheKind::NonZero,
        _ => NicheKind::None,
    }
}

/// Physical size, alignment, and niche metadata of a single union member.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MemberLayout {
    pub key: Ty,
    pub size: u32,
    pub align: u32,
    pub niche: NicheKind,
}

/// Computed local-tag physical layout of a canonical [`UnionTy`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UnionLayout {
    /// Local discriminant size in bytes:
    /// - `0` when `<= 1` member or when `niche_reused` is `true`
    /// - `1` (`u8`) when `2..=256` members
    /// - `2` (`u16`) when `257..=65536` members
    pub tag_bytes: u32,
    /// Byte offset of the payload within the union (`0` when `tag_bytes == 0`).
    pub payload_offset: u32,
    /// Maximum payload size across all members.
    pub payload_size: u32,
    /// Total union size in bytes, rounded up to a multiple of `align`.
    pub size: u32,
    /// Total union alignment in bytes.
    pub align: u32,
    /// `true` iff a member's niche absorbs the local discriminant (zero extra tag bytes).
    pub niche_reused: bool,
    /// Canonical member index providing the reused niche, if `niche_reused` is `true`.
    pub niche_member: Option<u32>,
}

/// The exact fixed set of traits automatically implemented for a union `A | B | ...` whenever
/// every member implements the trait.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum AutoTrait {
    Debug,
    Display,
    Error,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Send,
    Sync,
    Unpin,
}

/// Complete fixed allowlist of traits automatically lifted across union members.
pub const AUTO_TRAITS: &[AutoTrait] = &[
    AutoTrait::Debug,
    AutoTrait::Display,
    AutoTrait::Error,
    AutoTrait::Clone,
    AutoTrait::Copy,
    AutoTrait::PartialEq,
    AutoTrait::Eq,
    AutoTrait::Hash,
    AutoTrait::Send,
    AutoTrait::Sync,
    AutoTrait::Unpin,
];

impl AutoTrait {
    /// Resolves a trait name against the fixed union allowlist. Any trait outside [`AUTO_TRAITS`]
    /// returns `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Debug" => Some(Self::Debug),
            "Display" => Some(Self::Display),
            "Error" => Some(Self::Error),
            "Clone" => Some(Self::Clone),
            "Copy" => Some(Self::Copy),
            "PartialEq" => Some(Self::PartialEq),
            "Eq" => Some(Self::Eq),
            "Hash" => Some(Self::Hash),
            "Send" => Some(Self::Send),
            "Sync" => Some(Self::Sync),
            "Unpin" => Some(Self::Unpin),
            _ => None,
        }
    }

    /// Returns `true` iff `name` is in the fixed union auto-trait allowlist.
    #[inline]
    pub fn is_allowed_name(name: &str) -> bool {
        Self::from_name(name).is_some()
    }
}

/// Result of removing one matched member `M` from a union `U` (`U \ {M}`).
#[derive(Debug)]
pub enum NarrowResult<'a> {
    /// Removing the sole member leaves an uninhabited union (`!`).
    Empty,
    /// Removing a member from a 2-member union collapses to the single remaining [`Ty`].
    Single(Ty),
    /// Removing a member from a `>= 3`-member union produces the narrowed [`UnionTy`].
    Union(UnionTy<'a>),
}

/// Outcome of checking two branches (`if c { lhs } else { rhs }`) against an optional expected
/// union type annotation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchCheck {
    /// Both branches agree on a single non-union type (no expected union needed).
    Single(Ty),
    /// Both branches were checked against an explicitly annotated union type, yielding their
    /// local tag indices (`None` if that branch diverges with `!`).
    AnnotatedUnion { lhs_tag: Option<u16>, rhs_tag: Option<u16> },
}

/// Checks whether a user-written `impl` block on a union type is allowed.
///
/// Always returns `Err(UnionError::ImplForbidden)`: unions only receive the fixed [`AUTO_TRAITS`]
/// and never allow user-written inherent or trait `impl`s, eliminating coherence conflicts.
#[inline]
pub const fn check_impl_allowed() -> Result<(), UnionError> {
    Err(UnionError::ImplForbidden)
}

/// Checks unannotated branch typing (`if c { lhs } else { rhs }`).
///
/// Heterogeneous unannotated branches never implicitly infer a union type and return
/// `Err(UnionError::NoImplicitUnion { lhs, rhs })`.
#[inline]
pub const fn infer_branch_union(lhs: Ty, rhs: Ty) -> Result<Ty, UnionError> {
    if lhs.index() == rhs.index() {
        Ok(lhs)
    } else if lhs.index() == Ty::NEVER.index() {
        Ok(rhs)
    } else if rhs.index() == Ty::NEVER.index() {
        Ok(lhs)
    } else {
        Err(UnionError::NoImplicitUnion { lhs, rhs })
    }
}

/// Checks two branches against an optional expected union type annotation.
///
/// - When `expected` is `None`, delegates to [`infer_branch_union`] (rejecting heterogeneous
///   branches with [`UnionError::NoImplicitUnion`]).
/// - When `expected` is `Some(u)`, verifies every non-diverging branch is a member of `u` and
///   returns its local tag index.
pub fn check_branch_against_expected(
    cx: &mut TyCx<'_>,
    lhs: Ty,
    rhs: Ty,
    expected: Option<&UnionTy<'_>>,
    scratch: Alloc<'_>,
) -> Result<BranchCheck, UnionError> {
    let Some(union_ty) = expected else {
        return infer_branch_union(lhs, rhs).map(BranchCheck::Single);
    };
    let lhs_tag = if lhs == Ty::NEVER {
        None
    } else {
        let key = erase_regions(cx, lhs, scratch);
        let idx = union_ty.tag_of_key(key).ok_or(UnionError::UnknownMember { key })?;
        Some(idx)
    };
    let rhs_tag = if rhs == Ty::NEVER {
        None
    } else {
        let key = erase_regions(cx, rhs, scratch);
        let idx = union_ty.tag_of_key(key).ok_or(UnionError::UnknownMember { key })?;
        Some(idx)
    };
    Ok(BranchCheck::AnnotatedUnion { lhs_tag, rhs_tag })
}

#[inline]
const fn align_up(value: u32, align: u32) -> u32 {
    let a = if align == 0 { 1 } else { align };
    let rem = value % a;
    if rem == 0 {
        value
    } else {
        value.saturating_add(a - rem)
    }
}

/// Non-recursive check whether two distinct lifetime-erased types `a` and `b` might instantiate to
/// the same concrete type given a sorted slice of `T != U` [`DisjointBound`]s.
pub fn may_overlap(
    tys: &Tys<'_>,
    a: Ty,
    b: Ty,
    disjoint_bounds: &[DisjointBound],
    scratch: Alloc<'_>,
) -> bool {
    let mut stack: Vec<(Ty, Ty), Alloc<'_>> = Vec::with_capacity_in(8, scratch);
    stack.push((a, b));

    while let Some((x, y)) = stack.pop() {
        if x == y {
            continue;
        }
        let bound = DisjointBound::from_erased(x, y);
        if disjoint_bounds.binary_search(&bound).is_ok() {
            return false;
        }

        let kx = tys.ty_kind(x);
        let ky = tys.ty_kind(y);

        match (kx, ky) {
            // Unconstrained type parameters, inference variables, aliases, placeholders, or error
            // types can unify with any type unless ruled out by an explicit `T != U` bound above.
            (
                TyKind::Param(_)
                | TyKind::TyVar(_)
                | TyKind::Alias { .. }
                | TyKind::Placeholder(..)
                | TyKind::Bound(..)
                | TyKind::Canonical(_)
                | TyKind::Error,
                _,
            )
            | (
                _,
                TyKind::Param(_)
                | TyKind::TyVar(_)
                | TyKind::Alias { .. }
                | TyKind::Placeholder(..)
                | TyKind::Bound(..)
                | TyKind::Canonical(_)
                | TyKind::Error,
            ) => {}

            (TyKind::IntVar(_), TyKind::Int(_) | TyKind::Uint(_) | TyKind::IntVar(_))
            | (TyKind::Int(_) | TyKind::Uint(_), TyKind::IntVar(_)) => {}
            (TyKind::IntVar(_), _) | (_, TyKind::IntVar(_)) => return false,

            (TyKind::FloatVar(_), TyKind::Float(_) | TyKind::FloatVar(_))
            | (TyKind::Float(_), TyKind::FloatVar(_)) => {}
            (TyKind::FloatVar(_), _) | (_, TyKind::FloatVar(_)) => return false,

            (TyKind::Adt(d1, args1), TyKind::Adt(d2, args2)) => {
                if d1 != d2 {
                    return false;
                }
                let (l1, l2) = (tys.list(args1), tys.list(args2));
                if l1.len() != l2.len() {
                    return false;
                }
                for (a1, a2) in l1.iter().zip(l2.iter()) {
                    if let (Some(t1), Some(t2)) = (a1.ty(), a2.ty()) {
                        stack.push((t1, t2));
                    } else if let (Some(c1), Some(c2)) = (a1.konst(), a2.konst()) {
                        if c1 != c2
                            && matches!(tys.const_kind(c1), ConstKind::Value(..))
                            && matches!(tys.const_kind(c2), ConstKind::Value(..))
                        {
                            return false;
                        }
                    }
                }
            }
            (TyKind::Ref(_, t1, m1), TyKind::Ref(_, t2, m2)) => {
                if m1 != m2 {
                    return false;
                }
                stack.push((t1, t2));
            }
            (TyKind::RawPtr(t1, m1), TyKind::RawPtr(t2, m2)) => {
                if m1 != m2 {
                    return false;
                }
                stack.push((t1, t2));
            }
            (TyKind::Slice(t1), TyKind::Slice(t2)) => {
                stack.push((t1, t2));
            }
            (TyKind::Array(t1, c1), TyKind::Array(t2, c2)) => {
                if c1 != c2
                    && matches!(tys.const_kind(c1), ConstKind::Value(..))
                    && matches!(tys.const_kind(c2), ConstKind::Value(..))
                {
                    return false;
                }
                stack.push((t1, t2));
            }
            (TyKind::Tuple(l1), TyKind::Tuple(l2)) | (TyKind::Union(l1), TyKind::Union(l2)) => {
                let (s1, s2) = (tys.list(l1), tys.list(l2));
                if s1.len() != s2.len() {
                    return false;
                }
                for (a1, a2) in s1.iter().zip(s2.iter()) {
                    if let (Some(t1), Some(t2)) = (a1.ty(), a2.ty()) {
                        stack.push((t1, t2));
                    }
                }
            }
            (TyKind::Record(h1, l1), TyKind::Record(h2, l2)) => {
                if h1 != h2 {
                    return false;
                }
                let (s1, s2) = (tys.list(l1), tys.list(l2));
                if s1.len() != s2.len() {
                    return false;
                }
                for (a1, a2) in s1.iter().zip(s2.iter()) {
                    if let (Some(t1), Some(t2)) = (a1.ty(), a2.ty()) {
                        stack.push((t1, t2));
                    }
                }
            }
            (TyKind::FnPtr(l1, h1), TyKind::FnPtr(l2, h2)) => {
                if h1 != h2 {
                    return false;
                }
                let (s1, s2) = (tys.list(l1), tys.list(l2));
                if s1.len() != s2.len() {
                    return false;
                }
                for (a1, a2) in s1.iter().zip(s2.iter()) {
                    if let (Some(t1), Some(t2)) = (a1.ty(), a2.ty()) {
                        stack.push((t1, t2));
                    }
                }
            }
            // Any other distinct rigid heads are provably disjoint.
            _ => return false,
        }
    }

    true
}

/// Canonical, order-independent, lifetime-erased union type (`A | B | C`).
#[derive(Debug)]
pub struct UnionTy<'a> {
    /// Sorted strictly ascending by `member.key` (each entry's index is its local tag).
    members: Vec<UnionMember, Alloc<'a>>,
    /// Sorted, deduplicated `T != U` disjointness bounds in scope for this union.
    disjoint_bounds: Vec<DisjointBound, Alloc<'a>>,
}

impl<'a> UnionTy<'a> {
    /// Constructs an empty (uninhabited, `!`) union.
    pub fn empty(alloc: Alloc<'a>) -> Self {
        Self {
            members: Vec::new_in(alloc),
            disjoint_bounds: Vec::new_in(alloc),
        }
    }

    /// Canonicalizes an unordered list of member types into a [`UnionTy`], collapsing identical
    /// lifetime-erased keys (`&'a str | &'b str` becomes one member) and verifying that no pair
    /// of distinct members can overlap without a `T != U` bound.
    pub fn from_members(
        cx: &mut TyCx<'_>,
        raw_members: &[Ty],
        alloc: Alloc<'a>,
    ) -> Result<Self, UnionError> {
        Self::from_members_with_policy(
            cx,
            raw_members,
            &[],
            DuplicatePolicy::CollapseIdentical,
            alloc,
        )
    }

    /// Canonicalizes member types with explicit `T != U` [`DisjointBound`]s and a chosen
    /// [`DuplicatePolicy`].
    pub fn from_members_with_policy(
        cx: &mut TyCx<'_>,
        raw_members: &[Ty],
        raw_bounds: &[DisjointBound],
        policy: DuplicatePolicy,
        alloc: Alloc<'a>,
    ) -> Result<Self, UnionError> {
        let mut disjoint_bounds = Vec::with_capacity_in(raw_bounds.len(), alloc);
        for &b in raw_bounds {
            disjoint_bounds.push(DisjointBound::new(cx, b.lhs, b.rhs, alloc));
        }
        disjoint_bounds.sort_unstable();
        disjoint_bounds.dedup();

        let mut members = Vec::with_capacity_in(raw_members.len(), alloc);
        for &ty in raw_members {
            if ty == Ty::NEVER {
                // `!` is the algebraic identity element of union (`A | ! == A`).
                continue;
            }
            let key = erase_regions(cx, ty, alloc);
            members.push(UnionMember { key, ty });
        }
        members.sort_unstable_by_key(|m| m.key);

        match policy {
            DuplicatePolicy::CollapseIdentical => {
                members.dedup_by(|a, b| a.key == b.key);
            }
            DuplicatePolicy::RejectDuplicate => {
                for w in members.windows(2) {
                    if w[0].key == w[1].key {
                        return Err(UnionError::DuplicateMember { key: w[0].key });
                    }
                }
            }
        }

        // Verify pairwise disjointness for any members containing type parameters / aliases / vars.
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let a = members[i].key;
                let b = members[j].key;
                if may_overlap(&cx.tys, a, b, &disjoint_bounds, alloc) {
                    return Err(UnionError::ParamOverlap { lhs: a, rhs: b });
                }
            }
        }

        Ok(Self { members, disjoint_bounds })
    }

    /// Canonical sorted member slice; each member's slice index is its local tag.
    #[inline]
    pub fn members(&self) -> &[UnionMember] {
        &self.members
    }

    /// Canonical sorted `T != U` bounds recorded for this union.
    #[inline]
    pub fn disjoint_bounds(&self) -> &[DisjointBound] {
        &self.disjoint_bounds
    }

    /// Number of distinct members in this union.
    #[inline]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Returns `true` iff this union is uninhabited (0 members).
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Local discriminant size in bytes before niche optimization:
    /// `0` for `<= 1` member, `1` (`u8`) for `2..=256` members, `2` (`u16`) for `257..=65536`.
    #[inline]
    pub const fn local_tag_bytes_for_count(count: usize) -> u32 {
        if count <= 1 {
            0
        } else if count <= 256 {
            1
        } else if count <= 65_536 {
            2
        } else {
            4
        }
    }

    /// Local tag size in bytes for this union (before niche elision).
    #[inline]
    pub fn local_tag_bytes(&self) -> u32 {
        Self::local_tag_bytes_for_count(self.members.len())
    }

    /// Returns `true` iff `self` and `other` represent the exact same canonical union (same
    /// lifetime-erased member keys, independent of declaration order).
    pub fn eq_canonical(&self, other: &UnionTy<'_>) -> bool {
        self.members.len() == other.members.len()
            && self
                .members
                .iter()
                .zip(other.members.iter())
                .all(|(a, b)| a.key == b.key)
    }

    /// O(log N) lookup of a member's local tag index by its lifetime-erased key.
    #[inline]
    pub fn tag_of_key(&self, erased_key: Ty) -> Option<u16> {
        self.members
            .binary_search_by_key(&erased_key, |m| m.key)
            .ok()
            .and_then(|idx| u16::try_from(idx).ok())
    }

    /// O(log N) lookup of a member's local tag index by [`Ty`] (erasing lifetimes automatically).
    pub fn tag_of(&self, cx: &mut TyCx<'_>, ty: Ty, scratch: Alloc<'_>) -> Option<u16> {
        let key = erase_regions(cx, ty, scratch);
        self.tag_of_key(key)
    }

    /// Checks whether `self` can widen by value into `target` (`self ⊆ target`) in O(N + M) time.
    pub fn can_widen(&self, target: &UnionTy<'_>) -> Result<(), UnionError> {
        let (mut i, mut j) = (0, 0);
        while i < self.members.len() && j < target.members.len() {
            let s = self.members[i].key;
            let t = target.members[j].key;
            if s == t {
                i += 1;
                j += 1;
            } else if s < t {
                return Err(UnionError::NotSubset { missing_member: s });
            } else {
                j += 1;
            }
        }
        if i < self.members.len() {
            return Err(UnionError::NotSubset {
                missing_member: self.members[i].key,
            });
        }
        Ok(())
    }

    /// Checks whether `self` can widen into `target` under the given [`WidenMode`].
    ///
    /// - [`WidenMode::ByValue`]: succeeds iff `self ⊆ target`.
    /// - [`WidenMode::ByRef`]: only succeeds when `self` and `target` are canonically identical
    ///   (e.g., `&(A | B)` and `&(B | A)`). Any strict widening (`&(A | B)` -> `&(A | B | C)`)
    ///   fails with [`UnionError::RefCannotWiden`].
    pub fn can_widen_in_mode(
        &self,
        target: &UnionTy<'_>,
        mode: WidenMode,
    ) -> Result<(), UnionError> {
        self.can_widen(target)?;
        match mode {
            WidenMode::ByValue => Ok(()),
            WidenMode::ByRef(_) => {
                if self.eq_canonical(target) {
                    Ok(())
                } else {
                    Err(UnionError::RefCannotWiden)
                }
            }
        }
    }

    /// Computes the compile-time by-value re-tag mapping `map[source_tag] = target_tag` from
    /// subset `self` to superset `target` in O(N + M) sorted merge.
    pub fn widen_tag_map(
        &self,
        target: &UnionTy<'_>,
        alloc: Alloc<'a>,
    ) -> Result<Vec<u16, Alloc<'a>>, UnionError> {
        let mut map = Vec::with_capacity_in(self.members.len(), alloc);
        let (mut i, mut j) = (0, 0);
        while i < self.members.len() && j < target.members.len() {
            let s = self.members[i].key;
            let t = target.members[j].key;
            if s == t {
                map.push(u16::try_from(j).unwrap_or(u16::MAX));
                i += 1;
                j += 1;
            } else if s < t {
                return Err(UnionError::NotSubset { missing_member: s });
            } else {
                j += 1;
            }
        }
        if i < self.members.len() {
            return Err(UnionError::NotSubset {
                missing_member: self.members[i].key,
            });
        }
        Ok(map)
    }

    /// Computes the re-tag mapping only when allowed by [`WidenMode`], explicitly rejecting
    /// reference widening with [`UnionError::RefCannotWiden`].
    pub fn widen_tag_map_in_mode(
        &self,
        target: &UnionTy<'_>,
        mode: WidenMode,
        alloc: Alloc<'a>,
    ) -> Result<Vec<u16, Alloc<'a>>, UnionError> {
        self.can_widen_in_mode(target, mode)?;
        self.widen_tag_map(target, alloc)
    }

    /// Narrows `self` by removing a matched member `erased_key`, returning `U \ {erased_key}` as
    /// [`NarrowResult::Empty`], [`NarrowResult::Single`], or [`NarrowResult::Union`].
    pub fn narrow_remove_key(
        &self,
        erased_key: Ty,
        alloc: Alloc<'a>,
    ) -> Result<NarrowResult<'a>, UnionError> {
        let pos = self
            .members
            .binary_search_by_key(&erased_key, |m| m.key)
            .map_err(|_| UnionError::UnknownMember { key: erased_key })?;

        let rem_len = self.members.len() - 1;
        match rem_len {
            0 => Ok(NarrowResult::Empty),
            1 => {
                let sole = if pos == 0 { self.members[1] } else { self.members[0] };
                Ok(NarrowResult::Single(sole.ty))
            }
            _ => {
                let mut members = Vec::with_capacity_in(rem_len, alloc);
                members.extend_from_slice(&self.members[..pos]);
                members.extend_from_slice(&self.members[pos + 1..]);

                let mut disjoint_bounds = Vec::with_capacity_in(self.disjoint_bounds.len(), alloc);
                disjoint_bounds.extend_from_slice(&self.disjoint_bounds);

                Ok(NarrowResult::Union(UnionTy {
                    members,
                    disjoint_bounds,
                }))
            }
        }
    }

    /// Narrows `self` by removing a matched member `ty` (erasing lifetimes automatically).
    pub fn narrow_remove(
        &self,
        cx: &mut TyCx<'_>,
        ty: Ty,
        alloc: Alloc<'a>,
    ) -> Result<NarrowResult<'a>, UnionError> {
        let key = erase_regions(cx, ty, alloc);
        self.narrow_remove_key(key, alloc)
    }

    /// Narrows `self` by removing a sequence of matched member arms (`U \ {M1, M2, ...}`),
    /// returning the exact residual type for a catch-all / propagation arm (`Err(other) => return Err(other)`).
    ///
    /// Rejects arms not present in `self` with [`UnionError::UnknownMember`] and duplicate arms
    /// with [`UnionError::RedundantArm`].
    pub fn narrow_remove_many(
        &self,
        cx: &mut TyCx<'_>,
        matched: &[Ty],
        alloc: Alloc<'a>,
    ) -> Result<NarrowResult<'a>, UnionError> {
        let mut removed: Vec<bool, Alloc<'a>> = Vec::with_capacity_in(self.members.len(), alloc);
        removed.resize(self.members.len(), false);
        let mut remaining = self.members.len();

        for &ty in matched {
            let key = erase_regions(cx, ty, alloc);
            let pos = self
                .members
                .binary_search_by_key(&key, |m| m.key)
                .map_err(|_| UnionError::UnknownMember { key })?;
            if let Some(slot) = removed.get_mut(pos) {
                if *slot {
                    return Err(UnionError::RedundantArm { key });
                }
                *slot = true;
                remaining = remaining.saturating_sub(1);
            }
        }

        match remaining {
            0 => Ok(NarrowResult::Empty),
            1 => {
                for (m, &is_rm) in self.members.iter().zip(removed.iter()) {
                    if !is_rm {
                        return Ok(NarrowResult::Single(m.ty));
                    }
                }
                Ok(NarrowResult::Empty)
            }
            _ => {
                let mut members = Vec::with_capacity_in(remaining, alloc);
                for (m, &is_rm) in self.members.iter().zip(removed.iter()) {
                    if !is_rm {
                        members.push(*m);
                    }
                }
                let mut disjoint_bounds = Vec::with_capacity_in(self.disjoint_bounds.len(), alloc);
                disjoint_bounds.extend_from_slice(&self.disjoint_bounds);
                Ok(NarrowResult::Union(UnionTy { members, disjoint_bounds }))
            }
        }
    }

    /// Computes the optimal local-tag layout for this union given `(size, align, niche)` for each
    /// member key.
    ///
    /// Automatically detects niche-reuse opportunities (when one member carries a `Bool`,
    /// `NonZero`, or `Range` niche with enough unused bit patterns to encode all remaining
    /// zero-sized members with `tag_bytes == 0`).
    pub fn compute_layout_with(
        &self,
        mut layout_of: impl FnMut(Ty) -> (u32, u32, NicheKind),
    ) -> UnionLayout {
        let count = self.members.len();
        if count == 0 {
            return UnionLayout {
                tag_bytes: 0,
                payload_offset: 0,
                payload_size: 0,
                size: 0,
                align: 1,
                niche_reused: false,
                niche_member: None,
            };
        }

        let mut max_payload_size = 0u32;
        let mut max_payload_align = 1u32;
        let mut non_zst_count = 0usize;
        let mut sole_non_zst: Option<(u32, u32, u32, NicheKind)> = None;

        for (idx, m) in self.members.iter().enumerate() {
            let (sz, raw_al, niche) = layout_of(m.key);
            let al = raw_al.max(1);
            max_payload_size = max_payload_size.max(sz);
            max_payload_align = max_payload_align.max(al);
            if sz > 0 || al > 1 {
                non_zst_count += 1;
                sole_non_zst = Some((idx as u32, sz, al, niche));
            }
        }

        if count == 1 {
            let size = align_up(max_payload_size, max_payload_align);
            return UnionLayout {
                tag_bytes: 0,
                payload_offset: 0,
                payload_size: max_payload_size,
                size,
                align: max_payload_align,
                niche_reused: false,
                niche_member: None,
            };
        }

        // Niche reuse: if at most one member is non-ZST and it has enough niche states to encode
        // every other (zero-sized) member, the tag is absorbed into the niche with 0 tag bytes.
        let other_variants = u32::try_from(count - 1).unwrap_or(u32::MAX);
        if non_zst_count == 1 {
            if let Some((idx, sz, al, niche)) = sole_non_zst {
                if niche.capacity() >= other_variants {
                    let size = align_up(sz, al);
                    return UnionLayout {
                        tag_bytes: 0,
                        payload_offset: 0,
                        payload_size: sz,
                        size,
                        align: al,
                        niche_reused: true,
                        niche_member: Some(idx),
                    };
                }
            }
        }

        let tag_bytes = self.local_tag_bytes();
        let tag_align = tag_bytes.max(1);
        let align = tag_align.max(max_payload_align);

        if max_payload_size == 0 {
            let size = align_up(tag_bytes, align);
            return UnionLayout {
                tag_bytes,
                payload_offset: tag_bytes,
                payload_size: 0,
                size,
                align,
                niche_reused: false,
                niche_member: None,
            };
        }

        let payload_offset = align_up(tag_bytes, max_payload_align);
        let size = align_up(payload_offset.saturating_add(max_payload_size), align);
        UnionLayout {
            tag_bytes,
            payload_offset,
            payload_size: max_payload_size,
            size,
            align,
            niche_reused: false,
            niche_member: None,
        }
    }

    /// Computes the optimal local-tag layout from a slice of [`MemberLayout`] descriptors (order
    /// independent).
    pub fn compute_layout(&self, layouts: &[MemberLayout]) -> UnionLayout {
        self.compute_layout_with(|key| {
            layouts
                .iter()
                .find(|l| l.key == key)
                .map_or((0, 1, NicheKind::None), |l| (l.size, l.align, l.niche))
        })
    }

    /// Returns `true` iff every member of this union implements the allowlisted [`AutoTrait`].
    pub fn supports_auto_trait(
        &self,
        trait_kind: AutoTrait,
        mut member_impls: impl FnMut(Ty, AutoTrait) -> bool,
    ) -> bool {
        self.members.iter().all(|m| member_impls(m.ty, trait_kind))
    }

    /// Checks whether a trait named `trait_name` is automatically lifted for this union.
    /// Returns `false` for any trait outside [`AUTO_TRAITS`], even if every member implements it.
    pub fn supports_trait_by_name(
        &self,
        trait_name: &str,
        member_impls: impl FnMut(Ty, AutoTrait) -> bool,
    ) -> bool {
        let Some(auto_trait) = AutoTrait::from_name(trait_name) else {
            return false;
        };
        self.supports_auto_trait(auto_trait, member_impls)
    }

    /// Forbids user-written `impl` blocks on any union type ([`UnionError::ImplForbidden`]).
    #[inline]
    pub const fn check_impl_allowed(&self) -> Result<(), UnionError> {
        check_impl_allowed()
    }
}

// -------------------------------------------------------------------------------------------------
// Monomorphization-time generic unions (`E | Timeout`) & inferred private error sets (`Result<T, _>`)
// -------------------------------------------------------------------------------------------------

/// Substituted form of one member during [`ParametricUnionTy::instantiate`].
#[derive(Clone, Copy, Debug)]
pub enum InstMember<'s> {
    /// Substitutes to a single type (including `!`, which is dropped as union identity).
    Single(Ty),
    /// Substitutes to a nested union whose concrete members flatten into the enclosing union.
    Flatten(&'s [Ty]),
}

/// An unnormalized parametric union (`E | Timeout`, `A | B`) that may contain unconstrained type
/// parameters and normalizes, flattens, and deduplicates at monomorphization time.
#[derive(Debug)]
pub struct ParametricUnionTy<'a> {
    members: Vec<Ty, Alloc<'a>>,
}

impl<'a> ParametricUnionTy<'a> {
    /// Constructs a parametric union from raw member types (preserving type parameters without
    /// requiring `T != U` bounds up front; `!` is dropped immediately).
    pub fn from_members(raw: &[Ty], alloc: Alloc<'a>) -> Self {
        let mut members = Vec::with_capacity_in(raw.len(), alloc);
        for &ty in raw {
            if ty != Ty::NEVER {
                members.push(ty);
            }
        }
        Self { members }
    }

    /// Raw parametric members before instantiation.
    #[inline]
    pub fn members(&self) -> &[Ty] {
        &self.members
    }

    /// Instantiates this parametric union at monomorphization time: substitutes every member,
    /// flattens nested unions, drops `!`, deduplicates identical lifetime-erased keys, and
    /// collapses 0 -> [`NarrowResult::Empty`], 1 -> [`NarrowResult::Single`], or 2+ -> [`NarrowResult::Union`].
    pub fn instantiate<'b, 's>(
        &self,
        cx: &mut TyCx<'_>,
        mut subst: impl FnMut(Ty) -> InstMember<'s>,
        alloc: Alloc<'b>,
    ) -> Result<NarrowResult<'b>, UnionError> {
        let mut flat = Vec::with_capacity_in(self.members.len(), alloc);
        for &m in self.members.iter() {
            match subst(m) {
                InstMember::Single(ty) => {
                    if ty != Ty::NEVER {
                        flat.push(ty);
                    }
                }
                InstMember::Flatten(nested) => {
                    for &nt in nested {
                        if nt != Ty::NEVER {
                            flat.push(nt);
                        }
                    }
                }
            }
        }
        let u = UnionTy::from_members(cx, &flat, alloc)?;
        match u.members.len() {
            0 => Ok(NarrowResult::Empty),
            1 => match u.members.first() {
                Some(sole) => Ok(NarrowResult::Single(sole.ty)),
                None => Ok(NarrowResult::Empty),
            },
            _ => Ok(NarrowResult::Union(u)),
        }
    }
}

/// Accumulates error types returned by `?` / `Err(...)` / `return` sites in a private function
/// whose signature writes `Result<T, _>` ("infer inside, declare at the border").
#[derive(Debug)]
pub struct ErrorSetInfer<'a> {
    collected: Vec<Ty, Alloc<'a>>,
}

impl<'a> ErrorSetInfer<'a> {
    /// Creates an empty error-set inference accumulator.
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self { collected: Vec::new_in(alloc) }
    }

    /// Records a single error type produced by an expression (`?`, `Err(e)`).
    pub fn record_member(&mut self, ty: Ty) {
        if ty != Ty::NEVER {
            self.collected.push(ty);
        }
    }

    /// Records every member of a nested union error returned from a callee (`?` on a union).
    pub fn record_union(&mut self, u: &UnionTy<'_>) {
        for m in u.members() {
            if m.ty != Ty::NEVER {
                self.collected.push(m.ty);
            }
        }
    }

    /// Finalizes the inferred error set into `!` (`Empty`), a single error type (`Single`), or
    /// the minimal canonical [`UnionTy`].
    pub fn finish<'b>(
        &self,
        cx: &mut TyCx<'_>,
        alloc: Alloc<'b>,
    ) -> Result<NarrowResult<'b>, UnionError> {
        let u = UnionTy::from_members(cx, &self.collected, alloc)?;
        match u.members.len() {
            0 => Ok(NarrowResult::Empty),
            1 => match u.members.first() {
                Some(sole) => Ok(NarrowResult::Single(sole.ty)),
                None => Ok(NarrowResult::Empty),
            },
            _ => Ok(NarrowResult::Union(u)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{Def, IntTy, List, Mutability, Region, UintTy};

    #[test]
    fn identity_order_independence_and_lifetime_erasure_dedup() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let a = cx.tys.adt(Def(1), List::EMPTY);
        let b = cx.tys.adt(Def(2), List::EMPTY);
        let c = cx.tys.adt(Def(3), List::EMPTY);

        // 1. Order independence: A | B | C == C | A | B == B | C | A.
        let u1 = UnionTy::from_members(&mut cx, &[a, b, c], alloc).unwrap();
        let u2 = UnionTy::from_members(&mut cx, &[c, a, b], alloc).unwrap();
        let u3 = UnionTy::from_members(&mut cx, &[b, c, a], alloc).unwrap();
        assert!(u1.eq_canonical(&u2));
        assert!(u2.eq_canonical(&u3));
        assert_eq!(u1.len(), 3);

        // 2. Lifetime erasure deduplication: &'a str | &'b str collapses into ONE member.
        let re_a = cx.tys.re_early_param(0);
        let re_b = cx.tys.re_early_param(1);
        let ref_a_str = cx.tys.reference(re_a, Ty::STR, Mutability::Not);
        let ref_b_str = cx.tys.reference(re_b, Ty::STR, Mutability::Not);
        let collapsed = UnionTy::from_members(&mut cx, &[ref_a_str, ref_b_str], alloc).unwrap();
        assert_eq!(collapsed.len(), 1);
        let erased_str_ref = erase_regions(&mut cx, ref_a_str, alloc);
        assert_eq!(collapsed.members()[0].key, erased_str_ref);

        // 3. RejectDuplicate policy explicitly rejects identical erased keys (&'a str | &'b str).
        assert_eq!(
            UnionTy::from_members_with_policy(
                &mut cx,
                &[ref_a_str, ref_b_str],
                &[],
                DuplicatePolicy::RejectDuplicate,
                alloc,
            )
            .unwrap_err(),
            UnionError::DuplicateMember { key: erased_str_ref }
        );

        // 4. `!` is the identity element of union: A | ! == A.
        let with_never = UnionTy::from_members(&mut cx, &[a, Ty::NEVER], alloc).unwrap();
        let just_a = UnionTy::from_members(&mut cx, &[a], alloc).unwrap();
        assert!(with_never.eq_canonical(&just_a));
    }

    #[test]
    fn generic_param_disjoint_bounds_enforce_t_ne_u() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let t = cx.tys.param(0);
        let u = cx.tys.param(1);
        let u32_ty = Ty::uint(UintTy::U32);

        // Without `T != U`, `T | U` is rejected as potentially identical.
        assert_eq!(
            UnionTy::from_members(&mut cx, &[t, u], alloc).unwrap_err(),
            UnionError::ParamOverlap { lhs: t, rhs: u }
        );

        // Without `T != u32`, `T | u32` is also rejected.
        assert_eq!(
            UnionTy::from_members(&mut cx, &[t, u32_ty], alloc).unwrap_err(),
            UnionError::ParamOverlap { lhs: u32_ty, rhs: t }
        );

        // With `T != U` bound (symmetric: `U != T` works too), `T | U` is accepted.
        let t_ne_u = DisjointBound::new(&mut cx, u, t, alloc);
        let valid_tu = UnionTy::from_members_with_policy(
            &mut cx,
            &[t, u],
            &[t_ne_u],
            DuplicatePolicy::CollapseIdentical,
            alloc,
        )
        .unwrap();
        assert_eq!(valid_tu.len(), 2);

        // Generic ADTs with same head (`Box<T> | Box<U>`) require `T != U`.
        let list_t = cx.tys.mk_ty_list(&[t]);
        let list_u = cx.tys.mk_ty_list(&[u]);
        let box_t = cx.tys.adt(Def(10), list_t);
        let box_u = cx.tys.adt(Def(10), list_u);
        assert_eq!(
            UnionTy::from_members(&mut cx, &[box_t, box_u], alloc).unwrap_err(),
            UnionError::ParamOverlap { lhs: box_t, rhs: box_u }
        );
        let valid_boxes = UnionTy::from_members_with_policy(
            &mut cx,
            &[box_t, box_u],
            &[t_ne_u],
            DuplicatePolicy::CollapseIdentical,
            alloc,
        )
        .unwrap();
        assert_eq!(valid_boxes.len(), 2);

        // Generic ADTs with DIFFERENT heads (`Vec<T> | Option<U>`) are provably disjoint without bounds!
        let vec_t = cx.tys.adt(Def(11), list_t);
        let opt_u = cx.tys.adt(Def(12), list_u);
        let disjoint_adts = UnionTy::from_members(&mut cx, &[vec_t, opt_u], alloc).unwrap();
        assert_eq!(disjoint_adts.len(), 2);
    }

    #[test]
    fn local_tag_layout_u8_u16_and_niche_reuse() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let u8_ty = Ty::uint(UintTy::U8);
        let u32_ty = Ty::uint(UintTy::U32);
        let i64_ty = Ty::int(IntTy::I64);
        let ref_str = cx.tys.reference(Region::STATIC, Ty::STR, Mutability::Not);
        let ref_erased = erase_regions(&mut cx, ref_str, alloc);

        // 1. Standard 3-member union: u8 tag (1 byte), align 8, payload_offset 8, total size 16.
        let u = UnionTy::from_members(&mut cx, &[u8_ty, u32_ty, i64_ty], alloc).unwrap();
        let lay = u.compute_layout(&[
            MemberLayout { key: u8_ty, size: 1, align: 1, niche: NicheKind::None },
            MemberLayout { key: u32_ty, size: 4, align: 4, niche: NicheKind::None },
            MemberLayout { key: i64_ty, size: 8, align: 8, niche: NicheKind::None },
        ]);
        assert_eq!(lay.tag_bytes, 1);
        assert_eq!(lay.payload_offset, 8);
        assert_eq!(lay.payload_size, 8);
        assert_eq!(lay.align, 8);
        assert_eq!(lay.size, 16);
        assert!(!lay.niche_reused);
        assert_eq!(lay.niche_member, None);

        // 2. Niche reuse with `bool | () | UnitB`: 254 niche values absorb 2 ZST variants -> 1 byte total!
        let unit_b = cx.tys.adt(Def(99), List::EMPTY);
        let bool_union = UnionTy::from_members(&mut cx, &[Ty::BOOL, Ty::UNIT, unit_b], alloc).unwrap();
        let bool_lay = bool_union.compute_layout(&[
            MemberLayout { key: Ty::BOOL, size: 1, align: 1, niche: niche_for_ty(&cx.tys, Ty::BOOL) },
            MemberLayout { key: Ty::UNIT, size: 0, align: 1, niche: NicheKind::None },
            MemberLayout { key: unit_b, size: 0, align: 1, niche: NicheKind::None },
        ]);
        assert!(bool_lay.niche_reused);
        assert_eq!(bool_lay.tag_bytes, 0);
        assert_eq!(bool_lay.payload_offset, 0);
        assert_eq!(bool_lay.size, 1);
        assert_eq!(bool_lay.align, 1);

        // 3. Niche reuse with `&str | ()` (NonZero niche): 16 bytes instead of 24!
        let opt_str = UnionTy::from_members(&mut cx, &[ref_str, Ty::UNIT], alloc).unwrap();
        let opt_lay = opt_str.compute_layout(&[
            MemberLayout { key: ref_erased, size: 16, align: 8, niche: niche_for_ty(&cx.tys, ref_erased) },
            MemberLayout { key: Ty::UNIT, size: 0, align: 1, niche: NicheKind::None },
        ]);
        assert!(opt_lay.niche_reused);
        assert_eq!(opt_lay.tag_bytes, 0);
        assert_eq!(opt_lay.size, 16);
        assert_eq!(opt_lay.align, 8);

        // 4. Tag width threshold: <= 256 members => u8 (1 byte), 257 members => u16 (2 bytes).
        assert_eq!(UnionTy::local_tag_bytes_for_count(1), 0);
        assert_eq!(UnionTy::local_tag_bytes_for_count(2), 1);
        assert_eq!(UnionTy::local_tag_bytes_for_count(256), 1);
        assert_eq!(UnionTy::local_tag_bytes_for_count(257), 2);

        let mut many = Vec::with_capacity_in(257, alloc);
        for id in 1000..1257u32 {
            many.push(cx.tys.adt(Def(id), List::EMPTY));
        }
        let big = UnionTy::from_members(&mut cx, &many, alloc).unwrap();
        assert_eq!(big.len(), 257);
        assert_eq!(big.local_tag_bytes(), 2);
        let big_lay = big.compute_layout_with(|_| (4, 4, NicheKind::None));
        assert_eq!(big_lay.tag_bytes, 2);
        assert_eq!(big_lay.payload_offset, 4);
        assert_eq!(big_lay.size, 8);
    }

    #[test]
    fn widening_retag_map_and_ref_widening_rejection() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let a = cx.tys.adt(Def(1), List::EMPTY);
        let b = cx.tys.adt(Def(2), List::EMPTY);
        let c = cx.tys.adt(Def(3), List::EMPTY);
        let d = cx.tys.adt(Def(4), List::EMPTY);

        let ac = UnionTy::from_members(&mut cx, &[a, c], alloc).unwrap();
        let abc = UnionTy::from_members(&mut cx, &[c, b, a], alloc).unwrap();
        let ca = UnionTy::from_members(&mut cx, &[c, a], alloc).unwrap();
        let ad = UnionTy::from_members(&mut cx, &[a, d], alloc).unwrap();

        // By-value widening `A | C` -> `A | B | C`: tag 0 (A) -> 0, tag 1 (C) -> 2.
        assert_eq!(ac.can_widen(&abc), Ok(()));
        let map = ac.widen_tag_map(&abc, alloc).unwrap();
        assert_eq!(map.as_slice(), &[0, 2]);
        assert_eq!(abc.members()[usize::from(map[0])].key, ac.members()[0].key);
        assert_eq!(abc.members()[usize::from(map[1])].key, ac.members()[1].key);

        // Reference widening `&(A | C)` -> `&(A | B | C)` is FORBIDDEN.
        assert_eq!(
            ac.can_widen_in_mode(&abc, WidenMode::ByRef(Mutability::Not)),
            Err(UnionError::RefCannotWiden)
        );
        assert_eq!(
            ac.widen_tag_map_in_mode(&abc, WidenMode::ByRef(Mutability::Mut), alloc)
                .unwrap_err(),
            UnionError::RefCannotWiden
        );

        // Same canonical union behind a reference (`&(A | C)` == `&(C | A)`) is identity (allowed).
        assert_eq!(
            ac.can_widen_in_mode(&ca, WidenMode::ByRef(Mutability::Not)),
            Ok(())
        );

        // Non-subset widening (`A | D` -> `A | B | C`) fails with `NotSubset { missing_member: D }`.
        assert_eq!(
            ad.can_widen(&abc),
            Err(UnionError::NotSubset { missing_member: d })
        );
        assert_eq!(
            ad.widen_tag_map(&abc, alloc).unwrap_err(),
            UnionError::NotSubset { missing_member: d }
        );
    }

    #[test]
    fn narrowing_collapses_union_to_union_single_and_empty() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let io_err = cx.tys.adt(Def(1), List::EMPTY);
        let parse_err = cx.tys.adt(Def(2), List::EMPTY);
        let timeout_err = cx.tys.adt(Def(3), List::EMPTY);
        let other_err = cx.tys.adt(Def(4), List::EMPTY);

        let u3 = UnionTy::from_members(&mut cx, &[io_err, parse_err, timeout_err], alloc).unwrap();

        // Removing an absent member returns `UnknownMember`.
        assert_eq!(
            u3.narrow_remove(&mut cx, other_err, alloc).unwrap_err(),
            UnionError::UnknownMember { key: other_err }
        );

        // 3 -> 2: `Io | Parse | Timeout` \ {Io} => `Parse | Timeout`.
        let NarrowResult::Union(u2) = u3.narrow_remove(&mut cx, io_err, alloc).unwrap() else {
            panic!("expected narrowed 2-member union");
        };
        let expected_u2 = UnionTy::from_members(&mut cx, &[parse_err, timeout_err], alloc).unwrap();
        assert!(u2.eq_canonical(&expected_u2));

        // 2 -> 1: `Parse | Timeout` \ {Parse} => `Single(Timeout)`.
        let NarrowResult::Single(sole) = u2.narrow_remove(&mut cx, parse_err, alloc).unwrap() else {
            panic!("expected Single(timeout_err)");
        };
        assert_eq!(sole, timeout_err);

        // 1 -> 0: `Timeout` \ {Timeout} => `Empty`.
        let u1 = UnionTy::from_members(&mut cx, &[timeout_err], alloc).unwrap();
        assert!(matches!(
            u1.narrow_remove(&mut cx, timeout_err, alloc).unwrap(),
            NarrowResult::Empty
        ));
    }

    #[test]
    fn fixed_auto_traits_impl_forbidden_and_no_implicit_inference() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let a = cx.tys.adt(Def(1), List::EMPTY);
        let b = cx.tys.adt(Def(2), List::EMPTY);
        let u = UnionTy::from_members(&mut cx, &[a, b], alloc).unwrap();

        // 1. Exact fixed allowlist of 11 traits.
        assert_eq!(AUTO_TRAITS.len(), 11);
        for name in [
            "Debug", "Display", "Error", "Clone", "Copy", "PartialEq", "Eq", "Hash", "Send",
            "Sync", "Unpin",
        ] {
            assert!(AutoTrait::is_allowed_name(name));
            assert!(u.supports_trait_by_name(name, |_, _| true));
        }

        // Non-allowlisted traits are NEVER lifted onto unions, even if every member implements them.
        for forbidden in ["Default", "Ord", "PartialOrd", "Serialize", "Deserialize", "Iterator"] {
            assert!(!AutoTrait::is_allowed_name(forbidden));
            assert!(!u.supports_trait_by_name(forbidden, |_, _| true));
        }

        // If one member lacks a trait (e.g. `b` is not `Copy`), the union does NOT implement it.
        assert!(!u.supports_auto_trait(AutoTrait::Copy, |ty, tr| !(ty == b && tr == AutoTrait::Copy)));
        assert!(u.supports_auto_trait(AutoTrait::Debug, |ty, tr| !(ty == b && tr == AutoTrait::Copy)));

        // 2. User-written `impl` blocks on unions are strictly forbidden.
        assert_eq!(check_impl_allowed(), Err(UnionError::ImplForbidden));
        assert_eq!(u.check_impl_allowed(), Err(UnionError::ImplForbidden));

        // 3. No implicit union inference for unannotated heterogeneous branches.
        let u8_ty = Ty::uint(UintTy::U8);
        let str_ref = cx.tys.reference(Region::STATIC, Ty::STR, Mutability::Not);
        assert_eq!(
            infer_branch_union(u8_ty, str_ref),
            Err(UnionError::NoImplicitUnion { lhs: u8_ty, rhs: str_ref })
        );
        assert_eq!(
            check_branch_against_expected(&mut cx, u8_ty, str_ref, None, alloc),
            Err(UnionError::NoImplicitUnion { lhs: u8_ty, rhs: str_ref })
        );

        // Homogeneous or diverging branches still typecheck without a union.
        assert_eq!(infer_branch_union(u8_ty, u8_ty), Ok(u8_ty));
        assert_eq!(infer_branch_union(u8_ty, Ty::NEVER), Ok(u8_ty));
        assert_eq!(infer_branch_union(Ty::NEVER, str_ref), Ok(str_ref));

        // Explicitly annotated union type accepts both branches and maps them to local tags.
        let annotated = UnionTy::from_members(&mut cx, &[u8_ty, str_ref], alloc).unwrap();
        let checked = check_branch_against_expected(&mut cx, u8_ty, str_ref, Some(&annotated), alloc).unwrap();
        assert_eq!(
            checked,
            BranchCheck::AnnotatedUnion {
                lhs_tag: annotated.tag_of(&mut cx, u8_ty, alloc),
                rhs_tag: annotated.tag_of(&mut cx, str_ref, alloc),
            }
        );
    }

    #[test]
    fn residual_narrowing_parametric_monomorphization_inferred_set_and_20_members() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut cx = TyCx::new_in(alloc);

        let io_err = cx.tys.adt(Def(10), List::EMPTY);
        let parse_err = cx.tys.adt(Def(11), List::EMPTY);
        let timeout_err = cx.tys.adt(Def(12), List::EMPTY);
        let stopped_err = cx.tys.adt(Def(13), List::EMPTY);
        let param_e = cx.tys.param(0);

        // 1. Multi-member residual narrowing: handle Io + Parse, forward remaining `Timeout | Stopped`.
        let u4 = UnionTy::from_members(&mut cx, &[io_err, parse_err, timeout_err, stopped_err], alloc).unwrap();
        let NarrowResult::Union(residual) =
            u4.narrow_remove_many(&mut cx, &[io_err, parse_err], alloc).unwrap()
        else {
            panic!("expected 2-member residual union");
        };
        let caller_ret = UnionTy::from_members(&mut cx, &[timeout_err, stopped_err], alloc).unwrap();
        assert!(residual.eq_canonical(&caller_ret));
        assert!(residual.can_widen(&caller_ret).is_ok());

        // Redundant arm is rejected.
        assert_eq!(
            u4.narrow_remove_many(&mut cx, &[io_err, io_err], alloc).unwrap_err(),
            UnionError::RedundantArm { key: io_err }
        );

        // 2. Parametric union `E | Timeout` instantiated at monomorphization time:
        let poly = ParametricUnionTy::from_members(&[param_e, timeout_err], alloc);
        // 2a. E = Timeout -> collapses to Single(Timeout)!
        let NarrowResult::Single(collapsed) = poly
            .instantiate(
                &mut cx,
                |t| if t == param_e { InstMember::Single(timeout_err) } else { InstMember::Single(t) },
                alloc,
            )
            .unwrap()
        else {
            panic!("expected Single(Timeout)");
        };
        assert_eq!(collapsed, timeout_err);

        // 2b. E = (Io | Parse) -> flattens into `Io | Parse | Timeout`!
        let inner = [io_err, parse_err];
        let NarrowResult::Union(flattened) = poly
            .instantiate(
                &mut cx,
                |t| if t == param_e { InstMember::Flatten(&inner) } else { InstMember::Single(t) },
                alloc,
            )
            .unwrap()
        else {
            panic!("expected 3-member flattened union");
        };
        let expected_flat = UnionTy::from_members(&mut cx, &[io_err, parse_err, timeout_err], alloc).unwrap();
        assert!(flattened.eq_canonical(&expected_flat));

        // 3. Inferred private error set (`Result<T, _>`):
        let mut infer = ErrorSetInfer::new(alloc);
        infer.record_member(io_err);
        infer.record_union(&caller_ret);
        infer.record_member(io_err); // duplicate collapsed
        let NarrowResult::Union(inferred) = infer.finish(&mut cx, alloc).unwrap() else {
            panic!("expected 3-member inferred error set");
        };
        let expected_inf = UnionTy::from_members(&mut cx, &[io_err, timeout_err, stopped_err], alloc).unwrap();
        assert!(inferred.eq_canonical(&expected_inf));

        // 4. Arbitrary arity (> 12 members): 20-member union with u8 local tag & widening.
        let mut twenty = Vec::with_capacity_in(20, alloc);
        for d in 100..120u32 {
            twenty.push(cx.tys.adt(Def(d), List::EMPTY));
        }
        let u20 = UnionTy::from_members(&mut cx, &twenty, alloc).unwrap();
        assert_eq!(u20.len(), 20);
        assert_eq!(u20.local_tag_bytes(), 1);
        let sub = UnionTy::from_members(&mut cx, &[twenty[3], twenty[17]], alloc).unwrap();
        let tag_map = sub.widen_tag_map(&u20, alloc).unwrap();
        assert_eq!(tag_map.as_slice(), &[3, 17]);
    }
}
