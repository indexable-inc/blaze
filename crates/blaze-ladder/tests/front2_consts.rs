//! Constants through front2 (front2/consts.rs): each constant's body lowered and checked by
//! the same loop as a fn's, then evaluated by blaze-consteval over front2's records. Needs the
//! installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::{ConstFail, Consts, int_of};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
const A: u32 = 3;
const B: u32 = A * 2 + 1;
const C: u32 = D;
const D: u32 = C;
const M: u32 = u32::MAX;
";

/// A line of the outcome, as it happens: appended to the file `FRONT2_WORK_LIST` names, since a
/// passing (or hung) test's output is not shown by the lane's check script.
fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

#[test]
fn constants_evaluate_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/consts/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "consts", dir: "/consts/", root: "/consts/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    note("names resolved");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let (b, c, m) = (lib("B"), lib("C"), lib("M"));
    let (b, c, m) = (front.keys.item(b).expect("B's key"), front.keys.item(c).expect("C's key"), front.keys.item(m).expect("M's key"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();

    // `const A: u32 = 3; const B: u32 = A * 2 + 1;`: B reads A, evaluated first.
    note("evaluating B");
    let got = consts.value(&mut records, prepared, b).map(|r| int_of(r));
    note(&format!("B: {got:?}"));
    assert!(matches!(got, Ok(Some(7))), "B is 7, got {got:?}");

    // `const C: u32 = D; const D: u32 = C;`: the language's cycle error, typed, not a hang.
    note("evaluating C");
    let got = consts.value(&mut records, prepared, c).map(|r| int_of(r));
    note(&format!("C: {got:?}"));
    match &got {
        Err(ConstFail::Cycle { shown, .. }) => note(&format!("cycle: {shown}")),
        Err(ConstFail::Eval(e)) => note(&format!("cycle: {e}")),
        other => panic!("C and D are a cycle, got {other:?}"),
    }

    // `u32::MAX` through core's own definition: printed, the gap named if it is one.
    match consts.value(&mut records, prepared, m) {
        Ok(r) => note(&format!("M: {:?}", int_of(r))),
        Err(e) => note(&format!("M (u32::MAX): {e}")),
    }
}
