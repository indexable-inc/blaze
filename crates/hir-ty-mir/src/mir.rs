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
pub mod const_check;
mod eval;
mod framed;
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
pub mod type_name;

pub use eval::{
    Evaluator, MirOutput, foreign, interpret_mir,
    machine::{
        Access, AtomicOrd, EvalThread, MResult, Machine, MemKind, Plain, Ptr, RetagMode, Shadow,
        Site, Stop, TypedCx,
    },
    macro_call::{FlatInput, MacroArg, call_const_macro},
    render_const_using_debug_impl, run_function,
};
// The memos of what this crate computes are hir-ty's; `providers` registers the code.
pub use hir_ty::providers::{
    mir_body_for_closure_query, mir_body_for_coroutine_by_move_query,
    mir_body_for_coroutine_query, mir_body_query,
    monomorphized_mir_body_for_closure_query, monomorphized_mir_body_for_coroutine_by_move_query,
    monomorphized_mir_body_for_coroutine_query, monomorphized_mir_body_query,
    monomorphized_optimized_mir_body_for_closure_query,
    monomorphized_optimized_mir_body_for_coroutine_by_move_query,
    monomorphized_optimized_mir_body_for_coroutine_query, monomorphized_optimized_mir_body_query,
    monomorphized_runtime_mir_body_for_closure_query,
    monomorphized_runtime_mir_body_for_coroutine_by_move_query,
    monomorphized_runtime_mir_body_for_coroutine_query, monomorphized_runtime_mir_body_query,
    optimized_mir_body_for_closure_query, optimized_mir_body_for_coroutine_by_move_query,
    optimized_mir_body_for_coroutine_query, optimized_mir_body_query,
    runtime_mir_body_for_closure_query, runtime_mir_body_for_coroutine_by_move_query,
    runtime_mir_body_for_coroutine_query, runtime_mir_body_query, set_mir_capacity,
    set_runtime_mir_capacity,
};
pub use lower::is_builtin_index;
pub use lower::{
    has_failed_expansion, inline_const_at, lower_body_to_mir, lower_to_mir_with_store,
    promoted_mir_body_query,
};
pub use monomorphization::monomorphized;
pub use monomorphization::{monomorphized_args, monomorphized_const, monomorphized_ty};
pub use monomorphization::monomorphized_promoted_mir_body_query;
pub use monomorphization::opaque_of_impl;
pub use monomorphization::erase_and_anonymize_regions;
pub use monomorphization::normalize_or_keep;
pub use optimize::{
    CallerLocation, caller_location, owner_of, passes_not_ported, set_interpreter_mir_opt_level,
    source_span, written_checked_at,
};
pub use pretty::{closure_path_suffix, impl_written_at, print_mir_body};
pub use storage::{FrameStorage, SharedSlots, StorageConflicts, share_greedily};
pub use transform::{noop_landing_pads, runtime_promoted_mir_body_query};
