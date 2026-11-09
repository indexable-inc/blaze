//! VIR's checks and passes, then AIR and Z3: the port of `rust_verify`'s verifier loop
//! (`query.rs`'s checked and merged crate, `verifier.rs`'s `verify_bucket` and
//! `check_result_validity`) for one lowered function in a crate of its own.
//!
//! The solver is deterministic: the Z3 [`crate::Config`] names (air refuses one whose version
//! is not [`crate::Z3_VERSION`]), fixed seeds, and a resource limit (`rlimit`) in place of a
//! timeout, so a query that runs out is [`Outcome::Unknown`] on every machine alike. Z3's
//! "incomplete" answer is an unknown too, never a refutation.

use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::Arc,
};

use air::{
    ast::CommandX,
    context::{Context, SmtSolver, ValidityResult},
    messages::{ArcDynMessage, Diagnostics, MessageLevel},
    query_result_observer::{CheckValidResult, QueryResultObserver},
};
use vir::{
    ast::{Fun, FunctionStub, VarIdentDisambiguate, VirErr},
    context::{Ctx, GlobalCtx, WarningConfig, WarningCtx},
    def::ProverChoice,
    messages::{MessageX, VirMessageInterface},
};

use crate::{
    Config, Counterexample, Obligation, Outcome, ProofKey, RLIMIT_PER_SECOND, Refutation, SEED,
    Unknown, UnknownWhy, Unsupported, VERUS_COMMIT, Value, VerifyError, Z3_VERSION,
    commands::{self, Op, QueryOp},
    lower::{Lowered, Origin, vir_err_text},
};

/// How many more failing assertions a refuted query is asked for before only earlier ones are
/// (`--multiple-errors`), and the bound on asking at all (rule 18).
const MORE_ERRORS: u32 = 8;
const MAX_PROBES: u32 = 64;

/// The key of `lowered`'s proof under `config` (see [`ProofKey`]).
pub(crate) fn proof_key(lowered: &Lowered, config: &Config) -> ProofKey {
    let mut printed = Vec::new(); // alloc: a function's printed VIR, hashed once
    let opts = vir::printer::ToDebugSNodeOpts {
        no_span: true,
        no_type: false,
        no_fn_details: false,
        no_encoding: false,
    };
    vir::printer::write_krate(&mut printed, &lowered.krate, &opts);
    let mut hasher = blaze_ids::Hasher::new("blaze v2 verus proof 1");
    hasher
        .part(VERUS_COMMIT.as_bytes())
        .part(Z3_VERSION.as_bytes())
        .part(&config.rlimit.to_le_bytes())
        .part(&SEED.to_le_bytes())
        .part(&printed);
    ProofKey(hasher.finish())
}

/// The messages VIR and AIR report, in order.
#[derive(Default)]
struct Collect {
    messages: RefCell<Vec<ArcDynMessage>>,
}

impl Diagnostics for Collect {
    fn report(&self, msg: &ArcDynMessage) {
        self.messages.borrow_mut().push(msg.clone());
    }
    fn report_now(&self, msg: &ArcDynMessage) {
        self.report(msg);
    }
    fn report_as(&self, msg: &ArcDynMessage, _msg_as: MessageLevel) {
        self.report(msg);
    }
    fn report_as_now(&self, msg: &ArcDynMessage, _msg_as: MessageLevel) {
        self.report(msg);
    }
}

impl Collect {
    fn text(&self) -> String {
        let messages = self.messages.borrow();
        let texts: Vec<String> = messages.iter().map(|it| message_text(it)).collect(); // alloc: error message
        texts.join("; ")
    }
}

fn message_text(msg: &ArcDynMessage) -> String {
    match msg.downcast_ref::<MessageX>() {
        Some(m) => vir_err_text(&Arc::new(m.clone())),
        None => "a message air does not describe".to_owned(), // alloc: error message
    }
}

/// The model of the last refuted query: each SMT constant's value.
#[derive(Default)]
struct Capture {
    model: Option<Vec<(String, String)>>,
}

impl QueryResultObserver for Capture {
    fn on_check_valid_result(&mut self, result: &mut CheckValidResult<'_>) {
        if let CheckValidResult::Invalid { model_defs, .. } = result {
            let mut model: Vec<(String, String)> = model_defs
                .iter()
                .filter(|(_, def)| def.params.is_empty())
                .map(|(name, def)| ((**name).clone(), (*def.body).clone()))
                .collect(); // alloc: a counterexample
            model.sort();
            self.model = Some(model);
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Proves `lowered` with `config`. A panic in vendored code fails this function's verdict
/// with the panic's message, and nothing else (rule 20).
pub(crate) fn prove(lowered: &Lowered, config: &Config) -> Outcome {
    let diags = Collect::default();
    let result = catch_unwind(AssertUnwindSafe(|| prove_inner(lowered, config, &diags)));
    match result {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => Outcome::Error(err),
        Err(panic) => {
            let what = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|it| (*it).to_owned())) // alloc: error message
                .unwrap_or_else(|| "a panic with no message".to_owned()); // alloc: error message
            let reported = diags.text();
            let message = if reported.is_empty() { what } else { format!("{what} ({reported})") };
            Outcome::Error(VerifyError {
                function: lowered.display.clone(),
                stage: "panic",
                message,
            })
        }
    }
}

fn prove_inner(
    lowered: &Lowered,
    config: &Config,
    diags: &Collect,
) -> Result<Outcome, VerifyError> {
    let function = &lowered.display;
    let error = |stage: &'static str, message: String| VerifyError {
        function: function.clone(),
        stage,
        message,
    };
    let vir = |stage: &'static str| move |err: VirErr| error(stage, vir_err_text(&err));
    let virs = |stage: &'static str| {
        move |errs: Vec<VirErr>| {
            error(stage, errs.iter().map(vir_err_text).collect::<Vec<_>>().join("; "))
        }
    };
    let blocking = |stage: &'static str, errs: Vec<VirErr>| match errs.is_empty() {
        true => Ok(()),
        false => Err(virs(stage)(errs)),
    };

    // The checked crate (`query.rs`'s `_compute_checked_function` and `_compute_exportable_crate`).
    let krate = &lowered.krate;
    let mut stubs: HashMap<Fun, FunctionStub> = HashMap::new(); // alloc: VIR's own map
    for f in &krate.functions {
        stubs.insert(f.x.name.clone(), vir::ast_util::function_to_stub(f));
    }
    let stubs = Arc::new(stubs);
    let mut without_funs = (**krate).clone();
    without_funs.functions = Vec::new(); // alloc: VIR
    let without_funs = Arc::new(without_funs);
    let merged_traits =
        vir::traits::merge_external_traits(&without_funs, &[]).map_err(vir("vir"))?;
    let demote = vir::traits::DemoteTraitLookup::new(&without_funs, &[], &stubs);
    let (initial, non_blocking, _) = vir::well_formed::check_crate_initial(
        &without_funs,
        &[],
        stubs.clone(),
        merged_traits.clone(),
        false,
    );
    blocking("vir", non_blocking)?;
    let initial = initial.map_err(|err| virs("vir")(err.errors))?;
    let mut warning_ctx = WarningCtx { fun_warn_configs: HashMap::new() }; // alloc: VIR's own map
    for f in &krate.functions {
        warning_ctx.fun_warn_configs.insert(f.x.name.clone(), Some(WarningConfig(Vec::new()))); // alloc: VIR
    }

    let mut kratex = (*without_funs).clone();
    let mut dyn_checks = vec![initial.dyn_compatability_checks]; // alloc: VIR
    let mut resolution = HashMap::new(); // alloc: VIR's own map
    for f in &krate.functions {
        let f =
            vir::traits::demote_external_traits_function_calls(&demote, f).map_err(vir("vir"))?;
        let (wf, non_blocking, _) = vir::well_formed::check_function(
            &without_funs,
            stubs.clone(),
            initial.lookup.clone(),
            &demote,
            &f,
            &warning_ctx,
            false,
            false,
        );
        blocking("vir", non_blocking)?;
        let wf = wf.map_err(|err| virs("vir")(err.errors))?;
        dyn_checks.push(wf.dyn_compatability_checks);
        let f = vir::autospec::resolve_autospec_in_function(&stubs, &f).map_err(vir("vir"))?;
        let (f, _erasure, info) =
            vir::modes::check_function(stubs.clone(), initial.lookup.clone(), &f, &demote)
                .map_err(virs("modes"))?;
        if let Some(info) = info {
            resolution.insert(f.x.name.clone(), Arc::new(info));
        }
        kratex.functions.push(f);
    }
    vir::traits::set_krate_dyn_compatibility(&Vec::new(), &mut kratex); // alloc: VIR
    let mut krate = vir::ast_sort::sort_krate(&Arc::new(kratex));
    Arc::make_mut(&mut krate).arch =
        vir::ast_simplify::krate_merge_arch(&krate, &[]).map_err(vir("vir"))?;
    let (check, _) = vir::well_formed::check_crate_final(&krate, &warning_ctx);
    check.map_err(vir("vir"))?;
    vir::well_formed::finish_dyn_compatability_checks(&krate, dyn_checks).map_err(virs("vir"))?;

    // The merged crate (`_compute_merged_crate`): blaze merges no other crate yet (vstd and
    // callees join with calls).
    let mine = krate.clone();
    let unpruned =
        vir::ast_simplify::merge_krates(vec![krate], &merged_traits).map_err(vir("vir"))?; // alloc: VIR
    let (krate, _) = vir::prune::prune_krate_for_module_or_krate(
        &unpruned,
        &lowered.crate_id,
        Some(&mine),
        None,
        None,
        false,
        false,
    );
    let krate = vir::traits::demote_external_traits(diags, &warning_ctx, &krate, &demote)
        .map_err(vir("vir"))?;
    let mut warning_ctx = warning_ctx.clone();
    let krate =
        vir::traits::inherit_default_bodies(&krate, &mut warning_ctx).map_err(vir("vir"))?;
    let krate = vir::resolution_inference::krate_infer_resolution(&krate, &resolution)
        .map_err(virs("vir"))?;
    vir::well_formed::check_crate_merged(&krate).map_err(vir("vir"))?;

    // The verifier (`verify_crate_inner`, then `verify_bucket` for the function's module).
    #[expect(
        clippy::disallowed_types,
        reason = "vendored vir's `GlobalCtx::new` takes `Arc<std::sync::Mutex<..>>` for its logs; blaze passes none"
    )]
    let mut global = GlobalCtx::new(
        &krate,
        lowered.crate_id.clone(),
        lowered.span.clone(),
        config.rlimit as f32 / RLIMIT_PER_SECOND as f32,
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(std::sync::Mutex::new(None)),
        Arc::new(warning_ctx),
        SmtSolver::Z3,
        false,
        false,
        false,
        false,
        false,
    )
    .map_err(vir("vir"))?;
    vir::recursive_types::check_traits(&krate, &global).map_err(vir("vir"))?;
    let krate = vir::ast_simplify::simplify_krate(&mut global, &krate).map_err(vir("vir"))?;
    let (pruned, info) = vir::prune::prune_krate_for_module_or_krate(
        &krate,
        &lowered.crate_id,
        None,
        Some(lowered.module.clone()),
        None,
        true,
        true,
    );
    let missing = |what: &str| error("vir", format!("pruning for the module left out {what}"));
    let module = pruned
        .modules
        .iter()
        .find(|it| it.x.path == lowered.module)
        .cloned()
        .ok_or_else(|| missing("the module"))?;
    let mut ctx = Ctx::new(
        &pruned,
        global,
        module,
        info.mono_abstract_datatypes.ok_or_else(|| missing("the monomorphic types"))?,
        info.spec_fn_types,
        info.dyn_traits,
        info.used_builtins,
        info.fndef_types,
        info.resolved_typs.ok_or_else(|| missing("the resolvable types"))?,
        vir::vir_observer::Observers::default(),
    )
    .map_err(vir("vir"))?;
    let bucket: Vec<Fun> = pruned
        .functions
        .iter()
        .filter(|f| f.x.owning_module.as_ref() == Some(&lowered.module))
        .map(|f| f.x.name.clone())
        .collect(); // alloc: a module's functions
    if !bucket.contains(&lowered.fun) {
        return Err(error("vir", "pruning for the module left out the function itself".to_owned())); // alloc: error message
    }
    let bucket_set: HashSet<Fun> = bucket.iter().cloned().collect(); // alloc: VIR's own set
    let krate_sst = vir::ast_to_sst_crate::ast_to_sst_krate(&mut ctx, diags, &bucket_set, &pruned)
        .map_err(vir("sst"))?;
    let krate_sst = vir::poly::poly_krate_for_module(&mut ctx, &krate_sst);

    // The solver (`new_air_context_with_prelude`).
    let msg = Arc::new(VirMessageInterface {});
    let mut air = Context::new(msg.clone(), SmtSolver::Z3);
    air.start_solver(&config.z3).map_err(|err| error("solver", err.to_string()))?;
    air.set_expected_solver_version(Z3_VERSION.to_owned()); // alloc: air's own field
    air.set_solver_option("air_recommended_options", "true");
    air.set_solver_option("smt.random_seed", &SEED.to_string());
    air.set_solver_option("sat.random_seed", &SEED.to_string());
    air.set_rlimit(config.rlimit);
    let capture = Rc::new(RefCell::new(Capture::default()));
    let observer: Rc<RefCell<dyn QueryResultObserver>> = capture.clone();
    air.set_observers(None, Some(observer));

    let feed_all = |air: &mut Context,
                    commands: &[air::ast::Command],
                    what: &str|
     -> Result<(), VerifyError> {
        for command in commands {
            match air.command(&*msg, diags, command, Default::default()) {
                ValidityResult::Valid(_) => {}
                ValidityResult::TypeError(err) => {
                    return Err(error("air", format!("{what}: {err}")));
                }
                ValidityResult::UnexpectedOutput(out) => {
                    return Err(error("solver", format!("{what}: Z3 answered `{out}`")));
                }
                _ => {
                    return Err(error(
                        "air",
                        format!("{what}: a declaration was answered as a query"),
                    ));
                }
            }
        }
        Ok(())
    };
    let prelude = ctx.prelude(vir::prelude::PreludeConfig {
        arch_word_bits: ctx.arch_word_bits,
        smt_encoding: SmtSolver::Z3.default_encoding(),
    });
    feed_all(&mut air, &prelude, "the prelude")?;
    let module = ctx.module_path();
    let datatypes: Vec<_> = krate_sst
        .datatypes
        .iter()
        .filter(|d| vir::ast_util::is_visible_to(&d.x.visibility, &module))
        .cloned()
        .collect(); // alloc: VIR
    let batches = [
        ctx.fuel(),
        vir::traits::trait_decls_to_air(&ctx, &krate_sst),
        vir::assoc_types_to_air::assoc_type_decls_to_air(&ctx, &krate_sst.traits),
        vir::datatype_to_air::datatypes_and_primitives_to_air(&ctx, &datatypes),
        vir::traits::trait_bound_axioms(&ctx, &krate_sst.traits),
        vir::assoc_types_to_air::assoc_type_impls_to_air(&ctx, &krate_sst.assoc_type_impls),
        vir::opaque_type_to_air::opaque_types_to_air(&ctx, &krate_sst.opaque_types),
    ];
    for batch in &batches {
        feed_all(&mut air, batch, "the module's declarations")?;
    }
    for f in &krate_sst.functions {
        ctx.fun = vir::ast_to_sst_func::mk_fun_ctx(f, false);
        let names = vir::sst_to_air_func::func_name_to_air(&ctx, diags, f);
        ctx.fun = None;
        feed_all(&mut air, &names.map_err(vir("air"))?, "a function's name")?;
    }

    let ops = commands::ops(&mut ctx, &krate_sst, &bucket).map_err(vir("air"))?;
    let mut refutations = Vec::new(); // alloc: a function's failures
    let mut unknown: Option<Unknown> = None;
    for op in ops {
        let (query_op, function_sst, commands) = match op {
            Op::Context(commands) => {
                feed_all(&mut air, &commands, "the specifications")?;
                continue;
            }
            Op::Query { op, function, commands } => (op, function, commands),
        };
        for with_context in commands.iter() {
            if with_context.prover_choice != ProverChoice::DefaultProver {
                return Ok(Outcome::Unsupported(Unsupported {
                    function: function.clone(),
                    what: format!("a proof by {:?}", with_context.prover_choice),
                    at: function_sst.span.as_string.clone(), // clone: a span's text
                }));
            }
            for command in with_context.commands.iter() {
                if !matches!(&**command, CommandX::CheckValid(_)) {
                    feed_all(&mut air, std::slice::from_ref(command), "a query's declarations")?;
                    continue;
                }
                let mut result = air.command(&*msg, diags, command, Default::default());
                let mut probes = 0u32;
                loop {
                    let (has_model, err) = match result {
                        ValidityResult::Valid(_) => break,
                        ValidityResult::Canceled => {
                            unknown.get_or_insert(Unknown {
                                function: function.clone(),
                                obligation: None,
                                why: UnknownWhy::Rlimit,
                                rlimit: config.rlimit,
                                rlimit_used: None,
                            });
                            break;
                        }
                        ValidityResult::TypeError(err) => {
                            return Err(error("air", format!("a query: {err}")));
                        }
                        ValidityResult::UnexpectedOutput(out) => {
                            return Err(error("solver", format!("a query: Z3 answered `{out}`")));
                        }
                        ValidityResult::Invalid(model, err, _) => (model.is_some(), err),
                    };
                    let classified = err.as_ref().map(|err| classify(err, query_op));
                    if air.last_unknown_incomplete() {
                        unknown.get_or_insert(Unknown {
                            function: function.clone(),
                            obligation: classified.map(|it| it.0),
                            why: UnknownWhy::Incomplete,
                            rlimit: config.rlimit,
                            rlimit_used: None,
                        });
                        break;
                    }
                    let Some((obligation, message)) = classified else {
                        return Err(error(
                            "air",
                            "a refuted query with no message naming what failed".to_owned(),
                        )); // alloc: error message
                    };
                    let model = capture.borrow_mut().model.take().unwrap_or_default();
                    refutations.push(Refutation {
                        function: function.clone(),
                        obligation,
                        message,
                        counterexample: counterexample(lowered, &model),
                    });
                    if !has_model {
                        break;
                    }
                    probes += 1;
                    if probes > MAX_PROBES {
                        return Err(error(
                            "air",
                            format!(
                                "asked Z3 for {MAX_PROBES} more failing assertions of one query, and it kept finding them"
                            ),
                        ));
                    }
                    result =
                        air.check_valid_again(diags, probes >= MORE_ERRORS, Default::default());
                }
                air.finish_query();
            }
        }
    }
    let rlimit_used = air.get_rlimit_count().map(|(_, run)| run);
    drop(ctx);
    if !refutations.is_empty() {
        return Ok(Outcome::Refuted(refutations));
    }
    if let Some(mut unknown) = unknown {
        unknown.rlimit_used = rlimit_used;
        return Ok(Outcome::Unknown(unknown));
    }
    Ok(Outcome::Proved)
}

/// What a failed query's message is about: the clause any of its labels or spans comes from,
/// else the code its first span is.
fn classify(msg: &ArcDynMessage, op: QueryOp) -> (Obligation, String) {
    let Some(m) = msg.downcast_ref::<MessageX>() else {
        let what = "a failure air does not describe".to_owned(); // alloc: error message
        return (Obligation::Code { what: what.clone(), text: String::new() }, what);
    };
    let spans = || m.labels.iter().map(|label| &label.span).chain(m.spans.iter());
    let origin = |span: &vir::messages::Span| -> Option<Origin> {
        let raw: &dyn Any = &*span.raw_span;
        raw.downcast_ref::<Origin>().cloned()
    };
    for span in spans() {
        if let Some(Origin::Clause(clause)) = origin(span) {
            return (Obligation::Clause(clause), m.note.clone()); // clone: a verdict's own copy
        }
    }
    let text = spans()
        .find_map(|span| match origin(span) {
            Some(Origin::Code(text)) => Some(text),
            _ => None,
        })
        .or_else(|| m.spans.first().map(|it| it.as_string.clone())) // clone: a verdict's own copy
        .unwrap_or_default();
    let what = match op {
        QueryOp::Body => m.note.clone(), // clone: a verdict's own copy
        QueryOp::SpecTermination => format!("a spec function's termination: {}", m.note),
    };
    (Obligation::Code { what, text }, m.note.clone())
}

/// Each parameter's value in `model`: the SMT constant VIR names it (`n!` once simplification
/// renames parameters, `n~id` before), or a snapshot of it (`n!@0`); a parameter the model
/// leaves out is unconstrained.
fn counterexample(lowered: &Lowered, model: &[(String, String)]) -> Counterexample {
    let mut params = Vec::new(); // alloc: a counterexample
    for param in &lowered.params {
        let base = (*param.var.0).clone();
        let candidates = [
            vir::def::unique_var_name(base.clone(), VarIdentDisambiguate::VirParam),
            vir::def::unique_var_name(base, param.var.1.clone()),
        ];
        let value = candidates.iter().find_map(|candidate| {
            model
                .iter()
                .find(|(name, _)| name == candidate || name.starts_with(&format!("{candidate}@")))
                .map(|(_, value)| Value::Is(value.clone())) // clone: a verdict's own copy
        });
        params.push((param.name.clone(), value.unwrap_or(Value::Any)));
    }
    Counterexample { params }
}
