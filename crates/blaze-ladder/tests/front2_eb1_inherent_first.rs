//! Method lookup on an overloaded dereference of a trait object, and associated
//! constant lookup on a macro-defined newtype.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;

const PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-clo-04-generic-fn/src/prelude.rs");
const HEADER: &str = r#"#![feature(no_core, lang_items, intrinsics, macro_derive, rustc_attrs, unboxed_closures)]
#![no_core]
#![no_main]
mod prelude;
use prelude::*;
"#;

fn check(source: &str, cases: &[(&str, &[&str])]) {
    let source = [HEADER, source].concat();
    let files = [
        SourceFile { path: "src/main.rs", text: &source },
        SourceFile { path: "src/prelude.rs", text: PRELUDE },
    ];
    let alloc = blaze_own::testing::heap().alloc();
    let tc = Toolchain::none(alloc);
    let new = NewFront { package: "inherent", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).expect("fixture texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("fixture crates");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("front2");
    let bodies = world.bodies(alloc);
    for &(name, expected) in cases {
        let body = bodies.iter().find_map(|&(body, _)| match body {
            BodyRef::Free(key) if names.item_text(key)?.name == Some(name) => Some(body),
            _ => None,
        }).unwrap_or_else(|| panic!("missing fixture function `{name}`"));
        let verdict = world.check(body, alloc);
        assert!(verdict.reads.is_some() && verdict.gaps.is_empty() && verdict.refused.is_none() && verdict.undecided.is_empty(), "{name}: {verdict:?}");
        let codes: Vec<_> = verdict.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes.as_slice(), expected, "{name}: {verdict:?}");
    }
}

#[test]
fn dyn_inspector_derefs_to_an_inherent_method() {
    const SOURCE: &str = r#"
#[lang = "deref"]
trait Deref: PointeeSized {
    #[lang = "deref_target"]
    type Target: ?Sized;
    fn deref(&self) -> &Self::Target;
}
trait NonStaticAny {}
impl dyn NonStaticAny + 'static {
    fn downcast_ref<T: 'static>(&self) -> Option<&T> { None }
}
trait Inspector<T>: NonStaticAny {}
impl<'a, T> Deref for dyn Inspector<T> + 'a {
    type Target = dyn NonStaticAny + 'a;
    fn deref(&self) -> &Self::Target { self }
}
impl<T> Option<T> {
    fn unwrap(self) -> T { match self { Some(t) => t, None => loop {} } }
}
struct CountInspector;
fn through<'a>(value: Option<&'a (dyn Inspector<u8> + 'static)>) -> Option<&'a CountInspector> {
    value.unwrap().downcast_ref::<CountInspector>()
}
trait OtherInspector: NonStaticAny {}
fn without_deref<'a>(value: &'a (dyn OtherInspector + 'static)) -> Option<&'a CountInspector> {
    value.downcast_ref::<CountInspector>()
}
"#;
    check(SOURCE, &[("through", &[]), ("without_deref", &["E0599"])]);
    println!("through: clean, without_deref: E0599");
}

#[test]
fn inherent_const_precedes_a_blanket_trait_const() {
    // Alloy's wrapper forwards an extra derive as a path after LowerHex. The
    // formatting assertion needs the generated impl; its integer blanket cannot apply.
    const SOURCE: &str = r#"
#[rustc_builtin_macro]
macro_rules! format_args { ($($arg:tt)*) => {}; }
pub mod fmt {
    #[lang = "format_arguments"] pub struct Arguments;
    pub trait LowerHex {}
}
trait ArbitraryInt {}
impl<T: ArbitraryInt> fmt::LowerHex for T {}
trait WithZero { const ZERO: bool; }
impl<T> WithZero for T { const ZERO: bool = false; }

mod primitives {
    macro_rules! LowerHex {
        derive() (pub struct $name:ident(u8);) => { impl crate::fmt::LowerHex for $name {} };
    }
    macro_rules! Extra {
        derive() ($item:item) => {};
    }
    macro_rules! wrap_fixed_bytes {
        ($name:ident, $extra:path) => {
            #[derive(LowerHex, $extra)]
            pub struct $name(u8);
            impl $name { pub const ZERO: Self = Self(0); }
        };
    }
    wrap_fixed_bytes!(Bloom, Extra);
}
fn zero() -> primitives::Bloom { primitives::Bloom::ZERO }
fn hex_zero() { let _ = format_args!("{:#x}", primitives::Bloom::ZERO); }
fn trait_zero() -> bool { <primitives::Bloom as WithZero>::ZERO }
struct Unformatted;
fn without_formatter(value: Unformatted) { let _ = format_args!("{:#x}", value); }
"#;
    check(SOURCE, &[("zero", &[]), ("trait_zero", &[]), ("without_formatter", &["E0277"]), ("hex_zero", &[])]);
    println!("zero: clean, trait_zero: clean, without_formatter: E0277, hex_zero: clean");
}
