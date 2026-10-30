use syntax::{TextRange, TextSize};

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: borrowck
//
// An error of rustc's borrow checker: a use of a moved or uninitialized value, a mutation of
// what is not mutable, a borrow that conflicts with another or does not live long enough. Also
// rustc's `unused_mut` lint, which its borrow checker gives: a variable declared `mut` that is
// never mutated.
pub(crate) fn borrowck_error(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckError,
) -> Diagnostic {
    // rustc gives some errors no code, as "lifetime may not live long enough".
    let code = match d.code {
        "" => DiagnosticCode::Ra("borrowck", crate::Severity::Error),
        "unused_mut" => DiagnosticCode::RustcLint("unused_mut"),
        code => DiagnosticCode::RustcHardError(code),
    };
    let mut diagnostic = Diagnostic::new_with_syntax_node_ptr(ctx, code, d.message.clone(), d.node);
    // rustc's span of an expression a macro writes, `&$e`, is in the macro's definition, where
    // its first token is; one the caller writes is the caller's. A definition in another file
    // stays at the call.
    if let Some(file) = d.node.file_id.macro_file() {
        let start = d.node.value.text_range().start();
        let (origin, context) = hir::InMacroFile::new(file, start).original_file_range(ctx.sema.db);
        if !context.is_root() && origin.file_id.file_id(ctx.sema.db) == diagnostic.range.file_id {
            diagnostic.range.range = origin.range;
        }
    }
    if d.at_end {
        let range = diagnostic.range.range;
        if let Some(last) = range.end().checked_sub(TextSize::new(1))
            && last >= range.start()
        {
            diagnostic.range.range = TextRange::new(last, range.end());
        }
    }
    diagnostic.stable()
}

// Diagnostic: borrowck-skipped
//
// The borrow checker did not check a body, because MIR lowering does not support something
// in it. Reported only for the borrow checker's test harness.
pub(crate) fn borrowck_skipped(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::BorrowckSkipped,
) -> Diagnostic {
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::Ra("borrowck-skipped", crate::Severity::WeakWarning),
        format!("not borrow checked: {}", d.reason),
        d.node,
    )
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn impl_trait_captures_elided_lifetime_2024() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2024
struct S(i32);
impl S {
    fn get(&self) -> impl Sized { &self.0 }
    fn kept(&self) -> impl Sized + use<> {}
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let kept = s.kept();
    s.set();
    consume(kept);
}
"#,
        );
    }

    #[test]
    fn impl_trait_captures_elided_lifetime_2021() {
        check_diagnostics(
            r#"
//- minicore: sized, copy
//- /main.rs edition:2021
struct S(i32);
impl S {
    fn get(&self) -> impl Sized + '_ { &self.0 }
    fn copied(&self) -> impl Sized { self.0 }
    fn set(&mut self) {}
}
fn consume<T>(_: T) {}
fn user(mut s: S) {
    let it = s.get();
    s.set();
  //^^^^^^^ error: cannot borrow `s` as mutable because it is also borrowed as immutable
    consume(it);
    let copied = s.copied();
    s.set();
    consume(copied);
}
"#,
        );
    }

    #[test]
    fn constant_of_a_signature_is_borrow_checked() {
        check_diagnostics(
            r#"
//- minicore: sized
pub struct X<const N: usize = {
    let n: usize;
    n
  //^ error: used binding `n` isn't initialized
}>;
pub fn f() -> [u8; { let n = 1; n }] {
    [0]
}
"#,
        );
    }

    #[test]
    fn higher_ranked_bound_holds_for_any_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, fn
trait Deserialize<'de> {}
trait DeserializeOwned: for<'de> Deserialize<'de> {}
impl<T> DeserializeOwned for T where T: for<'de> Deserialize<'de> {}
impl<'de: 'a, 'a> Deserialize<'de> for &'a str {}
impl<'de> Deserialize<'de> for u32 {}
fn owned<T: DeserializeOwned>() {}
trait Write {}
fn handler(_: &u32, _: &mut dyn Write) {}
fn run(_: impl FnMut(&u32, &mut dyn Write)) {}
fn main() {
    owned::<u32>();
    owned::<&'static str>();
  //^^^^^^^^^^^^^^^^^^^^^^^ error: higher-ranked lifetime error
    run(handler);
}
"#,
        );
    }

    #[test]
    fn one_fn_pointer_of_items_or_closures_holds_for_any_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized, fn, copy
fn up(_: &u32, _: &u32) -> bool { true }
fn down(_: &u32, _: &u32) -> bool { false }
fn sort(_: impl FnMut(&u32, &u32) -> bool) {}
fn pick(reverse: bool) {
    sort(if reverse { up } else { down });
    sort(match reverse {
        true => |_: &u32, _: &u32| true,
        false => |_: &u32, _: &u32| false,
    });
}
struct Line<'a>(&'a u32);
fn mode<'a>(_: &mut Line<'a>) {}
fn name<'a>(_: &mut Line<'a>) {}
fn element<'a>(first: bool) -> impl Fn(&mut Line<'a>) {
    match first {
        true => mode,
        false => name,
    }
}
"#,
        );
    }

    #[test]
    fn destructuring_assignment_moves_what_it_takes() {
        check_diagnostics(
            r#"
//- minicore: copy
struct NC;
struct S { a: u8, b: NC }
union U { field: u8 }
fn copies(r: &S, u: &U, mut a: u8, mut field: u8) {
    S { a, .. } = *r;
    S { a: _, .. } = *r;
    unsafe { U { field } = *u };
    let _ = (a, field);
}
fn moves(r: &S, mut b: NC) {
    S { b, .. } = *r;
                //^^ error: cannot move out of `r.b` which is behind a shared reference
    let _ = b;
}
"#,
        );
    }

    #[test]
    fn a_body_with_an_error_is_not_borrow_checked() {
        check_diagnostics(
            r#"
//- minicore: copy
struct NC;
fn tainted(x: NC) -> u32 {
    let _y = x;
    let _z = x;
    NC as u32
  //^^^^^^^^^ error: non-primitive cast: `NC` as `u32`
}
"#,
        );
    }

    // minicore's `impl<T> Default for &[T]`.
    #[test]
    fn an_empty_array_is_promoted_whatever_its_elements() {
        check_diagnostics(
            r#"
//- minicore: copy, cell, unsize, coerce_unsized
use core::cell::Cell;
fn shared<T>() -> &'static [T] { &[] }
fn unique<'a, T>() -> &'a mut [T] { &mut [] }
fn cells() -> &'static [Cell<u8>] { &[] }
fn one<T>(x: T) -> &'static [T] { &[x] }
                                //^^^^ error: returns a value referencing data owned by the current function
"#,
        );
    }

    // rustc's tests/ui/expr/if/expr-stack-overflow.rs: each expression's syntax was found by
    // walking down from the body's root, quadratic in the chain, and lowering it recursed
    // past the stack.
    #[test]
    fn a_5204_arm_else_if_chain_is_diagnosed_in_linear_time() {
        let mut fixture = String::from(
            "//- minicore: copy\npub fn banana(v: u32) -> u32 {\n    if v == 0 { 0 }\n",
        );
        for arm in 1..5204 {
            fixture.push_str(&format!("    else if v == {arm} {{ {arm} }}\n"));
        }
        fixture.push_str("    else { 5204 }\n}\n");
        let started = std::time::Instant::now();
        check_diagnostics(&fixture);
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(60), "took {took:?}");
    }

    // rustc's tests/ui/associated-type-bounds/issue-83017.rs.
    #[test]
    fn bound_of_an_associated_type_in_a_for_binds_the_outer_lifetimes() {
        check_diagnostics(
            r#"
//- minicore: sized
trait TraitA<'a> { type AsA; }
trait TraitB<'a, 'b> { type AsB; }
trait TraitC<'a, 'b, 'c> {}
struct X;
impl<'a, 'b, 'c> TraitC<'a, 'b, 'c> for X {}
struct Y;
impl<'a, 'b> TraitB<'a, 'b> for Y { type AsB = X; }
struct Z;
impl<'a> TraitA<'a> for Z { type AsA = Y; }
fn foo<T>()
where
    for<'a> T: TraitA<'a, AsA: for<'b> TraitB<'a, 'b, AsB: for<'c> TraitC<'a, 'b, 'c>>>,
{
}
fn main() {
    foo::<Z>();
}
"#,
        );
    }

    // rustc's tests/ui/dyn-compatibility/multiple-supers-should-work.rs.
    #[test]
    fn a_trait_object_keeps_projections_that_become_the_same() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait Sup<T> { type Assoc; }
impl<T> Sup<T> for () { type Assoc = T; }
trait Trait<A, B>: Sup<A, Assoc = A> + Sup<B, Assoc = B> {}
impl<T, U> Trait<T, U> for () {}
fn main() {
    let x: &dyn Trait<(), _> = &();
    let _y: &dyn Trait<_, ()> = x;
}
"#,
        );
    }

    // rustc's tests/ui/traits/trait-upcasting/higher-ranked-upcasting-ok.rs.
    #[test]
    fn upcasting_a_higher_ranked_trait_object() {
        check_diagnostics(
            r#"
//- minicore: sized, unsize, coerce_unsized, dispatch_from_dyn
trait Supertrait<'a, 'b> {}
trait Subtrait<'a, 'b>: Supertrait<'a, 'b> {}
impl Supertrait<'_, '_> for () {}
impl Subtrait<'_, '_> for () {}
fn ok(x: &dyn for<'a, 'b> Subtrait<'a, 'b>) -> &dyn for<'a> Supertrait<'a, 'a> {
    x
}
fn main() {
    ok(&());
}
"#,
        );
    }

    #[test]
    fn unit_expression_initializes_its_place() {
        check_diagnostics(
            r#"
//- minicore: copy
#![deny(unused_mut)]
fn f() -> () {
    let mut x: i32;
    let z: () = x = 10;
    let w = z;
    let mut q: () = x = 11;
    q = x += 1;
    let mut b: () = { x += 1; };
    b = ();
    let mut l: () = loop { break; };
    l = ();
    let mut i: () = if x > 0 { x = 1 };
    i = ();
    let _ = (q, b, l, i, x);
    w
}
"#,
        );
    }

    // salsa's `&dyn Database` methods: the object's `'static` is not an elided input lifetime.
    #[test]
    fn a_trait_objects_default_lifetime_is_not_elided() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Z: 'static { fn z(&self) -> &u8; }
fn f(d: &dyn Z) -> &u8 { d.z() }
fn g<'a>(d: &'a dyn Z, x: &'a u8) -> &'a u8 { if true { d.z() } else { x } }
"#,
        );
    }

    // biome_fs: an alias's elided lifetime is one lifetime wherever the alias uses it.
    #[test]
    fn an_aliases_elided_lifetime_is_one_lifetime() {
        check_diagnostics(
            r#"
//- minicore: sized
trait TS<'s> {}
type Pair<'f, 's> = (&'f dyn TS<'s>, &'f dyn TS<'s>);
fn same<'s>(_: &dyn TS<'s>, _: &dyn TS<'s>) {}
fn split(pair: Pair) { same(pair.0, pair.1) }
"#,
        );
    }

    // helix-vcs: a closure's elided parameter lifetimes are its own, for any lifetime.
    #[test]
    fn a_closures_annotated_reference_is_higher_ranked() {
        check_diagnostics(
            r#"
//- minicore: sized, fn, copy, slice, index
struct H { a: u32 }
fn f(hs: &[H], inv: bool) -> (u32, u32) {
    let r = if inv { |h: &H| h.a } else { |h: &H| h.a };
    let x = H { a: 1 };
    (r(&x), r(&hs[0]))
}
"#,
        );
    }

    // biome's `use_sorted_keys`: the pointer two fn items coerce to holds for any lifetime.
    #[test]
    fn fn_items_coerce_to_a_higher_ranked_pointer() {
        check_diagnostics(
            r#"
//- minicore: sized, fn, copy
struct T(u8);
impl T {
    fn a(x: &T, y: &T) -> bool { x.0 < y.0 }
    fn b(x: &T, y: &T) -> bool { x.0 > y.0 }
}
fn find(_: impl FnMut(&T) -> bool) {}
fn f(k: bool) {
    let c = match k { true => T::a, false => T::b };
    let t = T(3);
    find(|x| c(x, &t))
}
"#,
        );
    }

    // biome_formatter's buffers: `&S<Self::C>` implies what `S<C>` does.
    #[test]
    fn implied_bounds_see_through_a_projection() {
        check_diagnostics(
            r#"
//- minicore: sized
struct S<C>(C);
trait B { type C; fn st(&self) -> &S<Self::C>; }
impl<W: B<C = C> + ?Sized, C> B for &mut W {
    type C = C;
    fn st(&self) -> &S<Self::C> { (**self).st() }
}
"#,
        );
    }

    // biome's `markup!`: a value with no destructor and no interior mutability is promoted,
    // whatever its type.
    #[test]
    fn promotion_goes_by_the_value() {
        check_diagnostics(
            r#"
//- minicore: sized, drop, option, copy
struct D;
impl Drop for D { fn drop(&mut self) {} }
enum M { Hard, Soft }
enum F { L(M), T(D) }
fn take(_: Option<&'static F>) {}
fn take_d(_: &'static D) {}
fn g() {
    take(Some(&F::L(M::Hard)));
    take_d(&D);
         //^^ error: temporary value dropped while borrowed
}
"#,
        );
    }

    #[test]
    fn const_call_arguments_promote_associated_constants() {
        check_diagnostics(
            r#"
//- minicore: sized, copy, unsize, coerce_unsized
struct Item(u8);
const fn item(value: u8) -> Item { Item(value) }
trait HasItem {
    const ITEM: Item;
    const ITEMS: &'static [Item] = &[Self::ITEM, item(2)];
}
struct A;
impl HasItem for A { const ITEM: Item = item(1); }
impl A { const INHERENT: Item = item(3); }
const QUALIFIED: &[Item] = &[<A as HasItem>::ITEM, item(2)];
const TRAIT: &[Item] = &[A::ITEM, item(2)];
const INHERENT: &[Item] = &[A::INHERENT, item(2)];
static STATIC: &[Item] = &[A::ITEM, item(2)];
"#,
        );
    }

    #[test]
    fn associated_constants_do_not_promote_runtime_const_calls() {
        check_diagnostics(
            r#"
//- minicore: sized, copy, unsize, coerce_unsized
struct Item(u8);
const fn item(value: u8) -> Item { Item(value) }
trait HasItem { const ITEM: Item; }
struct A;
impl HasItem for A { const ITEM: Item = item(1); }
fn runtime() -> &'static [Item] {
    &[A::ITEM, item(2)]
  //^^^^^^^^^^^^^^^^^^^ error: returns a value referencing data owned by the current function
}
"#,
        );
    }

    // biome's organize_imports: `use<>` on an `impl Trait` inside the return type.
    #[test]
    fn use_bound_of_a_nested_impl_trait_captures_nothing() {
        check_diagnostics(
            r#"
//- minicore: sized, option, copy
//- /main.rs edition:2024
struct N(u8);
fn att(n: &N) -> Option<impl Sized + use<>> { Some(n.0) }
impl N { fn with(self, _i: impl Sized) -> N { self } }
fn f(mut n: N) -> N {
    if let Some(a) = att(&n) { n = n.with(a); }
    n
}
"#,
        );
    }

    // hir-ty's `InferBodyId::store_and_root_expr`: lifetime elision counts none of the impl's
    // lifetimes in a parameter that writes `Self`, so the return type has the lifetime of `db`
    // or `x`, here and where it is called.
    #[test]
    fn elision_counts_no_lifetime_of_self() {
        check_diagnostics(
            r#"
//- minicore: sized, deref, copy, clone, derive
use core::ops::Deref;
struct Store(u32);
struct Body(Store);
impl Deref for Body {
    type Target = Store;
    fn deref(&self) -> &Store { &self.0 }
}
trait Db {}
fn body_of(_db: &dyn Db) -> &Body { loop {} }
#[derive(Clone, Copy)]
struct Id<'a>(&'a u8);
impl Id<'_> {
    fn store(self, db: &dyn Db) -> (&Store, u32) {
        let body = body_of(db);
        (body, 0)
    }
    fn other(self, _o: Self, x: &u32) -> &u32 { x }
}
impl<'a> Id<'a> {
    fn named(self, x: &u32) -> &u32 { x }
}
fn caller<'a>(db: &'a dyn Db, id: Id<'_>) -> &'a Store { id.store(db).0 }
fn caller_err(id: Id<'static>) -> &'static u32 {
    let x = 1;
    id.named(&x)
  //^^^^^^^^^^^^ error: cannot return reference to local variable
}
"#,
        );
    }

    // hir-ty's `path_to_const`: what the types in a trait object's arguments imply holds,
    // `'db: 'a` of `&dyn Fn() -> &'a G<'db>`.
    #[test]
    fn implied_bounds_of_a_trait_objects_arguments() {
        check_diagnostics(
            r#"
//- minicore: sized, fn, dispatch_from_dyn
struct G<'db>(&'db u32);
fn need<'a, 'db: 'a>(_: &'a G<'db>) {}
fn f<'a, 'db>(g: &dyn Fn() -> &'a G<'db>) { need(g()) }
"#,
        );
    }

    // core's `fmt::rt::Argument::new_display`: the `'_` of `Formatter<'_>` is late-bound in
    // `Display::fmt`, so the item reifies to the `for<'a, 'b, 'c> fn` it is given as.
    #[test]
    fn a_fn_items_elided_lifetime_is_late_bound_when_reified() {
        check_diagnostics(
            r#"
//- minicore: sized
struct Formatter<'a>(&'a ());
trait Display {
    fn fmt(&self, f: &mut Formatter<'_>);
}
fn new<T>(_: &T, _: fn(&T, &mut Formatter<'_>)) {}
fn new_display<T: Display>(x: &T) {
    new(x, Display::fmt)
}
"#,
        );
    }

    // ruff's `OptionSet::of`: `record: T::record` of `fn record(visit: &mut dyn Visit)` into a
    // `fn(&mut dyn Visit)`. The item's elided `&mut dyn Visit` is `&'r mut (dyn Visit + 'r)`.
    #[test]
    fn a_fn_items_elided_trait_object_has_its_references_lifetime_when_reified() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Visit {}
trait Options { fn record(visit: &mut dyn Visit); }
struct Set { record: fn(&mut dyn Visit), shared: fn(&dyn Visit) }
fn free(_: &mut dyn Visit) {}
fn shared(_: &dyn Visit) {}
fn of<T: Options>() -> Set { Set { record: T::record, shared } }
fn local() { let _f: fn(&mut dyn Visit) = free; }
fn named<'a>() { let _f: fn(&'a mut dyn Visit) = free; }
"#,
        );
    }

    // hir-def's `module_for_assoc_item_loc`: `dyn 'db + Database` implies `'db: 'static`
    // where a supertrait of `Database` is `'static`.
    #[test]
    fn a_supertraits_static_bound_is_implied() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Base: 'static {}
trait Database: Base {}
fn take(_: &dyn Database) {}
fn f<'db>(db: &(dyn 'db + Database)) { take(db) }
"#,
        );
    }

    #[test]
    fn a_failed_guard_ends_what_its_pattern_binds() {
        check_diagnostics(
            r#"
//- minicore: copy
fn p(_: &u8) -> bool { false }
fn f(v: (u8, u8)) -> u8 {
    let mut n = 0;
    loop {
        n += match v {
            (ref a, 1) | (2, ref a) if p(a) => *a,
            (ref a, _) if p(a) => *a,
            _ => 1,
        };
        if n > 2 {
            return n;
        }
    }
}
"#,
        );
    }

    #[test]
    fn unused_mut_self() {
        check_diagnostics(
            r#"
#![deny(unused_mut)]
struct S(u8);
impl S {
    fn read(mut self) -> u8 { self.0 }
          //^^^^^^^^ error: variable does not need to be mutable
    fn write(mut self) -> u8 { self.0 = 1; self.0 }
}
"#,
        );
    }

    #[test]
    fn destructuring_assignment_through_deref_mut_to_a_reference_field() {
        check_diagnostics(
            r#"
//- minicore: deref_mut, copy, fn
use core::ops::{Deref, DerefMut};
struct State { count: u32, label: &'static str }
struct Guard<'s>(&'s mut State);
impl Deref for Guard<'_> { type Target = State; fn deref(&self) -> &State { self.0 } }
impl DerefMut for Guard<'_> { fn deref_mut(&mut self) -> &mut State { self.0 } }
fn guard(mut g: Guard<'_>, flag: bool) {
    (g.count, g.label) = if flag { (1, "one") } else { (2, "two") };
    (g.label,) = ("x",);
}
fn closure(mut g: Guard<'_>) {
    let mut set = || (g.label,) = ("y",);
    set();
}
fn shared(s: &State) {
    (s.label,) = ("z",);
   //^^^^^^^ error: cannot assign to `s.label`, which is behind a `&` reference
}
"#,
        );
    }

    #[test]
    fn async_bodys_tail_temporaries_drop_before_its_parameters_2021() {
        check_diagnostics(
            r#"
//- minicore: future, option, drop, async_fn
//- /main.rs edition:2021
struct Guard<'a>(&'a u32);
impl Drop for Guard<'_> { fn drop(&mut self) {} }
struct Memory(u32);
impl Memory { fn lock(&self) -> Option<Guard<'_>> { Some(Guard(&self.0)) } }
async fn param(memory: Memory) {
    if let Some(_g) = memory.lock() {}
}
async fn pattern((memory, _): (Memory, u32)) {
    if let Some(_g) = memory.lock() {}
}
fn closure() {
    let _ = async |memory: Memory| {
        if let Some(_g) = memory.lock() {}
    };
}
async fn local() {
    let memory = Memory(0);
    if let Some(_g) = memory.lock() {}
                    //^^^^^^ error: `memory` does not live long enough
}
"#,
        );
    }
}
