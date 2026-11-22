//! The library features of a small std-shaped crate under two targets' cfg, the forms that stop
//! the walk, and the tree's own `library/` (what blaze's toolchain is cut from) for the macro
//! side's `wasm32-wasip1` and the host.

use std::println;
use std::string::String as StdString;
use std::vec::Vec as StdVec;

use crate::resolve::SourceTexts;
use crate::task::Cfg;

use super::{LibFeatures, LibFeaturesOpts, Listed, ScanError};

/// A crate's files by absolute path.
struct Files(&'static [(&'static str, &'static str)]);

impl SourceTexts<'static> for Files {
    fn read(&self, _krate: usize, path: &str) -> Option<&'static str> {
        self.0.iter().find(|(p, _)| *p == path).map(|(_, text)| *text)
    }
}

const LIB: &str = r##"//! A std in small.
#![stable(feature = "rust1", since = "1.0.0")]
#![allow(dead_code)]

pub mod os;

#[unstable(feature = "gone", issue = "none")]
#[cfg(target_os = "none")]
pub fn gone() {}

pub struct S {
    #[stable(feature = "field_f", since = "1.0.0")]
    pub f: u8,
    #[cfg(windows)]
    #[unstable(feature = "win_field", issue = "none")]
    pub g: core::cell::Cell<(u8, u16)>,
}

impl S {
    #[unstable(feature = "in_impl", issue = "none")]
    pub fn get<'a>(&'a self) -> char {
        let _ = r#"#[unstable(feature = "raw")] }"#;
        let _ = "feature = \"not_a_mention\"";
        '{'
    }
}

macro_rules! m {
    () => {
        #[unstable(feature = "in_macro", issue = "none")]
        fn x() {}
    };
}

include!("docs.rs");
"##;

const OS: &str = r#"
cfg_select! {
    target_family = "wasm" => {
        #[cfg(any(target_env = "p1", target_env = "p2"))]
        pub mod wasi;
    }
    _ => {}
}

#[cfg_attr(unix, unstable(feature = "unix_only", issue = "none"))]
pub mod x {
    #[path = "../y.rs"]
    mod y;
}
"#;

const WASI: &str = "pub mod fs;\n";
const FS: &str = "//! wasi's fs\n#![unstable(feature = \"wasi_ext\", issue = \"71213\")]\npub fn f() {}\n";
const Y: &str = "#[stable(feature = \"by_path\", since = \"1.0.0\")]\npub struct Y;\n";
const DOCS: &str = "#[stable(feature = \"from_include\", since = \"1.0.0\")]\nmod doc_mod {}\n";

static FILES: &[(&str, &str)] = &[
    ("/1/std/src/lib.rs", LIB),
    ("/1/std/src/os/mod.rs", OS),
    ("/1/std/src/os/wasi/mod.rs", WASI),
    ("/1/std/src/os/wasi/fs.rs", FS),
    ("/1/std/src/os/y.rs", Y),
    ("/1/std/src/docs.rs", DOCS),
];

fn listed<'a>(files: &'static [(&'static str, &'static str)], atoms: &[(&'static str, Option<&'static str>)], alloc: blaze_own::Alloc<'a>) -> Result<Listed<'static, 'a>, ScanError<'a>> {
    let cfg = Cfg::from_atoms(atoms, alloc.clone());
    let mut features = LibFeatures::new(LibFeaturesOpts { alloc: alloc.clone() });
    features.scan(&Files(files), 0, files[0].0, &cfg)?;
    Ok(features.listed(alloc))
}

const WASM: &[(&str, Option<&str>)] = &[("target_env", Some("p1")), ("target_family", Some("wasm")), ("target_os", Some("wasi"))];
const MAC: &[(&str, Option<&str>)] = &[("target_family", Some("unix")), ("target_os", Some("macos")), ("unix", None)];

/// The features each target's cfg declares: `wasi_ext` only where std's `os::wasi` is (the
/// `cfg_select!` arm and the module's cfg), `unix_only` only under `cfg_attr(unix)`; a field's,
/// an impl item's, a `#[path]` file's and an `include!`d file's are read; a cfg'd-out item's
/// never; a macro body's is a mention only; strings, raw strings and a `'{'` move nothing.
/// Prints "lib_features: wasm and host listed".
/// Control: take the `cfg_select!` arm whatever its predicate and the host declares `wasi_ext`.
#[test]
fn lib_features_follow_each_targets_cfg() {
    let alloc = blaze_own::testing::heap().alloc();
    let wasm = listed(FILES, WASM, alloc.clone()).expect("the wasm listing");
    assert_eq!(&wasm.declared[..], &["by_path", "field_f", "from_include", "in_impl", "rust1", "wasi_ext"]);
    assert_eq!(&wasm.mentioned[..], &["in_macro"]);
    assert_eq!(wasm.files, 6);
    let mac = listed(FILES, MAC, alloc.clone()).expect("the host listing");
    assert_eq!(&mac.declared[..], &["by_path", "field_f", "from_include", "in_impl", "rust1", "unix_only"]);
    assert_eq!(&mac.mentioned[..], &["in_macro"]);
    assert_eq!(mac.files, 4, "os/wasi is not read for the host");
    println!("lib_features: wasm and host listed");
}

/// A file a module names that the crate lacks, and a `mod x;` inside a macro's tokens, stop
/// the listing by name: neither is passed over, since the file could declare a feature.
/// Prints "lib_features: missing and unfollowed refused".
#[test]
fn lib_features_refuse_what_could_hide_a_file() {
    let alloc = blaze_own::testing::heap().alloc();
    static MISSING: &[(&str, &str)] = &[("/1/c/src/lib.rs", "pub mod nowhere;\n")];
    match listed(MISSING, MAC, alloc.clone()) {
        Err(ScanError::Missing { path }) => assert_eq!(path.as_str(), "/1/c/src/nowhere/mod.rs"),
        other => panic!("not refused as missing: {other:?}"),
    }
    static HIDDEN: &[(&str, &str)] = &[("/1/c/src/lib.rs", "wrap! { pub mod hidden; }\n")];
    assert!(matches!(listed(HIDDEN, MAC, alloc.clone()), Err(ScanError::Unfollowed { at: 22, .. })));
    static OFF: &[(&str, &str)] = &[("/1/c/src/lib.rs", "#[cfg(windows)]\nmod nowhere;\n")];
    assert!(listed(OFF, MAC, alloc.clone()).is_ok(), "an item that is off is not read");
    static OFF_FILE: &[(&str, &str)] = &[("/1/c/src/lib.rs", "#![cfg(windows)]\nmod nowhere;\n")];
    assert!(listed(OFF_FILE, MAC, alloc).is_ok(), "a module whose own cfg is off is not read");
    println!("lib_features: missing and unfollowed refused");
}

/// `cfg_if!` (std's backtrace): each chain's first branch that holds is walked, its `mod x;`
/// followed; the others are passed over, their modules never read; a shape that is not the
/// crate's grammar is refused.
/// Prints "lib_features: cfg_if followed".
/// Control: walk every branch and the host reads `wasm.rs`, which the fixture lacks (Missing).
#[test]
fn lib_features_follow_cfg_if() {
    let alloc = blaze_own::testing::heap().alloc();
    static CFG_IF: &[(&str, &str)] = &[
        (
            "/1/b/src/lib.rs",
            "cfg_if::cfg_if! {\n    if #[cfg(miri)] {\n        mod miri;\n    } else if #[cfg(\n        any(unix, windows,)\n    )] {\n        mod unix;\n    } else {\n        mod wasm;\n    }\n}\ncfg_if! { if #[cfg(windows)] { mod win; } }\n",
        ),
        ("/1/b/src/unix.rs", "#[unstable(feature = \"unix_trace\", issue = \"none\")]\npub fn t() {}\n"),
    ];
    let mac = listed(CFG_IF, MAC, alloc.clone()).expect("the host listing");
    assert_eq!(&mac.declared[..], &["unix_trace"]);
    assert_eq!(mac.files, 2);
    static ODD: &[(&str, &str)] = &[("/1/b/src/lib.rs", "cfg_if! { mod hidden; }\n")];
    assert!(matches!(listed(ODD, MAC, alloc), Err(ScanError::Unfollowed { at: 10, .. })));
    println!("lib_features: cfg_if followed");
}

/// Files read from a folder on disk, each kept for the test's life.
struct Disk;

impl SourceTexts<'static> for Disk {
    fn read(&self, _krate: usize, path: &str) -> Option<&'static str> {
        let text = std::fs::read_to_string(path).ok()?;
        Some(StdString::leak(text))
    }
}

/// The library blaze's toolchain is cut from (the tree's own `library/`, which the world's
/// toolchain packs hold): its crates of `staged_api` that std reaches (core, alloc, std,
/// std_detect, unwind, panic_unwind, panic_abort) for `wasm32-wasip1` and
/// `aarch64-apple-darwin`, each read with no refusal; `wasi_ext` is declared for the first
/// only, `rust1` and `core_c_str` for both. Prints the counts.
/// Control: drop `cfg_if!` from the walk and std's `backtrace` is refused (`Unfollowed`).
#[test]
fn lib_features_of_the_pinned_library() {
    let alloc = blaze_own::testing::heap().alloc();
    let library = StdString::leak(std::format!("{}/../../library", env!("CARGO_MANIFEST_DIR")));
    let mut shared: StdVec<(&'static str, Option<&'static str>)> = StdVec::new();
    shared.extend([("target_endian", Some("little")), ("panic", Some("unwind")), ("fmt_debug", Some("full")), ("target_has_atomic", None), ("target_has_atomic_load_store", None)]);
    for width in ["8", "16", "32", "64", "ptr"] {
        shared.push(("target_has_atomic", Some(width)));
        shared.push(("target_has_atomic_load_store", Some(width)));
        shared.push(("target_has_atomic_equal_alignment", Some(width)));
    }
    let mut wasm = shared.clone();
    wasm.extend([("target_arch", Some("wasm32")), ("target_os", Some("wasi")), ("target_env", Some("p1")), ("target_family", Some("wasm")), ("target_pointer_width", Some("32")), ("target_vendor", Some("unknown")), ("target_abi", Some(""))]);
    let mut mac = shared;
    mac.extend([("target_arch", Some("aarch64")), ("target_os", Some("macos")), ("target_env", Some("")), ("target_family", Some("unix")), ("unix", None), ("target_pointer_width", Some("64")), ("target_vendor", Some("apple")), ("target_abi", Some("")), ("target_has_atomic", Some("128")), ("target_feature", Some("neon")), ("target_thread_local", None)]);
    for (name, atoms, wasi) in [("wasm32-wasip1", &wasm, true), ("aarch64-apple-darwin", &mac, false)] {
        let cfg = Cfg::from_atoms(atoms, alloc.clone());
        let mut features = LibFeatures::new(LibFeaturesOpts { alloc: alloc.clone() });
        for krate in ["core", "alloc", "std", "std_detect", "unwind", "panic_unwind", "panic_abort"] {
            let root = StdString::leak(std::format!("{library}/{krate}/src/lib.rs"));
            if let Err(error) = features.scan(&Disk, 0, root, &cfg) {
                panic!("{name}: {krate}: {error}");
            }
        }
        let listed = features.listed(alloc.clone());
        assert_eq!(listed.declared.binary_search(&"wasi_ext").is_ok(), wasi, "{name}: wasi_ext");
        for known in ["rust1", "core_c_str", "core_ffi_c", "alloc_c_string", "alloc_ffi", "error_in_core"] {
            assert!(listed.declared.binary_search(&known).is_ok(), "{name}: {known} not declared");
        }
        println!("lib_features: {name}: {} declared, {} mentioned only, {} files", listed.declared.len(), listed.mentioned.len(), listed.files);
    }
}
