//! Test strength: which parts of what a function hands back no test looks at, said without
//! running anything; the static counterpart of mutation testing (a mutant that changes only a
//! part no test observes survives every test).
//!
//! The analysis. For every body of test code (a `#[test]` and each function of test code, with
//! its closures), [`lower`] turns the MIR into [`flow`]'s IR and [`flow::run`] follows each value
//! a call of the code under test hands back (what it returns, and what it writes through a
//! `&mut` argument) to the checks that can fail the test: an `assert!`, the arm of a `match`
//! that panics, an `unwrap`, a `?` out of a test that returns a `Result`. What reaches a check is
//! *observed*, by part: `Config.port`, the variant of a `Result`, the length of a `Vec`. Calls
//! of the workspace's own functions are followed through summaries ([`summary`]) for
//! [`lower::ROOT_DEPTH`] levels: a helper that asserts is followed, a hand-written `PartialEq`
//! that skips a field leaves it unobserved, a derived one reads every field. [`strength_index`]
//! merges by function the observations of every test that can see a crate (its own unit tests
//! and the workspace's integration tests that depend on it) and walks the call graph from the
//! tests for what they reach; a function's lints read only its own [`Answer`] ([`strength`]),
//! so an edit elsewhere that leaves the answer as it was leaves its lints as they were.
//!
//! Every approximation leans one way: what the analysis can't follow (a dependency's function, a
//! closure handed to std, a workspace function past the depth, a body over a bound) observes all
//! it is handed, so a part is reported unobserved only where no path the analysis keeps reaches
//! a check. A value only printed (`{:?}` into `println!`) is not observed: printing can't fail a
//! test.
//!
//! The lints: `result_part_unobserved` (a field of the result no test looks at),
//! `weak_assertion` (a test checks only `is_ok()`, `len()` or `contains` of a value with more
//! in it), `mirror_assertion` (both sides of an assertion are computed by the code under test),
//! `pseudo_tested` (called by tests, nothing it returns is looked at), `unreached_by_tests` (no
//! test reaches it), `error_path_untested` (no test checks which error it returns) and
//! `property_over_examples` (it has an inverse and no property test checks the round trip).

mod flow;
mod lower;
mod parts;
#[cfg(test)]
mod prop;
#[cfg(test)]
mod tests;

use std::alloc::Global;

use base_db::{Db, TargetKind};
use blaze_hash::{HashMap, HashSet};
use hir::{AsAssocItem as _, HasVisibility as _, HirDisplay as _};
use hir_def::{
    DefWithBodyId, FunctionId, VariantId,
    expr_store::{Body, scope::ExprScopes},
    hir::{Expr, ExprId},
    resolver::{ValueNs, resolver_for_scope},
    signatures::FunctionSignature,
};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{AggregateKind, MirBody, Rvalue, StatementKind, TerminatorKind},
};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use self::{
    flow::{Out, Path, Src, Step},
    lower::{CLOSURES, Lower, Mode, ROOT_DEPTH},
    parts::Seen,
};
use super::{
    Finding, Lint, facts, frozen_collection,
    sans_io_core::{fn_name, root_path},
    test_code,
};
use crate::check::Cx;

/// How many tests a finding names before it says how many more.
const SHOWN: usize = 3;

/// How many places a finding of `error_path_untested` names where the function returns an error.
const ERR_SITES: usize = 4;

/// The longest snippet a finding quotes.
const QUOTE: usize = 48;

/// How deep [`Same::expr`] compares two expressions before it calls them different.
const SAME_DEPTH: u32 = 8;

/// The lints read at a test's body.
const TEST_SIDE: [Lint; 2] = [Lint::WeakAssertion, Lint::MirrorAssertion];

/// The lints read at the body of a function tests call.
const CODE_SIDE: [Lint; 5] = [
    Lint::ResultPartUnobserved,
    Lint::PseudoTested,
    Lint::UnreachedByTests,
    Lint::ErrorPathUntested,
    Lint::PropertyOverExamples,
];

/// Names that come in inverse pairs: the first of each is where `property_over_examples`
/// reports. A pair also matches as a prefix or suffix word (`encode_header`, `to_bytes`).
const INVERSES: &[(&str, &str)] = &[
    ("encode", "decode"),
    ("serialize", "deserialize"),
    ("compress", "decompress"),
    ("encrypt", "decrypt"),
    ("pack", "unpack"),
    ("escape", "unescape"),
    ("print", "parse"),
    ("render", "parse"),
    ("unparse", "parse"),
    ("to", "from"),
];

/// What the body of the workspace function `f` does with its parameters, followed `depth`
/// levels into the workspace functions it calls.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn summary(db: &Db, f: FunctionId, depth: u32) -> flow::Summary {
    let def = DefWithBodyId::FunctionId(f);
    let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else {
        return flow::Summary::opaque();
    };
    let mut lower = Lower::new(db, def, Mode::Summary, depth);
    let body = lower.body(mir, false, true);
    flow::Summary::of(&flow::run(&body), &lower.mut_params(mir))
}

/// What a body calls and names, for the walk of what the tests reach.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn callees(db: &Db, def: DefWithBodyId) -> lower::Callees {
    lower::callees_of(db, def)
}

/// One call of the code under test in a test, and what the test observes of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Called {
    pub(crate) f: FunctionId,
    /// The function's path from its crate root: one key across the crates built from one
    /// source (a library and its `cfg(test)` build).
    pub(crate) key: String,
    pub(crate) at: Option<ExprId>,
    /// What the test observes of its outputs, sorted.
    pub(crate) seen: Vec<(Out, Path)>,
    /// The test passed a bound of the analysis: everything of the call counts as observed.
    pub(crate) all: bool,
    /// The shadow calls (`is_ok`, `len`) the observations came through, sorted.
    pub(crate) shadows: Vec<String>,
}

/// An asserted comparison whose two sides are both computed by the code under test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mirror {
    /// The comparison.
    pub(crate) at: Option<ExprId>,
    /// The two calls compared.
    pub(crate) sides: [Option<ExprId>; 2],
    /// The function whose call is on one side, and the one it runs that is on the other (the
    /// same one when both sides call it).
    pub(crate) runner: FunctionId,
    pub(crate) ran: FunctionId,
}

/// What one body of test code observes of the code under test.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reads {
    /// Its MIR, or a closure's, could not be built: what it does is not known.
    pub(crate) unread: bool,
    pub(crate) calls: Vec<Called>,
    pub(crate) mirrors: Vec<Mirror>,
    /// It calls a property-testing crate (`blaze_sim`, or Hegel, proptest, quickcheck in other
    /// people's projects).
    pub(crate) property: bool,
    /// The workspace functions outside test code it calls or names.
    pub(crate) reached: Vec<FunctionId>,
}

/// [`Reads`] of the body `def` of test code, once per body and edit.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn test_reads(db: &Db, def: DefWithBodyId) -> Reads {
    let mut out = Reads::default();
    let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(def)) else {
        out.unread = true;
        return out;
    };
    // A `#[test]` that returns a `Result` fails on the error a `?` returns.
    let residual_fails = match def {
        DefWithBodyId::FunctionId(f) => {
            let f = hir::Function::from(f);
            f.is_test(db) && !f.ret_type(db).is_unit()
        }
        DefWithBodyId::StaticId(_) | DefWithBodyId::ConstId(_) | DefWithBodyId::VariantId(_) => {
            false
        }
    };
    
    let mut bodies: Vec<&MirBody<'_>> = vec![mir];
    let mut next = 0;
    while next < bodies.len() {
        if bodies.len() >= CLOSURES {
            out.unread = true;
            return out;
        }
        for &closure in &bodies[next].closures {
            match db.mir_body_for_closure(closure) {
                Ok(it) => bodies.push(it),
                Err(_) => {
                    out.unread = true;
                    return out;
                }
            }
        }
        next += 1;
    }
    let mut lower = Lower::new(db, def, Mode::Root { residual_fails }, ROOT_DEPTH);
    
    let mut outcomes = Vec::new();
    for (at, mir) in bodies.iter().enumerate() {
        if at == 1 {
            lower.enter_closure();
        }
        // What a test returns goes to the harness (which checks a `Result`), and what a helper
        // or a closure returns goes to code that may check it: observed whole.
        let body = lower.body(mir, true, false);
        outcomes.push(flow::run(&body));
    }
    let all = outcomes.iter().any(|it| it.saturated);
    let sites = lower.sites.len();
    
    let mut seen: Vec<Vec<(Out, Path)>> = vec![Vec::new(); sites];
    let mut shadows: Vec<Vec<String>> = vec![Vec::new(); sites];
    for outcome in &outcomes {
        for obs in &outcome.obs {
            let Src::Call { site, out: output } = obs.src else { continue };
            let Some(parts) = seen.get_mut(site as usize) else { continue };
            parts.push((output, obs.part.clone())); // clone: a short path
            if let Some(shadow) = obs.how.and_then(|it| lower.shadows.get(it as usize)) {
                shadows[site as usize].push(shadow.name.clone()); // clone: a method's name
            }
        }
    }
    for (at, site) in lower.sites.iter().enumerate() {
        let mut parts = std::mem::take(&mut seen[at]);
        parts.sort();
        parts.dedup();
        let mut through = std::mem::take(&mut shadows[at]);
        through.sort();
        through.dedup();
        out.calls.push(Called {
            f: site.f,
            key: root_path(db, site.f),
            at: site.at,
            seen: parts,
            all,
            shadows: through,
        });
    }
    for outcome in &outcomes {
        for compared in &outcome.compares {
            for left in &compared.left {
                for right in &compared.right {
                    let (Src::Call { site: a, .. }, Src::Call { site: b, .. }) = (*left, *right)
                    else {
                        continue;
                    };
                    let (Some(x), Some(y)) = (lower.sites.get(a as usize), lower.sites.get(b as usize))
                    else {
                        continue;
                    };
                    if a == b {
                        continue;
                    }
                    let (runner, ran) = if x.f == y.f || runs(db, x.f, y.f) {
                        (x.f, y.f)
                    } else if runs(db, y.f, x.f) {
                        (y.f, x.f)
                    } else {
                        continue;
                    };
                    let mirror = Mirror {
                        at: lower.marks.get(compared.mark as usize).copied().flatten(),
                        sides: [x.at, y.at],
                        runner,
                        ran,
                    };
                    if !out.mirrors.contains(&mirror) {
                        out.mirrors.push(mirror);
                    }
                }
            }
        }
    }
    out.property = lower.property;
    out.reached = std::mem::take(&mut lower.reached);
    out
}

/// Whether the workspace function `g` calls `f`, itself or through one workspace function.
fn runs(db: &Db, g: FunctionId, f: FunctionId) -> bool {
    let first = &callees(db, DefWithBodyId::FunctionId(g)).fns;
    first.contains(&f)
        || first.iter().any(|&h| {
            in_workspace(db, h) && callees(db, DefWithBodyId::FunctionId(h)).fns.contains(&f)
        })
}

fn in_workspace(db: &Db, f: FunctionId) -> bool {
    hir::Function::from(f).module(db).krate(db).origin(db).is_local()
}

/// Whether `f` may run where no call names it: a method of a trait or of a trait's impl (a
/// `dyn`, a generic bound, `format!` call it unnamed).
fn dispatched(db: &Db, f: FunctionId) -> bool {
    hir::Function::from(f).as_assoc_item(db).is_some_and(|it| {
        matches!(it.container(db), hir::AssocItemContainer::Trait(_))
            || it.implemented_trait(db).is_some()
    })
}

/// The crates whose tests count for `krate`: the crates built from its root file (itself and
/// its `cfg(test)` build) and the workspace's test targets that depend on one of them.
fn scope(db: &Db, krate: base_db::Crate) -> Vec<base_db::Crate> {
    let root = hir::Crate::from(krate).root_file(db);
    let all = base_db::all_crates(db);
    let local = |it: base_db::Crate| hir::Crate::from(it).origin(db).is_local();
    
    let mut out: Vec<base_db::Crate> = all
        .iter()
        .copied()
        .filter(|&it| local(it) && hir::Crate::from(it).root_file(db) == root)
        .collect();
    let twins = out.len();
    for &it in all.iter() {
        if !local(it) || out.contains(&it) {
            continue;
        }
        let test = matches!(hir::Crate::from(it).target_kind(db), TargetKind::Test);
        if test && it.transitive_deps(db).iter().any(|dep| out[..twins].contains(dep)) {
            out.push(it);
        }
    }
    out
}

/// What the tests that can see one function observe of it, and what is true of its crate's
/// tests as a whole: what its lints read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Answer {
    /// The bodies of test code that call it, by name, sorted.
    pub(crate) tests: Vec<String>,
    /// What they observe of its outputs, sorted.
    pub(crate) seen: Vec<(Out, Path)>,
    /// One of them passed a bound of the analysis: all of it counts as observed.
    pub(crate) all: bool,
    /// A test reaches it: calls or names it, or something it reaches does.
    pub(crate) reached: bool,
    /// What the property tests that call it call too, by key, sorted.
    pub(crate) partners: Vec<String>,
    /// The crate's tests include at least one `#[test]`.
    pub(crate) any_tests: bool,
    /// The bodies of test code and reached functions whose MIR could not be built, sorted.
    pub(crate) unread: Vec<String>,
}

/// Every [`Answer`] of a crate, by function key.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Index {
    by_key: HashMap<String, Answer, Global>,
    tests: u32,
    unread: Vec<String>,
}

impl Index {
    fn answer(&mut self, key: &str) -> Option<&mut Answer> {
        if !self.by_key.contains_key(key) {
            self.by_key.insert(key.to_owned(), Answer::default()); 
        }
        self.by_key.get_mut(key)
    }

    /// Adds what the body of test code `f` observes.
    fn test(&mut self, db: &Db, f: FunctionId, reads: &Reads, walk: &mut Walk) {
        let name = fn_name(db, f);
        if reads.unread {
            self.unread.push(name);
            return;
        }
        if hir::Function::from(f).is_test(db) {
            self.tests += 1;
        }
        let partners: &[Called] = if reads.property { &reads.calls } else { &[] };
        for called in &reads.calls {
            let Some(answer) = self.answer(&called.key) else { continue };
            answer.tests.push(name.clone()); // clone: a test's name, per function it calls
            answer.all |= called.all;
            answer.seen.extend(called.seen.iter().cloned()); // clone: short paths
            answer.partners.extend(partners.iter().map(|it| it.key.clone())); // clone: keys
        }
        for &g in &reads.reached {
            walk.push(DefWithBodyId::FunctionId(g));
        }
    }
}

/// The walk of what the tests reach.
struct Walk {
    queue: Vec<DefWithBodyId>,
    queued: HashSet<DefWithBodyId, Global>,
}

impl Walk {
    fn push(&mut self, def: DefWithBodyId) {
        if self.queued.insert(def) {
            self.queue.push(def);
        }
    }
}

/// What every test that can see `krate` observes, merged by function, and what they reach;
/// once per crate and edit.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn strength_index(db: &Db, krate: base_db::Crate) -> Index {
    let mut index = Index::default();
    
    let mut walk = Walk { queue: Vec::new(), queued: HashSet::new_in(Global) };
    for it in scope(db, krate) {
        for def in frozen_collection::crate_bodies(db, it) {
            let test = test_code::is_test_def(db, def);
            match def {
                DefWithBodyId::FunctionId(f) if test => index.test(db, f, test_reads(db, def), &mut walk),
                // Run where no call names them: what they call is reached whenever they are.
                DefWithBodyId::FunctionId(f) if dispatched(db, f) => walk.push(def),
                DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) if !test => walk.push(def),
                DefWithBodyId::FunctionId(_)
                | DefWithBodyId::ConstId(_)
                | DefWithBodyId::StaticId(_)
                | DefWithBodyId::VariantId(_) => {}
            }
        }
    }
    let mut next = 0;
    // Bounded: each body is queued once, and there are finitely many.
    while let Some(&def) = walk.queue.get(next) {
        next += 1;
        if let DefWithBodyId::FunctionId(f) = def
            && let Some(answer) = index.answer(&root_path(db, f))
        {
            answer.reached = true;
        }
        let found = callees(db, def);
        if found.unread
            && let DefWithBodyId::FunctionId(f) = def
        {
            index.unread.push(fn_name(db, f));
        }
        for &g in &found.fns {
            let body = DefWithBodyId::FunctionId(g);
            if in_workspace(db, g) && !test_code::is_test_def(db, body) {
                walk.push(body);
            }
        }
    }
    index.unread.sort();
    index.unread.dedup();
    for answer in index.by_key.values_mut() {
        answer.tests.sort();
        answer.tests.dedup();
        answer.seen.sort();
        answer.seen.dedup();
        answer.partners.sort();
        answer.partners.dedup();
    }
    index
}

/// One function of one crate: the key of [`strength`].
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct StrengthKey {
    #[returns(copy)]
    pub(crate) krate: base_db::Crate,
    #[returns(copy)]
    pub(crate) f: FunctionId,
}

/// The [`Answer`] of one function: what its lints read, so an edit elsewhere that leaves it as
/// it was leaves them as they were.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn strength(db: &Db, key: StrengthKey) -> Answer {
    let index = strength_index(db, key.krate(db));
    let mut answer = index.by_key.get(&root_path(db, key.f(db))).cloned().unwrap_or_default(); // clone: one function's answer
    answer.any_tests = index.tests > 0;
    answer.unread = index.unread.clone(); // clone: a few names
    answer
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    
    let mut out = Vec::new();
    let Some(f) = ast::Fn::cast(cx.root()) else { return out };
    if f.body().is_none() || cx.expansion().is_some() {
        return out;
    }
    let Some(analysis) = cx.analysis() else { return out };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return out };
    let DefWithBodyId::FunctionId(id) = def else { return out };
    if test_code::is_test_code(cx, f.syntax()) {
        if TEST_SIDE.iter().any(|&it| cx.on_blaze(it)) {
            test_side(cx, analysis, def, &mut out);
        }
        return out;
    }
    if !CODE_SIDE.iter().any(|&it| cx.on_blaze(it)) {
        return out;
    }
    let Some(name) = f.name() else { return out };
    let Some(at) = cx.range(name.syntax()) else { return out };
    let code = Code { cx, db: cx.db(), f: id, at, name: name.text().to_string() };
    code.check(analysis, &mut out);
    out
}

/// Whether two expressions of one test body compute the same value the same way, for
/// `mirror_assertion`'s calls of one function on both sides.
struct Same<'a> {
    db: &'a Db,
    def: DefWithBodyId,
    body: &'a Body,
}

impl Same<'_> {
    /// Equal literals, paths that resolve to one item or one local binding, and calls,
    /// method calls, operators, fields, borrows and tuples of such. Anything else, or deeper
    /// than `depth`, is different, so a pair is called a mirror only when it surely is one.
    fn expr(&self, a: ExprId, b: ExprId, depth: u32) -> bool {
        let Some(depth) = depth.checked_sub(1) else { return false };
        let all = |x: &[ExprId], y: &[ExprId]| {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| self.expr(*x, *y, depth))
        };
        match (self.body.expr(a), self.body.expr(b)) {
            (Expr::Literal(x), Expr::Literal(y)) => x == y,
            (Expr::Path(_), Expr::Path(_)) => {
                self.value_of(a).is_some_and(|x| Some(x) == self.value_of(b))
            }
            (Expr::Call { callee: f, args: x }, Expr::Call { callee: g, args: y }) => {
                self.expr(f, g, depth) && all(x, y)
            }
            (
                Expr::MethodCall { receiver: r, method_name: n, args: x, generic_args: p },
                Expr::MethodCall { receiver: s, method_name: m, args: y, generic_args: q },
            ) => n == m && p == q && self.expr(r, s, depth) && all(x, y),
            (Expr::Field { expr: x, name: n }, Expr::Field { expr: y, name: m }) => {
                n == m && self.expr(x, y, depth)
            }
            (Expr::UnaryOp { expr: x, op: o }, Expr::UnaryOp { expr: y, op: p }) => {
                o == p && self.expr(x, y, depth)
            }
            (
                Expr::BinaryOp { lhs: x, rhs: u, op: o },
                Expr::BinaryOp { lhs: y, rhs: v, op: p },
            ) => o == p && self.expr(x, y, depth) && self.expr(u, v, depth),
            (
                Expr::Ref { expr: x, rawness: r, mutability: m },
                Expr::Ref { expr: y, rawness: s, mutability: n },
            ) => r == s && m == n && self.expr(x, y, depth),
            (Expr::Tuple { exprs: x }, Expr::Tuple { exprs: y }) => all(x, y),
            _ => false,
        }
    }

    /// What the path expression `expr` names, resolved in its scope (so a shadowed local is
    /// a different binding).
    fn value_of(&self, expr: ExprId) -> Option<ValueNs> {
        let Expr::Path(path) = self.body.expr(expr) else { return None };
        let scope = ExprScopes::of(self.db, self.def).scope_for(expr);
        resolver_for_scope(self.db, self.def, scope).resolve_path_in_value_ns_fully(
            self.db,
            path,
            self.body.expr_path_hygiene(expr),
        )
    }
}

/// `text`, cut to [`QUOTE`] characters.
fn quote(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" "); 
    match flat.char_indices().nth(QUOTE) {
        Some((at, _)) => format!("{}..", &flat[..at]),
        None => flat,
    }
}

/// `names` as a finding lists them.
fn listed(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().take(SHOWN).map(|it| format!("`{it}`")).collect(); 
    if names.len() > SHOWN {
        format!("{} and {} more", shown.join(", "), names.len() - SHOWN)
    } else {
        shown.join(", ")
    }
}

/// The lints of one body of test code.
fn test_side(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, def: DefWithBodyId, out: &mut Vec<Finding>) {
    let db = cx.db();
    let reads = test_reads(db, def);
    if cx.on_blaze(Lint::WeakAssertion) {
        for called in &reads.calls {
            let Some(what) = weak(db, called) else { continue };
            let Some(range) = called.at.and_then(|it| analysis.expr_range(it)) else { continue };
            let name = fn_name(db, called.f);
            let message = format!(
                "the test checks only {what} of what `{name}` returns, never the value: a wrong \
                 value that keeps {what} passes"
            );
            let note = format!(
                "compare the whole value with the one expected: `assert_eq!({}, expected)`",
                quote(cx.snippet(range))
            );
            facts::push(out, Lint::WeakAssertion, range, message, Some(note));
        }
    }
    if cx.on_blaze(Lint::MirrorAssertion) {
        for mirror in &reads.mirrors {
            let [Some(left), Some(right)] = mirror.sides.map(|it| it.and_then(|it| analysis.expr_range(it)))
            else {
                continue;
            };
            let runner = fn_name(db, mirror.runner);
            let message = if mirror.runner == mirror.ran {
                // The same function on both sides is a mirror only with the same arguments,
                // compared as expressions: inside `assert_eq!` a side's range is the whole
                // macro call, so its text cannot tell `double(4)` from `double(2)`.
                let [Some(a), Some(b)] = mirror.sides else { continue };
                let same = Same { db, def, body: Body::of(db, def) };
                if !same.expr(a, b, SAME_DEPTH) {
                    continue;
                }
                // A side written in the macro's arguments maps to the whole macro call, so the
                // shorter range is the one that is the call itself; when both map to the one
                // macro call, neither is, and the call is named instead.
                let call = match left == right {
                    true => format!("calls to `{runner}` with the same arguments"),
                    false => {
                        let shorter = match left.len() <= right.len() {
                            true => left,
                            false => right,
                        };
                        format!("`{}`", quote(cx.snippet(shorter)))
                    }
                };
                format!(
                    "both sides of the assertion are {call}: the expected value comes from the \
                     code under test, so the assertion holds whatever `{runner}` does"
                )
            } else {
                format!(
                    "the two sides of the assertion are `{runner}` and `{}`, which `{runner}` \
                     runs: the expected value comes from the code under test, so a bug they share \
                     passes",
                    fn_name(db, mirror.ran)
                )
            };
            let range = mirror.at.and_then(|it| analysis.expr_range(it)).unwrap_or(right);
            let note = "write the expected value out (a literal), or compute it another way (by \
                        hand, or by a reference model that shares no code with it)"
                .to_owned(); 
            facts::push(out, Lint::MirrorAssertion, range, message, Some(note));
        }
    }
}

/// What a test checks of a call when that is only a property of a value with more in it
/// (`` `is_ok()` ``, `` `len()` ``), for `weak_assertion`.
fn weak(db: &Db, called: &Called) -> Option<String> {
    if called.all || called.seen.is_empty() {
        return None;
    }
    let only_property = called.seen.iter().all(|(output, part)| {
        *output == Out::Ret && matches!(part.as_slice(), [Step::Discr] | [Step::Len] | [Step::Probe])
    });
    if !only_property {
        return None;
    }
    let only_variant = called.seen.iter().all(|(_, part)| part.as_slice() == [Step::Discr]);
    if only_variant && !has_payload(db, &hir::Function::from(called.f).ret_type(db)) {
        return None;
    }
    // `assert!(x.is_none())` and `assert!(v.is_empty())` pin the whole value when they hold;
    // the polarity of the check is not followed, so a negated one (`!v.is_empty()`) is missed
    // rather than a pinning one reported.
    if !called.shadows.is_empty() && called.shadows.iter().all(|it| matches!(it.as_str(), "is_none" | "is_empty")) {
        return None;
    }
    Some(match called.shadows.as_slice() {
        [] => "which variant it is".to_owned(), 
        names => names.iter().map(|it| format!("`{it}()`")).collect::<Vec<_>>().join(" and "), 
    })
}

/// Whether a value of `ty` holds more than which variant it is: a `Some` or `Ok` with a
/// payload, or an enum with a variant that has fields.
fn has_payload(db: &Db, ty: &hir::Type<'_>) -> bool {
    let ty = ty.strip_references();
    if let Some((payload, _)) = parts::wrapped(db, &ty) {
        return !payload.is_unit();
    }
    match ty.as_adt() {
        Some(hir::Adt::Enum(it)) => it.variants(db).into_iter().any(|v| !v.fields(db).is_empty()),
        Some(hir::Adt::Struct(_) | hir::Adt::Union(_)) | None => true,
    }
}

/// The lints of one function that tests may call.
struct Code<'a, 'c, 't> {
    cx: &'a Cx<'c, 't>,
    db: &'c Db,
    f: FunctionId,
    /// Its name, where findings are reported.
    at: TextRange,
    name: String,
}

impl Code<'_, '_, '_> {
    fn check(&self, analysis: &hir::BodyAnalysis<'_>, out: &mut Vec<Finding>) {
        let db = self.db;
        let answer = strength(db, StrengthKey::new(db, self.cx.krate().base(), self.f));
        let f = hir::Function::from(self.f);
        let unread = match answer.unread.as_slice() {
            [] => String::new(), 
            names => format!(
                "; the MIR of {} could not be built, so what it does is not counted",
                listed(names)
            ),
        };
        let on = |lint: Lint| self.cx.on_blaze(lint);
        if on(Lint::UnreachedByTests)
            && answer.any_tests
            && !answer.reached
            && matches!(f.visibility(db), hir::Visibility::Public | hir::Visibility::PubCrate(_))
            && !dispatched(db, self.f)
            && !f.is_main(db)
        {
            let message = format!(
                "no test reaches `{}`: none calls it, or calls anything that does",
                self.name
            );
            let note = format!(
                "a bug in it fails no test; call it from a test that checks what it returns{unread}"
            );
            facts::push(out, Lint::UnreachedByTests, self.at, message, Some(note));
        }
        self.property(&answer, out);
        if answer.tests.is_empty() || answer.all {
            return;
        }
        let tests = listed(&answer.tests);
        let ret = f.ret_type(db);
        
        let seen_of = |output: Out| -> Vec<Path> {
            answer.seen.iter().filter(|(it, _)| *it == output).map(|(_, part)| part.clone()).collect() // clone: short paths
        };
        let ret_seen = seen_of(Out::Ret);
        
        let mut_params: Vec<(u32, String, hir::Type<'_>)> = f
            .assoc_fn_params(db)
            .into_iter()
            .enumerate()
            .filter(|(_, it)| it.ty().is_mutable_reference())
            .map(|(at, it)| {
                let name = it.name(db).map_or_else(|| "self".to_owned(), |it| it.as_str().to_owned()); 
                (at as u32, name, it.ty().clone()) // clone: the parameter's type
            })
            .collect();
        if answer.seen.is_empty() {
            if on(Lint::PseudoTested) && (!ret.is_unit() || !mut_params.is_empty()) {
                let message = format!(
                    "`{}` is called by tests ({tests}) but none of them looks at what it returns \
                     or writes: a body that returns anything that doesn't panic passes them",
                    self.name
                );
                let note = match hir_ty_pure::derived_pure(db, self.f) {
                    true => "assert on what it returns: `assert_eq!(f(..), expected)`".to_owned(), 
                    false => format!(
                        "assert on what it returns; if the tests check it another way (a file it \
                         writes, state it changes), say so with `#[expect(blaze::pseudo_tested, \
                         reason = \"..\")]`{unread}"
                    ),
                };
                facts::push(out, Lint::PseudoTested, self.at, message, Some(note));
            }
        } else if on(Lint::ResultPartUnobserved) {
            let mut said = Vec::new(); 
            if !ret_seen.is_empty() {
                let parts = parts::unobserved(db, &ret, &Seen(&ret_seen));
                if !parts.is_empty() {
                    said.push(format!("{} of what it returns", listed(&parts)));
                }
            }
            for (at, param, ty) in &mut_params {
                let seen = seen_of(Out::Arg(*at));
                if seen.is_empty() {
                    continue;
                }
                let parts = parts::unobserved(db, ty, &Seen(&seen));
                if !parts.is_empty() {
                    said.push(format!("{} of what it writes to `{param}`", listed(&parts)));
                }
            }
            if !said.is_empty() {
                let message = format!(
                    "the tests of `{}` ({tests}) never look at {}: a bug there passes them all",
                    self.name,
                    said.join(", nor ")
                );
                let note = format!(
                    "assert on the whole value (`assert_eq!(.., expected)`), or on those parts{unread}"
                );
                facts::push(out, Lint::ResultPartUnobserved, self.at, message, Some(note));
            }
        }
        if on(Lint::ErrorPathUntested) && parts::is_result(db, &ret) && !pins_err(&ret_seen) {
            let sites = self.err_sites(analysis);
            if !sites.is_empty() {
                let checks_failure = ret_seen.iter().any(|it| it.as_slice() == [Step::Discr]);
                let message = match checks_failure {
                    true => format!(
                        "the tests of `{}` ({tests}) check whether it fails, never which error \
                         it returns",
                        self.name
                    ),
                    false => format!(
                        "no test of `{}` ({tests}) looks at an error it returns",
                        self.name
                    ),
                };
                let note = format!(
                    "it returns an error at {}: assert on one (`assert_eq!(f(bad), Err(..))`, \
                     `matches!(f(bad), Err(E::..))`){unread}",
                    listed(&sites)
                );
                facts::push(out, Lint::ErrorPathUntested, self.at, message, Some(note));
            }
        }
    }

    /// Where the body returns an error: an `Err(..)` it makes and each `?`, as written.
    fn err_sites(&self, analysis: &hir::BodyAnalysis<'_>) -> Vec<String> {
        let db = self.db;
        
        let mut out = Vec::new();
        let Ok(mir) = db.mir_body(InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(self.f)))
        else {
            return out;
        };
        let mut push = |range: Option<TextRange>| {
            if let Some(range) = range {
                let quoted = quote(self.cx.snippet(range));
                if out.len() < ERR_SITES && !out.contains(&quoted) {
                    out.push(quoted);
                }
            }
        };
        for (_, block) in mir.basic_blocks.iter() {
            if block.is_cleanup {
                continue;
            }
            for statement in &block.statements {
                if let StatementKind::Assign(_, Rvalue::Aggregate(AggregateKind::Adt(VariantId::EnumVariantId(v), _), _)) =
                    &statement.kind
                    && v.index(db) == 1
                    && facts::adt_is(db, hir::Adt::Enum(hir::Enum::from(v.loc(db).parent)), "core", "Result")
                {
                    push(facts::mir_range(analysis, statement.span));
                }
            }
            if let Some(terminator) = &block.terminator
                && let TerminatorKind::Call { func, .. } = &terminator.kind
                && let Some((g, _)) = func.fn_item()
                && FunctionSignature::of(db, g).name.as_str() == "from_residual"
            {
                push(facts::mir_range(analysis, terminator.span));
            }
        }
        out
    }

    /// `property_over_examples`: the function has an inverse beside it, its tests try examples,
    /// and no property test calls both.
    fn property(&self, answer: &Answer, out: &mut Vec<Finding>) {
        let db = self.db;
        if !self.cx.on_blaze(Lint::PropertyOverExamples) || answer.tests.is_empty() {
            return;
        }
        let Some(name) = inverse_name(FunctionSignature::of(db, self.f).name.as_str()) else {
            return;
        };
        let Some(inverse) = beside(db, self.f, &name) else { return };
        if !inverse_types(db, self.cx.krate(), self.f, inverse) {
            return;
        }
        if answer.partners.contains(&root_path(db, inverse)) {
            return;
        }
        let message = format!(
            "`{}` has an inverse, `{name}`, and its tests ({}) try examples only: no property \
             test checks that `{name}` gives back what `{}` was handed",
            self.name,
            listed(&answer.tests),
            self.name
        );
        let note = format!(
            "add a property test of the round trip over generated inputs (blaze's own form: \
             `blaze_sim::check(Config::cases(200), |tape| {{ let x = tape.any(\"x\"); \
             assert_eq!({name}(&{}(&x)), Ok(x)) }})`): it tries the inputs no example wrote down",
            self.name
        );
        facts::push(out, Lint::PropertyOverExamples, self.at, message, Some(note));
    }
}

/// Whether an observation of a `Result` pins its error: the whole value, or anything inside
/// `Err`.
fn pins_err(seen: &[Path]) -> bool {
    seen.iter().any(|it| matches!(it.as_slice(), [Step::Whole] | [Step::Variant(1), _, ..]))
}

/// The name of the inverse of the function named `name`, when it is the first of an
/// [`INVERSES`] pair, whole or as a word at either end.
fn inverse_name(name: &str) -> Option<String> {
    for &(forward, back) in INVERSES {
        if name == forward {
            return Some(back.to_owned()); 
        }
        if let Some(rest) = name.strip_prefix(forward)
            && rest.starts_with('_')
        {
            return Some(format!("{back}{rest}"));
        }
        if let Some(head) = name.strip_suffix(forward)
            && head.ends_with('_')
        {
            return Some(format!("{head}{back}"));
        }
    }
    None
}

/// The function named `name` beside `f`: in the same impl, or in the same module.
fn beside(db: &Db, f: FunctionId, name: &str) -> Option<FunctionId> {
    let f = hir::Function::from(f);
    let fns: Vec<hir::Function> = match f.as_assoc_item(db).map(|it| it.container(db)) {
        Some(hir::AssocItemContainer::Impl(imp)) => imp
            .items(db)
            .into_iter()
            .filter_map(|it| if let hir::AssocItem::Function(g) = it { Some(g) } else { None })
            .collect(), 
        Some(hir::AssocItemContainer::Trait(_)) => return None,
        None => f
            .module(db)
            .declarations(db)
            .into_iter()
            .filter_map(|it| if let hir::ModuleDef::Function(g) = it { Some(g) } else { None })
            .collect(), 
    };
    let found = fns.into_iter().find(|g| g.name(db).as_str() == name)?;
    FunctionId::try_from(found).ok()
}

/// Whether `back` undoes `forward` by their types: what `forward` takes first is what `back`
/// gives, and what `forward` gives is what `back` takes first (references, `Option` and
/// `Result` seen through; `String` as `str`, `Vec<u8>` as `[u8]`).
fn inverse_types(db: &Db, krate: hir::Crate, forward: FunctionId, back: FunctionId) -> bool {
    let target = krate.to_display_target(db);
    let shown = |ty: &hir::Type<'_>| -> String {
        let mut ty = ty.strip_references();
        if let Some((payload, _)) = parts::wrapped(db, &ty) {
            ty = payload.strip_references();
        }
        let text = ty.display(db, target).to_string();
        match text.as_str() {
            "String" => "str".to_owned(), 
            "Vec<u8>" => "[u8]".to_owned(), 
            _ => text,
        }
    };
    let first = |f: FunctionId| {
        hir::Function::from(f).assoc_fn_params(db).first().map(|it| shown(it.ty()))
    };
    let (Some(takes), Some(back_takes)) = (first(forward), first(back)) else { return false };
    let gives = shown(&hir::Function::from(forward).ret_type(db));
    let back_gives = shown(&hir::Function::from(back).ret_type(db));
    takes == back_gives && gives == back_takes
}
