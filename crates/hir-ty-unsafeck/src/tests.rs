//! Fixture tests, each a case of rustc's `tests/ui` with what its `.stderr` gives: an
//! annotation `//^^^ lint: Reason` under each operation, `first` where rustc notes that an
//! unsafe function restricts its caller.

mod db;

use base_db::{EditionedFileId, all_crates};
use hir_def::{
    AssocItemId, DefWithBodyId, ModuleDefId, expr_store::Body, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use test_utils::extract_annotations;

use crate::{InsideUnsafeBlock, UnsafeLint, UnsafeOp, unsafe_operations, unsafety_check};

use self::db::TestDB;

/// Every body of every crate: functions, consts and statics, in modules, traits and impls.
fn bodies(db: &TestDB) -> Vec<DefWithBodyId> {
    fn push(item: AssocItemId, bodies: &mut Vec<DefWithBodyId>) {
        match item {
            AssocItemId::FunctionId(it) => bodies.push(it.into()),
            AssocItemId::ConstId(it) => bodies.push(it.into()),
            AssocItemId::TypeAliasId(_) => {}
        }
    }
    let mut bodies = Vec::new();
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    ModuleDefId::TraitId(it) => {
                        for &(_, item) in it.trait_items(db).items.iter() {
                            push(item, &mut bodies);
                        }
                    }
                    _ => {}
                }
            }
            bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    push(item, &mut bodies);
                }
            }
        }
    }
    bodies
}

fn render(op: &UnsafeOp) -> String {
    let lint = match op.lint {
        UnsafeLint::HardError => "error",
        UnsafeLint::UnsafeOpInUnsafeFn => "unsafe_op_in_unsafe_fn",
        UnsafeLint::DeprecatedSafe2024 => "deprecated_safe_2024",
    };
    let mut rendered = format!("{lint}: {:?}", op.reason);
    if op.first {
        rendered.push_str(" first");
    }
    if op.lint == UnsafeLint::DeprecatedSafe2024 && op.in_unsafe_fn {
        rendered.push_str(" in_unsafe_fn");
    }
    rendered
}

/// The operations of every body, each with the file and range it is written at, in order.
fn operations(db: &TestDB) -> Vec<(EditionedFileId, syntax::TextRange, String)> {
    let mut found = Vec::new();
    for def in bodies(db) {
        let (_, source_map) = Body::with_source_map(db, def);
        for op in unsafety_check(db, def).ops() {
            match op.lint {
                UnsafeLint::UnsafeOpInUnsafeFn => assert!(op.in_unsafe_fn, "{op:?}"),
                UnsafeLint::HardError => assert!(!op.in_unsafe_fn && !op.first, "{op:?}"),
                UnsafeLint::DeprecatedSafe2024 => assert!(!op.first, "{op:?}"),
            }
            let node = source_map
                .expr_or_pat_syntax(op.node())
                .unwrap_or_else(|_| panic!("{def:?}: {op:?} is not written anywhere"));
            let range = node.map(|it| it.text_range()).original_node_file_range_rooted(db);
            found.push((range.file_id, range.range, render(op)));
        }
    }
    found
}

#[track_caller]
fn check(#[rust_analyzer::rust_fixture] fixture: &str) {
    let fx = fixture::db(fixture);
    let files = fx.files().to_vec();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let found = operations(&db);
        for file in files {
            let text = db.file_text(file.file_id(&db)).text(&db);
            let mut expected = extract_annotations(text);
            let mut actual: Vec<_> = found
                .iter()
                .filter(|(at, ..)| *at == file)
                .map(|(_, range, rendered)| (*range, rendered.clone()))
                .collect();
            expected.sort_by_key(|(range, text)| (range.start(), range.end(), text.clone()));
            actual.sort_by_key(|(range, text)| (range.start(), range.end(), text.clone()));
            assert_eq!(actual, expected, "\n{text}");
        }
    });
}

/// The source of each operation of the one file, in the order the check gives them.
#[track_caller]
fn order(#[rust_analyzer::rust_fixture] fixture: &str) -> Vec<String> {
    let fx = fixture::db(fixture);
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let text = db.file_text(file.file_id(&db)).text(&db);
        operations(&db)
            .into_iter()
            .filter(|(at, ..)| *at == file)
            .map(|(_, range, _)| text[range].to_owned())
            .collect()
    })
}

// tests/ui/unsafe-fields/unsafe-fields.rs
#[test]
fn unsafe_fields() {
    check(
        r#"
struct WithUnsafeField {
    unsafe unsafe_field: u32,
    safe_field: u32,
}

enum A {
    WithUnsafeField { unsafe unsafe_field: u32, safe_field: u32 },
}

fn f(a: A) {
    let A::WithUnsafeField { unsafe_field, safe_field } = a;
                           //^^^^^^^^^^^^ error: UseOfUnsafeField
}

impl WithUnsafeField {
    fn new() -> WithUnsafeField {
        unsafe { WithUnsafeField { unsafe_field: 0, safe_field: 0 } }
    }

    fn new_without_unsafe() -> WithUnsafeField {
        WithUnsafeField { unsafe_field: 0, safe_field: 0 }
      //^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ error: InitializingTypeWithUnsafeField
    }

    fn operate_on_safe_field(&mut self) {
        self.safe_field = 2;
        &self.safe_field;
        self.safe_field;
    }

    fn set_unsafe_field(&mut self) {
        unsafe {
            self.unsafe_field = 2;
        }
    }

    fn read_unsafe_field(&self) -> u32 {
        unsafe { self.unsafe_field }
    }

    fn ref_unsafe_field(&self) -> &u32 {
        unsafe { &self.unsafe_field }
    }

    fn destructure(&self) {
        unsafe {
            let Self { safe_field, unsafe_field } = self;
        }
    }

    fn set_unsafe_field_without_unsafe(&mut self) {
        self.unsafe_field = 2;
      //^^^^^^^^^^^^^^^^^ error: UseOfUnsafeField
    }

    fn read_unsafe_field_without_unsafe(&self) -> u32 {
        self.unsafe_field
      //^^^^^^^^^^^^^^^^^ error: UseOfUnsafeField
    }

    fn ref_unsafe_field_without_unsafe(&self) -> &u32 {
        &self.unsafe_field
       //^^^^^^^^^^^^^^^^^ error: UseOfUnsafeField
    }

    fn destructure_without_unsafe(&self) {
        let Self { safe_field, unsafe_field } = self;
                             //^^^^^^^^^^^^ error: UseOfUnsafeField
        let WithUnsafeField { safe_field, .. } = self;
    }

    fn raw_const(&self) -> *const u32 {
        &raw const self.unsafe_field
                 //^^^^^^^^^^^^^^^^^ error: UseOfUnsafeField
    }
}
"#,
    );
}

// tests/ui/unsafe-binders: each way of the cast needs `unsafe`, and its operand is walked.
#[test]
fn unsafe_binder_cast() {
    check(
        r#"
static mut N: i32 = 0;

fn main() {
    let x = &1i32;
    builtin # wrap_binder(x);
  //^^^^^^^^^^^^^^^^^^^^^^^^ error: UnsafeBinderCast
    builtin # unwrap_binder(x, &i32);
  //^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ error: UnsafeBinderCast
    unsafe { builtin # wrap_binder(x) };
}

unsafe fn in_unsafe_fn(x: &i32) {
    builtin # wrap_binder(N);
  //^^^^^^^^^^^^^^^^^^^^^^^^ unsafe_op_in_unsafe_fn: UnsafeBinderCast first
                        //^ unsafe_op_in_unsafe_fn: MutableStatic
}
"#,
    );
}

// tests/ui/unsafe/unsafe_op_in_unsafe_fn/rfc-2585-unsafe_op_in_unsafe_fn.rs
#[test]
fn unsafe_op_in_unsafe_fn() {
    check(
        r#"
unsafe fn unsf() {}
const PTR: *const () = 0 as *const ();
static mut VOID: () = ();

unsafe fn deny_level() {
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    *PTR;
  //^^^^ unsafe_op_in_unsafe_fn: RawPtrDeref
    VOID = ();
  //^^^^ unsafe_op_in_unsafe_fn: MutableStatic
}

unsafe fn explicit_block() {
    unsafe {
        unsf();
        *PTR;
        VOID = ();
    }
}

unsafe fn two_explicit_blocks() {
    unsafe { unsafe { unsf() } }
}

fn main() {
    unsf();
  //^^^^^^ error: UnsafeFnCall
}
"#,
    );
}

// tests/ui/unsafe/unsafe_op_in_unsafe_fn/wrapping-unsafe-block-sugg.rs, and a closure starting
// from the flag where it is written.
#[test]
fn first_of_each_body() {
    check(
        r#"
unsafe fn unsf() {}

unsafe fn foo() {
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
}

unsafe fn bar(x: *const i32) -> i32 {
    let y = *x;
          //^^ unsafe_op_in_unsafe_fn: RawPtrDeref first
    y + *x
      //^^ unsafe_op_in_unsafe_fn: RawPtrDeref
}

static mut BAZ: i32 = 0;
unsafe fn baz() -> i32 {
    let y = BAZ;
          //^^^ unsafe_op_in_unsafe_fn: MutableStatic first
    y + BAZ
      //^^^ unsafe_op_in_unsafe_fn: MutableStatic
}

unsafe fn closures() {
    let f = || unsf();
             //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    let g = || unsf();
             //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
    unsafe {
        let h = || unsf();
    }
}
"#,
    );
}

// rustc's `requires_unsafe`: where `unsafe_op_in_unsafe_fn` is allowed at the operation (its
// level at the innermost expression, then the statements and items around it), nothing is
// linted and `suggest_unsafe_block` stays set for the next one.
#[test]
fn an_allowed_operation_leaves_the_next_first() {
    check(
        r#"
unsafe fn unsf() {}

unsafe fn block(x: *const i32) -> i32 {
    #[allow(unsafe_op_in_unsafe_fn)]
    {
        unsf();
      //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
    }
    let y = *x;
          //^^ unsafe_op_in_unsafe_fn: RawPtrDeref first
    y + *x
      //^^ unsafe_op_in_unsafe_fn: RawPtrDeref
}

unsafe fn statement(x: *const i32) -> i32 {
    #[allow(unsafe_op_in_unsafe_fn)]
    let y = *x;
          //^^ unsafe_op_in_unsafe_fn: RawPtrDeref
    #[expect(unsafe_op_in_unsafe_fn)]
    let z = *x;
          //^^ unsafe_op_in_unsafe_fn: RawPtrDeref first
    y + z + *x
          //^^ unsafe_op_in_unsafe_fn: RawPtrDeref
}

#[allow(unsafe_op_in_unsafe_fn)]
unsafe fn item() {
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
    #[deny(unsafe_op_in_unsafe_fn)]
    {
        unsf();
      //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    }
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
}

unsafe fn closure() {
    let f = || {
        #[allow(unsafe_op_in_unsafe_fn)]
        {
            unsf();
          //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
        }
        unsf();
      //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    };
    unsf();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
}
"#,
    );
}

// tests/ui/asm/x86_64/goto.rs: a label block is safe, in an `unsafe fn` too.
#[test]
fn asm_label_is_safe_in_unsafe_fn() {
    check(
        r#"
//- minicore: asm
unsafe fn foo() {
    unsafe {
        core::arch::asm!(
            "jmp {}",
            label {
                let p = 0xDEADBEAF as *mut u8;
                *p = 3;
              //^^ error: RawPtrDeref
            },
        );
    }
}
"#,
    );
}

// tests/ui/rust-2024/unsafe-env.rs, both editions.
#[test]
fn deprecated_safe_2024() {
    check(
        r#"
//- /ed2021.rs crate:ed2021 edition:2021
#[rustc_deprecated_safe_2024(audit_that = "the environment is only accessed by one thread")]
pub unsafe fn set_var() {}

//- /e2021.rs crate:e2021 deps:ed2021 edition:2021
unsafe fn unsafe_fn() {
    ed2021::set_var();
  //^^^^^^^^^^^^^^^^^ deprecated_safe_2024: UnsafeFnCall in_unsafe_fn
    if false {
        unsafe_fn();
      //^^^^^^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
    }
}

fn main() {
    ed2021::set_var();
  //^^^^^^^^^^^^^^^^^ deprecated_safe_2024: UnsafeFnCall
    unsafe {
        ed2021::set_var();
    }
    unsafe_fn();
  //^^^^^^^^^^^ error: UnsafeFnCall
}

//- /e2024.rs crate:e2024 deps:ed2021 edition:2024
unsafe fn unsafe_fn() {
    ed2021::set_var();
  //^^^^^^^^^^^^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
    if false {
        unsafe_fn();
      //^^^^^^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall
    }
}

fn main() {
    ed2021::set_var();
  //^^^^^^^^^^^^^^^^^ error: UnsafeFnCall
    unsafe {
        ed2021::set_var();
    }
    unsafe_fn();
  //^^^^^^^^^^^ error: UnsafeFnCall
}
"#,
    );
}

// tests/ui/rfcs/rfc-2396-target_feature-11/safe-calls.rs
#[test]
fn target_feature_calls() {
    check(
        r#"
//- /main.rs crate:main cfg:target_arch=x86_64
#[target_feature(enable = "sse2")]
const fn sse2() {}

#[target_feature(enable = "sse2")]
#[target_feature(enable = "fxsr")]
const fn sse2_and_fxsr() {}

#[target_feature(enable = "avx")]
#[target_feature(enable = "bmi2")]
fn avx_bmi2() {}

struct Quux;

impl Quux {
    #[target_feature(enable = "avx")]
    #[target_feature(enable = "bmi2")]
    fn avx_bmi2(&self) {}
}

fn foo() {
    sse2();
  //^^^^^^ error: UnsafeFnCall
    avx_bmi2();
  //^^^^^^^^^^ error: UnsafeFnCall
    Quux.avx_bmi2();
  //^^^^^^^^^^^^^^^ error: UnsafeFnCall
}

#[target_feature(enable = "sse2")]
fn bar() {
    sse2();
    avx_bmi2();
  //^^^^^^^^^^ error: UnsafeFnCall
    Quux.avx_bmi2();
  //^^^^^^^^^^^^^^^ error: UnsafeFnCall
}

#[target_feature(enable = "avx")]
fn baz() {
    sse2();
    avx_bmi2();
  //^^^^^^^^^^ error: UnsafeFnCall
    Quux.avx_bmi2();
  //^^^^^^^^^^^^^^^ error: UnsafeFnCall
}

#[target_feature(enable = "avx")]
#[target_feature(enable = "bmi2")]
fn qux() {
    sse2();
    avx_bmi2();
    Quux.avx_bmi2();
}

const _: () = sse2();
            //^^^^^^ error: UnsafeFnCall

const _: () = sse2_and_fxsr();
            //^^^^^^^^^^^^^^^ error: UnsafeFnCall

unsafe fn needs_unsafe_block() {
    sse2();
  //^^^^^^ unsafe_op_in_unsafe_fn: UnsafeFnCall first
}
"#,
    );
}

// rustc's `is_target_feature_call_safe` implies features by the target's table alone: `aes`
// is `sse2` on x86 and `neon` on AArch64.
#[test]
fn target_feature_implications_are_the_targets() {
    check(
        r#"
//- /x86.rs crate:x86 cfg:target_arch=x86_64
#[target_feature(enable = "neon")]
fn neon() {}

#[target_feature(enable = "sse2")]
fn sse2() {}

#[target_feature(enable = "aes")]
fn aes() {
    neon();
  //^^^^^^ error: UnsafeFnCall
    sse2();
}

//- /arm.rs crate:arm cfg:target_arch=aarch64
#[target_feature(enable = "neon")]
fn neon() {}

#[target_feature(enable = "sse2")]
fn sse2() {}

#[target_feature(enable = "aes")]
fn aes() {
    neon();
    sse2();
  //^^^^^^ error: UnsafeFnCall
}
"#,
    );
}

#[test]
fn target_feature_calls_are_safe_on_wasm() {
    check(
        r#"
//- target_arch: wasm32
#[target_feature(enable = "simd128")]
fn requires_target_feature() {}

fn main() {
    requires_target_feature();
}
"#,
    );
}

// tests/ui/pin-ergonomics/pinned-drop-unsafety-check.rs
#[test]
fn call_drop_explicitly() {
    check(
        r#"
#![feature(lang_items)]
#[lang = "drop"]
trait Drop {
    fn drop(&mut self) {
        Self::pin_drop(self);
      //^^^^^^^^^^^^^^^^^^^^ error: CallDropExplicitly
        unsafe { Self::pin_drop(self) };
    }

    fn pin_drop(&mut self) {}
}
"#,
    );
}

// tests/ui/union/union-unsafe.rs
#[test]
fn union_fields_and_patterns() {
    check(
        r#"
union U { a: u32, b: u32 }

fn reads(u: U) {
    let _x = u.a;
           //^^^ error: UnionField
    let U { a } = u;
          //^ error: UnionField
    let U { b: _ } = u;
    match u {
        U { b: 0 } => {}
             //^ error: UnionField
        _ => {}
    }
}

fn writes(mut u: U) {
    u.a = 1;
    u.b += 1;
  //^^^ error: UnionField
    let _x = &raw const u.a;
    let _y = &raw mut u.b;
}

fn params(U { a }: U) {}
            //^ error: UnionField

unsafe fn unsafe_params(U { a }: U) {}
                          //^ unsafe_op_in_unsafe_fn: UnionField first
"#,
    );
}

// THIR's order: a `let`'s initializer, then its pattern; an arm's guard, then its pattern.
#[test]
fn order_is_thirs() {
    let found = order(
        r#"
union U { a: u32, b: u32 }

fn f(u: U) {
    let U { a } = U { b: u.b };
    match u {
        U { b } if u.a == 0 => {}
        _ => {}
    }
}
"#,
    );
    assert_eq!(found, ["u.b", "a", "u.a", "b"]);
}

// tests/ui/static/raw-ref-deref-without-unsafe.rs, tests/ui/extern/extern-static-safe.rs
#[test]
fn statics() {
    check(
        r#"
static mut M: u32 = 0;
static S: u32 = 0;
extern "C" {
    static E: u32;
}
unsafe extern "C" {
    safe static SAFE: u32;
    static UNSAFE: u32;
}

fn f() -> u32 {
    let _a = &raw const M;
    let _b = &raw const E;
    let _c = S + SAFE;
    let _d = M;
           //^ error: MutableStatic
    let _e = E;
           //^ error: ExternStatic
    UNSAFE
  //^^^^^^ error: ExternStatic
}
"#,
    );
}

#[test]
fn calls() {
    check(
        r#"
unsafe fn unsf() {}
extern "C" {
    fn ext();
}
unsafe extern "C" {
    safe fn safe_ext();
}

fn f() {
    let p: unsafe fn() = unsf;
    p();
  //^^^ error: UnsafeFnCall
    ext();
  //^^^^^ error: UnsafeFnCall
    safe_ext();
    let q: fn() = f;
    q();
}
"#,
    );
}

// A repeat count is an anonymous const of its own, safe inside `unsafe` too; an inline const is
// checked with its body.
#[test]
fn repeat_counts_and_inline_consts() {
    check(
        r#"
const unsafe fn uf() -> usize { 1 }

fn f() {
    let _a = unsafe { [0u8; uf()] };
                          //^^^^ error: UnsafeFnCall
    let _b = unsafe { const { uf() } };
    let _c = const { uf() };
                   //^^^^ error: UnsafeFnCall
}
"#,
    );
}

// tests/ui/unsafe/ranged_ints.rs, ranged_ints2.rs, ranged_ints3.rs, ranged_ints3_match.rs and
// ranged_ints4.rs, with an autoref, a compound assignment and a deref that ends the place.
#[test]
fn layout_constrained_fields() {
    check(
        r#"
//- minicore: cell, option
use core::cell::UnsafeCell;

#[rustc_layout_scalar_valid_range_start(1)]
#[repr(transparent)]
pub(crate) struct NonZero<T>(pub(crate) T);

trait Bump {
    fn bump(&mut self);
}

impl Bump for u32 {
    fn bump(&mut self) {}
}

fn init() {
    let _x = NonZero(0);
           //^^^^^^^^^^ error: InitializingTypeWith
    let _y = unsafe { NonZero(1) };
}

fn mutation(mut x: NonZero<u32>, mut p: NonZero<&'static mut u32>) {
    let _y = &mut x.0;
           //^^^^^^^^ error: MutationOfLayoutConstrainedField
    x.0 = 0;
  //^^^^^^^ error: MutationOfLayoutConstrainedField
    x.0 += 1;
  //^^^^^^^^ error: MutationOfLayoutConstrainedField
    x.0.bump();
  //^^^ error: MutationOfLayoutConstrainedField
    let _r = &x.0;
    let _z = &mut *p.0;
    let NonZero(&mut ref mut _w) = p;
    if let Some(NonZero(ref mut y)) = Some(x) {}
                      //^^^^^^^^^ error: MutationOfLayoutConstrainedField
}

fn borrow(x: NonZero<UnsafeCell<u32>>, y: NonZero<u32>) {
    let _y = &x.0;
           //^^^^ error: BorrowOfLayoutConstrainedField
    match x {
        NonZero(ref x) => {}
              //^^^^^ error: BorrowOfLayoutConstrainedField
    }
    match y {
        NonZero(ref y) => {}
    }
}
"#,
    );
}

#[test]
fn operations_for_an_editor() {
    let fx = fixture::db(
        r#"
unsafe fn unsf() {}

fn f() {
    unsf();
    unsafe { unsf(); }
}
"#,
    );
    let file = fx.only_file();
    let db = fx.into_db();
    hir_ty::attach_db(&db, || {
        let text = db.file_text(file.file_id(&db)).text(&db);
        let named_f = |def: &DefWithBodyId| match *def {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(&db, it).name.as_str() == "f",
            _ => false,
        };
        let def = bodies(&db).into_iter().find(named_f).expect("`f` in the fixture");
        let (body, source_map) = Body::with_source_map(&db, def);
        let infer = hir_ty::InferenceResult::of(&db, def);
        let mut found = Vec::new();
        unsafe_operations(&db, infer, def.into(), body, body.root_expr(), &mut |node, inside| {
            let node = source_map.expr_or_pat_syntax(node).expect("written in the file");
            found.push((text[node.value.text_range()].to_owned(), inside));
        });
        let expected = [
            ("unsf()".to_owned(), InsideUnsafeBlock::No),
            ("unsf()".to_owned(), InsideUnsafeBlock::Yes),
        ];
        assert_eq!(found, expected);
    });
}
