//! rustc's model of a trait method's return-position `impl Trait` (RPITIT, `hir_ty::rpitit`),
//! ported from rustc's `tests/ui/impl-trait/in-trait/` cases that pass under rustc: what a
//! caller sees through a concrete impl (`refine.rs`, `refine-captures.rs`), through a generic
//! one (`opaque-in-impl.rs`), a default body (`default-body.rs`, `default-body-with-rpit.rs`),
//! a nested `impl Trait` (`nested-rpitit.rs`), auto traits leaking through the impl's type
//! (`auto-trait-leakage`), and a lifetime the impl's type captures (`lifetime-in-return.rs`).

use super::{check_infer_with_mismatches, check_types};
use expect_test::expect;

/// `refine.rs`: an impl whose method returns a concrete type for the trait's `impl Trait` gives
/// that type to a caller that names the impl, as rustc's normalization of the trait's item to
/// the impl's does.
#[test]
fn a_caller_through_a_refining_impl_sees_its_type() {
    check_types(
        r#"
//- minicore: sized
struct Vec<T>(T);
impl<T> Vec<T> { fn len(&self) -> usize { 0 } }
trait Roots { fn roots(&self) -> impl Sized; }
struct F;
impl Roots for F { fn roots(&self) -> Vec<u32> { Vec(0) } }
fn caller() {
    let r = F.roots();
      //^ Vec<u32>
    let n = r.len();
      //^ usize
}
"#,
    );
}

/// `opaque-in-impl.rs`: an impl's own `impl Trait` is what the caller sees through it, its
/// bounds the impl's, not the trait's.
#[test]
fn a_caller_through_an_impl_sees_the_impls_opaque_type() {
    check_types(
        r#"
//- minicore: sized
trait Tr { fn f(&self) -> impl Sized; }
trait Extra { fn extra(&self) -> u8; }
impl Extra for u16 { fn extra(&self) -> u8 { 0 } }
struct S;
impl Tr for S { fn f(&self) -> impl Extra { 0u16 } }
fn caller() {
    let e = S.f().extra();
      //^ u8
}
"#,
    );
}

/// A generic caller sees the trait's item, whose bounds are the ones written on the trait.
#[test]
fn a_generic_caller_sees_the_traits_bounds() {
    check_types(
        r#"
//- minicore: sized, iterator
trait Tr { fn f(&self) -> impl Iterator<Item = u32>; }
fn caller<T: Tr>(t: &T) {
    let mut it = t.f();
    let x = it.next();
      //^ Option<u32>
}
"#,
    );
}

/// `default-body.rs`: a default body is checked with the trait's item equal to its own opaque
/// type (rustc's `ImplTraitInTraitFinder` clause), and an impl that keeps it gives callers that
/// opaque type.
#[test]
fn a_default_body_defines_its_own_impl_trait() {
    check_infer_with_mismatches(
        r#"
//- minicore: sized, copy
trait Tr {
    fn f(&self) -> impl Sized { 0u8 }
    fn g(&self) -> impl Copy { (1u8, 2u16) }
}
struct S;
impl Tr for S {}
fn caller() { let _x = S.f(); let _y = S.g(); }
"#,
        &expect![[r#"
            21..25 'self': &'? Self
            41..48 '{ 0u8 }': u8
            43..46 '0u8': u8
            59..63 'self': &'? Self
            78..93 '{ (1u8, 2u16) }': (u8, u16)
            80..91 '(1u8, 2u16)': (u8, u16)
            81..84 '1u8': u8
            86..90 '2u16': u16
            135..170 '{ let ...g(); }': ()
            141..143 '_x': impl Sized
            146..147 'S': S
            146..151 'S.f()': impl Sized
            157..159 '_y': impl Copy
            162..163 'S': S
            162..167 'S.g()': impl Copy
        "#]],
    );
}

/// `nested-rpitit.rs`: an `impl Trait` whose bounds hold another is, through an impl, the
/// impl's type, whose own `Iterator` impl then answers. (The inner one's value, which a generic
/// caller reaches, is hir-ty's `impl_rpitit_tys` test and hir-ty-identity's.)
#[test]
fn a_nested_impl_trait_through_an_impl_is_the_impls_item() {
    check_types(
        r#"
//- minicore: sized, iterator
struct Once<T>(T);
impl<T> Iterator for Once<T> { type Item = T; fn next(&mut self) -> Option<T> { loop {} } }
trait Tr { fn items(&self) -> impl Iterator<Item = impl Sized>; }
struct S;
impl Tr for S { fn items(&self) -> Once<u16> { Once(0) } }
fn caller() {
    let x = S.items().next();
      //^ Option<u16>
}
"#,
    );
}

/// Auto traits of the impl's type leak to a caller that names the impl, as they do through an
/// opaque type: a method of a blanket impl for `T: Send` is found on a value whose type is
/// `Send`, and not on one whose type is not.
#[test]
fn auto_traits_leak_through_the_impls_type() {
    check_types(
        r#"
//- minicore: sized, send
trait IsSend { fn is_send(&self) -> u32; }
impl<T: Send> IsSend for T { fn is_send(&self) -> u32 { 0 } }
trait Tr { fn f(&self) -> impl Sized; fn g(&self) -> impl Sized; }
struct S;
impl Tr for S {
    fn f(&self) -> impl Sized { 0u8 }
    fn g(&self) -> impl Sized { 0 as *const u8 }
}
fn caller() {
    let n = S.f().is_send();
      //^ u32
    let m = S.g().is_send();
      //^ {unknown}
}
"#,
    );
}

/// `lifetime-in-return.rs`: a lifetime the impl's type captures is the trait method's, so a
/// caller's borrow of `self` reaches the value.
#[test]
fn a_captured_lifetime_reaches_the_impls_type() {
    check_types(
        r#"
//- minicore: sized, iterator
struct Iter<'a, T>(&'a T);
impl<'a, T> Iterator for Iter<'a, T> { type Item = &'a T; fn next(&mut self) -> Option<&'a T> { loop {} } }
trait Tr { fn iter(&self) -> impl Iterator<Item = &u32>; }
struct S(u32);
impl Tr for S { fn iter(&self) -> Iter<'_, u32> { Iter(&self.0) } }
fn caller(s: &S) {
    let x = s.iter().next();
      //^ Option<&'? u32>
}
"#,
    );
}
