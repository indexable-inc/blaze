//! The order cargo sorts units in, which is the order of the graph it prints and of each
//! unit's dependencies: the order `Ord` derives for cargo's `UnitInner`, `TargetInner` and
//! `Profile`, field by field, written out here over blaze-manifest's types. Nothing here
//! allocates: the graph ranks targets and profiles with these once per build and sorts units by
//! the ranks.

use std::cmp::Ordering;

use blaze_manifest::profiles::{DebugInfo, Lto, PanicStrategy, Profile, ProfileRoot, Strip, StripInner};
use blaze_manifest::targets::{CrateType, Target, TargetKind};
use cargo_util_schemas::manifest::{TomlDebugInfo, TomlTrimPaths};

/// A string's first eight bytes, big-endian and zero-padded: `prefix(a) < prefix(b)` implies
/// `a < b`, so a sort compares the strings only when their prefixes are equal.
pub(crate) fn prefix(s: &str) -> u64 {
    let bytes = s.as_bytes();
    let mut word = [0u8; 8];
    let n = bytes.len().min(8);
    word[..n].copy_from_slice(&bytes[..n]);
    u64::from_be_bytes(word)
}

/// `a` and `b` in string order, their prefixes compared first.
pub(crate) fn name(a: &str, b: &str) -> Ordering {
    prefix(a).cmp(&prefix(b)).then_with(|| a.cmp(b))
}

fn crate_type(it: &CrateType) -> (u8, &str) {
    match it {
        CrateType::Bin => (0, ""),
        CrateType::Lib => (1, ""),
        CrateType::Rlib => (2, ""),
        CrateType::Dylib => (3, ""),
        CrateType::Cdylib => (4, ""),
        CrateType::Staticlib => (5, ""),
        CrateType::ProcMacro => (6, ""),
        CrateType::Other(name) => (7, name),
    }
}

fn target_kind(kind: &TargetKind) -> (u8, &[CrateType]) {
    match kind {
        TargetKind::Lib(types) => (0, types),
        TargetKind::Bin => (1, &[]),
        TargetKind::Test => (2, &[]),
        TargetKind::Bench => (3, &[]),
        TargetKind::ExampleLib(types) => (4, types),
        TargetKind::ExampleBin => (5, &[]),
        TargetKind::CustomBuild => (6, &[]),
    }
}

fn target_kinds(a: &TargetKind, b: &TargetKind) -> Ordering {
    let ((x, xs), (y, ys)) = (target_kind(a), target_kind(b));
    x.cmp(&y).then_with(|| xs.iter().map(crate_type).cmp(ys.iter().map(crate_type)))
}

/// cargo's `TargetInner` order. `doc_scrape_examples`, its last field, is left out: two
/// targets of one package are told apart well before it.
pub(crate) fn target(a: &Target, b: &Target) -> Ordering {
    target_kinds(&a.kind, &b.kind)
        .then_with(|| name(&a.name, &b.name))
        .then_with(|| a.name_inferred.cmp(&b.name_inferred))
        .then_with(|| a.bin_name.cmp(&b.bin_name))
        .then_with(|| a.src_path.cmp(&b.src_path))
        .then_with(|| a.required_features.cmp(&b.required_features))
        .then_with(|| a.tested.cmp(&b.tested))
        .then_with(|| a.benched.cmp(&b.benched))
        .then_with(|| a.doc.cmp(&b.doc))
        .then_with(|| a.doctest.cmp(&b.doctest))
        .then_with(|| a.harness.cmp(&b.harness))
        .then_with(|| a.for_host.cmp(&b.for_host))
        .then_with(|| a.proc_macro.cmp(&b.proc_macro))
        .then_with(|| a.edition.cmp(&b.edition))
}

fn root(it: ProfileRoot) -> u8 {
    match it {
        ProfileRoot::Release => 0,
        ProfileRoot::Debug => 1,
    }
}

fn lto(it: &Lto) -> (u8, bool, &str) {
    match it {
        Lto::Off => (0, false, ""),
        Lto::Bool(on) => (1, *on, ""),
        Lto::Named(name) => (2, false, name),
    }
}

fn debuginfo(it: DebugInfo) -> TomlDebugInfo {
    it.into_inner()
}

fn panic(it: PanicStrategy) -> u8 {
    match it {
        PanicStrategy::Unwind => 0,
        PanicStrategy::Abort => 1,
        PanicStrategy::ImmediateAbort => 2,
    }
}

fn strip(it: &Strip) -> (u8, &str) {
    match it.into_inner() {
        StripInner::None => (0, ""),
        StripInner::Named(name) => (1, name),
    }
}

/// What cargo compares a profile that sets no `trim-paths` as.
// alloc: none, an empty `Vec` in a static.
static NO_TRIM_PATHS: TomlTrimPaths = TomlTrimPaths::Values(Vec::new());

fn trim_paths(it: &Option<TomlTrimPaths>) -> &TomlTrimPaths {
    it.as_ref().unwrap_or(&NO_TRIM_PATHS)
}

/// cargo's derived `Profile` order: the deferred `debuginfo` and `strip` compare as what they
/// hold, as cargo's own `Ord` for them does.
pub(crate) fn profile(a: &Profile, b: &Profile) -> Ordering {
    a.name
        .cmp(&b.name)
        .then_with(|| a.opt_level.cmp(&b.opt_level))
        .then_with(|| root(a.root).cmp(&root(b.root)))
        .then_with(|| lto(&a.lto).cmp(&lto(&b.lto)))
        .then_with(|| a.codegen_backend.cmp(&b.codegen_backend))
        .then_with(|| a.codegen_units.cmp(&b.codegen_units))
        .then_with(|| debuginfo(a.debuginfo).cmp(&debuginfo(b.debuginfo)))
        .then_with(|| a.split_debuginfo.cmp(&b.split_debuginfo))
        .then_with(|| a.debug_assertions.cmp(&b.debug_assertions))
        .then_with(|| a.overflow_checks.cmp(&b.overflow_checks))
        .then_with(|| a.rpath.cmp(&b.rpath))
        .then_with(|| a.incremental.cmp(&b.incremental))
        .then_with(|| panic(a.panic).cmp(&panic(b.panic)))
        .then_with(|| strip(&a.strip).cmp(&strip(&b.strip)))
        .then_with(|| a.rustflags.cmp(&b.rustflags))
        .then_with(|| trim_paths(&a.trim_paths).cmp(trim_paths(&b.trim_paths)))
        .then_with(|| a.hint_mostly_unused.cmp(&b.hint_mostly_unused))
}
