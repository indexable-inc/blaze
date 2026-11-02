//! E0109 "type arguments are not allowed on module `m`": generic arguments on a path segment
//! that takes none (a module, a type parameter, `Self`, a primitive type, a local variable, a
//! variant whose enum has them too), one error for the segments rustc checks together, with
//! the note or suggestion of what the segment is; E0229 for an associated item constraint
//! there; and E0107 for a static's or a const parameter's, which rustc counts as a value's.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs`
//! (`prohibit_generic_args`, `probe_generic_path_segments`, `lower_resolved_ty_path`),
//! `compiler/rustc_hir_analysis/src/hir_ty_lowering/errors.rs`
//! (`report_prohibited_generic_args`, `generics_args_err_extend`,
//! `prohibit_assoc_item_constraint`), `compiler/rustc_hir_typeck/src/fn_ctxt/_impl.rs`
//! (`instantiate_value_path`) and `compiler/rustc_hir/src/def.rs` (`Res::descr`) (MIT or
//! Apache-2.0).

use either::Either;
use hir::{Adt, AsAssocItem as _, GenericArgsProhibitedReason, GenericDef, InFile, ModuleDef, PathResolution, StructKind};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasName as _},
};

use stdx::list::Join;
use crate::{
    cx::{Cx, def_range_view as def_range},
    lower::{
        Found,
        incorrect_generics_len::{Segment, def_name_span, phase_at, s, site_fact, takes_none},
    },
    model::{Applicability, Diag, MultiSpan},
};

/// rustc's `ProhibitGenericsArg`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgKind {
    Lifetime,
    Type,
    Const,
    Infer,
}

/// A segment of the path, the path up to it, and what that resolves to.
struct Seg<'db> {
    segment: ast::PathSegment<'db>,
    res: Option<PathResolution<'db>>,
}

/// The note or suggestion rustc adds for what the segments are: its `GenericsArgsErrExtend`.
enum Extend {
    None,
    Param(hir::TypeParam),
    SelfTyParam,
    SelfTyAlias(Either<hir::Impl, Adt>),
    PrimTy(String),
    DefVariant,
}

/// The segments of `top`, first to last, each with what the path up to it resolves to.
fn segments<'db>(cx: &Cx<'_, 'db>, top: ast::Path<'db>) -> Vec<Seg<'db>> {
    let mut paths = Vec::new(); // alloc: the path's prefixes, on the error path
    let mut at = Some(top);
    while let Some(path) = at {
        at = path.qualifier();
        paths.push(path);
    }
    paths.reverse();
    paths
        .into_iter()
        .filter_map(|prefix| {
            let segment = prefix.segment()?;
            Some(Seg { segment, res: cx.vsema.resolve_path(prefix) })
        })
        .collect() // alloc: the path's segments, on the error path
}

fn has_args(segment: ast::PathSegment<'_>) -> bool {
    segment.generic_arg_list().is_some() || segment.parenthesized_arg_list().is_some()
}

/// The generic arguments of a segment, constraints left out: rustc's `GenericArgs::args`, the
/// inputs of `Fn`-style ones a tuple type.
fn args(segment: ast::PathSegment<'_>) -> Vec<(ArgKind, TextRange)> {
    if let Some(list) = segment.parenthesized_arg_list() {
        return vec![(ArgKind::Type, list.syntax().text_range())]; // alloc: on the error path
    }
    let Some(list) = segment.generic_arg_list() else { return Vec::new() }; // alloc: empty
    list.generic_args()
        .filter_map(|arg| {
            let kind = match arg {
                ast::GenericArg::AssocTypeArg(_) => return None,
                ast::GenericArg::LifetimeArg(_) => ArgKind::Lifetime,
                ast::GenericArg::ConstArg(_) => ArgKind::Const,
                ast::GenericArg::TypeArg(it) => match it.ty() {
                    Some(ast::Type::InferType(_)) => ArgKind::Infer,
                    _ => ArgKind::Type,
                },
            };
            Some((kind, arg.syntax().text_range()))
        })
        .collect() // alloc: a segment's arguments, on the error path
}

fn is_trait_self(cx: &Cx<'_, '_>, param: hir::TypeParam) -> bool {
    matches!(param.parent(cx.db), GenericDef::Trait(_)) && param.is_implicit(cx.db)
}

/// Whether `path` is of a value: an expression or a pattern that is not a struct's.
fn in_value(path: ast::Path<'_>) -> bool {
    path.syntax().parent().is_some_and(|it| {
        ast::PathExpr::can_cast(it.kind())
            || ast::PathPat::can_cast(it.kind())
            || ast::TupleStructPat::can_cast(it.kind())
    })
}

fn is_enum(res: &Option<PathResolution<'_>>) -> bool {
    matches!(res, Some(PathResolution::Def(ModuleDef::Adt(Adt::Enum(_)))))
}

/// The segments rustc allows arguments on for a variant: its enum's when the enum has them,
/// else the variant's.
fn variant_allowed(segs: &[Seg<'_>]) -> Vec<usize> {
    let last = segs.len() - 1;
    match last.checked_sub(1) {
        Some(i) if has_args(segs[i].segment) && is_enum(&segs[i].res) => vec![i], // alloc: error path
        _ => vec![last], // alloc: on the error path
    }
}

/// The segments rustc checks together, the ones that may have arguments among them, and its
/// extension of the error.
fn checked(cx: &Cx<'_, '_>, segs: &[Seg<'_>], value: bool) -> (Vec<usize>, Extend) {
    let db = cx.db;
    let last = segs.len() - 1;
    let all: Vec<usize> = (0..segs.len()).collect(); // alloc: on the error path
    let but = |allowed: Vec<usize>| -> Vec<usize> {
        (0..segs.len()).filter(|it| !allowed.contains(it)).collect() // alloc: on the error path
    };
    let last_seg = &segs[last];
    let is_self = last_seg.segment.self_type_token().is_some();
    if value {
        return match &last_seg.res {
            Some(PathResolution::Local(_)) | Some(PathResolution::SelfType(_)) => {
                (all, Extend::None)
            }
            Some(PathResolution::Def(ModuleDef::EnumVariant(_))) => {
                (but(variant_allowed(segs)), Extend::DefVariant)
            }
            Some(PathResolution::Def(ModuleDef::Function(it))) if it.as_assoc_item(db).is_some() => {
                // alloc: on the error path
                (but([last.checked_sub(1), Some(last)].into_iter().flatten().collect()), Extend::None)
            }
            Some(PathResolution::Def(ModuleDef::Const(it))) if it.as_assoc_item(db).is_some() => {
                // alloc: on the error path
                (but([last.checked_sub(1), Some(last)].into_iter().flatten().collect()), Extend::None)
            }
            _ => (but(vec![last]), Extend::None), // alloc: on the error path
        };
    }
    match &last_seg.res {
        Some(PathResolution::TypeParam(it)) if is_trait_self(cx, *it) => {
            let single = segs.len() == 1 && has_args(last_seg.segment);
            (all, if single { Extend::SelfTyParam } else { Extend::None })
        }
        Some(PathResolution::TypeParam(it)) => (all, Extend::Param(*it)),
        Some(PathResolution::SelfType(it)) => (all, Extend::SelfTyAlias(Either::Left(*it))),
        Some(PathResolution::Def(ModuleDef::Adt(it))) if is_self => {
            (all, Extend::SelfTyAlias(Either::Right(*it)))
        }
        Some(PathResolution::Def(ModuleDef::BuiltinType(it))) => {
            // alloc: a name for the error
            (all, Extend::PrimTy(it.name().display(db, cx.edition).to_string()))
        }
        Some(PathResolution::Def(ModuleDef::EnumVariant(_))) => {
            (but(variant_allowed(segs)), Extend::DefVariant)
        }
        Some(PathResolution::Def(ModuleDef::TypeAlias(it))) if it.as_assoc_item(db).is_some() => {
            // alloc: on the error path
            (but([last.checked_sub(1), Some(last)].into_iter().flatten().collect()), Extend::None)
        }
        _ => (but(vec![last]), Extend::None), // alloc: on the error path
    }
}

/// rustc's `Res::descr`, with the name of a definition: "module `m`", "builtin type `u8`".
fn what(cx: &Cx<'_, '_>, seg: &Seg<'_>, value_last: bool) -> String {
    let db = cx.db;
    // alloc: a label of the error
    let named = |descr: &str, name: hir::Name| format!("{descr} `{}`", name.display(db, cx.edition));
    match &seg.res {
        None => "this type".to_owned(), // alloc: a label of the error
        Some(PathResolution::Local(_)) => "local variable".to_owned(), // alloc: a label of the error
        // alloc: a label of the error
        Some(PathResolution::SelfType(_)) if value_last => "self constructor".to_owned(),
        Some(PathResolution::SelfType(_)) => "self type".to_owned(), // alloc: a label of the error
        // alloc: a label of the error
        Some(PathResolution::TypeParam(it)) if is_trait_self(cx, *it) => "self type".to_owned(),
        Some(PathResolution::TypeParam(it)) => named("type parameter", it.name(db)),
        Some(PathResolution::ConstParam(it)) => named("const parameter", it.name(db)),
        Some(PathResolution::Def(def)) => match def {
            // alloc: a label of the error
            ModuleDef::Adt(_) if seg.segment.self_type_token().is_some() => "self type".to_owned(),
            ModuleDef::Module(it) => match it.name(db) {
                Some(name) => named("module", name),
                None => "module".to_owned(), // alloc: a label of the error
            },
            ModuleDef::Adt(Adt::Struct(it)) => named("struct", it.name(db)),
            ModuleDef::Adt(Adt::Enum(it)) => named("enum", it.name(db)),
            ModuleDef::Adt(Adt::Union(it)) => named("union", it.name(db)),
            ModuleDef::EnumVariant(it) => {
                let descr = match (value_last, it.kind(db)) {
                    (true, StructKind::Tuple) => "tuple variant",
                    (true, StructKind::Unit) => "unit variant",
                    _ => "variant",
                };
                named(descr, it.name(db))
            }
            ModuleDef::Function(it) => match it.as_assoc_item(db) {
                Some(_) => named("associated function", it.name(db)),
                None => named("function", it.name(db)),
            },
            ModuleDef::Const(it) => {
                let descr = if it.as_assoc_item(db).is_some() { "associated constant" } else { "constant" };
                match it.name(db) {
                    Some(name) => named(descr, name),
                    None => descr.to_owned(), // alloc: a label of the error
                }
            }
            ModuleDef::Static(it) => named("static", it.name(db)),
            ModuleDef::Trait(it) => named("trait", it.name(db)),
            ModuleDef::TypeAlias(it) => match it.as_assoc_item(db) {
                Some(_) => named("associated type", it.name(db)),
                None => named("type alias", it.name(db)),
            },
            ModuleDef::BuiltinType(it) => named("builtin type", it.name()),
            ModuleDef::Macro(it) => named("macro", it.name(db)),
        },
        Some(_) => "this type".to_owned(), // alloc: a label of the error
    }
}

/// From the end of the segment's name to the end of its arguments: `::<T>`, `<T>`, `(A)`.
fn args_after_name(segment: ast::PathSegment<'_>) -> Option<TextRange> {
    let start = segment.name_ref()?.syntax().text_range().end();
    let end = match (segment.generic_arg_list(), segment.parenthesized_arg_list()) {
        (Some(list), _) => list.syntax().text_range().end(),
        (None, Some(list)) => match segment.ret_type() {
            Some(ret) => ret.syntax().text_range().end(),
            None => list.syntax().text_range().end(),
        },
        (None, None) => return None,
    };
    Some(TextRange::new(start, end.max(start)))
}

/// A segment's generic arguments blaze can't report as rustc does: an error naming why, never
/// nothing.
fn refused<'db>(cx: &Cx<'_, 'db>, file: hir::HirFileId, segment: ast::PathSegment<'db>, why: &str) -> Vec<Found> {
    // alloc: the error's text, for an error hir found
    let message = format!("blaze can't report these generic arguments as rustc does: {why}");
    let mut diag = Diag::error(None, message, cx.span(InFile::new(file, segment.syntax().text_range())));
    site_fact(cx, &mut diag, InFile::new(file, segment.syntax()));
    vec![Found::new(diag, phase_at(segment.syntax()))] // alloc: the error found
}

/// rustc's `lower_assoc_path` of a variant a struct path reaches through a type: one error for
/// the variant segment's arguments, the segment "this type" (the resolver left it unresolved),
/// with `GenericsArgsErrExtend::EnumVariant`'s note and suggestion.
fn type_relative_variant<'db>(
    cx: &Cx<'_, 'db>,
    segs: &[Seg<'db>],
    h: usize,
    file: hir::HirFileId,
    segment: ast::PathSegment<'db>,
) -> Vec<Found> {
    let db = cx.db;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let seg = &segs[h];
    let seg_args = args(seg.segment);
    let Some(&(_, last_range)) = seg_args.last() else {
        return refused(cx, file, segment, "hir found arguments on a segment that has none");
    };
    let mut kinds: Vec<ArgKind> = Vec::new(); // alloc: for the error's message
    for (kind, _) in &seg_args {
        if !kinds.contains(kind) {
            kinds.push(*kind);
        }
    }
    let kind_names: Vec<&str> = kinds
        .iter()
        .map(|it| match it {
            ArgKind::Lifetime => "lifetime",
            ArgKind::Type => "type",
            ArgKind::Const => "const",
            ArgKind::Infer => "generic",
        })
        .collect(); // alloc: for the error's message
    let kind = stdx::list::list(kind_names.iter(), Join::AndNoSerialComma, false);
    let mut diag = Diag::error(
        Some("E0109"),
        format!("{kind} arguments are not allowed on this type"), // alloc: the error's message
        span(seg_args[0].1),
    );
    for (_, range) in &seg_args[1..] {
        diag.primary(span(*range));
    }
    // alloc: the error's label
    diag.span_label(span(last_range), format!("{kind} argument{} not allowed", s(kinds.len())));
    if let Some(name) = seg.segment.name_ref() {
        diag.span_label(span(name.syntax().text_range()), "not allowed on this type");
    }

    diag.note("enum variants can't have type parameters");
    let Some(PathResolution::Def(ModuleDef::EnumVariant(variant))) = &seg.res else {
        return refused(cx, file, segment, "its path does not resolve to the variant hir found");
    };
    let adt = variant.parent_enum(db);
    let type_name = adt.name(db).display(db, cx.edition).to_string(); // alloc: for the suggestion
    let msg = format!("you might have meant to specify type parameters on enum `{type_name}`"); // alloc
    let Some(args_span) = args_after_name(seg.segment) else {
        return refused(cx, file, segment, "the variant's segment has no name before its arguments");
    };
    if GenericDef::Adt(Adt::Enum(adt)).params(db).is_empty() {
        diag.suggestion_verbose(
            span(args_span),
            format!("{type_name} doesn't have generic parameters"), // alloc: the suggestion's message
            "",
            Applicability::MachineApplicable,
        );
    } else {
        // rustc's `qself`: the type before the variant (`Alias` of `Alias::V`, `E` of `<E>::V`);
        // a plain path of one segment, or one whose second to last segment is the enum, gets
        // the arguments moved onto it.
        let qself_path = seg.segment.parent_path().qualifier().and_then(|q| {
            match q.segment().and_then(|it| it.type_anchor()) {
                Some(anchor) => match anchor.ty() {
                    Some(ast::Type::PathType(it)) => it.path(),
                    _ => None,
                },
                None => Some(q),
            }
        });
        let qself = qself_path.map(|path| segments(cx, path)).unwrap_or_default();
        let target = match &qself[..] {
            [only] => Some((only, only.segment.self_type_token().is_some())),
            [.., enum_seg, _] if is_enum(&enum_seg.res) => Some((enum_seg, false)),
            _ => None,
        };
        match target {
            Some((target, is_self)) => {
                // rustc's `span_to_snippet` of `args_span`, which lies in the segment.
                let seg_start = seg.segment.syntax().text_range().start();
                let seg_text = seg.segment.syntax().text();
                let snippet = &seg_text
                    [usize::from(args_span.start() - seg_start)..usize::from(args_span.end() - seg_start)];
                let Some(name) = target.segment.name_ref().map(|it| it.syntax().text_range()) else {
                    diag.note(msg);
                    return finish(cx, diag, file, segment);
                };
                let target_args = args_after_name(target.segment).unwrap_or_else(|| TextRange::empty(name.end()));
                let first = if is_self {
                    let qself_range = target.segment.syntax().text_range();
                    (span(qself_range), format!("{type_name}{snippet}")) // alloc: a part
                } else {
                    (span(target_args), snippet.to_owned()) // alloc: a part
                };
                diag.multipart_suggestion(
                    msg,
                    vec![first, (span(args_span), String::new())], // alloc: the parts
                    Applicability::MaybeIncorrect,
                );
            }
            None => {
                diag.note(msg);
            }
        }
    }
    finish(cx, diag, file, segment)
}

fn finish<'db>(cx: &Cx<'_, 'db>, mut diag: Diag, file: hir::HirFileId, segment: ast::PathSegment<'db>) -> Vec<Found> {
    site_fact(cx, &mut diag, InFile::new(file, segment.syntax()));
    vec![Found::new(diag, phase_at(segment.syntax()))] // alloc: the error found
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::GenericArgsProhibited) -> Vec<Found> {
    let db = cx.db;
    let node = cx.node_view(d.args);
    let file = node.file_id;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let owner = node.value.syntax().parent().and_then(ast::PathSegment::cast);
    let Some(segment) = owner else {
        let range = node.value.syntax().text_range();
        let mut diag = Diag::error(Some("E0109"), "type arguments are not allowed on this type", span(range));
        diag.span_label(span(range), "type argument not allowed");
        site_fact(cx, &mut diag, InFile::new(file, node.value.syntax()));
        return vec![Found::new(diag, phase_at(node.value.syntax()))]; // alloc: the error found
    };
    let top = segment.parent_path().top_path();
    let value = in_value(top);
    let segs = segments(cx, top);
    let Some(h) = segs.iter().position(|it| it.segment.syntax() == segment.syntax()) else {
        return refused(cx, file, segment, "its segment is not in its own path");
    };
    let last = segs.len() - 1;
    if d.reason == GenericArgsProhibitedReason::TypeRelativeVariant {
        return type_relative_variant(cx, &segs, h, file, segment);
    }

    // A static's or a const parameter's arguments are counted as a value's, E0107.
    if value && h == last {
        let found = match &segs[last].res {
            Some(PathResolution::Def(ModuleDef::Static(it))) => {
                Segment::of_node(file, segment.syntax(), None)
                    .map(|seg| takes_none(cx, &seg, GenericDef::Static(*it), None))
            }
            Some(PathResolution::ConstParam(it)) => {
                Segment::of_node(file, segment.syntax(), None)
                    .map(|seg| takes_none(cx, &seg, it.parent(db), Some(*it)))
            }
            _ => None,
        };
        if let Some(found) = found {
            return found;
        }
    }

    let (checked, extend) = checked(cx, &segs, value);
    let (checked, extend) = match checked.contains(&h) {
        true => (checked, extend),
        false => (vec![h], Extend::None), // alloc: on the error path
    };
    // alloc: on the error path
    let with_args: Vec<usize> = checked.iter().copied().filter(|&i| has_args(segs[i].segment)).collect();
    // The segments checked together are one error, given with the first.
    if with_args.first() != Some(&h) {
        return Vec::new(); // alloc: empty, allocates nothing
    }

    let mut found = Vec::new(); // alloc: the errors found
    let mut kinds: Vec<ArgKind> = Vec::new(); // alloc: for the error's message
    let mut arg_spans: Vec<TextRange> = Vec::new(); // alloc: the error's spans
    let mut types_and_spans: Vec<(String, TextRange)> = Vec::new(); // alloc: the error's labels
    for &i in &with_args {
        let seg = &segs[i];
        let seg_args = args(seg.segment);
        if seg_args.is_empty() {
            continue;
        }
        for (kind, range) in &seg_args {
            if !kinds.contains(kind) {
                kinds.push(*kind);
            }
            arg_spans.push(*range);
        }
        if let Some(name) = seg.segment.name_ref() {
            types_and_spans.push((what(cx, seg, value && i == last), name.syntax().text_range()));
        }
    }

    if let Some(&last_span) = arg_spans.last() {
        let kind_names: Vec<&str> = kinds
            .iter()
            .map(|it| match it {
                ArgKind::Lifetime => "lifetime",
                ArgKind::Type => "type",
                ArgKind::Const => "const",
                ArgKind::Infer => "generic",
            })
            .collect(); // alloc: for the error's message
        let kind = stdx::list::list(kind_names.iter(), Join::AndNoSerialComma, false);
        // alloc: for the error's message
        let this_type = stdx::list::list(types_and_spans.iter().map(|(it, _)| it.as_str()), Join::AndNoSerialComma, false);
        let mut diag = Diag::error(
            Some("E0109"),
            format!("{kind} arguments are not allowed on {this_type}"), // alloc: the error's message
            span(arg_spans[0]),
        );
        for range in &arg_spans[1..] {
            diag.primary(span(*range));
        }
        // alloc: the error's label
        diag.span_label(span(last_span), format!("{kind} argument{} not allowed", s(kinds.len())));
        for (what, range) in &types_and_spans {
            diag.span_label(span(*range), format!("not allowed on {what}")); // alloc: its label
        }
        extend_err(cx, &mut diag, &segs, &checked, extend, file);
        site_fact(cx, &mut diag, InFile::new(file, segment.syntax()));
        found.push(Found::new(diag, phase_at(segment.syntax())));
    }

    // Only the first constraint is an error, rustc's not to overload with them. A parenthesized
    // segment's is its `Output` (`(A) -> B` is `<(A,), Output = B>` to rustc): at the return
    // type, or just after `)` when it has none.
    let constraint = checked.iter().find_map(|&i| {
        let segment = segs[i].segment;
        if let Some(list) = segment.parenthesized_arg_list() {
            return Some(match segment.ret_type().and_then(|it| it.ty()) {
                Some(ty) => ty.syntax().text_range(),
                None => TextRange::empty(list.syntax().text_range().end()),
            });
        }
        segment.generic_arg_list()?.generic_args().find_map(|it| match it {
            ast::GenericArg::AssocTypeArg(it) => Some(it.syntax().text_range()),
            _ => None,
        })
    });
    if let Some(range) = constraint {
        let mut diag =
            Diag::error(Some("E0229"), "associated item constraints are not allowed here", span(range));
        diag.span_label(span(range), "associated item constraint not allowed here");
        site_fact(cx, &mut diag, InFile::new(file, segment.syntax()));
        found.push(Found::new(diag, phase_at(segment.syntax())));
    }
    found
}

/// rustc's `generics_args_err_extend`.
fn extend_err(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    segs: &[Seg<'_>],
    checked: &[usize],
    extend: Extend,
    file: hir::HirFileId,
) {
    let db = cx.db;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    match extend {
        Extend::None => {}
        Extend::Param(param) => {
            let Some(src) = cx.vsema.source(param.merge()) else { return };
            let Either::Left(ast::TypeOrConstParam::Type(it)) = src.value else { return };
            let Some(name) = it.name() else { return };
            let at = cx.span(src.with_value(name.syntax().text_range()));
            diag.span_note(
                at,
                // alloc: the note
                format!("type parameter `{}` defined here", param.name(db).display(db, cx.edition)),
            );
        }
        Extend::SelfTyParam => {
            let Some(range) = args_after_name(segs[0].segment) else { return };
            diag.suggestion_verbose(
                span(range),
                "the `Self` type doesn't accept type parameters",
                "",
                Applicability::MaybeIncorrect,
            );
        }
        Extend::PrimTy(name) => {
            for &i in checked {
                let Some(range) = args_after_name(segs[i].segment) else { continue };
                diag.suggestion_verbose(
                    span(range),
                    // alloc: the suggestion's message
                    format!("primitive type `{name}` doesn't have generic parameters"),
                    "",
                    Applicability::MaybeIncorrect,
                );
            }
        }
        Extend::DefVariant => {
            let args: Vec<TextRange> = segs
                .iter()
                .filter(|it| {
                    matches!(
                        it.res,
                        Some(PathResolution::Def(ModuleDef::EnumVariant(_)))
                            | Some(PathResolution::Def(ModuleDef::Adt(Adt::Enum(_))))
                    )
                })
                .filter_map(|it| args_after_name(it.segment))
                .collect(); // alloc: on the error path
            if args.len() > 1
                && let Some(&range) = args.last()
            {
                diag.note(
                    "generic arguments are not allowed on both an enum and its variant's path \
                     segments simultaneously; they are only valid in one place or the other",
                );
                diag.suggestion_verbose(
                    span(range),
                    "remove the generics arguments from one of the path segments",
                    "",
                    Applicability::MaybeIncorrect,
                );
            }
        }
        Extend::SelfTyAlias(of) => {
            let (ty, impl_span) = match of {
                Either::Left(impl_) => {
                    let impl_span =
                        cx.vsema.source(impl_).map(|src| cx.span(src.map(|it| def_range(it.syntax()))));
                    (impl_.self_ty(db), impl_span)
                }
                Either::Right(adt) => (adt.ty(db), None),
            };
            let Some(adt) = ty.as_adt() else { return };
            let type_name = adt.name(db).display(db, cx.edition).to_string(); // alloc: for the note
            let ty_span = def_name_span(cx, GenericDef::Adt(adt));
            let generics = GenericDef::Adt(adt).params(db).len();
            let msg = format!("`Self` is of type `{}`", cx.ty(&ty)); // alloc: the note
            match (impl_span, ty_span) {
                (Some(impl_span), Some(ty_span)) => {
                    let postfix = match generics {
                        0 => ", which doesn't have generic parameters",
                        _ => "",
                    };
                    let spans = MultiSpan {
                        primary: vec![ty_span.clone()], // clone: the note's span; alloc: its spans
                        // alloc: the note's labels
                        labels: vec![
                            // alloc: the note's label
                            (impl_span, format!("`Self` is on type `{type_name}` in this `impl`")),
                            // alloc: the note's label
                            (ty_span, format!("`Self` corresponds to this type{postfix}")),
                        ],
                    };
                    diag.span_note(spans, msg);
                }
                _ => {
                    diag.note(msg);
                }
            }
            for seg in segs {
                if seg.segment.self_type_token().is_none() {
                    continue;
                }
                let Some(range) = args_after_name(seg.segment) else { continue };
                if generics == 0 {
                    diag.suggestion_verbose(
                        span(range),
                        "the `Self` type doesn't accept type parameters",
                        "",
                        Applicability::MachineApplicable,
                    );
                    return;
                }
                let Some(token) = seg.segment.self_type_token() else { continue };
                diag.suggestion_verbose(
                    span(token.text_range()),
                    // alloc: the suggestion's message
                    format!(
                        "the `Self` type doesn't accept type parameters, use the concrete type's \
                         name `{type_name}` instead if you want to specify its type parameters"
                    ),
                    type_name.clone(), // clone: the suggestion's text
                    Applicability::MaybeIncorrect,
                );
            }
        }
    }
}
