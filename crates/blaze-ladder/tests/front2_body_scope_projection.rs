//! D189 step 4, proof (1): an upstream body writes `impl Tr for X { type A = Local; }` (and
//! `impl Local { fn m }`), and a downstream body calls `m` on a `<X as Tr>::A`. The body's
//! blocks are solved twice: as modules of the upstream crate's solve (`BlockScopes::Modules`)
//! and as the body's own (`Bodies`, blaze-scope body_scope.rs `body_scope`). The answer the
//! downstream call reads is the same under both: `Local` is the same key, read in the same
//! block, and both impls are filed in the same bucket (trait, self head) at the same place.
//!
//! The control (rule 23): the same question asked of the upstream crate's names under `Bodies`
//! WITHOUT the body's record (blaze-scope items_record/buckets.rs `OwnNames`) does not find
//! `Local`. So the record is what answers it, and a reader that skipped it would be caught.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`. Prints the verdicts first:
//! `projection: Local modules .. bodies ..: SAME`, `projection: impls equal N`,
//! `projection: control: ..`.

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_own::{AVec, Alloc};
use blaze_scope::body_scope::{BodyScope, BodyWorld, body_scope, bodies_impl_table, impl_diff, modules_impl_table};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::items_record::buckets::OwnNames;
use blaze_scope::names::{self, Written};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::surface::BlockScopes;
use blaze_scope::task::{Par, Serial};
use blaze_surface::{Namespace, PathKind};

/// The upstream crate: `up`'s body declares `Local`, an inherent impl on it and the trait impl
/// that makes it `<X as Tr>::A`.
const DEP: &str = "pub trait Tr { type A; }
pub struct X;
pub fn up() {
    pub struct Local;
    impl Local { pub fn m(&self) -> u8 { 1 } }
    impl Tr for X { type A = Local; }
}
";

/// The downstream crate: a body that calls `m` on the projection.
const USER: &str = "pub fn down(a: <dep::X as dep::Tr>::A) -> u8 { a.m() }
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

struct WithDep<'t> {
    base: &'t Texts<'t>,
    dep: usize,
    files: &'t [(String, String)],
}

impl<'t> SourceTexts<'t> for WithDep<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == self.dep {
            return self.files.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        self.base.read(krate, path)
    }
}

/// The two crates' inputs (the user crate at 0, `dep` last), leaked for the test's life.
fn two_crates(user: &'static str, dep: &'static str) -> (&'static [CrateInput<'static>], Sysroot, &'static WithDep<'static>, usize) {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
    let user_files: &'static [(String, String)] = Box::leak(vec![("/user/src/main.rs".to_string(), user.to_string())].into_boxed_slice());
    let dep_files: &'static [(String, String)] = Box::leak(vec![("/dep/src/lib.rs".to_string(), dep.to_string())].into_boxed_slice());
    let user: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: user_files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(user, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let mut crates: Vec<CrateInput<'static>> = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let dep_at = crates.len();
    let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("dep", dep_at)].into_boxed_slice());
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = user_deps;
    crates.push(CrateInput {
        name: "dep",
        key: CrateItemKey(ContentId::of("blaze-ladder body scope projection test crate", b"dep")),
        edition: crate_edition,
        implicit,
        root: "/dep/src/lib.rs",
        shown_from: "/dep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts: &'static WithDep<'static> = Box::leak(Box::new(WithDep { base: &inputs.texts, dep: dep_at, files: dep_files }));
    (Vec::leak(crates), sysroot, texts, dep_at)
}

/// `name` walked as a type from `module` in `world`: its key, or `None`.
fn type_key<W: names::World<&'static str>>(world: &W, module: ItemKey, name: &'static str, alloc: Alloc<'_>) -> Option<ItemKey> {
    let segments = [name];
    names::path(world, module, Namespace::Types, Written { kind: PathKind::Plain, segments: &segments[..] }, alloc).ok().and_then(|r| r.whole(1)).map(|d| d.key)
}

#[test]
fn a_projection_through_a_body_impl_is_the_same_under_both() {
    let alloc = blaze_own::testing::heap().alloc();
    let bump = blaze_own::testing::bump();
    let (crates, sysroot, texts, dep) = two_crates(USER, DEP);
    let modules: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates under Modules: {e}"))));
    let bodies: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));
    let (c_mod, c_bod) = (modules.crates()[dep], bodies.crates()[dep]);

    // `up`'s body, held by the upstream crate's solve under `Bodies`, solved as its own.
    let held = c_bod.bodies.iter().find(|b| b.src.text.contains("struct Local")).copied().unwrap_or_else(|| panic!("`up`'s body is held under Bodies: {} held", c_bod.bodies.len()));
    let deps = BodyDeps::of_names(bodies, dep, alloc);
    let prepares = Prepares::new_in(bump);
    let scope: BodyScope<'static> = body_scope(c_bod, &deps, &crates[dep], dep, held, texts, prepares, bump).unwrap_or_else(|e| panic!("body_scope(up): {e}"));
    let block = *scope.outermost.first().expect("`up`'s body has its outermost block");

    // The answer: `Local` read in `up`'s block, under each strategy.
    let world = BodyWorld { krate: c_bod, body: &scope };
    let under_modules = type_key(modules, block, "Local", alloc);
    let under_bodies = type_key(&world, block, "Local", alloc);
    let same = under_modules.is_some() && under_modules == under_bodies;
    println!("projection: Local modules {under_modules:?} bodies {under_bodies:?}: {}", if same { "SAME" } else { "DIFFER" });

    // The impls: the trait impl and the inherent impl, filed alike under both.
    let rows_mod = modules_impl_table(c_mod, alloc).unwrap_or_else(|e| panic!("the Modules impl table: {e:?}"));
    let rows_bod = bodies_impl_table(c_bod, core::slice::from_ref(&scope), alloc).unwrap_or_else(|e| panic!("the Bodies impl table: {e:?}"));
    let diff = impl_diff(&rows_mod, &rows_bod, alloc);
    let in_block = rows_bod.iter().filter(|r| r.module == block).count();
    println!("projection: impls equal {} (in up's block {in_block}); only-modules {} only-bodies {} differ {}", diff.equal, diff.only_modules.len(), diff.only_bodies.len(), diff.differ.len());
    let local_head = scope.locals.iter().find(|l| l.name == "Local").map(|l| l.key);
    println!("projection: the record's local head Local {local_head:?}");

    // The control: the crate's names alone under `Bodies` hold no block, so `Local` is not
    // found there; the body's record is what answers it.
    let without_record = type_key(&OwnNames(c_bod), block, "Local", alloc);
    println!("projection: control: Local without the body's record {without_record:?}: {}", if without_record.is_none() { "NOT FOUND (caught)" } else { "FOUND (the control proves nothing)" });

    assert!(same, "`Local` in up's block: Modules {under_modules:?}, Bodies {under_bodies:?}");
    assert!(diff.is_empty(), "the upstream crate's impl tables differ: {diff:?}");
    assert_eq!(in_block, 2, "both impls of `up` are in its block under Bodies: {rows_bod:?}");
    assert_eq!(local_head, under_modules, "the record names `Local` as a local head the impls reach");
    assert!(without_record.is_none(), "without the body's record `Local` must not be found, or the control proves nothing");

    // The downstream crate solved under both, its one function named alike.
    let down = |n: &'static Names<'static>| {
        let segments = ["down"];
        names::path(n, n.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &segments[..] }, alloc).ok().and_then(|r| r.whole(1)).map(|d| d.key)
    };
    let (down_mod, down_bod) = (down(modules), down(bodies));
    assert!(down_mod.is_some() && down_mod == down_bod, "the downstream `down` is one item under both: {down_mod:?} {down_bod:?}");
}
