//! D189 step 4, proof (2): `use dep::emits_impl as format; fn g() { format!(); }` makes `g` a
//! surface body (`format` is `emits_impl`, whose `$crate` callee writes an impl). Under `Bodies`
//! only surface bodies get a `BodyScope` task. The oracle compares the user crate's impl table
//! built that way with the table under `Modules`: every row that only `Modules` holds must be
//! inside-only, meaning it is filed under a block's type and, for a trait impl, a block's trait
//! (blaze-scope body_scope/impl_table.rs `inside_only`). A row seen outside that no surface body
//! gave is a miss.
//!
//! The planted bug `Plant::SkipEffect` judges a call by its written name instead of by its
//! definition, so it skips the MacroEffect lookup. That makes `g` a non-surface body, its impl
//! of `dep::Marker` goes missing from the `Bodies` table, and the oracle must catch it. Both
//! verdicts print first: `surface oracle: real: misses 0 ..` and
//! `surface oracle: planted: misses N ..`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::surface_names::surface_of;
use blaze_own::AVec;
use blaze_scope::body_scope::{BodyScope, ImplHeaders, ImplRow, bodies_impl_table, body_scope, impl_diff, inside_only, modules_impl_table, written_in_a_body};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::surface::{BlockScopes, BodyAt, Plant};
use blaze_scope::task::{Par, Serial};
use blaze_surface::Range;

/// The library: `emits_impl!` writes an impl (through its `$crate` callee), `plain!` an
/// expression. The callee is called with braces: an item-position call needs no `;` that way,
/// so the `Modules` solve (the oracle's truth) certainly writes the impl; with `()` and no `;`
/// the first run's truth held no impl at all (its "misses 0" was vacuous; whether that call
/// form is dropped by the old block modules is not established).
const DEP: &str = "pub trait Marker {}
#[doc(hidden)]
#[macro_export]
macro_rules! writes { () => { impl $crate::Marker for u8 {} } }
#[macro_export]
macro_rules! emits_impl { () => { $crate::writes! {} } }
#[macro_export]
macro_rules! plain { () => { 0 } }
";

/// The user crate: `format` renamed to `emits_impl` (D189's case), `fine` to `plain`.
const USER: &str = "use dep::emits_impl as format;
use dep::plain as fine;
pub fn g() { format!(); }
pub fn h() { let _ = fine!(); }
pub fn main() {}
";

/// Proof 2's truth, stated by hand from Rust's rules and not read off either table: the impls
/// the bodies of [`USER`] write, each as the body and the words its header holds. `g`'s
/// `format!()` is `dep::emits_impl!` (the Reference, "Use declarations": the `use` names
/// `format` in the module, and "Macros By Example", "Path-based scope": a bang call's path is
/// looked up there before the standard library prelude), whose `$crate::writes! {}` writes
/// `impl $crate::Marker for u8 {}` ("Statements", "Item declarations": an item in a block); `h`'s
/// `fine!()` is `dep::plain!`, an expression (`0`), which writes none. So exactly one impl.
const WRITTEN_IN_BODIES: &[(&str, &[&str])] = &[("g", &["Marker", "u8"])];

/// Whether a header in `headers` holds every word of `words`.
fn holds(headers: &[String], words: &[&str]) -> bool {
    headers.iter().any(|h| words.iter().all(|w| h.contains(w)))
}

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
        key: CrateItemKey(ContentId::of("blaze-ladder body scope surface test crate", b"dep")),
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

fn line_of(text: &str, start: &str) -> Range {
    let at = text.find(start).unwrap_or_else(|| panic!("`{start}` in the text"));
    let end = text[at..].find('\n').map_or(text.len(), |e| at + e);
    Range::new(u32::try_from(at).expect("a short text"), u32::try_from(end).expect("a short text"))
}

/// One run of the oracle: the bodies `plant`'s surface pass names get their `BodyScope`. The
/// truth is the `Modules` table's rows written inside a body (`written_in_a_body`); a truth row
/// the `Bodies` table does not hold, and that may be seen outside (not `inside_only`), is a
/// miss. Every row is named by its header (rule 19).
struct Verdict {
    surface: usize,
    scoped: usize,
    /// The `Modules` rows written inside a body, by header.
    truth: Vec<String>,
    misses: Vec<String>,
    only_bodies: usize,
    differ: usize,
    /// The entries of [`WRITTEN_IN_BODIES`] (by body) the `Bodies` table holds no impl for.
    listed_misses: Vec<&'static str>,
    /// The entries of [`WRITTEN_IN_BODIES`] the `Modules` table (the default path) holds no
    /// impl written in a body for.
    listed_misses_modules: Vec<&'static str>,
}

fn oracle(modules: &'static Names<'static>, bodies: &'static Names<'static>, crates: &'static [CrateInput<'static>], texts: &'static WithDep<'static>, plant: Plant, bump: blaze_own::Bump<'static>) -> Verdict {
    let alloc = bump.alloc();
    let (c_mod, c_bod) = (modules.crates()[0], bodies.crates()[0]);
    let root = bodies.root(0);
    let spans = [line_of(USER, "pub fn g"), line_of(USER, "pub fn h")];
    let at = [BodyAt { range: spans[0], module: root }, BodyAt { range: spans[1], module: root }];
    let (_, record) = surface_of(bodies, 0, USER, &at, plant, alloc).unwrap_or_else(|e| panic!("{e}"));

    // Each held body of the user crate whose text is in a surface body's span gets its task.
    let deps = BodyDeps::of_names(bodies, 0, alloc);
    let prepares = Prepares::new_in(bump);
    let mut scopes: AVec<'static, BodyScope<'static>> = AVec::new_in(alloc);
    for held in c_bod.bodies.iter() {
        let surface = (0..spans.len()).any(|i| {
            let span = &USER[spans[i].start as usize..spans[i].end as usize];
            record.is_surface(u32::try_from(i).expect("two bodies")) && span.contains(held.src.text)
        });
        if surface {
            scopes.push(body_scope(c_bod, &deps, &crates[0], 0, *held, texts, prepares, bump).unwrap_or_else(|e| panic!("body_scope: {e}")));
        }
    }
    let rows_mod = modules_impl_table(c_mod, alloc).unwrap_or_else(|e| panic!("the Modules impl table: {e:?}"));
    let rows_bod = bodies_impl_table(c_bod, &scopes, alloc).unwrap_or_else(|e| panic!("the Bodies impl table: {e:?}"));
    let diff = impl_diff(&rows_mod, &rows_bod, alloc);
    let headers = ImplHeaders::of(c_mod, &[], alloc);
    let named = |r: &ImplRow| headers.of_key(r.key).map_or_else(|| "an impl with no header".to_string(), |h| h.to_string());
    let mut truth: Vec<String> = Vec::new();
    let mut misses: Vec<String> = Vec::new();
    for r in rows_mod.iter() {
        if !written_in_a_body(c_mod, r) {
            continue;
        }
        truth.push(named(r));
        let produced = rows_bod.binary_search_by_key(&r.key, |b| b.key).is_ok();
        if !produced && !inside_only(c_mod, r) {
            misses.push(named(r));
        }
    }
    // The explicit truth: each listed impl must be in the `Bodies` table (named by its headers,
    // the body records' with the crate's), and in the `Modules` table among the rows written
    // in a body.
    let bodies_headers = ImplHeaders::of(c_bod, &scopes, alloc);
    let bodies_named: Vec<String> = rows_bod.iter().map(|r| bodies_headers.of_key(r.key).map_or_else(|| "an impl with no header".to_string(), |h| h.to_string())).collect();
    let listed_misses: Vec<&'static str> = WRITTEN_IN_BODIES.iter().filter(|(_, words)| !holds(&bodies_named, words)).map(|(body, _)| *body).collect();
    let listed_misses_modules: Vec<&'static str> = WRITTEN_IN_BODIES.iter().filter(|(_, words)| !holds(&truth, words)).map(|(body, _)| *body).collect();
    Verdict { surface: record.rows.len(), scoped: scopes.len(), truth, misses, only_bodies: diff.only_bodies.len(), differ: diff.differ.len(), listed_misses, listed_misses_modules }
}

#[test]
fn a_renamed_emitting_macro_is_a_surface_body_and_the_oracle_catches_its_skip() {
    let bump = blaze_own::testing::bump();
    let (crates, sysroot, texts, _dep) = two_crates(USER, DEP);
    let modules: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates under Modules: {e}"))));
    let bodies: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));

    let real = oracle(modules, bodies, crates, texts, Plant::None, bump);
    let planted = oracle(modules, bodies, crates, texts, Plant::SkipEffect, bump);
    for (name, v) in [("real", &real), ("planted", &planted)] {
        println!(
            "surface oracle: {name}: misses {} of {} impl(s) written in bodies (surface bodies {}, scoped {}, only-bodies {}, differ {}); truth {:?}",
            v.misses.len(),
            v.truth.len(),
            v.surface,
            v.scoped,
            v.only_bodies,
            v.differ,
            v.truth
        );
    }
    for m in &planted.misses {
        println!("surface oracle: planted: missed {m}");
    }

    println!("surface oracle: listed truth {WRITTEN_IN_BODIES:?}: real misses {:?} (Modules {:?}); planted misses {:?}", real.listed_misses, real.listed_misses_modules, planted.listed_misses);

    // The truth is the explicit list (a table-derived truth was vacuous once: both tables lacked
    // `g`'s impl and the planted run then missed nothing either). Both tables must hold it: the
    // `Bodies` table through `g`'s BodyScope, the `Modules` table (the default path) since the
    // shadowing `use` is read before the prelude.
    assert!(real.listed_misses.is_empty(), "the Bodies table holds every impl the bodies write: missing for {:?}", real.listed_misses);
    assert!(real.listed_misses_modules.is_empty(), "the Modules table holds every impl the bodies write: missing for {:?}; its rows written in bodies: {:?}", real.listed_misses_modules, real.truth);
    assert!(real.scoped >= 1, "`g` is a surface body and gets its BodyScope");
    assert!(real.misses.is_empty() && real.only_bodies == 0 && real.differ == 0, "every impl seen outside is in the Bodies table: {:?}", real.misses);
    // The planted bug must miss `g`'s listed impl, and both oracles must say so, or this test
    // proves nothing.
    assert_eq!(planted.listed_misses, ["g"], "the planted bug (no MacroEffect lookup) misses `g`'s impl of `dep::Marker` against the listed truth");
    assert!(planted.misses.iter().any(|h| h.contains("Marker")), "the planted bug must miss `g`'s impl of `dep::Marker` against the Modules table too: {:?}", planted.misses);
}

/// The twin of the truth above: without `use dep::emits_impl as format;` the call `format!("x")`
/// in `g`'s block is the standard library's (the `#[macro_use]` prelude's), which writes no
/// item, so the default (`Modules`) solve's table holds no impl written in a body. With the
/// import the same call is `emits_impl` (the test above asserts that truth holds `Marker`):
/// the Reference, `names.preludes`: "Names in the preludes can be shadowed by declarations in
/// a module", and a block reaches its module's names lexically (blaze-scope cells
/// `first_segment`; its control is cells/tests.rs
/// `a_blocks_call_reads_its_modules_named_import_before_the_prelude`).
/// Prints: "surface twin: impls written in bodies under Modules: []".
#[test]
fn a_plain_format_call_in_a_block_writes_no_impl() {
    const PLAIN: &str = "pub fn g() { format!(\"x\"); }\npub fn main() {}\n";
    let alloc = blaze_own::testing::heap().alloc();
    let bump = blaze_own::testing::bump();
    let (crates, sysroot, texts, _dep) = two_crates(PLAIN, DEP);
    let modules: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, texts, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates under Modules: {e}"))));
    let c = modules.crates()[0];
    let rows = modules_impl_table(c, alloc).unwrap_or_else(|e| panic!("the Modules impl table: {e:?}"));
    let headers = ImplHeaders::of(c, &[], alloc);
    let in_bodies: Vec<String> = rows.iter().filter(|r| written_in_a_body(c, r)).map(|r| headers.of_key(r.key).map_or_else(|| "an impl with no header".to_string(), |h| h.to_string())).collect();
    println!("surface twin: impls written in bodies under Modules: {in_bodies:?}");
    assert!(in_bodies.is_empty(), "the prelude's `format!` writes no impl: {in_bodies:?}");
}
