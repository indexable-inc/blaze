//! `could_be_sans_io`: a module whose code already reaches no ambient effect (the wall clock,
//! threads, files, sockets, processes, the environment, stdio, OS randomness, an async runtime
//! or a syscall) but is not marked a sans-IO core. Marking it `#![blaze::sans_io]` costs
//! nothing today and keeps it so: `blaze::sans_io_core` then refuses the first call that would
//! reach one, so the module stays testable under deterministic simulation, and its results
//! depend only on its inputs.
//!
//! The walk is `sans_io_core`'s: every call of every function of the module and its children
//! (an inline module, an impl's functions, a trait's provided ones) through
//! `alloc::instance_reaches_root` with `Roots::Ambient`, through the workspace's own functions,
//! a callee already in a core taken as clean. Test code is left out (a test drives a core from
//! outside). Found once, at the highest such module: the crate root when the whole crate is
//! clean, else a module whose parent's code reaches an effect. Not found: a test, bench or
//! example target, a module already in a core, one with no function, a file of only `mod`
//! items (its module is named by its first other item), a module whose
//! walk passes [`FUNCTIONS`] functions (rule 18: a module that large is split before it is
//! marked), and an inline `mod { .. }` (only a module's own file can carry the inner attribute
//! written where its other inner attributes are).
//!
//! Class: cheap, a fold over the module's functions. Each function's answer is
//! `instance_reaches_root`'s, a tracked query, so a body edit reruns the walk of the calls it
//! changed and the module's verdict is the fold of those answers. Fix: insert
//! `#![blaze::sans_io]` after the file's inner attributes and `//!` docs, `MaybeIncorrect`
//! (under rustc the crate root needs `#![feature(register_tool, custom_inner_attributes)]` and
//! `#![register_tool(blaze)]`; under blaze it is accepted as written).

use base_db::Db;
use hir_def::{AttrDefId, DefWithBodyId, FunctionId, HasModule as _, attrs::AttrFlags, expr_store::Body, hir::Expr};
use hir_ty::{InferenceResult, ParamEnvAndCrate, db::HirDatabase as _};
use syntax::{
    SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::super::{
    Lint,
    alloc::{self, Resolved, Roots},
    sans_io_core::{core_of, module_path},
    std_mistakes::{self, Fix},
    test_code,
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::CouldBeSansIo;

/// The most functions one module's walk reads (rule 18).
const FUNCTIONS: usize = 4096;

/// What the walk of a module and its children found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// No function reaches an ambient effect; how many functions were walked.
    Clean(u32),
    /// A function reaches one.
    Reaches,
    /// The walk passed [`FUNCTIONS`].
    TooLarge,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, file: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let db = cx.db();
    if test_code::is_test_target(db, cx.krate()) {
        return;
    }
    let Some((module, first)) = module_of_file(cx, file) else { return };
    if in_core(db, module) || test_code::is_test_code(cx, first) {
        return;
    }
    let Verdict::Clean(functions) = walk(db, module) else { return };
    if functions == 0 {
        return;
    }
    // The highest clean module: the crate root, or one whose parent's code reaches an effect.
    if let Some(parent) = module.parent(db)
        && walk(db, parent) != Verdict::Reaches
    {
        return;
    }
    let text = cx.text();
    let at = after_inner_attributes(file, text);
    let insert = match at == TextSize::from(0) {
        true => "#![blaze::sans_io]\n\n".to_owned(),
        false => "\n#![blaze::sans_io]".to_owned(),
    };
    let start = first.text_range().start();
    let line_end = text[usize::from(start)..].find('\n').map_or(first.text_range().end(), |it| {
        (start + TextSize::from(u32::try_from(it).unwrap_or(0))).min(first.text_range().end())
    });
    let Some(range) = cx.range(first).map(|it| TextRange::new(it.start(), line_end.max(it.start()))) else {
        return;
    };
    let message = format!(
        "the module `{path}` reaches no ambient effect in its {functions} function{s}: mark it a sans-IO core",
        path = module_path(db, module),
        s = if functions == 1 { "" } else { "s" },
    );
    let note = "`#![blaze::sans_io]` keeps it so: `blaze::sans_io_core` refuses the first call that reaches the \
                clock, a file, a thread, a socket or the environment, and the module stays testable under \
                deterministic simulation";
    let fix = Fix {
        message: "mark the module a sans-IO core",
        // alloc: the fix's one part
        parts: vec![(TextRange::empty(at), insert)],
        applicability: Applicability::MaybeIncorrect,
    };
    std_mistakes::report(cx, LINT, file, range, message, note, Some(fix));
}

/// The module of the file walked and its first item that is not a `mod` declaration (a `mod`
/// item's def is the child, not this module).
fn module_of_file<'t>(cx: &Cx<'_, 't>, file: ast::Node<'t>) -> Option<(hir::Module, ast::Node<'t>)> {
    let db = cx.db();
    let first = file.children().find(|it| ast::Item::cast(*it).is_some() && it.kind() != SyntaxKind::MODULE)?;
    let module = match cx.def(first) {
        Some(def) => def.module(db)?,
        None => {
            let imp = ast::Impl::cast(first)?;
            cx.impl_def(imp)?.module(db)
        }
    };
    Some((module, first))
}

/// Whether `module` or an ancestor is marked `#![blaze::sans_io]`.
fn in_core(db: &Db, module: hir::Module) -> bool {
    let mut at = Some(module);
    while let Some(module) = at.take() {
        if AttrFlags::query(db, AttrDefId::ModuleId(module.into())).contains(AttrFlags::SANS_IO) {
            return true;
        }
        at = module.parent(db);
    }
    false
}

/// The verdict of `module` and its children, bounded by [`FUNCTIONS`].
fn walk(db: &Db, module: hir::Module) -> Verdict {
    let mut functions: Vec<FunctionId, std::alloc::Global> = Vec::new_in(std::alloc::Global);
    let mut modules: Vec<hir::Module, std::alloc::Global> = Vec::new_in(std::alloc::Global);
    modules.push(module);
    while let Some(module) = modules.pop() {
        for def in module.declarations(db) {
            match def {
                hir::ModuleDef::Function(it) => push(db, &mut functions, it),
                hir::ModuleDef::Trait(it) => {
                    for item in it.items(db) {
                        if let hir::AssocItem::Function(it) = item {
                            push(db, &mut functions, it);
                        }
                    }
                }
                _ => {}
            }
        }
        for imp in module.impl_defs(db) {
            for item in imp.items(db) {
                if let hir::AssocItem::Function(it) = item {
                    push(db, &mut functions, it);
                }
            }
        }
        modules.extend(module.children(db));
        if functions.len() > FUNCTIONS {
            return Verdict::TooLarge;
        }
    }
    for &f in &functions {
        if reaches(db, f) {
            return Verdict::Reaches;
        }
    }
    Verdict::Clean(u32::try_from(functions.len()).unwrap_or(u32::MAX))
}

/// `func`, when it has a body that is not test code.
fn push(db: &Db, functions: &mut Vec<FunctionId, std::alloc::Global>, func: hir::Function) {
    if !func.has_body(db) {
        return;
    }
    let Ok(id) = FunctionId::try_from(func) else { return };
    if test_code::is_test_def(db, DefWithBodyId::FunctionId(id)) {
        return;
    }
    functions.push(id);
}

/// Whether a call of `f`'s body reaches an ambient effect: `sans_io_core`'s walk.
fn reaches(db: &Db, f: FunctionId) -> bool {
    let def = DefWithBodyId::FunctionId(f);
    let env = ParamEnvAndCrate { param_env: db.trait_environment(def.generic_def(db)), krate: def.krate(db) };
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    for (id, expr) in body.exprs() {
        if !matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. }) {
            continue;
        }
        let Some((callee, args)) = alloc::written_callee(infer, body, id) else { continue };
        let resolved = alloc::resolve(db, env, callee, args);
        if let Resolved::Function(it, _) = resolved
            && !Roots::Ambient.is_root(db, it)
            && core_of(db, it).is_some()
        {
            continue;
        }
        if alloc::instance_reaches_root(db, env, resolved, true, Roots::Ambient) {
            return true;
        }
    }
    false
}

/// Where the file's leading inner attributes and `//!` docs end: the end of the last line of
/// them, or the file's start.
fn after_inner_attributes(file: ast::Node<'_>, text: &str) -> TextSize {
    let mut end = TextSize::from(0);
    for node in file.children() {
        if node.kind() != SyntaxKind::ATTR {
            break;
        }
        if node.text().starts_with("#!") {
            end = end.max(node.text_range().end());
        }
    }
    let mut at = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//!") {
            end = end.max(TextSize::from(u32::try_from(at + line.trim_end_matches('\n').len()).unwrap_or(0)));
        } else if !(trimmed.is_empty() || trimmed.starts_with("#![") || TextSize::from(u32::try_from(at).unwrap_or(0)) < end) {
            break;
        }
        at += line.len();
    }
    end
}
