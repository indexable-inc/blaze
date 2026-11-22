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

/// The text of the fixture's core: its prelude modules, and two items with heads a member body
/// can call (`::core::fixture_one()`, `::core::FixtureCell(x).get()`), so a test reads a
/// toolchain crate's head (blaze-cli engine_seeds_tests.rs).
///
/// It also has `rustc_coherence_is_core` and one inherent impl of a primitive, at the end: a
/// probe of a primitive's method (`n.wrapping_add(1)`) has an answer, and asks the
/// incoherent-impl fact the real core answers (blaze-cli engine_scope_reads_tests.rs).
///
/// And `fixture_len`, whose signature holds a constant (`[u8; FIXTURE_LEN]`): a toolchain
/// head a body's seed freezes only with a constant evaluator (blaze-cli
/// engine_refusal_causes_tests.rs). And `FixtureBad`, whose field's length (`FIXTURE_BAD`,
/// `0 - 1`) does not evaluate: a toolchain head whose freeze fails for a reason a body that
/// reads it must quote (the same file).
pub const CORE: &str = "#![no_core]\n#![feature(rustc_attrs)]\n#![rustc_coherence_is_core]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }\npub fn fixture_one() -> u8 {\n    1\n}\npub struct FixtureCell(pub u8);\nimpl FixtureCell {\n    pub fn get(&self) -> u8 {\n        self.0\n    }\n}\nimpl u32 {\n    pub const fn wrapping_add(self, rhs: u32) -> u32 {\n        self + rhs\n    }\n}\npub const FIXTURE_LEN: usize = 2;\npub fn fixture_len(_x: [u8; FIXTURE_LEN]) -> u8 {\n    1\n}\npub const FIXTURE_BAD: usize = 0 - 1;\npub struct FixtureBad {\n    pub x: [u8; FIXTURE_BAD],\n}\n";

// Some refusal tests need a core without Sized. Both shelves use the same borrow-checking items.
macro_rules! core_with_langs {
    ($features:literal, $markers:literal, $items:literal) => {
        concat!(
            $features,
            "#![no_core]\n#![feature(rustc_attrs, lang_items)]\n#![rustc_coherence_is_core]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }\npub mod marker {\n    #[lang = \"copy\"]\n    pub trait Copy {}\n",
            $markers,
            "}\npub mod ops {\n    #[lang = \"drop\"]\n    pub trait Drop {\n        fn drop(&mut self);\n    }\n}\npub mod boxed {\n    #[lang = \"owned_box\"]\n    pub struct Box<T>(*mut T);\n}\n",
            $items,
            "pub fn fixture_one() -> u8 {\n    1\n}\npub struct FixtureCell(pub u8);\nimpl FixtureCell {\n    pub fn get(&self) -> u8 {\n        self.0\n    }\n}\nimpl u32 {\n    pub const fn wrapping_add(self, rhs: u32) -> u32 {\n        self + rhs\n    }\n}\npub const FIXTURE_LEN: usize = 2;\npub fn fixture_len(_x: [u8; FIXTURE_LEN]) -> u8 {\n    1\n}\npub const FIXTURE_BAD: usize = 0 - 1;\npub struct FixtureBad {\n    pub x: [u8; FIXTURE_BAD],\n}\n",
        )
    };
}

/// [`CORE`] with the three lang items the borrow check's facts ask for (blaze-ladder front2
/// borrows.rs `BorrowLangs::of`: `copy`, `drop`, `owned_box`), declared as the real core and
/// alloc declare them (`core::marker::Copy`, `core::ops::Drop`, `alloc::boxed::Box`; the
/// fixture's `Box` is in core so every program links it). Without `copy`, a body naming an ADT
/// is not borrow-checked (`FactUnknown { fact: "Copy", why: NoLangItem }`); [`CORE`] keeps that
/// state for the tests that show the refusal (blaze-cli engine_check_tests.rs).
pub const CORE_WITH_LANGS: &str = core_with_langs!("", "", "");

const CORE_WITH_ASYNC_LANGS: &str = core_with_langs!(
    "#![feature(auto_traits, negative_impls)]\n",
    r#"    #[lang = "sized"]
    pub trait Sized {}
    pub unsafe auto trait Send {}
    impl<T: ?Sized> !Send for *const T {}
    #[lang = "sync"]
    pub unsafe auto trait Sync {}
"#,
    r#"pub mod future {
    #[lang = "future_trait"]
    pub trait Future {
        #[lang = "future_output"]
        type Output;
    }
    pub trait IntoFuture {
        type Output;
        type IntoFuture: Future<Output = Self::Output>;
        #[lang = "into_future"]
        fn into_future(self) -> Self::IntoFuture;
    }
    impl<F: Future> IntoFuture for F {
        type Output = F::Output;
        type IntoFuture = F;
        fn into_future(self) -> F { self }
    }
}
"#
);

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
    shelf_of(&[(triple, cfg)], CORE, std, Shape::Plain, alloc)
}

/// [`shelf`] whose core is [`CORE_WITH_LANGS`]: the borrow check's lang items are declared, so
/// a body naming an ADT is borrow-checked (blaze-cli engine_borrow_forms_tests.rs).
pub fn shelf_with_langs(
    triple: &'static str,
    std: &str,
    cfg: &'static [(&'static str, Option<&'static str>)],
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    shelf_of(&[(triple, cfg)], CORE_WITH_LANGS, std, Shape::Plain, alloc)
}

/// The lang shelf with the core definitions an async fn and its callers use.
pub fn shelf_with_async_langs(
    triple: &'static str,
    std: &str,
    cfg: &'static [(&'static str, Option<&'static str>)],
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    shelf_of(&[(triple, cfg)], CORE_WITH_ASYNC_LANGS, std, Shape::Plain, alloc)
}

/// [`shelf_with_std_dep`] where bstd also depends on `blaze_ids` (`Role::Blaze`), a crate that
/// uses std's prelude (`Vec<u64>`, as crates/blaze-ids does): its row's sysroot is `std` and
/// it holds the edge `("std", 2)` the cutter writes for it.
pub fn shelf_with_bstd_naming_std(
    triple: &'static str,
    std: &str,
    cfg: &'static [(&'static str, Option<&'static str>)],
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    shelf_of(&[(triple, cfg)], CORE, std, Shape::BstdNamesStd, alloc)
}

/// Which units the fixture's shelf holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// core, alloc, std, proc_macro, bstd.
    Plain,
    /// And std on `cfg_if`.
    StdDep,
    /// And bstd on `blaze_ids`, which names std.
    BstdNamesStd,
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
    shelf_of(&[(triple, cfg)], CORE, std, Shape::StdDep, alloc)
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
    shelf_of(&[(triple, cfg), ("wasm32-wasip1", macro_cfg)], CORE, std, Shape::Plain, alloc)
}

/// The fixture's shelf cut for each of `targets` (its triple and atoms), every target with the
/// same units, as `shape` says, `core` core's root text. Each unit's sysroot is its root
/// text's (core `#![no_core]`: none; the `#![no_std]` ones: core; `blaze_ids`: std), with the
/// edges the cutter writes.
fn shelf_of(
    targets: &[(&'static str, &'static [(&'static str, Option<&'static str>)])],
    core: &str,
    std: &str,
    shape: Shape,
    alloc: Alloc<'_>,
) -> (ToolchainId, StdVec<(StdString, StdVec<u8>)>) {
    use blaze_toolchain::ImplicitSysroot;
    let library = pack(
        &[
            // Two items a body calls through a head, a fn and a struct with an inherent method:
            // what blaze-cli's engine tests read as toolchain heads (engine_seeds_tests.rs).
            ("core/src/lib.rs", core),
            ("alloc/src/lib.rs", "#![no_std]\npub mod vec {}"),
            ("std/src/lib.rs", std),
            ("proc_macro/src/lib.rs", "#![no_std]\npub struct TokenStream;"),
            ("cfg_if/src/lib.rs", "#![no_std]"),
        ],
        alloc,
    );
    let bstd = pack(
        &[
            ("crates/blaze-ids/src/lib.rs", "pub struct Bits<S = Vec<u64>>(S);"),
            ("crates/blaze-std/src/lib.rs", "#![no_std]\npub mod prelude { pub mod blaze_2026_11 {} }"),
        ],
        alloc,
    );
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
        row("blaze-ids", "blaze_ids", "crates/blaze-ids", Role::Blaze, &bstd),
    ];
    // Plain: the five crates it always was.
    let crates = match shape {
        Shape::Plain => &crates[..5],
        Shape::StdDep => &crates[..6],
        Shape::BstdNamesStd => &crates[..],
    };
    let lib = |krate: u32, deps: &'static [(&'static str, u32)]| UnitIn {
        krate,
        kind: blaze_toolchain::UnitKind::Lib,
        for_host: false,
        root: "src/lib.rs",
        features: &[],
        deps,
        sysroot: match krate {
            0 => ImplicitSysroot::None,
            6 => ImplicitSysroot::Std,
            _ => ImplicitSysroot::Core,
        },
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
    // With `BstdNamesStd`: bstd (unit 4) also on `blaze_ids` (unit 6), whose root uses std's
    // prelude, so its row holds the edges the cutter writes: `core` and `std`.
    let names_std = [
        lib(0, &[]),
        lib(1, &[("core", 0)]),
        lib(2, &[("alloc", 1), ("cfg_if", 5), ("core", 0)]),
        lib(3, &[("core", 0), ("std", 2)]),
        lib(4, &[("alloc", 1), ("blaze_ids", 6), ("core", 0)]),
        lib(5, &[("core", 0)]),
        lib(6, &[("core", 0), ("std", 2)]),
    ];
    let units: &[UnitIn<'_>] = match shape {
        Shape::Plain => &plain,
        Shape::StdDep => &with_dep,
        Shape::BstdNamesStd => &names_std,
    };
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
