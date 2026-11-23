//! D189 step 5, S2: the crate's merged impl table under `BlockScopes::Bodies` is keyed by each
//! surface body's IMPL ROWS digest (blaze-scope body_scope/merged.rs), so an edit to a body's
//! statements that touches no impl rechecks nothing downstream, and an edit to the impl does.
//!
//! The world: crate `up` writes `fn f() { .. impl Tr for X { type A = Local; .. } .. }`. A
//! dependent `down` that reads `<X as Tr>::A` reads the merged table by its key; one recheck of
//! `down` is one edit whose merged key differs from the base's. Three edits of `f`:
//! - a statement (`let a = 1` to `let a = 2`): 0 rechecks;
//! - the impl's member (`3` to `4`): 1 recheck;
//! - the local type the impl names (`struct Local;` to `struct Local(u8);`): 1 recheck.
//!
//! Planted bug (rule 23): the merged table keyed by each record's WHOLE bytes (its read list,
//! its blocks' scopes, its key over the body's text). The statement edit then rechecks `down`,
//! and the count catches it.
//!
//! What this does not show: `down` type-checked THROUGH the type checker reading the merged rows.
//! The items record does not take them until the step-6 switch (merged.rs names the readers).
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_own::Alloc;
use blaze_scope::body_scope::merged::{SurfaceBody, body_rows, impl_rows_digest, merged_key};
use blaze_scope::body_scope::{BodyScope, body_scope};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, Names, resolve_crates};
use blaze_scope::surface::BlockScopes;
use blaze_scope::task::{Par, Serial};
use blaze_id::ContentId;
use blaze_own::AVec;

const BASE: &str = "pub trait Tr { type A; fn get(&self) -> u8; }
pub struct X;
pub fn f() -> u8 {
    let a = 1;
    struct Local;
    impl Tr for X { type A = Local; fn get(&self) -> u8 { 3 } }
    a + X.get()
}
pub fn main() {}
";

/// The serial solve with D189's seam set to `Bodies`.
struct Bodies;

impl Par for Bodies {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn block_scopes(&self) -> BlockScopes {
        BlockScopes::Bodies
    }
}

/// `up`'s one surface body `f`, solved under `Bodies` from `text`: its record.
fn f_scope(text: &'static str, alloc: Alloc<'static>) -> &'static BodyScope<'static> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
    let files: &'static [(String, String)] = Box::leak(vec![("/up/src/main.rs".to_string(), text.to_string())].into_boxed_slice());
    let up: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate { name: "up", dir: "/up/", root: "/up/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(up, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let bump = blaze_own::testing::bump();
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, &inputs.texts, &Bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));
    let c = names.crates()[0];
    let deps = BodyDeps::of_names(names, 0, alloc);
    let prepares = Prepares::new_in(bump);
    let held = c.bodies.iter().find(|h| h.src.text.contains("impl Tr for X")).unwrap_or_else(|| panic!("`f` is held under Bodies: {} held", c.bodies.len()));
    let scope = body_scope(c, &deps, &crates[0], 0, *held, &inputs.texts, prepares, bump).unwrap_or_else(|e| panic!("body_scope of `f`: {e}"));
    // The digest the record holds is the one its rows give (one function, merged.rs).
    let rows = body_rows(c, &scope, alloc).unwrap_or_else(|e| panic!("f's rows: {e:?}"));
    assert_eq!(scope.impl_rows, impl_rows_digest(&scope, &rows), "the record's impl rows digest is its rows'");
    assert!(!rows.is_empty(), "`f`'s impl of `Tr` is a row of its record");
    Box::leak(Box::new(scope))
}

/// What `down` reads `up`'s merged table by. `planted`: each record's whole bytes instead.
fn key_of(scope: &BodyScope<'_>, planted: bool, alloc: Alloc<'_>) -> ContentId {
    // The module-level names of `up` are the same text in every edit (only `f`'s body moves):
    // one names key stands for them here.
    let names_key = ContentId::of("blaze-ladder merged world test: up's names", b"up");
    let impl_rows = match planted {
        false => scope.impl_rows,
        true => ContentId::of("blaze-ladder merged world test: planted whole record", &scope.write(alloc).unwrap_or_else(|e| panic!("write: {e:?}"))),
    };
    merged_key(names_key, &[SurfaceBody { file: 0, ordinal: 0, impl_rows }])
}

#[test]
fn a_statement_edit_in_a_surface_body_rechecks_nothing_downstream() {
    let alloc = blaze_own::testing::heap().alloc();
    let statement: &'static str = Box::leak(BASE.replace("let a = 1;", "let a = 2;").into_boxed_str());
    let member: &'static str = Box::leak(BASE.replace("{ 3 }", "{ 4 }").into_boxed_str());
    let local: &'static str = Box::leak(BASE.replace("struct Local;", "struct Local(u8);").into_boxed_str());
    let base = f_scope(BASE, alloc);
    let edits = [("statement", f_scope(statement, alloc), 0usize), ("impl member", f_scope(member, alloc), 1), ("local type", f_scope(local, alloc), 1)];

    let mut caught = false;
    for (name, scope, want) in edits.iter() {
        assert_ne!(scope.key, base.key, "{name}: the body's record is a new key (its text moved)");
        let rechecks = usize::from(key_of(scope, false, alloc) != key_of(base, false, alloc));
        let planted = usize::from(key_of(scope, true, alloc) != key_of(base, true, alloc));
        println!("merged table: edit {name}: rechecks of down {rechecks} (want {want}); planted whole-record key {planted}");
        assert_eq!(rechecks, *want, "{name}: the merged table's key moves exactly when an impl or a type it names does");
        caught |= planted != rechecks;
    }
    assert!(caught, "the planted bug (the merged table keyed by the records' whole bytes) rechecks `down` on the statement edit, and the count must catch it, or this test proves nothing");
}
