//! A refused expansion is stored refused, never empty (rules 3 and 19): reduced from
//! reth-mdbx-sys 2.11.0, whose whole `src/lib.rs` is `include!(concat!(env!("OUT_DIR"),
//! "/bindings.rs"))` (its build script runs bindgen), and reth-libmdbx, which names
//! `ffi::MDBX_env` through `pub extern crate reth_mdbx_sys as ffi`. With no outputs of the
//! script, cargo's `OUT_DIR` ("the folder in which all output and intermediate artifacts should
//! be placed", the cargo book, "Environment variables Cargo sets for build scripts") is set for
//! the package but not known here, so `env!("OUT_DIR")` is refused (core/src/macros/mod.rs,
//! `env!`: "Inspects an environment variable at compile time"), and with it the `include!`.

use std::format;

use blaze_expand::seen::SeenRoot;
use blaze_format::Aligned;
use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind, RefusedWhy, ScopeBuilder, ScopeError, ScopeView};

use super::entry::{Texts, input};
use crate::crate_env::{CrateEnv, EnvFacts};
use crate::graph::Sysroot;
use crate::names::{PathError, Why, Written};
use crate::resolve::{SourceTexts, resolve_crates};
use crate::task::Serial;

static SYS: Texts = Texts(&[
    &[(
        "/sys/src/lib.rs",
        "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! env { () => {} }\npub mod ffi {\n    include!(concat!(env!(\"OUT_DIR\"), \"/b.rs\"));\n}\n",
    )],
    &[("/user/lib.rs", "#![no_core]\npub fn f() {}\n")],
]);

/// The texts, and crate 0's environment: a package with a build script whose outputs record
/// is not joined (`EnvFacts::script` is `None`), so its `OUT_DIR` is unknown.
struct NoOutputs {
    env: CrateEnv<'static>,
}

impl SourceTexts<'static> for NoOutputs {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        SYS.read(krate, path)
    }

    fn crate_env(&self, krate: usize) -> Option<&CrateEnv<'static>> {
        (krate == 0).then_some(&self.env)
    }
}

/// `sys::ffi::X` written in a dependent: its last segment is looked up in `ffi`, whose one
/// item, the `include!`, was refused. The answer names that call and the build script's missing
/// `OUT_DIR` ([`Why::Refused`] with [`RefusedWhy::BuildScriptOutputs`]); the refusal is held in
/// `ffi`'s scope ([`ScopeError::Refused`]) and survives the scope's record (what a dependent
/// reads). CONTROL: the old empty expansion (resolve.rs `FillCx::expand` storing
/// `DeclsBuilder::new_in(..)` for a refused call) leaves `ffi`'s scope with no error and the
/// path a plain `Why::NotFound`, failing the first assertion. Passing prints
/// "test tests::refused::a_name_behind_a_refused_include_names_the_missing_out_dir ... ok".
#[test]
fn a_name_behind_a_refused_include_names_the_missing_out_dir() {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let facts = EnvFacts {
        crate_name: "sys",
        package: Some(("sys", "2.11.0")),
        bin: None,
        seen: SeenRoot::Package,
        read_folder: "/sys/",
        build_script: true,
        script: None,
        test_or_bench: false,
        bin_exes: crate::crate_env::BinNames::NONE,
        meta: None,
    };
    let sources = NoOutputs { env: CrateEnv::of(facts, alloc).unwrap_or_else(|e| panic!("the crate's environment: {e:?}")) };
    let mut sys = input("sys", "/sys/src/lib.rs", &[]);
    sys.shown_from = "/sys/";
    let crates = [sys, input("user", "/user/lib.rs", &[("sys", 0)])];
    let names = resolve_crates(&crates, Sysroot::default(), &sources, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let user = names.root(1);
    let ffi = names
        .path(user, Namespace::Types, Written { kind: PathKind::Plain, segments: &["sys", "ffi"] }, alloc)
        .ok()
        .and_then(|found| found.whole(2))
        .unwrap_or_else(|| panic!("`sys::ffi` names the module"))
        .key;

    let found = names.path(user, Namespace::Types, Written { kind: PathKind::Plain, segments: &["sys", "ffi", "MDBX_env"] }, alloc);
    let call = match found {
        Err(PathError { module, at: 2, why: Why::Refused { module: behind, call, why: RefusedWhy::BuildScriptOutputs } }) if module == user && behind == ffi => call,
        other => panic!("`sys::ffi::MDBX_env` must be unknown behind the refused `include!` of `ffi`, naming the missing OUT_DIR; got {other:?}"),
    };
    let shown = format!("{}", found.err().unwrap_or_else(|| panic!("an error")));
    assert!(shown.contains("OUT_DIR") && shown.contains("build script"), "the error names the build script's OUT_DIR: {shown}");

    // The scope a dependent reads holds the refusal, through its record.
    let scope = &names.crates()[0].done[&ffi].scope;
    let held: ScopeError<&str, ()> = ScopeError::Refused { call, name: "OUT_DIR", why: RefusedWhy::BuildScriptOutputs };
    let in_scope = scope.errors.iter().filter(|e| matches!(e, ScopeError::Refused { .. })).map(|e| e.clone().map_path(|_| ())).collect::<std::vec::Vec<_>>();
    assert_eq!(in_scope, [held.clone()], "ffi's scope holds the one refusal");
    let bytes = scope.write(alloc).unwrap_or_else(|e| panic!("ffi's scope writes: {e:?}"));
    let buffer = Aligned::copy_from(&bytes);
    let view = ScopeView::read(&buffer).unwrap_or_else(|e| panic!("ffi's scope record reads: {e}"));
    let back = ScopeBuilder::from(&view, alloc);
    let read_back = back.errors.iter().filter(|e| matches!(e, ScopeError::Refused { .. })).map(|e| e.clone().map_path(|_| ())).collect::<std::vec::Vec<_>>();
    assert_eq!(read_back, [held], "the record keeps the refusal");
}
