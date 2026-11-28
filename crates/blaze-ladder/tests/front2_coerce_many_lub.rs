//! End-to-end front2 checks for `CoerceMany` when expected is an unconstrained outer type
//! variable (e.g. `with_label_values(&[&s, "lit"])` where `fn with_labels<V: AsRef<str>>(&[V])`),
//! alongside concrete-expected and incompatible-element negative controls.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
trait Tr {}
struct C1;
struct C2;
impl Tr for C1 {}
impl Tr for C2 {}

fn with_labels<V: AsRef<str>>(_vals: &[V]) {}
fn take<T>(_x: T) {}
fn take_dyn(_xs: &[&dyn Tr]) {}
fn take_strings(_vals: &[&String]) {}

fn pos_array_string_then_str(s: String) {
    with_labels(&[&s, "lit"]);
}

fn pos_array_str_then_string(s: String) {
    with_labels(&["lit", &s]);
}

fn pos_if_and_block(cond: bool, s: String) {
    take(if cond { &s } else { "lit" });
    take('b: {
        if cond {
            break 'b &s;
        }
        "lit"
    });
}

fn pos_array_diverging_mid(s: String) {
    with_labels(&[&s, panic!(), "lit"]);
}

fn pos_array_diverging_first(s: String) {
    with_labels(&[panic!(), &s, "lit"]);
}

fn pos_concrete_dyn() {
    take_dyn(&[&C1, &C2]);
}

fn pos_diverging_rpit() -> impl Sized {
    panic!()
}

fn pos_diverging_rpit_if(c: bool) -> impl Sized {
    if c {
        panic!()
    } else {
        return todo!();
    }
}

fn neg_concrete_string_slice(s: String) {
    take_strings(&[&s, "lit"]);
}

fn neg_incompatible_elements(s: String) {
    with_labels(&[&s, &42i32]);
}

fn main() {}
"#;

#[test]
fn coerce_many_unconstrained_expected_positive_and_negative_controls() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain")))
        .expect("installed toolchain");
    let root = "/coerce_many_lub/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "coerce_many_lub",
        dir: "/coerce_many_lub/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture and standard library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let bodies = world.bodies(alloc);
    let named = |body: BodyRef<'_>| -> Option<String> {
        match body {
            BodyRef::Free(key) => names.item_text(key)?.name.map(str::to_owned),
            BodyRef::Member { name, .. } => Some(name.to_owned()),
        }
    };
    let none: &[&str] = &[];
    for (name, expected_err) in [
        ("pos_array_string_then_str", none),
        ("pos_array_str_then_string", none),
        ("pos_if_and_block", none),
        ("pos_array_diverging_mid", none),
        ("pos_array_diverging_first", none),
        ("pos_concrete_dyn", none),
        ("pos_diverging_rpit", none),
        ("pos_diverging_rpit_if", none),
        ("neg_concrete_string_slice", &["E0308"][..]),
        ("neg_incompatible_elements", &["E0308"][..]),
    ] {
        let matching: Vec<BodyRef<'_>> = bodies
            .iter()
            .map(|&(b, _)| b)
            .filter(|&b| named(b).as_deref() == Some(name))
            .collect();
        assert_eq!(matching.len(), 1, "{name}: expected 1 body");
        let v = world.check(matching[0], alloc);
        assert!(
            v.reads.is_some() && v.gaps.is_empty() && v.refused.is_none() && v.undecided.is_empty(),
            "{name}: {v:?}"
        );
        if expected_err.is_empty() {
            assert!(v.diagnostics.is_empty(), "{name}: expected clean, got {v:?}");
        } else {
            let found = v
                .diagnostics
                .iter()
                .find(|d| expected_err.iter().any(|&code| d.code == code));
            assert!(found.is_some(), "{name}: expected one of {expected_err:?}, got {v:?}");
        }
    }
}
