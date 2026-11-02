//! Allocation budgets of the lowerings' hot paths: a crate whose files parse is the same few
//! reads at any size, and once printing's and imports' tables are built every path printed and
//! every import looked for is a read of them, allocating nothing.

use blaze_alloc_count::{assert_at_most, measure};
use hir_def::ModuleDefId;

use crate::{cx::Cx, lower::parse, print::paths::Def};

/// A crate of `n` modules, each a file, and a dependency that re-exports a type.
fn fixture(n: usize) -> String {
    let mut out = String::from("//- /main.rs crate:other:main deps:dep\n");
    for i in 0..n {
        out.push_str(&format!("mod m{i};\n"));
    }
    out.push_str("fn main() {}\n");
    for i in 0..n {
        out.push_str(&format!("//- /m{i}.rs\npub struct S{i};\npub fn f{i}() {{}}\n"));
    }
    out.push_str("//- /lib.rs crate:dep\npub mod inner { pub struct Dep; }\npub use inner::Dep;\n");
    out
}

/// `check` given the context of the crate `main` of `fixture(n)`.
fn with_cx<R>(n: usize, check: impl FnOnce(&Cx<'_, '_>) -> R) -> R {
    let checked = crate::tests::checked(&fixture(n));
    let db = &checked.db;
    hir::attach_db(db, || {
        let krate = hir::check::workspace_crates(db)
            .into_iter()
            .find(|it| it.display_name(db).is_some_and(|name| name.to_string() == "main"))
            .expect("the fixture has a crate `main`");
        let frame = crate::tests::frame();
        let cx = Cx::new(db, krate, checked.files(), &frame);
        check(&cx)
    })
}

/// The allocations of `parse::parsed` of a crate of `n` clean files, its parses memoized.
fn parse_allocations(n: usize) -> u64 {
    with_cx(n, |cx| {
        let parsed = parse::parsed(cx);
        assert!(parsed.found.is_empty() && !parsed.fatal, "the fixture's files parse");
        measure(|| parse::parsed(cx)).1.allocations
    })
}

/// A crate whose files parse is one memoized read per file: as many allocations at 4 files as
/// at 32.
#[test]
fn a_clean_crate_parses_in_constant_allocations() {
    let (small, large) = (parse_allocations(4), parse_allocations(32));
    assert_eq!(small, large, "the clean path's allocations grow with the crate");
    assert!(small <= 2, "the clean path allocates {small} times");
}

/// Trimmed names, visible parents, re-export names and declarations by name, asked once the
/// tables are built, are hash probes and slices.
#[test]
fn path_and_import_lookups_allocate_nothing() {
    with_cx(8, |cx| {
        let paths = cx.paths();
        let names = ["S0", "f3", "Dep", "inner", "m5", "not_declared"];
        let defs: Vec<Def> = names
            .iter()
            .flat_map(|name| paths.decls(cx).named(name))
            .filter_map(|it| ModuleDefId::try_from(it.def).ok())
            .filter_map(Def::of)
            .collect();
        assert!(defs.len() >= 5, "the fixture declares the names looked for: {defs:?}");
        for &def in &defs {
            paths.trimmed(cx, def);
        }
        assert_at_most(0, "a path or import lookup", || {
            for name in names {
                std::hint::black_box(paths.decls(cx).named(name));
            }
            for &def in &defs {
                std::hint::black_box(paths.trimmed(cx, def));
                if let Some(parent) = paths.visible_parent(cx, def) {
                    std::hint::black_box(paths.reexport_name(cx, parent, def));
                }
            }
        });
    });
}
