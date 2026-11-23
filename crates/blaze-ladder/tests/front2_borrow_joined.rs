//! A normalized projection in a call's output tied both ways to two or more joined positions is a
//! relation STRICTER than Rust's, and it is recorded as such (blaze-flow-lower generic_call.rs
//! `Slot::Joined`, `CallLacks::JoinedSuperset`, the coarse-relations rule: `Lowered::coarse`).
//!
//! The rule: `<Iter<'a, W<'b>> as Iterator>::Item` is `&'a W<'b>` (the impl's associated type);
//! its first position is `'a` and its second `'b`, ONE input each. The lowering does not lay the
//! projection through the impl here: it ties every position of the output to every position of
//! the joined argument, both ways, which makes `'a == 'b`. Rust asks only `'b: 'a` (implied by
//! `&'a [W<'b>]` being well formed). So `for x in xs` over `xs: &'a [W<'b>]` got a false "a
//! lifetime must outlive another" (tempo: bin/prover/utils/src/main.rs:167 `for (name, elapsed)
//! in &self.0` over `Vec<(&'static str, Duration)>`, asking `'s: 'static`;
//! crates/precompiles-macros/src/packing.rs:135 `for field in fields`, `fields: &'a
//! [LayoutField<'b>]`, asking `'a == 'b`).
//!
//! - `two`: two joined positions. rustc accepts it. It must NOT be reported as a finding: it is
//!   clean, or refused by the step's name (`Stricter(CallGenericUnaligned/JoinedSuperset)`).
//! - `statics`: `&'static str` inside a borrowed `Vec`: as `two`.
//! - TWIN `one` (rule 23): one joined position (`&'a [u8]`): no stricter step is taken, and it
//!   is CHECKED clean.
//! - TWIN `escapes`: rustc refuses it (`'a: 'b` is missing for `&'b W<'b>`). It must never be
//!   clean: refused by the step's name (the finding depends on it) or a finding.
//! - TWIN `moved`: a move finding (E0382 reads no region) beside the step is reported.
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
pub struct W<'b>(pub &'b u8);

fn two<'a, 'b>(xs: &'a [W<'b>]) {
    for x in xs {
        let _ = x;
    }
}

fn statics(v: &Vec<(&'static str, u8)>) {
    for (n, _) in v {
        let _ = n;
    }
}

fn one<'a>(xs: &'a [u8]) {
    for x in xs {
        let _ = x;
    }
}

fn escapes<'a, 'b>(xs: &'a [W<'b>]) -> &'b W<'b> {
    xs.iter().next().unwrap()
}

fn moved<'a, 'b>(xs: &'a [W<'b>]) {
    for x in xs {
        let _ = x;
    }
    let s = String::new();
    drop(s);
    drop(s);
}

fn main() {}
"#;

fn is_joined_step(r: &BorrowRefused) -> bool {
    matches!(
        r,
        BorrowRefused::Stricter(step)
            if matches!(step.form, blaze_flow_lower::Form::CallGenericUnaligned { lacks: blaze_flow_lower::CallLacks::JoinedSuperset, .. })
    )
}

#[test]
fn a_projection_tied_to_two_joined_positions_is_a_recorded_stricter_step() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/joined/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "joined",
        dir: "/joined/",
        root,
        files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"),
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let mut check = |name: &str| {
        let key = names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect("fixture body");
        let (types, borrows) = world.check_borrows(BodyRef::Free(key), alloc);
        assert!(types.is_clean(), "{name}'s types: {types:?}");
        let codes: Vec<&str> = borrows.findings.iter().map(|d| d.code).collect();
        std::println!("{name}: codes {codes:?}, refused {:?}", borrows.refused.as_ref().map(|r| std::format!("{r}")));
        (codes, borrows.refused)
    };

    // Print every verdict first.
    let rows: Vec<(&str, (Vec<&str>, Option<BorrowRefused>))> =
        ["two", "statics", "one", "escapes", "moved"].into_iter().map(|n| (n, check(n))).collect();
    let of = |n: &str| &rows.iter().find(|r| r.0 == n).expect("checked above").1;

    for n in ["two", "statics"] {
        let (codes, refused) = of(n);
        assert!(codes.is_empty(), "ORACLE `{n}`: rustc accepts it; no finding may be reported: {codes:?} {refused:?}");
        assert!(refused.as_ref().is_none_or(is_joined_step), "`{n}`: clean, or refused by the joined step only: {refused:?}");
    }

    let (codes, refused) = of("one");
    assert!(codes.is_empty() && refused.is_none(), "TWIN `one`: one joined position takes no stricter step and checks clean: {codes:?} {refused:?}");

    let (codes, refused) = of("escapes");
    assert!(!codes.is_empty() || refused.as_ref().is_some_and(is_joined_step), "TWIN `escapes`: rustc refuses it; it must not check clean: {codes:?} {refused:?}");

    let (codes, refused) = of("moved");
    assert!(codes.contains(&"E0382"), "TWIN `moved`: the move finding stands beside the step: {codes:?} {refused:?}");
}
