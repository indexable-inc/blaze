//! The labels, notes and suggestions of the errors against rustc's: the `.stderr` of its
//! `tests/ui/error-codes` tests for E0499, E0505, E0596 and E0597, with what each part points
//! at written as its source text. And the budget: the check of a body with no error takes
//! nothing from the global allocator.

use std::cell::Cell;

use base_db::{Db, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, ModuleDefId,
    expr_store::{Body, BodySourceMap},
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature},
};
use hir_ty::mir::MirSpan;
use hir_ty_mir as _;
use hir_expand::fixture;

use crate::{Applicability, BorrowckDiagnostic, Mode, PartSpan, Sublevel, borrowck_in_mode};


thread_local! {
    /// What the parts of the checks run on this thread took from the global allocator.
    pub(super) static GLOBAL: Cell<u64> = const { Cell::new(0) };
}

const FIXTURE: &str = r#"
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

fn function(db: &Db, name: &str) -> DefWithBodyId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it.into();
                }
            }
        }
    }
    panic!("no function `{name}`")
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

fn part(db: &Db, source_map: &BodySourceMap, span: PartSpan) -> String {
    match span {
        PartSpan::At(span) => source(db, source_map, span),
        PartSpan::Before(span) => format!("before {}", source(db, source_map, span)),
        PartSpan::Def(AdtId::StructId(it)) => {
            format!("def {}", StructSignature::of(db, it).name.as_str())
        }
        other => panic!("{other:?} is not in these tests"),
    }
}

type Labels = Vec<(String, String)>;

fn labels(db: &Db, source_map: &BodySourceMap, labels: &[(PartSpan, String)]) -> Labels {
    let mut found: Labels =
        labels.iter().map(|(at, text)| (part(db, source_map, *at), text.clone())).collect();
    found.sort();
    found
}

fn owned(labels: &[(&str, &str)]) -> Labels {
    let mut owned: Labels =
        labels.iter().map(|&(at, text)| (at.to_owned(), text.to_owned())).collect();
    owned.sort();
    owned
}

/// The one error `name` gives, with its code `code` and `labels`, in any order, and what
/// `rest` asks of it with the labels of its children rendered.
fn error_of(
    name: &str,
    code: &str,
    want: &[(&str, &str)],
    rest: impl FnOnce(&Db, &BodySourceMap, &BorrowckDiagnostic),
) {
    let fx = fixture::db(FIXTURE);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = function(&db, name);
        let (_, source_map) = Body::with_source_map(&db, def);
        let result = crate::borrowck(&db, def);
        let errors: Vec<&BorrowckDiagnostic> =
            result.diagnostics.iter().filter(|it| it.code.starts_with('E')).collect();
        let &[error] = &errors[..] else { panic!("`{name}`: {errors:?}") };
        assert_eq!(error.code, code, "`{name}`: {error:?}");
        assert_eq!(labels(&db, &source_map, &error.labels), owned(want), "`{name}`");
        rest(&db, &source_map, error);
    });
}

#[test]
fn e0499_points_at_both_borrows_and_the_later_use() {
    error_of(
        "e0499",
        "E0499",
        &[
            ("&mut i", "first mutable borrow occurs here"),
            ("&mut i", "second mutable borrow occurs here"),
            ("x", "first borrow later used here"),
        ],
        |_, _, error| {
            assert!(error.children.is_empty() && error.suggestions.is_empty(), "{error:?}")
        },
    );
}

#[test]
fn e0502_says_which_borrow_is_mutable() {
    error_of(
        "e0502",
        "E0502",
        &[
            ("&mut s", "mutable borrow occurs here"),
            ("&s", "immutable borrow occurs here"),
            ("a", "immutable borrow later used here"),
        ],
        |_, _, error| {
            assert!(error.children.is_empty() && error.suggestions.is_empty(), "{error:?}")
        },
    );
}

#[test]
fn e0505_names_the_binding_and_says_what_cloning_needs() {
    error_of(
        "e0505",
        "E0505",
        &[
            ("x", "binding `x` declared here"),
            ("&x", "borrow of `x` occurs here"),
            ("x", "move out of `x` occurs here"),
            ("_ref_to_val", "borrow later used here"),
        ],
        |db, source_map, error| {
            let [note] = &error.children[..] else { panic!("{error:?}") };
            assert_eq!(note.level, Sublevel::Note);
            assert_eq!(note.message, "if `Value` implemented `Clone`, you could clone the value");
            let spans: Vec<String> =
                note.spans.iter().map(|&it| part(db, source_map, it)).collect();
            assert_eq!(spans, ["def Value"]);
            assert_eq!(
                labels(db, source_map, &note.labels),
                owned(&[
                    ("def Value", "consider implementing `Clone` for this type"),
                    ("x", "you could clone this value"),
                ])
            );
            assert!(error.suggestions.is_empty(), "{error:?}");
        },
    );
}

#[test]
fn e0596_suggests_mut_at_the_binding() {
    error_of(
        "e0596",
        "E0596",
        &[("&mut x", "cannot borrow as mutable")],
        |db, source_map, error| {
            assert!(error.children.is_empty(), "{error:?}");
            let [suggestion] = &error.suggestions[..] else { panic!("{error:?}") };
            assert_eq!(suggestion.message, "consider changing this to be mutable");
            assert_eq!(suggestion.applicability, Applicability::MachineApplicable);
            assert!(suggestion.verbose);
            let parts: Labels = suggestion
                .parts
                .iter()
                .map(|(at, text)| (part(db, source_map, *at), text.clone()))
                .collect();
            assert_eq!(parts, owned(&[("before x", "mut ")]));
        },
    );
}

#[test]
fn e0597_points_at_the_drop_and_the_later_use() {
    error_of(
        "e0597",
        "E0597",
        &[
            ("s", "binding `s` declared here"),
            ("&s", "borrowed value does not live long enough"),
            ("}", "`s` dropped here while still borrowed"),
            // A call's argument is read into a temporary first (`as_call_operand`), so the
            // later use is that read, as in rustc's tests/ui/variance/variance-issue-20533.
            ("r", "borrow later used here"),
        ],
        |_, _, error| {
            assert!(error.children.is_empty() && error.suggestions.is_empty(), "{error:?}")
        },
    );
}

/// Bodies with no error that reach each part: two-phase borrows, loans in scope at calls
/// that may unwind and across a loop's way back, guards, an index after an index of a
/// slice, a union, a `static mut`, closures by reference and by value, a returned
/// `impl Trait`, moves out and reinitialization.
const CLEAN: &str = r#"
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

#[test]
fn a_body_with_no_error_takes_nothing_from_the_global_allocator() {
    let fx = fixture::db(CLEAN);
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let mut bodies = 0;
        for &krate in all_crates(&db).iter() {
            let def_map = crate_def_map(&db, krate);
            for (_, module) in def_map.modules(&db) {
                for def in module.scope(&db).declarations() {
                    let ModuleDefId::FunctionId(it) = def else { continue };
                    for mode in [Mode::Nll, Mode::Polonius] {
                        // The first run warms what outlives a check: the queries it asks and
                        // the types it interns.
                        let first = borrowck_in_mode(&db, it.into(), mode);
                        assert!(first.skipped.is_none(), "{it:?}: {:?}", first.skipped);
                        assert!(
                            first.not_checked.is_none(),
                            "{}: {first:?}",
                            FunctionSignature::of(&db, it).name.as_str()
                        );
                        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
                        GLOBAL.with(|it| it.set(0));
                        let _ = borrowck_in_mode(&db, it.into(), mode);
                        let global = GLOBAL.with(Cell::get);
                        assert_eq!(global, 0, "{it:?} in {mode:?}: global allocations");
                    }
                    bodies += 1;
                }
            }
        }
        assert!(bodies >= 10, "{bodies} bodies");
    });
}
