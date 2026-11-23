//! Block-local values written by hand and by a macro are visible throughout their block.
//!
//! The Reference, "Statements" / "Item declarations", puts a block item's name in scope for
//! the whole block. Macro-by-example item names follow that same block scope. A module declared
//! in a block is the path root for the values it contains, including when both declarations were
//! written by one macro expansion.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = r#"
fn main() {}
fn direct_static() -> u8 { static V: u8 = 1; V }
macro_rules! static_value { () => {{ static V: u8 = 1; V }}; }
fn macro_static() -> u8 { static_value!() }
macro_rules! block_slots { () => {{ mod slots { pub const A: u8 = 1; } slots::A }}; }
fn macro_block_mod() -> u8 { block_slots!() }
fn missing_value() -> u8 { MISSING() }
"#;

#[test]
fn block_values_written_by_macros_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/eb3/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "eb3", dir: "/eb3/", root: "/eb3/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    for (name, clean) in [
        ("direct_static", true),
        ("macro_static", true),
        ("macro_block_mod", true),
        ("missing_value", false),
    ] {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, blaze_own::Alloc::global())
            .ok()
            .and_then(|path| path.whole(1))
            .map(|def| def.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"));
        store.ensure(&names, &mut front.keys, Want::Fn(key)).unwrap_or_else(|e| panic!("{name}: no head: {e}"));
        let body = lower_fn(&names, prepared, &mut front.keys, &mut store, key).unwrap_or_else(|e| panic!("{name}: does not lower: {e}"));
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("body spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let result = check_body(&mut records, &body, &scope);
        match (clean, result) {
            (true, Ok(done)) if done.diags.is_empty() => println!("{name}: clean"),
            (false, Err(CheckFail::Diags { diags, .. })) => {
                let shown = format!("{diags:?}");
                assert!(shown.contains("MISSING"), "{name}: {shown}");
                println!("{name}: cannot find function `MISSING` in this scope");
            }
            (true, Ok(_)) => panic!("{name}: unexpected diagnostics"),
            (true, Err(error)) => panic!("{name}: {error:?}"),
            (false, Ok(_)) => panic!("{name}: checked clean unexpectedly"),
            (false, Err(error)) => panic!("{name}: unexpected failure {error:?}"),
        }
    }
}
