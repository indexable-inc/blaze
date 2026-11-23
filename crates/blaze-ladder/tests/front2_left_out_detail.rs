//! A survey's left-out row carries the fn's typed refusal and names the item that refusal is
//! about by its path (front2 program/left_out.rs; rule 19: an error names the thing).
//!
//! The fault this guards (build census 5 over tempo's `main` walk, port27/tempo-run/
//! build-census-5.txt): 3,499 of 6,170 left-out fns were one class, "check gap: RefusedImpl",
//! that no census could split, because the row was `(fn, class)` and the refused impl and its
//! reason (`Gap::RefusedImpl { impl_, place, why }`) were dropped where it was filed.
//!
//! The rules the program rests on (the Rust Reference):
//! - "Trait and lifetime bounds": `need::<G<u8>>` holds only when an impl gives `G<u8>: Tr`;
//!   the one impl of `Tr` is written for `G<Missing>`, and `Missing` names nothing ("Paths":
//!   a path that resolves to no item is an error of the program, rustc's E0412), so its header
//!   cannot be built: front2 cannot say whether it applies at `G<u8>`, and `same_shape`'s check
//!   is the gap naming that impl (tests/front2_items.rs
//!   `an_unbuildable_header_is_a_named_gap_of_its_own_questions_only`, the same form).
//! - "Implementations": an impl has no name; front2's one printer shows it by its header as
//!   written, `<impl Tr for G<Missing>>` (item_shown.rs `lib_path`).
//!
//! It needs no toolchain: the program is `#![no_core]` (as tests/front2_survey.rs).
#![feature(allocator_api)]

use std::alloc::Global;

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::check::CheckFail;
use blaze_ladder::front2::program::{Front2Program, Front2Refusal, LeftAt};
use blaze_ladder::front2::world_items::Gap;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;

/// `main` calls `same_shape`, whose call `need(G(1u8))` asks `G<u8>: Tr` of the one candidate
/// impl, whose header names a type that does not exist.
const PROGRAM: &str = "#![feature(no_core, lang_items)]
#![no_core]
#![no_main]

#[lang = \"sized\"]
trait Sized {}

#[lang = \"copy\"]
trait Copy {}

struct G<T>(T);

trait Tr {}

impl Tr for G<Missing> {}

fn need<T: Tr>(_t: T) -> i32 {
    0
}

fn same_shape() -> i32 {
    need(G(1u8))
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    same_shape()
}
";

/// Whether the survey's fn path `f` is the crate-root fn `name`.
fn is_fn(f: &str, name: &str) -> bool {
    f == name || f.ends_with(&*format!("::{name}"))
}

/// `same_shape`'s row is "check gap: RefusedImpl" and carries the gap itself; its split names
/// the refused impl by its header as written and ends with the gap's why; its place is the
/// impl's own key (item-relative, never a `file:line`). CONTROL: the class-only form, the
/// row as it was before (`{fn}: {class}`, which `LeftOut`'s `Display` still prints), holds no
/// path of the impl: what the census could not split.
///
/// Prints: `LEFT OUT DETAIL: same_shape: check gap: RefusedImpl | the impl <impl Tr for
/// G<Missing>> (key ..): .. | at Some(Lib(..))` and
/// `test a_left_out_row_names_the_refused_impl_and_why ... ok`
#[test]
fn a_left_out_row_names_the_refused_impl_and_why() {
    let files = [SourceFile { path: "src/main.rs", text: PROGRAM }];
    let tc = Toolchain::none(blaze_own::Alloc::global());
    let new = NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).unwrap_or_else(|e| panic!("the program's texts: {e}"));
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
    let mut left_out = Vec::new_in(Global);
    let _program = Front2Program::survey(&names, &frame, &files, &mut left_out, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("no survey: {e}"));
    for (f, row) in left_out.iter() {
        println!("LEFT OUT DETAIL: {f}: {} | at {:?}", row.group(), row.at);
    }
    let of: Vec<_> = left_out.iter().filter(|(f, _)| is_fn(f, "same_shape")).collect();
    assert_eq!(of.len(), 1, "front2 leaves out `same_shape` (its bound's one candidate impl has a refused header)");
    let (f, row) = of[0];
    assert_eq!(&*row.class, "check gap: RefusedImpl", "the class as the survey always filed it");
    // The typed refusal itself, not a string: the check's gap, with the impl and its why.
    let Front2Refusal::Check(CheckFail::Gap { gaps, .. }) = &row.refusal else { panic!("the row keeps the check's gap: {:?}", row.refusal) };
    let Some(Gap::RefusedImpl { impl_, why, .. }) = gaps.first() else { panic!("the first gap is the refused impl: {gaps:?}") };
    assert!(row.split.contains("impl Tr for G<Missing>"), "the split names the refused impl by its header (the one printer): {}", row.split);
    assert!(!why.is_empty() && row.split.ends_with(&**why), "the split ends with the gap's why ({why}): {}", row.split);
    assert_eq!(row.at, Some(LeftAt::Lib(*impl_)), "the place is the refused impl's own key");
    assert!(row.group().starts_with("check gap: RefusedImpl | the impl "), "the census groups by class and split: {}", row.group());
    // CONTROL: the class-only row, as filed before this change.
    let class_only = format!("{f}: {row}");
    assert!(!class_only.contains("impl Tr for G"), "control: the class-only row holds no path of the impl: {class_only}");
}
