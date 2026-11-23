//! Projection normalization regressions for array ranges and concrete associated types.

use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_own::Alloc;

const SOURCE: &str = r#"
trait HasOutput {
    type Output;
    fn output(self) -> Self::Output;
}
struct Concrete;
impl HasOutput for Concrete {
    type Output = u8;
    fn output(self) -> u8 { 1 }
}
fn generic<T: HasOutput>(value: T) -> T::Output { value.output() }
fn concrete() {
    let value: u8 = generic(Concrete);
    let _ = value;
}
fn arrays() {
    let a = [1u8; 3];
    let s: &[u8] = &a[..];
    let v = [&a[..], &[2u8; 6][..]].concat();
    let _ = (s, v);
}
"#;

fn check_clean(world: &mut ProjectWorld<'_>, names: &blaze_scope::resolve::Names<'_>, name: &str) {
    let mut bodies = world.bodies(Alloc::global()).into_iter().filter_map(|(body, _)| {
        let matches = match body {
            BodyRef::Free(key) => names.item_text(key)?.name? == name,
            BodyRef::Member { .. } => false,
        };
        matches.then_some(body)
    });
    let body = bodies.next().unwrap_or_else(|| panic!("missing fixture body {name}"));
    assert!(bodies.next().is_none(), "ambiguous fixture body {name}");
    let verdict = world.check(body, Alloc::global());
    assert!(
        verdict.reads.is_some()
            && verdict.gaps.is_empty()
            && verdict.refused.is_none()
            && verdict.undecided.is_empty()
            && verdict.diagnostics.is_empty(),
        "{name}: {verdict:?}"
    );
}

#[test]
fn front2_eb4_projection_normalization() {
    let files = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let alloc = Alloc::global();
    // The pinned shelf, as the neighbouring ladder fixtures use: ranges and `concat` are core's.
    let shelf = format!("{}/.blaze/v2/toolchain", std::env::var("HOME").expect("HOME"));
    let tc = Toolchain::open(&mut blaze_ladder::crates::DirShelf::new(shelf)).expect("fixture toolchain");
    let new = NewFront { package: "eb4", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::Off };
    let texts = ProgramTexts::new(&files, "src/main.rs", new, &tc, blaze_build::Panic::Unwind).expect("fixture texts");
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).expect("fixture inputs");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
    let mut world = ProjectWorld::new(&names, &frame, &files, alloc).expect("front2");
    for name in ["generic", "concrete", "arrays"] {
        check_clean(&mut world, &names, name);
    }
    println!("EB4 projections: generic, concrete, arrays clean");
}
