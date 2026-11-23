//! Canonical Structural Record Types & Named-Argument Defaults (`blaze-ty::record_ty`).
//!
//! First-class compiler representation for Blaze structural records (`#![blaze::records]`,
//! `docs/v2/plan/design/records.md`, edition change 4 / objective E5):
//!
//! ```ignore
//! fn obj_row(ui: &uniq UiWorld, p: { name: &str, depth: u32 = 0, open: bool = false })
//! ```
//!
//! Implements every rule of `docs/v2/plan/design/records.md`:
//!
//! 1. **Order-independent canonical identity**: `{ name: &str, depth: u32 }` and
//!    `{ depth: u32, name: &str }` sort their fields by UTF-8 byte order into the exact same
//!    canonical representation matching `core::record::Record<"depth,name", (u32, &str)>`.
//!    Defaults are *not* part of the record's type identity.
//! 2. **Named arguments & callee const defaults**: [`RecordParamSig`] validates parameter-only
//!    defaults (rejecting defaults on non-parameter types and trait impls), preserves both
//!    declaration order (for default evaluation) and canonical sorted order (for tuple lowering),
//!    and [`RecordParamSig::resolve_call`] resolves `{ name: "agent-1", depth: 2, .. }` into
//!    exact slot assignments, evaluation order, and per-field incremental dependency keys
//!    (`used_defaults`).
//! 3. **Structural extension, override & projection**: supports `{ ..RowProps, look: RowLook }`
//!    type-level composition, functional updates (`{ depth: 2, ..old }`), record pattern checking
//!    (`let { name, depth: d, .. } = p`), and tuple-matching orphan/impl rules.
//! 4. **Deterministic layout**: computes exact field byte offsets, total size, and alignment in
//!    either sorted-tuple order or alignment-descending minimal-padding order.

use core::cmp::Ordering;
use core::fmt;

use blaze_own::{AString, AVec, Alloc};

use crate::cx::TyCx;
use crate::ids::{Const, Ty};
use crate::type_set::erase_regions;

/// One field of a structural record type (without default value).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordField<'s> {
    /// Field identifier (non-empty, contains no `,`).
    pub name: &'s str,
    /// Lifetime-erased type key for canonical identity and equality checks.
    pub key: Ty,
    /// Full field type preserving concrete lifetimes.
    pub ty: Ty,
}

/// One field of a function parameter record type, with an optional callee const default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecordParamField<'s> {
    /// Field identifier in declaration order.
    pub name: &'s str,
    /// Full field type.
    pub ty: Ty,
    /// Optional anonymous constant default (`= expr`), valid only on function/trait-decl params.
    pub default: Option<Const>,
}

/// Where a record type appears syntactically (governs whether `= default` is permitted).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParamContext {
    /// Direct parameter of a free function, inherent method, or extern-free signature.
    FnParam,
    /// Parameter of a trait method declaration (defaults live here; impls inherit them).
    TraitDeclParam,
    /// Parameter of a trait method `impl` (must repeat types with NO defaults).
    TraitImplParam,
    /// Type alias defining a reusable options/record schema (`type OpenOpts = { create: bool = false }`).
    TypeAlias,
    /// Struct field holding a record with defaults (`default_field_values`).
    StructField,
    /// Any other non-parameter position (return type, local annotation, generic arg).
    NonParam,
}

/// How a callee is invoked (governs whether `..` omitted-default syntax is permitted).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallTargetKind {
    /// Direct function item, inherent method, or trait method (static declaration known).
    DirectOrTrait,
    /// Function pointer (`fn(...)`), closure, or `Fn`/`FnMut`/`FnOnce` value (no callee defaults).
    Indirect,
}

/// What kind of `impl` block is attempted on a structural record type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordImplKind {
    /// `impl { a: u32 } { ... }` — always refused (E0116, foreign like a tuple).
    Inherent,
    /// `impl ForeignTrait for { a: u32 }` — always refused (E0117, orphan rule).
    ForeignTrait,
    /// `impl LocalTrait for { a: u32 }` — permitted, as for tuples.
    LocalTrait,
}

/// Errors produced by structural record construction, defaults validation, call resolution,
/// functional updates, patterns, or impl checks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordError<'s> {
    /// A field name was empty or contained `,` (reserved as the `core::record::Record` delimiter).
    InvalidFieldName { name: &'s str },
    /// Two fields shared the same name.
    DuplicateField { name: &'s str },
    /// A field carried a default `= expr` outside a function/trait-declaration parameter.
    DefaultOutsideParam { field: &'s str },
    /// A trait `impl` method re-declared a default on a record parameter field.
    ImplDeclaresDefault { field: &'s str },
    /// Call site or update referenced a field not present in the expected record type.
    UnknownField { field: &'s str },
    /// Call site omitted a field that has no default value.
    MissingRequiredField { field: &'s str },
    /// Call site omitted a defaulted field without writing trailing `..`.
    OmittedFieldWithoutDots { field: &'s str },
    /// Call site used `..` defaults through a function pointer, closure, or `Fn` value.
    IndirectCallDefaults,
    /// Functional update `{ a: 1, ..base }` used a base record with a different field set.
    UpdateBaseMismatch { expected_field: &'s str },
    /// Functional update `{ a: 1, ..base }` supplied a field whose type disagrees with the record.
    FieldTypeMismatch { field: &'s str, expected: Ty, found: Ty },
    /// Non-exhaustive record pattern omitted a field without writing `..`.
    PatternMissingField { field: &'s str },
    /// Inherent impl on a structural record (E0116): name it with a `struct` to add methods.
    InherentImplForbidden,
    /// Foreign trait impl on a structural record (E0117, orphan rule).
    ForeignTraitImplForbidden,
}

impl fmt::Display for RecordError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecordError::InvalidFieldName { name } => {
                write!(f, "invalid record field name '{name}': must be non-empty and contain no `,`")
            }
            RecordError::DuplicateField { name } => {
                write!(f, "duplicate record field '{name}'")
            }
            RecordError::DefaultOutsideParam { field } => {
                write!(f, "record field '{field}' has a default outside a function parameter")
            }
            RecordError::ImplDeclaresDefault { field } => {
                write!(f, "trait impl repeats default on record field '{field}'; defaults belong on the trait declaration")
            }
            RecordError::UnknownField { field } => {
                write!(f, "unknown record field '{field}'")
            }
            RecordError::MissingRequiredField { field } => {
                write!(f, "missing required record field '{field}' (no default declared)")
            }
            RecordError::OmittedFieldWithoutDots { field } => {
                write!(f, "record field '{field}' omitted without `..`; add `..` to use its default")
            }
            RecordError::IndirectCallDefaults => {
                write!(f, "`..` record defaults cannot be used through a function pointer, closure, or `Fn` value")
            }
            RecordError::UpdateBaseMismatch { expected_field } => {
                write!(f, "functional update base record is missing field '{expected_field}'")
            }
            RecordError::FieldTypeMismatch { field, expected, found } => {
                write!(f, "record field '{field}' type mismatch: expected {expected:?}, found {found:?}")
            }
            RecordError::PatternMissingField { field } => {
                write!(f, "record pattern omits field '{field}' without `..`")
            }
            RecordError::InherentImplForbidden => {
                write!(f, "cannot define inherent impl on anonymous record type (name it with a `struct`)")
            }
            RecordError::ForeignTraitImplForbidden => {
                write!(f, "cannot implement foreign trait on anonymous record type (orphan rule)")
            }
        }
    }
}

/// Validates a record field identifier (non-empty, no `,` separator).
#[inline]
fn validate_field_name(name: &str) -> Result<(), RecordError<'_>> {
    if name.is_empty() || name.as_bytes().contains(&b',') {
        return Err(RecordError::InvalidFieldName { name });
    }
    Ok(())
}

/// Canonical, order-independent structural record type (`{ a: A, b: B }` == `{ b: B, a: A }`).
///
/// Fields are stored sorted by UTF-8 byte order of their names, matching
/// `core::record::Record<FIELDS, Tuple>`.
#[derive(Debug)]
pub struct RecordTy<'a, 's> {
    fields: AVec<'a, RecordField<'s>>,
}

impl<'a, 's> RecordTy<'a, 's> {
    /// Builds a canonical [`RecordTy`] from an arbitrary-order slice of `(name, ty)` pairs,
    /// erasing lifetimes for canonical keys, sorting by field name bytes, and rejecting duplicates.
    pub fn from_fields(
        cx: &mut TyCx<'_>,
        raw: &[(&'s str, Ty)],
        alloc: Alloc<'a>,
    ) -> Result<Self, RecordError<'s>> {
        let mut fields = AVec::with_capacity_in(raw.len(), alloc);
        for &(name, ty) in raw {
            validate_field_name(name)?;
            let key = erase_regions(cx, ty, alloc);
            fields.push(RecordField { name, key, ty });
        }
        Self::canonicalize(fields)
    }

    /// Builds a canonical [`RecordTy`] from already-erased `(name, ty)` pairs (useful when no
    /// regions need erasure).
    pub fn from_erased(raw: &[(&'s str, Ty)], alloc: Alloc<'a>) -> Result<Self, RecordError<'s>> {
        let mut fields = AVec::with_capacity_in(raw.len(), alloc);
        for &(name, ty) in raw {
            validate_field_name(name)?;
            fields.push(RecordField { name, key: ty, ty });
        }
        Self::canonicalize(fields)
    }

    fn canonicalize(mut fields: AVec<'a, RecordField<'s>>) -> Result<Self, RecordError<'s>> {
        fields.sort_unstable_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        for pair in fields.windows(2) {
            if let (Some(a), Some(b)) = (pair.first(), pair.get(1))
                && a.name == b.name
            {
                return Err(RecordError::DuplicateField { name: a.name });
            }
        }
        Ok(Self { fields })
    }

    /// Canonical sorted fields slice.
    #[inline]
    pub fn fields(&self) -> &[RecordField<'s>] {
        &self.fields
    }

    /// Number of fields in this record (`0` for the empty record `{}`).
    #[inline]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether this is the empty record `{}`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Binary search for field `name`, returning its canonical tuple index `0..self.len()`.
    #[inline]
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.binary_search_by(|f| f.name.as_bytes().cmp(name.as_bytes())).ok()
    }

    /// Returns the [`RecordField`] for `name`, if present.
    #[inline]
    pub fn field(&self, name: &str) -> Option<RecordField<'s>> {
        self.field_index(name).and_then(|i| self.fields.get(i).copied())
    }

    /// Whether `name` is present in this record.
    #[inline]
    pub fn contains(&self, name: &str) -> bool {
        self.field_index(name).is_some()
    }

    /// Produces the canonical `FIELDS` const string matching `core::record::Record<FIELDS, T>`,
    /// e.g. `"depth,name,open"`.
    pub fn canonical_fields_str<'b>(&self, alloc: Alloc<'b>) -> AString<'b> {
        let cap: usize = self.fields.iter().map(|f| f.name.len() + 1).sum();
        let mut out = AString::with_capacity_in(cap, alloc);
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(field.name);
        }
        out
    }

    /// Structural type equality (order-independent, lifetime-erased, defaults excluded).
    pub fn same_structure(&self, other: &RecordTy<'_, '_>) -> bool {
        if self.fields.len() != other.fields.len() {
            return false;
        }
        self.fields
            .iter()
            .zip(other.fields.iter())
            .all(|(a, b)| a.name == b.name && a.key == b.key)
    }

    /// Extends `self` with disjoint `extra` fields (`{ ..Self, extra.. }`), rejecting any overlap.
    pub fn extend<'b>(
        &self,
        extra: &RecordTy<'_, 's>,
        alloc: Alloc<'b>,
    ) -> Result<RecordTy<'b, 's>, RecordError<'s>> {
        let mut merged = AVec::with_capacity_in(self.fields.len() + extra.fields.len(), alloc);
        let (mut i, mut j) = (0, 0);
        while let (Some(&a), Some(&b)) = (self.fields.get(i), extra.fields.get(j)) {
            match a.name.as_bytes().cmp(b.name.as_bytes()) {
                core::cmp::Ordering::Less => {
                    merged.push(a);
                    i += 1;
                }
                core::cmp::Ordering::Greater => {
                    merged.push(b);
                    j += 1;
                }
                core::cmp::Ordering::Equal => {
                    return Err(RecordError::DuplicateField { name: a.name });
                }
            }
        }
        if let Some(rest) = self.fields.get(i..) {
            merged.extend_from_slice(rest);
        }
        if let Some(rest) = extra.fields.get(j..) {
            merged.extend_from_slice(rest);
        }
        Ok(RecordTy { fields: merged })
    }

    /// Merges `overrides` over `self`: fields in `overrides` replace matching names in `self`
    /// and add new names cleanly (used for record/prop inheritance like `ObjRow : Row`).
    pub fn override_with<'b>(
        &self,
        overrides: &RecordTy<'_, 's>,
        alloc: Alloc<'b>,
    ) -> RecordTy<'b, 's> {
        let mut merged = AVec::with_capacity_in(self.fields.len() + overrides.fields.len(), alloc);
        let (mut i, mut j) = (0, 0);
        while let (Some(&a), Some(&b)) = (self.fields.get(i), overrides.fields.get(j)) {
            match a.name.as_bytes().cmp(b.name.as_bytes()) {
                core::cmp::Ordering::Less => {
                    merged.push(a);
                    i += 1;
                }
                core::cmp::Ordering::Greater => {
                    merged.push(b);
                    j += 1;
                }
                core::cmp::Ordering::Equal => {
                    merged.push(b);
                    i += 1;
                    j += 1;
                }
            }
        }
        if let Some(rest) = self.fields.get(i..) {
            merged.extend_from_slice(rest);
        }
        if let Some(rest) = overrides.fields.get(j..) {
            merged.extend_from_slice(rest);
        }
        RecordTy { fields: merged }
    }

    /// Projects `self` onto the requested subset of field names (order-independent).
    pub fn project<'b>(
        &self,
        names: &[&'s str],
        alloc: Alloc<'b>,
    ) -> Result<RecordTy<'b, 's>, RecordError<'s>> {
        let mut out = AVec::with_capacity_in(names.len(), alloc);
        for &name in names {
            let Some(f) = self.field(name) else {
                return Err(RecordError::UnknownField { field: name });
            };
            out.push(f);
        }
        RecordTy::canonicalize(out)
    }

    /// Returns `true` iff `self` structurally covers every field of `target` with matching
    /// lifetime-erased types (`self` is a width-subrecord of `target`: `{ a, b, c } <: { a, c }`),
    /// in O(N + M) sorted merge.
    pub fn is_subrecord_of(&self, target: &RecordTy<'_, '_>) -> bool {
        let (mut i, mut j) = (0usize, 0usize);
        while i < self.fields.len() && j < target.fields.len() {
            let (Some(sf), Some(tf)) = (self.fields.get(i), target.fields.get(j)) else {
                break;
            };
            match sf.name.as_bytes().cmp(tf.name.as_bytes()) {
                Ordering::Less => i += 1,
                Ordering::Equal => {
                    if sf.key != tf.key {
                        return false;
                    }
                    i += 1;
                    j += 1;
                }
                Ordering::Greater => return false,
            }
        }
        j == target.fields.len()
    }

    /// Computes the canonical source-field indices mapping `self` down to narrower `target`
    /// (`out[target_idx] = self_idx`), or returns the missing/mismatched field error.
    pub fn project_indices<'b>(
        &self,
        target: &RecordTy<'_, 's>,
        alloc: Alloc<'b>,
    ) -> Result<AVec<'b, usize>, RecordError<'s>> {
        let mut out = AVec::with_capacity_in(target.fields.len(), alloc);
        let (mut i, mut j) = (0usize, 0usize);
        while j < target.fields.len() {
            let Some(tf) = target.fields.get(j) else {
                break;
            };
            let Some(sf) = self.fields.get(i) else {
                return Err(RecordError::MissingRequiredField { field: tf.name });
            };
            match sf.name.as_bytes().cmp(tf.name.as_bytes()) {
                Ordering::Less => i += 1,
                Ordering::Equal => {
                    if sf.key != tf.key {
                        return Err(RecordError::FieldTypeMismatch {
                            field: tf.name,
                            expected: tf.key,
                            found: sf.key,
                        });
                    }
                    out.push(i);
                    i += 1;
                    j += 1;
                }
                Ordering::Greater => {
                    return Err(RecordError::MissingRequiredField { field: tf.name });
                }
            }
        }
        Ok(out)
    }

    /// Validates a functional update `{ field: val, ..base }` against `self`.
    ///
    /// Per `records.md`, `base` must have the exact same structural field set as `self`, and
    /// every written field must exist with a matching type and no duplicates.
    pub fn check_functional_update<'b>(
        &self,
        written: &[(&'s str, Ty)],
        base: &RecordTy<'_, 's>,
        alloc: Alloc<'b>,
    ) -> Result<AVec<'b, usize>, RecordError<'s>> {
        for f in self.fields.iter() {
            match base.field(f.name) {
                None => return Err(RecordError::UpdateBaseMismatch { expected_field: f.name }),
                Some(bf) if bf.key != f.key => {
                    return Err(RecordError::FieldTypeMismatch {
                        field: f.name,
                        expected: f.key,
                        found: bf.key,
                    });
                }
                Some(_) => {}
            }
        }
        for bf in base.fields.iter() {
            if !self.contains(bf.name) {
                return Err(RecordError::UnknownField { field: bf.name });
            }
        }

        let mut seen = AVec::with_capacity_in(self.fields.len(), alloc);
        seen.resize(self.fields.len(), false);
        let mut indices = AVec::with_capacity_in(written.len(), alloc);
        for &(name, ty) in written {
            let Some(idx) = self.field_index(name) else {
                return Err(RecordError::UnknownField { field: name });
            };
            if let Some(slot) = seen.get_mut(idx) {
                if *slot {
                    return Err(RecordError::DuplicateField { name });
                }
                *slot = true;
            }
            if let Some(expected_field) = self.fields.get(idx)
                && expected_field.key != ty
            {
                return Err(RecordError::FieldTypeMismatch {
                    field: name,
                    expected: expected_field.key,
                    found: ty,
                });
            }
            indices.push(idx);
        }
        Ok(indices)
    }

    /// Validates a record pattern `{ a, b: p, .. }` against `self`, returning each bound field's
    /// canonical tuple index in pattern order.
    pub fn check_pattern<'b>(
        &self,
        bound_fields: &[&'s str],
        has_rest: bool,
        alloc: Alloc<'b>,
    ) -> Result<AVec<'b, usize>, RecordError<'s>> {
        let mut seen = AVec::with_capacity_in(self.fields.len(), alloc);
        seen.resize(self.fields.len(), false);
        let mut indices = AVec::with_capacity_in(bound_fields.len(), alloc);
        for &name in bound_fields {
            let Some(idx) = self.field_index(name) else {
                return Err(RecordError::UnknownField { field: name });
            };
            if let Some(slot) = seen.get_mut(idx) {
                if *slot {
                    return Err(RecordError::DuplicateField { name });
                }
                *slot = true;
            }
            indices.push(idx);
        }
        if !has_rest {
            for (field, &was_seen) in self.fields.iter().zip(seen.iter()) {
                if !was_seen {
                    return Err(RecordError::PatternMissingField { field: field.name });
                }
            }
        }
        Ok(indices)
    }

    /// Enforces `records.md` coherence rules (identical to tuples: foreign to every crate).
    #[inline]
    pub const fn check_impl(kind: RecordImplKind) -> Result<(), RecordError<'static>> {
        match kind {
            RecordImplKind::Inherent => Err(RecordError::InherentImplForbidden),
            RecordImplKind::ForeignTrait => Err(RecordError::ForeignTraitImplForbidden),
            RecordImplKind::LocalTrait => Ok(()),
        }
    }

    /// Computes a deterministic alignment-descending memory layout for `self` (zero wasted
    /// inter-field padding), given `(size, align)` per canonical field index.
    pub fn compute_layout<'b>(
        &self,
        size_align: impl Fn(Ty) -> (u32, u32),
        alloc: Alloc<'b>,
    ) -> RecordLayout<'b> {
        let n = self.fields.len();
        let mut order: AVec<'b, (usize, u32, u32)> = AVec::with_capacity_in(n, alloc);
        for (idx, field) in self.fields.iter().enumerate() {
            let (sz, al) = size_align(field.ty);
            let align = if al == 0 { 1 } else { al };
            order.push((idx, sz, align));
        }
        // Sort by descending alignment, tie-breaking by canonical field index (deterministic).
        order.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

        let mut offsets = AVec::with_capacity_in(n, alloc);
        offsets.resize(n, 0u32);
        let mut cursor = 0u32;
        let mut max_align = 1u32;
        for &(idx, sz, align) in order.iter() {
            if align > max_align {
                max_align = align;
            }
            let rem = cursor % align;
            if rem != 0 {
                cursor = cursor.saturating_add(align - rem);
            }
            if let Some(off) = offsets.get_mut(idx) {
                *off = cursor;
            }
            cursor = cursor.saturating_add(sz);
        }
        let rem = cursor % max_align;
        let total_size = if rem == 0 { cursor } else { cursor.saturating_add(max_align - rem) };
        RecordLayout { offsets, size: total_size, align: max_align }
    }
}

/// Deterministic layout of a [`RecordTy`]: byte offset per canonical field index, total size,
/// and alignment.
#[derive(Debug)]
pub struct RecordLayout<'a> {
    /// Byte offset of canonical field `i` (`offsets[field_index]`).
    pub offsets: AVec<'a, u32>,
    /// Total padded size in bytes.
    pub size: u32,
    /// Struct alignment in bytes (at least 1).
    pub align: u32,
}

// -------------------------------------------------------------------------------------------------
// Named-argument parameter signature & call-site default resolution
// -------------------------------------------------------------------------------------------------

/// One field entry stored inside [`RecordParamSig`], linking declaration order and canonical
/// sorted tuple index.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ParamFieldEntry<'s> {
    pub name: &'s str,
    pub ty: Ty,
    pub key: Ty,
    /// Index in the callee's source declaration order (`0..N`).
    pub decl_index: usize,
    /// Index in the canonical sorted `Record<FIELDS, Tuple>` (`0..N`).
    pub canon_index: usize,
    /// Optional const default expression (`= expr`).
    pub default: Option<Const>,
}

/// A function parameter's structural record signature with optional const defaults:
/// e.g. `p: { name: &str, depth: u32 = 0, open: bool = false }`.
#[derive(Debug)]
pub struct RecordParamSig<'a, 's> {
    /// Underlying canonical record type (sorted by field name, defaults excluded from identity).
    ty: RecordTy<'a, 's>,
    /// Entries in callee declaration order (for deterministic default evaluation order).
    decl_order: AVec<'a, ParamFieldEntry<'s>>,
    /// Map from canonical tuple index -> declaration index.
    canon_to_decl: AVec<'a, usize>,
}

/// One named argument supplied at a call site (`{ name: expr, depth: expr, .. }`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CallArg<'s> {
    pub name: &'s str,
    /// Argument expression's type (lifetime-erased or exact).
    pub ty: Ty,
}

/// Where a canonical tuple slot gets its value when lowering a named-argument call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotSource {
    /// Supplied explicitly at call-site argument index `arg_index`.
    ExplicitArg(usize),
    /// Filled from the callee's const default expression.
    CalleeDefault(Const),
}

/// One step of evaluation order for a lowered named-argument call:
/// written arguments evaluate in source order, followed by omitted defaults in callee
/// declaration order (`records.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EvalStep<'s> {
    /// Evaluate call-site argument `arg_index`, placing it in canonical slot `canon_index`.
    Arg { arg_index: usize, canon_index: usize, name: &'s str },
    /// Place callee default `constant` into canonical slot `canon_index`.
    Default { constant: Const, canon_index: usize, name: &'s str },
}

/// The resolved plan for lowering a call to a record parameter with named arguments & defaults.
#[derive(Debug)]
pub struct ResolvedCall<'a, 's> {
    /// Value source for each canonical sorted tuple slot `0..N`.
    pub slots: AVec<'a, SlotSource>,
    /// Exact evaluation order (written args in source order, then omitted defaults in decl order).
    pub eval_order: AVec<'a, EvalStep<'s>>,
    /// Field names whose callee defaults were actually omitted by this caller — exact input keys
    /// for incremental `record_param_default(callee, param, field)` invalidation.
    pub used_defaults: AVec<'a, &'s str>,
}

impl<'a, 's> RecordParamSig<'a, 's> {
    /// Constructs and validates a record parameter signature in `ctx`.
    pub fn new(
        cx: &mut TyCx<'_>,
        fields: &[RecordParamField<'s>],
        ctx: ParamContext,
        alloc: Alloc<'a>,
    ) -> Result<Self, RecordError<'s>> {
        // Validate field names and default placement rules first.
        for f in fields {
            validate_field_name(f.name)?;
            if f.default.is_some() {
                match ctx {
                    ParamContext::FnParam
                    | ParamContext::TraitDeclParam
                    | ParamContext::TypeAlias
                    | ParamContext::StructField => {}
                    ParamContext::TraitImplParam => {
                        return Err(RecordError::ImplDeclaresDefault { field: f.name });
                    }
                    ParamContext::NonParam => {
                        return Err(RecordError::DefaultOutsideParam { field: f.name });
                    }
                }
            }
        }

        // Build canonical RecordTy (checks duplicates and sorts by name bytes).
        let mut raw = AVec::with_capacity_in(fields.len(), alloc);
        for f in fields {
            raw.push((f.name, f.ty));
        }
        let ty = RecordTy::from_fields(cx, &raw, alloc)?;

        let mut decl_order = AVec::with_capacity_in(fields.len(), alloc);
        let mut canon_to_decl = AVec::with_capacity_in(fields.len(), alloc);
        canon_to_decl.resize(fields.len(), 0);

        for (decl_index, f) in fields.iter().enumerate() {
            let canon_index = ty.field_index(f.name).unwrap_or(0);
            let key = ty.fields().get(canon_index).map_or(f.ty, |rf| rf.key);
            if let Some(slot) = canon_to_decl.get_mut(canon_index) {
                *slot = decl_index;
            }
            decl_order.push(ParamFieldEntry {
                name: f.name,
                ty: f.ty,
                key,
                decl_index,
                canon_index,
                default: f.default,
            });
        }

        Ok(Self { ty, decl_order, canon_to_decl })
    }

    /// The parameter's canonical structural record type (defaults excluded from identity).
    #[inline]
    pub fn record_ty(&self) -> &RecordTy<'a, 's> {
        &self.ty
    }

    /// Parameter fields in declaration order.
    #[inline]
    pub fn decl_fields(&self) -> &[ParamFieldEntry<'s>] {
        &self.decl_order
    }

    /// Looks up the default constant for a field by name.
    pub fn default_of(&self, name: &str) -> Option<Const> {
        let canon = self.ty.field_index(name)?;
        let &decl = self.canon_to_decl.get(canon)?;
        self.decl_order.get(decl)?.default
    }

    /// Returns `true` iff every field in this record parameter declares a default value
    /// (allowing `open(p, { .. })` or omitting the trailing record argument entirely).
    pub fn all_have_defaults(&self) -> bool {
        self.decl_order.iter().all(|e| e.default.is_some())
    }

    /// Resolves an all-defaults call (`open(p, { .. })` or omitted trailing all-default arg).
    pub fn resolve_all_defaults<'b>(
        &self,
        target: CallTargetKind,
        alloc: Alloc<'b>,
    ) -> Result<ResolvedCall<'b, 's>, RecordError<'s>> {
        self.resolve_call(&[], true, target, alloc)
    }

    /// Resolves a call-site record argument list (`{ a: x, b: y }`, `{ a: x, .. }`, or `{ .. }`)
    /// against this parameter.
    ///
    /// Defaulted fields are filled automatically when omitted without requiring trailing `, ..`
    /// (`has_rest_dots` remains accepted as optional explicit syntax). Omitting a required field
    /// with no default is always rejected, and omitting any defaulted field through an indirect
    /// call (`fn(...)` / closure / `Fn`) is rejected with [`RecordError::IndirectCallDefaults`].
    pub fn resolve_call<'b>(
        &self,
        args: &[CallArg<'s>],
        has_rest_dots: bool,
        target: CallTargetKind,
        alloc: Alloc<'b>,
    ) -> Result<ResolvedCall<'b, 's>, RecordError<'s>> {
        if has_rest_dots && target == CallTargetKind::Indirect {
            return Err(RecordError::IndirectCallDefaults);
        }

        let n = self.ty.len();
        let mut assigned: AVec<'b, Option<usize>> = AVec::with_capacity_in(n, alloc);
        assigned.resize(n, None);

        let mut eval_order = AVec::with_capacity_in(n, alloc);

        // 1. Validate written arguments in source order.
        for (arg_index, arg) in args.iter().enumerate() {
            let Some(canon_index) = self.ty.field_index(arg.name) else {
                return Err(RecordError::UnknownField { field: arg.name });
            };
            if let Some(slot) = assigned.get_mut(canon_index) {
                if slot.is_some() {
                    return Err(RecordError::DuplicateField { name: arg.name });
                }
                if let Some(expected_field) = self.ty.fields().get(canon_index)
                    && expected_field.key != arg.ty
                {
                    return Err(RecordError::FieldTypeMismatch {
                        field: arg.name,
                        expected: expected_field.key,
                        found: arg.ty,
                    });
                }
                *slot = Some(arg_index);
            }
            eval_order.push(EvalStep::Arg { arg_index, canon_index, name: arg.name });
        }

        // 2. Fill omitted fields in callee declaration order (`records.md`).
        let mut used_defaults = AVec::new_in(alloc);
        let mut slots = AVec::with_capacity_in(n, alloc);
        // Temporary sentinel slots; every index `0..n` is filled below before returning.
        slots.resize(n, SlotSource::ExplicitArg(0));

        for entry in self.decl_order.iter() {
            let arg_slot = assigned.get(entry.canon_index).copied().flatten();
            if let Some(arg_index) = arg_slot {
                if let Some(dest) = slots.get_mut(entry.canon_index) {
                    *dest = SlotSource::ExplicitArg(arg_index);
                }
            } else if let Some(constant) = entry.default {
                if target == CallTargetKind::Indirect {
                    return Err(RecordError::IndirectCallDefaults);
                }
                if let Some(dest) = slots.get_mut(entry.canon_index) {
                    *dest = SlotSource::CalleeDefault(constant);
                }
                used_defaults.push(entry.name);
                eval_order.push(EvalStep::Default {
                    constant,
                    canon_index: entry.canon_index,
                    name: entry.name,
                });
            } else {
                return Err(RecordError::MissingRequiredField { field: entry.name });
            }
        }

        Ok(ResolvedCall { slots, eval_order, used_defaults })
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::testing::heap;

    use super::*;
    use crate::ids::{IntTy, UintTy};

    fn make_const(cx: &mut TyCx<'_>, ty: Ty, val: u128) -> Const {
        cx.tys.const_value(ty, val)
    }

    #[test]
    fn order_independent_canonical_identity_and_name_string() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let str_ty = Ty::STR;
        let u32_ty = Ty::uint(UintTy::U32);
        let bool_ty = Ty::BOOL;

        let r1 = RecordTy::from_fields(
            &mut cx,
            &[("name", str_ty), ("depth", u32_ty), ("open", bool_ty)],
            a,
        )
        .expect("valid record");
        let r2 = RecordTy::from_fields(
            &mut cx,
            &[("open", bool_ty), ("name", str_ty), ("depth", u32_ty)],
            a,
        )
        .expect("valid reordered record");

        assert!(r1.same_structure(&r2));
        assert_eq!(r1.canonical_fields_str(a).as_str(), "depth,name,open");
        assert_eq!(r2.canonical_fields_str(a).as_str(), "depth,name,open");
        assert_eq!(r1.field_index("depth"), Some(0));
        assert_eq!(r1.field_index("name"), Some(1));
        assert_eq!(r1.field_index("open"), Some(2));
    }

    #[test]
    fn duplicate_and_invalid_field_names_rejected() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        assert_eq!(
            RecordTy::from_fields(&mut cx, &[("x", Ty::BOOL), ("x", Ty::BOOL)], a).unwrap_err(),
            RecordError::DuplicateField { name: "x" }
        );
        assert_eq!(
            RecordTy::from_fields(&mut cx, &[("", Ty::BOOL)], a).unwrap_err(),
            RecordError::InvalidFieldName { name: "" }
        );
        assert_eq!(
            RecordTy::from_fields(&mut cx, &[("a,b", Ty::BOOL)], a).unwrap_err(),
            RecordError::InvalidFieldName { name: "a,b" }
        );
    }

    #[test]
    fn named_args_with_defaults_obj_row_scenario() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let u32_ty = Ty::uint(UintTy::U32);
        let c_zero = make_const(&mut cx, u32_ty, 0);
        let c_false = make_const(&mut cx, Ty::BOOL, 0);

        // fn obj_row(ui: &uniq UiWorld, p: { name: &str, depth: u32 = 0, open: bool = false })
        let param = RecordParamSig::new(
            &mut cx,
            &[
                RecordParamField { name: "name", ty: Ty::STR, default: None },
                RecordParamField { name: "depth", ty: u32_ty, default: Some(c_zero) },
                RecordParamField { name: "open", ty: Ty::BOOL, default: Some(c_false) },
            ],
            ParamContext::FnParam,
            a,
        )
        .expect("valid param signature");

        // Call: obj_row(ui, { name: "agent-1", open: true, .. })
        let resolved = param
            .resolve_call(
                &[CallArg { name: "name", ty: Ty::STR }, CallArg { name: "open", ty: Ty::BOOL }],
                true,
                CallTargetKind::DirectOrTrait,
                a,
            )
            .expect("call with .. resolves");

        // Canonical sorted slots: 0=depth (default), 1=name (arg 0), 2=open (arg 1)
        assert_eq!(resolved.slots[0], SlotSource::CalleeDefault(c_zero));
        assert_eq!(resolved.slots[1], SlotSource::ExplicitArg(0));
        assert_eq!(resolved.slots[2], SlotSource::ExplicitArg(1));

        // Evaluation order: written args first in source order, then omitted defaults in decl order.
        assert_eq!(resolved.eval_order.len(), 3);
        assert_eq!(
            resolved.eval_order[0],
            EvalStep::Arg { arg_index: 0, canon_index: 1, name: "name" }
        );
        assert_eq!(
            resolved.eval_order[1],
            EvalStep::Arg { arg_index: 1, canon_index: 2, name: "open" }
        );
        assert_eq!(
            resolved.eval_order[2],
            EvalStep::Default { constant: c_zero, canon_index: 0, name: "depth" }
        );

        // Only `depth` default was actually used -> incremental invalidation tracks only `depth`.
        assert_eq!(resolved.used_defaults.as_slice(), &["depth"]);

        // Omitting `depth` WITHOUT `, ..` now auto-fills the default seamlessly!
        let resolved_no_dots = param
            .resolve_call(
                &[CallArg { name: "name", ty: Ty::STR }, CallArg { name: "open", ty: Ty::BOOL }],
                false,
                CallTargetKind::DirectOrTrait,
                a,
            )
            .expect("call without .. auto-fills defaulted fields");
        assert_eq!(resolved_no_dots.slots.as_slice(), resolved.slots.as_slice());
        assert_eq!(resolved_no_dots.used_defaults.as_slice(), &["depth"]);

        // Indirect call omitting a defaulted field (even without `..`) is still rejected.
        assert_eq!(
            param
                .resolve_call(
                    &[CallArg { name: "name", ty: Ty::STR }],
                    false,
                    CallTargetKind::Indirect,
                    a,
                )
                .unwrap_err(),
            RecordError::IndirectCallDefaults
        );

        // Omitting required `name` even with `..` is rejected.
        assert_eq!(
            param
                .resolve_call(
                    &[CallArg { name: "depth", ty: u32_ty }],
                    true,
                    CallTargetKind::DirectOrTrait,
                    a,
                )
                .unwrap_err(),
            RecordError::MissingRequiredField { field: "name" }
        );

        // Indirect call with `..` is rejected.
        assert_eq!(
            param
                .resolve_call(
                    &[CallArg { name: "name", ty: Ty::STR }],
                    true,
                    CallTargetKind::Indirect,
                    a,
                )
                .unwrap_err(),
            RecordError::IndirectCallDefaults
        );

        // Unknown field is rejected.
        assert_eq!(
            param
                .resolve_call(
                    &[
                        CallArg { name: "name", ty: Ty::STR },
                        CallArg { name: "bogus", ty: Ty::BOOL },
                    ],
                    true,
                    CallTargetKind::DirectOrTrait,
                    a,
                )
                .unwrap_err(),
            RecordError::UnknownField { field: "bogus" }
        );
    }

    #[test]
    fn default_placement_and_orphan_rules() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let c0 = make_const(&mut cx, Ty::BOOL, 0);
        let fields = [RecordParamField { name: "open", ty: Ty::BOOL, default: Some(c0) }];

        assert_eq!(
            RecordParamSig::new(&mut cx, &fields, ParamContext::NonParam, a).unwrap_err(),
            RecordError::DefaultOutsideParam { field: "open" }
        );
        assert_eq!(
            RecordParamSig::new(&mut cx, &fields, ParamContext::TraitImplParam, a).unwrap_err(),
            RecordError::ImplDeclaresDefault { field: "open" }
        );
        assert!(RecordParamSig::new(&mut cx, &fields, ParamContext::TraitDeclParam, a).is_ok());
        // Type-level defaults on reusable aliases and struct fields are permitted!
        let alias_sig =
            RecordParamSig::new(&mut cx, &fields, ParamContext::TypeAlias, a).expect("type alias defaults");
        assert!(alias_sig.all_have_defaults());
        let all_def = alias_sig
            .resolve_all_defaults(CallTargetKind::DirectOrTrait, a)
            .expect("all-defaults call");
        assert_eq!(all_def.slots.as_slice(), &[SlotSource::CalleeDefault(c0)]);
        assert_eq!(all_def.used_defaults.as_slice(), &["open"]);
        assert!(RecordParamSig::new(&mut cx, &fields, ParamContext::StructField, a).is_ok());

        assert_eq!(
            RecordTy::check_impl(RecordImplKind::Inherent),
            Err(RecordError::InherentImplForbidden)
        );
        assert_eq!(
            RecordTy::check_impl(RecordImplKind::ForeignTrait),
            Err(RecordError::ForeignTraitImplForbidden)
        );
        assert_eq!(RecordTy::check_impl(RecordImplKind::LocalTrait), Ok(()));
    }

    #[test]
    fn extend_override_update_pattern_and_layout() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let u32_ty = Ty::uint(UintTy::U32);
        let i64_ty = Ty::int(IntTy::I64);

        let base = RecordTy::from_fields(&mut cx, &[("name", Ty::STR), ("depth", u32_ty)], a)
            .expect("base");
        let extra = RecordTy::from_fields(&mut cx, &[("open", Ty::BOOL)], a).expect("extra");
        let extended = base.extend(&extra, a).expect("disjoint extend");
        assert_eq!(extended.canonical_fields_str(a).as_str(), "depth,name,open");

        // Override replaces depth type and adds score.
        let ov = RecordTy::from_fields(&mut cx, &[("depth", i64_ty), ("score", u32_ty)], a)
            .expect("override");
        let merged = extended.override_with(&ov, a);
        assert_eq!(merged.field("depth").unwrap().ty, i64_ty);
        assert_eq!(merged.field("score").unwrap().ty, u32_ty);

        // Functional update `{ depth: 2, ..base }`
        let updated_idxs = base
            .check_functional_update(&[("depth", u32_ty)], &base, a)
            .expect("valid update");
        assert_eq!(updated_idxs.as_slice(), &[0]);

        // Pattern checks
        assert_eq!(
            extended.check_pattern(&["name"], false, a).unwrap_err(),
            RecordError::PatternMissingField { field: "depth" }
        );
        let bound = extended.check_pattern(&["open", "name"], true, a).expect("rest pattern");
        assert_eq!(bound.as_slice(), &[2, 1]);

        // Layout: i64 (8/8), u32 (4/4), bool (1/1) -> offsets 0, 8, 12, total 16 align 8
        let r = RecordTy::from_fields(
            &mut cx,
            &[("flag", Ty::BOOL), ("count", u32_ty), ("big", i64_ty)],
            a,
        )
        .expect("layout record");
        let layout = r.compute_layout(
            |t| {
                if t == i64_ty {
                    (8, 8)
                } else if t == u32_ty {
                    (4, 4)
                } else {
                    (1, 1)
                }
            },
            a,
        );
        // Canonical sorted order: 0=big (8), 1=count (4), 2=flag (1)
        assert_eq!(layout.offsets.as_slice(), &[0, 8, 12]);
        assert_eq!(layout.size, 16);
        assert_eq!(layout.align, 8);
    }

    #[test]
    fn subrecord_projection_and_arbitrary_arity_beyond_12_fields() {
        let h = heap();
        let a = h.alloc();
        let mut cx = TyCx::new_in(a);
        let u32_ty = Ty::uint(UintTy::U32);
        let c0 = make_const(&mut cx, u32_ty, 0);

        // 16 fields (past Rust's 12-tuple ceiling!) with defaults.
        let names = [
            "f15", "f02", "f11", "f00", "f08", "f14", "f03", "f07",
            "f12", "f01", "f09", "f05", "f13", "f04", "f10", "f06",
        ];
        let mut param_fields = AVec::with_capacity_in(names.len(), a);
        for &n in &names {
            param_fields.push(RecordParamField { name: n, ty: u32_ty, default: Some(c0) });
        }
        let sig = RecordParamSig::new(&mut cx, &param_fields, ParamContext::TypeAlias, a)
            .expect("16-field record");
        assert_eq!(sig.record_ty().len(), 16);
        assert!(sig.all_have_defaults());

        let resolved = sig
            .resolve_call(&[CallArg { name: "f07", ty: u32_ty }], false, CallTargetKind::DirectOrTrait, a)
            .expect("16-field auto-filled defaults");
        assert_eq!(resolved.used_defaults.len(), 15);
        assert_eq!(resolved.slots[7], SlotSource::ExplicitArg(0));

        // Width subrecord projection: 16-field record -> 2-field subrecord { f02, f13 }
        let narrow = RecordTy::from_fields(&mut cx, &[("f13", u32_ty), ("f02", u32_ty)], a).expect("narrow");
        assert!(sig.record_ty().is_subrecord_of(&narrow));
        assert!(!narrow.is_subrecord_of(sig.record_ty()));
        let idx = sig.record_ty().project_indices(&narrow, a).expect("project");
        assert_eq!(idx.as_slice(), &[2, 13]);
    }
}
