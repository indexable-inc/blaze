//! The ENVIRONMENT of a closure or `async` block, field by field: THE ONE LIST every reader of
//! captures takes (the build's layout and walk, the flow lowering, the ladder's witnesses), so
//! no two of them can disagree on a count, an order or a type.
//!
//! The Reference, "Closure types": a closure's type is "approximately equivalent to a struct
//! which contains the captured values"; since Rust 2021 (RFC 2229) what is captured is a PLACE
//! (`x.a`, `*r`, `(*self).f`), one field per place, in capture order (capture_places.rs
//! `finish`), which is also the order the fields drop in.
//!
//! Two strategies at one seam (rule 78): [`WholeBindings`] reads the whole-binding rows
//! (`Checked::captures`, one field per binding), [`Places`] the place rows
//! (`Checked::capture_places`) of every owner the place analysis decided and the whole-binding
//! rows of the rest. [`ActiveEnv`] names the one the build and the flow read; the stage-2 switch
//! is that alias.
//!
//! A captured use inside the body is found by [`use_path`] (the place an expression names, as
//! capture_places.rs `cat` walks it, from the record alone) and [`cover`] (which field holds it).

use crate::body::{BodyView, Expr, UnOp};
use crate::columns::{AdjustKind, CaptureKind, Checked};
use crate::ids::{ExprId, Mutbl, PatId, UintTy};
use crate::portable::{PNode, PTy};

/// One projection of a captured place (`Checked::capture_projs`, kind 0 to 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureProj {
    /// `*` of a reference or raw pointer, or of a `Box` (the type before it says which:
    /// [`EnvField::ty_before`]).
    Deref,
    /// Field `field` of variant `variant` (0 for a struct, a tuple or a closure).
    Field { variant: u32, field: u32 },
    /// An element: the analysis never keeps one (an index stops the place before it).
    Index,
}

/// One field of an environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvField {
    /// The captured place's root: a binding of an enclosing body.
    pub base: PatId,
    /// Its projections: rows `start..start + len` of `Checked::capture_projs`; none is the
    /// whole binding.
    pub start: u32,
    pub len: u32,
    /// How the field holds the place: by value (`T`), or a reference to it (`&T`, `&mut T`;
    /// a unique immutable borrow is held as `&mut T`).
    pub how: CaptureKind,
    /// The field's type as the place analysis filed it (`T`, `&T` or `&mut T`); `None` for a
    /// whole-binding row, whose field is `local_ty[base]` wrapped by `how`.
    pub stored: Option<PTy>,
}

impl EnvField {
    /// The whole binding, no projection.
    pub fn is_whole(&self) -> bool {
        self.len == 0
    }

    /// Projection `i` of the place and the type it yields.
    pub fn proj(&self, ck: &Checked<'_>, i: u32) -> Option<(CaptureProj, PTy)> {
        if i >= self.len {
            return None;
        }
        ck.capture_proj(self.start + i)
    }

    /// The type before projection `i` (the binding's for 0).
    pub fn ty_before(&self, ck: &Checked<'_>, i: u32) -> Option<PTy> {
        if i == 0 { ck.local_ty.get(self.base.ix()).copied() } else { self.proj(ck, i - 1).map(|p| p.1) }
    }

    /// The captured place's own type (the binding's for a whole one).
    pub fn place_ty(&self, ck: &Checked<'_>) -> Option<PTy> {
        self.ty_before(ck, self.len)
    }

    /// Whether projection `i` is a `Box`'s dereference (a dereference of other than a
    /// reference or a raw pointer: the analysis keeps no other overloaded one).
    pub fn derefs_box(&self, ck: &Checked<'_>, i: u32) -> Option<bool> {
        match self.proj(ck, i)?.0 {
            CaptureProj::Deref => {
                let before = self.ty_before(ck, i)?;
                Some(!matches!(ck.types.view().node(before).ok()?, PNode::Ref(..) | PNode::Ptr(..)))
            }
            CaptureProj::Field { .. } | CaptureProj::Index => Some(false),
        }
    }
}

/// A strategy for what an environment's fields are (rule 78).
pub trait EnvRule {
    /// Whether a decided owner's fields are its places.
    const PLACES: bool;
}

/// One field per captured binding (`Checked::captures`).
#[derive(Clone, Copy, Debug)]
pub struct WholeBindings;

/// One field per captured place where the place analysis decided the owner
/// (`Checked::capture_places`), one per binding elsewhere.
#[derive(Clone, Copy, Debug)]
pub struct Places;

impl EnvRule for WholeBindings {
    const PLACES: bool = false;
}

impl EnvRule for Places {
    const PLACES: bool = true;
}

/// THE strategy every reader takes. The stage-2 switch (one save across typeck, the build, the
/// flow lowering and the ladder) sets it to [`Places`].
pub type ActiveEnv = WholeBindings;

/// The fields of one environment, in field order ([`Checked::env_fields`]).
#[derive(Clone, Debug)]
pub struct EnvFields<'c, 'a> {
    ck: &'c Checked<'a>,
    owner: ExprId,
    /// Place rows still to give, when the owner's fields are its places.
    places: core::ops::Range<usize>,
    /// Whole-binding rows still to scan, when they are not.
    whole: core::ops::Range<usize>,
}

impl Iterator for EnvFields<'_, '_> {
    type Item = EnvField;

    fn next(&mut self) -> Option<EnvField> {
        let ck = self.ck;
        if let Some(i) = self.places.next() {
            let r = &ck.capture_places;
            return Some(EnvField {
                base: r.column::<1>()[i],
                start: r.column::<2>()[i],
                len: r.column::<3>()[i],
                how: r.column::<4>()[i],
                stored: Some(r.column::<5>()[i]),
            });
        }
        let caps = &ck.captures;
        for i in self.whole.by_ref() {
            if caps.column::<0>()[i] == self.owner {
                return Some(EnvField { base: caps.column::<1>()[i], start: 0, len: 0, how: caps.column::<2>()[i], stored: None });
            }
        }
        None
    }
}

impl<'a> Checked<'a> {
    /// The fields of closure or `async` block `e`'s environment under strategy `R`, in field
    /// order. Under [`Places`], an owner with place rows has exactly those; any other owner has
    /// its whole-binding rows (the stage-2 switch makes a decided owner's whole-binding rows the
    /// bindings of its places, so a decided owner capturing nothing has neither).
    pub fn env_fields_by<R: EnvRule>(&self, e: ExprId) -> EnvFields<'_, 'a> {
        let places = if R::PLACES { self.capture_places_of(e) } else { 0..0 };
        let whole = if places.is_empty() { 0..self.captures.len() } else { 0..0 };
        EnvFields { ck: self, owner: e, places, whole }
    }

    /// The fields of `e`'s environment under [`ActiveEnv`].
    pub fn env_fields(&self, e: ExprId) -> EnvFields<'_, 'a> {
        self.env_fields_by::<ActiveEnv>(e)
    }

    /// The bindings of place-decided owner `e` whose whole-binding row is by value although
    /// their places are not exactly that whole binding by value. Under [`WholeBindings`] the
    /// environment holds each such binding moved whole, which its places are no superset of, so
    /// a witness over the places (`T: Send` for an async block's interior) adds each binding's
    /// type, as capture_places.rs `places_tuple` adds it for the check: the same rule, over the
    /// record. Empty for an owner without place rows (its fields are then those rows) and under
    /// [`Places`], whose layout holds the places alone: the stage-2 switch empties it.
    pub fn moved_whole_beside_places(&self, e: ExprId) -> impl Iterator<Item = PatId> + '_ {
        let places = self.capture_places_of(e);
        let rows = if places.is_empty() || <ActiveEnv as EnvRule>::PLACES { 0..0 } else { 0..self.captures.len() };
        let caps = &self.captures;
        let pl = &self.capture_places;
        rows.filter_map(move |i| {
            if caps.column::<0>()[i] != e || caps.column::<2>()[i] != CaptureKind::ByValue {
                return None;
            }
            let base = caps.column::<1>()[i];
            let mut of = places.clone().filter(|&j| pl.column::<1>()[j] == base);
            let exact = matches!((of.next(), of.next()), (Some(j), None) if pl.column::<3>()[j] == 0 && pl.column::<4>()[j] == CaptureKind::ByValue);
            (!exact).then_some(base)
        })
    }

    /// Row `i` of `capture_projs`: the projection and the type it yields; `None` for a kind no
    /// projection has (the record's reader refuses one) or a row past the column.
    pub fn capture_proj(&self, i: u32) -> Option<(CaptureProj, PTy)> {
        let r = &self.capture_projs;
        let i = i as usize;
        let kind = match *r.column::<0>().get(i)? {
            0 => CaptureProj::Deref,
            1 => CaptureProj::Field { variant: r.column::<1>()[i], field: r.column::<2>()[i] },
            2 => CaptureProj::Index,
            _ => return None,
        };
        Some((kind, r.column::<3>()[i]))
    }
}

/// The longest place [`use_path`] follows; a longer one is [`PathError::TooDeep`].
pub const MOST_PATH: usize = 32;

/// One step of a used place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A builtin `*` of a reference or raw pointer.
    Deref,
    /// An overloaded `*` (an adjustment row `Deref { overloaded: Some(_) }`): a `Box`'s is a
    /// place the analysis may capture through; any other's place is a temporary, which no
    /// captured place reaches through.
    OverDeref,
    Field { variant: u32, field: u32 },
    /// A builtin element of an array or slice at a `usize`.
    Index,
}

/// The place an expression names: a binding and its steps.
#[derive(Clone, Copy, Debug)]
pub struct UsePath {
    pub root: PatId,
    steps: [Step; MOST_PATH],
    len: u8,
}

impl UsePath {
    pub fn steps(&self) -> &[Step] {
        &self.steps[..self.len as usize]
    }

    fn push(&mut self, s: Step) -> Result<(), PathError> {
        let n = self.len as usize;
        if n >= MOST_PATH {
            return Err(PathError::TooDeep);
        }
        self.steps[n] = s;
        self.len += 1;
        Ok(())
    }
}

/// Why [`use_path`] could not say which place an expression names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathError {
    /// A place deeper than [`MOST_PATH`] steps.
    TooDeep,
    /// A field expression the record has no field row of.
    NoFieldRow(ExprId),
    /// An expression or a type past its table, or an adjustment no place takes.
    Malformed(ExprId),
}

impl core::fmt::Display for PathError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            PathError::TooDeep => write!(f, "a captured place deeper than {MOST_PATH} projections (blaze-typeck env_fields.rs)"),
            PathError::NoFieldRow(e) => write!(f, "field expression {e:?} has no field row (blaze-typeck env_fields.rs)"),
            PathError::Malformed(e) => write!(f, "expression {e:?} past its body's tables (blaze-typeck env_fields.rs)"),
        }
    }
}

/// The adjustment rows of `e` in `ck` (sorted by expression).
fn rows_of(ck: &Checked<'_>, e: ExprId) -> core::ops::Range<usize> {
    let col = ck.adjusts.column::<0>();
    col.partition_point(|&x| x < e)..col.partition_point(|&x| x <= e)
}

/// The `(variant, field)` row of field expression `e`.
fn field_of(ck: &Checked<'_>, e: ExprId) -> Option<(u32, u32)> {
    let col = ck.fields.column::<0>();
    let i = col.partition_point(|&x| x < e);
    (col.get(i) == Some(&e)).then(|| (ck.fields.column::<1>()[i], ck.fields.column::<2>()[i]))
}

/// The steps `x`'s first `n` adjustment rows add; `Ok(false)` once a row makes a value of its
/// own (a borrow, a coercion): no place of a binding.
fn apply_rows(ck: &Checked<'_>, x: ExprId, n: u32, path: &mut UsePath) -> Result<bool, PathError> {
    let tys = ck.types.view();
    let mut before = *ck.expr_ty.get(x.ix()).ok_or(PathError::Malformed(x))?;
    for (k, r) in rows_of(ck, x).enumerate() {
        if k as u32 >= n {
            break;
        }
        let target = ck.adjusts.column::<2>()[r];
        match ck.adjusts.column::<1>()[r] {
            AdjustKind::Deref { overloaded: None } => {
                match (tys.node(before).map_err(|_| PathError::Malformed(x))?, tys.node(target).map_err(|_| PathError::Malformed(x))?) {
                    // The slice step of an array: the same place, unsized.
                    (PNode::Array(..), PNode::Slice(_)) => {}
                    (PNode::Ref(..) | PNode::Ptr(..), _) => path.push(Step::Deref)?,
                    _ => return Err(PathError::Malformed(x)),
                }
            }
            AdjustKind::Deref { overloaded: Some(_) } => path.push(Step::OverDeref)?,
            _ => return Ok(false),
        }
        before = target;
    }
    Ok(true)
}

/// Whether `x[index]` (with `base` its base) is the builtin element: an array or slice
/// (after the base's adjustments) at a `usize` (capture_places.rs `place_index_is_builtin`).
fn index_is_builtin(ck: &Checked<'_>, base: ExprId, index: ExprId) -> Result<bool, PathError> {
    let tys = ck.types.view();
    let rows = rows_of(ck, base);
    let t = match rows.end.checked_sub(1).filter(|&last| last >= rows.start) {
        Some(last) => ck.adjusts.column::<2>()[last],
        None => *ck.expr_ty.get(base.ix()).ok_or(PathError::Malformed(base))?,
    };
    let i = *ck.expr_ty.get(index.ix()).ok_or(PathError::Malformed(index))?;
    Ok(matches!(tys.node(t).map_err(|_| PathError::Malformed(base))?, PNode::Array(..) | PNode::Slice(_))
        && matches!(tys.node(i).map_err(|_| PathError::Malformed(index))?, PNode::Uint(UintTy::Usize)))
}

/// The place `e` names (after its first `adjusted` adjustment rows: 0 is `e` itself,
/// `u32::MAX` all of them), when it is a binding or a projection of one, as capture_places.rs
/// `cat` walks it from the record alone; `Ok(None)` for any other value (a call's, a
/// temporary's, an overloaded index's).
pub fn use_path(body: &BodyView<'_>, ck: &Checked<'_>, e: ExprId, adjusted: u32) -> Result<Option<UsePath>, PathError> {
    const MOST_CHAIN: usize = 2 * MOST_PATH;
    let mut chain = [ExprId(0); MOST_CHAIN];
    let mut n = 0usize;
    let mut x = e;
    let root = loop {
        if n >= MOST_CHAIN {
            return Err(PathError::TooDeep);
        }
        chain[n] = x;
        n += 1;
        match *body.exprs.get(x.ix()).ok_or(PathError::Malformed(x))? {
            Expr::Paren(i) => x = i,
            Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Index { base, .. } => x = base,
            Expr::Unary { op: UnOp::Deref, operand } => x = operand,
            Expr::Local(p) => break p,
            _ => return Ok(None),
        }
    };
    let mut path = UsePath { root, steps: [Step::Deref; MOST_PATH], len: 0 };
    for k in (0..n).rev() {
        let x = chain[k];
        if k + 1 < n {
            let child = chain[k + 1];
            match *body.exprs.get(x.ix()).ok_or(PathError::Malformed(x))? {
                Expr::Paren(_) => {}
                Expr::Field { .. } | Expr::TupleField { .. } => {
                    let (variant, field) = field_of(ck, x).ok_or(PathError::NoFieldRow(x))?;
                    path.push(Step::Field { variant, field })?;
                }
                Expr::Unary { .. } => {
                    // An overloaded `*` is its operand's last row, applied with the operand's.
                    let rows = rows_of(ck, child);
                    let overloaded = rows.end > rows.start && matches!(ck.adjusts.column::<1>()[rows.end - 1], AdjustKind::Deref { overloaded: Some(_) });
                    if !overloaded {
                        path.push(Step::Deref)?;
                    }
                }
                Expr::Index { index, .. } => {
                    if !index_is_builtin(ck, child, index)? {
                        return Ok(None);
                    }
                    path.push(Step::Index)?;
                }
                _ => return Err(PathError::Malformed(x)),
            }
        }
        let n_rows = if k > 0 { u32::MAX } else { adjusted };
        if n_rows > 0 && !apply_rows(ck, x, n_rows, &mut path)? {
            return Ok(None);
        }
    }
    Ok(Some(path))
}

/// Which field of an environment holds a used place ([`cover`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cover {
    /// Field `j` holds exactly this place (by value, or a reference to it).
    Exact(usize),
    /// Field `j` holds a place this one is under: the first `taken` steps are the field's, the
    /// rest project from it.
    Under { j: usize, taken: u32 },
    /// Field `j` is a shared reference to `*this` (`truncate_capture_for_optimization`: a place
    /// through a shared reference is captured as the reference's target, by reference), so this
    /// place's value is the field's: the same pointer.
    RefOf(usize),
    /// The place is above captured places without being covered (only parts of it are held).
    Above,
    /// No field of this root.
    NotCaptured,
}

/// Whether step `s` is projection `p`.
fn same(s: Step, p: CaptureProj) -> bool {
    match (s, p) {
        (Step::Deref | Step::OverDeref, CaptureProj::Deref) => true,
        (Step::Field { variant, field }, CaptureProj::Field { variant: v, field: f }) => variant == v && field == f,
        _ => false,
    }
}

/// Which of `fields` (one environment's, in order) holds the place `path`.
pub fn cover(ck: &Checked<'_>, fields: &[EnvField], path: &UsePath) -> Cover {
    let steps = path.steps();
    let mut above = false;
    for (j, f) in fields.iter().enumerate() {
        if f.base != path.root {
            continue;
        }
        let n = f.len as usize;
        let common = (0..n.min(steps.len())).take_while(|&i| f.proj(ck, i as u32).is_some_and(|(p, _)| same(steps[i], p))).count();
        if common == n {
            return if n == steps.len() { Cover::Exact(j) } else { Cover::Under { j, taken: n as u32 } };
        }
        if common == steps.len() {
            // `path` is above field `j`'s place: its value is the field's when the field is a
            // shared reference to `*path`.
            if n == steps.len() + 1
                && matches!(f.how, CaptureKind::ByRef | CaptureKind::ByMutRef)
                && f.proj(ck, n as u32 - 1).is_some_and(|(p, _)| p == CaptureProj::Deref)
                && f.ty_before(ck, n as u32 - 1).and_then(|t| ck.types.view().node(t).ok()).is_some_and(|t| match (t, f.how) {
                    (PNode::Ref(Mutbl::Not, _), CaptureKind::ByRef)
                    | (PNode::Ref(Mutbl::Mut, _), CaptureKind::ByMutRef | CaptureKind::ByRef) => true,
                    _ => false,
                })
            {
                return Cover::RefOf(j);
            }
            above = true;
        }
    }
    if above { Cover::Above } else { Cover::NotCaptured }
}
