//! How the messages name a place, as rustc's `describe_place`: `x`, `x.field`, `*x`, `x[_]`.

use std::fmt::{self, Write as _};

use hir_def::{VariantId, expr_store::Body, hir::BindingId};
use hir_expand::{MacroCallId, MacroCallKind};
use rustc_type_ir::inherent::IntoKind;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use hir_ty::{
    InferBodyId,
    mir::{LocalId, MirBody, PlaceElem, PlaceTy, ProjectionElem},
    next_solver::TyKind,
};

use super::Ctx;
use crate::mem::{FVec, Frame};

/// A string written in a frame: a diagnostic's text while it is drafted.
pub(super) struct Text<'f>(FVec<'f, u8>);

impl<'f> Text<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        Text(Vec::new_in(frame))
    }

    /// `args` formatted into `frame`: a literal as it is, anything else written in pieces.
    pub(super) fn format(frame: &'f Frame, args: fmt::Arguments<'_>) -> &'f str {
        if let Some(text) = args.as_str() {
            return text;
        }
        let mut text = Text::new(frame);
        fmt::write(&mut text, args).expect("a diagnostic's text formats");
        text.finish()
    }

    pub(super) fn push(&mut self, piece: &str) {
        self.0.extend_from_slice(piece.as_bytes());
    }

    /// Puts the ASCII `byte` before what is written: the `*` of a dereference.
    pub(super) fn prefix(&mut self, byte: u8) {
        debug_assert!(byte.is_ascii());
        self.0.insert(0, byte);
    }

    pub(super) fn finish(self) -> &'f str {
        std::str::from_utf8(self.0.leak()).expect("written as `str`s")
    }
}

impl fmt::Write for Text<'_> {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        self.push(piece);
        Ok(())
    }
}

impl<'a, 'db> Ctx<'a, 'db> {
    /// The name of `local` as the source has it: the variable, or the variable a closure
    /// captured.
    pub(super) fn local_name(&self, local: LocalId) -> Option<&'a str> {
        let binding = match self.bindings.get(&local) {
            Some(&binding) => binding,
            None => self.upvars.iter().find(|it| it.local == local)?.binding,
        };
        Some(self.store[binding].name.as_str())
    }

    /// The place as rustc writes it in a message, or `None` for a temporary.
    pub(super) fn describe(
        &self,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Option<&'a str> {
        let mut name = Text::new(self.frame);
        let mut start = 0;
        match self.upvar_of_projection(body, local, projection) {
            Some((upvar, used)) => {
                start = used;
                self.capture_name(upvar, &mut name);
            }
            None if self.bindings.get(&local).is_some_and(|&it| self.from_external_macro(it)) => {
                return None;
            }
            None => name.push(self.local_name(local)?),
        }
        let mut ty = self.place_ty(body, local, &projection[..start]);
        for (index, elem) in projection.iter().enumerate().skip(start) {
            match elem {
                ProjectionElem::Deref => {
                    let next_is_field =
                        matches!(projection.get(index + 1), Some(ProjectionElem::Field(_)));
                    if !next_is_field {
                        name.prefix(b'*');
                    }
                }
                ProjectionElem::Field(field) => {
                    name.push(".");
                    self.field_name(ty, field.0, &mut name);
                }
                ProjectionElem::Index(_) => name.push("[_]"),
                ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. } => {
                    name.push("[..]")
                }
                ProjectionElem::Downcast(_) => (),
            }
            ty = self.project(ty, elem);
        }
        Some(name.finish())
    }

    /// Whether a macro of another crate, or an attribute or derive, wrote the name of
    /// `binding`, as `pinned` of `pin!`: rustc's `in_external_macro`, whose variables its
    /// messages call temporaries.
    fn from_external_macro(&self, binding: BindingId) -> bool {
        let InferBodyId::DefWithBodyId(def) = self.owner else { return false };
        let source_map = Body::unmoved_source_map(self.db, def);
        let Some(&pat) = source_map.patterns_for_binding(binding).first() else { return false };
        let Ok(source) = source_map.pat_syntax(pat) else { return false };
        let Some(call) = source.file_id.macro_file() else { return false };
        let node = source.value.to_view(source.file_id.root(self.db));
        let Some(name) = node.right().and_then(|it| match it {
            ast::Pat::IdentPat(it) => it.name(),
            _ => None,
        }) else {
            return false;
        };
        let span = call.expansion_span_map(self.db).span_at(name.syntax().text_range().start());
        let Some(expansion) = span.ctx.outer_expn(self.db) else { return false };
        let loc = MacroCallId::from(expansion).loc(self.db);
        loc.def.krate != self.krate || !matches!(loc.kind, MacroCallKind::FnLike { .. })
    }

    /// What a closure calls its capture `upvar`: the place it captured, as rustc's
    /// `place_to_string_for_capture`, `z.0.0.0` or `*bx.0`.
    fn capture_name(&self, upvar: usize, out: &mut Text<'_>) {
        use hir_ty::closure_analysis::ProjectionKind;
        let place = &self.upvars[upvar].place.place;
        out.push(self.store[self.upvars[upvar].binding].name.as_str());
        for (index, projection) in place.projections.iter().enumerate() {
            match projection.kind {
                ProjectionKind::Deref => out.prefix(b'*'),
                ProjectionKind::Field { field_idx, variant_idx } => {
                    let variant = match place.ty_before_projection(index).kind() {
                        TyKind::Adt(def, _) => {
                            def.def_id().variants(self.db).nth(variant_idx as usize)
                        }
                        _ => None,
                    };
                    out.push(".");
                    match variant {
                        Some(variant) => self.variant_field_name(variant, field_idx, out),
                        None => write!(out, "{field_idx}").expect("a `Text` takes every write"),
                    }
                }
                _ => (),
            }
        }
    }

    fn variant_field_name(&self, variant: VariantId, field: u32, out: &mut Text<'_>) {
        let fields = variant.fields(self.db);
        match fields.fields().iter().nth(field as usize) {
            Some((_, data)) => out.push(data.name.as_str()),
            None => write!(out, "{field}").expect("a `Text` takes every write"),
        }
    }

    fn field_name(&self, ty: PlaceTy<'db>, field: u32, out: &mut Text<'_>) {
        let variant = match (ty.variant_id, ty.ty.kind()) {
            (Some(variant), _) => variant,
            (None, TyKind::Adt(def, _)) if !def.is_enum() => {
                match VariantId::from_non_enum(def.def_id()) {
                    Some(it) => it,
                    None => return write!(out, "{field}").expect("a `Text` takes every write"),
                }
            }
            _ => return write!(out, "{field}").expect("a `Text` takes every write"),
        };
        self.variant_field_name(variant, field, out)
    }

    /// In a closure, the capture `local` with the first projections of a place stands for,
    /// and how many of the projections that is: `(*_1).0`, or the local lowering copied the
    /// capture into, a capture by reference behind the `Deref` after it.
    pub(super) fn upvar_of_projection(
        &self,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Option<(usize, usize)> {
        if let Some(upvar) = self.upvar_of_local(local) {
            let deref =
                self.upvars[upvar].by_ref && projection.first() == Some(&ProjectionElem::Deref);
            return Some((upvar, deref as usize));
        }
        if Some(local) != self.closure_local {
            return None;
        }
        let (field, used) = match projection {
            [ProjectionElem::Deref, ProjectionElem::Field(field), ..] => (field.0 as usize, 2),
            [ProjectionElem::Field(field), ..] => (field.0 as usize, 1),
            _ => return None,
        };
        let _ = body;
        let upvar = self.upvars.get(field)?;
        let deref = upvar.by_ref && projection.get(used) == Some(&ProjectionElem::Deref);
        Some((field, used + deref as usize))
    }
}
