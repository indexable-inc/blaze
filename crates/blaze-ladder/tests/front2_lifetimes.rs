//! The lifetime part of a head (front2/lifetimes.rs; docs/v2/plan/design/flow.md, "What the
//! signature must say (H1)"): one small crate, each fn's `FnLife`, the struct's `AdtLife` and
//! the impl's `ContainerLife` written out. Needs the installed toolchain under
//! `$HOME/.blaze/v2/toolchain`, as tests/front2_items.rs does.
//!
//! Prints, when right: `front2_lifetimes: 5 signatures, 1 struct and 1 impl as written`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::lifetimes::{LifeVariance, Region, Variance};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const EDITION: &str = "blaze-2026.11";

const TEXT: &str = "
struct S<'a, T> { r: &'a T }
struct M<V> { v: V }
impl<V> M<V> { fn get(&self, k: &str) -> &V { let _ = k; &self.v } }
impl<'a> S<'a, u8> { fn h(&self) -> &'a u8 { self.r } }
fn first<'a>(x: &'a (u8, u8)) -> &'a u8 { &x.0 }
fn f(x: &u8) -> &u8 { x }
fn g<'a, 'b: 'a>(x: &'b u8) -> &'a u8 { x }
fn main() {}
";

/// The Reference, "Lifetime elision": "Each elided lifetime in the parameters becomes a
/// distinct lifetime parameter" (`Anon(i)`); "If there is exactly one lifetime used in the
/// parameters (elided or not), that lifetime is assigned to all elided output lifetimes" (`f`);
/// "If the receiver has type `&Self` or `&mut Self`, then the lifetime of that reference to
/// `Self` is assigned to all elided output lifetime parameters" (`get`: the output takes
/// `self`'s region, not `k`'s). A named lifetime is `Named(i)`, the impl's first (`h`: the
/// impl's `'a` is `Named(0)`, and `Self`'s one position is it), then the fn's in declaration
/// order (`g`: `'a` is 0, `'b` is 1, and the declared `'b: 'a` is listed). "Subtyping and
/// Variance": `&'a T` is covariant in `'a` (`S`).
///
/// A wrong reader fails here: one that gives `get`'s output `k`'s region, numbers `g`'s
/// lifetimes by appearance (`'b` first), drops the declared bound, makes `Self` a region apart
/// from the impl's `'a`, or counts a position for `T`.
#[test]
fn the_lifetime_part_of_each_head() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "hello_world", dir: "/hello-world/", root: "/hello-world/src/main.rs", files: &files, edition };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame);

    // The free fns: (name, regions, ends, bounds).
    type Want3 = (&'static str, Vec<Region>, Vec<u32>, Vec<(Region, Region)>);
    let free: [Want3; 3] = [
        ("first", vec![Region::Named(0), Region::Named(0)], vec![1], vec![]),
        ("f", vec![Region::Anon(0), Region::Anon(0)], vec![1], vec![]),
        ("g", vec![Region::Named(1), Region::Named(0)], vec![1], vec![(Region::Named(1), Region::Named(0))]),
    ];
    let mut signatures = 0;
    for (n, regions, ends, bounds) in &free {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[*n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let lib = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        store.ensure(&names, &mut front.keys, Want::Fn(lib)).unwrap_or_else(|e| panic!("`{n}`: no head: {e:?}"));
        let life = store.fn_life(&names, &front.keys, checker_of(lib)).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        assert_eq!(life.regions.to_vec(), *regions, "`{n}`: regions");
        assert_eq!(life.ends.to_vec(), *ends, "`{n}`: ends");
        assert_eq!(life.bounds.to_vec(), *bounds, "`{n}`: bounds");
        assert_eq!(life.container, 0, "`{n}`: a free fn has no container");
        signatures += 1;
    }

    // The methods, each found in its impl by its name.
    let methods: [Want3; 2] = [
        // `&self` (no position in `M<V>`), `k: &str`; the output is `self`'s reference's.
        ("get", vec![Region::Anon(0), Region::Anon(1), Region::Anon(0)], vec![1, 2], vec![]),
        // `&self`, then `Self = S<'a, u8>`'s one position, the impl's `'a`; the output `&'a u8`.
        ("h", vec![Region::Anon(0), Region::Named(0), Region::Named(0)], vec![2], vec![]),
    ];
    let mut impls = 0;
    let impl_keys: Vec<_> = names.items(0).map(|(k, _, _)| k).filter(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some())).collect();
    for k in impl_keys {
        store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("an impl: no head: {e:?}"));
        for (n, regions, ends, bounds) in &methods {
            let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, n) else { continue };
            let life = store.fn_life(&names, &front.keys, mk).unwrap_or_else(|e| panic!("`{n}`: {e}"));
            assert_eq!(life.regions.to_vec(), *regions, "`{n}`: regions");
            assert_eq!(life.ends.to_vec(), *ends, "`{n}`: ends");
            assert_eq!(life.bounds.to_vec(), *bounds, "`{n}`: bounds");
            signatures += 1;
            if *n == "h" {
                // `impl<'a> S<'a, u8>`: one declared lifetime, none elided; its self type's
                // one position is `'a`, and `h`'s one named region is the impl's.
                assert_eq!((life.container, life.named), (1, 1), "`h`: the impl's `'a` is `Named(0)`");
                let c = store.container_life(&names, &front.keys, k).unwrap_or_else(|e| panic!("the impl of `h`: {e}"));
                assert_eq!((c.lifetimes, c.elided), (1, 0), "the impl of `h`: lifetimes");
                assert_eq!(c.self_regions.to_vec(), vec![Region::Named(0)], "the impl of `h`: its self type");
                assert!(c.bounds.is_empty(), "the impl of `h`: no declared bound");
                impls += 1;
            }
        }
    }
    assert_eq!(signatures, 5, "every fn was asked");
    assert_eq!(impls, 1, "the impl of `h` was asked");

    // `struct S<'a, T> { r: &'a T }`: one lifetime, covariant by the table; its one field's
    // one position is `'a` (`T` has none).
    let found = names::path(&names, names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &["S"] }, Alloc::global()).unwrap_or_else(|e| panic!("`S`: {e}"));
    let lib = found.whole(1).expect("`S` is named").key;
    store.ensure(&names, &mut front.keys, Want::Adt(lib)).unwrap_or_else(|e| panic!("`S`: no row: {e:?}"));
    let s = store.adt_life(&names, &front.keys, lib).unwrap_or_else(|e| panic!("`S`: {e}"));
    assert_eq!(s.lifetimes, 1, "`S`: lifetimes");
    assert_eq!(s.variances.to_vec(), vec![LifeVariance { variance: Variance::Co, computed: true }], "`S`: variances");
    assert!(s.bounds.is_empty(), "`S`: no declared bound");
    assert_eq!(s.fields.to_vec(), vec![Region::Named(0)], "`S`: its field's regions");
    assert_eq!(s.field_ends.to_vec(), vec![1], "`S`: where its field ends");

    println!("front2_lifetimes: {signatures} signatures, 1 struct and {impls} impl as written");
}
