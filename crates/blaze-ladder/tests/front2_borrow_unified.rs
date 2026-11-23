//! Relations the lowering makes STRICTER than Rust's outside the call mechanism are recorded as
//! coarse steps (blaze-flow-lower `Form::Unified`, the coarse-relations rule: `Lowered::coarse`),
//! so a finding that depends on one is never reported: the body is refused by the step's name.
//!
//! The rules:
//! - The Reference, "Arithmetic and Logical Binary Operators": `x + y` on `&'a u32` and `&'b u32`
//!   is `<&'a u32 as Add<&'b u32>>::add`, whose impl relates `'a` and `'b` to nothing. The
//!   lowering made both operands and the result equal (lib.rs `bin_done`): `'a == 'b`. Now
//!   exact on primitive operands (nothing related, no step); on others still recorded as
//!   `Unified::BinaryOperator`.
//! - The Reference, "Subtyping and Variance": `Option<T>` is covariant in `T`, so
//!   `Option<&'static str>` is a subtype of `Option<&'a str>`. The lowering takes an ADT's type
//!   parameter invariant when its variance is not filed (variance.rs), which demands
//!   `'a: 'static`. Recorded as `Unified::VarianceUnknown`.
//!
//! - `binop`, `widen`: rustc accepts each. No finding may be reported: clean, or refused by the
//!   step's name.
//! - TWIN `binop_one` (rule 23): one operand position, nothing unified: no step; checked clean.
//! - TWIN `same`: the unknown variance is taken (a step) but no finding depends on it: checked
//!   clean, not refused.
//! - TWINS `binop_escapes`, `narrow`: rustc refuses each; neither may check clean.
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
fn binop<'a, 'b>(x: &'a u32, y: &'b u32) -> &'a u32 {
    let _s = x + y;
    x
}

fn binop_one<'a>(x: &'a u32, y: u32) -> u32 {
    x + y
}

fn binop_escapes<'a, 'b>(x: &'a u32, y: &'b u32) -> &'b u32 {
    let _s = x + y;
    x
}

fn widen<'a>(x: Option<&'static str>) -> Option<&'a str> {
    x
}

fn same<'a>(x: Option<&'a str>) -> Option<&'a str> {
    x
}

fn narrow<'a>(x: Option<&'a str>) -> Option<&'static str> {
    x
}

fn main() {}
"#;

fn is_unified(r: &BorrowRefused, why: fn(blaze_flow_lower::Unified) -> bool) -> bool {
    matches!(r, BorrowRefused::Stricter(step) if matches!(step.form, blaze_flow_lower::Form::Unified { why: w } if why(w)))
}

fn binary(w: blaze_flow_lower::Unified) -> bool {
    w == blaze_flow_lower::Unified::BinaryOperator
}

fn variance(w: blaze_flow_lower::Unified) -> bool {
    matches!(w, blaze_flow_lower::Unified::VarianceUnknown { .. })
}

#[test]
fn unified_relations_are_recorded_stricter_steps() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/unified/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "unified",
        dir: "/unified/",
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
        ["binop", "binop_one", "binop_escapes", "widen", "same", "narrow"].into_iter().map(|n| (n, check(n))).collect();
    let of = |n: &str| &rows.iter().find(|r| r.0 == n).expect("checked above").1;

    let (codes, refused) = of("binop");
    assert!(codes.is_empty(), "ORACLE `binop`: rustc accepts it; no finding may be reported: {codes:?} {refused:?}");
    assert!(refused.is_none(), "`binop`: primitive operands are related exactly (nothing): checked clean, no step: {refused:?}");
    let _ = binary;

    let (codes, refused) = of("binop_one");
    assert!(codes.is_empty() && refused.is_none(), "TWIN `binop_one`: no step, checked clean: {codes:?} {refused:?}");

    let (codes, refused) = of("binop_escapes");
    assert!(!codes.is_empty() || refused.is_some(), "TWIN `binop_escapes`: rustc refuses it; it must not check clean: {codes:?} {refused:?}");

    let (codes, refused) = of("widen");
    assert!(codes.is_empty(), "ORACLE `widen`: rustc accepts it (Option is covariant); no finding may be reported: {codes:?} {refused:?}");
    assert!(refused.as_ref().is_none_or(|r| is_unified(r, variance)), "`widen`: clean, or refused by VarianceUnknown: {refused:?}");

    let (codes, refused) = of("same");
    assert!(codes.is_empty() && refused.is_none(), "TWIN `same`: a step with no finding depending on it checks clean: {codes:?} {refused:?}");

    let (codes, refused) = of("narrow");
    assert!(!codes.is_empty() || refused.is_some(), "TWIN `narrow`: rustc refuses it; it must not check clean: {codes:?} {refused:?}");
}
