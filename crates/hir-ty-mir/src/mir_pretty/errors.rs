//! Pretty-printing MIR and consteval errors.

use std::fmt::Write;

use base_db::Db;
use either::Either;
use hir_def::{
    GenericParamId, ItemContainerId, Lookup,
    expr_store::{Body, ExpressionStore},
    hir::generics::GenericParams,
    signatures::{FunctionSignature, TraitSignature},
};
use hir_expand::{InFile, name::Name};
use hir_ty::{
    InferBodyId,
    consteval::ConstEvalError,
    db::HirDatabase,
    mir::{ComptimeError, ComptimeErrorKind, Dispatch, MirEvalError, MirLowerError, MirSpan},
};
use macros::extension;
use span::{FileId, TextRange};
use syntax::SyntaxNodePtr;

use crate::display::{ClosureStyle, DisplayTarget, HirDisplay};

#[extension(pub trait MirLowerErrorPretty)]
impl MirLowerError<'_> {
    fn pretty_print(
        &self,
        f: &mut String,
        db: &Db,
        span_formatter: impl Fn(FileId, TextRange) -> String,
        display_target: DisplayTarget,
    ) -> std::result::Result<(), std::fmt::Error> {
        match self {
            MirLowerError::ConstEvalError(name, e) => {
                writeln!(f, "In evaluating constant {name}")?;
                match &**e {
                    ConstEvalError::MirLowerError(e) => {
                        e.pretty_print(f, db, span_formatter, display_target)?
                    }
                    ConstEvalError::MirEvalError(e) => {
                        e.pretty_print(f, db, span_formatter, display_target)?
                    }
                }
            }
            MirLowerError::MissingFunctionDefinition(owner, it) => {
                let owner = owner.expression_store_owner(db);
                let store = ExpressionStore::of(db, owner);
                writeln!(
                    f,
                    "Missing function definition for {}",
                    hir_def::expr_store::pretty::print_expr_hir(
                        db,
                        store,
                        owner,
                        *it,
                        display_target.edition
                    )
                )?;
            }
            MirLowerError::HasErrors => writeln!(f, "Type inference result contains errors")?,
            MirLowerError::GenericArgNotProvided(id, subst) => {
                let param_name = match *id {
                    GenericParamId::TypeParamId(id) => {
                        GenericParams::of(db, id.parent())[id.local_id()].name().cloned()
                    }
                    GenericParamId::ConstParamId(id) => {
                        GenericParams::of(db, id.parent())[id.local_id()].name().cloned()
                    }
                    GenericParamId::LifetimeParamId(id) => {
                        Some(GenericParams::of(db, id.parent)[id.local_id].name.clone())
                    }
                };
                writeln!(
                    f,
                    "Generic arg not provided for {}",
                    param_name.unwrap_or(Name::missing()).display(db, display_target.edition)
                )?;
                writeln!(f, "Provided args: [")?;
                for g in subst.as_ref() {
                    write!(f, "    {},", g.display(db, display_target))?;
                }
                writeln!(f, "]")?;
            }
            MirLowerError::UnknownIntrinsic(name) => {
                writeln!(f, "{}", hir_ty::intrinsic::UnknownIntrinsic(name.clone()))?
            }
            MirLowerError::ComptimeUnexpanded(it) => writeln!(f, "{it}")?,
            MirLowerError::RpititValue(message) => writeln!(f, "{message}")?,
            MirLowerError::LayoutError(_)
            | MirLowerError::IncompleteExpr
            | MirLowerError::IncompletePattern
            | MirLowerError::InaccessibleLocal
            | MirLowerError::TraitFunctionDefinition(_, _)
            | MirLowerError::UnresolvedName { .. }
            | MirLowerError::RecordLiteralWithoutPath
            | MirLowerError::UnresolvedMethod(_)
            | MirLowerError::UnresolvedField
            | MirLowerError::TypeError(_)
            | MirLowerError::NotSupported(_)
            | MirLowerError::ContinueWithoutLoop
            | MirLowerError::BreakWithoutLoop
            | MirLowerError::Loop
            | MirLowerError::ImplementationError(_)
            | MirLowerError::LangItemNotFound
            | MirLowerError::MutatingRvalue
            | MirLowerError::UnresolvedLabel
            | MirLowerError::UnresolvedUpvar(_) => writeln!(f, "{self:?}")?,
        }
        Ok(())
    }
}

#[extension(pub trait MirEvalErrorPretty)]
impl MirEvalError<'_> {
    fn pretty_print(
        &self,
        f: &mut String,
        db: &Db,
        span_formatter: impl Fn(FileId, TextRange) -> String,
        display_target: DisplayTarget,
    ) -> std::result::Result<(), std::fmt::Error> {
        writeln!(f, "Mir eval error:")?;
        let mut err = self;
        while let MirEvalError::InFunction(e, stack) = err {
            err = e;
            for (func, span, def) in stack.iter().take(30).rev() {
                match func {
                    Either::Left(func) => {
                        let function_name = FunctionSignature::of(db, *func);
                        writeln!(
                            f,
                            "In function {} ({:?})",
                            function_name.name.display(db, display_target.edition),
                            func
                        )?;
                    }
                    Either::Right(closure) => {
                        writeln!(f, "In {closure:?}")?;
                    }
                }
                let Some((file_id, range)) = source_range(db, *def, *span) else { continue };
                writeln!(f, "{}", span_formatter(file_id, range))?;
            }
        }
        match err {
            MirEvalError::InFunction(..) => unreachable!(),
            MirEvalError::LayoutError(err, ty) => {
                write!(
                    f,
                    "Layout for type `{}` is not available due {err:?}",
                    ty.as_ref()
                        .display(db, display_target)
                        .with_closure_style(ClosureStyle::ClosureWithId)
                )?;
            }
            MirEvalError::MirLowerError(func, err) => {
                let function_name = FunctionSignature::of(db, *func);
                let self_ = match func.lookup(db).container {
                    ItemContainerId::ImplId(impl_id) => Some({
                        db.impl_self_ty(impl_id)
                            .instantiate_identity()
                            .skip_norm_wip()
                            .display(db, display_target)
                            .to_string()
                    }),
                    ItemContainerId::TraitId(it) => Some(
                        TraitSignature::of(db, it)
                            .name
                            .display(db, display_target.edition)
                            .to_string(),
                    ),
                    _ => None,
                };
                writeln!(
                    f,
                    "MIR lowering for function `{}{}{}` ({:?}) failed due:",
                    self_.as_deref().unwrap_or_default(),
                    if self_.is_some() { "::" } else { "" },
                    function_name.name.display(db, display_target.edition),
                    func
                )?;
                err.pretty_print(f, db, span_formatter, display_target)?;
            }
            MirEvalError::ConstEvalError(name, err) => {
                MirLowerError::ConstEvalError((**name).into(), err.clone()).pretty_print(
                    f,
                    db,
                    span_formatter,
                    display_target,
                )?;
            }
            MirEvalError::OverBudget { spent, at } => {
                write!(f, "const eval {spent}")?;
                if let Some((owner, span)) = at
                    && let Some((file_id, range)) = source_range(db, *owner, *span)
                {
                    write!(f, " at {}", span_formatter(file_id, range))?;
                }
                writeln!(f, " (raise {} under `[workspace.metadata.blaze]`)", spent.setting())?;
            }
            MirEvalError::LongRunning { at } => {
                f.write_str("constant evaluation is taking a long time")?;
                if let Some((owner, span)) = at
                    && let Some((file_id, range)) = source_range(db, *owner, *span)
                {
                    write!(f, " at {}", span_formatter(file_id, range))?;
                }
                writeln!(
                    f,
                    "\nnote: this lint makes sure the compiler doesn't get stuck due to infinite loops in \
                     const eval.\n    If your compilation actually takes a long time, you can safely \
                     allow the lint"
                )?;
            }
            MirEvalError::UndefinedBehavior(_)
            | MirEvalError::TargetDataLayoutNotAvailable(_)
            | MirEvalError::Panic(_)
            | MirEvalError::Aborted(_)
            | MirEvalError::MirLowerErrorForClosure(_, _)
            | MirEvalError::TypeIsUnsized(_, _)
            | MirEvalError::NotSupported(_)
            | MirEvalError::InvalidConst
            | MirEvalError::StackOverflow
            | MirEvalError::HeapInValue { .. }
            | MirEvalError::CoerceUnsizedError(_)
            | MirEvalError::InternalError(_)
            | MirEvalError::MachineStop
            | MirEvalError::InvalidVTableId(_) => writeln!(f, "{err:?}")?,
            MirEvalError::ComptimeUnexpanded(it) => writeln!(f, "{it}")?,
            MirEvalError::Comptime(it) => {
                f.push_str(&comptime_error_text(db, it, display_target));
                if let Some((owner, span)) = it.at
                    && let Some((file_id, range)) = source_range(db, owner, span)
                {
                    write!(f, " at {}", span_formatter(file_id, range))?;
                }
                writeln!(f)?;
            }
        }
        Ok(())
    }

    fn is_panic(&self) -> Option<&str> {
        let mut err = self;
        while let MirEvalError::InFunction(e, _) = err {
            err = e;
        }
        match err {
            MirEvalError::Panic(msg) => Some(msg),
            _ => None,
        }
    }
}

/// The file and range of `span` in the body of `def`.
fn source_range(db: &Db, def: InferBodyId<'_>, span: MirSpan) -> Option<(FileId, TextRange)> {
    let (source_map, self_param_syntax) = match def {
        InferBodyId::DefWithBodyId(def) => {
            let body = Body::source_map(db, def);
            (&**body, body.self_param_syntax())
        }
        InferBodyId::AnonConstId(def) => {
            (ExpressionStore::with_source_map(db, def.loc(db).owner).1, None)
        }
    };
    let span: InFile<SyntaxNodePtr> = match span {
        MirSpan::ExprId(e) | MirSpan::ExprEnd(e) => {
            source_map.expr_syntax(e).ok()?.map(|it| it.into())
        }
        MirSpan::PatId(p) => source_map.pat_syntax(p).ok()?.map(|it| it.syntax_node_ptr()),
        MirSpan::BindingId(b) => source_map
            .patterns_for_binding(b)
            .iter()
            .find_map(|p| source_map.pat_syntax(*p).ok())?
            .map(|it| it.syntax_node_ptr()),
        MirSpan::SelfParam => self_param_syntax?.map(|it| it.syntax_node_ptr()),
        // The interpreter runs no optimized body, so no span of one reaches it.
        MirSpan::Unknown | MirSpan::Inlined(_) => return None,
    };
    Some((span.file_id.original_file(db).file_id(db), span.value.text_range()))
}

#[extension(pub trait ConstEvalErrorPretty)]
impl ConstEvalError<'_> {
    fn pretty_print(
        &self,
        f: &mut String,
        db: &Db,
        span_formatter: impl Fn(span::FileId, span::TextRange) -> String,
        display_target: DisplayTarget,
    ) -> std::result::Result<(), std::fmt::Error> {
        match self {
            ConstEvalError::MirLowerError(e) => {
                e.pretty_print(f, db, span_formatter, display_target)
            }
            ConstEvalError::MirEvalError(e) => {
                e.pretty_print(f, db, span_formatter, display_target)
            }
        }
    }
}

/// What blaze's `#![feature(blaze_comptime)]` refused (docs/v2/plan/design/comptime.md, "Phase
/// 2b"), as a diagnostic says it: the function, the dispatch or the read, by name. Where it
/// happened is the error's `at`, which the caller places.
pub fn comptime_error_text(
    db: &Db,
    error: &ComptimeError<'_>,
    display_target: DisplayTarget,
) -> String {
    match &error.kind {
        ComptimeErrorKind::NonConstTarget { callee, args, through, bounds } => {
            let through = match through {
                // alloc: the text of a diagnostic, made when it is reported
                Dispatch::FnPointer => "a function pointer".to_owned(),
                Dispatch::Vtable { dyn_ty } => {
                    let dyn_ty = dyn_ty.as_ref();
                    // alloc: the text of a diagnostic, made when it is reported
                    format!("the vtable of `{}`", dyn_ty.display(db, display_target))
                }
            };
            let (callee, why) = match callee {
                Either::Left(func) => {
                    let args = args.as_ref();
                    let name = hir_ty_print::rustc_function_name(db, *func, args.as_slice())
                        .unwrap_or_else(|| {
                            // alloc: the text of a diagnostic, made when it is reported
                            FunctionSignature::of(db, *func).name.as_str().to_owned()
                        });
                    let why = match bounds {
                        true => "a const function whose `[const]` bounds do not hold const here",
                        false => "which is not a `const fn` nor a method of a `const impl`",
                    };
                    // alloc: the text of a diagnostic, made when it is reported
                    (format!("`{name}`"), why)
                }
                // alloc: the text of a diagnostic, made when it is reported
                Either::Right(_) => ("a closure".to_owned(), "which is not a `const` closure"),
            };
            // alloc: the text of a diagnostic, made when it is reported
            format!(
                "a call through {through} in a const context reached {callee}, {why}: \
                 `#![feature(blaze_comptime)]` checks such a call when it is evaluated, and \
                 only const code runs in a constant outside a `const static {{ .. }}` block"
            )
        }
        ComptimeErrorKind::Foreign { func } => {
            let name = FunctionSignature::of(db, *func).name.as_str();
            // alloc: the text of a diagnostic, made when it is reported
            format!(
                "a `const static {{ .. }}` block called the foreign function `{name}`, which \
                 cannot run at compile time: I/O, time, randomness, threads, the environment and \
                 system calls are refused"
            )
        }
        // alloc: the text of a diagnostic, made when it is reported
        ComptimeErrorKind::PointerAddress { how } => format!(
            "a `const static {{ .. }}` block read a pointer's address as an integer ({how}): \
             the evaluator's addresses are not the program's"
        ),
    }
}
