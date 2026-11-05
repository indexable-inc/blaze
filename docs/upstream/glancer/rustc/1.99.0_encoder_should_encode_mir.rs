/// unreachable and yet it can be evaluated in other crates:
///
/// ```
/// const fn f() -> usize { 0 }
/// pub struct S { pub a: [usize; f()] }
/// ```
fn should_encode_mir(
    tcx: TyCtxt<'_>,
    reachable_set: &LocalDefIdSet,
    def_id: LocalDefId,
) -> (bool, bool) {
    match tcx.def_kind(def_id) {
        // instance_mir uses mir_for_ctfe rather than optimized_mir for constructors
        DefKind::Ctor(_, _) => (true, false),
        // Constants
        DefKind::AnonConst | DefKind::AssocConst { .. } | DefKind::Const { .. } => (true, false),
        // Coroutines require optimized MIR to compute layout.
        DefKind::Closure if tcx.is_coroutine(def_id.to_def_id()) => (false, true),
        DefKind::SyntheticCoroutineBody => (false, true),
        // Full-fledged functions + closures
        DefKind::AssocFn | DefKind::Fn | DefKind::Closure => {
            let opt = tcx.sess.opts.unstable_opts.always_encode_mir
                || (tcx.sess.opts.output_types.should_codegen()
                    && reachable_set.contains(&def_id)
                    && (tcx.generics_of(def_id).requires_monomorphization(tcx)
                        || tcx.cross_crate_inlinable(def_id)));
            // Comptime fns do not have optimized MIR at all.
            let opt =
                opt && !matches!(tcx.constness(def_id), hir::Constness::Const { always: true });
            // The function has a `const` modifier or is in a `const trait`.
            let is_const_fn = tcx.is_const_fn(def_id.to_def_id());
            (is_const_fn, opt)
        }
        // The others don't have MIR.
        _ => (false, false),
    }
}

fn should_encode_variances<'tcx>(tcx: TyCtxt<'tcx>, def_id: DefId, def_kind: DefKind) -> bool {
    match def_kind {
        DefKind::Struct
