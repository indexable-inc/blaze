//! Const traits (`const_trait_impl`), const closures (`const_closures`) and fn pointers in
//! const. Expected values are what rustc nightly-2026-10-27 computes for the same program.

use hir_expand::fixture;

use super::{check_number, check_number_in};

/// [`check_number`] with builtin derives expanded, as a build has them: the fast path that only
/// records them leaves the derived methods without bodies to evaluate.
#[track_caller]
fn check_number_derived(#[rust_analyzer::rust_fixture] ra_fixture: &str, answer: i128) {
    let fx = fixture::db(ra_fixture);
    let file_ids = fx.files().to_vec();
    let mut db = fx.into_db();
    hir_def::nameres::set_expand_builtin_derives(&mut db, true);
    check_number_in(&db, *file_ids.last().unwrap(), answer);
}

#[test]
fn const_trait_impl_const_generic_call() {
    check_number(
        r#"
    //- minicore: sized
    const trait Double {
        fn double(&self) -> i32;
        fn quad(&self) -> i32 {
            self.double() * 2
        }
    }
    struct S(i32);
    const impl Double for S {
        fn double(&self) -> i32 {
            self.0 * 2
        }
    }
    const fn via_bound<T: [const] Double>(t: &T) -> i32 {
        t.double() + t.quad()
    }
    const fn via_where<T>(t: T) -> i32 where T: [const] Double {
        Double::double(&t)
    }
    const fn via_impl(t: &impl [const] Double) -> i32 {
        Double::quad(t)
    }
    const GOAL: i32 = via_bound(&S(1)) * 100 + via_where(S(2)) * 10 + S(3).quad();
    "#,
        6 * 100 + 4 * 10 + 12,
    );
}

#[test]
fn const_trait_const_bound_in_const_item() {
    check_number(
        r#"
    //- minicore: sized
    const trait Val {
        fn val() -> u8;
    }
    struct A;
    struct B;
    const impl Val for A {
        fn val() -> u8 { 3 }
    }
    const impl Val for B {
        fn val() -> u8 { 4 }
    }
    const fn sum<T: const Val, U: [const] Val>() -> u8 {
        T::val() + U::val()
    }
    const GOAL: u8 = sum::<A, B>();
    "#,
        7,
    );
}

#[test]
fn const_trait_tilde_const_bound() {
    check_number(
        r#"
    //- minicore: sized
    const trait Val {
        fn val(&self) -> u8;
    }
    const impl Val for u8 {
        fn val(&self) -> u8 { *self + 1 }
    }
    const fn get<T: ~const Val>(t: T) -> u8 {
        t.val()
    }
    const GOAL: u8 = get(41u8);
    "#,
        42,
    );
}

#[test]
fn const_trait_supertrait_and_assoc_type() {
    check_number(
        r#"
    //- minicore: sized
    const trait Base {
        type Out;
        fn base(&self) -> Self::Out;
    }
    const trait Derived: [const] Base<Out = u32> {
        fn derived(&self) -> u32 {
            self.base() + 1
        }
    }
    struct S;
    const impl Base for S {
        type Out = u32;
        fn base(&self) -> u32 { 9 }
    }
    const impl Derived for S {}
    const fn go<T: [const] Derived>(t: &T) -> u32 {
        t.derived() + t.base()
    }
    const GOAL: u32 = go(&S);
    "#,
        19,
    );
}

#[test]
fn const_trait_std_ops() {
    check_number(
        r#"
    //- minicore: add, eq, ord, clone, copy, default, derive, from, sized, option
    #[derive(Clone, Copy)]
    struct P(i32, i32);
    const impl core::ops::Add for P {
        type Output = P;
        fn add(self, o: P) -> P { P(self.0 + o.0, self.1 + o.1) }
    }
    const impl PartialEq for P {
        fn eq(&self, o: &P) -> bool { self.0 == o.0 && self.1 == o.1 }
    }
    const impl Default for P {
        fn default() -> P { P(1, 2) }
    }
    const impl From<i32> for P {
        fn from(x: i32) -> P { P(x, x) }
    }
    const fn add_all<T: [const] core::ops::Add<Output = T> + Copy>(a: T, b: T) -> T {
        a + b
    }
    const fn same<T: [const] PartialEq>(a: &T, b: &T) -> bool {
        a == b
    }
    const fn make<T: [const] Default>() -> T {
        T::default()
    }
    const GOAL: i32 = {
        let p = add_all(P(1, 2), P::from(3));
        let d: P = make();
        let eq = same(&p, &P(4, 5)) as i32;
        let ne = same(&d, &p) as i32;
        p.0 * 1000 + p.1 * 100 + eq * 10 + ne
    };
    "#,
        4510,
    );
}

#[test]
fn const_trait_derive_const() {
    // minicore lacks what a derived `eq` of a struct with fields or of an enum calls
    // (`PartialEq for &A`, `discriminant_value`): the harness case runs those on std.
    check_number_derived(
        r#"
    //- minicore: derive, eq, clone, copy, default, sized, builtin_impls
    #[derive(Copy)]
    #[derive_const(Clone)]
    struct W(u8);
    const impl Default for W {
        fn default() -> W { W(7) }
    }
    #[derive_const(Clone, Default)]
    struct S {
        a: W,
        b: u8,
    }
    #[derive_const(PartialEq)]
    struct Unit;
    const fn dup<T: [const] Clone>(t: &T) -> T {
        t.clone()
    }
    const fn same<T: [const] PartialEq>(a: &T, b: &T) -> bool {
        *a == *b
    }
    const GOAL: u8 = {
        let d: S = S::default();
        let s = dup(&S { a: W(1), b: 2 });
        (same(&Unit, &Unit) as u8) * 100 + s.a.0 * 10 + s.b + d.a.0 + d.b
    };
    "#,
        100 + 10 + 2 + 7,
    );
}

#[test]
fn const_closure_called_in_const() {
    check_number(
        r#"
    //- minicore: fn, copy
    const GOAL: i32 = {
        let k = 5;
        let add = const |x: i32| x + k;
        let mul = const move |x: i32| x * k;
        add(1) * 100 + mul(2)
    };
    "#,
        610,
    );
}

#[test]
fn const_closure_through_const_fn_bound() {
    check_number(
        r#"
    //- minicore: fn, copy
    const fn apply<F: [const] Fn(i32) -> i32>(f: F, x: i32) -> i32 {
        f(x)
    }
    const fn apply_mut(mut f: impl [const] FnMut(i32), n: i32) {
        let mut i = 0;
        while i < n {
            f(i);
            i += 1;
        }
    }
    const fn apply_once<F: [const] FnOnce() -> i32>(f: F) -> i32 {
        f()
    }
    const GOAL: i32 = {
        let mut acc = 0;
        apply_mut(const |i| acc += i, 5);
        apply(const |x| x * 3, 7) * 100 + acc + apply_once(const || 1000)
    };
    "#,
        2100 + 10 + 1000,
    );
}

#[test]
fn const_fn_item_through_fn_bound() {
    check_number(
        r#"
    //- minicore: fn, copy
    const fn inc(x: i32) -> i32 { x + 1 }
    const fn apply<F: [const] Fn(i32) -> i32>(f: F, x: i32) -> i32 {
        f(x)
    }
    const GOAL: i32 = apply(inc, 41);
    "#,
        42,
    );
}

#[test]
fn fn_pointer_called_in_const() {
    check_number(
        r#"
    //- minicore: fn, copy
    const fn inc(x: i32) -> i32 { x + 1 }
    const fn call(f: fn(i32) -> i32, x: i32) -> i32 {
        f(x)
    }
    const GOAL: i32 = call(inc, 1);
    "#,
        2,
    );
}
