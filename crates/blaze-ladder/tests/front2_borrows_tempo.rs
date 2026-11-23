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

#[test]
fn tempo_classes_and_controls() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
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
