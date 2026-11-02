//! The qualif checks of `const_check` (`check_consts/qualifs.rs`, the qualif half of
//! `check.rs` and `post_drop_elaboration.rs`): values dropped in a const context (E0493),
//! mutable (E0764) and interior mutable (E0492) borrows that outlive the evaluation, and reads
//! of a `#[thread_local]` static (E0625), against what rustc reports at the pinned nightly. Each
//! test ports the rustc ui test it names, with its expected errors. Where the ui test uses std's
//! types the fixture declares its own of the same shape (`Option`, a `Box` with a destructor,
//! `Cell` for `AtomicUsize`), and std's `const` `Drop` and `Destruct` are declared as the lang
//! items they are, since minicore's `Drop` is not `const`.

use hir_def::{DefWithBodyId, expr_store::Body};
use hir_ty_mir::mir::{
    MirSpan,
    const_check::{ConstCheckKind, const_check},
};
use hir_expand::fixture;

use crate::{constant, db::TestDB, function};

/// A qualif check's finding: its code, message and the text its span covers, and for a
/// `LiveDrop` the text where the value is dropped and whether its destructor is not const.
type Found = (&'static str, String, String, Option<(String, bool)>);

/// What the qualif checks report in the body `pick` finds in `src` with `minicore`'s flags.
/// The end of an expression (`ExprEnd`) is its last character, as rustc points at a scope's
/// closing brace.
#[track_caller]
fn check(minicore: &str, src: &str, pick: impl FnOnce(&TestDB) -> DefWithBodyId) -> Vec<Found> {
    let fx = fixture::db(&format!("//- minicore: {minicore}\n{src}"));
    let _ = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let def = pick(&db);
        let (_, map) = Body::with_source_map(&db, def);
        let at = |span: MirSpan| {
            let range = match span {
                MirSpan::ExprId(expr) | MirSpan::ExprEnd(expr) => {
                    map.expr_syntax(expr).ok().map(|it| it.value.text_range())
                }
                MirSpan::PatId(pat) => map.pat_syntax(pat).ok().map(|it| it.value.text_range()),
                MirSpan::SelfParam => map.self_param_syntax().map(|it| it.value.text_range()),
                other => panic!("a span with no syntax: {other:?}"),
            }
            .unwrap_or_else(|| panic!("synthetic syntax at {span:?}"));
            let end = usize::from(range.end());
            let start = match span {
                MirSpan::ExprEnd(_) => end - 1,
                _ => usize::from(range.start()),
            };
            src[start..end].to_owned()
        };
        const_check(&db, def)
            .diagnostics
            .iter()
            .filter_map(|it| {
                let dropped = match &it.kind {
                    ConstCheckKind::LiveDrop { dropped_at, needs_non_const_drop, .. } => {
                        Some((at(*dropped_at), *needs_non_const_drop))
                    }
                    ConstCheckKind::EscapingMutBorrow
                    | ConstCheckKind::EscapingCellBorrow
                    | ConstCheckKind::ThreadLocalAccess => None,
                    _ => return None,
                };
                // clone: the test's own copy of the message
                Some((it.code, it.message.clone(), at(it.span), dropped))
            })
            .collect()
    })
}

type Reported<'a> = (&'static str, &'a str, &'a str, Option<(&'a str, bool)>);

fn reported(found: &[Found]) -> Vec<Reported<'_>> {
    found
        .iter()
        .map(|(code, message, at, dropped)| {
            let dropped = dropped.as_ref().map(|(at, non_const)| (at.as_str(), *non_const));
            (*code, message.as_str(), at.as_str(), dropped)
        })
        .collect()
}

fn func(name: &'static str) -> impl FnOnce(&TestDB) -> DefWithBodyId {
    move |db| function(db, name).into()
}

fn konst(name: &'static str) -> impl FnOnce(&TestDB) -> DefWithBodyId {
    move |db| constant(db, name).into()
}

fn statik(name: &'static str) -> impl FnOnce(&TestDB) -> DefWithBodyId {
    move |db| hir_def::test_items::static_item(db, fixture::test_crate(&db), name).into()
}

const NONE: [Reported<'static>; 0] = [];

fn destructor(ty: &str) -> String {
    format!("destructor of `{ty}` cannot be evaluated at compile-time")
}

const MUT_BORROW: &str = "mutable borrows of temporaries that have their lifetime extended \
                          until the end of the program are not allowed";
const CELL_BORROW: &str = "interior mutable shared borrows of temporaries that have their \
                           lifetime extended until the end of the program are not allowed";
const THREAD_LOCAL: &str = "thread-local statics cannot be accessed at compile-time";

/// std's `Drop` and `Destruct`, both `const` traits.
const DROP: &str = "\
#![feature(lang_items)]
#[lang = \"drop\"]
pub const trait Drop {
    fn drop(&mut self);
}
#[lang = \"destruct\"]
pub const trait Destruct {}
";

const OPTION: &str = "\
enum Option<T> {
    Some(T),
    None,
}
use Option::{None, Some};
";

/// `tests/ui/consts/const-eval/livedrop.rs`, with a type whose destructor is `const` for
/// `Vec`: the value assigned over is dropped at the assignment, and the error is at the local.
#[test]
fn a_value_assigned_over_is_dropped_at_the_assignment() {
    let src = format!(
        "#![feature(const_trait_impl)]
{DROP}{OPTION}
struct V;
impl const Drop for V {{
    fn drop(&mut self) {{}}
}}

const C: Option<V> = {{
    let mut never_returned = Some(V);
    let mut always_returned = None;

    let mut i = 0;
    loop {{
        always_returned = never_returned;
        never_returned = None;

        i += 1;
        if i == 10 {{
            break always_returned;
        }}
    }}
}};
"
    );
    let found = check("sized, copy", &src, konst("C"));
    assert_eq!(
        reported(&found),
        [("E0493", &*destructor("Option<V>"), "mut always_returned", Some(("always_returned", false)))]
    );
}

/// `tests/ui/consts/const-eval/livedrop.rs` with a destructor that is not `const`.
#[test]
fn a_non_const_destructor_is_a_non_const_drop() {
    let src = format!(
        "{DROP}{OPTION}
struct V;
impl Drop for V {{
    fn drop(&mut self) {{}}
}}

const C: Option<V> = {{
    let mut never_returned = Some(V);
    let mut always_returned = None;

    let mut i = 0;
    loop {{
        always_returned = never_returned;
        never_returned = None;

        i += 1;
        if i == 10 {{
            break always_returned;
        }}
    }}
}};
"
    );
    let found = check("sized, copy", &src, konst("C"));
    assert_eq!(
        reported(&found),
        [("E0493", &*destructor("Option<V>"), "mut always_returned", Some(("always_returned", true)))]
    );
}

/// `tests/ui/consts/drop_box.rs`: a parameter dropped at the end of the body.
#[test]
fn a_parameter_is_dropped_at_the_end() {
    let src = format!(
        "{DROP}
struct Box<T>(*mut T);
impl<T> Drop for Box<T> {{
    fn drop(&mut self) {{}}
}}

const fn f<T>(_: Box<T>) {{}}
"
    );
    let found = check("sized, copy", &src, func("f"));
    assert_eq!(reported(&found), [("E0493", &*destructor("Box<T>"), "_", Some(("}", true)))]);
}

/// `tests/ui/consts/drop_none.rs`: the constant `FOO` is `None`, which has nothing to drop,
/// whatever its type.
#[test]
fn a_constant_is_its_value_not_its_type() {
    let src = format!(
        "{DROP}{OPTION}
struct A;
impl Drop for A {{
    fn drop(&mut self) {{}}
}}

const FOO: Option<A> = None;

const BAR: () = (FOO, ()).1;
"
    );
    assert_eq!(reported(&check("sized, copy", &src, konst("BAR"))), NONE);
}

/// `tests/ui/consts/unstable-const-fn-in-libcore.rs`, its method a free function: `f` is moved
/// out of on one arm only and `this` only in part, so both are dropped at the end, `f` first.
#[test]
fn what_one_arm_moves_is_dropped_at_the_end() {
    let src = format!(
        "#![feature(const_trait_impl)]
{DROP}
enum Opt<T> {{
    Some(T),
    None,
}}

const fn unwrap_or_else<T, F: FnOnce() -> T>(this: Opt<T>, f: F) -> T {{
    match this {{
        Opt::Some(t) => t,
        Opt::None => f(),
    }}
}}
"
    );
    let found = check("sized, copy, fn", &src, func("unwrap_or_else"));
    assert_eq!(
        reported(&found),
        [
            ("E0493", &*destructor("F"), "f", Some(("}", true))),
            ("E0493", &*destructor("Opt<T>"), "this", Some(("}", true))),
        ]
    );
}

/// `tests/ui/consts/control-flow/drop-fail.rs` without its `Option` and `Result`.
fn drop_fail(features: &str) -> String {
    format!(
        "#![feature({features})]
{DROP}{OPTION}
enum Result<T, E> {{
    Ok(T),
    Err(E),
}}
use Result::{{Err, Ok}};

struct NotConstDestruct;

impl Drop for NotConstDestruct {{
    fn drop(&mut self) {{}}
}}

const C1: Option<NotConstDestruct> = {{
    let y: Option<NotConstDestruct> = None;
    let x = Some(NotConstDestruct);

    if true {{
        x
    }} else {{
        y
    }}
}};

const C2: NotConstDestruct = {{
    let vec_tuple = (NotConstDestruct,);

    vec_tuple.0
}};

const C3: NotConstDestruct = {{
    let x: Result<_, NotConstDestruct> = Ok(NotConstDestruct);

    match x {{
        Ok(x) | Err(x) => x,
    }}
}};

const C4: Option<NotConstDestruct> = {{
    let mut some = Some(NotConstDestruct);
    let mut tmp = None;

    let mut i = 0;
    while i < 10 {{
        tmp = some;
        some = None;

        if i > 100 {{
            break;
        }}

        some = tmp;
        tmp = None;

        i += 1;
    }}

    some
}};
"
    )
}

/// `tests/ui/consts/control-flow/drop-fail.rs`, revision `stock`: a local moved out of in part
/// still needs its drop.
#[test]
fn a_value_moved_out_of_in_part_is_dropped() {
    let src = drop_fail("const_trait_impl, const_destruct");
    let option = destructor("Option<NotConstDestruct>");
    let c1 = check("sized, copy", &src, konst("C1"));
    assert_eq!(reported(&c1), [("E0493", &*option, "x", Some(("}", true)))]);
    let c2 = check("sized, copy", &src, konst("C2"));
    assert_eq!(
        reported(&c2),
        [("E0493", &*destructor("(NotConstDestruct,)"), "vec_tuple", Some(("}", true)))]
    );
    let c3 = check("sized, copy", &src, konst("C3"));
    assert_eq!(
        reported(&c3),
        [(
            "E0493",
            &*destructor("Result<NotConstDestruct, NotConstDestruct>"),
            "x",
            Some(("}", true)),
        )]
    );
    let c4 = check("sized, copy", &src, konst("C4"));
    assert_eq!(reported(&c4), [("E0493", &*option, "mut tmp", Some(("}", true)))]);
}

/// `tests/ui/consts/control-flow/drop-fail.rs`, revision `precise`: after drop elaboration
/// what was moved out of, in whole or in part, is not dropped.
#[test]
fn precise_live_drops_check_after_drop_elaboration() {
    let src = drop_fail("const_trait_impl, const_destruct, const_precise_live_drops");
    let option = destructor("Option<NotConstDestruct>");
    let c1 = check("sized, copy", &src, konst("C1"));
    assert_eq!(reported(&c1), [("E0493", &*option, "x", Some(("}", true)))]);
    assert_eq!(reported(&check("sized, copy", &src, konst("C2"))), NONE);
    assert_eq!(reported(&check("sized, copy", &src, konst("C3"))), NONE);
    let c4 = check("sized, copy", &src, konst("C4"));
    assert_eq!(reported(&c4), [("E0493", &*option, "mut tmp", Some(("}", true)))]);
}

/// `tests/ui/consts/drop_zst.rs`: what is dropped after drop elaboration is still refused.
#[test]
fn precise_live_drops_refuse_what_is_dropped() {
    let src = format!(
        "#![feature(const_precise_live_drops)]
{DROP}
struct S;

impl Drop for S {{
    fn drop(&mut self) {{}}
}}

const fn foo() {{
    let s = S;
}}
"
    );
    let found = check("sized, copy", &src, func("foo"));
    assert_eq!(reported(&found), [("E0493", &*destructor("S"), "s", Some(("}", true)))]);
}

/// `tests/ui/consts/const-mut-refs/mut_ref_in_final.rs`, without std.
const MUT_REF_IN_FINAL: &str = "\
enum Option<T> {
    Some(T),
    None,
}
use Option::{None, Some};

const B: *mut i32 = &mut 4;

const B2: Option<&mut i32> = None;

const B3: Option<&mut i32> = Some(&mut 42);

const fn helper(x: &mut i32) -> Option<&mut i32> { Some(x) }
const B4: Option<&mut i32> = helper(&mut 42);

const C: *const i32 = &{
    let mut x = 42;
    x += 3;
    x
};

const C_NO: *const i32 = &{
    let mut x = 42;
    let _ref = &mut x;
    x
};

static mut MUT_ARRAY: &mut [u8] = &mut [42];
";

/// `tests/ui/consts/const-mut-refs/mut_ref_in_final.rs`: a mutable borrow in a constant's
/// final value.
#[test]
fn a_mutable_borrow_in_the_final_value_is_refused() {
    let flags = "sized, coerce_unsized";
    let b = check(flags, MUT_REF_IN_FINAL, konst("B"));
    assert_eq!(reported(&b), [("E0764", MUT_BORROW, "&mut 4", None)]);
    let b3 = check(flags, MUT_REF_IN_FINAL, konst("B3"));
    assert_eq!(reported(&b3), [("E0764", MUT_BORROW, "&mut 42", None)]);
}

/// `tests/ui/consts/const-mut-refs/mut_ref_in_final.rs`: no mutable borrow outlives `B2`,
/// `C` and `C_NO`; `B4`'s temporary is dropped, which borrow checking refuses (E0716), not
/// these checks; a `static mut` may hold one.
#[test]
fn a_mutable_borrow_that_does_not_outlive_the_evaluation_is_accepted() {
    let flags = "sized, coerce_unsized";
    for name in ["B2", "B4", "C", "C_NO"] {
        assert_eq!(reported(&check(flags, MUT_REF_IN_FINAL, konst(name))), NONE, "{name}");
    }
    assert_eq!(reported(&check(flags, MUT_REF_IN_FINAL, statik("MUT_ARRAY"))), NONE);
}

/// `tests/ui/consts/const-mut-refs/const_mut_refs.rs`: a `const fn`'s mutable borrows, and a
/// constant's of a temporary that is dropped.
#[test]
fn mutable_borrows_that_end_are_accepted() {
    let src = "\
struct Foo {
    x: usize
}

const fn foo() -> Foo {
    Foo { x: 0 }
}

impl Foo {
    const fn bar(&mut self) -> usize {
        self.x = 1;
        self.x
    }
}

const fn baz(foo: &mut Foo) -> usize {
    let x = &mut foo.x;
    *x = 2;
    *x
}

const fn bazz(foo: &mut Foo) -> usize {
    foo.x = 3;
    foo.x
}

const BAZ: usize = baz(&mut foo());
";
    for pick in [func("baz"), func("bazz")] {
        assert_eq!(reported(&check("sized", src, pick)), NONE);
    }
    assert_eq!(reported(&check("sized", src, konst("BAZ"))), NONE);
}

/// `tests/ui/error-codes/E0492.rs`, with an `AtomicUsize` of its own: a borrow of a constant
/// whose value has interior mutability; `None` has none.
#[test]
fn a_borrow_of_a_constant_with_interior_mutability_is_refused() {
    let src = format!(
        "{OPTION}
use core::cell::UnsafeCell;

pub struct AtomicUsize {{
    v: UnsafeCell<usize>,
}}
unsafe impl Sync for AtomicUsize {{}}
impl AtomicUsize {{
    pub const fn new(v: usize) -> AtomicUsize {{
        AtomicUsize {{ v: UnsafeCell::new(v) }}
    }}
}}

const A: AtomicUsize = AtomicUsize::new(0);
const B: &'static AtomicUsize = &A;
static C: &'static AtomicUsize = &A;

const NONE: &'static Option<AtomicUsize> = &None;
"
    );
    let b = check("cell, sync", &src, konst("B"));
    assert_eq!(reported(&b), [("E0492", CELL_BORROW, "&A", None)]);
    let c = check("cell, sync", &src, statik("C"));
    assert_eq!(reported(&c), [("E0492", CELL_BORROW, "&A", None)]);
    assert_eq!(reported(&check("cell, sync", &src, konst("NONE"))), NONE);
}

/// `tests/ui/consts/refs-to-cell-in-final.rs`, without `assert!`: a borrow of a value with
/// interior mutability, by what the value is: of `None` or a variant without a cell there is
/// none.
#[test]
fn a_borrow_of_a_cell_in_the_final_value_is_refused() {
    let src = format!(
        "{OPTION}
use core::cell::{{Cell, UnsafeCell}};

struct SyncPtr<T> {{
    x: *const T,
}}
unsafe impl<T> Sync for SyncPtr<T> {{}}

static RAW_SYNC_S: SyncPtr<Cell<i32>> = SyncPtr {{ x: &Cell::new(42) }};
const RAW_SYNC_C: SyncPtr<Cell<i32>> = SyncPtr {{ x: &Cell::new(42) }};

pub enum JsValue {{
    Undefined,
    Object(Cell<bool>),
}}
impl Drop for JsValue {{
    fn drop(&mut self) {{}}
}}
const UNDEFINED: &JsValue = &JsValue::Undefined;

const NONE: &'static Option<Cell<i32>> = &None;
const NONE_EXPLICIT_PROMOTED: &'static Option<Cell<i32>> = {{
    let x = &None;
    x
}};

const INTERIOR_MUT_VARIANT: &Option<UnsafeCell<bool>> = &{{
    let mut x = None;
    x = Some(UnsafeCell::new(false));
    x
}};
"
    );
    let flags = "cell, sync";
    let s = check(flags, &src, statik("RAW_SYNC_S"));
    assert_eq!(reported(&s), [("E0492", CELL_BORROW, "&Cell::new(42)", None)]);
    let c = check(flags, &src, konst("RAW_SYNC_C"));
    assert_eq!(reported(&c), [("E0492", CELL_BORROW, "&Cell::new(42)", None)]);
    for name in ["UNDEFINED", "NONE", "NONE_EXPLICIT_PROMOTED"] {
        assert_eq!(reported(&check(flags, &src, konst(name))), NONE, "{name}");
    }
    let variant = check(flags, &src, konst("INTERIOR_MUT_VARIANT"));
    let borrow = "&{\n    let mut x = None;\n    x = Some(UnsafeCell::new(false));\n    x\n}";
    assert_eq!(reported(&variant), [("E0492", CELL_BORROW, borrow, None)]);
}

/// `tests/ui/consts/partial_qualif.rs`: assigning a field keeps the qualif of the rest.
#[test]
fn assigning_a_field_keeps_the_qualif() {
    let src = "\
use core::cell::Cell;

const FOO: &(Cell<usize>, bool) = {
    let mut a = (Cell::new(0), false);
    a.1 = true;
    &{a}
};
";
    let found = check("cell", src, konst("FOO"));
    assert_eq!(reported(&found), [("E0492", CELL_BORROW, "&{a}", None)]);
}

/// `tests/ui/consts/qualif_overwrite.rs` and `qualif_overwrite_2.rs`: assigning over a value
/// with interior mutability does not clear it, of a local or of its field.
#[test]
fn assigning_over_a_cell_keeps_the_qualif() {
    let src = format!(
        "{OPTION}
use core::cell::Cell;

const FOO: &Option<Cell<usize>> = {{
    let mut a = Some(Cell::new(0));
    a = None;
    &{{a}}
}};

const FOO_2: &Option<Cell<usize>> = {{
    let mut a = (Some(Cell::new(0)),);
    a.0 = None;
    &{{a.0}}
}};
"
    );
    let foo = check("cell", &src, konst("FOO"));
    assert_eq!(reported(&foo), [("E0492", CELL_BORROW, "&{a}", None)]);
    let foo_2 = check("cell", &src, konst("FOO_2"));
    assert_eq!(reported(&foo_2), [("E0492", CELL_BORROW, "&{a.0}", None)]);
}

/// `tests/ui/thread-local/thread-local-static.rs`, with a `swap` of its own and the use of the
/// mutable static in `unsafe` (E0133 is not these checks').
#[test]
fn a_thread_local_static_is_refused() {
    let src = "\
#![feature(thread_local)]

#[thread_local]
static mut STATIC_VAR_2: [u32; 8] = [4; 8];
const fn swap<T>(_: &mut T, _: &mut T) {}
const fn g(x: &mut [u32; 8]) {
    unsafe { swap(x, &mut STATIC_VAR_2) }
}
";
    let found = check("sized, copy", src, func("g"));
    assert_eq!(reported(&found), [("E0625", THREAD_LOCAL, "STATIC_VAR_2", None)]);
}
