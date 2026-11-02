//! E0072: a struct, enum or union that holds itself by value, through its fields, arrays,
//! tuples and the type parameters other types hold by value, so it has no finite size.
//!
//! Follows rustc's `compiler/rustc_ty_utils/src/representability.rs` (`check_representability`,
//! `check_representability_adt_ty`, `params_in_repr`) and
//! `compiler/rustc_query_impl/src/handle_cycle_error.rs` (`check_representability_inner`,
//! `recursive_type_error`, `find_item_ty_spans`) (MIT or Apache-2.0): the walk is rustc's
//! queries, its stack their stack, a key met again on it the cycle rustc reports.

use std::rc::Rc;

use blaze_hash::HashMap;
use blaze_items::{Fact, ItemId};
use either::Either;
use hir::{FieldSourceView, InFile, PathResolution};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasGenericParams as _, HasName as _},
};

use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// How many of a cycle's types rustc names and points at.
const ITEM_LIMIT: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    OnStack,
    Representable,
    /// In a cycle reported, or on the stack when one was: rustc's poisoned query, which gives
    /// no other error.
    Infinite,
}

/// A frame of rustc's query stack.
enum Frame<'db> {
    /// `check_representability` of an ADT.
    Item(hir::Adt),
    /// `check_representability` of a field of the ADT.
    Field(hir::Field, hir::Adt),
    /// `check_representability_adt_ty`.
    AdtTy(hir::Type<'db>),
}

/// What the ADTs of the crate are found to be, across the checks of each.
#[derive(Default)]
pub(super) struct Cycles<'db> {
    state: HashMap<hir::Adt, State>,
    /// rustc's `params_in_repr`: for each type parameter of an ADT in order, whether a value
    /// of the ADT holds one by value.
    params: HashMap<hir::Adt, Rc<[bool]>>,
    /// The query stack, empty between checks and reused for each.
    stack: Vec<Frame<'db>>,
}

impl<'db> Cycles<'db> {
    /// rustc's `check_representability` of `adt`: the cycle it is in, reported once.
    pub(super) fn check(&mut self, cx: &Cx<'_, 'db>, adt: hir::Adt) -> Vec<Found> {
        if !is_local(cx, adt) {
            // alloc: empty, allocates nothing
            return Vec::new();
        }
        let stack = std::mem::take(&mut self.stack);
        // alloc: empty, allocates only when a cycle is found
        let mut walk = Walk { cx, cycles: self, stack, found: Vec::new() };
        if walk.item(adt).is_err() {
            for frame in &walk.stack {
                if let Frame::Item(it) = frame {
                    walk.cycles.state.insert(*it, State::Infinite);
                }
            }
        }
        let Walk { mut stack, found, .. } = walk;
        stack.clear();
        self.stack = stack;
        found
    }
}

struct Walk<'a, 'b, 'db> {
    cx: &'a Cx<'b, 'db>,
    cycles: &'a mut Cycles<'db>,
    stack: Vec<Frame<'db>>,
    found: Vec<Found>,
}

/// A cycle was found, or a type already in one.
struct Infinite;

impl<'db> Walk<'_, '_, 'db> {
    fn item(&mut self, adt: hir::Adt) -> Result<(), Infinite> {
        match self.cycles.state.get(&adt) {
            Some(State::Representable) => return Ok(()),
            Some(State::Infinite) => return Err(Infinite),
            Some(State::OnStack) => {
                let start = self
                    .stack
                    .iter()
                    .position(|it| matches!(it, Frame::Item(it) if *it == adt))
                    .expect("an ADT on the stack has its frame");
                self.report(start);
                return Err(Infinite);
            }
            None => {}
        }
        self.cycles.state.insert(adt, State::OnStack);
        self.stack.push(Frame::Item(adt));
        for field in fields(self.cx, adt) {
            self.stack.push(Frame::Field(field, adt));
            self.ty(field.ty(self.cx.db))?;
            self.stack.pop();
        }
        self.stack.pop();
        self.cycles.state.insert(adt, State::Representable);
        Ok(())
    }

    /// rustc's `check_representability_ty`.
    fn ty(&mut self, ty: hir::Type<'db>) -> Result<(), Infinite> {
        if ty.as_adt().is_some() {
            return self.adt_ty(ty);
        }
        if let Some((element, _)) = ty.as_array(self.cx.db) {
            return self.ty(element);
        }
        if ty.is_tuple() {
            for field in ty.tuple_fields(self.cx.db) {
                self.ty(field)?;
            }
        }
        Ok(())
    }

    /// rustc's `check_representability_adt_ty`: the ADT itself, then the arguments it holds by
    /// value.
    fn adt_ty(&mut self, ty: hir::Type<'db>) -> Result<(), Infinite> {
        if let Some(start) = self.stack.iter().position(|it| matches!(it, Frame::AdtTy(it) if *it == ty)) {
            self.report(start);
            return Err(Infinite);
        }
        let adt = ty.as_adt().expect("an ADT's type");
        // clone: an interned type, kept on the stack while its arguments are walked
        self.stack.push(Frame::AdtTy(ty.clone()));
        if is_local(self.cx, adt) {
            self.item(adt)?;
        }
        let in_repr = params_in_repr(self.cx, &mut self.cycles.params, adt);
        for (index, arg) in ty.type_arguments().enumerate() {
            if in_repr.get(index) == Some(&true) {
                self.ty(arg)?;
            }
        }
        self.stack.pop();
        Ok(())
    }

    /// rustc's `check_representability_inner` and `recursive_type_error` of the cycle of the
    /// frames from `start` up.
    fn report(&mut self, start: usize) {
        let cx = self.cx;
        let frames = &self.stack[start..];
        let mut items: Vec<(hir::Adt, hir::Field)> = frames
            .iter()
            .filter_map(|it| match it {
                Frame::Field(field, adt) => Some((*adt, *field)),
                _ => None,
            })
            // alloc: on the error path (a cycle was found)
            .collect();
        let representable: Vec<hir::Adt> = frames
            .iter()
            .filter_map(|it| match it {
                Frame::AdtTy(ty) => ty.as_adt(),
                _ => None,
            })
            .filter(|adt| is_local(cx, *adt) && !items.iter().any(|(it, _)| it == adt))
            // alloc: as above
            .collect();
        for (adt, _) in &items {
            self.cycles.state.insert(*adt, State::Infinite);
        }
        if items.is_empty() {
            return;
        }
        // alloc: as above
        let spans: Vec<Option<Span>> = items.iter().map(|(adt, _)| def_span(cx, *adt)).collect();
        // clone: as above
        let first = spans.iter().flatten().next().map(|it| it.file.clone());
        let lowest = spans
            .iter()
            .enumerate()
            .min_by_key(|(_, span)| match span {
                Some(span) if Some(&span.file) == first.as_ref() => (0, span.lo),
                _ => (1, 0),
            })
            .map_or(0, |(index, _)| index);
        items.rotate_left(lowest);
        let mut spans = spans;
        spans.rotate_left(lowest);

        let len = items.len();
        let shown = len.min(ITEM_LIMIT);
        // alloc: clone: as above
        let primary: Vec<Span> = spans[..shown].iter().flatten().cloned().collect();
        let Some(first_span) = primary.first().cloned() else { return };
        // alloc: the error's message
        let mut list = String::new();
        for (index, (adt, _)) in items.iter().enumerate() {
            // alloc: the error's message
            list.push_str(&format!("`{}`", cx.def_path(hir::ModuleDef::Adt(*adt))));
            if index == ITEM_LIMIT - 1 && len > ITEM_LIMIT {
                // alloc: the error's message
                list.push_str(&format!(" and {} more", len - ITEM_LIMIT));
                break;
            }
            if len > 1 && index < len - 2 {
                list.push_str(", ");
            } else if len > 1 && index == len - 2 {
                list.push_str(" and ");
            }
        }
        let (types, has) = match len {
            1 => ("type", "has"),
            _ => ("types", "have"),
        };
        let mut diag =
            // alloc: the error's message
            Diag::error(Some("E0072"), format!("recursive {types} {list} {has} infinite size"), first_span);
        for span in &primary[1..] {
            // clone: the error's span
            diag.primary(span.clone());
        }
        let mut suggestion = Vec::with_capacity(shown * 2);
        for index in 0..shown {
            let (_, field) = items[index];
            let (next, _) = items[(index + 1) % len];
            let Some(source) = cx.vsema.source(field) else { continue };
            let ty = match source.value {
                FieldSourceView::Named(it) => it.ty(),
                FieldSourceView::Pos(it) => it.ty(),
            };
            let Some(ty) = ty else { continue };
            // alloc: on the error path (the error's labels)
            let mut found = Vec::new();
            self.find_spans(ty, next, &representable, &mut found);
            if found.is_empty() {
                found.push(ty.syntax().text_range());
            }
            for range in found {
                let span = cx.span(source.with_value(range));
                // clone: the error's span
                diag.span_label(span.clone(), "recursive without indirection");
                // alloc: the error's suggestion
                suggestion.push((span.shrink_to_lo(), "Box<".to_owned()));
                // alloc: the error's suggestion
                suggestion.push((span.shrink_to_hi(), ">".to_owned()));
            }
        }
        diag.multipart_suggestion(
            "insert some indirection (e.g., a `Box`, `Rc`, or `&`) to break the cycle",
            suggestion,
            Applicability::HasPlaceholders,
        );
        for (adt, _) in &items {
            // alloc: the error's fact
            let what = format!("the fields of `{}`", cx.def_path(hir::ModuleDef::Adt(*adt)));
            diag.fact(Fact::Layout(cx.key(ItemId::Adt((*adt).into()))), what);
        }
        self.found.push(Found::new(diag, Phase::Items));
    }

    /// rustc's `find_item_ty_spans`: where the written type `ty` names `needle` by value.
    fn find_spans(
        &mut self,
        ty: ast::Type<'db>,
        needle: hir::Adt,
        representable: &[hir::Adt],
        found: &mut Vec<TextRange>,
    ) {
        let cx = self.cx;
        match ty {
            ast::Type::PathType(it) => {
                let Some(path) = it.path() else { return };
                let Some(segment) = path.segment() else { return };
                if segment.self_type_token().is_some() && path.qualifier().is_none() {
                    return;
                }
                let Some(PathResolution::Def(hir::ModuleDef::Adt(adt))) = cx.vsema.resolve_path(path)
                else {
                    return;
                };
                let check_params = match is_local(cx, adt) {
                    true => {
                        if adt == needle {
                            found.push(ty.syntax().text_range());
                        }
                        representable.contains(&adt)
                    }
                    false => true,
                };
                let Some(args) = segment.generic_arg_list() else { return };
                if !check_params {
                    return;
                }
                let in_repr = all_params_in_repr(cx, &mut self.cycles.params, adt);
                for (index, arg) in args.generic_args().enumerate().take(in_repr.len()) {
                    if let ast::GenericArg::TypeArg(arg) = arg
                        && in_repr[index]
                        && let Some(ty) = arg.ty()
                    {
                        self.find_spans(ty, needle, representable, found);
                    }
                }
            }
            ast::Type::ArrayType(it) => {
                if let Some(ty) = it.ty() {
                    self.find_spans(ty, needle, representable, found);
                }
            }
            ast::Type::TupleType(it) => {
                for ty in it.fields() {
                    self.find_spans(ty, needle, representable, found);
                }
            }
            ast::Type::ParenType(it) => {
                if let Some(ty) = it.ty() {
                    self.find_spans(ty, needle, representable, found);
                }
            }
            _ => {}
        }
    }
}

fn is_local(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    adt.module(cx.db).krate(cx.db) == cx.krate
}

/// The fields of every variant of `adt`, in order.
fn fields(cx: &Cx<'_, '_>, adt: hir::Adt) -> impl Iterator<Item = hir::Field> {
    match adt {
        hir::Adt::Struct(it) => Either::Left(hir::Variant::from(it).fields_iter(cx.db)),
        hir::Adt::Union(it) => Either::Left(hir::Variant::from(it).fields_iter(cx.db)),
        hir::Adt::Enum(it) => Either::Right(it.variants_iter(cx.db).flat_map(move |it| it.fields_iter(cx.db))),
    }
}

/// The type parameters of `adt`, in order.
fn type_params(cx: &Cx<'_, '_>, adt: hir::Adt) -> Vec<hir::TypeParam> {
    hir::GenericDef::from(adt)
        .type_or_const_params(cx.db)
        .into_iter()
        .filter_map(|it| it.as_type_param(cx.db))
        // alloc: once per ADT, its result memoized
        .collect()
}

/// rustc's `params_in_repr`, of the type parameters only.
fn params_in_repr(
    cx: &Cx<'_, '_>,
    memo: &mut HashMap<hir::Adt, Rc<[bool]>>,
    adt: hir::Adt,
) -> Rc<[bool]> {
    if let Some(it) = memo.get(&adt) {
        // clone: a reference count
        return it.clone();
    }
    // A type met again while its own parameters are being found holds none more by value.
    memo.insert(adt, Rc::from(&[][..]));
    let params = type_params(cx, adt);
    // alloc: once per ADT, memoized
    let mut in_repr = vec![false; params.len()];
    for field in fields(cx, adt) {
        params_in_repr_ty(cx, memo, &field.ty(cx.db), &params, &mut in_repr);
    }
    let in_repr: Rc<[bool]> = in_repr.into();
    // clone: a reference count
    memo.insert(adt, in_repr.clone());
    in_repr
}

/// rustc's `params_in_repr_ty`.
fn params_in_repr_ty(
    cx: &Cx<'_, '_>,
    memo: &mut HashMap<hir::Adt, Rc<[bool]>>,
    ty: &hir::Type<'_>,
    params: &[hir::TypeParam],
    in_repr: &mut [bool],
) {
    if let Some(adt) = ty.as_adt() {
        let inner = params_in_repr(cx, memo, adt);
        for (index, arg) in ty.type_arguments().enumerate() {
            if inner.get(index) == Some(&true) {
                params_in_repr_ty(cx, memo, &arg, params, in_repr);
            }
        }
    } else if let Some((element, _)) = ty.as_array(cx.db) {
        params_in_repr_ty(cx, memo, &element, params, in_repr);
    } else if ty.is_tuple() {
        for field in ty.tuple_fields(cx.db) {
            params_in_repr_ty(cx, memo, &field, params, in_repr);
        }
    } else if let Some(param) = ty.as_type_param(cx.db)
        && let Some(index) = params.iter().position(|it| *it == param)
    {
        in_repr[index] = true;
    }
}

/// rustc's `params_in_repr` over all of `adt`'s generic parameters as rustc numbers them
/// (lifetimes first), for the arguments as written.
fn all_params_in_repr(
    cx: &Cx<'_, '_>,
    memo: &mut HashMap<hir::Adt, Rc<[bool]>>,
    adt: hir::Adt,
) -> Vec<bool> {
    let def = hir::GenericDef::from(adt);
    let types = params_in_repr(cx, memo, adt);
    // alloc: on the error path (E0072's spans)
    let mut all = vec![false; def.lifetime_params(cx.db).len()];
    let mut type_index = 0;
    for param in def.type_or_const_params(cx.db) {
        match param.as_type_param(cx.db) {
            Some(_) => {
                all.push(types.get(type_index).copied().unwrap_or(false));
                type_index += 1;
            }
            None => all.push(false),
        }
    }
    all
}

/// rustc's `def_span` of an ADT: from where it starts to the end of its name or generics.
fn def_span(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<Span> {
    let source = cx.vsema.source(adt)?;
    let node = source.value;
    let start = item_range(node.syntax()).start();
    let name_end = node.name().map(|it| it.syntax().text_range().end());
    let generics_end = node.generic_param_list().map(|it| it.syntax().text_range().end());
    let end = name_end.max(generics_end)?;
    Some(cx.span(InFile::new(source.file_id, TextRange::new(start, end))))
}
