// Own program f5-closures (front2): a closure capturing a local by reference, called directly,
// and one passed to a generic `apply` whose bound is `Fn`. Each closure's kind, signature and
// environment come from its owner's check record (the seam's `LayoutFacts::closure`); its body
// is the instance of `Fn::call` at `[closure, (u8,)]`. `no_core`: the `Fn*` traits are
// declared here as core declares them (rustc's minicore).
#![feature(no_core, lang_items, unboxed_closures, rustc_attrs)]
#![no_core]
#![no_main]

#[lang = "sized"]
trait Sized {}

#[lang = "copy"]
trait Copy {}

// core's own impl (core/src/marker.rs `marker_impls!`): a primitive is `Copy` only by an impl,
// never by a built-in rule (rustc select/mod.rs `copy_clone_conditions`: "libcore-provided
// impl"). Without it `x * k` moves `k` (the Reference, "Closure types": a by-value use of a
// non-`Copy` capture), the closure is only `FnOnce`, and `apply`'s `F: Fn(u8)` is unmet.
impl Copy for u8 {}

#[lang = "tuple_trait"]
trait Tuple {}

#[lang = "fn_once"]
#[rustc_paren_sugar]
trait FnOnce<Args: Tuple> {
    #[lang = "fn_once_output"]
    type Output;
    extern "rust-call" fn call_once(self, args: Args) -> Self::Output;
}

#[lang = "fn_mut"]
#[rustc_paren_sugar]
trait FnMut<Args: Tuple>: FnOnce<Args> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> Self::Output;
}

#[lang = "fn"]
#[rustc_paren_sugar]
trait Fn<Args: Tuple>: FnMut<Args> {
    extern "rust-call" fn call(&self, args: Args) -> Self::Output;
}

fn apply<F: Fn(u8) -> u8>(f: F, x: u8) -> u8 {
    f(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let base = 40u8;
    let add = |x: u8| x + base;
    let direct = add(1);
    let k = 2u8;
    let through = apply(|x| x * k, 21);
    // 41 and 42 when both closures read their captures.
    if direct == 41 && through == 42 { 42 } else { 1 }
}
