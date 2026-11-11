//! `durable_write`: a write that waits for the disk, with no `// durable: <why>` comment on its
//! line or the line above saying what must survive a crash.
//!
//! The rule: a write waits for a sync only where losing it at a crash loses something no build
//! can make again, and the line says what. A sync holds its asker until the disk has the bytes
//! (seconds on a busy disk: a build's answer was once held 4 s behind the sync of a file the
//! next build would have written again). Derived data, which a build makes again and whose
//! reader checks it before use, takes `Effect::WriteUnsynced` and `Effect::RenameUnsynced`.
//!
//! A finding is:
//!
//! - a `blaze_core::Effect::Write`, `Effect::Rename` or `Effect::Sync` built (a record
//!   literal; a pattern that names one builds nothing);
//! - a call of `File::sync_all` or `File::sync_data` (std's, tokio's);
//! - a call of blaze-core's `write_atomically`, which is `Effect::Write` carried out;
//! - a call of `fsync` or `fdatasync` (libc's, rustix's) or rustix's `fcntl_fullfsync`.
//!
//! The variant and the functions are resolved, so a renamed import is found. Tests are not
//! linted: a test of a driver asks for syncs to see them happen. There is no fix: only a
//! person knows whether the data can be made again.

use base_db::Db;
use hir_def::{FunctionId, Lookup as _, VariantId, hir::Expr, signatures::FunctionSignature};

use super::{Finding, Lint, alloc::written_callee, exempt, facts};
use crate::check::Cx;

const LINT: Lint = Lint::DurableWrite;

/// The variants of `blaze_core::Effect` that complete only once the disk has them.
const SYNCED: &[&str] = &["Write", "Rename", "Sync"];

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    if cx.is_in_test(cx.root()) {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let what = match expr {
            Expr::RecordLit { .. } => effect(db, infer, id),
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                written_callee(infer, body, id).and_then(|(f, _)| synced_call(db, f))
            }
            _ => None,
        };
        let Some(what) = what else { continue };
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let message = format!(
            "{what} waits for the disk with no `// durable: <why>` saying what must survive a crash"
        );
        // alloc: a message's part
        let note = "a synced write holds its asker until the disk has the bytes, seconds on a busy disk; data a build \
                    can make again, checked by its reader before use, takes `Effect::WriteUnsynced` or \
                    `Effect::RenameUnsynced`"
            .to_owned();
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// What the record literal `id` builds, when it is one of [`SYNCED`] of `blaze_core::Effect`.
fn effect(db: &Db, infer: &hir_ty::InferenceResult<'_>, id: hir_def::hir::ExprId) -> Option<String> {
    let Some(VariantId::EnumVariantId(variant)) = infer.variant_resolution_for_expr(id) else {
        return None;
    };
    let name = hir::EnumVariant::from(variant).name(db);
    let synced = SYNCED.iter().copied().find(|it| *it == name.as_str())?;
    let enum_ = hir::Enum::from(variant.lookup(db).parent);
    (enum_.name(db).as_str() == "Effect"
        && facts::crate_is(db, enum_.module(db).krate(db), "blaze_core"))
    // alloc: a message's part
    .then(|| format!("`Effect::{synced}`"))
}

/// What a call of `f` is, when it returns only once the disk has what was written.
fn synced_call(db: &Db, f: FunctionId) -> Option<String> {
    let krate = hir::Function::from(f).module(db).krate(db);
    let is = |name: &str| facts::crate_is(db, krate, name);
    let name = FunctionSignature::of(db, f).name.as_str();
    let found = if is("std") || is("tokio") {
        matches!(name, "sync_all" | "sync_data")
            && facts::impl_adt(db, f).is_some_and(|it| it.name(db).as_str() == "File")
    } else if is("blaze_core") {
        name == "write_atomically"
    } else if is("libc") {
        matches!(name, "fsync" | "fdatasync")
    } else if is("rustix") {
        matches!(name, "fsync" | "fdatasync" | "fcntl_fullfsync")
    } else {
        false
    };
    // alloc: a message's part
    found.then(|| match facts::impl_adt(db, f) {
        Some(adt) => format!("`{}::{name}`", adt.name(db).as_str()),
        None => format!("`{name}`"),
    })
}

#[cfg(test)]
mod tests {
    use crate::tests::lint;

    const FOUND: &str = "with no `// durable: <why>`";

    /// A `blaze_core` with the effects, and an `std` with `fs::File`.
    const CRATES: &str = r#"
//- minicore: sized, copy, clone, option, result, fn
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2015 { pub use core::prelude::rust_2015::*; }
    pub mod rust_2018 { pub use core::prelude::rust_2018::*; }
    pub mod rust_2021 { pub use core::prelude::rust_2021::*; }
    pub mod rust_2024 { pub use core::prelude::rust_2024::*; }
}
pub mod fs {
    pub struct File(u32);
    impl File {
        pub fn sync_all(&self) -> Result<(), ()> { Ok(()) }
        pub fn sync_data(&self) -> Result<(), ()> { Ok(()) }
        pub fn set_len(&self, _len: u64) -> Result<(), ()> { Ok(()) }
    }
}
//- /blaze_core/lib.rs crate:blaze_core deps:std
pub struct Tag(pub u64);
pub enum Effect {
    Append { file: u32, tag: Tag },
    Sync { file: u32, tag: Tag },
    Write { file: u32, tag: Tag },
    WriteUnsynced { file: u32, tag: Tag },
    Rename { from: u32, to: u32, tag: Tag },
    RenameUnsynced { from: u32, to: u32, tag: Tag },
}
//- /src/lib.rs crate:main deps:std,blaze_core
"#;

    fn found(main: &str) -> String {
        let fixture = format!("{}{}", CRATES.trim_start_matches('\n'), main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::durable_write")
    }

    /// The bug in miniature: an output placed from the store, which the next build places
    /// again, written whole and renamed into place with a sync of each.
    #[test]
    fn a_synced_write_of_derived_data_with_no_reason_is_found() {
        let out = found(
            r#"
use blaze_core::{Effect, Effect::Rename as Moved, Tag};
pub fn place(temp: u32, file: u32) -> [Effect; 3] {
    [
        Effect::Write { file: temp, tag: Tag(0) },
        Effect::Sync { file: temp, tag: Tag(1) },
        Moved { from: temp, to: file, tag: Tag(2) },
    ]
}
pub fn flush(file: &std::fs::File) {
    let _ = file.sync_all();
    let _ = file.sync_data();
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 5, "{out}");
        assert!(out.contains("`Effect::Write` waits for the disk"), "{out}");
        assert!(out.contains("`Effect::Sync` waits for the disk"), "{out}");
        assert!(out.contains("`Effect::Rename` waits for the disk"), "the renamed variant is resolved:\n{out}");
        assert!(out.contains("`File::sync_all` waits for the disk"), "{out}");
        assert!(out.contains("takes `Effect::WriteUnsynced` or `Effect::RenameUnsynced`"), "{out}");
    }

    /// The fix: the same output placed with no sync is not a finding; neither is a synced
    /// write that says what must survive, a pattern that names a synced effect, or a call of
    /// `File` that waits for no disk.
    #[test]
    fn unsynced_effects_a_stated_reason_and_patterns_are_left_alone() {
        let out = found(
            r#"
use blaze_core::{Effect, Tag};
pub fn place(temp: u32, file: u32) -> [Effect; 2] {
    [
        Effect::WriteUnsynced { file: temp, tag: Tag(0) },
        Effect::RenameUnsynced { from: temp, to: file, tag: Tag(1) },
    ]
}
pub fn commit(temp: u32, file: u32) -> [Effect; 3] {
    [
        // durable: the manifest is the one record of which files a flush wrote
        Effect::Write { file: temp, tag: Tag(0) },
        Effect::Sync { file: temp, tag: Tag(1) }, // durable: the manifest, before its rename
        // durable: the rename is the commit of the flush
        Effect::Rename { from: temp, to: file, tag: Tag(2) },
    ]
}
pub fn synced(effect: &Effect) -> bool {
    match effect {
        Effect::Write { .. } | Effect::Rename { .. } | Effect::Sync { .. } => true,
        _ => false,
    }
}
pub fn cut(file: &std::fs::File) {
    let _ = file.set_len(0);
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// A test asks for syncs to see a driver carry them out: not linted.
    #[test]
    fn a_test_is_not_linted() {
        let out = found(
            r#"
use blaze_core::{Effect, Tag};
#[cfg(test)]
mod tests {
    use super::*;
    pub fn asks() -> Effect {
        Effect::Sync { file: 1, tag: Tag(0) }
    }
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }
}
