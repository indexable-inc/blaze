//! `parallelizable` and `serial_dependence`: a sequential loop or iterator chain whose
//! iterations are independent and worth running in parallel, and one that would be but for
//! one carried dependence
//! ([docs/v2/plan/lanes/lints/parallel.md](../../../../docs/v2/plan/lanes/lints/parallel.md)).
//!
//! Judged on the body's MIR, so a finding is exact: a `for` loop's iterations write only memory
//! no other iteration reads or writes, carry values only as reductions whose operator is
//! associative and commutative, share only what is `Sync`, and call only what has no effect
//! another order would show ([`effects`], summaries bottom-up). An iterator chain's closures
//! are held to the same. Where blaze can't tell (a `dyn` call, a foreign function, a pointer it
//! can't trace) it says nothing. A loop is flagged only when its work, estimated from its MIR
//! and its callees' and multiplied by its trip count, passes `parallel-threshold`.

mod deps;
mod effects;
mod iter;
#[cfg(test)]
mod prop;
mod region;
#[cfg(test)]
mod tests;

use base_db::Db;
use hir_def::{DefWithBodyId, HasModule as _, ItemContainerId, hir::Expr};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate,
    db::HirDatabase as _,
    next_solver::{GenericArgs, Ty, TyKind},
    traits::implements_trait_unique,
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};
use syntax::{
    NodeOrToken, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use self::{
    deps::{Dependence, Found, Mir, Reduced, Reduction, Verdict},
    effects::{DEEP, DIRECT, STATIC, UNKNOWN, deep, direct},
    iter::{Chain, Head, Std, rayon_name},
};
use super::Lint;
use crate::{check::Cx, diag::Applicability};

/// The work, in MIR statements, below which a loop is not worth splitting:
/// `parallel-threshold`.
const THRESHOLD: i64 = 50_000;

/// The trip count taken for a loop whose count is not written down: `parallel-assumed-trips`.
const ASSUMED_TRIPS: i64 = 1_000;

/// A finding of a body, before it is reported.
struct Finding {
    lint: Lint,
    range: TextRange,
    message: String,
    notes: Vec<String>,
    helps: Vec<String>,
    suggestion: Option<(TextRange, String, String, Applicability)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    let (on_parallel, on_serial) =
        (cx.on_blaze(Lint::Parallelizable), cx.on_blaze(Lint::SerialDependence));
    if !(on_parallel || on_serial) {
        return;
    }
    let found = find(cx, on_parallel, on_serial);
    let root = cx.root();
    for finding in found {
        let at = match root.text_range().contains_range(finding.range) {
            true => match root.covering_element(finding.range) {
                NodeOrToken::Node(it) => it,
                NodeOrToken::Token(it) => it.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(finding.range);
        let suggestion = finding
            .suggestion
            .map(|(range, message, code, applicability)| (cx.span(range), message, code, applicability));
        let diagnostic = cx.blaze(finding.lint, at, span, finding.message);
        for note in finding.notes {
            diagnostic.note(note);
        }
        if let Some((span, message, code, applicability)) = suggestion {
            diagnostic.suggestion(span, message, code, applicability);
        }
        for help in finding.helps {
            diagnostic.help(help);
        }
        diagnostic
            .help(finding.lint.help())
            .help(format!("for further information visit {}", finding.lint.docs_url()));
    }
}

/// What a body's loops and chains look like.
struct Body<'c, 'a, 'db> {
    cx: &'c Cx<'db, 'a>,
    m: Mir<'c, 'db>,
    analysis: &'c hir::BodyAnalysis<'db>,
    threshold: u64,
    assumed: u64,
    /// The file imports rayon's prelude, so its parallel methods are in scope.
    rayon: bool,
    /// The crate depends on `blaze_graph`, whose pool can run the iterations.
    pool: bool,
}

fn find(cx: &Cx<'_, '_>, on_parallel: bool, on_serial: bool) -> Vec<Finding> {
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else { return found };
    let env = ParamEnvAndCrate {
        param_env: db.trait_environment(def.generic_def(db)),
        krate: def.krate(db),
    };
    let std = Std::new(cx);
    let hir = analysis.body();
    let conf = cx.conf();
    let body = Body {
        cx,
        m: Mir::new(db, env, mir, hir, &std, reassociate(cx)),
        analysis,
        threshold: conf.int("parallel-threshold", THRESHOLD).max(0) as u64,
        assumed: conf.int("parallel-assumed-trips", ASSUMED_TRIPS).max(1) as u64,
        rayon: cx.text().contains("rayon::prelude"),
        pool: cx.krate().dependencies(db).iter().any(|it| it.name.as_str() == "blaze_graph"),
    };
    for lp in deps::for_loops(hir, mir) {
        if let Some(finding) = body.for_loop(&lp)
            && (finding.lint == Lint::Parallelizable && on_parallel
                || finding.lint == Lint::SerialDependence && on_serial)
        {
            found.push(finding);
        }
    }
    for (id, expr) in hir.exprs() {
        let Expr::MethodCall { receiver, method_name, args, .. } = expr else { continue };
        if let Some(finding) = body.chain(id, receiver, method_name.as_str(), args)
            && (finding.lint == Lint::Parallelizable && on_parallel
                || finding.lint == Lint::SerialDependence && on_serial)
        {
            found.push(finding);
        }
    }
    found
}

/// Whether the item linted is `#[blaze::reassociate]`, blaze's own attribute that lets its
/// float reductions be computed in any order (and so round differently).
fn reassociate(cx: &Cx<'_, '_>) -> bool {
    ast::AnyHasAttrs::cast(cx.root()).is_some_and(|item| {
        item.attrs()
            .any(|attr| attr.path().is_some_and(|path| path.syntax().text() == "blaze::reassociate"))
    })
}

/// What a closure of a chain does, as the chain's iterations see it.
enum Closure {
    /// It changes nothing another iteration sees; it runs `cost` and may write through its
    /// item when `writes_item`.
    Independent { cost: u64, writes_item: bool },
    /// It changes what it captures.
    Captures,
}

impl<'db> Body<'_, '_, 'db> {
    fn db(&self) -> &'db Db {
        self.m.db
    }

    fn implements(&self, ty: Ty<'db>, trait_: Option<hir_def::TraitId>) -> bool {
        trait_.is_some_and(|it| implements_trait_unique(ty, self.m.db, self.m.env, it))
    }

    /// What the closure or function `ty` an adaptor or consumer calls does, if it has no
    /// effect another order would show and may be shared between threads.
    fn closure(&self, ty: Ty<'db>) -> Option<Closure> {
        let (db, env) = (self.db(), self.m.env);
        let (summary, cost) = match ty.kind() {
            TyKind::Closure(id, args) => {
                let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                (
                    effects::closure_effects(db, env.store(), id.0, parent.store()),
                    effects::closure_cost(db, env.store(), id.0, parent.store()),
                )
            }
            TyKind::FnDef(def, args) => {
                let hir_def::CallableDefId::FunctionId(f) = def.0 else { return None };
                (
                    effects::effects(db, env.store(), f, args.skip_binder().store()),
                    effects::cost(db, env.store(), f, args.skip_binder().store()),
                )
            }
            _ => return None,
        };
        if !summary.allowed()
            || !self.implements(ty, self.m.std.sync)
            || !self.implements(ty, self.m.std.send)
        {
            return None;
        }
        let closure = matches!(ty.kind(), TyKind::Closure(..));
        let captures = if closure { direct(0) | deep(0) } else { 0 };
        if summary.writes & captures != 0 {
            return Some(Closure::Captures);
        }
        Some(Closure::Independent {
            cost: u64::from(cost),
            writes_item: summary.writes & !captures & (DIRECT | DEEP) != 0,
        })
    }

    /// Whether the items of the iterator `ty` may move to the thread that consumes them.
    fn items_send(&self, ty: Ty<'db>) -> bool {
        let (Some(iterator), Some(send)) = (self.m.std.iterator, self.m.std.send) else {
            return false;
        };
        let Some(item) = self.cx.assoc_ty(&self.analysis.ty(ty), iterator.into(), "Item") else {
            return false;
        };
        item.impls_trait(self.db(), send.into(), &[])
    }

    /// Whether a `Map` closure's result points only into its item or into new memory.
    fn returns_item(&self, ty: Ty<'db>) -> bool {
        let (db, env) = (self.db(), self.m.env);
        let returns = match ty.kind() {
            TyKind::Closure(id, args) => {
                let parent = GenericArgs::new_from_slice(args.as_closure().parent_args());
                effects::closure_effects(db, env.store(), id.0, parent.store()).returns
            }
            TyKind::FnDef(def, args) => match def.0 {
                hir_def::CallableDefId::FunctionId(f) => {
                    effects::effects(db, env.store(), f, args.skip_binder().store()).returns
                }
                _ => return false,
            },
            _ => return false,
        };
        returns & (direct(0) | deep(0) | STATIC | UNKNOWN) == 0
    }

    /// The closures of `chain` and the chains it takes in: their work for one item, or the
    /// dependence the first that changes its captures is.
    fn closures(&self, chain: &Chain<'db>) -> Option<Result<u64, Dependence>> {
        let mut work = chain.overhead();
        for &ty in &chain.closures {
            match self.closure(ty)? {
                Closure::Independent { cost, writes_item: false } => work += cost,
                Closure::Independent { writes_item: true, .. } => return None,
                Closure::Captures => {
                    return Some(Err(Dependence::Carried { name: "an adaptor's closure".to_owned() }));
                }
            }
        }
        for other in &chain.others {
            match self.closures(other)? {
                Ok(it) => work += it,
                Err(it) => return Some(Err(it)),
            }
        }
        Some(Ok(work))
    }

    fn range(&self, expr: hir_def::hir::ExprId) -> Option<TextRange> {
        self.analysis.expr_range(expr)
    }

    fn snippet(&self, expr: hir_def::hir::ExprId) -> Option<&str> {
        let range = self.range(expr)?;
        Some(self.cx.snippet(range))
    }

    /// The text of the chain starting at `head` with its source made parallel, and the range
    /// and new name of the source's method, when it has one.
    fn parallel_head(
        &self,
        head_expr: hir_def::hir::ExprId,
        head: &Head,
    ) -> Option<(String, Option<(TextRange, &'static str)>)> {
        let head_range = self.range(head_expr)?;
        let text = self.cx.snippet(head_range);
        if let Some((call, name)) = head.call {
            let call_range = self.range(call)?;
            let call_text = self.cx.snippet(call_range);
            let at = call_text.rfind(&format!(".{name}("))? + 1;
            let start = call_range.start() + TextSize::from(at as u32);
            let name_range = TextRange::at(start, TextSize::from(name.len() as u32));
            let offset = usize::from(start.checked_sub(head_range.start())?);
            let par = rayon_name(name)?;
            let mut rewritten = text.to_owned();
            rewritten.replace_range(offset..offset + name.len(), par);
            return Some((rewritten, Some((name_range, par))));
        }
        if let Some(range) = head.range {
            let range_text = self.snippet(range)?;
            let rewritten = match text == range_text {
                true => format!("({range_text}).into_par_iter()"),
                false => {
                    let wrapped = format!("({range_text})");
                    match text.contains(&wrapped) {
                        true => text.replacen(&wrapped, &format!("{wrapped}.into_par_iter()"), 1),
                        false => text.replacen(range_text, &format!("{wrapped}.into_par_iter()"), 1),
                    }
                }
            };
            return Some((rewritten, None));
        }
        let base = head.base?;
        let hir = self.analysis.body();
        let rewritten = match hir.expr(base) {
            Expr::Ref { expr, mutability, .. } => {
                let inner = self.snippet(expr)?;
                match mutability {
                    hir_def::type_ref::Mutability::Mut => format!("{inner}.par_iter_mut()"),
                    hir_def::type_ref::Mutability::Shared => format!("{inner}.par_iter()"),
                }
            }
            _ => format!("{}.into_par_iter()", self.snippet(base)?),
        };
        Some((rewritten, None))
    }

    /// The note that says how much work the loop does, and the trip count's source.
    fn work_note(&self, work: u64, trips: Option<u128>) -> Option<String> {
        let (count, known) = match trips {
            Some(it) => (u64::try_from(it).unwrap_or(u64::MAX), true),
            None => (self.assumed, false),
        };
        let total = work.saturating_mul(count);
        if total < self.threshold || count < 2 {
            return None;
        }
        let trips = match known {
            true => format!("{count} iterations"),
            false => format!("{count} iterations (not known; `parallel-assumed-trips`)"),
        };
        Some(format!(
            "each iteration runs about {work} MIR statements, over {trips}: {total}, past \
             `parallel-threshold` ({})",
            self.threshold
        ))
    }

    fn reduction_notes(reductions: &[Reduced], notes: &mut Vec<String>) {
        for reduced in reductions {
            let mut note = format!(
                "`{}` is {}, which any order of the iterations gives the same of",
                reduced.name,
                reduced.reduction.said()
            );
            if reduced.overflow {
                note.push_str(
                    "; with overflow checks on, which iteration overflows, if one does, may differ",
                );
            }
            if reduced.float {
                note.push_str(
                    "; it is of floats, which round differently in another order, and qualifies \
                     only because the function is `#[blaze::reassociate]`",
                );
            }
            notes.push(note);
        }
    }

    fn pool_help(&self, helps: &mut Vec<String>) {
        if self.pool {
            helps.push(
                "or split the iterations over blaze's pool: `blaze_graph::pool::Pool::scope`"
                    .to_owned(),
            );
        }
    }

    fn for_loop(&self, lp: &deps::Loop) -> Option<Finding> {
        let iter_ty = self.m.mir.locals[lp.iter].ty.as_ref();
        let chain = Chain::of(self.m.std, iter_ty)?;
        let closures = self.closures(&chain)?;
        let head = Head::of(self.db(), self.analysis.body(), self.analysis.infer(), lp.head);
        let range = self.range(lp.expr)?;
        let verdict = match closures {
            Ok(_) => self.m.look(lp, &chain, |ty| self.returns_item(ty)),
            Err(dependence) => match self.m.look(lp, &chain, |ty| self.returns_item(ty)) {
                Verdict::Parallel(found) => Verdict::Serial(found, dependence),
                _ => Verdict::No,
            },
        };
        let (parallel_head, _) = self.parallel_head(lp.head, &head)?;
        match verdict {
            Verdict::Parallel(found) => {
                let work = self.work_note(found.work, head.trips)?;
                let mut notes = vec![work];
                self.found_notes(&found, &mut notes);
                let mut helps = Vec::new();
                let consumer = match found.reductions.as_slice() {
                    [] => ".for_each(|..| ..)".to_owned(),
                    [one] => format!(".map(|..| ..){} for `{}`", one.reduction.rayon(), one.name),
                    _ => ".fold(..).reduce(..) for the reductions".to_owned(),
                };
                helps.push(format!("run the iterations with rayon: `{parallel_head}{consumer}`"));
                if let Some(first) = found.indexed.first() {
                    helps.push(format!(
                        "rayon's closures may not write what they capture: reach `{first}`'s \
                         elements through `{first}.par_iter_mut().enumerate()` (or `.zip`) \
                         instead of by index"
                    ));
                }
                self.pool_help(&mut helps);
                let suggestion = match found.reductions.is_empty() && found.indexed.is_empty() {
                    true => self.for_each_rewrite(lp, &parallel_head, found.may_panic),
                    false => None,
                };
                Some(Finding {
                    lint: Lint::Parallelizable,
                    range,
                    message: "this loop's iterations are independent: it can run them in parallel"
                        .to_owned(),
                    notes,
                    helps,
                    suggestion,
                })
            }
            Verdict::Serial(found, dependence) => {
                let work = self.work_note(found.work, head.trips)?;
                Some(Finding {
                    lint: Lint::SerialDependence,
                    range,
                    message: format!(
                        "this loop would run its iterations in parallel but for one dependence: {}",
                        dependence.said()
                    ),
                    notes: vec![work],
                    helps: vec![dependence.help()],
                    suggestion: None,
                })
            }
            Verdict::No => None,
        }
    }

    fn found_notes(&self, found: &Found, notes: &mut Vec<String>) {
        Self::reduction_notes(&found.reductions, notes);
        if !found.indexed.is_empty() {
            let names: Vec<String> = found.indexed.iter().map(|it| format!("`{it}`")).collect();
            notes.push(format!(
                "each iteration writes {} only at its own index",
                names.join(" and ")
            ));
        }
        if found.item_writes {
            notes.push("each iteration writes only through its own item".to_owned());
        }
        if found.may_panic && (found.item_writes || !found.indexed.is_empty()) {
            notes.push(
                "if an iteration panics, the others may have run further than in order: what \
                 they wrote before the panic differs"
                    .to_owned(),
            );
        }
    }

    /// `for pat in head { body }` as `head.par_iter().for_each(|pat| { body });`, when the
    /// body leaves the loop only by its end.
    fn for_each_rewrite(
        &self,
        lp: &deps::Loop,
        parallel_head: &str,
        may_panic: bool,
    ) -> Option<(TextRange, String, String, Applicability)> {
        let range = self.range(lp.expr)?;
        let head = self.range(lp.head)?;
        let body = self.range(lp.body)?;
        let hir = self.analysis.body();
        let leaves = hir.exprs().any(|(id, expr)| {
            matches!(expr, Expr::Break { .. } | Expr::Continue { .. } | Expr::Return { .. })
                && self.range(id).is_some_and(|it| body.contains_range(it))
        });
        if leaves {
            return None;
        }
        let text = self.cx.snippet(range);
        let before_head = usize::from(head.start().checked_sub(range.start())?);
        let pat = text.get(..before_head)?.strip_prefix("for")?.trim().strip_suffix("in")?.trim();
        let code = format!("{parallel_head}.for_each(|{pat}| {});", self.cx.snippet(body));
        let applicability = match self.rayon && !may_panic {
            true => Applicability::MachineApplicable,
            false => Applicability::MaybeIncorrect,
        };
        Some((range, "run it with rayon".to_owned(), code, applicability))
    }

    /// A consumer of an iterator chain: `xs.iter().map(f).sum()`.
    fn chain(
        &self,
        id: hir_def::hir::ExprId,
        receiver: hir_def::hir::ExprId,
        method: &str,
        args: &[hir_def::hir::ExprId],
    ) -> Option<Finding> {
        const CONSUMERS: [&str; 9] =
            ["sum", "product", "count", "min", "max", "for_each", "collect", "any", "all"];
        if !CONSUMERS.contains(&method) {
            return None;
        }
        let (db, std) = (self.db(), self.m.std);
        let infer = self.analysis.infer();
        let (f, _) = infer.method_resolution(id)?;
        let ItemContainerId::TraitId(trait_) = f.loc(db).container else { return None };
        if Some(trait_) != std.iterator {
            return None;
        }
        let receiver_ty = infer.expr_ty(receiver);
        let chain = Chain::of(std, receiver_ty)?;
        if !self.items_send(receiver_ty) {
            return None;
        }
        let mut work = match self.closures(&chain)? {
            Ok(it) => it,
            Err(dependence) => return self.serial_chain(id, receiver, dependence, chain.overhead()),
        };
        let output = infer.expr_ty(id);
        let mut reduction = None;
        let mut writes_item = false;
        match method {
            "for_each" | "any" | "all" => {
                let closure = infer.expr_ty(*args.first()?);
                match self.closure(closure)? {
                    Closure::Independent { cost, writes_item: writes } => {
                        work += cost;
                        writes_item = writes;
                    }
                    Closure::Captures => {
                        let dependence = Dependence::Carried { name: format!(".{method}(..)") };
                        return self.serial_chain(id, receiver, dependence, work);
                    }
                }
            }
            "sum" | "product" | "min" | "max" => {
                let it = match method {
                    "sum" => Reduction::Sum,
                    "product" => Reduction::Product,
                    "min" => Reduction::Min,
                    _ => Reduction::Max,
                };
                let result = match method {
                    "min" | "max" => output.as_adt().and_then(|(_, args)| args.types().next())?,
                    _ => output,
                };
                if matches!(result.kind(), TyKind::Float(_)) && !self.m.reassociate {
                    let dependence =
                        Dependence::Float { name: format!(".{method}()"), reduction: it };
                    return self.serial_chain(id, receiver, dependence, work);
                }
                if !matches!(result.kind(), TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)) {
                    return None;
                }
                reduction = Some(it);
            }
            "collect" => {
                let (adt, _) = output.as_adt()?;
                let is = |it: Option<hir_def::AdtId>| it == Some(adt);
                if is(std.hash_set) || is(std.hash_map) {
                    let dependence = Dependence::HashOrder { name: ".collect()".to_owned() };
                    return self.serial_chain(id, receiver, dependence, work);
                }
                if !(is(std.vec) || is(std.string) || is(std.btree_set) || is(std.btree_map)) {
                    return None;
                }
            }
            _ => reduction = Some(Reduction::Count),
        }
        if writes_item && !chain.disjoint(|ty| self.returns_item(ty)) {
            return None;
        }
        let head = Head::of(db, self.analysis.body(), infer, receiver);
        let work_note = self.work_note(work, head.trips)?;
        let range = self.range(id)?;
        let mut notes = vec![work_note];
        notes.push(
            "its closures change nothing another item's call sees, and call nothing whose \
             effect another order would show"
                .to_owned(),
        );
        if let Some(reduction) = reduction {
            notes.push(format!(
                "`.{method}()` is {}, which any order gives the same of",
                reduction.said()
            ));
        }
        if method == "collect" {
            notes.push("rayon's `collect` keeps the items' order".to_owned());
        }
        let (parallel_head, rename) = self.parallel_head(receiver, &head)?;
        let mut helps = Vec::new();
        let suggestion = match rename {
            Some((name_range, par)) if chain.rewrites_locally() => {
                let applicability = match self.rayon {
                    true => Applicability::MachineApplicable,
                    false => Applicability::MaybeIncorrect,
                };
                Some((name_range, format!("use rayon's `{par}`"), par.to_owned(), applicability))
            }
            _ => {
                helps.push(format!("run it with rayon: `{parallel_head}.{method}(..)`"));
                None
            }
        };
        self.pool_help(&mut helps);
        Some(Finding {
            lint: Lint::Parallelizable,
            range,
            message: "this iterator chain's items are independent: it can run in parallel"
                .to_owned(),
            notes,
            helps,
            suggestion,
        })
    }

    fn serial_chain(
        &self,
        id: hir_def::hir::ExprId,
        receiver: hir_def::hir::ExprId,
        dependence: Dependence,
        work: u64,
    ) -> Option<Finding> {
        let head = Head::of(self.db(), self.analysis.body(), self.analysis.infer(), receiver);
        let work_note = self.work_note(work, head.trips)?;
        Some(Finding {
            lint: Lint::SerialDependence,
            range: self.range(id)?,
            message: format!(
                "this iterator chain would run in parallel but for one dependence: {}",
                dependence.said()
            ),
            notes: vec![work_note],
            helps: vec![dependence.help()],
            suggestion: None,
        })
    }
}