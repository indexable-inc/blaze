//! Borrow checking at calls with generic bounds, projections and captured callables.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
use core::future::Future;

struct Handle<T>(T);
fn start<F>(_: F) -> Handle<F::Output> where F: Future + 'static, F::Output: 'static { loop {} }
fn output_only<F: Future>(_: F) where F::Output: 'static {}
fn retain<T: 'static>(_: T) {}
struct Byte<'a>(&'a u8);
fn use_byte(_: Byte<'_>) {}

fn owned_task(n: u8) { start(async move { n }); }
fn short_capture<'a>(x: Byte<'a>) { start(async move { use_byte(x); }); }
fn independent_output<'a>(x: Byte<'a>) { output_only(async move { use_byte(x); }); }
fn short_output(x: &u8) { output_only(async move { x }); }
fn generic_output<T: 'static>(x: T) { output_only(async move { x }); }
fn short_generic_output<T>(x: T) { output_only(async move { x }); }
fn forward_output<F: Future>(f: F) where F::Output: 'static { output_only(f); }
fn owned_closure<T: 'static>(x: T) { retain(move || x); }
fn short_closure<T>(x: T) { retain(move || x); }

trait Decode<'s> {}
struct Owned;
impl<'s> Decode<'s> for Owned {}
struct StaticOnly;
impl Decode<'static> for StaticOnly {}
fn decode<'s, T: Decode<'s>>(_: &'s u8) -> T { loop {} }
fn owned_decode(value: u8) { decode::<Owned>(&value); }
fn static_decode(value: u8) { decode::<StaticOnly>(&value); }

trait Parameter { type Input; }
struct Fixed;
impl Parameter for Fixed { type Input = &'static u8; }
fn take<P: Parameter, U>(_: P::Input, _: U) {}
fn independent_input<U>(x: &'static u8, u: U) { take::<Fixed, U>(x, u); }
fn short_input<U>(x: u8, u: U) { take::<Fixed, U>(&x, u); }

trait FromInput<'a> { type Output; }
impl<'a> FromInput<'a> for Fixed { type Output = &'a u8; }
fn custom<'a, F: FromInput<'a>>(_: F, _: &'a u8) where F::Output: 'static {}
fn short_other(x: u8) { custom(Fixed, &x); }

fn captured_callback(unused: u8, f: impl Fn(&u8) -> bool) -> bool {
    let g = || { let x = 1u8; f(&x) };
    g()
}
fn captured_static_callback(unused: u8, f: impl Fn(&'static u8)) {
    let g = || { let x = 1u8; f(&x); };
    g();
}
fn main() {}
"#;

fn check_cases(cases: &[(&str, &str)]) {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/w4/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "w4", dir: "/w4/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("edition"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
    for &(name, expected) in cases {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect("fixture body");
        let (typed, borrows) = world.check_borrows(BodyRef::Free(key), alloc);
        assert!(typed.is_clean() && typed.reads.is_some() && typed.undecided.is_empty(), "{name}: {typed:?}");
        match expected {
            "clean" => assert!(borrows.is_clean(), "{name}: {borrows:?}"),
            "CallOutlivesUnread" => {
                assert!(matches!(borrows.refused, Some(BorrowRefused::Lower(blaze_flow_lower::Refusal {
                    form: blaze_flow_lower::Form::CallOutlivesUnread { param: 0, .. }, ..
                }))), "{name}: {borrows:?}");
                assert!(borrows.findings.is_empty(), "{name}: {borrows:?}");
            }
            code => {
                assert!(borrows.refused.is_none(), "{name}: {borrows:?}");
                let [finding] = &borrows.findings[..] else { panic!("{name}: expected one lifetime error: {borrows:?}") };
                assert_eq!(finding.code, code, "{name}: {borrows:?}");
            }
        }
        println!("{name}: {expected}");
    }
}

#[test]
fn outlives_bounds_distinguish_captures_and_output() {
    check_cases(&[
        ("owned_task", "clean"),
        ("independent_output", "clean"),
        ("owned_closure", "clean"),
        ("generic_output", "clean"),
        ("forward_output", "clean"),
        ("short_capture", "outlives"),
        ("short_closure", "E0310"),
        ("short_generic_output", "E0310"),
        ("short_output", "CallOutlivesUnread"),
        ("short_other", "CallOutlivesUnread"),
    ]);
}

#[test]
fn bound_only_calls_use_the_selected_impl_lifetime() {
    check_cases(&[("owned_decode", "clean"), ("static_decode", "E0597")]);
}

#[test]
fn a_captured_callable_uses_its_written_signature() {
    check_cases(&[("captured_callback", "clean"), ("captured_static_callback", "E0597")]);
}

#[test]
fn a_closed_input_projection_uses_its_own_type_arguments() {
    check_cases(&[("independent_input", "clean"), ("short_input", "E0597")]);
}
