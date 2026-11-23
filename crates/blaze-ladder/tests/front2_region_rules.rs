//! What the head lowering must lay as a head's regions (D148; docs/v2/plan/design/flow.md,
//! section L): the rules the flow lowering's tests used to state of the TEXT reader
//! (`read_signature`, `FnLife`, `ContainerLife`, `AdtLife`), restated of the one producer, the
//! region record each head files (`store.regions`). The flow lowering's fixtures
//! (blaze-flow-lower sig_alias_tests.rs, sig_assoc_tests.rs, trait_ties_tests.rs, ...) lay
//! these same columns by hand; this file is what makes them true of the real producer. Needs
//! the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_head_records.rs
//! does.
//!
//! The numbering (crates/blaze-typeck/src/regions.rs `Numbering`): `Param(i)` is the
//! container's lifetimes, then the item's own in DECLARATION order, then one per elided input;
//! a fn's column is laid over `FnPtr(inputs.., output)`, its slots in the type's pre-order. A
//! lifetime is late-bound when it is in no where-clause or bound and is in the inputs.
//!
//! Sources, each test citing the line it states: the Reference, "Lifetime elision", "Type
//! aliases", "Generic parameters", "Trait and lifetime bounds", "Associated items".
//!
//! Prints, when right, one line per test: `front2_region_rules: <test>: <n> heads as the
//! rules say`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::{HeadAt, decode, encode, from_store};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of, opaque_key};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::region_facts::{Outlives, RegionFacts};
use blaze_typeck::regions::Region;

const EDITION: &str = "blaze-2026.11";

const P0: Region = Region::Param(0);
const P1: Region = Region::Param(1);
const P2: Region = Region::Param(2);
const ST: Region = Region::Static;

/// What one test asks of the crate.
#[derive(Clone, Copy, Debug)]
enum Ask {
    /// A free fn's head, by name.
    Free(&'static str),
    /// A method's head, by name, in whichever impl of the crate has it.
    Method(&'static str),
    /// The impl header of the impl that has the method of this name.
    HeaderOf(&'static str),
    /// A struct's head, by name: its fields' columns in (variant, field) order.
    Adt(&'static str),
    /// The `type <name> = U;` value's column of whichever impl of the crate files one, read
    /// through the store's reader and through the impl's record encoded and decoded again
    /// (`RegionFacts::impl_assoc`; the three must agree).
    Assoc(&'static str),
    /// The future of the `async fn` of this name (its opaque 0): its captures as
    /// `Param(0..n)`, `n` its filed `opaque_captures`, which the store's reader must give too.
    FutureOf(&'static str),
}

/// A head's regions as the store filed them.
#[derive(Debug, PartialEq, Eq)]
struct Laid {
    /// A fn's signature column, an impl's self type's column, or an ADT's fields' columns
    /// one after another.
    col: Vec<Region>,
    /// Whether each `Param(i)` is late-bound.
    late: Vec<bool>,
    /// The declared `'long: 'short` clauses, as the params and the head list them.
    outlives: Vec<(Region, Region)>,
}

/// The toolchain on its shelf.
fn toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

/// Lowers `text` as one crate and answers each of `asks`: the regions filed, or why there are
/// none (the store's refusal, the fn's gap, or the region gap).
fn heads(text: &'static str, asks: &[Ask]) -> Vec<Result<Laid, String>> {
    heads_in(EDITION, text, asks)
}

/// [`heads`] for a crate of edition `edition_name` (a manifest's `edition`).
fn heads_in(edition_name: &str, text: &'static str, asks: &[Ask]) -> Vec<Result<Laid, String>> {
    let tc = toolchain();
    let files = [("/krate/src/main.rs".to_string(), text.to_string())];
    let edition = edition::PackageEdition::of_manifest(edition_name).expect("a known edition");
    let user = UserCrate { name: "krate", dir: "/krate/", root: "/krate/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    // The test's one heap handle, named once.
    let g = Alloc::global();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, g).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, g);
    let impls: Vec<_> = names.items(0).map(|(k, _, _)| k).filter(|&k| names.item_text(k).is_some_and(|t| t.impl_header.is_some())).collect();

    let mut out = Vec::new();
    for &ask in asks {
        let found = |ns: Namespace, n: &str| {
            let found = names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[n] }, g).unwrap_or_else(|e| panic!("`{n}`: {e}"));
            found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key
        };
        // The want to make and the key its regions are filed under.
        let got: Result<(Want, blaze_typeck::ItemKey), String> = match ask {
            Ask::Free(n) => {
                let lib = found(Namespace::Values, n);
                Ok((Want::Fn(lib), checker_of(lib)))
            }
            Ask::Adt(n) => {
                let lib = found(Namespace::Types, n);
                Ok((Want::Adt(lib), checker_of(lib)))
            }
            Ask::FutureOf(n) => {
                let lib = found(Namespace::Values, n);
                Ok((Want::Fn(lib), opaque_key(checker_of(lib), 0)))
            }
            Ask::Method(n) | Ask::HeaderOf(n) => {
                let mut hit = None;
                for &k in &impls {
                    store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("an impl: no head: {e:?}"));
                    if let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, n) {
                        hit = Some(match ask {
                            Ask::Method(_) => (Want::MemberFn(k, mk), mk),
                            _ => (Want::Impl(k), checker_of(k)),
                        });
                    }
                }
                hit.ok_or_else(|| format!("`{n}`: in no impl"))
            }
            Ask::Assoc(n) => {
                let name = blaze_typeck::Name::of(n);
                let mut hit = None;
                let mut gaps = Vec::new();
                for &k in &impls {
                    store.ensure(&names, &mut front.keys, Want::Impl(k)).unwrap_or_else(|e| panic!("an impl: no head: {e:?}"));
                    let ck = checker_of(k);
                    match store.regions.made(ck) {
                        Some(f) if f.regions().impl_assoc.iter().any(|e| e.0 == name) => hit = Some((Want::Impl(k), ck)),
                        Some(_) => {}
                        None => gaps.push(format!("{:?}", store.region_gaps.get(&ck))),
                    }
                }
                hit.ok_or_else(|| format!("`{n}`: in no impl's filed regions; the impls' region gaps: {gaps:?}"))
            }
        };
        let laid = got.and_then(|(want, key)| {
            store.ensure(&names, &mut front.keys, want).map_err(|e| format!("ensure: {e:?}"))?;
            if let Some(why) = store.fn_gaps.get(&key) {
                return Err(format!("{why:?}"));
            }
            let Some(filed) = store.regions.made(key) else {
                return Err(format!("no regions filed: {:?}", store.region_gaps.get(&key)));
            };
            let hr = filed.regions();
            let col: Vec<Region> = match ask {
                Ask::Free(_) | Ask::Method(_) => hr.sig.as_ref().ok_or("no signature column")?.as_slice().to_vec(),
                Ask::HeaderOf(_) => hr.impl_header.as_ref().ok_or("no impl header column")?.0.as_slice().to_vec(),
                Ask::FutureOf(_) => {
                    let n = hr.opaque_captures.ok_or("no captures filed")?;
                    let read = store.regions.reader(g).opaque_captures(key).map_err(|e| format!("the reader: {e:?}"))?;
                    if read != n {
                        return Err(format!("the reader gave {read} captures, the store filed {n}"));
                    }
                    (0..n).map(Region::Param).collect()
                }
                Ask::Adt(_) => hr.fields.iter().flat_map(|(_, _, c)| c.as_slice().iter().copied()).collect(),
                Ask::Assoc(n) => {
                    let name = blaze_typeck::Name::of(n);
                    let col = hr.impl_assoc.iter().find(|e| e.0 == name).ok_or("no associated type column")?.1.as_slice().to_vec();
                    // The reader the flow lowering asks gives the same column.
                    let reader = store.regions.reader(g);
                    let read = reader.impl_assoc(key, name).map_err(|e| format!("the reader: {e:?}"))?;
                    if read.as_slice() != &col[..] {
                        return Err(format!("the reader gave {:?}, the store filed {col:?}", read.as_slice()));
                    }
                    // Through the record: encoded, decoded into the store's table, the same column.
                    let made = from_store(&store, HeadAt::Impl(key)).map_err(|e| format!("from_store: {e}"))?;
                    let bytes = encode(made, store.tys.view(), g).map_err(|e| format!("encode: {e}"))?;
                    let back = decode(&bytes, made.text(), &store.tys, g).map_err(|e| format!("decode: {e}"))?;
                    let back_col = back.as_ref().regions().and_then(|f| f.regions().impl_assoc.iter().find(|e| e.0 == name).map(|e| e.1.as_slice().to_vec()));
                    if back_col.as_deref() != Some(&col[..]) {
                        return Err(format!("the record gave {back_col:?}, the store filed {col:?}"));
                    }
                    col
                }
            };
            let mut late = Vec::new();
            for i in 0..hr.params.count() {
                late.push(hr.params.is_late(i).map_err(|e| format!("{e:?}"))?);
            }
            let mut outlives: Vec<(Region, Region)> = hr.params.outlives().to_vec();
            for o in hr.outlives.iter() {
                if let Outlives::Region { long, short } = *o
                    && !outlives.contains(&(long, short))
                {
                    outlives.push((long, short));
                }
            }
            Ok(Laid { col, late, outlives })
        });
        out.push(laid);
    }
    out
}

fn laid<'a>(r: &'a Result<Laid, String>, what: &str) -> &'a Laid {
    r.as_ref().unwrap_or_else(|e| panic!("`{what}`: no regions: {e}"))
}

/// The planted numbering a wrong producer would use: `Param`s renumbered by first appearance
/// in the column, not by declaration.
fn by_first_appearance(col: &[Region]) -> Vec<Region> {
    let mut seen: Vec<Region> = Vec::new();
    col.iter()
        .map(|&r| match r {
            Region::Param(_) => {
                let i = seen.iter().position(|&s| s == r).unwrap_or_else(|| {
                    seen.push(r);
                    seen.len() - 1
                });
                Region::Param(i as u32)
            }
            other => other,
        })
        .collect()
}

/// Was tests/front2_lifetimes.rs (the text reader's `FnLife`, `ContainerLife`, `AdtLife`).
/// The Reference, "Lifetime elision": "Each elided lifetime in the parameters becomes a
/// distinct lifetime parameter"; "If there is exactly one lifetime used in the parameters
/// (elided or not), that lifetime is assigned to all elided output lifetimes" (`f`); "If the
/// receiver has type `&Self` or `&mut Self`, then the lifetime of that reference to `Self` is
/// assigned to all elided output lifetime parameters" (`get`: the output takes `self`'s, not
/// `k`'s). The numbering: the impl's `'a` is `Param(0)` for `h` (the container's first), then
/// `&self`'s elided `Param(1)`; `g`'s `'a` is 0 and `'b` 1 by declaration, the declared
/// `'b: 'a` listed and `'b` early-bound by it ("Generic parameters", late-bound rule). A field
/// `r: &'a T` of `S<'a, T>` is `[Param(0)]` (`T` has no slot).
///
/// Controls: `g`'s column renumbered by first appearance (`'b` first) is not the laid one;
/// `get`'s output is not `k`'s region; `h`'s output is not `&self`'s elided region.
#[test]
fn each_head_numbers_its_lifetimes_by_the_rules() {
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
    let asks = [Ask::Free("first"), Ask::Free("f"), Ask::Free("g"), Ask::Method("get"), Ask::Method("h"), Ask::HeaderOf("h"), Ask::Adt("S")];
    let got = heads(TEXT, &asks);
    let first = laid(&got[0], "first");
    assert_eq!(first.col, [P0, P0], "`first`'s column");
    assert_eq!(first.late, [true], "`first`'s `'a` is in no bound and in the inputs: late-bound");
    let f = laid(&got[1], "f");
    assert_eq!(f.col, [P0, P0], "`f`: the one input lifetime is the output's");
    assert_eq!(f.late, [true], "`f`'s elided lifetime is late-bound");
    let g = laid(&got[2], "g");
    assert_eq!(g.col, [P1, P0], "`g`: `'b` is 1 and `'a` 0, by declaration");
    assert_eq!(g.outlives, [(P1, P0)], "`g`: the declared `'b: 'a`");
    assert_eq!(g.late, [false, false], "`g`: both are in the bound `'b: 'a`, so early-bound");
    assert_ne!(by_first_appearance(&g.col), g.col, "CONTROL: first-appearance numbering would lay `g` alike");
    let get = laid(&got[3], "get");
    // `&self` over `&M<V>` (no slot in `M<V>`), `k: &str`, the output `&V`.
    assert_eq!(get.col, [P0, P1, P0], "`get`: the output takes `&self`'s lifetime");
    assert_ne!(get.col[2], get.col[1], "CONTROL: `get`'s output took `k`'s lifetime");
    let h = laid(&got[4], "h");
    // `&self` over `&S<'a, u8>`: the elided `Param(1)`, then `S`'s one slot, the impl's `'a`.
    assert_eq!(h.col, [P1, P0, P0], "`h`: `&'a u8` is the impl's `'a`");
    assert_eq!(h.late, [false, true], "`h`: the impl's `'a` is early, `&self`'s elided one late");
    assert_ne!(h.col[2], h.col[0], "CONTROL: `h`'s written `'a` took `&self`'s elided lifetime");
    let header = laid(&got[5], "the impl of `h`");
    assert_eq!(header.col, [P0], "`impl<'a> S<'a, u8>`: its self type's one slot is `'a`");
    assert!(header.outlives.is_empty(), "the impl of `h`: no declared bound");
    let s = laid(&got[6], "S");
    assert_eq!(s.col, [P0], "`S<'a, T> {{ r: &'a T }}`: its field's one slot is `'a`");
    assert_eq!(s.late, [false], "an ADT's lifetimes are early-bound");
    println!("front2_region_rules: each_head_numbers_its_lifetimes_by_the_rules: 7 heads as the rules say");
}

/// Was blaze-flow-lower sig_alias_tests.rs (`an_alias_to_a_static_type_is_static`,
/// `every_alias_form_reads_its_expansion`, `the_grant_shape_reads_static`). The Reference,
/// "Type aliases": "A type alias defines a new name for an existing type": its slots are the
/// expansion's, the alias's arguments substituted. `type L = Hold<'static>`: `keep(h: L)` is
/// `['static]` and `scope(x: &S) -> L` `[Param(0), 'static]` (the output demands nothing of
/// `x`). `type R<'x> = (&'x S, Hold<'x>, &'x Hold<'x>)`: `r<'q>(v: R<'q>)` is four slots of
/// `'q`; `type R2<'x> = R<'x>` (an alias of an alias) the same. `type V<T> = (T, Hold<'static>)`:
/// `v<'q>(x: V<&'q S>)` is `[Param(0), 'static]`. `type Sw<'a, 'b> = (&'b S, &'a S)`:
/// `sw<'p, 'q>(x: Sw<'p, 'q>)` is `[Param(1), Param(0)]`. The Grant shape (bstd's 24 false
/// `UnprovenOutlives`): `impl<'g> Granting<'g> { fn scope(&self) -> (&'g Hold<'static>, &'g S) }`
/// written through `type LA = Hold<'static>` is `[Param(1), Param(0), Param(0), 'static, Param(0)]`.
///
/// Controls: the alias NOT expanded (a fresh lifetime for `L`'s one slot, as before piece
/// sig-paths) is not `'static`; `Sw`'s arguments lined up with the expansion in written order
/// (`[Param(0), Param(1)]`) are not the laid column.
#[test]
fn an_alias_is_laid_as_its_expansion() {
    const TEXT: &str = "
struct S;
struct Hold<'h> { r: &'h S }
struct Granting<'g> { s: &'g S }
type L = Hold<'static>;
type LA = Hold<'static>;
type R<'x> = (&'x S, Hold<'x>, &'x Hold<'x>);
type R2<'x> = R<'x>;
type V<T> = (T, Hold<'static>);
type Sw<'a, 'b> = (&'b S, &'a S);
fn keep(h: L) { let _ = h; }
fn scope(x: &S) -> L { let _ = x; loop {} }
fn r<'q>(v: R<'q>) { let _ = v; }
fn r2<'q>(v: R2<'q>) { let _ = v; }
fn v<'q>(x: V<&'q S>) { let _ = x; }
fn sw<'p, 'q>(x: Sw<'p, 'q>) { let _ = x; }
impl<'g> Granting<'g> { fn grant(&self) -> (&'g LA, &'g S) { loop {} } }
fn main() {}
";
    let asks = [Ask::Free("keep"), Ask::Free("scope"), Ask::Free("r"), Ask::Free("r2"), Ask::Free("v"), Ask::Free("sw"), Ask::Method("grant")];
    let got = heads(TEXT, &asks);
    let keep = laid(&got[0], "keep");
    assert_eq!(keep.col, [ST], "`keep(h: L)`: `L` is `Hold<'static>`");
    assert!(keep.late.is_empty(), "CONTROL: `L`'s slot became a fresh lifetime of `keep`: {keep:?}");
    let scope = laid(&got[1], "scope");
    assert_eq!(scope.col, [P0, ST], "`scope(x: &S) -> L`: the output is `'static`, not `x`'s");
    assert_ne!(scope.col[1], scope.col[0], "CONTROL: the elided output took `x`'s lifetime");
    let r = laid(&got[2], "r");
    assert_eq!(r.col, [P0, P0, P0, P0], "`R<'q>`: four slots, each `'q`");
    let r2 = laid(&got[3], "r2");
    assert_eq!(r2.col, r.col, "`R2<'q>` is `R<'q>`");
    let v = laid(&got[4], "v");
    assert_eq!(v.col, [P0, ST], "`V<&'q S>`: `T`'s slot `'q`, then the expansion's `'static`");
    let sw = laid(&got[5], "sw");
    assert_eq!(sw.col, [P1, P0], "`Sw<'p, 'q>` is `(&'q S, &'p S)`");
    assert_ne!(sw.col, [P0, P1], "CONTROL: the arguments were lined up with the expansion in written order");
    let grant = laid(&got[6], "grant");
    // `&self` over `&Granting<'g>`: elided `Param(1)`, `'g`; the output `(&'g Hold<'static>, &'g S)`.
    assert_eq!(grant.col, [P1, P0, P0, ST, P0], "the Grant shape: `LA`'s slot is `'static`");
    assert_ne!(grant.col[3], grant.col[0], "CONTROL: `LA`'s slot took `&self`'s elided lifetime");
    println!("front2_region_rules: an_alias_is_laid_as_its_expansion: 7 heads as the rules say");
}

/// Was blaze-flow-lower sig_alias_tests.rs `the_references_elision_examples`, the Reference's
/// own list ("Lifetime elision", "Lifetime elision in functions"): `fn print(s: &str)` is
/// `[Param(0)]`; `fn substr(s: &str, until: usize) -> &str` `[Param(0), Param(0)]`; `fn
/// new(buf: &mut [u8]) -> Hold<'_>` and `fn new2(buf: &mut [u8]) -> Hold` (the elided path
/// argument) both `[Param(0), Param(0)]`; `fn frob(s: &str, t: &str) -> &str` is refused, "an
/// illegal elision" (E0106).
///
/// Control: `frob` with one input (`frob1(s: &str) -> &str`) is laid, its output `s`'s, so the
/// refusal is the second input's.
#[test]
fn the_references_elision_examples() {
    const TEXT: &str = "
struct Hold<'h> { r: &'h [u8] }
fn print(s: &str) { let _ = s; }
fn substr(s: &str, until: usize) -> &str { let _ = until; s }
fn new(buf: &mut [u8]) -> Hold<'_> { Hold { r: buf } }
#[allow(elided_lifetimes_in_paths)]
fn new2(buf: &mut [u8]) -> Hold { Hold { r: buf } }
fn frob1(s: &str) -> &str { s }
fn main() {}
";
    const E0106: &str = "fn frob(s: &str, t: &str) -> &str { let _ = t; s }\nfn main() {}\n";
    let got = heads(TEXT, &[Ask::Free("print"), Ask::Free("substr"), Ask::Free("new"), Ask::Free("new2"), Ask::Free("frob1")]);
    assert_eq!(laid(&got[0], "print").col, [P0], "`print`");
    assert_eq!(laid(&got[1], "substr").col, [P0, P0], "`substr`: one input lifetime, given to the output");
    assert_eq!(laid(&got[2], "new").col, [P0, P0], "`new -> Hold<'_>`");
    assert_eq!(laid(&got[3], "new2").col, [P0, P0], "`new2 -> Hold`: the elided path argument is `'_`");
    assert_eq!(laid(&got[4], "frob1").col, [P0, P0], "CONTROL: one input is laid");
    let frob = heads(E0106, &[Ask::Free("frob")]);
    let refused = frob[0].as_ref().expect_err("`frob`: two input lifetimes and an elided output were laid");
    assert!(refused.contains("Missing") || refused.contains("E0106"), "`frob`'s refusal names E0106: {refused}");
    println!("front2_region_rules: the_references_elision_examples: 6 heads as the rules say");
}

/// The Reference, "Impl trait", "Capturing": a trait's or a trait impl's `-> impl Trait`
/// captures every generic parameter in scope, lifetimes included, in EVERY edition (RFC 3425).
/// tempo's 452 borrow refusals "a region position the signature's walk does not lay" (first
/// crates/zones/node/src/role.rs, calling a clock's `fn sleep(&self, duration: Duration) -> impl
/// Future`), reduced to an edition-2021 crate: `impl Clock for Ctx { fn sleep(&self, d: u32) ->
/// impl Sized }` is `[Param(0), Param(0)]` (`&self`'s elided lifetime, then the opaque's one
/// captured lifetime, that same one), as in edition 2024. Before, front2 heads.rs `fn_head` read
/// it by a free fn's pre-2024 rule (the lifetimes its bounds name: none of the one in scope),
/// left the opaque's captures undecided and refused its regions.
///
/// Control (rule 23): the same signature as a FREE fn in 2021 (`fn sleep_free(c: &Ctx, d: u32)
/// -> impl Sized`) captures NO lifetime (its bounds name none: RFC 3498, "Rust 2021 and
/// earlier"), so its column is `[Param(0)]` alone: the trait rule is what gives the method's
/// opaque its slot. (Before `Captures`, that smaller set was refused by name.)
#[test]
fn a_trait_methods_impl_trait_captures_every_lifetime_in_every_edition() {
    const TEXT: &str = "
pub trait Clock { fn sleep(&self, d: u32) -> impl Sized; }
pub struct Ctx;
impl Clock for Ctx { fn sleep(&self, d: u32) -> impl Sized { d } }
fn main() {}
";
    const FREE: &str = "
pub struct Ctx;
pub fn sleep_free(c: &Ctx, d: u32) -> impl Sized { let _ = c; d }
fn main() {}
";
    let old = heads_in("2021", TEXT, &[Ask::Method("sleep")]);
    assert_eq!(laid(&old[0], "sleep (2021)").col, [P0, P0], "a trait impl's `-> impl Sized` captures `&self`'s lifetime in 2021");
    let new = heads_in("2024", TEXT, &[Ask::Method("sleep")]);
    assert_eq!(laid(&new[0], "sleep (2024)").col, [P0, P0], "the same in 2024");
    let free = heads_in("2021", FREE, &[Ask::Free("sleep_free")]);
    assert_eq!(laid(&free[0], "sleep_free (2021)").col, [P0], "CONTROL: a free fn's 2021 opaque naming no lifetime captures none");
    println!("front2_region_rules: a_trait_methods_impl_trait_captures_every_lifetime_in_every_edition: 3 heads as the rules say");
}

/// RFC 3617 (precise capturing; in traits since Rust 1.87): a written `use<..>` decides an
/// opaque's captured lifetimes exactly, under every rule. tempo's 485 borrow refusals "a region
/// position the signature's walk does not lay" (first crates/contracts/src/precompiles/
/// tip20_channel_reserve.rs) all call commonware-runtime 2026.10.0's `Clock::sleep(&self,
/// duration: Duration) -> impl Future<Output = ()> + Send + 'static + use<Self>` (its impls:
/// `+ use<>`), reduced here: it captures `Self` and NO lifetime, so the method's column is
/// `[Param(0)]` (`&self`'s, and no slot for the opaque). A proper subset is laid as the list it
/// is: `pick<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized + use<'b>` is `[P0, P1, P1]`.
/// Before, front2 heads.rs `fn_head` skipped any opaque with a `use<..>` bound ("not decided
/// here") and every caller's borrow check was refused.
///
/// Twins (rule 23): the same method with no `use<..>` captures `&self`'s lifetime (`[P0, P0]`:
/// the trait rule), and `pick` with no `use<..>` in 2024 captures both (`[P0, P1, P0, P1]`), so
/// the shorter columns above are the `use<..>`'s, not a dropped slot.
#[test]
fn a_use_bound_decides_an_opaques_captures() {
    const USE: &str = "
pub trait Clock { fn sleep(&self, d: u32) -> impl Sized + use<Self>; }
pub struct Ctx;
impl Clock for Ctx { fn sleep(&self, d: u32) -> impl Sized + use<> { d } }
pub fn pick<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized + use<'b> { let _ = x; y }
fn main() {}
";
    const ALL: &str = "
pub trait Clock { fn sleep(&self, d: u32) -> impl Sized; }
pub struct Ctx;
impl Clock for Ctx { fn sleep(&self, d: u32) -> impl Sized { d } }
pub fn pick<'a, 'b>(x: &'a u8, y: &'b u8) -> impl Sized { let _ = x; y }
fn main() {}
";
    let got = heads_in("2024", USE, &[Ask::Method("sleep"), Ask::Free("pick")]);
    assert_eq!(laid(&got[0], "sleep + use<>").col, [P0], "`-> impl Sized + use<>` captures no lifetime: no slot for the opaque");
    assert_eq!(laid(&got[1], "pick + use<'b>").col, [P0, P1, P1], "`-> impl Sized + use<'b>` captures `'b` alone");
    let twin = heads_in("2024", ALL, &[Ask::Method("sleep"), Ask::Free("pick")]);
    assert_eq!(laid(&twin[0], "sleep").col, [P0, P0], "TWIN: with no `use<..>` the method's opaque captures `&self`'s lifetime");
    assert_eq!(laid(&twin[1], "pick").col, [P0, P1, P0, P1], "TWIN: with no `use<..>` in 2024 `pick`'s opaque captures both");
    println!("front2_region_rules: a_use_bound_decides_an_opaques_captures: 4 heads as the rules say");
}

/// A qualified projection in a signature, `<Wrap<'a> as Tr>::A`, is laid as the projection
/// (front2 types.rs `qualified_slots`): its own lifetimes (none), then its arguments' slots
/// (`Wrap<'a>`'s `'a`). Never normalized at the head (a head is a function of its own text):
/// the flow lowering maps `Inner<'a>` through the impl's column (`RegionFacts::impl_assoc`).
/// `get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8` is `[P0, P0]`. Before it was refused ("a
/// projection whose arguments or trait hold a lifetime"), so no body calling or being `get` was
/// borrow-checked (front2_closure_sigs.rs).
///
/// TWIN (rule 23): the same projection through a trait that declares a lifetime (`Tr2<'a>`)
/// stays refused by name: its lifetime arguments are not read here, never guessed.
#[test]
fn a_qualified_projection_lays_its_arguments_slots() {
    const TEXT: &str = "
pub struct Inner<'a>(pub &'a u8);
pub struct Wrap<'a>(pub &'a u8);
pub trait Tr { type A; }
impl<'a> Tr for Wrap<'a> { type A = Inner<'a>; }
pub trait Tr2<'t> { type A; }
impl<'a> Tr2<'a> for Wrap<'a> { type A = Inner<'a>; }
pub fn get<'a>(x: <Wrap<'a> as Tr>::A) -> &'a u8 { x.0 }
pub fn get2<'a>(x: <Wrap<'a> as Tr2<'a>>::A) -> &'a u8 { x.0 }
fn main() {}
";
    let got = heads_in("2024", TEXT, &[Ask::Free("get"), Ask::Free("get2")]);
    assert_eq!(laid(&got[0], "get").col, [P0, P0], "`<Wrap<'a> as Tr>::A` lays `Wrap<'a>`'s slot, then the output's `'a`");
    assert_eq!(laid(&got[1], "get2").col, [P0, P0, P0], "`<Wrap<'a> as Tr2<'a>>::A` lays `Wrap<'a>`'s slot, `Tr2<'a>`'s lifetime slot, then the output's `'a`");
    println!("front2_region_rules: a_qualified_projection_lays_its_arguments_slots: get [P0, P0]; get2 [P0, P0, P0]");
}

/// An `async fn`'s future captures every lifetime in scope (the Reference, "Async
/// functions": `async fn example(x: &str) -> usize` is "roughly equivalent to" `fn
/// example<'a>(x: &'a str) -> impl Future<Output = usize> + 'a`; in 2024 an opaque captures all
/// in-scope generic parameters): `two(x: &u8, y: &u8)`'s future captures its two elided
/// lifetimes, `[P0, P1]`, and `none()`'s none. The store's reader gives the same count
/// (`RegionReader::opaque_captures`, what the borrow check's generic shape reads). Before, the
/// future's regions were refused ("an `async fn`'s future"), so every body calling an `async
/// fn` with a lifetime had its call's shape refused.
///
/// TWIN (rule 23): `plain`, the same signature without `async`, has no future: nothing is
/// filed under its opaque 0.
#[test]
fn an_async_fns_future_captures_every_lifetime_in_scope() {
    const TEXT: &str = "
async fn two(x: &u8, y: &u8) -> u8 { *x + *y }
async fn none() -> u8 { 0 }
fn plain(x: &u8, y: &u8) -> u8 { *x + *y }
fn main() {}
";
    let got = heads_in("2024", TEXT, &[Ask::FutureOf("two"), Ask::FutureOf("none"), Ask::FutureOf("plain")]);
    assert_eq!(laid(&got[0], "two's future").col, [P0, P1], "`two`'s future captures both elided lifetimes");
    assert_eq!(laid(&got[1], "none's future").col, Vec::<Region>::new(), "`none`'s future captures nothing");
    assert!(got[2].is_err(), "TWIN: a fn that is not `async` has no future filed: {:?}", got[2]);
    println!("front2_region_rules: an_async_fns_future_captures_every_lifetime_in_scope: two [P0, P1], none [], plain none filed");
}

/// tempo's `zone_node::rpc::operator_commitment` (`async fn operator_commitment<E>(eth_api: &E,
/// l1_state: &L1StateProvider, address: Address, block_id: BlockId) -> Result<B256,
/// ErrorObjectOwned>`) and `rpc::tests::rpc_call`, reduced: jsonrpsee's `type ErrorObjectOwned =
/// ErrorObject<'static>` written as a one-name type ARGUMENT. The Reference, "Type aliases": its
/// slots are its expansion's, `'static`; the output elides nothing, so two input lifetimes are
/// no E0106 (rustc accepts it). Before: a one-name argument was laid as the ADT its made type
/// is (`ErrorObject` written bare, its lifetime elided), a false E0106 that left both heads
/// unfrozen and four bodies unchecked. `two(a: &S, b: &S) -> Result<u8, L>` is `[Param(0),
/// Param(1), 'static]`; `one(a: &S) -> Result<u8, L>` `[Param(0), 'static]` (before, wrongly
/// `[Param(0), Param(0)]`); the async form, tempo's, is laid.
///
/// Control (rule 23): the same output written with the ADT bare (`Result<u8, Hold>`, whose
/// lifetime IS elided) is still E0106, so the alias is what takes the refusal away.
#[test]
fn a_one_name_alias_argument_is_laid_as_its_expansion() {
    const TEXT: &str = "
struct S;
struct Hold<'h> { r: &'h S }
type L = Hold<'static>;
fn two(a: &S, b: &S) -> Result<u8, L> { let _ = (a, b); loop {} }
fn one(a: &S) -> Result<u8, L> { let _ = a; loop {} }
async fn two_async(a: &S, b: &S) -> Result<u8, L> { let _ = (a, b); loop {} }
fn main() {}
";
    const E0106: &str = "
struct S;
struct Hold<'h> { r: &'h S }
#[allow(elided_lifetimes_in_paths)]
fn two_adt(a: &S, b: &S) -> Result<u8, Hold> { let _ = (a, b); loop {} }
fn main() {}
";
    let got = heads(TEXT, &[Ask::Free("two"), Ask::Free("one"), Ask::Free("two_async")]);
    assert_eq!(laid(&got[0], "two").col, [P0, P1, ST], "`two -> Result<u8, L>`: `L`'s slot is `'static`, no E0106");
    assert_eq!(laid(&got[1], "one").col, [P0, ST], "`one -> Result<u8, L>`: `L`'s slot is `'static`, not `a`'s");
    assert!(got[2].is_ok(), "`async fn two_async -> Result<u8, L>` (tempo's shape) is laid: {:?}", got[2]);
    let adt = heads(E0106, &[Ask::Free("two_adt")]);
    let refused = adt[0].as_ref().expect_err("CONTROL: `-> Result<u8, Hold>` with two input lifetimes was laid");
    assert!(refused.contains("Missing") || refused.contains("E0106"), "CONTROL: `two_adt`'s refusal names E0106: {refused}");
    println!("front2_region_rules: a_one_name_alias_argument_is_laid_as_its_expansion: 4 heads as the rules say");
}

/// Was blaze-flow-lower sig_assoc_tests.rs (the builder and `Self::Item` reads, and
/// `a_head_naming_two_lifetimes_of_one_adt_is_read`). The Reference, "Associated items": a
/// projection on a concrete type is its impl's binding. `impl<'a> IntoFuture for Builder<'a> {
/// type IntoFuture = Pending<'a>; fn into_future(self) -> Self::IntoFuture }` is `[Param(0),
/// Param(0)]` (the impl's `'a`, both sides). `impl<'a> Iterator for It<'a> { type Item = &'a S;
/// fn get(&mut self) -> Option<Self::Item> }`, written `fn get<'s>(&'s self, ..)`, numbers the
/// impl's `'a` 0, then the method's own. "Generic parameters": an ADT has one slot per
/// lifetime parameter in declared order: `impl<'g> Given<'g> { fn checking(&self) -> Checking<'_,
/// 'g, ()> }` is `[Param(1), Param(0), Param(1), Param(0)]`.
///
/// Controls: `into_future`'s output on a fresh lifetime (the projection not expanded) is not
/// the impl's; `checking`'s two slots in the other order (`[.., Param(0), Param(1)]`) are not
/// the laid column.
#[test]
fn an_associated_type_is_laid_as_its_binding() {
    const TEXT: &str = "
use core::future::{Future, IntoFuture};
use core::pin::Pin;
use core::task::{Context, Poll};
struct S;
struct Builder<'a> { s: &'a S }
struct Pending<'a> { s: &'a S }
impl<'a> Future for Pending<'a> { type Output = (); fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> { let _ = cx; Poll::Ready(()) } }
impl<'a> IntoFuture for Builder<'a> { type Output = (); type IntoFuture = Pending<'a>; fn into_future(self) -> Self::IntoFuture { Pending { s: self.s } } }
struct Given<'g> { s: &'g S }
struct Checking<'c, 'g, T> { c: &'c Given<'g>, t: T }
impl<'g> Given<'g> { fn checking(&self) -> Checking<'_, 'g, ()> { Checking { c: self, t: () } } }
fn main() {}
";
    let got = heads(TEXT, &[Ask::Method("into_future"), Ask::Method("checking")]);
    let into = laid(&got[0], "into_future");
    assert_eq!(into.col, [P0, P0], "`into_future(self: Builder<'a>) -> Pending<'a>`");
    assert_eq!(into.late, [false], "the impl's `'a` is early-bound");
    let checking = laid(&got[1], "checking");
    assert_eq!(checking.col, [P1, P0, P1, P0], "`checking`: `Checking<'_, 'g, ()>`, `'_` is `&self`'s");
    assert_ne!(checking.col[2..], [P0, P1], "CONTROL: `Checking`'s two lifetimes laid in the other order");
    assert_eq!(checking.late, [false, true], "`'g` early (the impl's), `&self`'s elided one late");
    println!("front2_region_rules: an_associated_type_is_laid_as_its_binding: 2 heads as the rules say");
}

/// An impl's `type X = U;` value is laid in the IMPL's numbering (D148, `RegionFacts::
/// impl_assoc`, what blaze-flow-lower impl_call.rs reads for a call answered by the impl): the
/// Reference, "Generic parameters": the impl's lifetimes in declaration order are `Param(0..)`.
/// `impl<'a> Tr for Wrap<'a> { type Out = &'a S; }` gives `Out = [Param(0)]`; `impl Tk for Keep
/// { type Kept = &'static S; }` gives `Kept = ['static]`; `impl<'a, 'b> Tw<'a> for Two<'a, 'b> {
/// type Second = &'b S; }` gives `Second = [Param(1)]`. Each column reads the same through the
/// store's reader and through the impl's record encoded and decoded again (`Ask::Assoc`). "Lifetime
/// elision": no rule gives an associated type's value a lifetime, so `impl Tr for Keep { type
/// Out = &S; }` is E0106 and the impl's regions are refused by that name.
///
/// Controls: `Second` renumbered by first appearance (`'b` first) is not the laid column; the
/// E0106 impl files no column for `Out` (its regions refused, `Missing`), never a guessed one.
#[test]
fn an_impls_associated_type_value_is_laid_in_the_impls_numbering() {
    const TEXT: &str = "
struct S;
struct Wrap<'a>(&'a S);
struct Keep;
struct Two<'a, 'b>(&'a S, &'b S);
trait Tr { type Out; }
trait Tk { type Kept; }
trait Tw<'x> { type Second; }
impl<'a> Tr for Wrap<'a> { type Out = &'a S; }
impl Tk for Keep { type Kept = &'static S; }
impl<'a, 'b> Tw<'a> for Two<'a, 'b> { type Second = &'b S; }
fn main() {}
";
    let got = heads(TEXT, &[Ask::Assoc("Out"), Ask::Assoc("Kept"), Ask::Assoc("Second")]);
    let out = laid(&got[0], "Out");
    assert_eq!(out.col, [P0], "`type Out = &'a S` in `impl<'a> Tr for Wrap<'a>`: the impl's `'a`");
    assert_eq!(out.late, [false], "an impl's lifetimes are early-bound");
    let kept = laid(&got[1], "Kept");
    assert_eq!(kept.col, [ST], "`type Kept = &'static S`");
    let second = laid(&got[2], "Second");
    assert_eq!(second.col, [P1], "`type Second = &'b S` in `impl<'a, 'b> ..`: `'b` is 1 by declaration");
    assert_ne!(by_first_appearance(&second.col), second.col, "CONTROL: first-appearance numbering would lay `Second` alike");

    const E0106: &str = "struct S;\nstruct Keep;\ntrait Tr { type Out; }\nimpl Tr for Keep { type Out = &S; }\nfn main() {}\n";
    let refused = heads(E0106, &[Ask::Assoc("Out")]);
    let why = refused[0].as_ref().expect_err("CONTROL: `type Out = &S` (E0106) was laid");
    assert!(why.contains("Missing"), "`type Out = &S`: the impl's regions are refused as E0106: {why}");
    println!("front2_region_rules: an_impls_associated_type_value_is_laid_in_the_impls_numbering: 3 heads as the rules say, E0106 refused");
}
