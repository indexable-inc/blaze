//! MIR's passes, as children of hir-ty's `mir`, whose types and errors they see as their own:
//! `crate::mir::MirBody` and `super::MirBody` here are `hir_ty::mir::MirBody`.

pub use hir_ty::mir::*;

// What the passes name as `super::` or `crate::mir::` that hir-ty's `mir` has by `use`.

pub(crate) use hir_def::{VariantId, hir::Expr};
pub(crate) use la_arena::{Arena, Idx};
pub(crate) use rustc_ast_ir::Mutability;

pub(crate) use hir_ty::{
    InferenceResult, MemoryMap,
    infer::PointerCast,
    next_solver::{GenericArgs, Ty},
};

#[doc(hidden)]
pub mod budget;
pub mod checks;
pub mod const_check;
mod ctfe_limit;
pub mod dispatch;
pub mod early_drop;
mod eval;
mod framed;
pub mod home;
pub mod location;
mod lower;
mod monomorphization;
mod optimize;
pub mod overflow;
mod pretty;
mod providers;
pub mod purity;
mod storage;
mod transform;

pub use eval::{
    Evaluator, MirOutput, foreign, interpret_mir,
    machine::{
        Access, AtomicOrd, EvalThread, MResult, Machine, MemKind, Plain, Ptr, RetagMode, Shadow,
        Site, Stop, TypedCx,
    },
    macro_call::{FlatInput, MacroArg, call_const_macro},
    render_const_using_debug_impl, run_function,
};
/// The function a cached function calls for the code of its body. The code generator answers
/// its calls itself (`hir-ty-codegen`'s `fncache`), writing into the caller a value made from
/// the code of its type argument, so the optimizer copies no body of it and an instance's
/// identity (`hir-ty-identity`) keys a call of it by that argument's full identity. Matched by
/// the function's name, as the code generator matches it; this is the one place the name is
/// written, so the three can't disagree about which calls are answered.
pub const CACHE_CODE: &str = "__blaze_cache_code";

/// Whether the code generator answers calls of `func` itself ([`CACHE_CODE`]).
pub fn answered_by_codegen(db: &base_db::Db, func: hir_def::FunctionId) -> bool {
    hir_def::signatures::FunctionSignature::of(db, func).name.as_str() == CACHE_CODE
}

// The memos of what this crate computes are hir-ty's; `providers` registers the code.
pub use hir_ty::providers::{
    mir_body_for_closure_query, mir_body_for_coroutine_by_move_query, mir_body_for_coroutine_query,
    mir_body_query, monomorphized_mir_body_for_closure_query,
    monomorphized_mir_body_for_coroutine_by_move_query, monomorphized_mir_body_for_coroutine_query,
    monomorphized_mir_body_query, monomorphized_optimized_mir_body_for_closure_query,
    monomorphized_optimized_mir_body_for_coroutine_by_move_query,
    monomorphized_optimized_mir_body_for_coroutine_query, monomorphized_optimized_mir_body_query,
    monomorphized_runtime_mir_body_for_closure_query,
    monomorphized_runtime_mir_body_for_coroutine_by_move_query,
    monomorphized_runtime_mir_body_for_coroutine_query, monomorphized_runtime_mir_body_query,
    optimized_mir_body_for_closure_query, optimized_mir_body_for_coroutine_by_move_query,
    optimized_mir_body_for_coroutine_query, optimized_mir_body_query,
    runtime_mir_body_for_closure_query, runtime_mir_body_for_coroutine_by_move_query,
    runtime_mir_body_for_coroutine_query, runtime_mir_body_query,
};
pub use lower::is_builtin_index;
pub use lower::{
    EarlyDrop, has_failed_expansion, inline_const_at, lower_body_to_mir, lower_to_mir_with_store,
    lower_with_early_drops, promoted_mir_body_query,
};
pub use monomorphization::erase_and_anonymize_regions;
pub use monomorphization::monomorphized;
pub use monomorphization::monomorphized_promoted_mir_body_query;
pub use monomorphization::normalize_or_keep;
pub use monomorphization::{monomorphized_args, monomorphized_const, monomorphized_ty};
pub use optimize::{
    BOX_IN_PLACE_MIN, CallerLocation, CloneId, MAX_COPIES, MAX_COPIES_PER_BODY, caller_location,
    heap_held, owner_of, passes_not_ported, relied_on_own_no_unwind,
    set_interpreter_mir_opt_level, source_span, specialized_mir_body, written_checked_at,
};
pub use pretty::{closure_path_suffix, impl_written_at, print_mir_body};
pub use storage::{FrameStorage, SharedSlots, StorageConflicts, share_greedily};
pub use transform::{
    DropState, HeldPath, drop_states, noop_landing_pads, runtime_promoted_mir_body_query,
};

/// Where rustc points at a local, its `LocalDecl`'s span: the pattern of a parameter or a
/// binding, `self`, or the expression a temporary holds; `otherwise` for one with none.
pub fn local_span(
    db: &base_db::Db,
    owner: hir_ty::InferBodyId<'_>,
    body: &MirBody<'_>,
    local: LocalId,
    otherwise: MirSpan,
) -> MirSpan {
    // An anonymous constant has no parameters.
    if let hir_ty::InferBodyId::DefWithBodyId(def) = owner
        && let Some(at) = body.param_locals.iter().position(|&it| it == local)
    {
        let store = hir_def::expr_store::Body::of(db, def);
        // The `self` parameter's local comes first.
        let has_self = store.self_param.is_some();
        if has_self && at == 0 {
            return MirSpan::SelfParam;
        }
        if let Some(param) = store.params.get(at - usize::from(has_self)) {
            return MirSpan::PatId(param.user_written);
        }
    }
    if let Some((binding, _)) = body.binding_locals.iter().find(|&(_, &it)| it == local) {
        let (_, map) = hir_def::expr_store::ExpressionStore::with_source_map(
            db,
            owner.expression_store_owner(db),
        );
        return match map.patterns_for_binding(binding).first() {
            Some(&pat) => MirSpan::PatId(pat),
            None => MirSpan::BindingId(binding),
        };
    }
    body.basic_blocks
        .iter()
        .flat_map(|(_, block)| &block.statements)
        .find(|it| matches!(it.kind, StatementKind::StorageLive(it) if it == local))
        .map_or(otherwise, |it| it.span)
}
