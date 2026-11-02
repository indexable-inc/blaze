//! The allocator shim rustc puts in a program: the entries of the global allocator that
//! `alloc` calls by name (`__rust_alloc` and the rest), each going to the
//! `#[global_allocator]`'s (the functions its expansion exports) or else to std's defaults.
//! Each entry is an instance of its own ([`Kind::Allocator`]), defined under its symbol.

use base_db::{Db, Crate};
use blaze_codegen::backend::FrontRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{CodeHash, ItemKey};
use cranelift_codegen::ir::{AbiParam, InstBuilder as _, Signature};
use hir_def::HasModule as _;
use hir_ty::{ParamEnvAndCrate, next_solver::{DbInterner, ParamEnv}};

use super::{HirInstance, Kind, places::Ref};
use crate::{
    ALLOCATOR_ENTRIES, Produced, compile_as, declares_global_allocator, linked_crates,
    fragment::FuncTarget, global_allocator_export, ptr,
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
    let program = linked_crates(db, [krate]);
    if !program.contains(&boxed.krate(db)) {
        return Ok(Vec::new()); // alloc: empty, allocates nothing
    }
    let mut declared = program.iter().copied().filter(|&it| declares_global_allocator(db, it));
    let allocator = declared.next();
    if let Some(other) = declared.next() {
        // alloc: the error's message
        return Err(format!(
            "two `#[global_allocator]`s in one program, in crates `{}` and `{}`",
            crate::fncache::crate_name(db, allocator.expect("the first")),
            crate::fncache::crate_name(db, other),
        ));
    }
    let mut shim = Vec::with_capacity(2 * ALLOCATOR_ENTRIES.len() + 2);
    for (symbol, ours, params, returns) in ALLOCATOR_ENTRIES {
        let to = match allocator {
            Some(allocator) => {
                let Some(func) = global_allocator_export(db, allocator, ours) else {
                    let name = crate::fncache::crate_name(db, allocator);
                    // alloc: the error's message
                    return Err(format!("`#[global_allocator]` of crate `{name}` exports no `{ours}`"));
                };
                shim.push(root(func));
                ours
            }
            None => default_of(symbol)?,
        };
        let params = params as u8;
        shim.push(forward(Forward { symbol, to: Some(to), params, returns, krate }));
    }
    let handler = "__rust_alloc_error_handler";
    let to = Some("__rdl_alloc_error_handler");
    shim.push(forward(Forward { symbol: handler, to, params: 2, returns: false, krate }));
    let marker = "__rust_no_alloc_shim_is_unstable_v2";
    shim.push(forward(Forward { symbol: marker, to: None, params: 0, returns: false, krate }));
    Ok(shim)
}

fn forward(forward: Forward) -> HirInstance {
    HirInstance(Kind::Allocator(forward))
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

pub(super) fn built(db: &Db, forward: &Forward, checked: bool) -> crate::Result<Produced> {
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: forward.krate };
    compile_as(db, env, None, false, checked, true, |fx| {
        let mut sig = Signature::new(fx.cg.isa.default_call_conv());
        sig.params.extend((0..forward.params).map(|_| AbiParam::new(ptr())));
        sig.returns.extend(forward.returns.then(|| AbiParam::new(ptr())));
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
