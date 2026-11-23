//! A path that fails behind a failed dependency is unknown, not absent ([`crate::waits_on`]).
//! Reduced from turso's sync/engine (c1647, 2026-11-21): `turso_core` had no names this command
//! (its build script failed), `src/alloc.rs` is `pub use turso_core::alloc::*;`, and each `use
//! crate::alloc::{TursoAllocExt, TursoVecExt};` was an `error[unresolved]` naming nothing.

use std::collections::BTreeMap;
use std::string::String;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::Alloc;
use blaze_surface::{Edition, ScopeError};

use super::entry::{Texts, input};
use crate::crate_scope::{error_record, error_record_crate, resolve_crate};
use crate::graph::Sysroot;
use crate::prepares::Prepares;
use crate::resolve::aligned;
use crate::task::Serial;
use crate::waits_on::{FailedDep, WaitsOn};

/// `failed` stands for turso_core: a dependency handed as an error record. `k` is a dependency
/// that resolved.
const USER: &str = "#![no_core]
pub mod alloc {
    pub use failed::alloc::*;
}
pub mod all {
    pub use failed::*;
}
pub mod one {
    pub use failed::Thing;
}
pub mod known {
    pub struct Here;
}
pub mod via_known {
    pub use crate::known::*;
}
pub mod plain {
    pub use failed::*;
    pub use self::Gone;
}
use crate::alloc::TursoAllocExt;
use crate::all::Anything;
use crate::one::Thing;
use crate::one::Other;
use crate::via_known::Missing;
use k::m::Absent;
";

const K: &str = "#![no_core]
pub mod m {
    pub struct S;
}
";

static TEXTS: Texts = Texts(&[&[("/user/lib.rs", USER)], &[("/k/lib.rs", K)]]);

/// Each `use` error of `user`, by its path, with what it waits on when `failed` is handed as a
/// failed dependency (`with_failed`; else none is: the cause removed).
fn errors_waiting(with_failed: bool) -> BTreeMap<String, Option<String>> {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let g = bump.alloc();
    let prepares = Prepares::new_in(bump);
    let k = input("k", "/k/lib.rs", &[]);
    let k_done = resolve_crate(&k, &[], Sysroot::default(), &TEXTS, 1, &Serial, prepares, bump).unwrap_or_else(|e| panic!("k resolves: {e:?}"));
    let k_record = aligned(&k_done.record, bump);
    let failed_key = CrateItemKey(ContentId::of("blaze-scope test crate", b"failed"));
    let failed_record = aligned(&error_record(failed_key, "failed", Edition::E2021, g).unwrap_or_else(|e| panic!("the error record: {e:?}")), bump);

    // The error record's head names its crate; a resolved crate's record is no error record.
    assert_eq!(error_record_crate(failed_record), Some((failed_key, "failed")));
    assert_eq!(error_record_crate(k_record), None);

    let user = input("user", "/user/lib.rs", &[("failed", 0), ("k", 1)]);
    let done = resolve_crate(&user, &[failed_record, k_record], Sysroot::default(), &TEXTS, 0, &Serial, prepares, bump).unwrap_or_else(|e| panic!("user resolves: {e:?}"));
    let failed: Vec<FailedDep<'_>> = if with_failed {
        let (key, name) = error_record_crate(failed_record).expect("an error record");
        std::vec![FailedDep { called: Some("failed"), name, key }]
    } else {
        Vec::new()
    };
    let waits = WaitsOn::of(done.names, &failed, g);
    let mut seen = BTreeMap::new();
    for (&module, r) in done.names.done.iter() {
        for error in r.scope.errors.iter() {
            if let ScopeError::Import { path, .. } = error {
                seen.insert(path.segments.join("::"), waits.waits_on(module, error).map(String::from));
            }
        }
    }
    seen
}

/// The real case and its kin wait on `failed`: through a glob that failed into it
/// (`crate::alloc::TursoAllocExt`), a glob edge to its root (`crate::all::Anything`, and
/// `self::Gone` beside one), a named import that failed into it (`crate::one::Thing`), and the
/// failed imports themselves. The twins stay errors: `crate::one::Other` (that module's mark
/// is the one name it imported), `crate::via_known::Missing` (a glob of a module that holds no
/// such name), and `k::m::Absent`, a truly absent name in a dependency that resolved.
/// CONTROL (rule 23): with no failed dependency handed, every error stands.
#[test]
fn a_path_behind_a_failed_crate_waits_on_it_and_a_truly_absent_name_stays_an_error() {
    let seen = errors_waiting(true);
    let waits = |path: &str| -> Option<&str> {
        match seen.get(path) {
            Some(found) => found.as_deref(),
            None => panic!("no error of `{path}` among {seen:?}"),
        }
    };
    for path in ["failed::alloc", "failed::Thing", "alloc::TursoAllocExt", "all::Anything", "one::Thing"] {
        assert_eq!(waits(path), Some("failed"), "`{path}` waits on the failed crate: {seen:?}");
    }
    let gone = seen.iter().find(|(path, _)| path.ends_with("Gone")).unwrap_or_else(|| panic!("no error of `self::Gone` among {seen:?}"));
    assert_eq!(gone.1.as_deref(), Some("failed"), "a module with a glob of the failed root: {seen:?}");
    for path in ["one::Other", "via_known::Missing", "k::m::Absent"] {
        assert_eq!(waits(path), None, "`{path}` is truly absent: an error: {seen:?}");
    }

    let without = errors_waiting(false);
    assert_eq!(without.keys().collect::<Vec<_>>(), seen.keys().collect::<Vec<_>>(), "the same errors either way");
    assert!(without.values().all(Option::is_none), "the cause removed, every error stands: {without:?}");
}
