//! rustc's printing of types and definition paths, in one printer: the paths of definitions
//! (`def_key` parents, impls, trait qualification, closures numbered in their bodies) and of
//! every kind of type, spelled as a diagnostic prints them (`FmtPrinter`: trimmed and visible
//! paths, regions and binders, closures by their span), as `core::any::type_name` gives them,
//! as a v0 symbol demangles, or as debug info names them. [`Spelling`] picks the printer's
//! rules where rustc's printers differ; everything else is one rule in one place.
//!
//! A diagnostic's printer reads its crate's tables of trimmed and visible paths, and its
//! source, through a [`DiagnosticHost`]; the other spellings need only the database, and
//! refuse (naming the rule of rustc's not reproduced) a type they don't know how to spell.

mod consts;
pub mod def;
mod printer;
mod types;

use base_db::Db;
use hir_def::{DefWithBodyId, FunctionId, hir::ExprId};
use hir_ty::next_solver::{GenericArg, Ty};

pub use def::{Def, Ns, Parent, block_owner, closures_around};
pub use printer::{BinderKind, DiagnosticHost, Generics, Mode, Printer, Spelling};
pub use types::{coroutine_desc, desugaring, scalar_name};

/// `core::any::type_name` of `ty`; `None` is a type whose rustc spelling this does not
/// reproduce.
pub fn rustc_type_name<'db>(db: &'db Db, ty: Ty<'db>) -> Option<String> {
    rustc_type_name_or_refusal(db, ty).ok()
}

/// [`rustc_type_name`], or the rule of rustc's `type_name` that is not reproduced for the part
/// of `ty` that stopped it.
pub fn rustc_type_name_or_refusal<'db>(db: &'db Db, ty: Ty<'db>) -> Result<String, &'static str> {
    let mut p = Printer::spelled(db, Spelling::TypeName);
    p.print_type(ty);
    p.finish_spelled()
}

/// What the symbol of `func` with `args` demangles to, which is how a backtrace names it.
/// `None` without the arguments of the impl or trait it is in.
pub fn rustc_function_name<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[GenericArg<'db>],
) -> Option<String> {
    if args.len() < hir_ty::generics::generics(db, func.into()).len_parent() {
        return None;
    }
    let mut p = Printer::spelled(db, Spelling::Symbol);
    p.print_def_path(Def::Function(func), args);
    p.finish_spelled().ok()
}

/// The same of the closure or `async` block written at `expr` of `func`.
pub fn rustc_closure_name<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[GenericArg<'db>],
    expr: ExprId,
) -> Option<String> {
    let around = closures_around(db, func.into(), expr)?;
    let mut p = Printer::spelled(db, Spelling::Symbol);
    p.print_def_path(Def::Function(func), args);
    p.closure_segments(around);
    p.finish_spelled().ok()
}

/// What rustc writes after the path of the function a closure is written in: `::{closure#0}`,
/// the closures it is in first.
pub fn rustc_closure_suffix(db: &Db, owner: DefWithBodyId, expr: ExprId) -> Option<String> {
    let around = closures_around(db, owner, expr)?;
    let mut p = Printer::spelled(db, Spelling::Symbol);
    p.closure_segments(around);
    p.finish_spelled().ok()
}

/// A type as it stands among the generic arguments of a symbol.
pub fn rustc_symbol_type<'db>(db: &'db Db, ty: Ty<'db>) -> Option<String> {
    let mut p = Printer::spelled(db, Spelling::Symbol);
    p.print_type(ty);
    p.finish_spelled().ok()
}

/// What rustc calls a type where it tells a debugger of it: every generic argument that is
/// not a lifetime, a closure `{closure_env#0}` and an `async fn` `{async_fn_env#0}` after
/// what they are written in.
pub fn rustc_debug_name<'db>(db: &'db Db, ty: Ty<'db>) -> Option<String> {
    let mut p = Printer::spelled(db, Spelling::Debug);
    p.print_type(ty);
    p.finish_spelled().ok()
}
