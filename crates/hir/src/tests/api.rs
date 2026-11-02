//! The hir API the checks of items read: a function's signature, a variant's fields with the
//! type's arguments, unused parameters, where-clauses, E0053's comparison with the trait, a
//! proc macro's signature, a closure's source, deprecation and the body borrowck skipped.

use base_db::Db;
use hir_def::hir::Expr;
use hir_ty::{InferBodyId, InferenceResult};
use hir_expand::fixture;

use super::{db::TestDB, kinds};
use crate::{
    Adt, AssocItem, Crate, DisplayTarget, Function, HasAttrs, HirDisplay, Impl, ModuleDef,
    SignatureMismatchAt, Type, view::HasSourceView as _,
};

/// The item named `name` at the root of a crate of the fixture.
fn item(db: &TestDB, name: &str) -> ModuleDef {
    Crate::all(db)
        .into_iter()
        .flat_map(|krate| krate.root_module(db).declarations_iter(db))
        .find(|it| it.name(db).is_some_and(|it| it.as_str() == name))
        .unwrap_or_else(|| panic!("no item `{name}` in the fixture"))
}

fn func(db: &TestDB, name: &str) -> Function {
    match item(db, name) {
        ModuleDef::Function(it) => it,
        it => panic!("`{name}` is {it:?}"),
    }
}

/// The method `name` of a trait impl of the fixture.
fn impl_method(db: &TestDB, name: &str) -> Function {
    Crate::all(db)
        .into_iter()
        .flat_map(|krate| Impl::all_in_crate(db, krate))
        .flat_map(|it| it.items_iter(db))
        .find_map(|it| match it {
            AssocItem::Function(it) if it.name(db).as_str() == name => Some(it),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no impl method `{name}` in the fixture"))
}

fn target(db: &TestDB, f: Function) -> DisplayTarget {
    f.module(db).krate(db).to_display_target(db)
}

fn show<'db>(db: &'db Db, ty: &Type<'db>, target: DisplayTarget) -> String {
    ty.display(db, target).to_string()
}

const ITEMS: &str = r#"
//- minicore: sized, copy
//- /main.rs crate:main
struct Vec<T>(T);
enum E<T> { A(Vec<T>), B }
fn takes_e<T>(e: E<T>) {}
struct S<const N: usize>([S<N>; N]);
fn takes_s<const N: usize>(s: S<N>) {}
struct Unused<T, U>(U);
trait Tr { type Out; }
struct Constrained<T, U>(U) where U: Tr<Out = T>;
fn bounded<T: Copy>() where T: Tr {}
extern "C" { type Opaque; }
fn takes_opaque(o: Opaque) {}
"#;

#[test]
fn a_variant_s_fields_have_the_type_s_arguments() {
    let db = fixture::db(ITEMS).into_db();
    crate::attach_db(&db, || {
        let f = func(&db, "takes_e");
        let sig = f.signature(&db).expect("a function has a signature");
        let params = sig.params(&db);
        assert_eq!(params.len(), 1);
        let ModuleDef::Adt(Adt::Enum(e)) = item(&db, "E") else { panic!("`E` is an enum") };
        let a = e.variants(&db).into_iter().find(|it| it.name(&db).as_str() == "A").unwrap();
        let fields = params[0].variant_fields(&db, a);
        let shown: Vec<_> =
            fields.iter().map(|(_, ty)| show(&db, ty, target(&db, f))).collect();
        assert_eq!(shown, ["Vec<T>"]);

        let round = params[0].derived_from_next_solver(params[0].to_next_solver());
        assert_eq!(round, params[0], "a type comes back from the next solver's the same");
    });
}

#[test]
fn a_direct_tag_enum_gives_its_tag_and_variant_sizes_in_order() {
    let db = fixture::db(
        r#"
//- minicore: sized
//- /main.rs crate:main
#[repr(u8)]
enum Direct { A(u32), B, C([u8; 9]) }
enum Niche { A(&'static u8), B }
enum One { A(u32) }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let layout = |name: &str| {
            let ModuleDef::Adt(adt @ Adt::Enum(_)) = item(&db, name) else {
                panic!("`{name}` is an enum")
            };
            adt.layout(&db).expect("a layout")
        };
        let direct = layout("Direct");
        let (tag, sizes) =
            direct.direct_tag_variant_sizes().expect("`repr(u8)` stores the tag directly");
        let sizes: Vec<u64> = sizes.collect();
        assert_eq!(tag, 1);
        assert_eq!(sizes.len(), 3, "one size per variant: {sizes:?}");
        assert!(sizes[1] < sizes[0] && sizes[0] < sizes[2], "declaration order: {sizes:?}");
        let size = direct.size();
        assert!(sizes.iter().all(|&it| tag <= it && it <= size), "{sizes:?} in {size}");
        assert_eq!(direct.enum_tag_size(), Some(1));

        assert!(layout("Niche").direct_tag_variant_sizes().is_none(), "a niche holds `Niche`'s tag");
        assert!(layout("One").direct_tag_variant_sizes().is_none(), "one variant has no tag");
    });
}

#[test]
fn an_array_s_element_whatever_its_length() {
    let db = fixture::db(ITEMS).into_db();
    crate::attach_db(&db, || {
        let f = func(&db, "takes_s");
        let s = f.signature(&db).unwrap().params(&db).remove(0);
        let (_, field) = s.fields(&db).remove(0);
        let element = field.as_array_element(&db).expect("`[S<N>; N]` is an array");
        assert_eq!(show(&db, &element, target(&db, f)), "S<N>");
        assert_eq!(field.as_array(&db), None, "its length is a parameter");
    });
}

#[test]
fn a_parameter_no_field_or_projection_uses_is_unused() {
    let db = fixture::db(ITEMS).into_db();
    crate::attach_db(&db, || {
        let unused = |name: &str| -> Vec<String> {
            let ModuleDef::Adt(adt) = item(&db, name) else { panic!("`{name}` is an ADT") };
            adt.unused_params(&db).iter().map(|it| it.name(&db).as_str().to_owned()).collect()
        };
        assert_eq!(unused("Unused"), ["T"]);
        assert_eq!(unused("Constrained"), Vec::<String>::new(), "`Out = T` constrains `T`");
        assert_eq!(unused("E"), Vec::<String>::new());
    });
}

#[test]
fn where_predicates_are_the_written_ones() {
    let db = fixture::db(ITEMS).into_db();
    crate::attach_db(&db, || {
        let f = func(&db, "bounded");
        let preds = crate::GenericDef::Function(f).where_predicates(&db);
        let traits: Vec<_> = preds
            .iter()
            .map(|it| it.as_trait().expect("a trait bound").trait_ref().trait_().name(&db).as_str().to_owned())
            .collect();
        assert_eq!(traits, ["Copy", "Tr", "Sized"], "the written ones, then the implicit `Sized`");
        for pred in &preds {
            let types: Vec<_> =
                pred.types().iter().map(|it| show(&db, it, target(&db, f))).collect();
            assert_eq!(types, ["T"]);
        }
    });
}

#[test]
fn a_foreign_type_is_its_alias() {
    let db = fixture::db(ITEMS).into_db();
    crate::attach_db(&db, || {
        let f = func(&db, "takes_opaque");
        let ty = f.signature(&db).unwrap().params(&db).remove(0);
        let alias = ty.as_foreign(&db).expect("`Opaque` is a foreign type");
        assert_eq!(alias.name(&db).as_str(), "Opaque");
    });
}

/// rustc's `tests/ui/mismatched_types/E0053.rs`, a return type, and a projection that
/// normalizes to the impl's type.
const E0053: &str = r#"
//- minicore: sized
trait Foo {
    fn foo(x: u16);
    fn bar(&self);
    fn ret() -> u16;
    fn same(x: <u8 as Assoc>::Out);
}
trait Assoc { type Out; }
impl Assoc for u8 { type Out = u32; }
struct Bar;
impl Foo for Bar {
    fn foo(x: i16) {}
    fn bar(&mut self) {}
    fn ret() -> u8 { 0 }
    fn same(x: u32) {}
}
"#;

#[test]
fn an_impl_method_s_signature_is_compared_with_the_trait_s() {
    let db = fixture::db(E0053).into_db();
    crate::attach_db(&db, || {
        let foo = impl_method(&db, "foo");
        let it = foo.impl_signature_mismatch(&db).expect("`i16` is not `u16`: E0053");
        assert_eq!(it.at, SignatureMismatchAt::Param(0));
        assert_eq!(it.trait_method.name(&db).as_str(), "foo");
        let (expected, found) = it.types.expect("a parameter's types");
        assert_eq!(show(&db, &expected, target(&db, foo)), "u16");
        assert_eq!(show(&db, &found, target(&db, foo)), "i16");

        let bar = impl_method(&db, "bar").impl_signature_mismatch(&db).expect("E0053");
        assert_eq!(bar.at, SignatureMismatchAt::ParamMutability(0), "`&mut Bar` is not `&Bar`");
        assert_eq!(bar.types, None);

        let ret = impl_method(&db, "ret").impl_signature_mismatch(&db).expect("E0053");
        assert_eq!(ret.at, SignatureMismatchAt::Return);

        let same = impl_method(&db, "same");
        assert_eq!(same.impl_signature_mismatch(&db), None, "`<u8 as Assoc>::Out` is `u32`");
        let (trait_method, sig) = same.trait_signature_in_impl(&db).unwrap();
        assert_eq!(trait_method.name(&db).as_str(), "same");
        assert_eq!(show(&db, &sig.params(&db)[0], target(&db, same)), "<u8 as Assoc>::Out");
    });
}

const PROC_MACRO: &str = r#"
//- minicore: sized
//- /proc_macro.rs crate:proc_macro
#![feature(rustc_attrs)]
#[rustc_diagnostic_item = "TokenStream"]
pub struct TokenStream;
//- /main.rs crate:main deps:proc_macro
use proc_macro::TokenStream;
struct Text;
fn good(x: TokenStream) -> TokenStream { x }
fn attribute(x: TokenStream, y: TokenStream) -> TokenStream { x }
fn bad(x: Text) -> TokenStream { TokenStream }
unsafe fn not_safe(x: TokenStream) -> TokenStream { x }
"#;

#[test]
fn a_proc_macro_s_signature_is_fn_token_stream_to_token_stream() {
    let db = fixture::db(PROC_MACRO).into_db();
    crate::attach_db(&db, || {
        let at = |name, attribute| {
            func(&db, name).proc_macro_signature_mismatch(&db, attribute).map(|it| it.at)
        };
        assert_eq!(at("good", false), None);
        assert_eq!(at("attribute", true), None);
        assert_eq!(at("good", true), Some(SignatureMismatchAt::ParamCount));
        assert_eq!(at("bad", false), Some(SignatureMismatchAt::Param(0)), "`Text` is no stream");
        assert_eq!(at("not_safe", false), Some(SignatureMismatchAt::Signature));
        let bad = func(&db, "bad").proc_macro_signature_mismatch(&db, false).unwrap();
        let shown = |sig| {
            let params = crate::FnSignature::params(&sig, &db);
            show(&db, &params[0], target(&db, func(&db, "bad")))
        };
        assert_eq!((shown(bad.expected), shown(bad.found)), ("TokenStream".into(), "Text".into()));

        let main = func(&db, "good").module(&db).krate(&db);
        let ModuleDef::Adt(token_stream) = item(&db, "TokenStream") else { panic!() };
        let name = crate::Symbol::intern("TokenStream");
        assert_eq!(main.diagnostic_item_adt(&db, &name), Some(token_stream));
    });
}

#[test]
fn a_closure_s_source_is_its_expression() {
    let db = fixture::db(
        r#"
//- minicore: sized, fn
fn f() {
    let c = |x: u8| x;
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let f = func(&db, "f");
        let crate::AnyFunctionId::FunctionId(id) = f.id else { panic!() };
        let def = hir_def::DefWithBodyId::FunctionId(id);
        let body = hir_def::expr_store::Body::of(&db, def);
        let (closure, _) =
            body.exprs().find(|(_, it)| matches!(it, Expr::Closure { .. })).expect("a closure");
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        let ty = infer.type_of_expr_or_pat(closure.into()).expect("the closure has a type");
        let ty = Type::new(&db, id.into(), ty);
        let source = ty.as_closure().expect("a closure type").source_view(&db).expect("a source");
        assert_eq!(source.value.to_string(), "|x: u8| x");
    });
}

#[test]
fn a_closure_s_kind_origin_place_is_the_use_s_not_the_capture_s() {
    let db = fixture::db(
        r#"
//- minicore: sized, fn
struct S { a: u8 }
fn f(r: &mut S, mut t: (u8, u8)) {
    let mut moved = move || { r.a = 1; };
    let mut borrowed = || { t.1 = 2; };
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let f = func(&db, "f");
        let crate::AnyFunctionId::FunctionId(id) = f.id else { panic!() };
        let def = hir_def::DefWithBodyId::FunctionId(id);
        let body = hir_def::expr_store::Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        let places: Vec<_> = body
            .exprs()
            .filter(|(_, it)| matches!(it, Expr::Closure { .. }))
            .map(|(closure, _)| {
                let ty = infer.type_of_expr_or_pat(closure.into()).expect("the closure has a type");
                let closure = Type::new(&db, id.into(), ty).as_closure().expect("a closure type");
                let (_, capture) = closure.kind_origin(&db).expect("an `FnMut` closure's origin");
                let place = closure.kind_origin_place(&db).expect("the origin's place");
                (place, capture.display_place_source_code(&db, span::Edition::CURRENT))
            })
            .collect();
        // The `move` closure captures `r` itself; rustc labels the use's `(*r).a` as `*r.a`.
        assert_eq!(
            places,
            [("*r.a".to_owned(), "r".to_owned()), ("t.1".to_owned(), "t.1".to_owned())]
        );
    });
}

#[test]
fn deprecation_is_its_own_or_its_module_s() {
    let db = fixture::db(
        r#"
//- /main.rs crate:main
#[deprecated(note = "old")]
mod old {
    pub fn inside() {}
}
#[deprecated(since = "1.0.0")]
pub fn own() {}
pub fn current() {}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let note = |def: ModuleDef| {
            let entry = def.deprecation(&db)?;
            Some(entry.deprecation.note.as_ref().map(|it| it.as_str().to_owned()))
        };
        assert_eq!(note(item(&db, "own")), Some(None));
        assert_eq!(note(item(&db, "current")), None);
        let ModuleDef::Module(old) = item(&db, "old") else { panic!("`old` is a module") };
        let inside = old.declarations(&db).remove(0);
        assert_eq!(note(inside), Some(Some("old".to_owned())), "inherited from `old`");
        assert_eq!(item(&db, "own").stability(&db), None, "no `staged_api`");
    });
}

#[test]
fn a_body_borrowck_skipped_is_always_reported() {
    // MIR lowering refuses `twice!(n += 1;)` inside another call's parentheses (#154).
    let db = fixture::db(
        r#"
//- /main.rs crate:main
macro_rules! twice {
    ($($body:tt)*) => {
        $($body)*
        $($body)*
    };
}
fn main() {
    let mut n = 0u32;
    twice!(twice!(n += 1;));
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let found = crate::check::check_workspace(&db, false);
        let skipped: usize = found
            .iter()
            .map(|(_, it)| kinds(it).iter().filter(|it| *it == "BorrowckSkipped").count())
            .sum();
        assert_eq!(skipped, 1, "with no environment variable: {found:?}");
    });
}

/// `Adjust::from` gives what `Semantics::expr_adjustments` and the body analysis's did each
/// on their own before it.
#[test]
fn adjustments_convert_as_before() {
    use hir_def::type_ref::Mutability as Hir;
    use hir_ty::{
        PointerCast,
        infer::{AllowTwoPhase, AutoBorrowMutability},
        next_solver::Mutability as Ty,
    };

    use crate::{Adjust, AutoBorrow, OverloadedDeref};

    let old = |adjust: hir_ty::Adjust| -> Adjust {
        let mutability = |m| match m {
            Ty::Not => Hir::Shared,
            Ty::Mut => Hir::Mut,
        };
        match adjust {
            hir_ty::Adjust::NeverToAny => Adjust::NeverToAny,
            hir_ty::Adjust::Deref(Some(hir_ty::OverloadedDeref(m))) => {
                Adjust::Deref(Some(OverloadedDeref(mutability(m))))
            }
            hir_ty::Adjust::Deref(None) => Adjust::Deref(None),
            hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::RawPtr(m)) => {
                Adjust::Borrow(AutoBorrow::RawPtr(mutability(m)))
            }
            hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::Ref(m)) => {
                Adjust::Borrow(AutoBorrow::Ref(mutability(m.into())))
            }
            hir_ty::Adjust::Pointer(pc) => Adjust::Pointer(pc),
        }
    };
    let mut cases = vec![
        hir_ty::Adjust::NeverToAny,
        hir_ty::Adjust::Deref(None),
        hir_ty::Adjust::Pointer(PointerCast::ReifyFnPointer),
        hir_ty::Adjust::Pointer(PointerCast::Unsize),
        hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::Ref(AutoBorrowMutability::Not)),
    ];
    for two_phase in [AllowTwoPhase::Yes, AllowTwoPhase::No] {
        cases.push(hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::Ref(AutoBorrowMutability::Mut {
            allow_two_phase_borrow: two_phase,
        })));
    }
    for m in [Ty::Not, Ty::Mut] {
        cases.push(hir_ty::Adjust::Deref(Some(hir_ty::OverloadedDeref(m))));
        cases.push(hir_ty::Adjust::Borrow(hir_ty::AutoBorrow::RawPtr(m)));
    }
    for case in cases {
        assert_eq!(Adjust::from(case), old(case), "{case:?}");
    }
}
