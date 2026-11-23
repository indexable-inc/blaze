//! The owner's hello world, as written, resolves its names to bstd's and core's items through
//! `blaze_scope::resolve::resolve_crates`, with the toolchain's crates read from the installed,
//! pinned packs (src/crates.rs). The fixture is his four-line manifest's edition and his
//! `main.rs`, inline. It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, and
//! stops naming it when it is not there.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{DefKind, Namespace, PathKind};

/// His `Cargo.toml`'s edition line.
const EDITION: &str = "blaze-2026.11";

/// His `src/main.rs`, as written.
const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    io.need_out()?.line(cx, \"Hello, world!\")?;
    Ok(())
}
";

#[test]
fn his_main_names_bstds_and_cores_items() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let root = names.root(0);
    // (path, namespace, the crate its definition is in, its name, its kind when not `Other`).
    let expected: [(&str, Namespace, &str, Option<&str>, Option<DefKind>); 5] = [
        ("Cx", Namespace::Types, "bstd", Some("Cx"), None),
        ("Out", Namespace::Types, "bstd", Some("Out"), None),
        ("Error", Namespace::Types, "bstd", Some("Error"), None),
        ("Result", Namespace::Types, "core", Some("Result"), Some(DefKind::Enum)),
        // A variant has no item text of its own: its crate is what is checked.
        ("Ok", Namespace::Values, "core", None, None),
    ];
    for (path, ns, krate, name, kind) in expected {
        let found = names::path(&names, root, ns, Written { kind: PathKind::Plain, segments: &[path] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{path}` from his crate root: {e}"));
        let def = found.whole(1).unwrap_or_else(|| panic!("`{path}`: only {} of 1 segments named", found.consumed));
        let at = names.crate_of(def.key).map(|c| crates[c].name);
        assert_eq!(at, Some(krate), "`{path}` names an item of {at:?}, not of `{krate}`");
        if let Some(name) = name {
            assert_eq!(names.item_text(def.key).and_then(|t| t.name), Some(name), "`{path}` names another item");
        }
        if let Some(kind) = kind {
            assert_eq!(def.kind, kind, "`{path}`'s kind");
        }
    }
    // One name only: bstd is `std` under the blaze edition, and `bstd` is not bound.
    let bstd = names::path(&names, root, Namespace::Types, Written { kind: PathKind::Plain, segments: &["bstd", "cx", "Cx"] }, Alloc::global());
    assert!(bstd.is_err(), "`bstd::cx::Cx` resolved, yet only `std` names bstd: {bstd:?}");
}
