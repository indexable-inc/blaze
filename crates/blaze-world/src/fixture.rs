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
    let library = pack(
        &[
            ("core/src/lib.rs", "#![no_core]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }"),
            ("alloc/src/lib.rs", "#![no_std]\npub mod vec {}"),
            ("std/src/lib.rs", std),
            ("proc_macro/src/lib.rs", "#![no_std]\npub struct TokenStream;"),
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
    ];
    let lib = |krate: u32, deps: &'static [(&'static str, u32)]| UnitIn {
        krate,
        kind: blaze_toolchain::UnitKind::Lib,
        for_host: false,
        root: "src/lib.rs",
        features: &[],
        deps,
    };
    let units = [lib(0, &[]), lib(1, &[("core", 0)]), lib(2, &[("alloc", 1), ("core", 0)]), lib(3, &[("core", 0), ("std", 2)]), lib(4, &[("alloc", 1), ("core", 0)])];
    let targets = [TargetIn { triple, units: &units, cfg }];
    let manifest = record::write(&crates, &targets, alloc).expect("a record").to_vec();
    let id = ToolchainView::id_of_bytes(&manifest);
    let mut files = StdVec::new();
    files.push((blaze_toolchain::Hex::of(&id.0).as_str().to_string(), manifest));
    for p in [library, bstd] {
        files.push((blaze_toolchain::Hex::of(&digest(&p)).as_str().to_string(), p));
    }
    (id, files)
}
