//! A storage-layout-shaped module whose value names are emitted by nested macros.
//!
//! The real tempo proc macro emits `pub mod slots` and constructs each
//! `<FIELD>_BYTES` identifier with `Ident::new`/`format_ident!`. This fixture uses
//! the nearest macro-by-example shape available to the front end: one macro writes
//! the module, a nested macro writes the plain bytes constant, and metavariable
//! concatenation constructs its identifier. The sibling slot and offset constants
//! are read in the same body. A missing name remains an E0425 control.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const SOURCE: &str = r#"#![feature(macro_metavar_expr_concat)]
fn main() {}

macro_rules! write_bytes {
    ($base:ident) => { pub const ${concat($base, _BYTES)}: usize = 1; };
}

macro_rules! write_slots {
    () => {
        pub mod slots {
            pub const FIELD: usize = 1;
            pub const FIELD_OFFSET: usize = 0;
            write_bytes!(FIELD);
        }
    };
}

write_slots!();

fn generated_bytes() -> usize {
    slots::FIELD + slots::FIELD_OFFSET + slots::FIELD_BYTES
}

fn missing_bytes() -> usize {
    slots::MISSING_BYTES
}
"#;

#[test]
fn macro_generated_slots_bytes_are_values_in_front2() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let root = "/eb5/src/main.rs";
    let files = [(root.to_string(), SOURCE.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "eb5", dir: "/eb5/", root, files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).expect("fixture and standard library");
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("fixture names");
    let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("front2");
    let bodies = world.bodies(alloc);

    let find = |name: &str| {
        bodies.iter().find_map(|&(body, _)| match body {
            BodyRef::Free(key) if names.item_text(key)?.name == Some(name) => Some(body),
            _ => None,
        }).unwrap_or_else(|| panic!("missing fixture body `{name}`"))
    };

    let generated = world.check(find("generated_bytes"), alloc);
    assert!(generated.reads.is_some() && generated.gaps.is_empty() && generated.undecided.is_empty() && generated.refused.is_none(), "generated_bytes: {generated:?}");
    assert!(generated.diagnostics.is_empty(), "generated_bytes: {generated:?}");
    println!("generated_bytes: clean");

    let missing = world.check(find("missing_bytes"), alloc);
    assert!(missing.reads.is_some() && missing.gaps.is_empty() && missing.undecided.is_empty() && missing.refused.is_none(), "missing_bytes: {missing:?}");
    assert!(missing.diagnostics.iter().any(|d| d.code == "E0425" && d.text.contains("MISSING_BYTES")), "missing_bytes: {missing:?}");
    println!("missing_bytes: cannot find value `MISSING_BYTES` in module `slots`");
}
