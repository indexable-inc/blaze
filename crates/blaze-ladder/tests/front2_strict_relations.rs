//! STRICT RELATIONS REFUSE ONLY ON A FINDING (borrows.rs `check_borrows`, blaze-flow-lower
//! `Lowered::coarse`). Awaiting a future held in a local (`fut.await`) relates its value to the
//! future's positions, a relation stricter than Rust's (`AwaitValueUnrelated { why: NotACall }`,
//! coroutine.rs `await_unrelated`). A body clean under it stands checked; a body with a finding
//! that reads regions is refused by that step's name (`BorrowRefused::Stricter`), the region
//! findings dropped; a move finding (E0382) reads no region and is reported either way.
//! Twin (rule 23): the same bodies awaiting the call directly take no stricter step, and their
//! E0502 is reported. NOT YET RUN (written 2026-11-21 by a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowRefused;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"
async fn h(x: &u8) -> &u8 { x }

async fn local_clean() {
    let x = 1u8;
    let fut = h(&x);
    let r = fut.await;
    let _ = *r;
}

async fn local_move_and_conflict() {
    let x = 1u8;
    let fut = h(&x);
    let r = fut.await;
    let _ = *r;
    let s = String::new();
    drop(s);
    drop(s);
    let mut v = 1u8;
    let a = &mut v;
    let b = &v;
    *a = 2;
    let _ = *b;
}

async fn direct_move_and_conflict() {
    let x = 1u8;
    let r = h(&x).await;
    let _ = *r;
    let s = String::new();
    drop(s);
    drop(s);
    let mut v = 1u8;
    let a = &mut v;
    let b = &v;
    *a = 2;
    let _ = *b;
}

fn main() {}
"#;

#[test]
fn a_stricter_step_refuses_only_a_body_with_a_region_finding_and_keeps_move_findings() {
    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/strict/src/main.rs";
    let files = [(root.to_owned(), SOURCE.to_owned())];
    let user = UserCrate {
        name: "strict", dir: "/strict/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
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
        assert!(types.is_clean() && types.undecided.is_empty() && types.reads.is_some(), "{name}: {types:?}");
        let codes: Vec<&str> = borrows.findings.iter().map(|d| d.code).collect();
        std::println!("{name}: codes {codes:?}, refused {:?}", borrows.refused.as_ref().map(|r| std::format!("{r}")));
        (codes, borrows.refused)
    };

    // Clean under the stricter relation: checked, no refusal.
    let (codes, refused) = check("local_clean");
    assert!(codes.is_empty() && refused.is_none(), "local_clean: {codes:?} {refused:?}");

    // A region finding beside the stricter step: refused by its name, E0502 dropped, E0382 kept.
    let (codes, refused) = check("local_move_and_conflict");
    assert_eq!(codes, ["E0382"], "the move finding stands, the region finding is dropped");
    let Some(r @ BorrowRefused::Stricter(step)) = refused else { panic!("refused by the stricter step: {refused:?}") };
    assert!(
        matches!(step.form, blaze_flow_lower::Form::AwaitValueUnrelated { why: blaze_flow_lower::AwaitUnrelated::NotACall, .. }),
        "the step by name: {step:?}"
    );
    // The class the "refused by cause" line counts by leads the note (blaze-cli engine_bodies.rs
    // `first_refusal_class` keeps the bracket).
    assert!(std::format!("{r}").starts_with("[AwaitValueUnrelated/NotACall] "), "{r}");

    // TWIN: no stricter step, both findings reported, nothing refused.
    let (mut codes, refused) = check("direct_move_and_conflict");
    codes.sort_unstable();
    assert_eq!(codes, ["E0382", "E0502"], "direct: both findings");
    assert!(refused.is_none(), "direct: {refused:?}");
}
