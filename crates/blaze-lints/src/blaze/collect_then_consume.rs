//! `collect_then_consume`: an `Iterator::collect` into a `Vec`, `VecDeque`, `String` or a hash
//! or B-tree collection whose value is used once, by something the iterator could answer
//! itself: a `for` loop or an `.iter()`/`.into_iter()` chain, `.len()`, `.is_empty()`,
//! `.first()`, `.contains(..)`, or being passed to a function whose parameter is a type
//! parameter bounded by `IntoIterator`. The collection is allocated, filled and dropped for
//! nothing: `.count()`, `.next().is_none()`, `.next()`, `.any(..)`, or iterating directly
//! answers the same.
//!
//! The value is the `collect` itself (`it.collect::<Vec<_>>().len()`) or a local it is bound
//! to by a plain `let v = ..` (not `mut`, not `ref`), named once in the body. What the use asks
//! must be the same of the iterator as of the collection, so a set or map is found only for
//! `.is_empty()` (and a set for `.contains(..)`): its `len` counts distinct keys and its order
//! is its own; a `String` only for `.is_empty()` of `char`s. Left alone, as the collection may
//! be what makes the code right:
//!
//! - a use inside a loop or closure the `let` is not in (it runs more than once);
//! - a delayed use (a local's, or a loop's or callee's iteration) when a local the iterator
//!   reads is named again after the `collect`: collecting first ends the iterator's borrow,
//!   or takes its items before they change (`let keys: Vec<_> = map.keys().collect();` then
//!   `map.remove(..)` in the loop).
//!
//! The fix, machine-applicable, rewrites `.collect::<Vec<_>>().len()` to `.count()` and
//! `.collect::<Vec<_>>().is_empty()` to `.next().is_none()`, written in the file, when the
//! chain is plain: std's `iter`/`into_iter`/`chars`/... at its root and `Iterator`'s adaptors
//! (`map`, `filter`, `chain`, ...) above it. `.count()` runs every adaptor's closure once per
//! item, as `collect` did; `.next().is_none()` stops at the first item, so it is offered only
//! when the adaptors' closures call nothing and assign nothing. Every other finding gets help.

use base_db::Db;
use hir_def::{
    DefWithBodyId, FunctionId, ItemContainerId, Lookup as _, TraitId,
    expr_store::Body,
    hir::{BinaryOp, BindingAnnotation, BindingId, Expr, ExprId, LoopSource, Pat, Statement},
    signatures::FunctionSignature,
};
use syntax::{NodeOrToken, TextRange};

use super::{
    Lint,
    alloc::written_callee,
    exempt,
    facts::{self, Parents},
    hash_order_output::local_of,
};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::CollectThenConsume;

/// How many methods a chain is followed down to its root.
const CHAIN: usize = 16;
/// How many expressions up a use is followed to the block of its `let`.
const UP: usize = 64;

/// `Iterator`'s adaptors a plain chain may have: each yields from the items of the iterator
/// below, calling only the closures it is given.
const ADAPTORS: &[&str] = &[
    "map",
    "filter",
    "filter_map",
    "chain",
    "cloned",
    "copied",
    "enumerate",
    "zip",
    "rev",
    "skip",
    "take",
    "step_by",
    "skip_while",
    "take_while",
    "map_while",
    "flat_map",
    "flatten",
];

/// std's methods a plain chain may start from: each iterates what it is called on.
const ROOTS: &[&str] = &[
    "iter",
    "into_iter",
    "chars",
    "char_indices",
    "bytes",
    "keys",
    "values",
    "lines",
    "split_whitespace",
    "windows",
    "chunks",
];

/// What a collection is made of, for which uses ask the same of the iterator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Made {
    /// `Vec`, `VecDeque`: every item, in order.
    Seq,
    /// `String`.
    Text,
    /// A hash or B-tree set: the distinct items.
    Set,
    /// A hash or B-tree map: the distinct keys.
    Map,
}

/// How the collected value is used, once.
#[derive(Clone, PartialEq, Eq)]
enum Use {
    /// `.len()`, `.is_empty()`, `.first()`, `.contains(..)`: the call.
    Ask(&'static str, ExprId),
    /// `.iter()`, `.into_iter()`, a `for` loop over it.
    Iterate,
    /// Handed to the function named, whose parameter is `impl IntoIterator`.
    Passed(String),
}

/// A finding, before it is reported.
struct Found {
    range: TextRange,
    message: String,
    note: String,
    fix: Option<(TextRange, String)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    let root = cx.root();
    for it in found {
        let at = match root.text_range().contains_range(it.range) {
            true => match root.covering_element(it.range) {
                NodeOrToken::Node(node) => node,
                NodeOrToken::Token(token) => token.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(it.range);
        let fix = it.fix.map(|(at, text)| (cx.span(at), text));
        let diagnostic = cx.blaze(LINT, at, span, it.message);
        diagnostic.note(it.note);
        if let Some(part) = fix {
            // alloc: the fix's one part
            diagnostic.multipart_suggestion(
                "ask the iterator",
                vec![part],
                Applicability::MachineApplicable,
            );
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// What a body's walk knows.
struct Walk<'a, 'db> {
    db: &'db Db,
    def: DefWithBodyId,
    body: &'db Body,
    infer: &'db hir_ty::InferenceResult<'db>,
    parents: &'a Parents,
    iterator: TraitId,
    into_iterator: Option<hir::Trait>,
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let famous = cx.famous();
    let Some(iterator) = famous.core_iter_Iterator() else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, .. } = expr else { continue };
        let Some((callee, _)) = written_callee(infer, body, id) else { continue };
        if FunctionSignature::of(db, callee).name.as_str() != "collect"
            || !in_trait(db, callee, TraitId::from(iterator))
        {
            continue;
        }
        let ty = analysis.ty(infer.expr_ty(id));
        let Some(made) = made(cx, &ty) else { continue };
        let parents = parents.get_or_insert_with(|| Parents::of(body));
        let walk = Walk {
            db,
            def,
            body,
            infer,
            parents,
            iterator: TraitId::from(iterator),
            into_iterator: famous.core_iter_IntoIterator(),
        };
        let Some((use_, inline)) = walk.the_use(id) else { continue };
        let chars = || {
            let item = cx.assoc_ty(&analysis.ty(infer.expr_ty(receiver)), iterator, "Item");
            item.is_some_and(|it| it.is_char())
        };
        let same = match (&use_, made) {
            (_, Made::Seq) => true,
            (Use::Ask("is_empty", _), Made::Text) => chars(),
            (Use::Ask("is_empty", _), Made::Set | Made::Map) => true,
            (Use::Ask("contains", _), Made::Set) => true,
            _ => false,
        };
        if !same {
            continue;
        }
        let immediate = inline && matches!(use_, Use::Ask(..));
        let Some(collect_range) = analysis.expr_range(id) else { continue };
        if !immediate && walk.read_again(receiver, collect_range, analysis) {
            continue;
        }
        if exempt::exempt(cx.text(), collect_range, LINT) {
            continue;
        }
        let collection = ty
            .as_adt()
            .map_or_else(|| "collection".to_owned(), |it| it.name(db).as_str().to_owned());
        let (asked, instead) = match &use_ {
            Use::Ask("len", _) => ("for its `.len()`".to_owned(), "`.count()` counts the items"),
            Use::Ask("is_empty", _) => {
                ("for its `.is_empty()`".to_owned(), "`.next().is_none()` asks for one item")
            }
            Use::Ask("first", _) => {
                ("for its `.first()`".to_owned(), "`.next()` is the first item")
            }
            Use::Ask(name, _) => {
                (format!("for its `.{name}(..)`"), "`.any(|it| ..)` stops at the first match")
            }
            Use::Iterate => ("to be iterated once".to_owned(), "iterate the iterator itself"),
            Use::Passed(f) => (
                format!("to be handed to `{f}`, which takes `impl IntoIterator`"),
                "hand it the iterator itself",
            ),
        };
        let message = format!("this `collect` into a `{collection}` is only used {asked}");
        let note = format!(
            "the `{collection}` is allocated, filled and dropped for one answer the iterator gives: {instead}, \
             with no collection"
        );
        let fix = match (&use_, made, inline) {
            // `.count()` runs every closure once per item, as `collect` did; `.next()` stops at
            // the first item, so its closures must have nothing to do past it.
            (Use::Ask(name @ ("len" | "is_empty"), call), Made::Seq, true)
                if cx.in_file() && walk.plain(receiver, *name == "len", 0) =>
            {
                fix(analysis, receiver, *call, name).map(|(range, receiver, suffix)| {
                    (range, format!("{}{suffix}", cx.snippet(receiver)))
                })
            }
            _ => None,
        };
        let range = match &use_ {
            Use::Ask(_, call) if inline => analysis.expr_range(*call).unwrap_or(collect_range),
            _ => collect_range,
        };
        // alloc: reported at the edge
        found.push(Found { range, message, note, fix });
    }
    found
}

/// What the collection `ty` is, if one this lint knows.
fn made(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<Made> {
    let db = cx.db();
    let adt = ty.as_adt()?;
    let item =
        |name: &str| cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern(name)) == Some(adt);
    if item("Vec") || item("VecDeque") {
        return Some(Made::Seq);
    }
    if item("String") {
        return Some(Made::Text);
    }
    let set = adt.name(db).as_str().ends_with("Set");
    if facts::hash_collection(cx, ty) || item("BTreeMap") || item("BTreeSet") {
        return Some(if set { Made::Set } else { Made::Map });
    }
    None
}

/// Whether `f` is a function of the trait `trait_`'s own (a provided or required method).
fn in_trait(db: &Db, f: FunctionId, trait_: TraitId) -> bool {
    matches!(f.lookup(db).container, ItemContainerId::TraitId(it) if it == trait_)
}

/// Where the fix of `call`, `.len()` or `.is_empty()` of the `collect` of `receiver`, goes:
/// the call's range, the receiver's, and what follows the receiver's text in the call's place.
/// `None` unless both are written in the file.
fn fix(
    analysis: &hir::BodyAnalysis<'_>,
    receiver: ExprId,
    call: ExprId,
    name: &str,
) -> Option<(TextRange, TextRange, &'static str)> {
    let file = analysis.tree().1;
    let source_map = analysis.source_map();
    let written = |it: ExprId| source_map.expr_syntax(it).is_ok_and(|it| it.file_id == file);
    if !written(receiver) || !written(call) {
        return None;
    }
    let suffix = match name {
        "len" => ".count()",
        _ => ".next().is_none()",
    };
    Some((analysis.expr_range(call)?, analysis.expr_range(receiver)?, suffix))
}

impl Walk<'_, '_> {
    fn name(&self, call: ExprId) -> Option<&str> {
        let (f, _) = written_callee(self.infer, self.body, call)?;
        Some(FunctionSignature::of(self.db, f).name.as_str())
    }

    /// The one use of the value of `collect`, and whether it is used inline (not through a
    /// local).
    fn the_use(&self, collect: ExprId) -> Option<(Use, bool)> {
        let parent = self.parents.of_expr(collect)?;
        if let Expr::Block { statements, .. } = self.body.expr(parent) {
            let pat = statements.iter().find_map(|it| match it {
                Statement::Let { pat, initializer: Some(init), else_branch: None, .. }
                    if init == collect =>
                {
                    Some(pat)
                }
                _ => None,
            })?;
            let Pat::Bind { id: local, subpat: None } = self.body.pat(pat) else { return None };
            if !matches!(self.body[local].mode, BindingAnnotation::Unannotated) {
                return None;
            }
            let uses = self.uses(local);
            let &[only] = uses.as_slice() else { return None };
            if !self.runs_once(only, parent) {
                return None;
            }
            return Some((self.use_at(only)?, false));
        }
        Some((self.use_at(collect)?, true))
    }

    /// The path expressions of the body that name `local`.
    fn uses(&self, local: BindingId) -> Vec<ExprId> {
        // alloc: the uses of one collected local, a few
        let mut uses = Vec::new();
        for (id, expr) in self.body.exprs() {
            if matches!(expr, Expr::Path(_))
                && local_of(self.db, self.def, self.body, id) == Some(local)
            {
                uses.push(id);
            }
        }
        uses
    }

    /// Whether `expr` runs once for each run of `block`: no loop or closure between them.
    fn runs_once(&self, expr: ExprId, block: ExprId) -> bool {
        for it in self.parents.up(expr, UP) {
            if it == block {
                return true;
            }
            if matches!(self.body.expr(it), Expr::Loop { .. } | Expr::Closure { .. }) {
                return false;
            }
        }
        false
    }

    /// How the value at `at` is used, when it is one of [`Use`]'s.
    fn use_at(&self, at: ExprId) -> Option<Use> {
        let mut child = at;
        let mut parent = self.parents.of_expr(child)?;
        if let Expr::Ref { expr, .. } = self.body.expr(parent)
            && expr == child
        {
            child = parent;
            parent = self.parents.of_expr(child)?;
        }
        match self.body.expr(parent) {
            Expr::MethodCall { receiver, .. } if receiver == child => match self.name(parent)? {
                "len" => Some(Use::Ask("len", parent)),
                "is_empty" => Some(Use::Ask("is_empty", parent)),
                "first" => Some(Use::Ask("first", parent)),
                "contains" => Some(Use::Ask("contains", parent)),
                "iter" | "into_iter" => Some(Use::Iterate),
                _ => None,
            },
            Expr::Call { args, .. } => {
                let at = args.iter().position(|&it| it == child)?;
                let (f, _) = written_callee(self.infer, self.body, parent)?;
                let name = FunctionSignature::of(self.db, f).name.as_str();
                if name == "into_iter" && self.for_loop(parent) {
                    return Some(Use::Iterate);
                }
                self.takes_into_iterator(f, at).then(|| Use::Passed(name.to_owned())) // alloc: a message's part
            }
            Expr::MethodCall { args, .. } => {
                let at = args.iter().position(|&it| it == child)?;
                let (f, _) = written_callee(self.infer, self.body, parent)?;
                let params = hir::Function::from(f).params_without_self(self.db);
                let bounded = params.get(at).is_some_and(|it| self.into_iterator_param(it.ty()));
                // alloc: a message's part
                bounded.then(|| {
                    Use::Passed(FunctionSignature::of(self.db, f).name.as_str().to_owned())
                })
            }
            _ => None,
        }
    }

    /// Whether the call `id` of `IntoIterator::into_iter` is a `for` loop's.
    fn for_loop(&self, id: ExprId) -> bool {
        let Some(parent) = self.parents.of_expr(id) else { return false };
        let Expr::Match { arms: [arm], .. } = self.body.expr(parent) else { return false };
        matches!(self.body.expr(arm.expr), Expr::Loop { source: LoopSource::ForLoop, .. })
    }

    /// Whether the `at`th argument of a call of `f` (written as a path, `self` counted) is a
    /// type parameter bounded by `IntoIterator`.
    fn takes_into_iterator(&self, f: FunctionId, at: usize) -> bool {
        let params = hir::Function::from(f).assoc_fn_params(self.db);
        params.get(at).is_some_and(|it| self.into_iterator_param(it.ty()))
    }

    fn into_iterator_param(&self, ty: &hir::Type<'_>) -> bool {
        let Some(into_iterator) = self.into_iterator else { return false };
        ty.as_type_param(self.db)
            .is_some_and(|it| it.trait_bounds(self.db).contains(&into_iterator))
    }

    /// Whether a local the chain `receiver` reads is named again after the `collect` at
    /// `collected`, outside it.
    fn read_again(
        &self,
        receiver: ExprId,
        collected: TextRange,
        analysis: &hir::BodyAnalysis<'_>,
    ) -> bool {
        // alloc: the locals of one chain, a few
        let mut read = Vec::new();
        let mut stack = vec![receiver]; // alloc: the expressions of one chain, walked once
        while let Some(at) = stack.pop() {
            self.body.walk_child_exprs(at, |it| stack.push(it));
            if let Some(local) = local_of(self.db, self.def, self.body, at) {
                read.push(local);
            }
        }
        if read.is_empty() {
            return false;
        }
        self.body.exprs().any(|(id, expr)| {
            matches!(expr, Expr::Path(_))
                && local_of(self.db, self.def, self.body, id).is_some_and(|it| read.contains(&it))
                && analysis.expr_range(id).is_some_and(|it| {
                    !collected.contains_range(it) && it.start() > collected.start()
                })
        })
    }

    /// Whether the chain at `at` is plain: `Iterator`'s [`ADAPTORS`] down to one of std's
    /// [`ROOTS`]. Unless `any_closure`, the adaptors' closures call and assign nothing.
    fn plain(&self, at: ExprId, any_closure: bool, depth: usize) -> bool {
        let mut at = at;
        for _ in depth..CHAIN {
            let Expr::MethodCall { receiver, args, .. } = self.body.expr(at) else { return false };
            let Some((f, _)) = written_callee(self.infer, self.body, at) else { return false };
            let name = FunctionSignature::of(self.db, f).name.as_str();
            if in_trait(self.db, f, self.iterator) && ADAPTORS.contains(&name) {
                let args_plain = args.iter().all(|&arg| match self.body.expr(arg) {
                    Expr::Closure { body, .. } => any_closure || self.calls_nothing(body),
                    Expr::Literal(_) => true,
                    // `chain(other)`, `zip(other)`: the other iterator is a plain chain too.
                    _ => self.plain(arg, any_closure, depth + 1),
                });
                if !args_plain {
                    return false;
                }
                at = receiver;
                continue;
            }
            let module = hir::Function::from(f).module(self.db);
            return ROOTS.contains(&name)
                && ["core", "alloc", "std"]
                    .iter()
                    .any(|it| facts::crate_is(self.db, module.krate(self.db), it));
        }
        false
    }

    /// Whether the closure body `body` calls nothing, assigns nothing and suspends nowhere.
    fn calls_nothing(&self, body: ExprId) -> bool {
        let mut stack = vec![body]; // alloc: the expressions of one closure, walked once
        while let Some(at) = stack.pop() {
            match self.body.expr(at) {
                Expr::Call { .. }
                | Expr::MethodCall { .. }
                | Expr::Assignment { .. }
                | Expr::Await { .. }
                | Expr::Yield { .. }
                | Expr::InlineAsm(_)
                | Expr::BinaryOp { op: Some(BinaryOp::Assignment { .. }), .. } => return false,
                _ => {}
            }
            self.body.walk_child_exprs(at, |it| stack.push(it));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::perf_tests::perf_only;

    const LINT: &str = "collect_then_consume";
    const FOUND: &str = "is only used";

    #[test]
    fn a_collect_only_counted_or_asked_if_empty_is_fixed_to_ask_the_iterator() {
        let found = perf_only(
            LINT,
            r#"
pub fn count(xs: &[u32]) -> usize {
    xs.iter().map(|x| x + 1).collect::<Vec<_>>().len()
}
pub fn none(xs: &[u32]) -> bool {
    xs.iter().filter(|x| **x > 2).collect::<Vec<_>>().is_empty()
}
pub fn called(xs: &[u32]) -> bool {
    xs.iter().filter(|x| keep(**x)).collect::<Vec<_>>().is_empty()
}
fn keep(x: u32) -> bool { x > 2 }
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 3, "{found}");
        assert!(found.contains("only used for its `.len()`"), "{found}");
        assert!(found.contains("xs.iter().map(|x| x + 1).count()"), "the `len` is fixed:\n{found}");
        assert!(
            found.contains("xs.iter().filter(|x| **x > 2).next().is_none()"),
            "the `is_empty` is fixed:\n{found}"
        );
        assert!(
            !found.contains("keep(**x)).next().is_none()"),
            "a closure that calls is not fixed to stop at the first item:\n{found}"
        );
    }

    #[test]
    fn a_local_used_once_by_a_loop_is_found_and_one_used_twice_or_read_again_is_not() {
        let found = perf_only(
            LINT,
            r#"
pub fn looped(xs: &[u32]) -> u32 {
    let doubled: Vec<u32> = xs.iter().map(|x| x * 2).collect();
    let mut sum = 0;
    for x in doubled {
        sum += x;
    }
    sum
}
pub fn twice(xs: &[u32]) -> usize {
    let kept: Vec<_> = xs.iter().collect();
    kept.len() + kept.len()
}
pub fn reread(xs: &mut Vec<u32>) {
    let seen: Vec<u32> = xs.iter().map(|x| *x).collect();
    for x in seen {
        xs.push(x);
    }
}
pub fn in_a_loop(xs: &[u32], n: u32) -> u32 {
    let kept: Vec<_> = xs.iter().collect();
    let mut hits = 0;
    for _ in 0..n {
        hits += kept.len() as u32;
    }
    hits
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 1, "{found}");
        assert!(
            found.contains("this `collect` into a `Vec` is only used to be iterated once"),
            "{found}"
        );
    }

    #[test]
    fn a_collect_kept_mut_or_used_for_more_is_left_alone() {
        let found = perf_only(
            LINT,
            r#"
pub fn kept(xs: &[u32]) -> Vec<u32> {
    xs.iter().map(|x| *x).collect()
}
pub fn grown(xs: &[u32]) -> usize {
    let mut all: Vec<u32> = xs.iter().map(|x| *x).collect();
    all.push(1);
    all.len()
}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 0, "{found}");
    }
}
