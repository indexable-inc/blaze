//! E0747 "type provided when a lifetime was expected": a generic argument of the wrong kind for
//! the parameter rustc matches it with, with the note on the order of kinds and the reordered
//! parameters, and the suggestions for a type given to a const parameter (braces, an array's
//! length, making the type parameter const) and the helps for a function given to a type one.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/generics.rs`
//! (`lower_generic_args`, `generic_arg_mismatch_err`), `compiler/rustc_hir/src/hir.rs`
//! (`GenericArg::descr`, `GenericArg::to_ord`), `compiler/rustc_middle/src/ty/generics.rs`
//! (`GenericParamDefKind::descr`) and `compiler/rustc_ast/src/ast.rs` (`ParamKindOrd`) (MIT or
//! Apache-2.0).

use blaze_ids::radix;
use either::Either;
use hir::{GenericArgKind, GenericDef, InFile, ModuleDef, PathResolution};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lower::{
        Found,
        incorrect_generics_len::{phase_at, site_fact},
    },
    model::{Applicability, Diag},
};

/// A generic argument as rustc's HIR has it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Arg {
    Lifetime,
    Type,
    Const,
    Infer,
}

impl Arg {
    fn of(arg: ast::GenericArg<'_>) -> Option<Arg> {
        Some(match arg {
            ast::GenericArg::LifetimeArg(_) => Arg::Lifetime,
            ast::GenericArg::ConstArg(_) => Arg::Const,
            ast::GenericArg::TypeArg(it) => match it.ty() {
                Some(ast::Type::InferType(_)) => Arg::Infer,
                _ => Arg::Type,
            },
            ast::GenericArg::AssocTypeArg(_) => return None,
        })
    }

    fn descr(self) -> &'static str {
        match self {
            Arg::Lifetime => "lifetime",
            Arg::Type => "type",
            Arg::Const => "constant",
            Arg::Infer => "inferred",
        }
    }

    /// rustc's `ParamKindOrd`: lifetimes, then types and consts.
    fn ord(self) -> u8 {
        match self {
            Arg::Lifetime => 0,
            _ => 1,
        }
    }
}

fn kind_descr(kind: GenericArgKind) -> &'static str {
    match kind {
        GenericArgKind::Lifetime => "lifetime",
        GenericArgKind::Type => "type",
        GenericArgKind::Const => "constant",
    }
}

fn kind_ord(kind: GenericArgKind) -> u8 {
    match kind {
        GenericArgKind::Lifetime => 0,
        _ => 1,
    }
}

fn ord_name(ord: u8) -> &'static str {
    match ord {
        0 => "lifetime",
        _ => "type and const",
    }
}

/// A parameter of the definition, as rustc's `own_params` has it.
struct Param {
    kind: GenericArgKind,
    name: String,
    /// An `impl Trait` argument's, which an argument is never matched with.
    synthetic: bool,
    source: Option<Either<hir::TypeParam, hir::ConstParam>>,
}

fn params(cx: &Cx<'_, '_>, def: GenericDef) -> Vec<Param> {
    let db = cx.db;
    let lifetimes = def.lifetime_params(db).into_iter().map(|it| Param {
        kind: GenericArgKind::Lifetime,
        // alloc: the error's text, for an error hir found
        name: it.name(db).display(db, cx.edition).to_string(),
        synthetic: false,
        source: None,
    });
    let others = def.type_or_const_params(db).into_iter().filter_map(|it| {
        // alloc: the error's text, for an error hir found
        let name = it.name(db).display(db, cx.edition).to_string();
        Some(match it.split(db) {
            Either::Left(param) => Param {
                kind: GenericArgKind::Const,
                name,
                synthetic: false,
                source: Some(Either::Right(param)),
            },
            Either::Right(param) if param.is_implicit(db) && matches!(def, GenericDef::Trait(_)) => {
                return None;
            }
            Either::Right(param) => Param {
                kind: GenericArgKind::Type,
                name,
                synthetic: param.is_implicit(db),
                source: Some(Either::Left(param)),
            },
        })
    });
    // alloc: the parameters the error names, for an error hir found
    lifetimes.chain(others).collect()
}

/// The definition the arguments of `list` are given to.
fn def_of<'db>(cx: &Cx<'_, 'db>, list: ast::GenericArgList<'db>) -> Option<GenericDef> {
    let owner = list.syntax().parent()?;
    if let Some(call) = ast::MethodCallExpr::cast(owner) {
        return Some(GenericDef::Function(cx.vsema.resolve_method_call(call)?));
    }
    let segment = ast::PathSegment::cast(owner)?;
    Some(match cx.vsema.resolve_path(segment.parent_path())? {
        PathResolution::Def(ModuleDef::Adt(it)) => it.into(),
        PathResolution::Def(ModuleDef::EnumVariant(it)) => it.parent_enum(cx.db).into(),
        PathResolution::Def(ModuleDef::Function(it)) => it.into(),
        PathResolution::Def(ModuleDef::Trait(it)) => it.into(),
        PathResolution::Def(ModuleDef::TypeAlias(it)) => it.into(),
        PathResolution::Def(ModuleDef::Const(it)) => it.into(),
        PathResolution::Def(ModuleDef::Static(it)) => it.into(),
        _ => return None,
    })
}

/// What rustc's `lower_generic_args` reports for the arguments: the argument, the parameter,
/// whether it is an ordering error, and the reordered parameters.
struct Mismatch {
    arg: usize,
    param: usize,
    ordering: bool,
    help: Option<String>,
}

fn mismatch(args: &[Arg], params: &[Param]) -> Option<Mismatch> {
    let (mut a, mut p) = (0, 0);
    let mut force_infer_lt = None;
    loop {
        match (args.get(a), params.get(p)) {
            (Some(&arg), Some(param)) => match (arg, param.kind) {
                (Arg::Type, GenericArgKind::Type) if param.synthetic => p += 1,
                (Arg::Lifetime, GenericArgKind::Lifetime)
                | (Arg::Type | Arg::Infer, GenericArgKind::Type)
                | (Arg::Const | Arg::Infer, GenericArgKind::Const) => {
                    a += 1;
                    p += 1;
                }
                (_, GenericArgKind::Lifetime) => {
                    force_infer_lt = Some((a, p));
                    p += 1;
                }
                _ => {
                    let mut present: Vec<(u8, &Param)> =
                        // alloc: the error's help, for an error hir found
                        params.iter().map(|it| (kind_ord(it.kind), it)).collect();
                    radix::sort_by_key(&mut present, |(ord, _)| *ord);
                    // alloc: the error's help, for an error hir found
                    let mut ords: Vec<u8> = present.iter().map(|(ord, _)| *ord).collect();
                    ords.dedup();
                    // alloc: the error's help, for an error hir found
                    let help = format!(
                        "reorder the arguments: {}: `<{}>`",
                        // alloc: the error's help, for an error hir found
                        ords.iter().map(|it| format!("{}s", ord_name(*it))).collect::<Vec<_>>().join(", then "),
                        present
                            .iter()
                            .filter(|(_, it)| it.name != "Self")
                            // clone: the error's help, for an error hir found
                            .map(|(_, it)| it.name.clone())
                            // alloc: the error's help, for an error hir found
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    let rest = &args[a..];
                    let sorted = rest.windows(2).all(|it| it[0].ord() <= it[1].ord());
                    return Some(Mismatch { arg: a, param: p, ordering: !sorted, help: Some(help) });
                }
            },
            (Some(_), None) => {
                let (arg, param) = force_infer_lt?;
                return Some(Mismatch { arg, param, ordering: false, help: None });
            }
            (None, _) => return None,
        }
    }
}

/// The function a const argument names, `{ f }`.
fn const_arg_fn<'db>(cx: &Cx<'_, 'db>, arg: ast::ConstArg<'db>) -> Option<hir::Function> {
    let expr = match arg.expr()? {
        ast::Expr::BlockExpr(block) => {
            let list = block.stmt_list()?;
            if list.statements().next().is_some() {
                return None;
            }
            list.tail_expr()?
        }
        it => it,
    };
    let ast::Expr::PathExpr(path) = expr else { return None };
    match cx.vsema.resolve_path(path.path()?)? {
        PathResolution::Def(ModuleDef::Function(it)) => Some(it),
        _ => None,
    }
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::IncorrectGenericsOrder) -> Vec<Found> {
    let db = cx.db;
    let provided = cx.node_view(d.provided_arg);
    let file = provided.file_id;
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let arg_node = provided.value;
    let arg_range = arg_node.syntax().text_range();
    // alloc: empty, allocates nothing
    let Some(arg_kind) = Arg::of(arg_node) else { return Vec::new() };

    let list = arg_node.syntax().parent().and_then(ast::GenericArgList::cast);
    let found = list.and_then(|list| {
        let def = def_of(cx, list)?;
        // alloc: the arguments' kinds, on the error path
        let kinds: Vec<Arg> = list.generic_args().filter_map(Arg::of).collect();
        let params = params(cx, def);
        let m = mismatch(&kinds, &params)?;
        let at = list.generic_args().filter(|it| Arg::of(*it).is_some()).nth(m.arg)?.syntax().text_range();
        (at == arg_range).then_some((m, params))
    });

    let (expected_descr, expected_ord) = match &found {
        Some((m, params)) => (kind_descr(params[m.param].kind), kind_ord(params[m.param].kind)),
        None => (kind_descr(d.expected_kind), kind_ord(d.expected_kind)),
    };
    let mut diag = Diag::error(
        Some("E0747"),
        // alloc: the error's text, for an error hir found
        format!("{} provided when a {expected_descr} was expected", arg_kind.descr()),
        span(arg_range),
    );

    let braces = |diag: &mut Diag| {
        diag.multipart_suggestion(
            "if this generic argument was intended as a const parameter, surround it with braces",
            // alloc: the suggestion's parts, for an error hir found
            vec![
                // alloc: the suggestion's text, for an error hir found
                (span(arg_range).shrink_to_lo(), "{ ".to_owned()),
                // alloc: the suggestion's text, for an error hir found
                (span(arg_range).shrink_to_hi(), " }".to_owned()),
            ],
            Applicability::MaybeIncorrect,
        );
    };
    let param = found.as_ref().map(|(m, params)| &params[m.param]);
    let expects_const = expected_descr == "constant";
    match (arg_node, expects_const) {
        (ast::GenericArg::TypeArg(it), true) => match it.ty() {
            Some(ast::Type::PathType(ty)) => {
                let res = ty.path().and_then(|path| cx.vsema.resolve_path(path));
                match res {
                    None => {
                        braces(&mut diag);
                        // alloc: the error's text, for an error hir found
                        diag.message = "unresolved item provided when a constant was expected".to_owned();
                    }
                    Some(PathResolution::TypeParam(src)) => {
                        let target = param.and_then(|it| match it.source {
                            Some(Either::Right(it)) => Some(it),
                            _ => None,
                        });
                        let src_range = cx
                            .vsema
                            .source(src.merge())
                            .and_then(|it| match it.value {
                                Either::Left(ast::TypeOrConstParam::Type(p)) => {
                                    Some(it.with_value(ast::type_param_span(p)?))
                                }
                                _ => None,
                            });
                        if let (Some(target), Some(src_range)) = (target, src_range) {
                            // alloc: the suggestion's text, for an error hir found
                            let name = target.name(db).display(db, cx.edition).to_string();
                            diag.suggestion_verbose(
                                cx.span(src_range),
                                "consider changing this type parameter to a const parameter",
                                // alloc: the suggestion's text, for an error hir found
                                format!("const {name}: {}", cx.ty(&target.ty(db))),
                                Applicability::MaybeIncorrect,
                            );
                        }
                    }
                    Some(_) => braces(&mut diag),
                }
            }
            Some(ast::Type::ArrayType(array)) => {
                let usize_param = param.is_some_and(|it| {
                    matches!(it.source, Some(Either::Right(c)) if c.ty(db).is_usize())
                });
                if usize_param
                    && let Some(len) = array.const_arg()
                    && let Some(snippet) = cx.snippet(&span(len.syntax().text_range()))
                {
                    diag.suggestion(
                        span(arg_range),
                        "array type provided where a `usize` was expected, try",
                        // alloc: the suggestion's text, for an error hir found
                        format!("{{ {snippet} }}"),
                        Applicability::MaybeIncorrect,
                    );
                }
            }
            _ => {}
        },
        (ast::GenericArg::ConstArg(it), false) if expected_descr == "type" => {
            if let Some(f) = const_arg_fn(cx, it) {
                // alloc: the error's text, for an error hir found
                diag.help(format!(
                    "`{}` is a function item, not a type",
                    f.name(db).display(db, cx.edition)
                ));
                diag.help("function item types cannot be named directly");
            }
        }
        _ => {}
    }

    if let Some((m, _)) = &found
        && m.ordering
        && expected_ord != arg_kind.ord()
    {
        let (first, last) = match expected_ord < arg_kind.ord() {
            true => (expected_descr, arg_kind.descr()),
            false => (arg_kind.descr(), expected_descr),
        };
        // alloc: the error's text, for an error hir found
        diag.note(format!("{first} arguments must be provided before {last} arguments"));
        if let Some(help) = &m.help {
            // clone: the error's text, for an error hir found
            diag.help(help.clone());
        }
    }
    site_fact(cx, &mut diag, InFile::new(file, arg_node.syntax()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, phase_at(arg_node.syntax()))]
}
