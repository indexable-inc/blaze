//! Storage markers stay for a code generator that emits lifetime markers, and go before the
//! interpreter's optimized bodies at the levels rustc removes them.

use hir_ty_mir::mir::{MirOptLevel, MirOptions};

use crate::mir;

const BORROWED: &str = "fn borrowed() -> u32 { let x = 5u32; let r = &x; *r }";

#[test]
fn kept_for_a_code_generator_that_emits_lifetime_markers() {
    let options = MirOptions {
        level: MirOptLevel::Debug,
        ub_checks: false,
        lifetime_markers: true,
        inline: false,
    };
    let mir = mir(BORROWED, "borrowed", options);
    assert!(mir.contains("StorageLive(") && mir.contains("StorageDead("), "{mir}");
}

#[test]
fn removed_where_rustc_removes_them() {
    let mir = mir(BORROWED, "borrowed", MirOptions::at(MirOptLevel::Debug));
    assert!(!mir.contains("StorageLive(") && !mir.contains("StorageDead("), "{mir}");
}

#[test]
fn kept_at_release_as_rustc_does() {
    let mir = mir(BORROWED, "borrowed", MirOptions::at(MirOptLevel::Release));
    assert!(mir.contains("StorageLive("), "{mir}");
}
