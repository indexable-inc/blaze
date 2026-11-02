//! The names rustc has for types and functions: full paths from the crate root, spelled
//! either as `type_name` gives them or as its symbols demangle.

use std::fmt::Write;

use base_db::Db;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, GenericDefId, HasModule, ItemContainerId,
    Lookup, ModuleDefId, ModuleIdLt,
    expr_store::{Body, BodySourceMap, StoreVisitor},
    hir::{ClosureKind, CoroutineKind, CoroutineSource, Expr, ExprId, PatId},
    lang_item::lang_items,
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, StaticSignature, StructSignature,
        TraitSignature, TypeAliasSignature, UnionSignature,
    },
};
use rustc_abi::ExternAbi;
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{
    BoundExistentialPredicates as _, GenericArgs as _, IntoKind, Safety as _, Term as _,
};

use crate::{
    InferBodyId,
    consteval::try_const_usize,
    db::{HirDatabase, InternedClosure},
    generics::generics,
    next_solver::{
        BoundExistentialPredicates, Const, ConstKind, DbInterner, ExistentialProjection,
        ExistentialTraitRef, GenericArg, GenericArgs, RegionKind, TermId, TraitRef, Ty, TyKind,
    },
    utils::fn_traits,
};

/// `None` is a type whose rustc spelling this does not reproduce.
pub fn rustc_type_name<'db>(db: &'db Db, ty: Ty<'db>) -> Option<String> {
    let mut out = Spelled::new(Spelling::TypeName);
    write_ty(db, &mut out, ty)?;
    Some(out.into_name())
}

/// What the symbol of `func` with `args` demangles to, which is how a backtrace names it.
pub fn rustc_function_name<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[GenericArg<'db>],
) -> Option<String> {
    let mut out = Spelled::new(Spelling::Symbol);
    write_function(db, &mut out, func, args)?;
    Some(out.into_name())
}

/// The same of the closure or `async` block written at `expr` of `func`.
pub fn rustc_closure_name<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[GenericArg<'db>],
    expr: ExprId,
) -> Option<String> {
    let mut out = Spelled::new(Spelling::Symbol);
    write_closure(db, &mut out, func, args, expr, Named::Code)?;
    Some(out.into_name())
}

/// What rustc writes after the path of the function a closure is written in: `::{closure#0}`,
/// the closures it is in first.
pub fn rustc_closure_suffix(
    db: &Db,
    owner: DefWithBodyId,
    expr: ExprId,
) -> Option<String> {
    let around = closures_around(db, owner, expr)?;
    let mut out = Spelled::new(Spelling::Symbol);
    out.closures(&around);
    Some(out.into_name())
}

/// The places rustc numbers a closure by, `n` of each `{closure#n}` from the one outermost in
/// the body to the closure itself: what its `DefPath` adds to the function's.
pub fn rustc_closure_places(
    db: &Db,
    owner: DefWithBodyId,
    expr: ExprId,
) -> Option<Vec<usize>> {
    let around = closures_around(db, owner, expr)?;
    // alloc: the places, the caller's to keep
    (!around.is_empty()).then(|| around.iter().map(|&(place, _)| place).collect())
}

/// The path rustc tells a debugger the code of that closure or `async` block by.
pub fn rustc_debug_closure<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[GenericArg<'db>],
    expr: ExprId,
) -> Option<String> {
    let mut out = Spelled::new(Spelling::Debug);
    write_closure(db, &mut out, func, args, expr, Named::Code)?;
    Some(out.into_name())
}

/// The same of a type, as it stands among the generic arguments of a symbol.
pub fn rustc_symbol_type<'db>(db: &'db Db, ty: Ty<'db>) -> Option<String> {
    let mut out = Spelled::new(Spelling::Symbol);
    write_ty(db, &mut out, ty)?;
    Some(out.into_name())
}

/// What rustc calls a type where it tells a debugger of it: every generic argument that is
/// not a lifetime, a closure `{closure_env#0}` and an `async fn` `{async_fn_env#0}` after
/// what they are written in.
pub fn rustc_debug_name<'db>(db: &'db Db, ty: Ty<'db>) -> Option<String> {
    let mut out = Spelled::new(Spelling::Debug);
    write_ty(db, &mut out, ty)?;
    Some(out.into_name())
}

/// The path rustc tells a debugger a function by: the modules it is written in, then the
/// type an `impl` of its own is of, `{impl#0}` for any other `impl` by its place in the
/// module, or the trait, and last the name with what the function and the `impl` are
/// generic over.
pub fn rustc_debug_function<'db>(
    db: &'db Db,
    func: FunctionId,
    args: &[GenericArg<'db>],
) -> Option<String> {
    let mut out = Spelled::new(Spelling::Debug);
    write_container(db, &mut out, func, args)?;
    write!(out, "::{}", FunctionSignature::of(db, func).name.as_str()).ok()?;
    write_args(db, &mut out, args)?;
    Some(out.into_name())
}

/// How a type with no parts is written, the same in every spelling of a type: `type_name`,
/// symbols, debug info and the names [`crate`]'s users keep across runs. `None` for any
/// other type.
pub fn scalar_name(ty: Ty<'_>) -> Option<&'static str> {
    Some(match ty.kind() {
        TyKind::Bool => "bool",
        TyKind::Char => "char",
        TyKind::Str => "str",
        TyKind::Never => "!",
        TyKind::Int(it) => it.name_str(),
        TyKind::Uint(it) => it.name_str(),
        TyKind::Float(it) => it.name_str(),
        _ => return None,
    })
}

/// What a function is written in, as a debugger is told of it.
fn write_container<'db>(
    db: &'db Db,
    out: &mut Spelled,
    func: FunctionId,
    args: &[GenericArg<'db>],
) -> Option<()> {
    match func.lookup(db).container {
        ItemContainerId::ModuleId(module) => write_module(db, out, module),
        ItemContainerId::ImplId(impl_) => {
            let parent_args = args.get(..generics(db, func.into()).len_parent())?;
            let interner = DbInterner::new_no_crate(db);
            let self_ty = db.impl_self_ty(impl_).instantiate(interner, parent_args).skip_norm_wip();
            if db.impl_trait(impl_).is_none() && matches!(self_ty.kind(), TyKind::Adt(..)) {
                return write_ty(db, out, self_ty);
            }
            let module = impl_.lookup(db).container;
            let scope = module.def_map(db).module(db, module).scope(db);
            let place = scope.impls().position(|it| it == impl_)?;
            write_module(db, out, module)?;
            write!(out, "::{{impl#{place}}}").ok()
        }
        ItemContainerId::TraitId(trait_) => {
            write_module(db, out, trait_.module(db))?;
            write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()
        }
        ItemContainerId::ExternBlockId(_) => None,
    }
}

/// The body a block is written in, and the module that body's owner is declared in: a
/// function, constant or static of the module, or of an `impl` or a trait of it.
pub fn block_owner<'db>(
    db: &'db Db,
    block: hir_def::BlockIdLt<'db>,
) -> Option<(DefWithBodyId, ModuleIdLt<'db>)> {
    let has = |def: DefWithBodyId| Body::of(db, def).blocks(db).any(|(id, _)| id == block);
    let of_assoc = |item: &hir_def::AssocItemId| match *item {
        hir_def::AssocItemId::FunctionId(it) => Some(DefWithBodyId::from(it)),
        hir_def::AssocItemId::ConstId(it) => Some(it.into()),
        hir_def::AssocItemId::TypeAliasId(_) => None,
    };
    let mut around = block.module(db);
    loop {
        let scope = around.def_map(db).module(db, around).scope(db);
        let declared = scope.declarations().filter_map(|def| match def {
            ModuleDefId::FunctionId(it) => Some(DefWithBodyId::from(it)),
            ModuleDefId::ConstId(it) => Some(it.into()),
            ModuleDefId::StaticId(it) => Some(it.into()),
            _ => None,
        });
        let unnamed = scope.unnamed_consts().map(DefWithBodyId::from);
        let in_impls = scope
            .impls()
            .flat_map(|it| it.impl_items(db).items.iter().filter_map(|(_, item)| of_assoc(item)));
        let in_traits = scope.declarations().filter_map(|def| match def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        });
        let in_traits = in_traits
            .flat_map(|it| it.trait_items(db).items.iter().filter_map(|(_, item)| of_assoc(item)));
        let owner = declared.chain(unnamed).chain(in_impls).chain(in_traits).find(|&it| has(it));
        match owner {
            Some(owner) => return Some((owner, around)),
            // A block in a block, of the same body.
            None => around = around.block(db)?.module(db),
        }
    }
}

/// Whether `block` of `owner` is written in no closure or `async` block of it.
pub fn is_outside_closures<'db>(
    db: &'db Db,
    owner: DefWithBodyId,
    block: hir_def::BlockIdLt<'db>,
) -> bool {
    let body = Body::of(db, owner);
    let at = body
        .exprs()
        .find(|(_, expr)| matches!(expr, Expr::Block { id: Some(id), .. } if *id == block));
    at.and_then(|(at, _)| closures_around(db, owner, at)).is_some_and(|it| it.is_empty())
}

/// Which of rustc's ways to write a name is wanted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Spelling {
    /// `core::any::type_name`: no lifetimes but `'_`, every closure `{{closure}}`.
    TypeName,
    /// A demangled symbol: no lifetimes, closures numbered.
    Symbol,
    /// The name of a type in debug info: no lifetimes, defaults written out, closures
    /// numbered and called after what they are.
    Debug,
}

/// A name being written, into one of the thread's scratch strings.
struct Spelled {
    text: blaze_mem::scratch::Lent<String>,
    spelling: Spelling,
}

impl Spelled {
    fn new(spelling: Spelling) -> Self {
        Self { text: blaze_mem::scratch::lend_string(), spelling }
    }

    /// The name written, allocated once at its length.
    fn into_name(self) -> String {
        // alloc: the name is the result
        String::from(self.text.as_str())
    }

    fn push_str(&mut self, text: &str) {
        self.text.push_str(text);
    }

    fn push(&mut self, text: char) {
        self.text.push(text);
    }

    /// The closures something is written in, outermost first, by their places among the
    /// closures of what they are written in.
    fn closures(&mut self, places: &[(usize, ClosureKind)]) {
        for (place, kind) in places {
            match self.spelling {
                Spelling::TypeName => self.text.push_str("::{{closure}}"),
                Spelling::Symbol => {
                    let _ = write!(self.text, "::{{closure#{place}}}");
                }
                Spelling::Debug => {
                    let _ = write!(self.text, "::{{{}#{place}}}", kind_label(*kind));
                }
            }
        }
    }

    /// The same where the last of them is what is named, which a debugger is told of as
    /// what the closure keeps: its environment.
    fn closure_env(&mut self, places: &[(usize, ClosureKind)]) -> Option<()> {
        let ((place, kind), around) = places.split_last()?;
        self.closures(around);
        write!(self.text, "::{{{}_env#{place}}}", kind_label(*kind)).ok()
    }
}

/// What rustc calls a closure of a kind in debug info.
fn kind_label(kind: ClosureKind) -> &'static str {
    use CoroutineKind::{Async, AsyncGen, Gen};
    use CoroutineSource::{Block, Closure, Fn};
    match kind {
        ClosureKind::Closure | ClosureKind::CoroutineClosure(_) => "closure",
        ClosureKind::OldCoroutine(_) => "coroutine",
        ClosureKind::Coroutine { kind: Async, source: Block } => "async_block",
        ClosureKind::Coroutine { kind: Async, source: Closure } => "async_closure",
        ClosureKind::Coroutine { kind: Async, source: Fn } => "async_fn",
        ClosureKind::Coroutine { kind: Gen, source: Block } => "gen_block",
        ClosureKind::Coroutine { kind: Gen, source: Closure } => "gen_closure",
        ClosureKind::Coroutine { kind: Gen, source: Fn } => "gen_fn",
        ClosureKind::Coroutine { kind: AsyncGen, source: Block } => "async_gen_block",
        ClosureKind::Coroutine { kind: AsyncGen, source: Closure } => "async_gen_closure",
        ClosureKind::Coroutine { kind: AsyncGen, source: Fn } => "async_gen_fn",
    }
}

impl Write for Spelled {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.text.push_str(text);
        Ok(())
    }
}

fn write_module<'db>(
    db: &'db Db,
    out: &mut Spelled,
    module: ModuleIdLt<'db>,
) -> Option<()> {
    // rustc names an item in a body after the function the body is of, and the closures of
    // it the item is written in.
    if module.is_block_module(db) {
        let block = module.block(db)?;
        let (owner, around) = block_owner(db, block)?;
        let body = Body::of(db, owner);
        let (at, _) = body
            .exprs()
            .find(|(_, expr)| matches!(expr, Expr::Block { id: Some(id), .. } if *id == block))?;
        match owner {
            DefWithBodyId::FunctionId(func) => match func.lookup(db).container {
                ItemContainerId::ModuleId(_) => {
                    write_module(db, out, around)?;
                    write!(out, "::{}", FunctionSignature::of(db, func).name.as_str()).ok()?;
                }
                _ if out.spelling == Spelling::Debug => {
                    let args =
                        GenericArgs::identity_for_item(DbInterner::new_no_crate(db), func.into());
                    write_container(db, out, func, args.as_slice())?;
                    write!(out, "::{}", FunctionSignature::of(db, func).name.as_str()).ok()?;
                }
                _ => {
                    let args =
                        GenericArgs::identity_for_item(DbInterner::new_no_crate(db), func.into());
                    write_function(db, out, func, args.as_slice())?;
                }
            },
            DefWithBodyId::ConstId(it) => {
                let ItemContainerId::ModuleId(_) = it.lookup(db).container else { return None };
                let name = ConstSignature::of(db, it).name.as_ref();
                write_module(db, out, around)?;
                write!(out, "::{}", name.map_or("_", |it| it.as_str())).ok()?;
            }
            DefWithBodyId::StaticId(it) => {
                write_module(db, out, around)?;
                write!(out, "::{}", StaticSignature::of(db, it).name.as_str()).ok()?;
            }
            _ => return None,
        }
        out.closures(&closures_around(db, owner, at)?);
        return Some(());
    }
    match module.containing_module(db) {
        Some(parent) => {
            write_module(db, out, parent)?;
            write!(out, "::{}", module.name(db)?.as_str()).ok()
        }
        None => {
            let krate = module.krate(db);
            let name = krate.extra_data(db).display_name.as_ref()?;
            out.push_str(name.crate_name().as_str());
            Some(())
        }
    }
}

fn write_list<'db>(
    db: &'db Db,
    out: &mut Spelled,
    tys: impl IntoIterator<Item = Ty<'db>>,
) -> Option<()> {
    for (i, ty) in tys.into_iter().enumerate() {
        if i != 0 {
            out.push_str(", ");
        }
        write_ty(db, out, ty)?;
    }
    Some(())
}

fn write_ty<'db>(db: &'db Db, out: &mut Spelled, ty: Ty<'db>) -> Option<()> {
    if let Some(name) = scalar_name(ty) {
        out.push_str(name);
        return Some(());
    }
    match ty.kind() {
        TyKind::Ref(region, pointee, mutability) => {
            out.push('&');
            // To `type_name` a lifetime of a `for<'a>` is `'_`, and any other is not there.
            if out.spelling == Spelling::TypeName
                && matches!(region.kind(), RegionKind::ReBound(..))
            {
                out.push_str("'_ ");
            }
            if mutability == Mutability::Mut {
                out.push_str("mut ");
            }
            write_ty(db, out, pointee)?;
        }
        TyKind::RawPtr(pointee, mutability) => {
            out.push_str(match mutability {
                Mutability::Not => "*const ",
                Mutability::Mut => "*mut ",
            });
            write_ty(db, out, pointee)?;
        }
        TyKind::Slice(element) => {
            out.push('[');
            write_ty(db, out, element)?;
            out.push(']');
        }
        TyKind::Array(element, len) => {
            out.push('[');
            write_ty(db, out, element)?;
            write!(out, "; {}]", try_const_usize(db, len)?).ok()?;
        }
        TyKind::Tuple(tys) => {
            out.push('(');
            write_list(db, out, tys.iter())?;
            out.push_str(if tys.len() == 1 { ",)" } else { ")" });
        }
        TyKind::Adt(adt, args) => {
            let adt = adt.def_id();
            let name = match adt {
                AdtId::StructId(it) => &StructSignature::of(db, it).name,
                AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
                AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
            };
            write_module(db, out, adt.module(db))?;
            write!(out, "::{}", name.as_str()).ok()?;
            let args = match out.spelling {
                Spelling::Debug => args.as_slice(),
                _ => without_defaults(db, adt.into(), args.as_slice()),
            };
            write_args(db, out, args)?;
        }
        TyKind::Dynamic(bounds, _) if out.spelling == Spelling::Debug => {
            write_dyn_debug(db, out, bounds)?;
        }
        TyKind::Dynamic(bounds, _) => write_dyn(db, out, bounds)?,
        TyKind::FnDef(def, args) => {
            let args = args.skip_binder();
            match def.0 {
                CallableDefId::FunctionId(func) => write_function(db, out, func, args.as_slice())?,
                _ if out.spelling == Spelling::Symbol => return None,
                CallableDefId::StructId(it) => {
                    let ty = Ty::new_adt(DbInterner::new_no_crate(db), it.into(), args);
                    write_ty(db, out, ty)?;
                    out.push_str("::{{constructor}}");
                }
                CallableDefId::EnumVariantId(it) => {
                    let parent = it.lookup(db).parent;
                    let ty = Ty::new_adt(DbInterner::new_no_crate(db), parent.into(), args);
                    write_ty(db, out, ty)?;
                    let name = &parent.enum_variants(db).variant_name_by_id(it)?;
                    write!(out, "::{}::{{{{constructor}}}}", name.as_str()).ok()?;
                }
            }
        }
        TyKind::Closure(id, args) => {
            let func = written_in(id.0.loc(db))?;
            write_closure(db, out, func.0, args.as_closure().parent_args(), func.1, Named::Kept)?;
        }
        TyKind::Coroutine(id, args) => {
            let func = written_in(id.0.loc(db))?;
            // The future of an `async` closure is named after how the closure was called.
            let is_block = matches!(
                Body::of(db, func.0.into()).expr(func.1),
                Expr::Closure {
                    closure_kind: ClosureKind::Coroutine { source: CoroutineSource::Block, .. },
                    ..
                }
            );
            if out.spelling == Spelling::TypeName && !is_block {
                return None;
            }
            write_closure(db, out, func.0, args.as_coroutine().parent_args(), func.1, Named::Kept)?;
        }
        TyKind::CoroutineClosure(id, args) if out.spelling != Spelling::TypeName => {
            let func = written_in(id.0.loc(db))?;
            write_closure(
                db,
                out,
                func.0,
                args.as_coroutine_closure().parent_args(),
                func.1,
                Named::Kept,
            )?;
        }
        TyKind::FnPtr(sig, header) => {
            let sig = sig.skip_binder();
            if !header.safety().is_safe() {
                out.push_str("unsafe ");
            }
            if header.abi() != ExternAbi::Rust {
                write!(out, "extern \"{}\" ", header.abi().as_str()).ok()?;
            }
            out.push_str("fn(");
            write_list(db, out, sig.inputs().iter().copied())?;
            if header.c_variadic() {
                out.push_str(", ...");
            }
            out.push(')');
            if !sig.output().is_unit() {
                out.push_str(" -> ");
                write_ty(db, out, sig.output())?;
            }
        }
        _ => return None,
    }
    Some(())
}

/// A trait object: `dyn path::Trait<A, Assoc = B> + path::Auto`, a trait of the `Fn` family as
/// `dyn path::Fn(A) -> B`, and the auto traits in the order of their paths.
fn write_dyn<'db>(
    db: &'db Db,
    out: &mut Spelled,
    bounds: BoundExistentialPredicates<'db>,
) -> Option<()> {
    // alloc: a trait object's parts, sorted as rustc sorts them; only on `dyn` types
    let mut parts = Vec::new();
    if let Some(principal) = bounds.principal() {
        let mut part = Spelled::new(out.spelling);
        let projections: Vec<_> =
            // alloc: a trait object's projections; only on `dyn` types
            bounds.projection_bounds().into_iter().map(|it| it.skip_binder()).collect();
        write_principal(db, &mut part, principal.skip_binder(), &projections)?;
        parts.push(part.into_name());
    }
    // alloc: a trait object's parts, sorted as rustc sorts them; only on `dyn` types
    let mut autos = Vec::new();
    for auto in bounds.auto_traits() {
        let mut part = Spelled::new(out.spelling);
        write_module(db, &mut part, auto.0.module(db))?;
        write!(part, "::{}", TraitSignature::of(db, auto.0).name.as_str()).ok()?;
        autos.push(part.into_name());
    }
    autos.sort();
    parts.extend(autos);
    out.push_str("dyn ");
    out.push_str(&parts.join(" + "));
    Some(())
}

/// The trait a trait object is of, with what it says of the trait's associated types.
fn write_principal<'db>(
    db: &'db Db,
    out: &mut Spelled,
    principal: ExistentialTraitRef<'db>,
    projections: &[ExistentialProjection<'db>],
) -> Option<()> {
    let trait_ = principal.def_id.0;
    write_module(db, out, trait_.module(db))?;
    write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
    if out.spelling == Spelling::Symbol {
        if !projections.is_empty() {
            return None;
        }
        return write_args(db, out, principal.args.as_slice());
    }
    let is_fn = fn_traits(lang_items(db, trait_.krate(db))).any(|it| it == trait_);
    match (is_fn, principal.args.as_slice(), projections) {
        (true, [inputs], [output]) => {
            let TyKind::Tuple(inputs) = inputs.ty()?.kind() else { return None };
            out.push('(');
            write_list(db, out, inputs.iter())?;
            out.push(')');
            let output = output.term.as_type()?;
            if !output.is_unit() {
                out.push_str(" -> ");
                write_ty(db, out, output)?;
            }
        }
        (false, args, []) => write_args(db, out, args)?,
        (false, args, [projection]) => {
            let mut inner = Spelled::new(out.spelling);
            write_args(db, &mut inner, args)?;
            let before =
                // alloc: the text `type_name` returns is the result
                inner.text.strip_suffix('>').map_or("<".to_owned(), |it| format!("{it}, "));
            let TermId::TypeAliasId(alias) = projection.def_id.0 else { return None };
            write!(out, "{before}{} = ", TypeAliasSignature::of(db, alias).name.as_str()).ok()?;
            write_ty(db, out, projection.term.as_type()?)?;
            out.push('>');
        }
        // FIXME: rustc writes several associated types in an order that is not known here.
        _ => return None,
    }
    Some(())
}

/// The function a closure is written in and the expression that is the closure.
fn written_in(closure: InternedClosure<'_>) -> Option<(FunctionId, ExprId)> {
    match closure.owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)) => Some((func, closure.expr)),
        // FIXME: one of a constant or a static is named after that.
        _ => None,
    }
}

/// `<A, B>`, or nothing for no arguments. `type_name` writes every lifetime as `'_`, and a
/// symbol has none.
fn write_args<'db>(
    db: &'db Db,
    out: &mut Spelled,
    args: &[GenericArg<'db>],
) -> Option<()> {
    let mut written = 0;
    for arg in args {
        if out.spelling != Spelling::TypeName && arg.region().is_some() {
            continue;
        }
        out.push_str(if written == 0 { "<" } else { ", " });
        written += 1;
        match (arg.ty(), arg.konst()) {
            (Some(ty), _) => write_ty(db, out, ty)?,
            (_, Some(konst)) => match type_name_value(konst) {
                Some(value) if out.spelling == Spelling::TypeName => out.push_str(&value),
                _ => write!(out, "{}", try_const_usize(db, konst)?).ok()?,
            },
            _ => out.push_str("'_"),
        }
    }
    if written != 0 {
        out.push('>');
    }
    Some(())
}

/// A constant argument that is a value as `type_name` writes it: `true`, `'a'`, `-1`, or the
/// name of the least or the greatest value of its type, as `i8::MIN`.
fn type_name_value(konst: Const<'_>) -> Option<String> {
    let ConstKind::Value(value) = konst.kind() else { return None };
    let rustc_type_ir::ValTreeKind::Leaf(leaf) = value.value.inner() else { return None };
    let leaf = *leaf;
    let unused = 128 - leaf.size().bits();
    // alloc: the text `type_name` returns is the result
    let named = |name: &str, end: &str| Some(format!("{name}::{end}"));
    match value.ty.kind() {
        // alloc: the text `type_name` returns is the result
        TyKind::Bool => Some(leaf.try_to_bool().ok()?.to_string()),
        // alloc: the text `type_name` returns is the result
        TyKind::Char => Some(format!("{:?}", char::from_u32(leaf.to_uint_unchecked() as u32)?)),
        TyKind::Int(int) => {
            let name = match int {
                rustc_type_ir::IntTy::Isize => "isize",
                rustc_type_ir::IntTy::I8 => "i8",
                rustc_type_ir::IntTy::I16 => "i16",
                rustc_type_ir::IntTy::I32 => "i32",
                rustc_type_ir::IntTy::I64 => "i64",
                rustc_type_ir::IntTy::I128 => "i128",
            };
            match leaf.to_int_unchecked() {
                it if it == i128::MIN >> unused => named(name, "MIN"),
                it if it == i128::MAX >> unused => named(name, "MAX"),
                // alloc: the text `type_name` returns is the result
                it => Some(it.to_string()),
            }
        }
        TyKind::Uint(uint) => {
            let name = match uint {
                rustc_type_ir::UintTy::Usize => "usize",
                rustc_type_ir::UintTy::U8 => "u8",
                rustc_type_ir::UintTy::U16 => "u16",
                rustc_type_ir::UintTy::U32 => "u32",
                rustc_type_ir::UintTy::U64 => "u64",
                rustc_type_ir::UintTy::U128 => "u128",
            };
            match leaf.to_uint_unchecked() {
                it if it == u128::MAX >> unused => named(name, "MAX"),
                // alloc: the text `type_name` returns is the result
                it => Some(it.to_string()),
            }
        }
        _ => None,
    }
}

/// The closure or `async` block written at `expr` of `func`.
fn write_closure<'db>(
    db: &'db Db,
    out: &mut Spelled,
    func: FunctionId,
    args: &[GenericArg<'db>],
    expr: ExprId,
    named: Named,
) -> Option<()> {
    let around = closures_around(db, func.into(), expr)?;
    if out.spelling != Spelling::Debug {
        write_function(db, out, func, args)?;
        out.closures(&around);
        return Some(());
    }
    // A debugger is told of what the function is written in by its place there, then of the
    // closure, and last of what the function is generic over.
    match func.lookup(db).container {
        ItemContainerId::ModuleId(module) => write_module(db, out, module)?,
        ItemContainerId::ImplId(impl_) => {
            let module = impl_.lookup(db).container;
            let scope = module.def_map(db).module(db, module).scope(db);
            let place = scope.impls().position(|it| it == impl_)?;
            write_module(db, out, module)?;
            write!(out, "::{{impl#{place}}}").ok()?;
        }
        ItemContainerId::TraitId(trait_) => {
            write_module(db, out, trait_.module(db))?;
            write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
        }
        ItemContainerId::ExternBlockId(_) => return None,
    }
    write!(out, "::{}", FunctionSignature::of(db, func).name.as_str()).ok()?;
    match named {
        Named::Code => out.closures(&around),
        Named::Kept => out.closure_env(&around)?,
    }
    write_args(db, out, args)
}

/// Which of the two things a closure is to a debugger is being named.
#[derive(Clone, Copy)]
enum Named {
    /// The function its body is.
    Code,
    /// The type of what it keeps.
    Kept,
}

/// `dyn Trait<A, Item=B> + Auto` as a debugger is told of it, in brackets after `dyn` when
/// there is more than the trait.
fn write_dyn_debug<'db>(
    db: &'db Db,
    out: &mut Spelled,
    bounds: crate::next_solver::BoundExistentialPredicates<'db>,
) -> Option<()> {
    // alloc: a trait object's parts, sorted as rustc sorts them; only on `dyn` types
    let mut autos = Vec::new();
    for auto in bounds.auto_traits() {
        let mut name = Spelled::new(Spelling::Debug);
        write_module(db, &mut name, auto.0.module(db))?;
        write!(name, "::{}", TraitSignature::of(db, auto.0).name.as_str()).ok()?;
        autos.push(name.into_name());
    }
    autos.sort_unstable();
    let principal = bounds.principal();
    let bracketed = principal.is_some() && !autos.is_empty();
    out.push_str(if bracketed { "dyn (" } else { "dyn " });
    if let Some(principal) = principal {
        let principal = principal.skip_binder();
        let trait_ = principal.def_id.0;
        write_module(db, out, trait_.module(db))?;
        write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
        let mut written = Spelled::new(Spelling::Debug);
        write_args(db, &mut written, principal.args.as_slice())?;
        let mut inner =
            written.text.strip_prefix('<').and_then(|it| it.strip_suffix('>')).map(str::to_owned);
        for bound in bounds.projection_bounds() {
            let bound = bound.skip_binder();
            let crate::next_solver::TermId::TypeAliasId(alias) = bound.def_id.0 else {
                return None;
            };
            let mut part = Spelled::new(Spelling::Debug);
            write!(part, "{}=", TypeAliasSignature::of(db, alias).name.as_str()).ok()?;
            write_ty(db, &mut part, bound.term.as_type()?)?;
            inner = Some(match inner {
                // alloc: the text `type_name` returns is the result
                Some(inner) => format!("{inner}, {}", part.text.as_str()),
                None => part.into_name(),
            });
        }
        if let Some(inner) = inner {
            write!(out, "<{inner}>").ok()?;
        }
        if !autos.is_empty() {
            out.push_str(" + ");
        }
    }
    out.push_str(&autos.join(" + "));
    if bracketed {
        out.push(')');
    }
    Some(())
}

/// The module of the item rustc takes a type to be about, when it has one.
fn characteristic_module<'db>(db: &'db Db, ty: Ty<'db>) -> Option<ModuleIdLt<'db>> {
    match ty.kind() {
        TyKind::Adt(adt, _) => Some(adt.def_id().module(db)),
        TyKind::Dynamic(bounds, _) => Some(bounds.principal()?.skip_binder().def_id.0.module(db)),
        TyKind::Ref(_, inner, _)
        | TyKind::RawPtr(inner, _)
        | TyKind::Slice(inner)
        | TyKind::Array(inner, _) => characteristic_module(db, inner),
        TyKind::Tuple(tys) => tys.iter().find_map(|ty| characteristic_module(db, ty)),
        // FIXME: a function or a closure is about itself, in what it is written in.
        _ => None,
    }
}

fn write_trait<'db>(
    db: &'db Db,
    out: &mut Spelled,
    trait_ref: TraitRef<'db>,
) -> Option<()> {
    let trait_ = trait_ref.def_id.0;
    write_module(db, out, trait_.module(db))?;
    write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
    write_args(db, out, without_defaults(db, trait_.into(), trait_ref.args.as_slice()).get(1..)?)
}

/// The arguments of `def` that rustc writes: not the last ones that only repeat a default.
fn without_defaults<'a, 'db>(
    db: &'db Db,
    def: GenericDefId,
    mut args: &'a [GenericArg<'db>],
) -> &'a [GenericArg<'db>] {
    let interner = DbInterner::new_no_crate(db);
    let defaults = db.generic_defaults(def);
    while let Some((last, rest)) = args.split_last()
        && let Some(default) = defaults.get(rest.len())
        && default.instantiate(interner, rest).skip_norm_wip() == *last
    {
        args = rest;
    }
    args
}

/// A function with its generic arguments. To `type_name` that is `path::f<A>`, and for one
/// of an `impl` `path::Type<T>::f`, `<T as Trait>::f`, or `path::<impl Trait for T>::f` when
/// the `impl` is in the module of neither the type nor the trait. A symbol has `path::f::<A>`,
/// `<path::Type<T>>::f` and `<T as Trait>::f`.
fn write_function<'db>(
    db: &'db Db,
    out: &mut Spelled,
    func: FunctionId,
    args: &[GenericArg<'db>],
) -> Option<()> {
    let (parent_args, own_args) = args.split_at_checked(generics(db, func.into()).len_parent())?;
    // A lifetime of a function is mostly late bound, and then no argument to rustc.
    if out.spelling == Spelling::TypeName && own_args.iter().any(|arg| arg.region().is_some()) {
        return None;
    }
    match func.lookup(db).container {
        ItemContainerId::ModuleId(module) => write_module(db, out, module)?,
        ItemContainerId::ImplId(impl_) if out.spelling == Spelling::Symbol => {
            let interner = DbInterner::new_no_crate(db);
            let self_ty = db.impl_self_ty(impl_).instantiate(interner, parent_args).skip_norm_wip();
            out.push('<');
            write_ty(db, out, self_ty)?;
            if let Some(trait_ref) = db.impl_trait(impl_) {
                out.push_str(" as ");
                write_trait(db, out, trait_ref.instantiate(interner, parent_args).skip_norm_wip())?;
            }
            out.push('>');
        }
        ItemContainerId::TraitId(trait_) if out.spelling == Spelling::Symbol => {
            let self_ty = parent_args.first()?;
            out.push('<');
            write_ty(db, out, self_ty.ty()?)?;
            out.push_str(" as ");
            write_module(db, out, trait_.module(db))?;
            write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
            write_args(db, out, without_defaults(db, trait_.into(), parent_args).get(1..)?)?;
            out.push('>');
        }
        ItemContainerId::ImplId(impl_) => {
            let interner = DbInterner::new_no_crate(db);
            let self_ty = db.impl_self_ty(impl_).instantiate(interner, parent_args).skip_norm_wip();
            let trait_ref = db
                .impl_trait(impl_)
                .map(|it| it.instantiate(interner, parent_args).skip_norm_wip());
            let module = impl_.lookup(db).container;
            let in_self_module = characteristic_module(db, self_ty) == Some(module);
            let in_trait_module = trait_ref.is_some_and(|it| it.def_id.0.module(db) == module);
            if !in_self_module && !in_trait_module {
                write_module(db, out, module)?;
                out.push_str("::<impl ");
                if let Some(trait_ref) = trait_ref {
                    write_trait(db, out, trait_ref)?;
                    out.push_str(" for ");
                }
                write_ty(db, out, self_ty)?;
                out.push('>');
            } else if let Some(trait_ref) = trait_ref {
                out.push('<');
                write_ty(db, out, self_ty)?;
                out.push_str(" as ");
                write_trait(db, out, trait_ref)?;
                out.push('>');
            } else if matches!(
                self_ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::Str
                    | TyKind::Int(_)
                    | TyKind::Uint(_)
                    | TyKind::Float(_)
            ) {
                write_ty(db, out, self_ty)?;
            } else {
                out.push('<');
                write_ty(db, out, self_ty)?;
                out.push('>');
            }
        }
        // FIXME: a provided method is `<T as Trait>::f`.
        ItemContainerId::TraitId(_) | ItemContainerId::ExternBlockId(_) => return None,
    }
    write!(out, "::{}", FunctionSignature::of(db, func).name.as_str()).ok()?;
    if out.spelling == Spelling::Symbol && own_args.iter().any(|arg| arg.region().is_none()) {
        out.push_str("::");
    }
    write_args(db, out, own_args)
}

/// The closures and `async` blocks `target` is written in, outermost first and with `target`
/// last when it is one: each by its place among those written in the one before it.
fn closures_around(
    db: &Db,
    owner: DefWithBodyId,
    target: ExprId,
) -> Option<&[(usize, ClosureKind)]> {
    closures_around_query(db, owner, target).as_deref()
}

/// [`closures_around`], kept: it may read the source map, which changes with every edit above
/// the body, and what it finds does not. The map orders closures by the macro calls they are
/// in, so a body that called no macro is ordered without it, as every closure of it is in
/// the same file.
#[blaze_query::tracked(returns(ref))]
fn closures_around_query(
    db: &Db,
    owner: DefWithBodyId,
    target: ExprId,
) -> Option<Vec<(usize, ClosureKind)>> {
    let body = Body::of(db, owner);
    // alloc: once per closure, in the query that keeps its result
    let mut around = Vec::new();
    if !find_closures(body, body.root_expr(), target, &mut around) {
        return None;
    }
    let expanded = !body.macro_calls().is_empty();
    let order =
        |closure: ExprId| MacroCalls::around(db, Body::unmoved_source_map(db, owner), closure);
    let mut inside = None;
    let places = around.iter().map(|&closure| {
        // alloc: once per closure, in the query that keeps its result
        let mut beside = Vec::new();
        match inside.replace(closure) {
            Some(it) => {
                walk_outside_consts(body, it, |child| closures_in(body, child, &mut beside))
            }
            None => closures_in(body, body.root_expr(), &mut beside),
        }
        // Stable, so those of one expansion stay in the order they are written in.
        if expanded && beside.len() > 1 {
            beside.sort_by_cached_key(|&it| order(it));
        }
        let Expr::Closure { closure_kind, .. } = body.expr(closure) else { return None };
        Some((beside.iter().position(|&it| it == closure)?, closure_kind))
    });
    // alloc: the result the query keeps
    places.collect()
}

/// The closures `target` is written in, looking for it from `at` down.
fn find_closures(body: &Body, at: ExprId, target: ExprId, around: &mut Vec<ExprId>) -> bool {
    let is_closure = matches!(body.expr(at), Expr::Closure { .. });
    if is_closure {
        around.push(at);
    }
    let mut found = at == target;
    if !found {
        walk_outside_consts(body, at, |child| {
            found = found || find_closures(body, child, target, around);
        });
    }
    if is_closure && !found {
        around.pop();
    }
    found
}

/// The closures from `at` down that are written in no other closure on the way.
fn closures_in(body: &Body, at: ExprId, found: &mut Vec<ExprId>) {
    match body.expr(at) {
        Expr::Closure { .. } => found.push(at),
        _ => walk_outside_consts(body, at, |child| closures_in(body, child, found)),
    }
}

/// The expressions right in `at`, in the patterns too, but not those of an anonymous constant
/// (`const { .. }`, the length of `[x; n]`, a const argument): rustc makes each of those a
/// definition of its own and numbers the closures in it under that, not under the body. So a
/// closure in one is not found (its place is refused) and does not count among the others.
fn walk_outside_consts(body: &Body, at: ExprId, callback: impl FnMut(ExprId)) {
    return body.visit_expr_children(at, Visitor { callback, body });

    struct Visitor<'a, F> {
        callback: F,
        body: &'a Body,
    }

    impl<F: FnMut(ExprId)> StoreVisitor for Visitor<'_, F> {
        fn on_expr(&mut self, expr: ExprId) {
            (self.callback)(expr);
        }

        fn on_anon_const_expr(&mut self, _: ExprId) {}

        fn on_pat(&mut self, pat: PatId) {
            let body = self.body;
            body.visit_pat_children(pat, self);
        }
    }
}

/// The macro calls something is in the expansion of, outermost first: each by where in the
/// text it is expanded from the call starts.
///
/// rustc numbers what it finds in a body before it expands the macros of it, and then what
/// each expansion holds as it gets to it, the macros of an expansion first. That is the
/// order these sort in.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct MacroCalls(Vec<u32>);

impl MacroCalls {
    fn around(db: &Db, source_map: &BodySourceMap, expr: ExprId) -> Self {
        // alloc: a sort key, only in a body that called a macro, once per query run
        let mut calls = Vec::new();
        let mut file = source_map.expr_syntax(expr).ok().map(|it| it.file_id);
        while let Some(call) = file.and_then(|it| it.macro_file()) {
            let node = call.call_node_view(db);
            calls.push(node.value.text_range().start().into());
            file = Some(node.file_id);
        }
        calls.reverse();
        MacroCalls(calls)
    }
}
