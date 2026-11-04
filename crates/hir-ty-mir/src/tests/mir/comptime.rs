//! `for const` in MIR (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md, "MIR:
//! one generic body, unrolled at monomorphization").

use hir_expand::fixture;

use crate::{
    db::HirDatabase,
    mir::{MirBody, ProjectionElem, TerminatorKind},
    setup_tracing,
    test_db::TestDBExt as _,
};

/// `library/core/src/reflect.rs` reduced to what inference and lowering read.
const REFLECT: &str = r#"
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);
pub fn fields_of<T: ?Sized>() -> Fields<T> { loop {} }
pub trait Show {}
fn show<X: Show + ?Sized>(_: &X) {}
"#;

/// The one function `name` of the fixture's last file, lowered: a panic when it does not lower.
fn lowered(fixture_text: &str, name: &str, check: impl FnOnce(&MirBody<'_>)) {
    let _tracing = setup_tracing();
    let fx = fixture::db(fixture_text);
    let file_ids = fx.files().to_vec();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || {
        let file_id = *file_ids.last().unwrap();
        let module_id = db.module_for_file(file_id.file_id(&db));
        let def_map = module_id.def_map(&db);
        let scope = &def_map.module(&db, module_id).scope(&db);
        let func = scope
            .declarations()
            .find_map(|x| match x {
                hir_def::ModuleDefId::FunctionId(it)
                    if hir_def::signatures::FunctionSignature::of(&db, it).name.as_str()
                        == name =>
                {
                    Some(it)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fixture's `{name}`"));
        match db.mir_body(func.into()) {
            Ok(body) => check(&body),
            Err(e) => panic!("`{name}` does not lower: {e:?}"),
        }
    })
}

#[test]
fn a_for_const_body_is_lowered_once_generic_in_its_binder() {
    let fixture = format!(
        "//- minicore: sized, copy, clone\n#![feature(blaze_comptime, lang_items)]\n{REFLECT}\n\
         fn bounded<T>(t: &T) where for<F: Field<T>> F::Type: Show {{\n\
             for const f in fields_of::<T>() {{ show(&t.[f]); }}\n\
         }}\n"
    );
    lowered(&fixture, "bounded", |body| {
        let heads = body
            .basic_blocks
            .iter()
            .filter(|(_, it)| {
                matches!(
                    it.terminator.as_ref().map(|t| &t.kind),
                    Some(TerminatorKind::ComptimeFor { .. })
                )
            })
            .count();
        assert_eq!(heads, 1, "one loop head, not one body per field");
        let mut reflected = false;
        body.visit_places(|place| {
            reflected |= place
                .projection
                .as_slice()
                .iter()
                .any(|it| matches!(it, ProjectionElem::ReflectedField(_)));
        });
        assert!(reflected, "`t.[f]` is a reflected-field place");
    });
}

#[test]
fn a_body_without_the_feature_has_no_comptime_loop() {
    // The expansion and the lowering environment return at once for such a body: nothing of
    // the feature touches its MIR.
    lowered(
        "//- minicore: sized, iterator, range\nfn plain() { for _ in 0..3u8 {} }\n",
        "plain",
        |body| assert!(!body.has_comptime_loop()),
    );
}

/// `library/core/src/reflect.rs` and `core::field` reduced to what a `for const` printer runs:
/// the field representing type, `Field::{NAME, INDEX}` from their intrinsics, `fields_of`, and a
/// `write` to stdout.
const REFLECT_RUN: &str = r#"
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "field_representing_type"]
/// Zero-sized, as core's (`library/core/src/field.rs`): a loop's binding is its one value.
pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32> {
    _phantom: core::marker::PhantomData<fn(T) -> T>,
}
pub struct TypeId(u128);
#[rustc_intrinsic]
pub const fn type_id<T: ?Sized>() -> TypeId;
#[rustc_intrinsic]
pub const fn field_representing_type_name(id: TypeId) -> &'static str;
#[rustc_intrinsic]
pub const fn blaze_adt_name<T: ?Sized>() -> &'static str;
#[rustc_intrinsic]
pub const fn blaze_field_index<F: FieldOf>() -> usize;
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {
    const NAME: &'static str = field_representing_type_name(type_id::<Self>());
    const INDEX: usize = blaze_field_index::<Self>();
    fn name(self) -> &'static str { Self::NAME }
}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_struct"]
pub unsafe trait Struct {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);
pub const fn fields_of<T>() -> Fields<T> { Fields(0usize as *const T, ()) }
/// Whether the field `F` is before the `n`th, for an `if const` condition naming the binder.
pub const fn before<F: FieldOf>(_: F, n: usize) -> bool { blaze_field_index::<F>() < n }

extern "C" {
    pub fn write(fd: i32, buf: *const u8, count: usize) -> usize;
}
fn put(s: &str) {
    let b = s.as_bytes();
    unsafe { write(1, b as *const [u8] as *const u8, b.len()); }
}
pub trait Show { fn show(&self); }
impl Show for u8 {
    fn show(&self) {
        let digit = [b'0' + *self];
        unsafe { write(1, &digit[0], 1); }
    }
}
impl Show for bool {
    fn show(&self) { put(if *self { "true" } else { "false" }) }
}
"#;

/// The phase 2 parity test (docs/v2/plan/design/comptime.md, "Tests"): a `#[derive]`-free
/// Debug written once with `for const` prints a struct exactly as the impl written by hand
/// for it does. It runs the struct's name (`blaze_adt_name`), each field's name and value in
/// declaration order, and a separator chosen by `if const` on the field's index
/// (`blaze_field_index` in an inline const that names the binder).
#[test]
fn a_for_const_debug_prints_what_the_hand_written_one_does() {
    super::eval::check_pass_and_stdio(
        &format!(
            "//- minicore: sized, copy, clone, slice, index, coerce_unsized, str, phantom_data\n\
             #![feature(blaze_comptime, lang_items)]\n{REFLECT_RUN}\n{}",
            r#"
fn reflected<T>(t: &T) where for<F: Field<T>> F::Type: Show {
    put(const { blaze_adt_name::<T>() });
    put(" { ");
    for const f in fields_of::<T>() {
        if const { !before(f, 1) } {
            put(", ");
        }
        put(f.name());
        put(": ");
        Show::show(&t.[f]);
    }
    put(" }\n");
}

pub struct S { pub a: u8, pub b: bool }

fn by_hand(s: &S) {
    put("S");
    put(" { ");
    put("a");
    put(": ");
    Show::show(&s.a);
    put(", ");
    put("b");
    put(": ");
    Show::show(&s.b);
    put(" }\n");
}

fn main() {
    let s = S { a: 7, b: true };
    reflected(&s);
    by_hand(&s);
}
"#
        ),
        "S { a: 7, b: true }\nS { a: 7, b: true }\n",
        "",
    );
}

/// A member with bytes (a field representing type that is not core's zero-sized one) is
/// refused at monomorphization naming the member and the loop's function, never bound to a
/// value made of no bytes (which evaluation would read as an invalid constant).
#[test]
fn a_member_with_bytes_is_refused_naming_it() {
    const ZERO_SIZED: &str = "pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32> {\n    _phantom: core::marker::PhantomData<fn(T) -> T>,\n}";
    assert!(REFLECT_RUN.contains(ZERO_SIZED), "REFLECT_RUN's field representing type");
    let with_bytes = REFLECT_RUN.replace(
        ZERO_SIZED,
        "pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32>(*const T);",
    );
    super::eval::check_error_with(
        &format!(
            "//- minicore: sized, copy, clone, slice, index, coerce_unsized, str, phantom_data\n\
             #![feature(blaze_comptime, lang_items)]\n{with_bytes}\n{}",
            r#"
fn each<T>(t: &T) where for<F: Field<T>> F::Type: Show {
    for const f in fields_of::<T>() {
        Show::show(&t.[f]);
    }
}

pub struct S { pub a: u8 }

fn main() {
    each(&S { a: 1 });
}
"#
        ),
        |e| {
            let shown = format!("{e:?}");
            shown.contains(
                "the member FieldRepresentingType<S, 0_u32, 0_u32> of a `for const` loop in",
            ) && shown.contains("is 8 bytes")
        },
    );
}

/// The `if const` guarantee (docs/v2/plan/design/comptime.md, "Tests"): in each copy of a
/// `for const` body the arm its condition rules out is removed before anything evaluates it,
/// so the dead arm's `const { panic!() }` never runs and the program builds.
#[test]
fn an_if_const_dead_arm_is_never_evaluated() {
    super::eval::check_pass_and_stdio(
        &format!(
            "//- minicore: sized, copy, clone, slice, index, coerce_unsized, str, panic, phantom_data\n\
             #![feature(blaze_comptime, lang_items)]\n{REFLECT_RUN}\n{}",
            r#"
fn guarded<T>(t: &T) where for<F: Field<T>> F::Type: Show {
    for const f in fields_of::<T>() {
        if const { before(f, 2) } {
            Show::show(&t.[f]);
        } else {
            // Of type `()`, so it is a value the arm would use, not a diverging expression.
            const { if true { panic!("the dead arm's constant was evaluated") } };
        }
    }
}

pub struct S { pub a: u8, pub b: u8 }

fn main() {
    guarded(&S { a: 1, b: 2 });
}
"#
        ),
        "12",
        "",
    );
}
