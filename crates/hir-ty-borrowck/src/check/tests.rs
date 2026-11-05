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
use hir_expand::fixture;
use hir_ty::mir::MirSpan;
use hir_ty_mir as _;

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

#[test]
fn a_question_mark_with_an_uninhabited_output_does_not_leave_an_uninitialized_binding() {
    let fx = fixture::db(r#"
//- minicore: sized, copy, result, try, future, from
enum Void {}
enum Entered { Model(Void) }
fn read() -> Result<Void, u32> { Err(1) }
async fn read_async() -> Result<Void, u32> { Err(1) }
fn enter() -> Result<Entered, u32> {
    let definition = read()?;
    Ok(Entered::Model(definition))
}
async fn enter_async() -> Result<Entered, u32> {
    let definition = read_async().await?;
    Ok(Entered::Model(definition))
}
fn uninitialized() -> u32 { let value; value }
fn initialized() -> Result<u32, u32> { let value = Ok::<u32, u32>(1)?; Ok(value) }
"#);
    let _ = fx.only_file();
    let db = fx.into_db();
    for name in ["enter", "enter_async", "initialized"] {
        for mode in [Mode::Nll, Mode::Polonius, Mode::PoloniusFlow] {
            let result = borrowck_in_mode(&db, function(&db, name), mode);
            assert!(result.skipped.is_none() && result.not_checked.is_none(), "{name}: {result:?}");
            assert!(result.diagnostics.iter().all(|it| !it.code.starts_with('E')), "{name}: {:?}", result.diagnostics);
        }
    }
    let bad = crate::borrowck(&db, function(&db, "uninitialized"));
    assert!(bad.diagnostics.iter().any(|it| it.code == "E0381"), "{:?}", bad.diagnostics);
}

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
        PartSpan::LineAfter(span) => format!("line after {}", source(db, source_map, span)),
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
    error_in(FIXTURE, name, |db, source_map, error| {
        assert_eq!(error.code, code, "`{name}`: {error:?}");
        assert_eq!(labels(db, source_map, &error.labels), owned(want), "`{name}`");
        rest(db, source_map, error);
    });
}

/// What `rest` asks of the one error the function `name` of `fixture` gives.
///
/// Checked with no database attached to the thread, as a pool worker runs the check: a text
/// that names a type through `Debug` (which reads the attached database) instead of with the
/// database in hand panics outside a query's run (`fmt_with_attached_db` in hir-ty), so each test
/// here fails on a message rendered detached.
fn error_in(
    fixture: &str,
    name: &str,
    rest: impl FnOnce(&Db, &BodySourceMap, &BorrowckDiagnostic),
) {
    let fx = fixture::db(fixture);
    let _ = fx.only_file();
    let db = fx.into_db();
    let def = function(&db, name);
    let source_map = Body::source_map(&db, def);
    let result = crate::borrowck(&db, def);
    let errors: Vec<&BorrowckDiagnostic> =
        result.diagnostics.iter().filter(|it| it.code.starts_with('E')).collect();
    let &[error] = &errors[..] else { panic!("`{name}`: {errors:?}") };
    rest(&db, &source_map, error);
}

/// A value dropped while a move needs its place: a trait method's `impl Fn` (sb-rpitit's
/// case), an ADT with `Drop`, and a tuple holding one.
const DROPPED_LATER: &str = r#"
//- minicore: sized, fn, drop
struct Value {}
struct Guard<'a>(&'a Value);
impl Drop for Guard<'_> { fn drop(&mut self) {} }
fn eat<T>(_: T) {}
trait D { fn b(&self) -> impl Fn() -> u32; }

fn opaque<T: D>(t: T) {
    let f = t.b();
    eat(t);
}

fn adt() {
    let x = Value {};
    let g = Guard(&x);
    eat(x);
}

fn tuple() {
    let x = Value {};
    let g = (Guard(&x), 1u8);
    eat(x);
}

fn moved_twice() {
    let x = Value {};
    eat(x);
    eat(x);
}
"#;

/// The label of the drop that uses the borrow, of the one error of `name`.
fn drop_label(name: &str) -> String {
    let mut found = None;
    error_in(DROPPED_LATER, name, |_, _, error| {
        assert_eq!(error.code, "E0505", "`{name}`: {error:?}");
        found = error
            .labels
            .iter()
            .find(|(_, text)| text.contains("is dropped"))
            .map(|(_, text)| text.clone());
        assert!(found.is_some(), "`{name}`: no drop label: {error:?}");
    });
    found.expect("checked above")
}

/// E0505's drop label names the dropped value's type as the source writes it, rendered while
/// the check had the database, never `<AdtDef: no database attached>` (sb-rpitit, a check on
/// a worker with none attached).
#[test]
fn e0505_names_the_dropped_type_with_no_database_attached() {
    assert_eq!(
        drop_label("opaque"),
        "borrow might be used here, when `f` is dropped and runs the destructor for type `impl Fn() -> u32`"
    );
    assert_eq!(
        drop_label("adt"),
        "borrow might be used here, when `g` is dropped and runs the `Drop` code for type `Guard`"
    );
    assert_eq!(
        drop_label("tuple"),
        "borrow might be used here, when `g` is dropped and runs the destructor for type `(Guard<'_>, u8)`"
    );
}

/// E0382's `Copy` label names the type, with no database attached.
#[test]
fn e0382_names_the_moved_type_with_no_database_attached() {
    error_in(DROPPED_LATER, "moved_twice", |_, _, error| {
        assert_eq!(error.code, "E0382", "{error:?}");
        assert!(
            error.labels.iter().any(|(_, text)| text
                == "move occurs because `x` has type `Value`, which does not implement the `Copy` trait"),
            "{error:?}"
        );
    });
}

/// A `RefCell`'s `RefMut`, cut down: it borrows the cell and its drop does something, so it
/// is used again where its scope ends.
const DROP_EARLY: &str = r#"
//- minicore: sized, copy, drop
struct Cell { v: u32 }
struct RefMut<'a> { cell: &'a Cell }
impl Drop for RefMut<'_> { fn drop(&mut self) {} }
impl Cell { fn borrow_mut(&self) -> RefMut<'_> { RefMut { cell: self } } }
impl RefMut<'_> { fn touch(&mut self) {} }

fn blocks_a_later_borrow() {
    let mut c = Cell { v: 0 };
    let mut r = c.borrow_mut();
    r.touch();
    let m = &mut c;
    m.v = 1;
}

fn used_with_the_later_borrow() {
    let mut c = Cell { v: 0 };
    let mut r = c.borrow_mut();
    let m = { r.touch(); &mut c };
    m.v = 1;
}
"#;

/// The error of a `RefMut` that keeps its cell borrowed only because it is dropped where its
/// scope ends gets the note naming it and `drop(r);` after its last use: checked, the body
/// with that drop passes.
#[test]
fn a_refmut_dropped_at_scope_end_gets_the_drop_early_note() {
    error_in(DROP_EARLY, "blocks_a_later_borrow", |db, source_map, error| {
        assert_eq!(error.code, "E0502", "{error:?}");
        let notes: Vec<&crate::Child> = error
            .children
            .iter()
            .filter(|it| it.message.contains("is dropped only where its scope ends"))
            .collect();
        let [note] = &notes[..] else { panic!("{error:?}") };
        assert_eq!(note.level, Sublevel::Note, "{error:?}");
        assert!(note.message.starts_with("`r`, a `RefMut"), "{}", note.message);
        let last: Vec<String> =
            note.labels.iter().map(|(at, _)| part(db, source_map, *at)).collect();
        // The use written last in the statement: the call, or its receiver.
        assert!(matches!(&last[..], [it] if it == "r.touch()" || it == "r"), "{last:?}");
        let suggestion = error
            .suggestions
            .iter()
            .find(|it| it.message == "drop `r` after its last use")
            .unwrap_or_else(|| panic!("{error:?}"));
        assert_eq!(suggestion.applicability, Applicability::MachineApplicable);
        let parts: Labels = suggestion
            .parts
            .iter()
            .map(|(at, text)| (part(db, source_map, *at), text.clone()))
            .collect();
        assert_eq!(parts, owned(&[("line after r.touch()", "drop(r);")]));
    });
}

/// Where `drop(r);` could go only after the statement that borrows again, the body with it
/// fails the same way: no note, no fix.
#[test]
fn a_drop_the_rewrite_would_not_fix_gets_no_note() {
    error_in(DROP_EARLY, "used_with_the_later_borrow", |_, _, error| {
        assert!(error.dropped_at_scope_end.is_some(), "the later use is `r`'s drop: {error:?}");
        assert!(
            !error
                .children
                .iter()
                .any(|it| it.message.contains("is dropped only where its scope ends")),
            "{error:?}"
        );
        assert!(
            !error.suggestions.iter().any(|it| it.message.starts_with("drop `r`")),
            "{error:?}"
        );
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
    blaze_query::db::attach(&db, || {
        let mut bodies = 0;
        for &krate in all_crates(&db).iter() {
            let def_map = crate_def_map(&db, krate);
            for (_, module) in def_map.modules(&db) {
                for def in module.scope(&db).declarations() {
                    let ModuleDefId::FunctionId(it) = def else { continue };
                    for mode in [Mode::Nll, Mode::Polonius, Mode::PoloniusFlow] {
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
