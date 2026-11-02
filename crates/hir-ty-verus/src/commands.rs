//! The AIR commands of one module's proof, in order: the port of `rust_verify`'s `OpGenerator`
//! (`rust_verify/src/commands.rs`), trimmed to what blaze runs. Strongly connected components
//! of the call graph come in dependency order; for each, the functions' `req%`/`ens%`
//! declarations, then the spec-termination checks, then each body's proof, then the spec
//! definitions and broadcast axioms, which are only sound once the component's obligations are
//! met, then its trait impls' axioms.
//!
//! Left out against upstream, as blaze does not do them: expand-errors reruns, recommends
//! retries, profile reruns and the safe-API check (`Style::CheckApiSafety`, off by default
//! upstream).

use std::sync::Arc;

use air::ast::Command;
use blaze_hash::HashMap;
use vir::{
    ast::{Fun, FunctionKind, ImplPath, ItemKind, Mode, Path, TraitImpl, VirErr},
    ast_to_sst_func::{mk_fun_ctx, mk_fun_ctx_dec},
    ast_util::is_body_visible_to,
    context::Ctx,
    def::CommandsWithContext,
    recursion::Node,
    sst::{FunctionSst, KrateSst},
};

/// What a query proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueryOp {
    /// A spec function's `decreases`.
    SpecTermination,
    /// A proof or exec function's body against its clauses.
    Body,
}

/// One step of a module's proof.
pub(crate) enum Op {
    /// Declarations and axioms later queries assume.
    Context(Arc<Vec<Command>>),
    /// Assertions to prove.
    Query { op: QueryOp, function: FunctionSst, commands: Arc<Vec<CommandsWithContext>> },
}

/// Every op of the module `ctx` is for, in order. `bucket` is the functions the module owns,
/// whose obligations are proved; the rest only contribute their specifications.
pub(crate) fn ops(ctx: &mut Ctx, krate: &KrateSst, bucket: &[Fun]) -> Result<Vec<Op>, VirErr> {
    let mut func_map: HashMap<Fun, FunctionSst, std::alloc::Global> = HashMap::default(); // alloc: a module's functions, for one proof
    for function in &krate.functions {
        func_map.insert(function.x.name.clone(), function.clone());
    }
    let mut trait_impl_map: HashMap<Path, TraitImpl, std::alloc::Global> = HashMap::default(); // alloc: a module's trait impls, for one proof
    for imp in &krate.trait_impls {
        trait_impl_map.insert(imp.x.impl_path.clone(), imp.clone());
    }
    let module = ctx.module_path();
    let sccs = ctx.global.func_call_sccs.clone();
    let mut ops = Vec::new(); // alloc: a module's proof steps
    for scc_rep in sccs.iter() {
        let nodes = ctx.global.func_call_graph.get_scc_nodes(scc_rep);
        let functions: Vec<FunctionSst> = nodes
            .iter()
            .filter_map(|node| match node {
                Node::Fun(f) => func_map.get(f).cloned(),
                _ => None,
            })
            .collect(); // alloc: a component's functions

        // `handle_specs_scc_component`.
        for function in &functions {
            ctx.fun = mk_fun_ctx(function, false);
            let decl = vir::sst_to_air_func::func_decl_to_air(ctx, function);
            ctx.fun = None;
            ops.push(Op::Context(decl?));
        }
        let mut post_ops = Vec::new(); // alloc: a component's axioms
        for function in &functions {
            ctx.fun = mk_fun_ctx_dec(function, false, true);
            let axioms = vir::sst_to_air_func::func_axioms_to_air(
                ctx,
                function,
                is_body_visible_to(&function.x.body_visibility, &module),
            );
            ctx.fun = None;
            let (decl_commands, check_commands) = axioms?;
            if bucket.contains(&function.x.name) {
                ops.push(Op::Query {
                    op: QueryOp::SpecTermination,
                    function: function.clone(),
                    commands: Arc::new(check_commands),
                });
            }
            post_ops.push(Op::Context(decl_commands));
        }

        // `handle_proof_body` for each proof and exec function the module owns.
        for function in &functions {
            if function.x.mode == Mode::Spec && !matches!(function.x.item_kind, ItemKind::Const) {
                continue;
            }
            if let FunctionKind::TraitMethodImpl { inherit_body_from: Some(..), .. } = &function.x.kind {
                // Inherited from a trait's default method, which is proved in the trait.
                continue;
            }
            if !bucket.contains(&function.x.name) {
                continue;
            }
            let Some(check) = &function.x.exec_proof_check else { continue };
            ctx.fun = mk_fun_ctx(function, false);
            let commands = vir::sst_to_air_func::func_sst_to_air(ctx, function, check);
            ctx.fun = None;
            ops.push(Op::Query { op: QueryOp::Body, function: function.clone(), commands: commands? });
        }

        ops.extend(post_ops);
        for node in &nodes {
            if let Node::TraitImpl(ImplPath::TraitImplPath(impl_path)) = node {
                let mut impl_paths = vec![impl_path.clone()]; // alloc: a component's impls
                if let Some(extensions) = ctx.global.trait_impl_to_extensions.get(impl_path) {
                    impl_paths.extend(extensions.iter().cloned());
                }
                for path in &impl_paths {
                    if let Some(imp) = trait_impl_map.get(path) {
                        ops.push(Op::Context(vir::traits::trait_impl_to_air(ctx, imp)));
                    }
                }
            }
        }
    }
    Ok(ops)
}
