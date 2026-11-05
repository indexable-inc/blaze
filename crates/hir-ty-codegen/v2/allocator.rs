//! The allocator shim rustc puts in a program: the entries of the global allocator that
//! `alloc` calls by name (`__rust_alloc` and the rest), each going to the
//! `#[global_allocator]`'s (the functions its expansion exports) or else to std's defaults,
//! and `__rust_alloc_error_handler`, going to the `#[alloc_error_handler]`'s `__rg_oom` or else
//! to alloc's `__rdl_alloc_error_handler`.
//! Each entry is an instance of its own ([`Kind::Allocator`]), defined under its symbol.

use base_db::{Crate, Db};
use blaze_codegen::backend::FrontRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use cranelift_codegen::ir::{AbiParam, InstBuilder as _, Signature};
use hir_def::HasModule as _;
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase as _,
    next_solver::{DbInterner, ParamEnv},
};
use smallvec::SmallVec;

use super::{HirInstance, Kind, places::Ref};
use crate::{
    ALLOCATOR_ENTRIES, Produced, compile_as, declares_global_allocator, fragment::FuncTarget,
    global_allocator_export, linked_crates,
};

/// One function of the shim: `symbol`, of `params` pointer-sized arguments and a pointer
/// returned if `returns`, calling `to` with them, or doing nothing. `krate` is the program's
/// crate it is built in the environment of, which its code does not depend on.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Forward {
    pub(crate) symbol: &'static str,
    pub(crate) to: Option<&'static str>,
    pub(crate) params: u8,
    pub(crate) returns: bool,
    pub(crate) krate: Crate,
}

const ITEM_DOMAIN: &str = "blaze v2 allocator item 1";
const CODE_DOMAIN: &str = "blaze v2 allocator 1";

/// The shim of the program of `krate`, and the `#[global_allocator]`'s functions it calls,
/// which nothing else names: none for a program that does not link `alloc`.
pub(super) fn shim(
    db: &Db,
    krate: Crate,
    mut root: impl FnMut(hir_def::FunctionId) -> HirInstance,
) -> Result<Vec<HirInstance>, String> {
    let Some(boxed) = DbInterner::new_with(db, krate).lang_items().OwnedBox else {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    };
    let program = linked_crates(db, krate)?;
    if !program.contains(&boxed.krate(db)) {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    }
    let mut declared = program.iter().copied().filter(|&it| declares_global_allocator(db, it));
    let allocator = declared.next();
    if let (Some(first), Some(other)) = (allocator, declared.next()) {
        return Err(two_allocators(db, first, other));
    }
    let mut shim = Vec::with_capacity(2 * ALLOCATOR_ENTRIES.len() + 3);
    for (symbol, ours, params, returns) in ALLOCATOR_ENTRIES {
        let to = match allocator {
            Some(allocator) => {
                let Some(func) = global_allocator_export(db, allocator, ours) else {
                    let name = allocator.named(db);
                    // alloc: the error's message
                    return Err(format!(
                        "`#[global_allocator]` of crate `{name}` exports no `{ours}`"
                    ));
                };
                shim.push(root(func));
                ours
            }
            None => default_of(symbol)?,
        };
        let params = params as u8;
        shim.push(forward(Forward { symbol, to: Some(to), params, returns, krate }));
    }
    // `__rust_alloc_error_handler` goes to the program's `#[alloc_error_handler]` (std has one)
    // or else to alloc's default, as rustc's shim goes to `__rg_oom` or `__rdl_alloc_error_handler`.
    let handler = "__rust_alloc_error_handler";
    let mut handlers =
        program.iter().filter_map(|&it| Some((it, alloc_error_handler_export(db, it)?)));
    let to = match (handlers.next(), handlers.next()) {
        (Some((_, func)), None) => {
            shim.push(root(func));
            hir_expand::builtin::ALLOC_ERROR_HANDLER_EXPORT
        }
        (None, _) => "__rdl_alloc_error_handler",
        (Some((first, _)), Some((other, _))) => {
            return Err(two_alloc_error_handlers(db, first, other));
        }
    };
    shim.push(forward(Forward { symbol: handler, to: Some(to), params: 2, returns: false, krate }));
    let marker = "__rust_no_alloc_shim_is_unstable_v2";
    shim.push(forward(Forward { symbol: marker, to: None, params: 0, returns: false, krate }));
    Ok(shim)
}

fn forward(forward: Forward) -> HirInstance {
    HirInstance(Kind::Allocator(forward))
}

/// The error of a program with two `#[global_allocator]`s, `a`'s and `b`'s, which names their
/// crates in order of name: the same message whichever order the crate graph lists them in.
fn two_allocators(db: &Db, a: Crate, b: Crate) -> String {
    let mut names = [a.named(db).to_string(), b.named(db).to_string()]; // alloc: the error's names
    names.sort_unstable();
    let [first, second] = names;
    // alloc: the error's message
    format!("two `#[global_allocator]`s in one program, in crates `{first}` and `{second}`")
}

/// The function of `krate` its `#[alloc_error_handler]`'s expansion exports as `__rg_oom`
/// (`hir_expand::builtin::ALLOC_ERROR_HANDLER_EXPORT`), when it declares one.
#[blaze_query::tracked(returns(copy))]
fn alloc_error_handler_export(db: &Db, krate: Crate) -> Option<hir_def::FunctionId> {
    global_allocator_export(db, krate, hir_expand::builtin::ALLOC_ERROR_HANDLER_EXPORT)
}

/// The error of a program with two `#[alloc_error_handler]`s, `a`'s and `b`'s (rustc: "the
/// `#[alloc_error_handler]` in {} conflicts with allocation error handler in: {}"), naming
/// their crates in order of name.
fn two_alloc_error_handlers(db: &Db, a: Crate, b: Crate) -> String {
    let mut names = [a.named(db).to_string(), b.named(db).to_string()]; // alloc: the error's names
    names.sort_unstable();
    let [first, second] = names;
    // alloc: the error's message
    format!("two `#[alloc_error_handler]`s in one program, in crates `{first}` and `{second}`")
}

/// Whether the program of `krate` observes none of its allocations: its global allocator is
/// std's, as with no `#[global_allocator]` or one whose static is a `std::alloc::System`, which
/// does nothing but allocate. Then whether an allocation happens is not something the program
/// can read, and `hir-ty-mir`'s `HeapToStack` may move one that never leaves its function to
/// the stack (`MirOptions::heap_to_stack`).
///
/// Any other `#[global_allocator]` may count, log or otherwise act on each call where the
/// program reads it, which `core::alloc::GlobalAlloc` allows the optimizer to change but a
/// program can still tell: its allocations stay where its MIR puts them. So does one whose
/// static this can't tell (an expansion that names no static or more than one, or whose MIR
/// does not build, which compiling the export then reports): `false`, logged under target
/// `blaze` with why. Two allocators are the error the shim gives too ([`shim`]).
///
/// With `db` attached, as a query's run is: the codegen driver calls this outside any query,
/// and why the MIR does not build formats the types it names.
pub fn allocations_unobserved(db: &Db, krate: Crate) -> Result<bool, String> {
    blaze_query::db::attach(db, || allocations_unobserved_attached(db, krate))
}

fn allocations_unobserved_attached(db: &Db, krate: Crate) -> Result<bool, String> {
    let program = linked_crates(db, krate)?;
    let mut declared = program.iter().copied().filter(|&it| declares_global_allocator(db, it));
    let Some(allocator) = declared.next() else { return Ok(true) };
    if let Some(other) = declared.next() {
        return Err(two_allocators(db, allocator, other));
    }
    let kept = |what: &str| {
        let name = allocator.named(db);
        tracing::info!(
            target: "blaze",
            "heap_to_stack: every allocation is kept: the `#[global_allocator]` of crate `{name}` {what}"
        );
        Ok(false)
    };
    let Some(alloc) = global_allocator_export(db, allocator, "__blaze_global_alloc") else {
        return kept("exports no `__blaze_global_alloc`");
    };
    // The expansion's `__blaze_global_alloc` calls `GlobalAlloc::alloc(&NAME, ..)`: the one
    // static its body names is the allocator's.
    let body = match db.mir_body(hir_ty::InferBodyId::DefWithBodyId(alloc.into())) {
        Ok(body) => body,
        // alloc: the log's message
        Err(e) => {
            return kept(&format!("has a `__blaze_global_alloc` whose MIR does not build: {e:?}"));
        }
    };
    let mut statics = SmallVec::<[hir_def::StaticId; 1]>::new();
    let mut named = |operand: &hir_ty_mir::mir::Operand| {
        if let hir_ty_mir::mir::OperandKind::Static(id) = operand.kind
            && !statics.contains(&id)
        {
            statics.push(id);
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let hir_ty_mir::mir::StatementKind::Assign(_, rvalue) = &statement.kind {
                rvalue.for_each_operand(&mut named);
            }
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut named);
        }
    }
    let [id] = statics[..] else {
        // alloc: the log's message
        return kept(&format!(
            "has a `__blaze_global_alloc` that names {} statics, not one",
            statics.len()
        ));
    };
    let ty = hir_ty::InferenceResult::of(db, hir_def::DefWithBodyId::from(id))
        .expr_ty(hir_def::expr_store::Body::of(db, id.into()).root_expr());
    Ok(is_std_system(db, ty))
}

/// `ty` is `std::alloc::System`.
fn is_std_system(db: &Db, ty: hir_ty::next_solver::Ty<'_>) -> bool {
    use rustc_type_ir::inherent::IntoKind as _;
    let hir_ty::next_solver::TyKind::Adt(adt, _) = ty.kind() else { return false };
    let hir_def::AdtId::StructId(id) = adt.def_id() else { return false };
    matches!(
        id.krate(db).data(db).origin,
        base_db::CrateOrigin::Lang(base_db::LangCrateOrigin::Std)
    ) && hir_def::signatures::StructSignature::of(db, id).name.as_str() == "System"
}

/// std's default of an entry: `__rdl_alloc` for `__rust_alloc`.
fn default_of(symbol: &'static str) -> Result<&'static str, String> {
    Ok(match symbol {
        "__rust_alloc" => "__rdl_alloc",
        "__rust_dealloc" => "__rdl_dealloc",
        "__rust_realloc" => "__rdl_realloc",
        "__rust_alloc_zeroed" => "__rdl_alloc_zeroed",
        // alloc: the error's message
        other => return Err(format!("the allocator entry `{other}`, which std has no default of")),
    })
}

/// No item: the shim is named by its symbol.
pub(super) fn item(forward: &Forward) -> ItemKey {
    ItemKey(ContentId::of(ITEM_DOMAIN, forward.symbol.as_bytes()))
}

pub(super) fn code(forward: &Forward) -> CodeHash {
    let mut hasher = Hasher::new(CODE_DOMAIN);
    hasher.update(forward.symbol.as_bytes()).update(&[0xff]);
    match forward.to {
        Some(to) => hasher.update(&[1]).update(to.as_bytes()),
        None => hasher.update(&[0]),
    };
    hasher.update(&[forward.params, u8::from(forward.returns)]);
    CodeHash(hasher.finish())
}

/// What its IR asks for: the function it calls, by symbol.
pub(super) fn walk(forward: &Forward) -> Vec<Ref> {
    // alloc: one of the shim's few entries, once per program; `FrontRef::Extern` owns its name
    forward.to.iter().map(|to| FrontRef::Extern((*to).to_owned())).collect()
}

pub(super) fn built(
    db: &Db,
    forward: &Forward,
    checked: bool,
    unit: Option<super::build::Build>,
) -> crate::Result<Produced> {
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: forward.krate };
    compile_as(db, env, None, false, checked, true, unit, &mut None, |fx| {
        let mut sig = Signature::new(fx.cg.isa.default_call_conv());
        let pointer = fx.cg.target.pointer();
        sig.params.extend((0..forward.params).map(|_| AbiParam::new(pointer)));
        sig.returns.extend(forward.returns.then(|| AbiParam::new(pointer)));
        // clone: one of the shim's few entries, once per program
        fx.builder.func.signature = sig.clone();
        let entry = fx.builder.create_block();
        fx.builder.append_block_params_for_function_params(entry);
        fx.builder.switch_to_block(entry);
        // alloc: one of the shim's few entries, once per program
        let args = fx.builder.block_params(entry).to_vec();
        let results = match forward.to {
            Some(to) => {
                // alloc: one of the shim's few entries; `FuncTarget::Symbol` owns its name
                let callee = fx.cg.wants.func(FuncTarget::Symbol(to.to_owned()), &sig);
                let callee = fx.cg.declare_func_in_func(callee, fx.builder.func);
                let call = fx.builder.ins().call(callee, &args);
                // alloc: one of the shim's few entries, once per program
                fx.builder.inst_results(call).to_vec()
            }
            None => Vec::new(), // alloc: empty, allocates nothing
        };
        fx.builder.ins().return_(&results);
        Ok(())
    })
}
