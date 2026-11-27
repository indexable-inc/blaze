//! End-to-end checked complexity cost contracts (`#[blaze::cost(...)]`) through front2 with the
//! real core library, lowering, head records, and `blaze-typeck` (`rules/cost.rs`).
//!
//! Covers:
//! - Time contracts: `O(1)`, `amortized O(1)`, `O(log n)`, `O(n)`, `O(n log n)`, `O(n^2)`
//! - Space contracts: `space = O(1)`, `space = O(n)`, defaulting `space` to `time`
//! - Sync contracts: `lock_free` (`sync = local`), `atomic` (`sync = atomic`), `locks` (`sync = lock`)
//! - Loop marks: `#[blaze::cost(bounded)]`, `#[blaze::cost(amortized)]`, `#[blaze::cost(reset)]`
//! - `trusted = "..."` escape hatch (skips own body; callers still see declared contract)
//! - Direct recursion, closures inside loops, `for`/`while`/`loop`, inherent methods
//! - 100% opt-in behavior (unannotated fns produce zero findings)
//! - Malformed contracts & malformed loop marks (Rule 3: never silently ignored)
//! - `HeadRecord` (`K_FN`, version 42) encode/decode round-trip of `FnHead::cost`
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as `tests/front2_args_inline.rs`.

#![feature(allocator_api)]

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::head_record::{HeadAt, HeadRef, decode, encode, from_store};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::refusal::SourceFile;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::{CostClass, CostContract, SyncCost};

const EDITION: &str = "blaze-2026.11";

const TEXT: &str = r#"
#[blaze::cost(O(1))]
pub fn fast_o1(x: u32) -> u32 { x + 1 }

#[blaze::cost(amortized O(1))]
pub fn push_amort(x: u32) -> u32 { x + 1 }

#[blaze::cost(O(log n), trusted = "binary search halves range each step")]
pub fn bsearch(mut n: u32) -> u32 {
    while n > 1 {
        n /= 2;
    }
    n
}

#[blaze::cost(O(n))]
pub fn scan_for(xs: &[u32], needle: u32) -> bool {
    for &x in xs {
        if x == needle {
            return true;
        }
    }
    false
}

#[blaze::cost(time = O(n), space = O(1))]
pub fn scan_const_space(xs: &[u32]) -> u32 {
    let mut sum = 0u32;
    for &x in xs {
        sum = sum.wrapping_add(x);
    }
    sum
}

#[blaze::cost(O(1), space = O(n))]
pub fn alloc_scratch(n: u32) -> u32 { n }

#[blaze::cost(O(1), lock_free)]
pub fn local_op(x: u32) -> u32 { x }

#[blaze::cost(O(1), atomic)]
pub fn atomic_op(x: u32) -> u32 { x }

#[blaze::cost(O(1), locks)]
pub fn locked_op(x: u32) -> u32 { x }

pub struct Bag;
impl Bag {
    #[blaze::cost(O(n))]
    pub fn linear_method(&self, xs: &[u32]) -> u32 {
        let mut s = 0u32;
        for &x in xs {
            s = s.wrapping_add(fast_o1(x));
        }
        s
    }
}

// --- Clean (must produce zero findings) ---

#[blaze::cost(O(1))]
pub fn ok_o1_calls_o1(x: u32) -> u32 { fast_o1(x) }

#[blaze::cost(O(1))]
pub fn ok_o1_bounded_loop(x: u32) -> u32 {
    let mut acc = x;
    #[blaze::cost(bounded = "fixed 4 lanes")]
    for i in 0..4u32 {
        acc = fast_o1(acc + i);
    }
    acc
}

#[blaze::cost(O(n))]
pub fn ok_on_single_loop_amort(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(push_amort(x));
    }
    acc
}

#[blaze::cost(O(n log n))]
pub fn ok_nlogn_calls_bsearch(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(bsearch(x));
    }
    acc
}

#[blaze::cost(O(n))]
pub fn ok_on_amortized_inner(xs: &[u32]) -> u32 {
    let mut i = 0usize;
    let mut sum = 0u32;
    for &x in xs {
        #[blaze::cost(amortized = "two-pointer monotone advance")]
        while i < xs.len() {
            sum = sum.wrapping_add(x);
            i += 1;
        }
    }
    sum
}

#[blaze::cost(O(n), space = O(1))]
pub fn ok_const_space_caller(xs: &[u32]) -> u32 {
    scan_const_space(xs)
}

#[blaze::cost(O(n))]
pub fn ok_reset_loop_scratch(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    #[blaze::cost(reset = "frame reset per item")]
    for &x in xs {
        acc = acc.wrapping_add(alloc_scratch(x));
    }
    acc
}

#[blaze::cost(O(n), lock_free)]
pub fn ok_lock_free_calls_local(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(local_op(x));
    }
    acc
}

#[blaze::cost(O(n))]
pub fn ok_lock_outside_loop(x: u32) -> u32 {
    locked_op(x)
}

#[blaze::cost(O(n), locks)]
pub fn ok_locks_declared_in_loop(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(locked_op(x));
    }
    acc
}

pub fn ok_unannotated_nested(xs: &[u32]) -> bool {
    for &x in xs {
        for &y in xs {
            if scan_for(xs, x + y) {
                return true;
            }
        }
    }
    false
}

// --- Violations (must produce cost-contract findings) ---

#[blaze::cost(O(1))]
pub fn bad_o1_calls_amort(x: u32) -> u32 { push_amort(x) }

#[blaze::cost(O(1))]
pub fn bad_o1_unmarked_loop(xs: &[u32]) -> u32 {
    let mut s = 0u32;
    for &x in xs {
        s = s.wrapping_add(x);
    }
    s
}

#[blaze::cost(O(n))]
pub fn bad_on_calls_scan_in_loop(xs: &[u32]) -> bool {
    for &x in xs {
        if scan_for(xs, x) {
            return true;
        }
    }
    false
}

#[blaze::cost(O(n))]
pub fn bad_on_calls_bsearch_in_loop(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(bsearch(x));
    }
    acc
}

#[blaze::cost(O(n))]
pub fn bad_on_nested_loops(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        for &y in xs {
            acc = acc.wrapping_add(x + y);
        }
    }
    acc
}

#[blaze::cost(O(n), space = O(1))]
pub fn bad_const_space_calls_linear_space(xs: &[u32]) -> bool {
    scan_for(xs, 1)
}

#[blaze::cost(O(n))]
pub fn bad_loop_scratch_without_reset(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(alloc_scratch(x));
    }
    acc
}

#[blaze::cost(O(n), lock_free)]
pub fn bad_lock_free_calls_locks(x: u32) -> u32 {
    locked_op(x)
}

#[blaze::cost(O(n), lock_free)]
pub fn bad_lock_free_calls_atomic(x: u32) -> u32 {
    atomic_op(x)
}

#[blaze::cost(O(n))]
pub fn bad_lock_in_loop(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    for &x in xs {
        acc = acc.wrapping_add(locked_op(x));
    }
    acc
}

#[blaze::cost(O(n))]
pub fn bad_closure_in_loop(xs: &[u32]) -> bool {
    for &x in xs {
        let f = || scan_for(xs, x);
        if f() {
            return true;
        }
    }
    false
}

#[blaze::cost(O(log n))]
pub fn bad_direct_recursion(n: u32) -> u32 {
    if n == 0 { 0 } else { bad_direct_recursion(n - 1) }
}

#[blaze::cost(O(bogus))]
pub fn bad_malformed_contract(x: u32) -> u32 { x }

#[blaze::cost(O(n))]
pub fn bad_malformed_loop_mark(xs: &[u32]) -> u32 {
    let mut acc = 0u32;
    #[blaze::cost(bogus)]
    for &x in xs {
        acc = acc.wrapping_add(x);
    }
    acc
}

fn main() {}
"#;

fn toolchain() -> Toolchain {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))
}

fn findings() -> Vec<(String, Result<Vec<(String, String)>, String>)> {
    let tc = toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate {
        name: "hello_world",
        dir: "/hello-world/",
        root: "/hello-world/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut world = ProjectWorld::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let bodies = world.bodies(Alloc::global());
    let mut out = Vec::new();
    for (b, _) in bodies.iter() {
        let name = match *b {
            BodyRef::Free(k) => names.item_text(k).and_then(|t| t.name).unwrap_or("?").to_string(),
            BodyRef::Member { name, .. } => name.to_string(),
        };
        let v = world.check(*b, Alloc::global());
        let got = if !v.is_clean() {
            Err(format!("{v:?}"))
        } else {
            Ok(v.findings.iter().map(|f| (f.lint.to_string(), f.text.as_str().to_string())).collect())
        };
        out.push((name, got));
    }
    out
}

fn of<'g>(got: &'g [(String, Result<Vec<(String, String)>, String>)], n: &str) -> &'g [(String, String)] {
    match got.iter().find(|(m, _)| m == n) {
        Some((_, Ok(f))) => f,
        Some((_, Err(e))) => panic!("`{n}` was not checked clean: {e}"),
        None => panic!("`{n}` was not listed"),
    }
}

#[test]
fn clean_cost_contracts_produce_no_findings() {
    let got = findings();
    for n in [
        "fast_o1",
        "push_amort",
        "bsearch",
        "scan_for",
        "scan_const_space",
        "alloc_scratch",
        "local_op",
        "atomic_op",
        "locked_op",
        "linear_method",
        "ok_o1_calls_o1",
        "ok_o1_bounded_loop",
        "ok_on_single_loop_amort",
        "ok_nlogn_calls_bsearch",
        "ok_on_amortized_inner",
        "ok_const_space_caller",
        "ok_reset_loop_scratch",
        "ok_lock_free_calls_local",
        "ok_lock_outside_loop",
        "ok_locks_declared_in_loop",
        "ok_unannotated_nested",
        "main",
    ] {
        assert!(of(&got, n).is_empty(), "`{n}` must have no finding: {:?}", of(&got, n));
    }
}

#[test]
fn cost_contract_violations_are_reported_with_clear_messages() {
    let got = findings();
    let cases: &[(&str, &str)] = &[
        ("bad_o1_calls_amort", "amortized O(1)"),
        ("bad_o1_unmarked_loop", "loop at depth 1"),
        ("bad_on_calls_scan_in_loop", "O(n^2)"),
        ("bad_on_calls_bsearch_in_loop", "O(n log n)"),
        ("bad_on_nested_loops", "loop at depth 2"),
        ("bad_const_space_calls_linear_space", "effective space cost O(n)"),
        ("bad_loop_scratch_without_reset", "effective space cost O(n^2)"),
        ("bad_lock_free_calls_locks", "synchronization cost `locks`"),
        ("bad_lock_free_calls_atomic", "synchronization cost `atomic`"),
        ("bad_lock_in_loop", "acquires a lock (`locks`) inside a loop"),
        ("bad_closure_in_loop", "O(n^2)"),
        ("bad_direct_recursion", "direct recursive call"),
        ("bad_malformed_contract", "malformed or duplicate `#[blaze::cost(..)]` contract"),
        ("bad_malformed_loop_mark", "malformed or duplicate `#[blaze::cost(..)]` loop mark"),
    ];
    for &(name, needle) in cases {
        let f = of(&got, name);
        assert!(!f.is_empty(), "`{name}` must report a cost-contract finding");
        assert!(f.iter().all(|(lint, _)| lint == "cost-contract"), "`{name}`: {f:?}");
        assert!(
            f.iter().any(|(_, text)| text.contains(needle)),
            "`{name}` expected {needle:?} in {f:?}"
        );
    }
}

#[test]
fn cost_contracts_survive_head_record_round_trip() {
    let tc = toolchain();
    let files = [("/hello-world/src/main.rs".to_string(), TEXT.to_string())];
    let edition = edition::PackageEdition::of_manifest(EDITION).expect("a known edition");
    let user = UserCrate {
        name: "hello_world",
        dir: "/hello-world/",
        root: "/hello-world/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEXT }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());

    let expected: &[(&str, CostContract)] = &[
        ("fast_o1", CostContract::time_only(CostClass::O1, false)),
        ("bsearch", CostContract::time_only(CostClass::OLogN, true)),
        ("scan_const_space", CostContract::new(CostClass::ON, CostClass::O1, SyncCost::None, false)),
        ("alloc_scratch", CostContract::new(CostClass::O1, CostClass::ON, SyncCost::None, false)),
        ("local_op", CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Local, false)),
        ("atomic_op", CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Atomic, false)),
        ("locked_op", CostContract::new(CostClass::O1, CostClass::O1, SyncCost::Lock, false)),
        ("ok_unannotated_nested", CostContract::NONE),
        ("bad_malformed_contract", CostContract::INVALID),
    ];

    for &(n, want) in expected {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, Alloc::global())
            .unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        store.ensure(&names, &mut front.keys, Want::Fn(k)).unwrap_or_else(|e| panic!("`{n}`'s head: {e:?}"));
        let ck = blaze_ladder::front2::keys::checker_of(k);
        let head = from_store(&store, HeadAt::Fn(ck)).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let HeadRef::Fn { head: fn_head, .. } = head else { panic!("`{n}` is no fn head") };
        assert_eq!(fn_head.cost, want, "`{n}`'s made head cost");
        let bytes = encode(head, store.tys.view(), Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: not encoded: {e}"));
        let text = head.text();
        let decoded = decode(&bytes, text, &store.tys, Alloc::global()).unwrap_or_else(|e| panic!("`{n}`: not decoded: {e}"));
        let HeadRef::Fn { head: decoded_fn, .. } = decoded.as_ref() else { panic!("`{n}` decoded as no fn head") };
        assert_eq!(decoded_fn.cost, want, "`{n}`'s head cost after round trip");
    }
}
