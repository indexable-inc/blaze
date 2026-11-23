//! A DEPENDENCY's trait fn returning `impl Future`, awaited on a CONCRETE receiver in another
//! crate. RFC 3425 ("Return position `impl Trait` in traits"): each such `impl Trait` is an
//! associated type of the trait, `<Self as Tr>::{synthetic}`, whose bounds are the `impl
//! Trait`'s and whose value in an impl is the impl fn's return type; a provided fn's default is
//! its body's type. The Reference, "Await expressions": the `.await` is the future's `Output`.
//!
//! - `by_impl` (the impl writes the fn as an `async fn`), `by_default` (the trait provides the
//!   fn, the impl does not write it): clean, the projection normalized by the impl's row or the
//!   default.
//! - `by_env` (tempo crates/node/src/executed_state.rs:123: the receiver is concrete but a
//!   where-clause `Concrete: E<N>` is in the env, so the env's candidate is taken over the impl
//!   and the projection stays RIGID): clean by the associated type's bounds, the trait fn's
//!   `Future<Output = u8> + Send` with `Self` the projection (blaze-typeck infer/facts/
//!   item_bounds.rs `opaque_self_as_projection`). Before (run a0700, 2026-11-21): three E0277
//!   "is not a future" at the one `.await`, the bounds kept with `Self` the opaque and read as
//!   no bound of the projection.
//! - TWINS `*_bad`: the output used as `u64`: exactly ONE finding, E0271 naming `u8` found
//!   (blaze-typeck machine.rs `projection_found`; rustc words it E0308), never three (machine.rs
//!   `drop_projections_of_unmet_traits`).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.
//! NOT YET RUN (written 2026-11-21 by a write-only lane).

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;

const DEP: &str = r#"
use std::future::Future;
pub trait B { fn build(self) -> impl Future<Output = u8> + Send; }
pub trait P { fn build_p(self) -> impl Future<Output = u8> + Send { async { 1u8 } } }
pub trait E<N> { fn build_e(self, n: N) -> impl Future<Output = u8> + Send; }
pub struct Concrete;
impl B for Concrete { async fn build(self) -> u8 { 1 } }
impl P for Concrete {}
impl<N> E<N> for Concrete { async fn build_e(self, n: N) -> u8 { drop(n); 1 } }
"#;

const USER: &str = r#"
use rdep::{B, Concrete, E, P};
async fn by_impl() -> u8 { Concrete.build().await }
async fn by_impl_bad() -> u64 { let v: u64 = Concrete.build().await; v }
async fn by_default() -> u8 { Concrete.build_p().await }
async fn by_default_bad() -> u64 { let v: u64 = Concrete.build_p().await; v }
async fn by_env<N>(n: N) -> u8 where Concrete: E<N> { Concrete.build_e(n).await }
async fn by_env_bad<N>(n: N) -> u64 where Concrete: E<N> { let v: u64 = Concrete.build_e(n).await; v }
fn main() {}
"#;

/// The texts of the two-crate layout: the dependency's file for crate `dep`, the rest as
/// [`Inputs`] reads them.
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

/// Each named body of [`USER`]: (clean, the diagnostics' codes, the verdict as printed).
fn check_bodies(bodies: &[&str]) -> Vec<(bool, Vec<&'static str>, String)> {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME");
    let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("installed toolchain");
    let root = "/rpd/src/main.rs";
    let files = [(root.to_owned(), USER.to_owned())];
    let program = UserCrate {
        name: "rpd", dir: "/rpd/", root, files: &files,
        edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&program, &tc).expect("fixture crate");
    let mut crates: Vec<CrateInput<'_>> = inputs.crates();
    let dep_files = [("/rdep/src/lib.rs".to_owned(), DEP.to_owned())];
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let dep_at = crates.len();
    let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("rdep", dep_at)].into_boxed_slice());
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = user_deps;
    crates.push(CrateInput {
        name: "rdep",
        key: CrateItemKey(ContentId::of("blaze-ladder rpitit dependency test crate", b"rdep")),
        edition: crate_edition,
        implicit,
        root: "/rdep/src/lib.rs",
        shown_from: "/rdep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep: dep_at, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (two crates): {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: USER }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("checker frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("body checker");
    let key_of = |name: &str| names.items(0).find_map(|(key, _, _)| (names.item_text(key)?.name == Some(name)).then_some(key)).expect(name);
    bodies
        .iter()
        .map(|&name| {
            let v = world.check(BodyRef::Free(key_of(name)), alloc);
            (v.is_clean(), v.diagnostics.iter().map(|d| d.code).collect(), format!("{v:?}"))
        })
        .collect()
}

/// On failure panics naming the case and its verdict.
#[test]
fn a_dependencys_trait_fn_future_on_a_concrete_receiver_is_awaited_by_rust_rules() {
    let names = ["by_impl", "by_impl_bad", "by_default", "by_default_bad", "by_env", "by_env_bad"];
    let got = check_bodies(&names);
    for (name, g) in names.iter().zip(got.iter()) {
        println!("{name}: clean {} codes {:?}: {}", g.0, g.1, g.2);
    }
    for pair in 0..3 {
        let (good, bad) = (&got[2 * pair], &got[2 * pair + 1]);
        let (gn, bn) = (names[2 * pair], names[2 * pair + 1]);
        assert!(good.0, "{gn}: the future's output is `u8` by Rust's rules: clean: {}", good.2);
        assert!(!bad.0, "TWIN {bn}: `u8` used as `u64`: not clean: {}", bad.2);
        assert_eq!(bad.1, ["E0271"], "TWIN {bn}: exactly one finding, E0271 (`u8` found, `u64` expected): {}", bad.2);
        assert!(!bad.2.contains("is not a future"), "TWIN {bn}: never \"not a future\": {}", bad.2);
    }
}
