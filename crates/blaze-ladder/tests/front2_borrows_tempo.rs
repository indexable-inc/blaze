//! BW: the move and borrow check's false findings on tempo (a project the pinned nightly
//! accepts, so every finding in its `blaze check` is false), each class reduced to the smallest
//! plain-Rust body that shows it, with a CONTROL per class: the nearest body Rust refuses, which
//! must still be found.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain` (edition 2024: Rust's std).

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::borrows::BorrowVerdict;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;

const EDITION: &str = "2024";

const MAIN: &str = "fn main() {}

fn for_slice(xs: &[u8]) -> u32 {
    let mut n = 0u32;
    for x in xs {
        n += *x as u32;
    }
    n
}

fn for_local_return(c: u8) -> Option<u8> {
    let cands = [1u8, 2u8];
    for x in &cands {
        if *x == c {
            return Some(*x);
        }
    }
    None
}

fn while_next(xs: &[u8]) -> u32 {
    let mut n = 0u32;
    let mut it = xs.iter();
    while let Some(x) = it.next() {
        n += *x as u32;
    }
    n
}

fn next_once(xs: &[u8]) -> u8 {
    let mut it = xs.iter();
    let a = it.next();
    let b = it.next();
    match (a, b) {
        (Some(x), _) => *x,
        _ => 0,
    }
}

fn reborrow_arg(out: &mut Vec<u8>) {
    out.push(1);
    out.push(out.len() as u8);
}

struct At<'a> {
    xs: &'a [u8],
}

impl<'a> core::fmt::Debug for At<'a> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let d: &dyn core::fmt::Debug = &self.xs;
        d.fmt(f)
    }
}

struct Two<'a> {
    xs: &'a [u8],
    n: u8,
}

fn debug_two<'a>(t: &Two<'a>, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    core::fmt::Formatter::debug_struct_field2_finish(f, \"Two\", \"xs\", &t.xs, \"n\", &&t.n)
}

const CS: &[u8] = &[1, 2, 3];

fn const_slice_sum() -> u8 {
    let mut n = 0u8;
    for &c in CS {
        n += c;
    }
    n
}

struct Guard {
    v: Vec<u8>,
}

impl Drop for Guard {
    fn drop(&mut self) {}
}

fn tail_temp_guard(g: Guard) -> Option<u8> {
    let state = g;
    let first = state.v.first()?;
    state.v.iter().next().map(|x| *x + *first)
}

fn once_move(s: String) -> usize {
    let f = move || s;
    f().len()
}

fn dyn_of_ref<'a>(xs: &'a [u8]) -> usize {
    let d: &dyn core::fmt::Debug = &xs;
    let _ = d;
    0
}

fn control_item_kept_across_push(mut v: Vec<u8>) -> u8 {
    let mut it = v.iter();
    let a = it.next();
    v.push(2);
    match a {
        Some(x) => *x,
        None => 0,
    }
}

fn control_store_local_through_iter_mut() -> usize {
    let one = 1u8;
    let mut v: Vec<&u8> = Vec::new();
    v.push(&one);
    {
        let local = 2u8;
        for x in v.iter_mut() {
            *x = &local;
        }
    }
    v.len()
}

fn control_store_local_through_index() -> usize {
    let one = 1u8;
    let mut v: Vec<&u8> = Vec::new();
    v.push(&one);
    {
        let local = 2u8;
        v[0] = &local;
    }
    v.len()
}

fn index_read(v: Vec<&u8>, w: &mut Vec<&u8>) -> u8 {
    let one = &1u8;
    w[0] = one;
    *v[0] + *w[0]
}

fn control_two_mut_of_iter(xs: &[u8]) -> u8 {
    let mut it = xs.iter();
    let r = &mut it;
    let a = it.next();
    let _ = r.next();
    match a {
        Some(x) => *x,
        None => 0,
    }
}

trait Buf {
    fn put(&mut self, x: u8) {
        let _ = x;
    }
}

fn enc_inner(out: &mut dyn Buf) {
    out.put(1);
}

fn enc(out: &mut dyn Buf) {
    out.put(0);
    enc_inner(out);
    out.put(2);
}

#[derive(Debug)]
struct Four<'a> {
    n: u8,
    xs: &'a [u8],
    ys: &'a [u8],
    zs: &'a [u128],
}

fn control_dyn_two_reborrows(out: &mut dyn Buf) {
    let a = &mut *out;
    let b = &mut *out;
    a.put(1);
    b.put(2);
}

fn control_dyn_reborrow_kept<'a>(out: &'a mut (dyn Buf + 'a), keep: &mut Option<&'a mut (dyn Buf + 'a)>) {
    *keep = Some(&mut *out);
    out.put(1);
}

fn control_dyn_of_local_kept(keep: &mut Vec<&'static dyn core::fmt::Debug>) {
    let local = 1u8;
    keep.push(&local);
}

fn control_dyn_inner_short<'a>(x: &'a &u8) -> &'a (dyn core::fmt::Debug + 'static) {
    x
}

fn take_once<F: FnOnce() -> (u8, String)>(f: F) -> (u8, String) {
    f()
}

fn closure_moves_capture(s: String) -> usize {
    take_once(move || (1u8, s)).1.len()
}

fn closure_moves_capture_by_inference(s: String) -> usize {
    take_once(|| (1u8, s)).1.len()
}

fn control_move_out_of_ref_in_closure(s: String) -> usize {
    let r = &s;
    take_once(move || (1u8, *r)).1.len()
}

fn guard_tail(l: &std::sync::RwLock<std::collections::BTreeMap<u64, u64>>, at: Option<u64>) -> Option<u64> {
    let state = l.read().expect(\"poisoned\");
    let applied = at?;
    state.range(..=applied).next_back().map(|(_, e)| *e)
}

fn for_array_of_slices() -> usize {
    let mut n = 0;
    for bytes in [&[2u8][..], &[2, 1, 0]] {
        n += bytes.len();
    }
    n
}

std::thread_local! {
    static RNG: core::cell::RefCell<Option<(u64, u64)>> = const { core::cell::RefCell::new(None) };
}

fn with_rng() -> u64 {
    RNG.with_borrow_mut(|rng| {
        let (_, r) = rng.get_or_insert_with(|| (1, 2));
        *r
    })
}

fn wrap_output(c: &mut Vec<u8>) -> Result<std::io::BufWriter<&mut Vec<u8>>, ()> {
    Ok(std::io::BufWriter::new(c))
}

fn write_through_wrapper(plain: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut out = Vec::new();
    let mut w = wrap_output(&mut out).unwrap();
    w.write_all(plain).unwrap();
    w.into_inner().unwrap();
    out
}

trait Recv {
    fn recv(&mut self) -> impl std::future::Future<Output = u8> + Send {
        async { 0 }
    }
}

fn chain_filter_count(v: &[(u8, u8)], k: u8) -> usize {
    v.iter().filter(|g| g.0 == k).count()
}

fn chain_map_collect(v: &[(u8, u8)]) -> Vec<u8> {
    v.iter().map(|a| a.0).collect::<Vec<_>>()
}

fn chain_find_from(seed: u32) -> Option<u32> {
    (0u32..).map(|c| c + seed).find(|w| *w > 3)
}

fn chain_generic<I: Iterator<Item = u8>>(it: &mut I) -> Option<u8> {
    it.by_ref().map(|x| x + 1).find(|x| *x > 1)
}

fn control_find_while_borrowed(seed: u32) -> Option<u32> {
    let mut m = (0u32..).map(|c| c + seed);
    let r = &mut m;
    let x = m.find(|w| *w > 3);
    r.next();
    x
}

fn control_future_keeps_borrow() -> Option<u8> {
    let mut c = Chan(Vec::new());
    let f = c.pop();
    drop(c);
    drop(f);
    None
}

struct Chan(Vec<u8>);

impl Chan {
    async fn pop(&mut self) -> Option<u8> {
        self.0.pop()
    }
}

struct Mock {
    chan: Chan,
}

impl Recv for Mock {
    async fn recv(&mut self) -> u8 {
        self.chan.pop().await.unwrap_or(0)
    }
}


fn control_drop_after_maybe_move(c: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let w = std::io::BufWriter::new(&mut out);
    if c {
        drop(w);
    }
    out
}

fn make() -> Vec<u8> {
    Vec::new()
}

fn control_autoref_of_temporary() -> &'static [u8] {
    make().as_slice()
}

fn control_dyn_lengthened<'a>(x: &'a mut (dyn Buf + 'a)) -> &'a mut (dyn Buf + 'static) {
    &mut *x
}

fn control_sync_static_from_input(x: &u8) -> &'static u8 {
    x
}

async fn control_async_static_from_input(x: &u8) -> &'static u8 {
    x
}

async fn async_elided_from_input(x: &u8) -> &u8 {
    x
}

async fn control_async_other_named<'a, 'b>(x: &'a u8, y: &'b u8) -> &'a u8 {
    let _ = x;
    y
}

async fn async_named_from_input<'a, 'b>(x: &'a u8, y: &'b u8) -> &'a u8 {
    let _ = y;
    x
}

async fn async_block_borrows_awaited_local() -> u8 {
    let v = 1u8;
    let r = async { &v }.await;
    *r
}

fn control_return_ref_to_local() -> &'static u8 {
    let v = 1u8;
    &v
}

fn return_ref_to_promoted() -> &'static u8 {
    &1
}

pub trait AsDynError<'a> {
    fn as_dyn_error(&self) -> &(dyn std::error::Error + 'a) {
        loop {}
    }
}

impl<'a, T: std::error::Error + 'a> AsDynError<'a> for T {
    fn as_dyn_error(&self) -> &(dyn std::error::Error + 'a) {
        self
    }
}

pub struct IoFault {
    source: std::io::Error,
}

fn error_source(f: &IoFault) -> Option<&(dyn std::error::Error + 'static)> {
    Some(f.source.as_dyn_error())
}

fn some_of_input(x: &u8) -> Option<&u8> {
    Some(x)
}

fn control_some_of_local() -> Option<&'static u8> {
    let v = 1u8;
    Some(&v)
}

fn control_some_static_from_input<'x>(x: &'x u8) -> Option<&'static u8> {
    Some(x)
}
";

/// Each body's expected finding codes: empty for a body Rust accepts (each a tempo class
/// reduced), else the code Rust refuses it with, which must be among the findings (a CONTROL).
const EXPECTED: &[(&str, &[&str])] = &[
    // Class A (generic_call.rs, "PROJECTIONS"): `for` and `Iterator::next` through one hub,
    // the `&mut iter` loan living as long as the iterator.
    ("for_slice", &[]),
    ("for_local_return", &[]),
    ("while_next", &[]),
    ("next_once", &[]),
    ("const_slice_sum", &[]),
    // The edition-2024 tail of a `Drop` local's borrow: the same hub (`iter().next()`).
    ("tail_temp_guard", &[]),
    ("reborrow_arg", &[]),
    ("fmt", &[]),
    ("debug_two", &[]),
    ("drop", &[]),
    ("once_move", &[]),
    ("dyn_of_ref", &[]),
    ("index_read", &[]),
    // CONTROLS: the item still holds the collection's loan (E0502); a local stored through
    // `iter_mut`'s item, or through `IndexMut`, reaches the vector's elements (E0597: both were
    // MISSED before the overloaded deref and index related their targets, lib.rs
    // `target_flow`); a second `&mut` of the iterator while the first is used (E0499).
    ("control_item_kept_across_push", &["E0502"]),
    ("control_store_local_through_iter_mut", &["E0597"]),
    ("control_store_local_through_index", &["E0597"]),
    ("control_two_mut_of_iter", &["E0499"]),
    // Class B (lib.rs `borrow`, a trait object's lifetime shortened by the borrow): calls
    // through a `&mut dyn Buf`, alloy-rlp's `encode(&self, out: &mut dyn BufMut)`.
    ("put", &[]),
    ("enc_inner", &[]),
    ("enc", &[]),
    // Class E (lib.rs `unsize_to_object`): a derived `Debug` of a struct with lifetimes
    // (tempo's invariants `At<'a>`), `&&self.xs` coerced to `&dyn Debug`.
    ("fmt", &[]),
    // CONTROLS: a `&local` coerced to a `&'static dyn Debug` kept (E0597); an inner lifetime
    // made `'static` by the object (`outlives`).
    ("control_dyn_of_local_kept", &["E0597"]),
    ("control_dyn_inner_short", &["outlives"]),
    // Classes G, H, D and F, being reduced: a closure moving its capture into an `FnOnce`
    // (vsock-proxy main.rs:210, E0507 found); a guard's tail (zones manifest.rs:471); `for`
    // over an array of slices (protocol.rs:336); a thread-local's `with_borrow_mut`
    // (test_utils.rs:32); a writer over `&mut Vec` (consensus-config tests.rs:29).
    ("take_once", &[]),
    // Class G (blaze-typeck rules/closure.rs: a tuple's operands are value expressions).
    ("closure_moves_capture", &[]),
    ("closure_moves_capture_by_inference", &[]),
    ("guard_tail", &[]),
    ("for_array_of_slices", &[]),
    ("with_rng", &[]),
    ("wrap_output", &[]),
    ("write_through_wrapper", &[]),
    // Class H (tempo zones/p2p runtime.rs:882): an `async fn` implementing a trait's
    // `-> impl Future + Send`.
    ("pop", &[]),
    ("recv", &[]),
    // Iterator adapters taking closures (trie-shapes lib.rs:268, :423, launcher.rs:424; were
    // `CallOutlivesUnread`, 157 of tempo g052341's 243 bodies not checked). Class I
    // (generic_call.rs: a projection standing for no position relates nothing): `find` on a
    // `Map<_, {closure}>` went to the hub, a false E0505.
    ("chain_filter_count", &[]),
    ("chain_map_collect", &[]),
    ("chain_find_from", &[]),
    ("chain_generic", &[]),
    // CONTROL: `find`'s `&mut self` loan is still a mutable borrow (E0499).
    ("control_find_while_borrowed", &["E0499"]),
    // CONTROL: the future an `async fn` returns still holds its input's loan at the caller
    // (E0505: `c` moved while `f` borrows it).
    ("control_future_keeps_borrow", &["E0505"]),
    // CONTROL: a move out of a shared reference a closure captured (E0507).
    ("control_move_out_of_ref_in_closure", &["E0507"]),
    // CONTROLS for F (blaze-flow loans.rs: a drop is a use where it may run) and D (lib.rs:
    // a shared autoref of a constant is promoted): a writer dropped on one path only still
    // holds `out` at the scope's end (E0505); an autoref of a call's result is not promoted
    // (refused E0515, a reference to a temporary returned: blaze-flow's finding says the return
    // place holds the loan, `returned`).
    ("make", &[]),
    ("control_drop_after_maybe_move", &["E0505"]),
    ("control_autoref_of_temporary", &["E0515"]),
    // BX case 4: a reference to a local returned is E0515 too (the stored-local controls above
    // stay E0597: their loan is held by a local, not the return place); a promoted constant's
    // reference is `'static`.
    ("control_return_ref_to_local", &["E0515"]),
    ("return_ref_to_promoted", &[]),
    // A tuple-like constructor whose field is a bare type parameter (`Some(T)`): its operand
    // goes into the argument's own positions (lib.rs `ctor_fields`), not through one hub.
    // thiserror's derived `source` (`Some(source.as_dyn_error())`, its src/aserror.rs
    // `AsDynError<'a>`) printed a false "outlives"; the trait's `'a` per call is right as it is
    // (`f.source.as_dyn_error()` returned bare was already clean). CONTROLS: a local's borrow
    // in `Some` returned (E0515), an input's lifetime made `'static` through `Some`
    // ("outlives"). The trait's fn has a body (`loop {}`) only because a bodiless trait fn is
    // listed as a body and refused `TypesNotChecked` (bx_probe2, reported).
    ("as_dyn_error", &[]),
    ("error_source", &[]),
    ("some_of_input", &[]),
    ("control_some_of_local", &["E0515"]),
    ("control_some_static_from_input", &["outlives"]),
    // BX case 1 (RFC 2394; blaze-flow-lower sig.rs `BodyOut`): an `async fn`'s body's value is
    // checked against the WRITTEN return type's regions, as a sync fn's is ("outlives", the
    // code a sync fn gets for the same error: its twin first), never the future's.
    ("control_sync_static_from_input", &["outlives"]),
    ("control_async_static_from_input", &["outlives"]),
    ("async_elided_from_input", &[]),
    ("control_async_other_named", &["outlives"]),
    ("async_named_from_input", &[]),
    ("async_block_borrows_awaited_local", &[]),
    // CONTROLS: two live reborrows of one `&mut dyn` (E0499); a reborrow kept for `'a` while
    // the object is used (E0499); the object lifetime lengthened to `'static` ("outlives").
    ("control_dyn_two_reborrows", &["E0499"]),
    ("control_dyn_reborrow_kept", &["E0499"]),
    ("control_dyn_lengthened", &["outlives"]),
];

fn shown(item: &str, v: &BorrowVerdict<'_>) -> String {
    let found: Vec<String> = v.findings.iter().map(|d| format!("{} {} at {:?}", d.code, d.text, item.get(d.start as usize..d.end as usize).unwrap_or("<out of range>"))).collect();
    format!("{found:?} refused {:?}", v.refused)
}

fn open_toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

#[test]
fn tempo_classes_and_controls() {
    let heap = blaze_own::testing::heap();
    let tc = open_toolchain();
    let files = [("/bw/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "bw", dir: "/bw/", root: "/bw/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));

    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let name_of = |b: &BodyRef<'_>| -> String {
        match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        }
    };
    let text_of = |b: BodyRef<'_>| {
        let k = match b {
            BodyRef::Free(k) => k,
            BodyRef::Member { container, .. } => container,
        };
        names.item_text(k).expect("the body's item text").text
    };
    let mut wrong: Vec<String> = Vec::new();
    let mut flows_of_wrong: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (b, _) in bodies.iter() {
        let n = name_of(b);
        if n == "main" {
            continue;
        }
        let Some(&(_, want)) = EXPECTED.iter().find(|(e, _)| *e == n) else {
            panic!("a body this test does not expect: {n}");
        };
        seen.push(n.clone());
        let mut flows = String::new();
        let (types, v) = world.check_borrows_shown(*b, &mut flows, heap.alloc());
        let item = text_of(*b);
        let ok = types.is_clean()
            && match want {
                [] => v.refused.is_none() && v.findings.is_empty(),
                codes => {
                    (v.refused.is_none() && codes.iter().all(|c| v.findings.iter().any(|d| d.code == *c)))
                        || matches!(v.refused, Some(blaze_ladder::front2::borrows::BorrowRefused::Stricter(_)))
                }
            };
        if !ok {
            // One line per wrong body first (the runner keeps a failure's first lines), then
            // each one's facts and flow, to reduce it from.
            wrong.push(format!("{n}: want {want:?}, types clean {}: {}", types.is_clean(), shown(item, &v)));
            // Its flow, statements joined into long lines: the runner keeps 30 lines of a
            // failure, each up to 1200 bytes.
            let flow = flows.find("flow of").map_or(flows.as_str(), |i| &flows[i..]);
            let mut line = format!("=== {n}:");
            let fact = |s: &&str| {
                let t = s.trim_start();
                ["object trait", "head ", "sig ", "shape ", "no shape", "bounds ", "no call bounds"].iter().any(|p| t.starts_with(p))
            };
            for s in flows.lines().filter(fact).chain(flow.lines()) {
                if line.len() + s.len() > 1100 {
                    flows_of_wrong.push(core::mem::take(&mut line));
                }
                line.push_str(" | ");
                line.push_str(s.trim());
            }
            flows_of_wrong.push(line);
        }
    }
    for (e, _) in EXPECTED {
        assert!(seen.iter().any(|s| s == e), "an expected body was not checked: {e}");
    }
    assert!(wrong.is_empty(), "{} bodies not as Rust has them:\n{}\n{}", wrong.len(), wrong.join("\n"), flows_of_wrong.join("\n"));
}

#[test]
fn onerule_tempo_buckets() {
    const SRC: &str = r#"
fn main() {}

// Bucket 1: GenericSelf(Alias)
fn for_impl_into_iter(xs: impl IntoIterator<Item = u8>) -> u8 {
    let mut s = 0;
    for x in xs {
        s += x;
    }
    s
}

fn for_impl_into_iter_ref<'a>(xs: impl IntoIterator<Item = &'a u8>) -> u8 {
    let mut s = 0;
    for x in xs {
        s += *x;
    }
    s
}

trait DeError: Sized {
    fn missing_field(name: &'static str) -> Self;
}
trait MapAcc<'de> {
    type Error: DeError;
    fn next_key(&mut self) -> Result<Option<&'de str>, Self::Error>;
}
fn visit_map<'de, A: MapAcc<'de>>(mut map: A) -> Result<u8, A::Error> {
    let _ = map.next_key()?;
    Err(<A::Error as DeError>::missing_field("x"))
}

// Bucket 2: Unanswered (Serialize-style ? on Result<S::SerStruct, S::Error>)
trait SerStruct {
    type Ok;
    type Error;
    fn ser_field(&mut self, k: &'static str, v: &u8) -> Result<(), Self::Error>;
    fn end(self) -> Result<Self::Ok, Self::Error>;
}
trait Ser {
    type Ok;
    type Error;
    type SerStruct: SerStruct<Ok = Self::Ok, Error = Self::Error>;
    fn ser_struct(self, name: &'static str, len: usize) -> Result<Self::SerStruct, Self::Error>;
}
fn ser_group<S: Ser>(s: S, x: &u8) -> Result<S::Ok, S::Error> {
    let mut st = S::ser_struct(s, "G", 1)?;
    SerStruct::ser_field(&mut st, "x", x)?;
    SerStruct::end(st)
}

// Bucket 2: Unanswered (`Deserialize<'de>` / trait with lifetime param proved for primitive `usize`)
trait De<'de>: Sized {}
impl<'de> De<'de> for usize {}
trait MapDe<'de> {
    fn next_val<V: De<'de>>(&mut self) -> Result<V, ()>;
}
fn de_primitive<'de, M: MapDe<'de>>(m: &mut M) -> Result<usize, ()> {
    m.next_val::<usize>()
}
// Negative control: `trait LtTrait<'a>` with `impl LtTrait<'static> for usize` MUST constrain `'a: 'static`!
trait LtTrait<'a> {}
impl LtTrait<'static> for usize {}
fn need_lt_trait<'a, T: LtTrait<'a>>(_: &'a u8, _: T) {}
fn control_lt_trait_primitive_dangling() {
    let local = 1u8;
    need_lt_trait(&local, 0usize);
}

// Bucket 3: HeadUnread (tracing_subscriber `FormatFields` with supertrait param bound `M: MakeOutput<Writer<'w>, Out>` where `MakeOutput<T, Out>: MakeVisitor<T>` defines `type Visitor`)
struct Writer<'w>(&'w mut u8);
trait VisitFmt {}
trait MakeVisitor<T> {
    type Visitor;
}
trait MakeOut<T, Out>: MakeVisitor<T> {}
struct DefVis;
impl VisitFmt for DefVis {}
struct DefFields;
impl<'w> MakeVisitor<Writer<'w>> for DefFields {
    type Visitor = DefVis;
}
impl<'w> MakeOut<Writer<'w>, ()> for DefFields {}
trait LookupSpan<'a> {}
trait Sub {}
trait FmtFields<'w> {}
trait FmtLayer<S> {}
trait SubExt: Sub + Sized {
    fn with<L: FmtLayer<Self>>(self, layer: L) -> Layered<L, Self> {
        Layered(layer, self)
    }
}
struct Reg;
impl Sub for Reg {}
impl<'a> LookupSpan<'a> for Reg {}
impl<S: Sub> SubExt for S {}
struct Layered<L, S>(L, S);
struct Fmt<S, N>(core::marker::PhantomData<fn(S, N)>);
impl<'w, M> FmtFields<'w> for M
where
    M: MakeOut<Writer<'w>, ()>,
    M::Visitor: VisitFmt,
{}
impl<S, N> FmtLayer<S> for Fmt<S, N>
where
    S: Sub + for<'a> LookupSpan<'a>,
    N: for<'w> FmtFields<'w> + 'static,
{}
fn test_traced_repro() {
    let _ = Reg.with(Fmt::<_, DefFields>(core::marker::PhantomData));
}

// DefaultArg with parameter-substituted default (e.g. `once_cell::sync::Lazy<T, F = fn() -> T>`)
struct LazyCell<T, F = fn() -> T> {
    val: Option<T>,
    init: F,
}
impl<T, F> LazyCell<T, F> {
    fn get_ref(&self) -> Option<&T> {
        self.val.as_ref()
    }
}
fn use_lazy_static<'a>(cell: &'a LazyCell<&'static str>) -> Option<&'a &'static str> {
    cell.get_ref()
}
fn use_lazy_local<'a, 'b>(cell: &'a LazyCell<&'b str>) -> Option<&'a &'b str> {
    cell.get_ref()
}

// Negative soundness controls (must report borrow errors, never silently accept!)
fn control_rpitit_concrete_dangling(r: &RpititImpl) -> impl core::future::Future<Output = u8> + '_ {
    let local = 1u8;
    r.fetch(&local)
}
fn control_rpitit_default_dangling(r: &RpititDefImpl) -> impl core::future::Future<Output = u8> + '_ {
    let local = 1u8;
    r.get(&local)
}
fn control_async_fn_dangling() -> impl core::future::Future<Output = u8> {
    let local = 1u8;
    take_fut(wait_val(&local))
}
fn control_lazy_dangling<'a>() -> Option<&'a str> {
    let local = String::new();
    let cell: LazyCell<&str> = LazyCell { val: Some(local.as_str()), init: || "" };
    cell.val
}

// Bucket 4: Shape { Heads { Opaque, Opaque } } (RPITIT concrete & default)
trait RpititTr {
    async fn fetch(&self, x: &u8) -> u8;
}
struct RpititImpl;
impl RpititTr for RpititImpl {
    async fn fetch(&self, x: &u8) -> u8 {
        *x
    }
}
async fn call_rpitit_concrete(r: &RpititImpl, x: &u8) -> u8 {
    r.fetch(x).await
}

trait RpititDefault {
    async fn get(&self, x: &u8) -> u8 {
        *x
    }
}
struct RpititDefImpl;
impl RpititDefault for RpititDefImpl {}
async fn call_rpitit_default(r: &RpititDefImpl, x: &u8) -> u8 {
    r.get(x).await
}

// Bucket 5: GenericSelf(Opaque) (async fn opaque passed to FutureExt / Future bound)
trait FutExt: core::future::Future {
    fn poll_now(self: core::pin::Pin<&mut Self>) -> Option<Self::Output> {
        None
    }
}
impl<F: core::future::Future + ?Sized> FutExt for F {}
async fn wait_val(x: &u8) -> u8 {
    *x
}
fn call_future_ext_on_async_fn(x: &u8) -> Option<u8> {
    let mut f = core::pin::pin!(wait_val(x));
    f.as_mut().poll_now()
}
fn take_fut<F: core::future::Future<Output = u8>>(f: F) -> F {
    f
}
fn pass_async_fn_to_fut_bound(x: &u8) -> impl core::future::Future<Output = u8> + '_ {
    take_fut(wait_val(x))
}

// --- Final 36 tempo borrows classes (A..G) + negative controls ---

// Class A: Higher-ranked `dyn for<'a> Fn` in invariant `&mut` across multiple calls
struct Decoded<'a>(&'a [u8]);
type DecoderReg = Vec<Box<dyn for<'a> Fn(&'a [u8]) -> Option<Decoded<'a>> + Send + Sync>>;
fn add_reg(_: &mut DecoderReg) {}
fn dyn_hr_mut_multiple_calls() -> DecoderReg {
    let mut r: DecoderReg = Vec::new();
    add_reg(&mut r);
    add_reg(&mut r);
    add_reg(&mut r);
    r
}
fn take_hr_box(_: &mut Box<dyn for<'a> Fn(&'a [u8]) -> Option<Decoded<'a>> + Send + Sync>) {}
fn control_dyn_universal_passed_to_hr<'x>(r: &mut Box<dyn Fn(&'x [u8]) -> Option<Decoded<'x>> + Send + Sync>) {
    take_hr_box(r);
}
fn take_hr_two(_: &mut Box<dyn for<'a, 'b> Fn(&'a u8, &'b u8) -> &'a u8 + Send + Sync>) {}
fn take_hr_one(_: &mut Box<dyn for<'a> Fn(&'a u8, &'a u8) -> &'a u8 + Send + Sync>) {}
fn control_dyn_mismatch_hr_binders(mut b: Box<dyn for<'a, 'b> Fn(&'a u8, &'b u8) -> &'a u8 + Send + Sync>) {
    take_hr_two(&mut b);
    take_hr_one(&mut b);
}

// Class B: Structural drop glue with non-dropping borrowed fields (`RoutingPolicy`, `DecodedTempoPrecompileError`, `DbStorageOverlay`)
struct Policy<'a> {
    r: &'a u8,
    owned: Vec<u8>,
}
struct Actor {
    x: u8,
    y: u8,
}
impl Actor {
    fn policy(&self) -> Policy<'_> {
        Policy { r: &self.x, owned: Vec::new() }
    }
    fn drop_glue_unrelated_lifetime(&mut self) -> u8 {
        let p = self.policy();
        let v = *p.r;
        self.y = v;
        self.y
    }
}
struct DecErr<'a> {
    msg: String,
    bytes: &'a [u8],
}
fn try_dec<'a>(b: &'a [u8]) -> Option<DecErr<'a>> {
    if b.is_empty() { None } else { Some(DecErr { msg: String::new(), bytes: b }) }
}
fn drop_glue_match_none_move(v: Vec<u8>) -> usize {
    match try_dec(&v) {
        Some(e) => e.msg.len() + e.bytes.len(),
        None => {
            let n = v.len();
            drop(v);
            n
        }
    }
}
struct GuardDrop<'a>(&'a u8);
impl<'a> Drop for GuardDrop<'a> {
    fn drop(&mut self) {
        let _ = *self.0;
    }
}
struct WrapGuard<'a> {
    g: GuardDrop<'a>,
    owned: Vec<u8>,
}
fn control_drop_impl_borrows_lifetime(mut x: u8) -> u8 {
    let w = WrapGuard { g: GuardDrop(&x), owned: Vec::new() };
    let v = *w.g.0;
    x = v + 1;
    x
}
struct DynHolder<'a> {
    d: Box<dyn core::fmt::Debug + 'a>,
    owned: Vec<u8>,
}
fn control_drop_box_dyn_borrows_lifetime(mut x: u8) -> u8 {
    let h = DynHolder { d: Box::new(&x), owned: Vec::new() };
    let _ = h.owned.len();
    x = 2;
    x
}

// Class C: `Copy` impl with `RegionOutlives` bound already on ADT (`ContentRefDeserializer`)
struct RefDe<'a, 'de: 'a, E> {
    data: &'a &'de [u8],
    _e: core::marker::PhantomData<E>,
}
impl<'a, 'de: 'a, E> Copy for RefDe<'a, 'de, E> {}
impl<'a, 'de: 'a, E> Clone for RefDe<'a, 'de, E> {
    fn clone(&self) -> Self { *self }
}
fn copy_with_region_outlives<'a, 'de: 'a, E>(d: RefDe<'a, 'de, E>) -> (RefDe<'a, 'de, E>, RefDe<'a, 'de, E>) {
    (d, d)
}
struct NoOutlives<'a, 'b, E>(&'a u8, &'b u8, core::marker::PhantomData<E>);
impl<'a, 'b: 'a, E: Copy> Copy for NoOutlives<'a, 'b, E> {}
impl<'a, 'b: 'a, E: Copy> Clone for NoOutlives<'a, 'b, E> {
    fn clone(&self) -> Self { *self }
}
fn control_copy_not_copy_param<'a, 'b: 'a>(d: NoOutlives<'a, 'b, String>) -> (NoOutlives<'a, 'b, String>, NoOutlives<'a, 'b, String>) {
    (d, d)
}

// Class D: Constant promotion of array repeat expressions (`&[0; 64]`, `&[(1, [[0x11; 4]; 2])]`)
fn take_static_slice(s: &'static [u8]) -> usize { s.len() }
fn const_promote_repeat_slice() -> usize {
    take_static_slice(&[0u8; 64])
}
fn const_promote_nested_repeat() -> &'static [(u8, [[u8; 4]; 2])] {
    &[(1, [[0x11; 4]; 2]), (2, [[0x22; 4]; 2])]
}
fn control_non_const_repeat_not_promoted(x: u8) -> &'static [u8] {
    &[x; 4]
}

// Class E: Associated type `Copy` bound (`M::At<'_>` where `type At<'a>: Copy`)
trait Obs {
    type At<'a>: Copy;
    fn read(&self, at: Self::At<'_>) -> u8;
}
fn use_assoc_copy<M: Obs>(m: &M, at: M::At<'_>) -> u8 {
    m.read(at) + m.read(at)
}
trait ObsNoCopy {
    type At<'a>;
    fn read(&self, at: Self::At<'_>) -> u8;
}
fn control_assoc_type_not_copy<M: ObsNoCopy>(m: &M, at: M::At<'_>) -> u8 {
    m.read(at) + m.read(at)
}

// Class F: Disjoint field capture of enum matched by value inside `async` block
struct InnerBlock {
    body: String,
    witness: Option<String>,
}
struct OuterPeer {
    block: InnerBlock,
}
async fn take_str(s: String) -> usize { s.len() }
async fn async_disjoint_field_match(peer: OuterPeer) -> usize {
    let b = peer.block.body;
    let n = async {
        match peer.block.witness {
            Some(w) => take_str(w).await,
            None => 0,
        }
    }.await;
    b.len() + n
}
async fn control_async_same_field_moved_twice(peer: OuterPeer) -> usize {
    let w0 = peer.block.witness;
    let n = async {
        match peer.block.witness {
            Some(w) => take_str(w).await,
            None => 0,
        }
    }.await;
    w0.map_or(0, |s| s.len()) + n
}

// Class G: Nested RPIT / RPITIT opaque in `Result<impl Trait, E>`
trait EngineCfg {
    type Err;
    fn tx_iter(&self, p: &u8) -> Result<impl Iterator<Item = u8>, Self::Err>;
}
struct InnerCfg;
impl EngineCfg for InnerCfg {
    type Err = ();
    fn tx_iter(&self, p: &u8) -> Result<impl Iterator<Item = u8>, Self::Err> {
        Ok(core::iter::once(*p))
    }
}
struct OuterCfg {
    inner: InnerCfg,
}
impl EngineCfg for OuterCfg {
    type Err = ();
    fn tx_iter(&self, p: &u8) -> Result<impl Iterator<Item = u8>, Self::Err> {
        self.inner.tx_iter(p)
    }
}
fn control_nested_rpit_dangling(p: &u8) -> Result<impl Iterator<Item = &u8> + '_, ()> {
    let local = *p;
    Ok(core::iter::once(&local))
}
fn control_nested_rpit_uncaptured<'a, 'b>(_a: &'a u8, b: &'b u8) -> Result<impl Iterator<Item = u8> + use<'a>, ()> {
    Ok(core::iter::once(b).copied())
}

// Class H: Trait-lifetime & chained GAT projections (`type Token<'a> = <Self::Params<'a> as SolTy>::Token<'a>`)
trait Vis<'de> {
    type Val;
    fn visit(&self, data: &'de [u8]) -> Self::Val;
}
struct SliceVis;
impl<'de> Vis<'de> for SliceVis {
    type Val = &'de [u8];
    fn visit(&self, data: &'de [u8]) -> Self::Val { data }
}
fn use_trait_lt_assoc<'a>(v: &SliceVis, data: &'a [u8]) -> &'a [u8] {
    v.visit(data)
}
fn control_trait_lt_assoc_dangling(v: &SliceVis) -> &'static [u8] {
    let local = [1u8, 2];
    v.visit(&local)
}

trait SolTy {
    type Tok<'a>;
}
struct SolU8;
impl SolU8 {
    fn tok(x: &u8) -> &u8 { x }
}
impl SolTy for SolU8 {
    type Tok<'a> = &'a u8;
}
impl<A: SolTy, B: SolTy> SolTy for (A, B) {
    type Tok<'a> = (A::Tok<'a>, B::Tok<'a>);
}
trait SolFn {
    type Params<'a>: SolTy;
    type Tok<'a>;
    fn tokenize(&self) -> Self::Tok<'_>;
}
struct PairCall {
    a: u8,
    b: u8,
}
impl SolFn for PairCall {
    type Params<'a> = (SolU8, SolU8);
    type Tok<'a> = <Self::Params<'a> as SolTy>::Tok<'a>;
    fn tokenize(&self) -> Self::Tok<'_> {
        (SolU8::tok(&self.a), SolU8::tok(&self.b))
    }
}
fn use_chained_gat(c: &PairCall) -> (&u8, &u8) {
    c.tokenize()
}
fn control_chained_gat_dangling() -> &'static u8 {
    let local = PairCall { a: 1, b: 2 };
    local.tokenize().0
}

// Class I: Value callables (`static` fn-ptr slice indexing, tuple-field `Box<dyn FnOnce>`, local from call)
fn shim_first(xs: &[u8]) -> u8 { xs.first().copied().unwrap_or(0) }
static SHIMS: &[fn(&[u8]) -> u8] = &[shim_first];
fn call_static_fn_shim(idx: usize, data: &[u8]) -> u8 {
    SHIMS[idx](data)
}
fn shim_need_static(x: &'static u8) -> u8 { *x }
static STATIC_SHIMS: &[fn(&'static u8) -> u8] = &[shim_need_static];
fn control_static_fn_shim_non_static(idx: usize) -> u8 {
    let local = 7u8;
    STATIC_SHIMS[idx](&local)
}
struct Deferred(Box<dyn FnOnce(&u8) -> u8>);
impl Deferred {
    fn resolve(self, x: &u8) -> u8 {
        (self.0)(x)
    }
}
struct DeferredStatic(Box<dyn FnOnce(&'static u8) -> u8>);
impl DeferredStatic {
    fn control_tuple_field_fn_non_static(self) -> u8 {
        let local = 9u8;
        (self.0)(&local)
    }
}
fn make_hook() -> Box<dyn Fn(&u8) -> u8> {
    Box::new(|x| *x)
}
fn call_local_returned_dyn_fn_in_closure(x: &u8) -> u8 {
    let hook = make_hook();
    let run = move |y: &u8| hook(y);
    run(x)
}
fn make_static_hook() -> Box<dyn Fn(&'static u8) -> u8> {
    Box::new(|x| *x)
}
fn control_local_returned_dyn_fn_non_static() -> u8 {
    let hook = make_static_hook();
    let local = 3u8;
    hook(&local)
}

// Class J: Builtin `<async { .. } as Future>::Output` with non-zero lifetime slots & `IntoFuture`
fn drive_future<F: core::future::IntoFuture>(f: F) -> F::Output {
    let _ = f;
    loop {}
}
fn async_output_with_lifetime_slot() -> Result<(), Box<dyn core::error::Error>> {
    drive_future(async { Ok(()) })
}
fn async_output_borrows_outer(x: &u8) -> &u8 {
    drive_future(async move { x })
}
fn control_async_output_dangling() -> &'static u8 {
    let local = 42u8;
    drive_future(async { &local })
}

// Class K: Generic RPITIT trait method (`fn req<R, F>(&self, f: F) -> impl Future<Output = Option<R>>`)
trait ReqExt<T> {
    fn req<R, F>(&self, f: F) -> impl core::future::Future<Output = Option<R>>
    where
        F: FnOnce(T) -> Option<R>;
}
struct Chan<T>(T);
impl<T: Copy> ReqExt<T> for Chan<T> {
    async fn req<R, F>(&self, f: F) -> Option<R>
    where
        F: FnOnce(T) -> Option<R>,
    {
        f(self.0)
    }
}
async fn use_generic_rpitit(ch: &Chan<u8>) -> Option<u8> {
    ch.req(|v| Some(v + 1)).await
}
async fn control_generic_rpitit_dangling(ch: &Chan<u8>) -> Option<&'static u8> {
    let local = 5u8;
    ch.req(|_| Some(&local)).await
}

// Class L: Subtrait elaboration on associated types (`trait Outer: Base<Item: Helper>`)
trait Helper {
    fn help<'a>(&self, x: &'a u8) -> &'a u8;
}
trait Base {
    type Item;
}
trait Outer: Base<Item: Helper> {
    fn item(&self) -> &Self::Item;
}
fn use_assoc_subtrait<'a, T: Outer>(o: &T, x: &'a u8) -> &'a u8 {
    o.item().help(x)
}
fn control_assoc_subtrait_dangling<T: Outer>(o: &T) -> &'static u8 {
    let local = 11u8;
    o.item().help(&local)
}

// Class M: GenericSelf(Param) — `for value in iter` where `iter: I::IntoIter` (`deque::extend`, `csr::group_into`)
fn extend_into_iter<T, I: IntoIterator<Item = T>>(iter: I, out: &mut Vec<T>) {
    let iter = iter.into_iter();
    for value in iter {
        out.push(value);
    }
}
fn extend_into_iter_ref<'a, T: Copy + 'a, I: IntoIterator<Item = &'a T>>(iter: I, out: &mut Vec<T>) {
    let iter = iter.into_iter();
    for &value in iter {
        out.push(value);
    }
}
fn control_extend_into_iter_dangling<'a, I: IntoIterator<Item = &'a u8>>(iter: I) -> &'static u8 {
    let iter = iter.into_iter();
    for x in iter {
        return x;
    }
    &0
}

// Class N: GenericSelf(Other) — `TempoTxEnvelope` ground where-clauses on concrete types
struct SignedTx<T>(T);
impl<T: PartialEq> PartialEq for SignedTx<T> {
    fn eq(&self, other: &Self) -> bool { self.0 == other.0 }
}
#[derive(PartialEq)]
struct LegacyTx(u64);
enum EnvTx { Legacy(SignedTx<LegacyTx>) }
impl PartialEq for EnvTx
where
    SignedTx<LegacyTx>: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Legacy(a), Self::Legacy(b)) => a.eq(b),
        }
    }
}
trait Enc2718 {
    fn enc_len(&self) -> usize;
    fn enc(&self, out: &mut Vec<u8>);
    fn net_len(&self) -> usize { self.enc_len() + 1 }
}
impl<T> Enc2718 for SignedTx<T> {
    fn enc_len(&self) -> usize { 1 }
    fn enc(&self, out: &mut Vec<u8>) { out.push(1); }
}
impl Enc2718 for EnvTx
where
    SignedTx<LegacyTx>: Enc2718,
{
    fn enc_len(&self) -> usize { match self { Self::Legacy(tx) => tx.enc_len() } }
    fn enc(&self, out: &mut Vec<u8>) { match self { Self::Legacy(tx) => tx.enc(out) } }
}
trait MyEnc {
    fn length(&self) -> usize;
}
impl MyEnc for EnvTx
where
    SignedTx<LegacyTx>: Enc2718,
{
    fn length(&self) -> usize {
        <Self as Enc2718>::net_len(self)
    }
}
trait MyDec: Sized {
    fn decode_from<'a>(buf: &mut &'a [u8]) -> Result<Self, ()>;
}
impl<T> MyDec for SignedTx<T> where T: Default {
    fn decode_from<'a>(_buf: &mut &'a [u8]) -> Result<Self, ()> { Ok(SignedTx(T::default())) }
}
impl Default for LegacyTx { fn default() -> Self { LegacyTx(0) } }
impl MyDec for EnvTx
where
    SignedTx<LegacyTx>: MyDec,
{
    fn decode_from<'a>(buf: &mut &'a [u8]) -> Result<Self, ()> {
        Ok(Self::Legacy(SignedTx::<LegacyTx>::decode_from(buf)?))
    }
}

// Class O: GenericSelf(Other) — `blaze-format-core::Slice` derive on struct with associated type field + PhantomData<fn() -> T>
trait MyOffset: Copy + Eq + core::fmt::Debug {
    type Len: Copy + Eq + core::fmt::Debug;
}
impl MyOffset for i32 { type Len = u32; }
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct MySlice<T, O: MyOffset = i32> {
    offset: O,
    len: O::Len,
    of: core::marker::PhantomData<fn() -> T>,
}

// Class P: GenericSelf(Other) — `TempoTxEnvelope` serde/arbitrary higher-ranked ground where-clauses
trait MyDe<'de>: Sized {
    fn de(src: &'de [u8]) -> Result<Self, ()>;
}
trait MyDeOwned: for<'de> MyDe<'de> {}
impl<T: for<'de> MyDe<'de>> MyDeOwned for T {}
impl<'de, T: MyDe<'de>> MyDe<'de> for SignedTx<T> {
    fn de(src: &'de [u8]) -> Result<Self, ()> { Ok(SignedTx(T::de(src)?)) }
}
impl<'de> MyDe<'de> for LegacyTx {
    fn de(_src: &'de [u8]) -> Result<Self, ()> { Ok(LegacyTx(0)) }
}
fn de_via_bound<'de, V: MyDe<'de>>(src: &'de [u8]) -> Result<V, ()> {
    V::de(src)
}
impl<'de> MyDe<'de> for u32 {
    fn de(_src: &'de [u8]) -> Result<Self, ()> { Ok(0) }
}
impl<'de> MyDe<'de> for EnvTx
where
    SignedTx<LegacyTx>: MyDeOwned,
{
    fn de(src: &'de [u8]) -> Result<Self, ()> {
        let _ = de_via_bound::<u32>(src)?;
        Ok(Self::Legacy(de_via_bound::<SignedTx<LegacyTx>>(src)?))
    }
}
trait MyArb<'a>: Sized {
    fn arb(u: &'a [u8]) -> Result<Self, ()>;
}
impl<'a, T: MyArb<'a>> MyArb<'a> for SignedTx<T> {
    fn arb(u: &'a [u8]) -> Result<Self, ()> { Ok(SignedTx(T::arb(u)?)) }
}
impl<'a> MyArb<'a> for LegacyTx {
    fn arb(_u: &'a [u8]) -> Result<Self, ()> { Ok(LegacyTx(0)) }
}
fn arb_via_bound<'a, V: MyArb<'a>>(u: &'a [u8]) -> Result<V, ()> {
    V::arb(u)
}
impl<'a> MyArb<'a> for EnvTx
where
    SignedTx<LegacyTx>: for<'b> MyArb<'b>,
{
    fn arb(u: &'a [u8]) -> Result<Self, ()> {
        Ok(Self::Legacy(arb_via_bound::<SignedTx<LegacyTx>>(u)?))
    }
}
// Negative soundness control for Class P: a ground supertrait clause on an unrelated type MUST
// NOT shadow an impl that enforces `'static` on a different concrete type.
trait NeedStaticDe<'de>: Sized {}
impl NeedStaticDe<'static> for u16 {}
fn require_static_de<'de, T: NeedStaticDe<'de>>(_: &'de u8, _: T) {}
trait StaticDeSub: for<'de> NeedStaticDe<'de> {}
struct OtherDe;
impl<'de> NeedStaticDe<'de> for OtherDe {}
impl StaticDeSub for OtherDe {}
fn control_ground_supertrait_other_type_dangling()
where
    OtherDe: StaticDeSub,
{
    let local = 7u8;
    require_static_de(&local, 0u16);
}

// Class Q: Reflexive projection equality `P: WriterIn<Buffer = B>` instantiated at `B = P::Buffer`
// (`blaze-format-core::Writer::new` calling `Writer::with_buffer`)
trait MyWriterIn: Sized {
    type Buffer: MyWriteBuf<In = Self>;
    fn make_buf(self) -> Self::Buffer;
}
trait MyWriteBuf: Sized {
    type In: MyWriterIn<Buffer = Self>;
}
struct MyWriter<P: MyWriterIn> {
    buf: P::Buffer,
}
impl<P: MyWriterIn> MyWriter<P> {
    fn new_writer(p: P) -> MyWriter<P> {
        MyWriter::with_buf(p.make_buf())
    }
    fn with_buf<B>(buf: B) -> MyWriter<P>
    where
        B: MyWriteBuf<In = P>,
        P: MyWriterIn<Buffer = B>,
    {
        MyWriter { buf }
    }
}
// Negative control for reflexive projection equality: mismatched trait lifetime `'a` vs `'static`
trait LtProj<'a> {
    type Out;
    fn make(&self, x: &'a u8) -> Self::Out;
}
fn need_static_proj<P, B>(_: B)
where
    P: LtProj<'static, Out = B>,
{}
fn control_reflexive_proj_lifetime_mismatch<'a, P: LtProj<'a>>(p: &P, x: &'a u8) {
    let local = 1u8;
    let _ = x;
    need_static_proj::<P, _>(p.make(&local));
}

// Class R: tip20/checks.rs:48 check! macro with `over:` and `judge:`
type Emit<'a> = &'a mut dyn FnMut(&dyn core::fmt::Display, Option<u8>);
struct TipCheck<C: ?Sized> {
    id: &'static str,
    run: fn(&C, Emit<'_>),
}
struct TipLedger {
    token: u8,
}
impl core::fmt::Display for TipLedger {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { Ok(()) }
}
struct TipStep<M> {
    pre: M,
    post: M,
    logs: Vec<u8>,
}
impl<M> TipStep<M> {
    fn logs_from(&self, _: u8) -> impl Iterator<Item = &u8> {
        self.logs.iter()
    }
}
fn tip_each<C: ?Sized, S: core::fmt::Display>(
    ctx: &C,
    subjects: impl IntoIterator<Item = S>,
    judge: impl Fn(&C, &S) -> Option<u8>,
    emit: Emit<'_>,
) {
    for subject in subjects {
        let verdict = judge(ctx, &subject);
        emit(&subject, verdict);
    }
}
fn tip_step_checks() -> TipCheck<TipStep<TipLedger>> {
    TipCheck::<TipStep<TipLedger>> {
        id: "TRANSFER_EVENTS",
        run: |step: &TipStep<TipLedger>, emit| {
            tip_each(
                step,
                [&step.post],
                |s, _| {
                    let transfers: Vec<u8> = s.logs_from(s.post.token).filter_map(|log| Some(*log)).collect();
                    let _ = transfers.len();
                    Some(0u8)
                },
                emit,
            )
        },
    }
}
fn need_static_ledger(_: &'static TipLedger) {}
fn control_tip_step_escapes_to_static() -> TipCheck<TipStep<TipLedger>> {
    TipCheck::<TipStep<TipLedger>> {
        id: "BAD_STATIC",
        run: |step: &TipStep<TipLedger>, _emit| {
            need_static_ledger(&step.post);
        },
    }
}
fn need_static_emit(_: Emit<'static>) {}
fn control_tip_step_escapes_emit_to_static() -> TipCheck<TipStep<TipLedger>> {
    TipCheck::<TipStep<TipLedger>> {
        id: "BAD_EMIT_STATIC",
        run: |_step: &TipStep<TipLedger>, emit| {
            need_static_emit(emit);
        },
    }
}

// Class S: RFC 3407 `become` explicit tail calls (valid passthrough vs borrowing a local)
fn become_sink(x: &u8) -> u8 { *x }
fn become_valid_passthrough(x: &u8) -> u8 {
    let _local = 9u8;
    become become_sink(x);
}
fn control_become_borrows_local() -> u8 {
    let local = 9u8;
    become become_sink(&local);
}

// Class T: `<&Client as ChainSpecProvider>::ChainSpec` normalizes through `impl<T> ... for &T`
// to `<Client as ChainSpecProvider>::ChainSpec` (same interned `PTy`, different `EnvId`;
// `tempo_transaction_pool::amm::AmmLiquidityCache::repopulate`).
trait RepopHardforks {}
trait RepopEthChainSpec { type Header; }
trait RepopChainSpecProvider { type ChainSpec; }
impl<T: RepopChainSpecProvider + ?Sized> RepopChainSpecProvider for &T {
    type ChainSpec = T::ChainSpec;
}
trait RepopHeaderProvider { type Header; }
struct RepopHeader;
fn repop_on_new_blocks<P>(_client: P)
where
    P: RepopChainSpecProvider<ChainSpec: RepopHardforks>,
{}
fn repopulate_via_ref_chain_spec<Client>(client: &Client)
where
    Client: RepopHeaderProvider<Header = RepopHeader>
        + RepopChainSpecProvider<ChainSpec: RepopEthChainSpec<Header = RepopHeader> + RepopHardforks>,
{
    repop_on_new_blocks(client)
}
"#;
    let heap = blaze_own::testing::heap();
    let tc = open_toolchain();
    let files = [("/or/src/main.rs".to_string(), SRC.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate { name: "or", dir: "/or/", root: "/or/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: SRC }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, heap.alloc()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(heap.alloc());
    let mut report = blaze_own::Vec::new_in(heap.alloc());
    for (b, _) in bodies.iter() {
        let name = match b {
            BodyRef::Free(k) => names.item_text(*k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        };
        if name == "main" {
            continue;
        }
        let mut flows = "".to_owned();
        let (types, v) = world.check_borrows_shown(*b, &mut flows, heap.alloc());
        if name.starts_with("control_") {
            if !types.is_clean() || v.refused.is_some() || v.findings.is_empty() {
                report.push(format!("{name} (control): types_clean={} findings={:?} refused={:?}", types.is_clean(), v.findings, v.refused));
            }
        } else if !types.is_clean() || v.refused.is_some() || !v.findings.is_empty() {
            report.push(format!("{name}: types_clean={} findings={:?} refused={:?}", types.is_clean(), v.findings, v.refused));
        }
    }
    assert!(report.is_empty(), "Bucket repros:\n{}", report.join("\n"));
}
