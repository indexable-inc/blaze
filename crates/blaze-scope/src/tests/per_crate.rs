//! The per-crate entry, `crate_scope::resolve_crate`: crate by crate through records gives the
//! answers of the one whole-set call, its records' bytes are the same in any fill order, and a
//! record that shows a private name is caught.

use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Edition, ScopeError, Unresolved};

use super::entry::{Texts, input};
use crate::crate_scope::{CrateScope, CrateScopeError, Ready, read_record, resolve_by_records, resolve_by_records_with, resolve_crate, why_reported};
use crate::graph::{CrateId, Sysroot};
use crate::prepares::Prepares;
use crate::resolve::{CrateInput, Names, aligned, resolve_crates};
use crate::task::{Par, Serial};

/// The longest a test's free worker waits for a crate to become ready.
const WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// A value that crosses threads although its type is not `Send` or `Sync` only because it holds
/// `Alloc` handles: every handle in these tests is `Alloc::global()`, the process's heap, which
/// any thread may use at once (blaze-world's examples/names.rs, `OnGlobal`).
struct OnGlobal<T>(T);

// SAFETY: see `OnGlobal`.
unsafe impl<T> Send for OnGlobal<T> {}
// SAFETY: see `OnGlobal`.
unsafe impl<T> Sync for OnGlobal<T> {}

/// `n` indices on `self.0` real threads, each taking the next index; results in index order.
struct Threads(usize);

impl Par for Threads {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        let next = std::sync::atomic::AtomicUsize::new(0);
        let f = OnGlobal(f);
        let (f, next) = (&f, &next);
        let mut all: Vec<Option<T>> = (0..n).map(|_| None).collect();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..self.0.min(n))
                .map(|_| {
                    scope.spawn(move || {
                        let mut mine = Vec::new();
                        loop {
                            let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if at >= n {
                                break OnGlobal(mine);
                            }
                            mine.push((at, (f.0)(at)));
                        }
                    })
                })
                .collect();
            for h in handles {
                for (at, v) in h.join().unwrap_or_else(|_| panic!("a test worker thread panicked")).0 {
                    all[at] = Some(v);
                }
            }
        });
        out.extend(all.into_iter().map(|v| v.unwrap_or_else(|| panic!("an index was not run"))));
    }
}

/// The fill's waves, each in reverse: another completion order than [`Serial`]'s.
struct Reversed;

impl Par for Reversed {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        let mut done: Vec<(usize, T)> = (0..n).rev().map(|i| (i, f(i))).collect();
        done.sort_by_key(|it| it.0);
        out.extend(done.into_iter().map(|it| it.1));
    }
}

const C: &str = "#![no_core]
#[macro_export]
macro_rules! from_c { () => { pub fn made_by_c() {} } }
pub mod m {
    pub struct Thing;
    fn shut() {}
    pub enum Way { Up, Down(u8) }
    pub mod deep { pub fn far() {} }
}
pub use m::deep::*;
";

const MAC: &str = "#![no_core]
#[macro_export]
macro_rules! wrap { ($($t:tt)*) => { $($t)* } }
";

const B: &str = "#![no_core]
pub use c::*;
pub use c::m::Thing as Other;
pub mod more { pub use c::m::*; }
fn secret() {}
#[cfg(script_flag)]
pub fn gated() {}
#[cfg(not(script_flag))]
pub fn ungated() {}
pub use mac::wrap;
";

const A: &str = "#![no_core]
pub use b::Other;
pub use b::more::Thing;
pub use b::more::Way::Up;
pub use b::far;
pub use b::gated;
pub use b::ungated;
pub use b::secret;
pub use c::m::shut;
b::wrap! { pub fn wrapped() {} }
c::from_c!();
pub use b::nowhere::Gone;
";

static TEXTS: Texts = Texts(&[&[("/a/lib.rs", A)], &[("/b/lib.rs", B)], &[("/c/lib.rs", C)], &[("/mac/lib.rs", MAC)], &[("/d/lib.rs", "#![no_core]\npub use b::far;\n")]]);

fn crates() -> [CrateInput<'static>; 4] {
    let mut b = input("b", "/b/lib.rs", &[("c", 2), ("mac", 3)]);
    // The build script's `cargo:rustc-cfg=script_flag`, joined into the cfg as blaze-world does.
    b.cfg = &[("script_flag", None)];
    b.edition = Edition::E2021;
    [input("a", "/a/lib.rs", &[("b", 1), ("c", 2)]), b, input("c", "/c/lib.rs", &[]), input("mac", "/mac/lib.rs", &[])]
}

/// Each crate's module scopes, by module key, as `Debug` text: what every import and macro
/// path resolved to, and every error (an unresolved import, a macro path not found).
fn scopes_of(c: &crate::resolve::CrateNames<'_>) -> Vec<String> {
    let mut keys: Vec<_> = c.done.keys().copied().collect();
    keys.sort();
    keys.iter().map(|k| format!("{:?}", c.done[k].scope)).collect()
}

/// Through records, a path that meets another crate's private name fails as "not found": the
/// record a dependent reads holds no name it cannot see (an added private item must change no
/// dependent's key), so the answer is the same (the import is unresolved) and only the reason
/// differs; the reporter tells "private" from that crate's inner record. The whole set's
/// reasons are compared so on both sides (a private name kept because it may shadow a glob's
/// still says "private").
fn same_as_whole(whole: &Names<'_>, per: &[CrateScope<'_>]) -> Result<(), String> {
    let plain = |s: &String| s.replace("why: Private", "why: NotFound");
    for (i, c) in whole.crates().iter().enumerate() {
        let w: Vec<String> = scopes_of(c).iter().map(plain).collect();
        let p: Vec<String> = scopes_of(per[i].names).iter().map(plain).collect();
        if w != p {
            let at = w.iter().zip(p.iter()).position(|(x, y)| x != y).unwrap_or(w.len().min(p.len()));
            return Err(format!("crate `{}`: scope {at} differs:\nwhole: {:?}\nper:   {:?}", c.name, w.get(at), p.get(at)));
        }
    }
    Ok(())
}

#[test]
fn crate_by_crate_through_records_answers_as_the_whole_set_and_writes_equal_bytes() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let crates = crates();
    let whole = resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let (one, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    let (two, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Reversed, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    same_as_whole(&whole, &one).unwrap_or_else(|e| panic!("{e}"));
    for (x, y) in one.iter().zip(two.iter()) {
        assert!(x.record[..] == y.record[..], "crate `{}`: its record's bytes differ by fill order", x.names.name);
    }
    // The fixture says what it covers: the answers are not all failures.
    let a = &one[0].names;
    let text = scopes_of(a).join("\n");
    for name in ["Other", "Thing", "Up", "far", "gated", "wrapped", "made_by_c"] {
        assert!(text.contains(&format!("name: \"{name}\"")), "`{name}` is not bound in a:\n{text}");
    }
    assert!(!text.contains("name: \"ungated\", binding"), "the script's cfg did not reach b:\n{text}");
}

/// The ready queue changes when a crate is resolved, never what its records hold: every
/// crate's two records are byte-equal between one worker in the graph's order, four workers on
/// real threads, and four workers taking ready crates in shuffled orders; with no sysroot and
/// with `c` as core (which every other crate then waits on, as the serial walk had it finished
/// before them). Whatever ran before a crate, it is given the same records in the same order.
#[test]
fn records_equal_across_workers_and_orders() {
    let crates = crates();
    let frame = super::SyncFrame::new();
    let bump = frame.bump();
    let g = bump.alloc();
    // The read list is a record too: its bytes are the same whatever order the fill asked in;
    // and so is the full-names record ([`crate::names_record`]).
    let bytes = |all: &[CrateScope<'_>]| {
        all.iter()
            .map(|c| {
                let names = crate::names_record::encode(c.names, g).unwrap_or_else(|e| panic!("names record of `{}`: {e:?}", c.names.name));
                let items = crate::items_record::encode(c.names, g).unwrap_or_else(|e| panic!("items record of `{}`: {e:?}", c.names.name));
                (c.record.to_vec(), c.inner.to_vec(), crate::asked::encode(c.names.key, &[], &c.asked, g).to_vec(), names.to_vec(), items.to_vec())
            })
            .collect::<Vec<_>>()
    };
    for sysroot in [Sysroot::default(), Sysroot { core: Some(CrateId(2)), ..Sysroot::default() }] {
        let (one, _) = resolve_by_records(&crates, sysroot, &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("one worker: {e:?}"));
        let first = bytes(&one);
        // Ranks: the identity, reversed, and two fixed shuffles of the four crates.
        let ranks: [[u32; 4]; 4] = [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]];
        for rank in &ranks {
            let ready = Ready { workers: 4, rank: Some(rank), wait: WAIT, hooks: &crate::crate_scope::NoHooks };
            let (many, _) = resolve_by_records(&crates, sysroot, &TEXTS, &Threads(4), &ready, bump).unwrap_or_else(|e| panic!("four workers, ranks {rank:?}: {e:?}"));
            let other = bytes(&many);
            for (i, (a, b)) in first.iter().zip(other.iter()).enumerate() {
                assert!(a.0 == b.0, "crate `{}`: exports differ, 1 worker vs 4 with ranks {rank:?} (core {:?})", crates[i].name, sysroot.core);
                assert!(a.1 == b.1, "crate `{}`: inner differs, 1 worker vs 4 with ranks {rank:?} (core {:?})", crates[i].name, sysroot.core);
                assert!(!a.2.is_empty() && a.2 == b.2, "crate `{}`: read list differs, 1 worker vs 4 with ranks {rank:?} (core {:?})", crates[i].name, sysroot.core);
                assert!(!a.3.is_empty() && a.3 == b.3, "crate `{}`: names record differs, 1 worker vs 4 with ranks {rank:?} (core {:?})", crates[i].name, sysroot.core);
                assert!(!a.4.is_empty() && a.4 == b.4, "crate `{}`: items record differs, 1 worker vs 4 with ranks {rank:?} (core {:?})", crates[i].name, sysroot.core);
            }
        }
    }
}

/// `c` edited three ways, each beside the unedited set of [`TEXTS`].
const C_BODY: &str = "#![no_core]
#[macro_export]
macro_rules! from_c { () => { pub fn made_by_c() {} } }
pub mod m {
    pub struct Thing;
    fn shut() {}
    pub enum Way { Up, Down(u8) }
    pub mod deep { pub fn far() { let _edited = 1; } }
}
pub use m::deep::*;
";
const C_PUB: &str = "#![no_core]
#[macro_export]
macro_rules! from_c { () => { pub fn made_by_c() {} } }
pub mod m {
    pub struct Thing;
    fn shut() {}
    pub enum Way { Up, Down(u8) }
    pub mod deep { pub fn far() {} pub fn added() {} }
}
pub use m::deep::*;
";
const C_PRIV: &str = "#![no_core]
#[macro_export]
macro_rules! from_c { () => { pub fn made_by_c() {} } }
pub mod m {
    pub struct Thing;
    fn shut() {}
    fn hidden() {}
    pub(crate) fn inner() {}
    pub(super) fn upper() {}
    pub enum Way { Up, Down(u8) }
    mod private_mod { pub fn unseen() {} }
    pub mod deep { pub fn far() {} }
}
use m::Thing as Private;
pub use m::deep::*;
";
static TEXTS_BODY: Texts = Texts(&[&[("/a/lib.rs", A)], &[("/b/lib.rs", B)], &[("/c/lib.rs", C_BODY)], &[("/mac/lib.rs", MAC)]]);
static TEXTS_PUB: Texts = Texts(&[&[("/a/lib.rs", A)], &[("/b/lib.rs", B)], &[("/c/lib.rs", C_PUB)], &[("/mac/lib.rs", MAC)]]);
static TEXTS_PRIV: Texts = Texts(&[&[("/a/lib.rs", A)], &[("/b/lib.rs", B)], &[("/c/lib.rs", C_PRIV)], &[("/mac/lib.rs", MAC)]]);

/// The names of the crates whose exports record (what a dependent keys on) and whose inner
/// record differ between the unedited set and `edited`.
fn changed_by(edited: &'static Texts) -> (Vec<&'static str>, Vec<&'static str>) {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let crates = crates();
    let (before, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    let (after, _) = resolve_by_records(&crates, Sysroot::default(), edited, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    let mut exports = Vec::new();
    let mut inner = Vec::new();
    for (i, (x, y)) in before.iter().zip(after.iter()).enumerate() {
        if x.record[..] != y.record[..] {
            exports.push(crates[i].name);
        }
        if x.inner[..] != y.inner[..] {
            inner.push(crates[i].name);
        }
    }
    (exports, inner)
}

/// An edit inside a function's body changes no item's key and no scope (Reference, "Items":
/// a body is no item of the module), so no record's bytes change.
#[test]
fn a_body_edit_leaves_every_record_byte_equal() {
    assert_eq!(changed_by(&TEXTS_BODY), (vec![], vec![]));
}

/// A `pub fn` added to `c::m::deep` changes c's exports record and no other: b reads it through
/// glob edges (`pub use c::*`, `pub use c::m::*`), which name c's modules, not their contents,
/// so b's record is byte-equal; a, which walks those globs, keys on c's record by `reaches`.
#[test]
fn an_added_pub_fn_changes_only_its_own_crate_s_record() {
    assert_eq!(changed_by(&TEXTS_PUB), (vec!["c"], vec![]));
}

/// Names another crate can never see (Reference, "Visibility and privacy": a private item is
/// visible only in its module and below; `pub(crate)` and `pub(in path)` only inside the crate)
/// change c's inner record, never its exports record, so no dependent's key changes.
#[test]
fn an_added_private_item_changes_no_dependent_s_key() {
    assert_eq!(changed_by(&TEXTS_PRIV), (vec![], vec!["c"]));
}

#[test]
fn a_record_that_shows_a_private_name_is_caught() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let crates = crates();
    let whole = resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, bump).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let (leaky, _) = resolve_by_records_with(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), true, bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    let caught = same_as_whole(&whole, &leaky).expect_err("a record showing `secret` and `shut` as `pub` was not caught");
    assert!(caught.contains("crate `a`"), "{caught}");
}

/// The Reference, "Visibility and privacy": "If an item is private, it may be accessed by the
/// current module and its descendants." `a`'s `pub use c::m::shut;` names c's private `fn
/// shut` in `mod m`: through c's exports record (which leaves it out) the lookup fails, and the
/// reporter, reading c's inner record only then, says "private"; `b::nowhere` and the cfg'd-out
/// `b::ungated` stay "not found".
#[test]
fn a_path_to_another_crate_s_private_name_is_reported_private() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let crates = crates();
    let g = bump.alloc();
    let (all, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    let each: Vec<_> = all.iter().map(|c| c.names).collect();
    let names = Names::of_crates(&crates, Sysroot::default(), &each, g).unwrap_or_else(|e| panic!("names: {e:?}"));
    let mut seen = Vec::new();
    for r in all[0].names.done.values() {
        for error in r.scope.errors.iter() {
            if let ScopeError::Import { path, segment, why, .. } = error {
                let at = *segment as usize;
                let written = crate::names::Written { kind: path.kind, segments: &path.segments[..at] };
                let owner = names.item_at(r.scope.module, blaze_surface::Namespace::Types, written, g).unwrap_or_else(|e| panic!("the owner of `{:?}`: {e:?}", path.segments));
                let reported = why_reported(*why, &all[owner.krate].inner, owner.def.key, path.segments[at], g).unwrap_or_else(|e| panic!("the inner record: {e:?}"));
                seen.push((path.segments.join("::"), *why, reported));
            }
        }
    }
    seen.sort();
    let shut = (String::from("c::m::shut"),Unresolved::NotFound, Unresolved::Private);
    assert!(seen.contains(&shut), "{seen:?}");
    assert!(seen.iter().all(|(p, _, reported)| (*reported == Unresolved::Private) == (p.ends_with("shut") || p.ends_with("secret"))), "{seen:?}");
}

#[test]
fn a_dependency_s_reach_is_asked_for_by_key() {
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let bump = erased.bump();
    let g = bump.alloc();
    let crates = crates();
    let (all, _) = resolve_by_records(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), bump).unwrap_or_else(|e| panic!("by records: {e:?}"));
    let prepares = Prepares::new_in(bump);
    let b_record = aligned(&all[1].record, bump);
    let b = read_record(b_record, g).expect("b's record reads");
    let c_key: CrateItemKey = crates[2].key;
    assert!(b.reaches.contains(&c_key), "b re-exports c's items but its record does not reach c: {:?}", b.reaches);
    // `d` names only `b`; `b::far` is c's, through two globs.
    let d = CrateInput { key: CrateItemKey(ContentId::of("blaze-scope test crate", b"d")), ..input("d", "/d/lib.rs", &[("b", 0)]) };
    // The records' heads, read in place, ask for what the solve's `Need` asks for, before it.
    let unmet = crate::crate_scope::unmet_reaches(&[b_record], g).unwrap_or_else(|e| panic!("b's head: {e:?}"));
    match resolve_crate(&d, &[b_record], Sysroot::default(), &TEXTS, 4, &Serial, prepares, bump) {
        Err(CrateScopeError::Need(keys)) => {
            assert!(keys.contains(&c_key), "d's Need lacks c: {keys:?}");
            assert_eq!(&unmet[..], &keys[..], "the heads' unmet reaches are the solve's Need");
        }
        other => panic!("d without c's record: {other:?}"),
    }
    let mut deps = vec![b_record, aligned(&all[2].record, bump)];
    if b.reaches.contains(&crates[3].key) {
        deps.push(aligned(&all[3].record, bump));
    }
    assert!(crate::crate_scope::unmet_reaches(&deps, g).expect("the heads").is_empty(), "every reach handed: nothing unmet");
    let done = resolve_crate(&d, &deps, Sysroot::default(), &TEXTS, 4, &Serial, prepares, bump).unwrap_or_else(|e| panic!("d with c's record: {e:?}"));
    let text = scopes_of(done.names).join("\n");
    assert!(text.contains("name: \"far\"") && !text.contains("Unresolved"), "d's `use b::far` through records:\n{text}");
}

/// Reduced from c-kzg 2.1.8 (`bindings/rust/src/bindings/mod.rs`: `include!("./generated.rs");`,
/// a file in the package, no build script): its 19 + 2 unresolved imports in tempo and the
/// imports of alloy-eips and others that name `c_kzg::BYTES_PER_COMMITMENT`. The Reference,
/// "Macros By Example" / built-in `include!` (core `macros/mod.rs`): "Parses a file as an
/// expression or an item according to the context. The file is located relative to the
/// current file (similarly to how modules are found)."
#[test]
fn an_include_of_a_package_file_declares_its_items_relative_to_the_calling_file() {
    static KZG: Texts = Texts(&[
        &[
            ("/kzg/src/lib.rs", "#![no_core]\nmod bindings;\npub use bindings::BYTES_PER_COMMITMENT;\npub use bindings::KZGSettings;\n"),
            ("/kzg/src/bindings/mod.rs", "#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\ninclude!(\"./generated.rs\");\n"),
            ("/kzg/src/bindings/generated.rs", "pub const BYTES_PER_COMMITMENT: usize = 48;\npub struct KZGSettings;\n"),
        ],
        &[("/user/lib.rs", "#![no_core]\npub use kzg::BYTES_PER_COMMITMENT;\n")],
    ]);
    let mut kzg = input("kzg", "/kzg/src/lib.rs", &[]);
    kzg.shown_from = "/kzg/";
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let crates = [kzg, input("user", "/user/lib.rs", &[("kzg", 0)])];
    let (done, _) = resolve_by_records(&crates, Sysroot::default(), &KZG, &Serial, &Ready::serial(WAIT), erased.bump()).unwrap_or_else(|e| panic!("by records: {e:?}"));
    for c in &done {
        let text = scopes_of(c.names).join("\n");
        assert!(text.contains("BYTES_PER_COMMITMENT") && !text.contains("Unresolved") && !text.contains("NotFound"), "crate `{}`: the included items resolve:\n{text}", c.names.name);
    }
}

/// Reduced from libp2p-identity 0.2 (`src/lib.rs`: `mod proto { include!("generated/mod.rs");
/// pub(crate) use self::keys_proto::*; }`, whose `src/generated/mod.rs` writes `pub mod
/// keys_proto;` for `src/generated/keys_proto.rs`): the header `impl From<&PublicKey> for
/// proto::PublicKey` that stopped 1,897 bodies of tempo, since `keys_proto` was looked for in
/// `src/proto/` (the calling module's folder) and not found.
///
/// The rules: core `macros/mod.rs`, `include!`: "The file is located relative to the current
/// file (similarly to how modules are found)"; the Reference, "Modules", "Module Source
/// Filenames": "A module without a body is loaded from an external file. When the module does
/// not have a `path` attribute, the path to the file mirrors the logical module path. Ancestor
/// module path components are directories, and the module's contents are in a file with the
/// name of the module plus the `.rs` extension." The included file is the current file of what
/// it writes (`crate::included`): its `mod keys_proto;` is `generated/keys_proto.rs`, and its
/// inline `mod deep { pub mod leaf; }` is `generated/deep/leaf.rs`.
///
/// A fix that keeps the calling module's folder finds the planted `src/proto/keys_proto.rs`
/// (which has no `PublicKey`) and fails on "Unresolved"; one that finds no file fails on the
/// `Made { .. FileNotFound .. }` note. Passing prints
/// "test tests::per_crate::a_mod_in_an_included_file_is_found_from_the_included_file_s_folder ... ok".
#[test]
fn a_mod_in_an_included_file_is_found_from_the_included_file_s_folder() {
    static ID: Texts = Texts(&[&[
        (
            "/id/src/lib.rs",
            "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\nmod proto {\n    include!(\"generated/mod.rs\");\n    pub(crate) use self::keys_proto::*;\n    pub(crate) use self::deep::leaf::Leaf;\n}\npub(crate) use proto::PublicKey;\npub(crate) use proto::Leaf;\n",
        ),
        ("/id/src/generated/mod.rs", "pub mod keys_proto;\npub mod deep {\n    pub mod leaf;\n}\n"),
        ("/id/src/generated/keys_proto.rs", "pub struct PublicKey;\n"),
        ("/id/src/generated/deep/leaf.rs", "pub struct Leaf;\n"),
        // The plant: what the calling module's folder would give.
        ("/id/src/proto/keys_proto.rs", "pub struct Decoy;\n"),
    ]]);
    let mut id = input("id", "/id/src/lib.rs", &[]);
    id.shown_from = "/id/";
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let crates = [id];
    let (done, _) = resolve_by_records(&crates, Sysroot::default(), &ID, &Serial, &Ready::serial(WAIT), erased.bump()).unwrap_or_else(|e| panic!("by records: {e:?}"));
    for c in &done {
        let text = scopes_of(c.names).join("\n");
        let notes = format!("{:?}", c.names.notes);
        assert!(
            text.contains("PublicKey") && text.contains("Leaf") && !text.contains("Decoy") && !text.contains("Unresolved") && !text.contains("NotFound"),
            "crate `{}`: the names of the included file's modules resolve:\n{text}\nnotes: {notes}",
            c.names.name
        );
        assert!(!notes.contains("Made {"), "crate `{}`: every module the included file writes is made: {notes}", c.names.name);
    }
}

/// Reduced from serde_core 1.0.228 (`src/lib.rs`: `include!(concat!(env!("OUT_DIR"),
/// "/private.rs"));`, the file its build script writes): the 5 unresolved imports of
/// `serde_core_private` in tempo. The builtins take macro calls in their arguments' places and
/// expand them first, as the pinned library documents each (core/src/macros/mod.rs):
/// `include!` "Parses a file as an expression or an item", its argument a path; `concat!`
/// "Concatenates literals into a static string slice"; `env!` "Inspects an environment
/// variable at compile time". The file comes from the build script's output record.
#[test]
fn an_include_of_a_concat_of_env_out_dir_reads_the_build_script_s_file() {
    static SERDE: Texts = Texts(&[
        &[(
            "/serde/src/lib.rs",
            "#![no_core]\n#[rustc_builtin_macro]\nmacro_rules! include { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! concat { () => {} }\n#[rustc_builtin_macro]\nmacro_rules! env { () => {} }\ninclude!(concat!(env!(\"OUT_DIR\"), \"/private.rs\"));\npub use self::serde_core_private::Content;\n",
        )],
        &[("/user/lib.rs", "#![no_core]\npub use serde::serde_core_private::Content;\n")],
    ]);
    let mut outputs = blaze_build_script::outputs::Outputs::new_in("/out", Alloc::global());
    outputs
        .add_file("private.rs", b"pub mod serde_core_private { pub struct Content; }\n", blaze_build_script::outputs::FileLimits::DEFAULT, Alloc::global())
        .unwrap_or_else(|e| panic!("the record's file: {e:?}"));
    let mut bytes = AVec::new_in(Alloc::global());
    outputs.encode(&mut bytes);
    let bytes: &'static [u8] = Vec::leak(bytes.to_vec());
    let record = blaze_build_script::outputs::OutputsRef::open(bytes).unwrap_or_else(|e| panic!("the record opens: {e:?}"));
    let mut serde = input("serde", "/serde/src/lib.rs", &[]);
    serde.shown_from = "/serde/";
    serde.script = Some(record);
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let crates = [serde, input("user", "/user/lib.rs", &[("serde", 0)])];
    let (done, _) = resolve_by_records(&crates, Sysroot::default(), &SERDE, &Serial, &Ready::serial(WAIT), erased.bump()).unwrap_or_else(|e| panic!("by records: {e:?}"));
    for c in &done {
        let text = scopes_of(c.names).join("\n");
        assert!(text.contains("Content") && !text.contains("Unresolved") && !text.contains("NotFound"), "crate `{}`: the included items resolve:\n{text}\nnotes: {:?}", c.names.name, c.names.notes);
    }
}

/// Reduced from opentelemetry-proto 0.32.0 (`src/proto.rs`, a non-mod-rs file:
/// `#[path = "proto/tonic"] pub mod tonic { #[path = ""] pub mod metrics {
/// #[path = "opentelemetry.proto.metrics.v1.rs"] pub mod v1; } }`, the file at
/// `src/proto/tonic/`): its 20 + 6 + 6 unresolved imports `crate::proto::tonic::*::v1::*` in
/// tempo. The Reference, "Modules" / the `path` attribute: "For path attributes on modules not
/// inside inline module blocks, the file path is relative to the directory the source file is
/// located." An inline module's `path` names its directory the same way, from the directory of
/// the file it is written in (`src/`), not from the non-mod-rs module's own folder
/// (`src/proto/`); inline modules under it take their paths from that directory.
#[test]
fn an_inline_module_s_path_is_a_directory_relative_to_its_file_s_folder() {
    static OTEL: Texts = Texts(&[
        &[
            ("/otel/src/lib.rs", "#![no_core]\nmod proto;\npub use proto::tonic::metrics::v1::Exemplar;\n"),
            ("/otel/src/proto.rs", "#[path = \"proto/tonic\"]\npub mod tonic {\n    #[path = \"\"]\n    pub mod metrics {\n        #[path = \"opentelemetry.proto.metrics.v1.rs\"]\n        pub mod v1;\n    }\n}\n"),
            ("/otel/src/proto/tonic/opentelemetry.proto.metrics.v1.rs", "pub struct Exemplar;\n"),
        ],
        &[("/user/lib.rs", "#![no_core]\npub use otel::Exemplar;\n")],
    ]);
    let mut otel = input("otel", "/otel/src/lib.rs", &[]);
    otel.shown_from = "/otel/";
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let crates = [otel, input("user", "/user/lib.rs", &[("otel", 0)])];
    let (done, _) = resolve_by_records(&crates, Sysroot::default(), &OTEL, &Serial, &Ready::serial(WAIT), erased.bump()).unwrap_or_else(|e| panic!("by records: {e:?}"));
    for c in &done {
        let text = scopes_of(c.names).join("\n");
        assert!(text.contains("Exemplar") && !text.contains("Unresolved") && !text.contains("NotFound"), "crate `{}`: the module under the inline `#[path]` resolves:\n{text}", c.names.name);
    }
}

/// `c` with `shut` made public: `a`'s `pub use c::m::shut` now resolves, so `a`'s records
/// move although `a`'s text did not (what a found-but-stale `a` would miss).
static TEXTS_SHUT: Texts = Texts(&[
    &[("/a/lib.rs", A)],
    &[("/b/lib.rs", B)],
    &[("/c/lib.rs", "#![no_core]\n#[macro_export]\nmacro_rules! from_c { () => { pub fn made_by_c() {} } }\npub mod m {\n    pub struct Thing;\n    pub fn shut() {}\n    pub enum Way { Up, Down(u8) }\n    pub mod deep { pub fn far() {} }\n}\npub use m::deep::*;\n")],
    &[("/mac/lib.rs", MAC)],
]);

/// One kept solve in [`MemKept`]: its file's text, its read list, its two records.
#[derive(Clone, Copy)]
struct KeptSolve {
    text: &'static str,
    read: &'static [u8],
    record: &'static [u8],
    inner: &'static [u8],
}

/// A [`Kept`](crate::crate_scope::Kept) in memory: crate `i`'s solve is found when its one
/// file's text and every exports digest it read are unchanged. `planted`: the read list is not
/// recorded (a dependent is found although a crate it read changed).
struct MemKept {
    now: std::sync::Mutex<&'static Texts>,
    kept: std::sync::Mutex<Vec<Option<KeptSolve>>>,
    planted: bool,
    found: std::sync::atomic::AtomicUsize,
}

impl crate::crate_scope::Kept for MemKept {
    fn find(&self, krate: usize) -> Option<(&[u8], &[u8], &[u8])> {
        let solve = self.kept.lock().unwrap_or_else(|p| panic!("kept: {p}"))[krate]?;
        let now = *self.now.lock().unwrap_or_else(|p| panic!("now: {p}"));
        (now.0[krate][0].1 == solve.text).then(|| {
            self.found.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (solve.record, solve.inner, solve.read)
        })
    }

    fn made(&self, krate: usize, record: &[u8], inner: &[u8], read: &[u8]) {
        let now = *self.now.lock().unwrap_or_else(|p| panic!("now: {p}"));
        let empty = crate::asked::encode(CrateItemKey(ContentId([0; 32])), &[], &[], Alloc::global());
        let read: &'static [u8] = Vec::leak(if self.planted { empty.to_vec() } else { read.to_vec() });
        let solve = KeptSolve { text: now.0[krate][0].1, read, record: Vec::leak(record.to_vec()), inner: Vec::leak(inner.to_vec()) };
        self.kept.lock().unwrap_or_else(|p| panic!("kept: {p}"))[krate] = Some(solve);
    }
}

/// The early cutoff's oracle: after each edit of `c` (a body, a private item, a new public
/// item), solving with the unedited run's kept solves gives every crate the same two records
/// as a cold solve of the edited set, and finds the crates the edit cannot move. Planted: a
/// read list not recorded finds `a` stale after `shut` is made public.
#[test]
fn kept_solves_are_found_only_when_their_reads_hold_and_equal_a_cold_solve() {
    use crate::crate_scope::{Solved, resolve_kept};
    let crates = crates();
    let frame = blaze_mem::Frame::new(blaze_mem::Tag::Nameres).expect("frame");
    let erased = blaze_own::Erased::new(&frame);
    let g = erased.bump();
    let bytes = |s: &Solved<'_>| match s {
        Solved::Solved(c) => (c.record.to_vec(), c.inner.to_vec()),
        Solved::Found { record, inner } => (record.to_vec(), inner.to_vec()),
    };
    for planted in [false, true] {
        let mut stale = Vec::new();
        for (what, edited) in [("body", &TEXTS_BODY), ("private", &TEXTS_PRIV), ("public", &TEXTS_PUB), ("shut made public", &TEXTS_SHUT)] {
            let kept = MemKept { now: std::sync::Mutex::new(&TEXTS), kept: std::sync::Mutex::new(vec![None; crates.len()]), planted, found: 0.into() };
            resolve_kept(&crates, Sysroot::default(), &TEXTS, &Serial, &Ready::serial(WAIT), &kept, g).unwrap_or_else(|e| panic!("first run: {e:?}"));
            assert!(kept.found.load(std::sync::atomic::Ordering::Relaxed) == 0, "nothing is kept before the first run");
            *kept.now.lock().unwrap_or_else(|p| panic!("now: {p}")) = edited;
            let (again, _) = resolve_kept(&crates, Sysroot::default(), edited, &Serial, &Ready::serial(WAIT), &kept, g).unwrap_or_else(|e| panic!("{what}: {e:?}"));
            let (cold, _) = resolve_by_records(&crates, Sysroot::default(), edited, &Serial, &Ready::serial(WAIT), g).unwrap_or_else(|e| panic!("{what} cold: {e:?}"));
            for (i, (a, c)) in again.iter().zip(cold.iter()).enumerate() {
                if bytes(a) != (c.record.to_vec(), c.inner.to_vec()) {
                    stale.push(format!("{what}: crate `{}`", crates[i].name));
                }
            }
            // `c` is solved again (its text moved); `mac` reads nothing that moved.
            let solved: Vec<&str> = again.iter().enumerate().filter(|(_, s)| matches!(s, Solved::Solved(_))).map(|(i, _)| crates[i].name).collect();
            assert!(solved.contains(&"c") && !solved.contains(&"mac"), "{what} (planted {planted}): solved {solved:?}");
        }
        if planted {
            assert!(!stale.is_empty(), "the planted bug (no read list) was not caught");
        } else {
            assert!(stale.is_empty(), "kept solves differ from a cold solve: {stale:?}");
        }
    }
}
