//! A small toolchain shelf for tests, here and in the driver's: core, alloc, std, proc_macro
//! (one pack) and bstd (another), all target-side, cut for one triple. The one home of the
//! fixture: blaze-world's tests open it from memory, blaze-cli's write it into a home.

use std::string::{String as StdString, ToString};
use std::vec::Vec as StdVec;

use blaze_fetch::Mode;
use blaze_fetch::pack::{Builder, Kind, Limits};
use blaze_own::Alloc;
use blaze_toolchain::record::{self, CrateIn, Role, TargetIn, UnitIn};
use blaze_toolchain::{ToolchainId, ToolchainView};

/// A tree pack of `files` (path, text), built in `alloc` (a test's, from
/// `blaze_own::testing::heap()`).
pub fn pack(files: &[(&str, &str)], alloc: Alloc<'_>) -> StdVec<u8> {
    let limits = Limits { most_files: 64, most_bytes: 1 << 20, most_path: 256 };
    let mut builder = Builder::new(Kind::Tree, [7; 32], limits, 0, alloc).expect("a builder");
    for (path, text) in files {
        builder.file(path, Mode::File, text.as_bytes()).expect("a file");
    }
    builder.finish().expect("a pack").to_vec()
}

/// The text of the fixture's std when a test does not give its own.
pub const STD: &str = "#![no_std]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }";

/// The fixture's shelf for `triple`: its id and each file, by its 64-hex name, as a shelf
/// holds them. `std` is std's crate root; `cfg` the target's atoms; `alloc` the test's.
pub fn shelf(
    triple: &'static str,
    std: &str,
    cfg: &'static [(&'static str, Option<&'static str>)],
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    shelf_of(&[(triple, cfg)], std, false, alloc)
}

/// [`shelf`] whose std depends on a crate of library/ beneath it, `cfg_if` (`Role::Library`,
/// on core), as the real std does: the toolchain list holds it, and a program that needs no
/// Rust std does not open it.
pub fn shelf_with_std_dep(
    triple: &'static str,
    std: &str,
    cfg: &'static [(&'static str, Option<&'static str>)],
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    shelf_of(&[(triple, cfg)], std, true, alloc)
}

/// [`shelf`] cut for `triple` and for `wasm32-wasip1` (`macro_cfg` its atoms), the macro side a
/// member proc macro is built for (D89 (a), rule 10): the same units for both targets.
pub fn shelf_with_macro_side(
    triple: &'static str,
    std: &str,
    cfg: &'static [(&'static str, Option<&'static str>)],
    macro_cfg: &'static [(&'static str, Option<&'static str>)],
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    shelf_of(&[(triple, cfg), ("wasm32-wasip1", macro_cfg)], std, false, alloc)
}

/// The fixture's shelf cut for each of `targets` (its triple and atoms), every target with the
/// same units; with `std_dep`, std depends on `cfg_if`, a sixth crate.
fn shelf_of(
    targets: &[(&'static str, &'static [(&'static str, Option<&'static str>)])],
    std: &str,
    std_dep: bool,
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    let library = pack(
        &[
            ("core/src/lib.rs", "#![no_core]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }"),
            ("alloc/src/lib.rs", "#![no_std]\npub mod vec {}"),
            ("std/src/lib.rs", std),
            ("proc_macro/src/lib.rs", "#![no_std]\npub struct TokenStream;"),
            ("cfg_if/src/lib.rs", "#![no_std]"),
        ],
        alloc,
    );
    let bstd = pack(&[("crates/blaze-std/src/lib.rs", "#![no_std]\npub mod prelude { pub mod blaze_2026_11 {} }")], alloc);
    let digest = |p: &[u8]| blaze_toolchain::shelf::pack_digest(p).expect("a pack's footer");
    let row = |name: &'static str, lib: &'static str, dir: &'static str, role: Role, p: &[u8]| CrateIn {
        name,
        version: "1.0.0",
        edition: "2024",
        lib,
        root: "src/lib.rs",
        dir,
        role,
        pack_kind: 3,
        pack_len: p.len() as u64,
        pack: digest(p),
    };
    let crates = [
        row("core", "core", "core", Role::Core, &library),
        row("alloc", "alloc", "alloc", Role::Alloc, &library),
        row("std", "std", "std", Role::Std, &library),
        row("proc_macro", "proc_macro", "proc_macro", Role::ProcMacro, &library),
        row("blaze-std", "bstd", "crates/blaze-std", Role::Bstd, &bstd),
        row("cfg_if", "cfg_if", "cfg_if", Role::Library, &library),
    ];
    // Without `std_dep`, the shelf is the five crates it always was.
    let crates = if std_dep { &crates[..] } else { &crates[..5] };
    let lib = |krate: u32, deps: &'static [(&'static str, u32)]| UnitIn {
        krate,
        kind: blaze_toolchain::UnitKind::Lib,
        for_host: false,
        root: "src/lib.rs",
        features: &[],
        deps,
    };
    let plain = [lib(0, &[]), lib(1, &[("core", 0)]), lib(2, &[("alloc", 1), ("core", 0)]), lib(3, &[("core", 0), ("std", 2)]), lib(4, &[("alloc", 1), ("core", 0)])];
    // With `std_dep`: std (unit 2) also on `cfg_if` (unit 5, on core).
    let with_dep = [
        lib(0, &[]),
        lib(1, &[("core", 0)]),
        lib(2, &[("alloc", 1), ("cfg_if", 5), ("core", 0)]),
        lib(3, &[("core", 0), ("std", 2)]),
        lib(4, &[("alloc", 1), ("core", 0)]),
        lib(5, &[("core", 0)]),
    ];
    let units: &[UnitIn<'_>] = if std_dep { &with_dep } else { &plain };
    let cut: StdVec<TargetIn<'_>> = targets.iter().map(|(triple, cfg)| TargetIn { triple, units, cfg }).collect();
    let manifest = record::write(crates, &cut, alloc).expect("a record").to_vec();
    let id = ToolchainView::id_of_bytes(&manifest);
    let mut files = StdVec::new();
    files.push((blaze_toolchain::Hex::of(&id.0).as_str().to_string(), manifest));
    for p in [library, bstd] {
        files.push((blaze_toolchain::Hex::of(&digest(&p)).as_str().to_string(), p));
    }
    (id, files)
}
