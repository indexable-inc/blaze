//! Where an include may read (include_place.rs `include_place`, decision D111 amending D60,
//! 2026-11-16): a WORKSPACE MEMBER's include may read any file under the workspace root (tempo's
//! `crates/precompiles/src/zone_verifier/attestation.rs` reads
//! `../../../nitro-attestation/testdata/aws_attestation_2026_01_03.b64`), named by its path
//! under the workspace root, so the read list holds the tree's path and nothing of the machine
//! (D96: a member sees `/workspace/<dir>`), and one outside its own package is flagged (a
//! warning finding at the call, blaze-cli names/include_outside.rs); a DEPENDENCY's include
//! stays inside its own package; a path leaving the workspace root, or absolute and not under
//! the crate's own seen root, is `None`: the host answers `Included::Outside` and the expansion
//! refuses it by its path and the rule (`IncludeFailure::Outside`: "the path leaves the unit's
//! source root: decision D60").
//!
//! The rule the paths follow is the pinned library's (core `macros/mod.rs`, `include_str!`):
//! "The file is located relative to the current file (similarly to how modules are found)".

use blaze_expand::seen::SeenRoot;
use blaze_own::Alloc;

use crate::include_place::include_place;
use crate::included::included_file;

/// The tree as the names' texts hold it: the checkout (the workspace root) is `/0`.
const MEMBER: &str = "/0/crates/precompiles";
const CALLING: &str = "/0/crates/precompiles/src/zone_verifier/attestation.rs";
const SIBLING: &str = "../../../nitro-attestation/testdata/aws_attestation_2026_01_03.b64";

fn member() -> Option<SeenRoot<'static>> {
    Some(SeenRoot::member("crates/precompiles").unwrap_or_else(|e| panic!("{e}")))
}

/// The path and whether it left the package; `None` refused.
fn at(package: &str, seen: Option<SeenRoot<'_>>, calling: &str, written: &str) -> Option<(std::string::String, bool)> {
    let place = include_place(package, seen, calling, written, Alloc::global())?;
    // `included_file` is the same answer without the flag (one home, rule 30).
    let same = included_file(package, seen, calling, written, Alloc::global()).map(|path| std::string::String::from(path.as_str()));
    assert_eq!(same.as_deref(), Some(place.path.as_str()));
    Some((std::string::String::from(place.path.as_str()), place.outside_package))
}

/// (a) A member reads a sibling directory outside its package: allowed, named by its path
/// under the workspace root (`/0/crates/nitro-attestation/..`, never a spelling with `..`), and
/// flagged as outside its package; inside its package: read, not flagged. Without the member
/// arm in `include_place` the sibling is `None` (the package bound): tempo's next stop after
/// test-genesis.json.
///
/// Prints nothing; passes as `tests::include_roots::a_member_reads_a_sibling_directory_by_its_workspace_path ... ok`.
#[test]
fn a_member_reads_a_sibling_directory_by_its_workspace_path() {
    assert_eq!(at(MEMBER, member(), CALLING, SIBLING), Some(("/0/crates/nitro-attestation/testdata/aws_attestation_2026_01_03.b64".into(), true)));
    assert_eq!(at(MEMBER, member(), CALLING, "../../tests/assets/x.json"), Some(("/0/crates/precompiles/tests/assets/x.json".into(), false)));
    // `CARGO_MANIFEST_DIR` (D96 (4)): the member's own seen root, absolute, is its package.
    assert_eq!(at(MEMBER, member(), CALLING, "/workspace/crates/precompiles/data/a.bin"), Some(("/0/crates/precompiles/data/a.bin".into(), false)));
    // The workspace's root package (`dir` empty): its package is the workspace root.
    let root = Some(SeenRoot::member("").unwrap_or_else(|e| panic!("{e}")));
    assert_eq!(at("/0", root, "/0/src/lib.rs", "../data/a.json"), Some(("/0/data/a.json".into(), false)));
    assert_eq!(at("/0", root, "/0/src/lib.rs", "../../README.md"), None, "the root package's `../..` leaves the workspace");
}

/// (b) A dependency's include stays inside its own package: the same climb out of its package
/// is `None` (refused by the expansion as `Outside`, D60), inside it is read, never flagged.
///
/// Prints nothing; passes as `tests::include_roots::a_dependency_reads_only_inside_its_package ... ok`.
#[test]
fn a_dependency_reads_only_inside_its_package() {
    let dep = "/3/precompiles-1.0.0";
    let calling = "/3/precompiles-1.0.0/src/zone_verifier/attestation.rs";
    assert_eq!(at(dep, Some(SeenRoot::Package), calling, SIBLING), None);
    assert_eq!(at(dep, Some(SeenRoot::Package), calling, "../../data/a.bin"), Some(("/3/precompiles-1.0.0/data/a.bin".into(), false)));
    // A crate with no environment (no package known) is held to its package too.
    assert_eq!(at(dep, None, calling, SIBLING), None);
}

/// (c) A member's path leaving the workspace root, or absolute and not under its own seen root
/// (the machine's `/etc/..`, another member's `/workspace/..` path), is `None`: refused by
/// name, never read.
///
/// Prints nothing; passes as `tests::include_roots::a_member_path_leaving_the_workspace_or_absolute_is_refused ... ok`.
#[test]
fn a_member_path_leaving_the_workspace_or_absolute_is_refused() {
    assert_eq!(at(MEMBER, member(), CALLING, "../../../../../outside.txt"), None, "leaves the workspace root");
    assert_eq!(at(MEMBER, member(), CALLING, "/etc/passwd"), None, "absolute: the machine's");
    assert_eq!(at(MEMBER, member(), CALLING, "/workspace/crates/nitro-attestation/testdata/a.b64"), None, "absolute: another member's");
}
