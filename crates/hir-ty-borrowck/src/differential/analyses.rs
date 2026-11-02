//! The analyses the fixpoint serves, as they were ([`old::moves`], [`old::liveness`],
//! [`old::loans`]) against today's on the same bodies: the borrowck fixtures of `hir`'s and
//! `check`'s tests, the budget's, and generated bodies of thousands of locals and blocks. Both
//! sides run in one [`Ctx`] on one [`Regions`](crate::regions::Regions) and one set of region
//! values, so what they compute must agree exactly: the move paths and every fact of them, the
//! init entry sets, what drops may drop, each region's live points, the loans, what each point
//! does to them and their entry sets, with NLL's scopes and Polonius's. And what borrowck
//! reports for each body: rustc's codes at rustc's primary spans, in rustc's order.

use std::fmt::Write as _;

use base_db::Db;
use blaze_mem::{Tag, frame};
use hir_def::{
    DefWithBodyId, HasModule as _,
    expr_store::{Body, BodySourceMap, ExpressionStore},
    hir::ExprId,
};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::HirDatabase as _,
    mir::{LocalId, MirBody, MirSpan, OperandKind, PlaceElem, StatementKind, TerminatorKind},
    next_solver::DbInterner,
};
use hir_expand::fixture;

use super::old;
use crate::{
    Ctx, Mode, borrowck_in_mode,
    bitset::words_for,
    budget,
    cfg::{Cfg, Location},
    decode::{Decoded, raw},
    liveness, loans,
    mem::{self, FMap, FVec},
    moves, polonius, regions,
    rows::Rows,
    scan::Scan,
};

/// `check`'s tests' bodies with errors.
const CHECK_ERRORS: &str = r#"
//- minicore: sized, copy
struct S;
struct Value {}
fn eat(_: Value) {}
fn read(_: &S) {}
trait Fake { fn use_mut(&mut self) {} fn use_ref(&self) {} }
impl<T> Fake for T {}

fn e0499() {
    let mut i = 0;
    let mut x = &mut i;
    let mut a = &mut i;
    a.use_mut();
    x.use_mut();
}

fn e0502() {
    let mut s = S;
    let a = &s;
    let b = &mut s;
    a.use_ref();
}

fn e0505() {
    let x = Value{};
    {
        let _ref_to_val: &Value = &x;
        eat(x);
        _ref_to_val.use_ref();
    }
}

fn e0596() {
    let x = 1;
    let y = &mut x;
}

fn e0597() {
    let r;
    {
        let s = S;
        r = &s;
    }
    read(r);
}
"#;

/// `check`'s tests' bodies with no error.
const CHECK_CLEAN: &str = r#"
//- minicore: sized, copy, clone, fn, index, slice, add
struct Point { x: u32, y: u32 }
struct Stack { items: [u32; 4], len: usize }
enum Shape { Circle(Point), Square(u32), Empty }
union U { a: u32, b: f32 }
static mut COUNT: u32 = 0;

impl Stack {
    fn len(&self) -> usize { self.len }
    fn top(&self) -> &u32 { &self.items[0] }
    fn push(&mut self, value: u32) {
        self.items[self.len] = value;
        self.len += 1;
    }
}

fn borrows(p: &mut Point) -> u32 {
    let a = &p.x;
    let b = &p.y;
    let sum = *a + *b;
    p.x = sum;
    let c = &mut p.y;
    *c += 1;
    p.x + p.y
}

fn two_phase(s: &mut Stack) {
    s.push(s.len() as u32);
}

fn loops(s: &mut Stack) {
    let mut i = 0;
    while i < 3 {
        let top = s.top();
        s.push(*top);
        i += 1;
    }
}

fn guards(shape: Shape, limit: u32) -> u32 {
    match shape {
        Shape::Circle(point) if point.x > limit => point.y,
        Shape::Circle(point) => point.x,
        Shape::Square(side) if side > limit => side,
        Shape::Square(_) | Shape::Empty => 0,
    }
}

fn indexes(rows: &[&[u32]], grid: &mut [[u32; 4]; 4], i: usize, j: usize) -> u32 {
    grid[i][j] += 1;
    rows[1][i] + grid[j][i]
}

fn unions() -> u32 {
    let mut u = U { a: 1 };
    u.a = 2;
    unsafe { u.a }
}

fn statics() -> u32 {
    unsafe {
        COUNT += 1;
        COUNT
    }
}

fn closures(p: Point) -> u32 {
    let mut n = p.x;
    let mut add = |k: u32| n += k;
    add(1);
    add(2);
    let read = move || p.y;
    n + read()
}

fn opaque<'a>(p: &'a Point) -> impl Copy + 'a {
    &p.x
}

fn moves(shape: Shape) -> Shape {
    let mut held = shape;
    let taken = held;
    held = Shape::Empty;
    match held {
        Shape::Empty => taken,
        other => other,
    }
}
"#;

/// `hir`'s tests' bodies: a move twice, and the body its eviction test checks.
const HIR: &str = r#"
//- minicore: sized, copy
struct S;
fn consume(_: S) {}
fn moved_twice() {
    let s = S;
    consume(s);
    consume(s);
}
fn moved_once() {
    let s = S;
    consume(s);
}
fn mut_unused() {
    let mut s = S;
    consume(s);
}
fn mismatched() -> u32 {
    S
}
fn evicted() -> u32 {
    let s = S;
    consume(s);
    1
}
"#;

/// A function `name` of `segments` segments, each a value of two fields with drop glue, one
/// moved out on each path and put back on one, a borrow of a field and a match: move paths
/// below locals, partial moves and the drops of what is left.
fn paths_function(out: &mut String, name: &str, segments: usize) {
    writeln!(out, "fn {name}(flag: bool, n: u32) -> u32 {{").unwrap();
    writeln!(out, "    let mut total = 0;").unwrap();
    for at in 0..segments {
        writeln!(
            out,
            "    let mut t{at} = Two {{ a: Bag({at}), b: Bag(n) }};
    if flag {{ keep(t{at}.a); t{at}.a = Bag({at}); }} else {{ keep(t{at}.b); }}
    let s{at} = &t{at}.a;
    total += s{at}.0;
    match flag {{ true => keep(t{at}.a), false => total += 1, }}"
        )
        .unwrap();
    }
    writeln!(out, "    total\n}}").unwrap();
}

/// The budget's typical bodies and two generated ones of thousands of locals and blocks.
fn large_fixture() -> String {
    let mut out = String::from(budget::TYPICAL);
    out.push_str("struct Two { a: Bag, b: Bag }\n");
    budget::large_function(&mut out, "large", 400);
    paths_function(&mut out, "paths", 300);
    out
}

/// A borrowck body compared: its owner's name, blocks and locals.
struct Compared {
    name: String,
    blocks: usize,
    locals: usize,
}

/// Compares the analyses on every body of every function of `fixture` that borrowck checks.
fn compare_fixture(fixture: &str) -> Vec<Compared> {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut compared = Vec::new();
        for (name, def) in budget::functions(&db) {
            let checked = borrowck_in_mode(&db, def, Mode::Nll);
            let body = hir_def::expr_store::Body::of(&db, def);
            // A function without a body, as minicore's intrinsics, has no MIR, as in rustc.
            let bodyless = matches!(body.expr(body.root_expr()), hir_def::hir::Expr::Missing);
            if bodyless || checked.skipped.is_some() || checked.not_checked.is_some() {
                continue;
            }
            compare_owner(&db, &name, def, &mut compared);
        }
        compared
    })
}

/// The owner's body and the closures in it, each compared in a task of its own as borrowck
/// checks them, after one scan of the owner's store.
fn compare_owner(db: &Db, name: &str, def: DefWithBodyId, compared: &mut Vec<Compared>) {
    let owner = InferBodyId::DefWithBodyId(def);
    let infer = InferenceResult::of(db, owner);
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    let body = db.mir_body(owner).unwrap_or_else(|error| panic!("`{name}`: {error:?}"));
    let mut bodies: Vec<(&MirBody<'_>, Option<ExprId>)> = vec![(body, None)];
    let mut at = 0;
    while let Some(&(body, _)) = bodies.get(at) {
        at += 1;
        for &closure in body.closures.iter() {
            let lowered = db
                .mir_body_for_closure(closure)
                .unwrap_or_else(|error| panic!("`{name}`, a closure: {error:?}"));
            bodies.push((lowered, Some(closure.loc(db).expr)));
        }
    }
    frame::task(Tag::Mir, |frame| {
        let scan = Scan::new(frame, store, infer, DbInterner::new_with(db, owner.krate(db)));
        for (index, &(body, closure)) in bodies.iter().enumerate() {
            let what = format!("`{name}`, body {index}");
            frame::task(Tag::Mir, |frame| {
                let mode = Mode::Polonius;
                let ctx = Ctx::new(db, owner, store, infer, &scan, body, closure, mode, frame);
                compare_body(&what, &ctx, body);
            });
            compared.push(Compared {
                name: name.to_owned(),
                blocks: body.basic_blocks.len(),
                locals: body.locals.len(),
            });
        }
    });
}

/// Who a loan's mutable borrow is unique for: a fixed pick both sides are given, so the
/// closure capture's kind is reached without the check's own.
fn unique(assigned: LocalId, local: LocalId, _: &[PlaceElem]) -> bool {
    (raw(assigned) + raw(local)) % 3 == 0
}

/// The analyses of `body` both ways, in the order the check runs them.
fn compare_body<'db>(what: &str, ctx: &Ctx<'_, 'db>, body: &MirBody<'db>) {
    let frame = ctx.frame;
    let cfg = Cfg::new(frame, body);
    let decoded = Decoded::new(ctx, body, &cfg);
    let new_moves = moves::MoveData::gather(ctx, body, &cfg, &decoded);
    let old_moves = old::moves::MoveData::gather(ctx, body, &cfg);
    same_moves(what, ctx, body, &cfg, &new_moves, &old_moves);

    let flow = moves::InitFlow::compute(frame, body, &cfg, &new_moves);
    let old_flow = old::moves::InitFlow::compute(body, &cfg, &old_moves);
    let blocks = body.basic_blocks.len();
    let what_of = |part: &str| format!("{what}: {part}");
    let (paths, locals) = (new_moves.len(), body.locals.len());
    let (uninit, old_uninit) = (&flow.uninit_entry, &old_flow.uninit_entry);
    same_entries(&what_of("maybe uninitialized"), uninit, old_uninit, blocks, paths);
    let (ever, old_ever) = (&flow.ever_entry, &old_flow.ever_entry);
    same_entries(&what_of("ever initialized"), ever, old_ever, blocks, locals);

    let (found, exit) =
        liveness::maybe_initialized_at_drops(frame, body, &cfg, &decoded, &new_moves);
    let (old_found, old_exit) = old::liveness::maybe_initialized_at_drops(body, &cfg, &old_moves);
    assert_eq!(found.words(), old_found.words(), "{what}: the drops of what may be initialized");
    assert_eq!(exit.reached.words(), old_exit.reached.words(), "{what}: blocks reached");
    for block in 0..blocks {
        assert_eq!(
            exit.exit.row(block).words(),
            old_exit.exit.row(block),
            "{what}: initialized leaving block {block}"
        );
        assert_eq!(
            exit.returned.row(block).words(),
            old_exit.returned.row(block),
            "{what}: initialized as block {block}'s call returns"
        );
    }

    let requirements: FMap<'_, ExprId, (usize, FVec<'_, regions::Requirement<'db>>)> =
        mem::map(frame);
    let regions = regions::build(ctx, body, &cfg, &requirements);
    let count = ctx.infcx.num_region_vars();
    let live = liveness::region_liveness(ctx, body, &cfg, &decoded, &new_moves, &regions, count);
    let old_live = old::liveness::region_liveness(ctx, body, &cfg, &old_moves, &regions, count);
    assert_eq!(live.len(), old_live.len(), "{what}: regions");
    let last = cfg.points - 1;
    for rv in 0..count {
        assert!(
            live[rv].runs_in(0, last).eq(old_live[rv].runs_in(0, last)),
            "{what}: the points region {rv} is live at: {:?}, the old liveness's {:?}",
            live[rv].runs_in(0, last).collect::<Vec<_>>(),
            old_live[rv].runs_in(0, last).collect::<Vec<_>>(),
        );
    }
    let values = regions::infer(frame, &regions, count, cfg.points, &live);

    let mut new_loans = loans::Loans::gather(ctx, body, &cfg, &decoded, &regions, unique);
    let mut old_loans = old::loans::Loans::gather(ctx, body, &cfg, &regions, unique);
    same_loans(what, body, &cfg, &new_loans, &old_loans);
    new_loans.compute_scopes(ctx, body, &cfg, &values);
    old_loans.compute_scopes(body, &cfg, &values);
    same_borrows(&what_of("NLL"), ctx, body, &cfg, &new_loans, &old_loans);
    let live_loans = polonius::live_loans(ctx, body, &cfg, &regions, count, &live, &new_loans);
    new_loans.compute_polonius_scopes(ctx, body, &cfg, &live_loans);
    old_loans.compute_polonius_scopes(body, &cfg, &live_loans);
    same_borrows(&what_of("Polonius"), ctx, body, &cfg, &new_loans, &old_loans);
}

fn lookup(it: moves::Lookup) -> (bool, usize) {
    (matches!(it, moves::Lookup::Exact(_)), it.path())
}

fn old_lookup(it: old::moves::Lookup) -> (bool, usize) {
    (matches!(it, old::moves::Lookup::Exact(_)), it.path())
}

/// Every place `body` names: what it assigns, borrows, reads, moves and drops.
fn places(body: &MirBody<'_>) -> Vec<(LocalId, Vec<PlaceElem>)> {
    fn operand(kind: &OperandKind, found: &mut Vec<(LocalId, Vec<PlaceElem>)>) {
        if let OperandKind::Copy(place) | OperandKind::Move(place) = kind {
            found.push((place.local, place.projection.as_slice().to_vec()));
        }
    }
    let mut found = Vec::new();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            statement.kind.for_each_operand(&mut |it| operand(&it.kind, &mut found));
            if let StatementKind::Assign(place, rvalue) = &statement.kind {
                found.push((place.local, place.projection.as_slice().to_vec()));
                if let hir_ty::mir::Rvalue::Ref(_, borrowed) = rvalue {
                    found.push((borrowed.local, borrowed.projection.as_slice().to_vec()));
                }
            }
        }
        if let Some(terminator) = &block.terminator {
            terminator.kind.for_each_operand(&mut |it| operand(&it.kind, &mut found));
            match &terminator.kind {
                TerminatorKind::Drop { place, .. }
                | TerminatorKind::DropAndReplace { place, .. } => {
                    found.push((place.local, place.projection.as_slice().to_vec()))
                }
                TerminatorKind::Call { destination, .. } => {
                    found.push((destination.local, destination.projection.as_slice().to_vec()))
                }
                _ => (),
            }
        }
    }
    found
}

fn same_moves<'db>(
    what: &str,
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg<'_>,
    new: &moves::MoveData<'_>,
    old: &old::moves::MoveData,
) {
    assert_eq!(new.len(), old.len(), "{what}: move paths");
    for path in 0..new.len() {
        assert_eq!(new.local(path), old.local(path), "{what}: the local of path {path}");
        assert_eq!(new.parent(path), old.parent(path), "{what}: the parent of path {path}");
        assert_eq!(new.projection(path), old.projection(path), "{what}: path {path}");
        assert_eq!(new.subtree(path), old.subtree(path), "{what}: below path {path}");
        assert!(new.children(path).eq(old.children(path)), "{what}: the children of path {path}");
        assert_eq!(new.moves_of(path), old.moves_of(path), "{what}: the moves of path {path}");
        let (local, projection) = (new.local(path), new.projection(path));
        assert_eq!(
            lookup(new.find(ctx, body, local, projection)),
            old_lookup(old.find(ctx, body, local, projection)),
            "{what}: path {path} found by its place"
        );
    }
    for (local, _) in body.locals.iter() {
        assert_eq!(new.local_path(local), old.local_path(local), "{what}: the path of {local:?}");
    }
    for (local, projection) in places(body) {
        assert_eq!(
            lookup(new.find(ctx, body, local, &projection)),
            old_lookup(old.find(ctx, body, local, &projection)),
            "{what}: the path of {local:?}{projection:?}"
        );
    }
    assert_eq!(new.moves.len(), old.moves.len(), "{what}: moves");
    for (index, (it, was)) in new.moves.iter().zip(&old.moves).enumerate() {
        assert_eq!((it.path, it.location), (was.path, was.location), "{what}: move {index}");
    }
    assert_eq!(new.inits.len(), old.inits.len(), "{what}: inits");
    for (index, (it, was)) in new.inits.iter().zip(&old.inits).enumerate() {
        assert_eq!(
            (it.path, it.location, it.kind == moves::InitKind::Deep),
            (was.path, was.location, was.kind == old::moves::InitKind::Deep),
            "{what}: init {index}"
        );
    }
    for point in 0..cfg.points {
        assert_eq!(new.moves_at(point), old.moves_at(point), "{what}: the moves at {point}");
        assert_eq!(new.inits_at(point), old.inits_at(point), "{what}: the inits at {point}");
    }
}

/// Each block's row of `rows` against the old matrix of `columns` bits, bit for bit.
fn same_entries(what: &str, rows: &Rows<'_>, old: &old::BitMatrix, blocks: usize, columns: usize) {
    let words = words_for(columns);
    assert_eq!(rows.words(), words, "{what}: words per row");
    let mut row = vec![0u64; words];
    for block in 0..blocks {
        row.fill(!0);
        rows.load(block, &mut row);
        assert_eq!(row, old.row(block), "{what}: entry of block {block}");
    }
}

fn activation(it: loans::Activation) -> Option<Option<Location>> {
    match it {
        loans::Activation::NotTwoPhase => None,
        loans::Activation::NotActivated => Some(None),
        loans::Activation::At(at) => Some(Some(at)),
    }
}

fn old_activation(it: old::loans::Activation) -> Option<Option<Location>> {
    match it {
        old::loans::Activation::NotTwoPhase => None,
        old::loans::Activation::NotActivated => Some(None),
        old::loans::Activation::At(at) => Some(Some(at)),
    }
}

/// The loans in order, each field of each, and their indexes.
fn same_loans(
    what: &str,
    body: &MirBody<'_>,
    cfg: &Cfg<'_>,
    new: &loans::Loans<'_>,
    old: &old::loans::Loans,
) {
    assert_eq!(new.loans.len(), old.loans.len(), "{what}: loans");
    for (id, (it, was)) in new.loans.iter().zip(&old.loans).enumerate() {
        assert_eq!(
            (it.local, it.kind, it.region, it.location, it.span, it.assigned),
            (was.local, was.kind, was.region, was.location, was.span, was.assigned),
            "{what}: loan {id}"
        );
        assert_eq!(
            activation(it.activation),
            old_activation(was.activation),
            "{what}: loan {id}'s activation"
        );
        assert_eq!(new.projection(id), old.projection(id), "{what}: the place of loan {id}");
    }
    assert_eq!(new.promoted.len(), old.promoted.len(), "{what}: promoted borrows");
    for at in &old.promoted {
        assert!(new.promoted.contains(at), "{what}: the borrow at {at:?} is promoted");
    }
    for (local, _) in body.locals.iter() {
        assert_eq!(new.of_local(local), old.of_local(local), "{what}: the loans of {local:?}");
    }
    for point in 0..cfg.points {
        let (it, was) = (new.activated_at(point), old.activated_at(point));
        assert_eq!(it, was, "{what}: activated at {point}");
    }
}

/// What each reachable point does to the loans in scope, as it starts (the scopes that end)
/// and as it runs (its kills, then the loan it makes), and the entry sets.
fn same_borrows<'a>(
    what: &str,
    ctx: &Ctx<'a, '_>,
    body: &MirBody<'_>,
    cfg: &Cfg<'_>,
    new: &loans::Loans<'a>,
    old: &old::loans::Loans,
) {
    let count = new.loans.len();
    let all = old::BitSet::filled(count);
    let none = old::BitSet::new(count);
    let mut state = old::BitSet::new(count);
    let mut was = old::BitSet::new(count);
    for &block in cfg.order.iter() {
        for statement in 0..=body.basic_blocks[block].statements.len() {
            let location = Location { block, statement };
            for (start, part) in [(&all, "ending"), (&all, "killed"), (&none, "made")] {
                state.load(start.words());
                was.load(start.words());
                if part == "ending" {
                    new.before(cfg, location, &mut state);
                    old.before(cfg, location, &mut was);
                } else {
                    new.after(cfg, location, &mut state);
                    old.after(cfg, location, &mut was);
                }
                assert_eq!(state.words(), was.words(), "{what}: the loans {part} at {location:?}");
            }
        }
    }
    let entry = new.entry_sets(ctx, body, cfg);
    let old_entry = old.entry_sets(body, cfg);
    same_entries(&format!("{what}: borrows"), &entry, &old_entry, body.basic_blocks.len(), count);
}

#[test]
fn the_analyses_agree_on_the_borrowck_fixtures() {
    let mut compared = 0;
    for fixture in [CHECK_ERRORS, CHECK_CLEAN, HIR, budget::TYPICAL, budget::ERRORS] {
        compared += compare_fixture(fixture).len();
    }
    assert!(compared >= 30, "{compared} bodies compared");
}

#[test]
fn the_analyses_agree_on_large_generated_bodies() {
    let compared = compare_fixture(&large_fixture());
    for name in ["large", "paths"] {
        let Some(it) = compared.iter().find(|it| it.name == name) else {
            panic!("`{name}` was not compared")
        };
        let (blocks, locals) = (it.blocks, it.locals);
        assert!(blocks >= 2000 && locals >= 2000, "`{name}`: {blocks} blocks, {locals} locals");
    }
}

/// The source text `span` points at; of a scope end, its closing brace.
fn source(db: &Db, source_map: &BodySourceMap, span: MirSpan) -> String {
    let source = match span {
        MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => source_map.expr_syntax(expr),
        MirSpan::PatId(pat) => source_map.pat_syntax(pat),
        MirSpan::BindingId(binding) => {
            source_map.pat_syntax(source_map.patterns_for_binding(binding)[0])
        }
        other => panic!("{other:?} has no source"),
    }
    .unwrap_or_else(|_| panic!("{span:?} has no source"));
    let node = syntax::view::to_node(source.value.syntax_node_ptr(), source.file_id.root(db));
    match span {
        MirSpan::ExprEnd(_) => node.last_token().unwrap().text().to_owned(),
        _ => node.text().to_owned(),
    }
}

/// Per fixture, the functions with errors: each error's code and the source text of its
/// primary span, in the order rustc reports them; and the functions given `unused_mut`.
/// Every other function of the fixture has no error, and the only lint is `unused_mut`; the
/// functions of `minicore` are not the fixture's.
type Diagnosed = &'static [(&'static str, &'static [(&'static str, &'static str)])];

const DIAGNOSED: &[(&str, Diagnosed, &[&str])] = &[
    (
        CHECK_ERRORS,
        &[
            ("e0499", &[("E0499", "&mut i")]),
            ("e0502", &[("E0502", "&mut s")]),
            ("e0505", &[("E0505", "x")]),
            ("e0596", &[("E0596", "&mut x")]),
            ("e0597", &[("E0597", "&s")]),
        ],
        &[],
    ),
    (CHECK_CLEAN, &[], &[]),
    (HIR, &[("moved_twice", &[("E0382", "s")])], &["mut_unused"]),
    (budget::TYPICAL, &[], &[]),
    (
        budget::ERRORS,
        &[("moved_twice", &[("E0382", "text")]), ("two_mut", &[("E0499", "&mut n")])],
        &["unused_mut"],
    ),
];

#[test]
fn borrowck_reports_rustcs_codes_at_rustcs_spans_in_both_modes() {
    for &(fixture, diagnosed, unused_mut) in DIAGNOSED {
        let fx = fixture::db(fixture);
        let _ = fx.only_file();
        let db = fx.into_db();
        hir_ty::attach_db(&db, || {
            for (name, def) in budget::functions(&db) {
                let declared = |after: char| fixture.contains(&format!("fn {name}{after}"));
                if !declared('(') && !declared('<') {
                    continue;
                }
                let (_, source_map) = Body::with_source_map(&db, def);
                let want: &[(&str, &str)] =
                    diagnosed.iter().find(|it| it.0 == name).map_or(&[], |it| it.1);
                for mode in [Mode::Nll, Mode::Polonius] {
                    let result = borrowck_in_mode(&db, def, mode);
                    let errors: Vec<(&str, String)> = result
                        .diagnostics
                        .iter()
                        .filter(|it| it.is_error())
                        .map(|it| (it.code, source(&db, &source_map, it.span)))
                        .collect();
                    let want: Vec<(&str, String)> =
                        want.iter().map(|&(code, at)| (code, at.to_owned())).collect();
                    assert_eq!(errors, want, "`{name}` in {mode:?}");
                    let lints: Vec<&str> = result
                        .diagnostics
                        .iter()
                        .filter(|it| !it.is_error())
                        .map(|it| it.code)
                        .collect();
                    let only_unused_mut = lints.iter().all(|&it| it == "unused_mut");
                    assert!(only_unused_mut, "`{name}` in {mode:?}: {lints:?}");
                    if unused_mut.contains(&name.as_str()) {
                        assert_eq!(lints, ["unused_mut"], "`{name}` in {mode:?}");
                    }
                }
            }
        });
    }
}
