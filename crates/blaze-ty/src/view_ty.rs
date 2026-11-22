//! First-class View Types & Partial-Borrow Algebra for Blaze (`docs/v2/plan/design/edition.md` §3,
//! objective E4).
//!
//! # Semantics
//! A **view type** restricts a reference to a named subset of a struct's fields with per-field
//! sharing/uniqueness permissions — **without changing layout or ABI** (the callee still receives
//! the single whole-struct pointer):
//!
//! - `&self.{a, b}` — shared access to fields `a` and `b` only.
//! - `&mut self.{a}` (or `&uniq self.{a}`) — unique access to field `a` only.
//! - `&self.{config, mut cache}` — mixed view: shared access to `config` and unique access to
//!   `cache` through one pointer!
//! - `&self.{.. - a}` — complement view: all fields except `a`.
//! - `&self.{}` — empty view: borrows no fields (conflicts with nothing).
//!
//! # Soundness Rules
//! 1. **Permission lattice**: `Shared < Unique`. A `Unique` field can be narrowed (reborrowed) to
//!    `Shared`, never the reverse.
//! 2. **Disjointness**: two simultaneous views `v1` and `v2` of the same struct **do not conflict**
//!    iff for every field `f` in `v1 ∩ v2`, `v1[f] == Shared && v2[f] == Shared`.
//!    Thus `&self.{config, mut cache}` and `&self.{config, mut metrics}` can be active
//!    **simultaneously** because `config` is shared in both and `cache != metrics`.
//! 3. **Subtyping / Reborrowing**: `narrow` is a valid subview of `wide` iff every field required
//!    by `narrow` is present in `wide` with equal or stronger permission (`wide[f] >= narrow[f]`).
//! 4. **Visibility rule (`edition.md` §3)**: private methods infer their minimal view from their
//!    body and callees (fixed-point over intra-crate SCCs); `pub` methods declare their view
//!    explicitly and are checked so the body never exceeds the declared contract.
//! 5. **ABI unchanged**: every view reference has the exact representation of `&Self` / `&mut Self`
//!    (1 pointer, zero wrapper allocation).

use core::cmp::Ordering;

use blaze_own::{AVec, Alloc};

use crate::ids::{Def, Mutability, Region};

/// 0-based field index of a struct (aligned with `blaze-flow`'s `Proj::Field`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldId(pub u32);

/// Access permission granted to one field in a view.
///
/// Forms a two-point lattice `Shared < Unique`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FieldPerm {
    /// Shared (`&`) access: readable concurrently with other `Shared` views.
    Shared = 0,
    /// Unique (`&mut` / `&uniq`) access: exclusive read/write, conflicts with any other access
    /// to the same field.
    Unique = 1,
}

impl FieldPerm {
    /// Convert from `blaze_ty::Mutability`.
    #[inline]
    pub const fn from_mutability(m: Mutability) -> Self {
        match m {
            Mutability::Not => Self::Shared,
            Mutability::Mut => Self::Unique,
        }
    }

    /// Convert to `blaze_ty::Mutability`.
    #[inline]
    pub const fn to_mutability(self) -> Mutability {
        match self {
            Self::Shared => Mutability::Not,
            Self::Unique => Mutability::Mut,
        }
    }

    /// Least upper bound in the permission lattice (`Unique` dominates `Shared`).
    #[inline]
    pub const fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Shared, Self::Shared) => Self::Shared,
            _ => Self::Unique,
        }
    }

    /// Greatest lower bound in the permission lattice (`Shared` dominates `Unique`).
    #[inline]
    pub const fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unique, Self::Unique) => Self::Unique,
            _ => Self::Shared,
        }
    }

    /// Returns `true` iff having permission `self` is sufficient to satisfy a requirement of `need`.
    #[inline]
    pub const fn satisfies(self, need: Self) -> bool {
        match (self, need) {
            (Self::Unique, _) | (Self::Shared, Self::Shared) => true,
            (Self::Shared, Self::Unique) => false,
        }
    }

    /// Returns `true` iff simultaneous access with permissions `self` and `other` to the SAME field
    /// violates Rust's aliasing rules (XOR mutability).
    #[inline]
    pub const fn conflicts(self, other: Self) -> bool {
        matches!((self, other), (Self::Unique, _) | (_, Self::Unique))
    }
}

/// One field entry inside a canonical [`ViewSpec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ViewEntry {
    pub field: FieldId,
    pub perm: FieldPerm,
}

impl ViewEntry {
    #[inline]
    pub const fn shared(field: u32) -> Self {
        Self { field: FieldId(field), perm: FieldPerm::Shared }
    }

    #[inline]
    pub const fn unique(field: u32) -> Self {
        Self { field: FieldId(field), perm: FieldPerm::Unique }
    }
}

/// Canonical specification of which fields of a struct a view borrows and with what permission.
#[derive(Debug, PartialEq, Eq)]
pub enum ViewSpec<'a> {
    /// `&self.{}` — borrows no fields at all (disjoint from every other view).
    Empty,
    /// Sorted, deduplicated list of `(FieldId, FieldPerm)` entries (non-empty).
    Fields(AVec<'a, ViewEntry>),
    /// Un-narrowed whole-struct borrow (`&Self` or `&mut Self`).
    Whole(FieldPerm),
}

/// Conflict detected when two simultaneous views violate aliasing rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewConflict {
    /// Conflicting field, or `None` when two whole-struct borrows conflict across all fields.
    pub field: Option<FieldId>,
    pub existing: FieldPerm,
    pub requested: FieldPerm,
}

/// Typed view-checking error for subview narrowing, field access, or `pub` signature verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewError {
    /// Attempted to access or require a field not present in the available view.
    FieldOutsideView { field: FieldId, required: FieldPerm },
    /// Attempted to uniquely borrow / mutate a field that is only `Shared` in the available view.
    MutatingSharedField { field: FieldId },
    /// Attempted to coerce a partial view into a `Whole` borrow.
    PartialCannotSatisfyWhole { required: FieldPerm },
    /// Attempted to coerce `Whole(Shared)` into `Whole(Unique)`.
    WholeSharedCannotSatisfyUnique,
    /// `pub` method body accesses fields outside its declared view contract.
    BodyExceedsDeclaredView { field: Option<FieldId>, required: FieldPerm },
}

/// ABI proof record confirming a view reference uses the unchanged single-pointer ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewAbi {
    /// Effective pointer mutability passed at the machine ABI level (`Mut` if any field is `Unique`).
    pub pointer_mutability: Mutability,
    /// Always `1`: layout and ABI are identical to `&Self` / `&mut Self` (`edition.md` §3).
    pub pointer_words: u8,
}

impl<'a> ViewSpec<'a> {
    /// Construct a canonical [`ViewSpec`] from an arbitrary slice of field entries.
    ///
    /// Order-independent and deduplicating: duplicate fields merge via [`FieldPerm::join`]
    /// (so `{a, mut a}` canonicalizes to `{mut a}`).
    pub fn from_entries(entries: &[ViewEntry], alloc: Alloc<'a>) -> Self {
        if entries.is_empty() {
            return Self::Empty;
        }
        let mut sorted: AVec<'a, ViewEntry> = AVec::with_capacity_in(entries.len(), alloc);
        for &e in entries {
            sorted.push(e);
        }
        sorted.as_mut_slice().sort_by_key(|e| (e.field, e.perm));

        let mut dedup: AVec<'a, ViewEntry> = AVec::with_capacity_in(sorted.len(), alloc);
        for &e in sorted.as_slice() {
            if let Some(last) = dedup.as_mut_slice().last_mut()
                && last.field == e.field
            {
                last.perm = last.perm.join(e.perm);
                continue;
            }
            dedup.push(e);
        }
        Self::Fields(dedup)
    }

    /// Construct a complement view (`&self.{.. - excluded}`) covering all fields `0..total_fields`
    /// not listed in `excluded`, each with permission `perm`.
    pub fn from_complement(
        total_fields: u32,
        excluded: &[FieldId],
        perm: FieldPerm,
        alloc: Alloc<'a>,
    ) -> Self {
        let mut out = AVec::new_in(alloc);
        for idx in 0..total_fields {
            let f = FieldId(idx);
            if !excluded.contains(&f) {
                out.push(ViewEntry { field: f, perm });
            }
        }
        if out.is_empty() { Self::Empty } else { Self::Fields(out) }
    }

    /// Clone this `ViewSpec` into `alloc`.
    pub fn clone_in<'b>(&self, alloc: Alloc<'b>) -> ViewSpec<'b> {
        match self {
            Self::Empty => ViewSpec::Empty,
            Self::Whole(p) => ViewSpec::Whole(*p),
            Self::Fields(fs) => {
                let mut v = AVec::with_capacity_in(fs.len(), alloc);
                for &e in fs.as_slice() {
                    v.push(e);
                }
                ViewSpec::Fields(v)
            }
        }
    }

    /// Canonical field entries slice (empty for `Empty` and `Whole`).
    #[inline]
    pub fn entries(&self) -> &[ViewEntry] {
        match self {
            Self::Fields(fs) => fs.as_slice(),
            Self::Empty | Self::Whole(_) => &[],
        }
    }

    /// Returns the permission granted to `field` by this view, or `None` if excluded.
    pub fn perm_of(&self, field: FieldId) -> Option<FieldPerm> {
        match self {
            Self::Empty => None,
            Self::Whole(p) => Some(*p),
            Self::Fields(fs) => fs
                .as_slice()
                .binary_search_by_key(&field, |e| e.field)
                .ok()
                .map(|i| fs[i].perm),
        }
    }

    /// Returns `true` iff any field in this view requires unique (`&mut` / `&uniq`) access.
    pub fn has_unique(&self) -> bool {
        match self {
            Self::Empty => false,
            Self::Whole(p) => *p == FieldPerm::Unique,
            Self::Fields(fs) => fs.as_slice().iter().any(|e| e.perm == FieldPerm::Unique),
        }
    }

    /// Machine ABI of a reference carrying this view: always 1 pointer (`&Self` or `&mut Self`).
    pub fn abi(&self) -> ViewAbi {
        ViewAbi {
            pointer_mutability: if self.has_unique() { Mutability::Mut } else { Mutability::Not },
            pointer_words: 1,
        }
    }

    /// Least upper bound (`self ∪ other`): union of fields, joining permissions on overlap.
    pub fn union(&self, other: &ViewSpec<'_>, alloc: Alloc<'a>) -> Self {
        match (self, other) {
            (ViewSpec::Empty, x) => x.clone_in(alloc),
            (x, ViewSpec::Empty) => x.clone_in(alloc),
            (ViewSpec::Whole(a), ViewSpec::Whole(b)) => ViewSpec::Whole(a.join(*b)),
            (ViewSpec::Whole(FieldPerm::Unique), _) | (_, ViewSpec::Whole(FieldPerm::Unique)) => {
                ViewSpec::Whole(FieldPerm::Unique)
            }
            (ViewSpec::Whole(FieldPerm::Shared), ViewSpec::Fields(fs)) => {
                if fs.as_slice().iter().any(|e| e.perm == FieldPerm::Unique) {
                    ViewSpec::Whole(FieldPerm::Unique)
                } else {
                    ViewSpec::Whole(FieldPerm::Shared)
                }
            }
            (ViewSpec::Fields(fs), ViewSpec::Whole(FieldPerm::Shared)) => {
                if fs.as_slice().iter().any(|e| e.perm == FieldPerm::Unique) {
                    ViewSpec::Whole(FieldPerm::Unique)
                } else {
                    ViewSpec::Whole(FieldPerm::Shared)
                }
            }
            (ViewSpec::Fields(a), ViewSpec::Fields(b)) => {
                let mut out = AVec::with_capacity_in(a.len() + b.len(), alloc);
                let (mut i, mut j) = (0usize, 0usize);
                let (sa, sb) = (a.as_slice(), b.as_slice());
                while i < sa.len() && j < sb.len() {
                    match sa[i].field.cmp(&sb[j].field) {
                        Ordering::Less => {
                            out.push(sa[i]);
                            i += 1;
                        }
                        Ordering::Greater => {
                            out.push(sb[j]);
                            j += 1;
                        }
                        Ordering::Equal => {
                            out.push(ViewEntry {
                                field: sa[i].field,
                                perm: sa[i].perm.join(sb[j].perm),
                            });
                            i += 1;
                            j += 1;
                        }
                    }
                }
                while i < sa.len() {
                    out.push(sa[i]);
                    i += 1;
                }
                while j < sb.len() {
                    out.push(sb[j]);
                    j += 1;
                }
                if out.is_empty() { Self::Empty } else { Self::Fields(out) }
            }
        }
    }

    /// Greatest lower bound (`self ∩ other`): fields present in both with `meet` of permissions.
    pub fn intersect(&self, other: &ViewSpec<'_>, alloc: Alloc<'a>) -> Self {
        match (self, other) {
            (ViewSpec::Empty, _) | (_, ViewSpec::Empty) => ViewSpec::Empty,
            (ViewSpec::Whole(a), ViewSpec::Whole(b)) => ViewSpec::Whole(a.meet(*b)),
            (ViewSpec::Whole(wp), ViewSpec::Fields(fs)) => {
                let mut out = AVec::with_capacity_in(fs.len(), alloc);
                for &e in fs.as_slice() {
                    out.push(ViewEntry { field: e.field, perm: e.perm.meet(*wp) });
                }
                if out.is_empty() { ViewSpec::Empty } else { ViewSpec::Fields(out) }
            }
            (ViewSpec::Fields(fs), ViewSpec::Whole(wp)) => {
                let mut out = AVec::with_capacity_in(fs.len(), alloc);
                for &e in fs.as_slice() {
                    out.push(ViewEntry { field: e.field, perm: e.perm.meet(*wp) });
                }
                if out.is_empty() { ViewSpec::Empty } else { ViewSpec::Fields(out) }
            }
            (ViewSpec::Fields(a), ViewSpec::Fields(b)) => {
                let mut out = AVec::new_in(alloc);
                let (mut i, mut j) = (0usize, 0usize);
                let (sa, sb) = (a.as_slice(), b.as_slice());
                while i < sa.len() && j < sb.len() {
                    match sa[i].field.cmp(&sb[j].field) {
                        Ordering::Less => i += 1,
                        Ordering::Greater => j += 1,
                        Ordering::Equal => {
                            out.push(ViewEntry {
                                field: sa[i].field,
                                perm: sa[i].perm.meet(sb[j].perm),
                            });
                            i += 1;
                            j += 1;
                        }
                    }
                }
                if out.is_empty() { Self::Empty } else { Self::Fields(out) }
            }
        }
    }

    /// Remaining fields in `self` not exclusively borrowed by `taken` (when `self` is concrete
    /// `Fields` or expanded against `total_fields`).
    ///
    /// Note: if `taken` only borrows field `f` as `Shared` and `self` holds `f`, `f` remains
    /// available as `Shared` in the residual view! If `taken` borrows `f` as `Unique`, `f` is
    /// removed completely from the residual view.
    pub fn residual_after(
        &self,
        taken: &ViewSpec<'_>,
        total_fields: u32,
        alloc: Alloc<'a>,
    ) -> Self {
        let mut out = AVec::new_in(alloc);
        match self {
            Self::Empty => return Self::Empty,
            Self::Whole(wp) => {
                for idx in 0..total_fields {
                    let f = FieldId(idx);
                    match taken.perm_of(f) {
                        None => out.push(ViewEntry { field: f, perm: *wp }),
                        Some(FieldPerm::Shared) => {
                            out.push(ViewEntry { field: f, perm: FieldPerm::Shared })
                        }
                        Some(FieldPerm::Unique) => {}
                    }
                }
            }
            Self::Fields(fs) => {
                for &e in fs.as_slice() {
                    match taken.perm_of(e.field) {
                        None => out.push(e),
                        Some(FieldPerm::Shared) => {
                            out.push(ViewEntry { field: e.field, perm: FieldPerm::Shared })
                        }
                        Some(FieldPerm::Unique) => {}
                    }
                }
            }
        }
        if out.is_empty() { Self::Empty } else { Self::Fields(out) }
    }

    /// Check whether `self` (required/narrower view) is a sound subview of `wider` (available view).
    ///
    /// `narrow.subview_of(wide)` holds iff every field required by `narrow` is available in `wide`
    /// with permission `>=` what `narrow` demands.
    pub fn subview_of(&self, wider: &ViewSpec<'_>) -> Result<(), ViewError> {
        match (self, wider) {
            (ViewSpec::Empty, _) => Ok(()),
            (ViewSpec::Whole(need), ViewSpec::Whole(have)) => {
                if have.satisfies(*need) {
                    Ok(())
                } else {
                    Err(ViewError::WholeSharedCannotSatisfyUnique)
                }
            }
            (ViewSpec::Whole(need), ViewSpec::Empty | ViewSpec::Fields(_)) => {
                Err(ViewError::PartialCannotSatisfyWhole { required: *need })
            }
            (ViewSpec::Fields(req), ViewSpec::Whole(have)) => {
                for &e in req.as_slice() {
                    if !have.satisfies(e.perm) {
                        return Err(ViewError::MutatingSharedField { field: e.field });
                    }
                }
                Ok(())
            }
            (ViewSpec::Fields(req), ViewSpec::Empty) => {
                let first = req[0];
                Err(ViewError::FieldOutsideView { field: first.field, required: first.perm })
            }
            (ViewSpec::Fields(req), ViewSpec::Fields(_)) => {
                for &e in req.as_slice() {
                    match wider.perm_of(e.field) {
                        None => {
                            return Err(ViewError::FieldOutsideView {
                                field: e.field,
                                required: e.perm,
                            });
                        }
                        Some(have) if !have.satisfies(e.perm) => {
                            return Err(ViewError::MutatingSharedField { field: e.field });
                        }
                        Some(_) => {}
                    }
                }
                Ok(())
            }
        }
    }

    /// Check whether `self` and `other` can coexist simultaneously without violating aliasing.
    ///
    /// Returns `None` if compatible (disjoint on all unique fields, shared overlaps permitted),
    /// or `Some(ViewConflict)` naming the exact conflicting field.
    pub fn conflicts_with(&self, other: &ViewSpec<'_>) -> Option<ViewConflict> {
        match (self, other) {
            (ViewSpec::Empty, _) | (_, ViewSpec::Empty) => None,
            (ViewSpec::Whole(a), ViewSpec::Whole(b)) => {
                if a.conflicts(*b) {
                    Some(ViewConflict { field: None, existing: *a, requested: *b })
                } else {
                    None
                }
            }
            (ViewSpec::Whole(a), ViewSpec::Fields(fs)) => {
                for &e in fs.as_slice() {
                    if a.conflicts(e.perm) {
                        return Some(ViewConflict {
                            field: Some(e.field),
                            existing: *a,
                            requested: e.perm,
                        });
                    }
                }
                None
            }
            (ViewSpec::Fields(fs), ViewSpec::Whole(b)) => {
                for &e in fs.as_slice() {
                    if e.perm.conflicts(*b) {
                        return Some(ViewConflict {
                            field: Some(e.field),
                            existing: e.perm,
                            requested: *b,
                        });
                    }
                }
                None
            }
            (ViewSpec::Fields(a), ViewSpec::Fields(b)) => {
                let (mut i, mut j) = (0usize, 0usize);
                let (sa, sb) = (a.as_slice(), b.as_slice());
                while i < sa.len() && j < sb.len() {
                    match sa[i].field.cmp(&sb[j].field) {
                        Ordering::Less => i += 1,
                        Ordering::Greater => j += 1,
                        Ordering::Equal => {
                            if sa[i].perm.conflicts(sb[j].perm) {
                                return Some(ViewConflict {
                                    field: Some(sa[i].field),
                                    existing: sa[i].perm,
                                    requested: sb[j].perm,
                                });
                            }
                            i += 1;
                            j += 1;
                        }
                    }
                }
                None
            }
        }
    }

    /// Check whether a single direct field access `(field, perm)` is legal inside this view.
    pub fn check_field_access(&self, field: FieldId, required: FieldPerm) -> Result<(), ViewError> {
        match self.perm_of(field) {
            None => Err(ViewError::FieldOutsideView { field, required }),
            Some(have) if !have.satisfies(required) => {
                Err(ViewError::MutatingSharedField { field })
            }
            Some(_) => Ok(()),
        }
    }

    /// Verify that all views in `simultaneous` are pairwise conflict-free (e.g. a multi-way
    /// simultaneous view split).
    pub fn check_simultaneous_borrows(simultaneous: &[&ViewSpec<'_>]) -> Result<(), ViewConflict> {
        for i in 0..simultaneous.len() {
            for j in (i + 1)..simultaneous.len() {
                if let Some(conflict) = simultaneous[i].conflicts_with(simultaneous[j]) {
                    return Err(conflict);
                }
            }
        }
        Ok(())
    }
}

/// Complete view reference type `&'r {fields} BaseDef`.
#[derive(Debug, PartialEq, Eq)]
pub struct ViewType<'a> {
    pub base: Def,
    pub region: Region,
    pub spec: ViewSpec<'a>,
}

impl<'a> ViewType<'a> {
    #[inline]
    pub fn new(base: Def, region: Region, spec: ViewSpec<'a>) -> Self {
        Self { base, region, spec }
    }

    /// Machine-level ABI is always identical to `&Self` / `&mut Self` (single pointer).
    #[inline]
    pub fn abi(&self) -> ViewAbi {
        self.spec.abi()
    }
}

// =================================================================================================
// Intra-Crate Body View Inference & `pub` Contract Verification (`edition.md` §3)
// =================================================================================================

/// Visibility of a method for view inference (`edition.md` §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodVis {
    /// Private method: minimal view is inferred from body + callees (including recursive SCCs).
    Private,
    /// Public method: must declare its view explicitly; body must be a subview of the declaration.
    Public,
}

/// Summary of one method's receiver accesses and same-receiver calls for view inference/checking.
pub struct MethodSummary<'a> {
    pub vis: MethodVis,
    /// Declared view (`Some` required for `Public`; optional annotation for `Private`).
    pub declared: Option<ViewSpec<'a>>,
    /// Direct field reads/writes on `self` in the method body.
    pub direct_accesses: AVec<'a, ViewEntry>,
    /// Whether the body passes `self` as a whole `&Self`/`&mut Self` to an un-viewed function.
    pub whole_escape: Option<FieldPerm>,
    /// Indices of other methods in the same `impl` called on `self`.
    pub self_callees: AVec<'a, usize>,
}

/// Result of inferring and checking views across a set of methods on one struct.
pub struct InferredViews<'a> {
    /// Effective view of each method (inferred for private, declared for `pub`).
    pub views: AVec<'a, ViewSpec<'a>>,
}

/// Infer minimal views for all private methods via deterministic Kleene fixed-point iteration
/// (soundly handling arbitrary and mutually recursive call graphs!), and verify every declared
/// view (especially all `pub` methods) against the body's actual accesses.
pub fn infer_and_check_methods<'a>(
    methods: &[MethodSummary<'a>],
    alloc: Alloc<'a>,
) -> Result<InferredViews<'a>, (usize, ViewError)> {
    let n = methods.len();
    let mut current: AVec<'a, ViewSpec<'a>> = AVec::with_capacity_in(n, alloc);
    let mut body_base: AVec<'a, ViewSpec<'a>> = AVec::with_capacity_in(n, alloc);

    for m in methods {
        let mut base = ViewSpec::from_entries(m.direct_accesses.as_slice(), alloc);
        if let Some(wp) = m.whole_escape {
            base = base.union(&ViewSpec::Whole(wp), alloc);
        }
        current.push(base.clone_in(alloc));
        body_base.push(base);
    }

    // Kleene fixed-point over call edges (monotone on a finite height lattice -> guaranteed
    // termination in at most `n * (fields + 2)` rounds!).
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..n {
            let mut next = body_base[i].clone_in(alloc);
            for &callee in methods[i].self_callees.as_slice() {
                if callee < n {
                    // For a `pub` callee with a declared view, callers depend on its declared view
                    // (so changing a `pub` body without changing its signature doesn't silently leak).
                    let callee_view = match (&methods[callee].vis, &methods[callee].declared) {
                        (MethodVis::Public, Some(decl)) => decl,
                        _ => &current[callee],
                    };
                    next = next.union(callee_view, alloc);
                }
            }
            if next != current[i] {
                current[i] = next;
                changed = true;
            }
        }
    }

    // Verify declared contracts (`pub` methods MUST NOT exceed their declared view!).
    let mut effective: AVec<'a, ViewSpec<'a>> = AVec::with_capacity_in(n, alloc);
    for (i, m) in methods.iter().enumerate() {
        if let Some(ref declared) = m.declared {
            if let Err(err) = current[i].subview_of(declared) {
                let mapped = match err {
                    ViewError::FieldOutsideView { field, required } => {
                        ViewError::BodyExceedsDeclaredView { field: Some(field), required }
                    }
                    ViewError::MutatingSharedField { field } => {
                        ViewError::BodyExceedsDeclaredView {
                            field: Some(field),
                            required: FieldPerm::Unique,
                        }
                    }
                    ViewError::PartialCannotSatisfyWhole { required } => {
                        ViewError::BodyExceedsDeclaredView { field: None, required }
                    }
                    ViewError::WholeSharedCannotSatisfyUnique => {
                        ViewError::BodyExceedsDeclaredView {
                            field: None,
                            required: FieldPerm::Unique,
                        }
                    }
                    other => other,
                };
                return Err((i, mapped));
            }
            effective.push(declared.clone_in(alloc));
        } else {
            effective.push(current[i].clone_in(alloc));
        }
    }

    Ok(InferredViews { views: effective })
}

// =================================================================================================
// Tests (Objective E4 Acceptance & Refusal Fixtures)
// =================================================================================================

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;

    fn alloc() -> Alloc<'static> {
        heap().alloc()
    }

    #[test]
    fn disjoint_and_mixed_views_do_not_conflict_and_preserve_abi() {
        let a = alloc();
        // struct Engine { 0: config, 1: cache, 2: metrics, 3: lru }
        let cache_view = ViewSpec::from_entries(&[ViewEntry::shared(0), ViewEntry::unique(1)], a);
        let metrics_view = ViewSpec::from_entries(&[ViewEntry::shared(0), ViewEntry::unique(2)], a);
        let config_only = ViewSpec::from_entries(&[ViewEntry::shared(0)], a);
        let lru_mut = ViewSpec::from_entries(&[ViewEntry::unique(3)], a);

        // Disjoint unique fields + overlapping shared `config` -> ALL THREE coexist simultaneously!
        assert!(ViewSpec::check_simultaneous_borrows(&[&cache_view, &metrics_view, &lru_mut]).is_ok());
        assert!(cache_view.conflicts_with(&config_only).is_none());

        // Conflicting: unique cache vs shared cache!
        let cache_read = ViewSpec::from_entries(&[ViewEntry::shared(1)], a);
        assert_eq!(
            cache_view.conflicts_with(&cache_read),
            Some(ViewConflict {
                field: Some(FieldId(1)),
                existing: FieldPerm::Unique,
                requested: FieldPerm::Shared,
            })
        );

        // Order independence & deduplication: `{1, 0, mut 1}` == `{0, mut 1}`!
        let unordered = ViewSpec::from_entries(
            &[ViewEntry::shared(1), ViewEntry::shared(0), ViewEntry::unique(1)],
            a,
        );
        assert_eq!(unordered, cache_view);

        // Residual after taking `cache_view` from Whole(Unique):
        // field 0 stays Shared, field 1 is gone, fields 2 & 3 stay Unique!
        let residual = ViewSpec::Whole(FieldPerm::Unique).residual_after(&cache_view, 4, a);
        assert_eq!(residual.perm_of(FieldId(0)), Some(FieldPerm::Shared));
        assert_eq!(residual.perm_of(FieldId(1)), None);
        assert_eq!(residual.perm_of(FieldId(2)), Some(FieldPerm::Unique));
        assert_eq!(residual.perm_of(FieldId(3)), Some(FieldPerm::Unique));
        assert!(metrics_view.subview_of(&residual).is_ok());

        // ABI invariant: 1 pointer unchanged (`edition.md` §3).
        assert_eq!(cache_view.abi(), ViewAbi { pointer_mutability: Mutability::Mut, pointer_words: 1 });
        assert_eq!(config_only.abi(), ViewAbi { pointer_mutability: Mutability::Not, pointer_words: 1 });
    }

    #[test]
    fn subview_narrowing_complement_and_field_access_checks() {
        let a = alloc();
        let wide = ViewSpec::from_entries(&[ViewEntry::unique(0), ViewEntry::shared(1)], a);
        let narrow_ok = ViewSpec::from_entries(&[ViewEntry::shared(0)], a);
        let narrow_mut_shared = ViewSpec::from_entries(&[ViewEntry::unique(1)], a);
        let narrow_outside = ViewSpec::from_entries(&[ViewEntry::shared(2)], a);

        assert!(narrow_ok.subview_of(&wide).is_ok());
        assert_eq!(
            narrow_mut_shared.subview_of(&wide),
            Err(ViewError::MutatingSharedField { field: FieldId(1) })
        );
        assert_eq!(
            narrow_outside.subview_of(&wide),
            Err(ViewError::FieldOutsideView { field: FieldId(2), required: FieldPerm::Shared })
        );

        // Complement view `&self.{.. - 1}` over 4 fields -> `{0, 2, 3}`!
        let comp = ViewSpec::from_complement(4, &[FieldId(1)], FieldPerm::Unique, a);
        assert!(comp.check_field_access(FieldId(0), FieldPerm::Unique).is_ok());
        assert!(comp.check_field_access(FieldId(1), FieldPerm::Shared).is_err());
        assert!(comp.check_field_access(FieldId(3), FieldPerm::Unique).is_ok());
    }

    #[test]
    fn private_scc_view_inference_and_pub_contract_enforcement() {
        let a = alloc();
        let avec_entries = |items: &[ViewEntry]| {
            let mut v = AVec::new_in(a);
            for &x in items {
                v.push(x);
            }
            v
        };
        let avec_callees = |items: &[usize]| {
            let mut v = AVec::new_in(a);
            for &x in items {
                v.push(x);
            }
            v
        };

        // Method 0 (private): reads field 0, calls private method 1
        // Method 1 (private): mutates field 1, calls private method 0 (mutually recursive!)
        // Method 2 (pub): declared `&self.{0, mut 1}`, calls method 0 -> ACCEPTED!
        let m0 = MethodSummary {
            vis: MethodVis::Private,
            declared: None,
            direct_accesses: avec_entries(&[ViewEntry::shared(0)]),
            whole_escape: None,
            self_callees: avec_callees(&[1]),
        };
        let m1 = MethodSummary {
            vis: MethodVis::Private,
            declared: None,
            direct_accesses: avec_entries(&[ViewEntry::unique(1)]),
            whole_escape: None,
            self_callees: avec_callees(&[0]),
        };
        let m2_ok = MethodSummary {
            vis: MethodVis::Public,
            declared: Some(ViewSpec::from_entries(&[ViewEntry::shared(0), ViewEntry::unique(1)], a)),
            direct_accesses: avec_entries(&[]),
            whole_escape: None,
            self_callees: avec_callees(&[0]),
        };

        let inferred = infer_and_check_methods(&[m0, m1, m2_ok], a).expect("mutually recursive views infer & check");
        let expected = ViewSpec::from_entries(&[ViewEntry::shared(0), ViewEntry::unique(1)], a);
        assert_eq!(inferred.views[0], expected);
        assert_eq!(inferred.views[1], expected);
        assert_eq!(inferred.views[2], expected);

        // Now a `pub` method declaring only `&self.{0}` that calls private method 0 (which mutates field 1) -> REFUSED!
        let m0_b = MethodSummary {
            vis: MethodVis::Private,
            declared: None,
            direct_accesses: avec_entries(&[ViewEntry::shared(0)]),
            whole_escape: None,
            self_callees: avec_callees(&[1]),
        };
        let m1_b = MethodSummary {
            vis: MethodVis::Private,
            declared: None,
            direct_accesses: avec_entries(&[ViewEntry::unique(1)]),
            whole_escape: None,
            self_callees: avec_callees(&[0]),
        };
        let m2_bad = MethodSummary {
            vis: MethodVis::Public,
            declared: Some(ViewSpec::from_entries(&[ViewEntry::shared(0)], a)),
            direct_accesses: avec_entries(&[]),
            whole_escape: None,
            self_callees: avec_callees(&[0]),
        };
        let err = infer_and_check_methods(&[m0_b, m1_b, m2_bad], a);
        assert_eq!(
            err.err(),
            Some((
                2,
                ViewError::BodyExceedsDeclaredView {
                    field: Some(FieldId(1)),
                    required: FieldPerm::Unique,
                }
            ))
        );
    }
}
