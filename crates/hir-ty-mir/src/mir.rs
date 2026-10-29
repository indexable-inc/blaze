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

pub mod const_check;
mod eval;
pub mod location;
mod lower;
mod monomorphization;
mod optimize;
pub mod overflow;
mod pretty;
pub mod purity;
mod storage;
mod transform;
pub mod type_name;

pub use eval::{
    Evaluator, interpret_mir,
    macro_call::{FlatInput, MacroArg, call_const_macro},
    render_const_using_debug_impl,
};
pub use lower::is_builtin_index;
pub use lower::{
    inline_const_at, lower_body_to_mir, lower_to_mir_with_store, mir_body_for_closure_query,
    mir_body_for_coroutine_by_move_query, mir_body_for_coroutine_query, mir_body_query,
    promoted_mir_body_query, set_mir_capacity,
};
pub use monomorphization::monomorphized;
pub use monomorphization::opaque_of_impl;
pub use monomorphization::{
    monomorphized_mir_body_for_closure_query, monomorphized_mir_body_for_coroutine_by_move_query,
    monomorphized_mir_body_for_coroutine_query, monomorphized_mir_body_query,
    monomorphized_promoted_mir_body_query,
};
pub use optimize::{
    monomorphized_optimized_mir_body_for_closure_query, monomorphized_optimized_mir_body_query,
    optimized_mir_body_for_closure_query, optimized_mir_body_query, passes_not_ported,
    set_interpreter_mir_opt_level,
};
pub use pretty::{closure_path_suffix, impl_written_at, print_mir_body};
pub use storage::{FrameStorage, StorageConflicts};
pub use transform::{
    monomorphized_runtime_mir_body_for_closure_query,
    monomorphized_runtime_mir_body_for_coroutine_by_move_query,
    monomorphized_runtime_mir_body_for_coroutine_query, monomorphized_runtime_mir_body_query,
    runtime_mir_body_for_closure_query, runtime_mir_body_for_coroutine_by_move_query,
    runtime_mir_body_for_coroutine_query, runtime_mir_body_query, runtime_promoted_mir_body_query,
    set_runtime_mir_capacity,
};
