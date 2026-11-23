//! The home's bases end to end (engine_answer.rs `BASES_ROOT`, thoughts2 1.5): one home, three
//! checkouts, two repositories. A1 checks, then B (another repository) checks, then A2, a new
//! worktree of A1's repository with no answer of its own: it starts from A1's answer, its
//! workspace's base, and enters no body task. THE CONTROL (rule 23): the home's one pointer as
//! before 2026-11-21 (`BasePlant::OnePointer`), where B's save replaced the base A2 starts
//! from, so A2 starts from B's answer and enters A's bodies. A child of main.rs's tests: its
//! helpers are theirs.
//!
//! The two repositories hold nothing outside them, so their layouts read no lock: the save
//! learns each workspace from the members' own lock (units.rs `lay`, sweep.rs `members_read`),
//! and A2's start reads its checkout's `Cargo.lock` (engine_answer.rs `load_base`), written
//! here as cargo writes one for these members.

use super::*;
use crate::engine_answer::{BASE_PLANT, BasePlant};

const A1: Checkout = Checkout { dir: "a1", shown: "/a1" };
const B: Checkout = Checkout { dir: "b", shown: "/b" };
const A2: Checkout = Checkout { dir: "a2", shown: "/a2" };

/// Repository A: one crate `alpha`, two modules, a body each.
fn repository_a(io: &Io, at: Checkout) {
    write_in(io, at, "Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_in(io, at, "Cargo.lock", "version = 4\n\n[[package]]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
    write_in(io, at, "src/lib.rs", "mod a;\nmod b;\npub fn f() -> u8 {\n    a::g() + b::h()\n}\n");
    write_in(io, at, "src/a.rs", "pub fn g() -> u8 {\n    1\n}\n");
    write_in(io, at, "src/b.rs", "pub fn h() -> u8 {\n    2\n}\n");
}

/// Repository B: one crate `beta`, other bodies.
fn repository_b(io: &Io, at: Checkout) {
    write_in(io, at, "Cargo.toml", "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write_in(io, at, "Cargo.lock", "version = 4\n\n[[package]]\nname = \"beta\"\nversion = \"0.1.0\"\n");
    write_in(io, at, "src/lib.rs", "pub fn x() -> u32 {\n    let y = 3;\n    y * 7\n}\n\npub fn z() -> u32 {\n    x() + 1\n}\n");
}

fn at(io: &Io, checkout: Checkout) -> Checked {
    let opts = || {
        let mut opts = engine_check_tests::opts(io, 1, false);
        opts.place.checkout = checkout.dir;
        opts
    };
    check_in::<fix::Atomic>(io, checkout, &opts, View::Person, None)
}

/// A1, then B, then A2 under `plant`, one home: (A1's check, A2's check).
fn a_b_then_a_new_worktree(plant: BasePlant) -> (Checked, Checked) {
    BASE_PLANT.set(plant);
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    repository_a(&io, A1);
    repository_b(&io, B);
    repository_a(&io, A2);
    let a1 = at(&io, A1);
    let b = at(&io, B);
    assert!(b.engine().entered > 0, "B's cold check enters its own tasks:\n{}", b.profile);
    let a2 = at(&io, A2);
    BASE_PLANT.set(BasePlant::None);
    (a1, a2)
}

#[test]
fn a_new_worktree_starts_from_its_own_workspaces_base() {
    let (a1, a2) = a_b_then_a_new_worktree(BasePlant::None);
    assert!(a1.entered(blaze_engine::Stage::Bodies) > 0, "A1's cold check enters its bodies, or the comparison proves nothing:\n{}", a1.profile);
    assert_eq!(a2.entered(blaze_engine::Stage::Bodies), 0, "A2 entered body tasks A1's answer holds: it did not start from its workspace's base:\n{}", a2.profile);
    assert!(a2.engine().from_last, "A2 starts from a stored answer (A1's):\n{}", a2.profile);
    assert_eq!((a2.errors, a2.status, &a2.ids), (a1.errors, a1.status, &a1.ids), "A2's answer is A1's");
}

/// The control: with one pointer for the home, B's save is A2's base, so A2 enters A's bodies.
#[test]
fn control_one_pointer_for_the_home_starts_a_new_worktree_from_another_repository() {
    let (a1, a2) = a_b_then_a_new_worktree(BasePlant::OnePointer);
    assert!(a2.entered(blaze_engine::Stage::Bodies) > 0, "the one home-wide pointer (B's answer) still gave A2 every body: the control proves nothing:\n{}\n--- A1's:\n{}", a2.profile, a1.profile);
}

#[test]
fn unrelated_fresh_repo_reuses_toolchain_base_automatically() {
    BASE_PLANT.set(BasePlant::None);
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    let shelf = lang_shelf(&io);
    repository_a(&io, A1);
    repository_b(&io, B);
    let check_with_shelf = |checkout: Checkout| {
        let opts = || {
            let mut opts = engine_check_tests::opts(&io, 2, false);
            opts.place.checkout = checkout.dir;
            opts.toolchain = shelf;
            opts
        };
        check_in::<fix::Atomic>(&io, checkout, &opts, View::Person, None)
    };
    let a1 = check_with_shelf(A1);
    assert!(a1.entered(blaze_engine::Stage::Names) >= 5, "A1 cold check resolves 3 toolchain crates + alpha's units:\n{}", a1.profile);
    let b = check_with_shelf(B);
    assert!(b.engine().from_last, "unrelated repo B starts from `tool-base` fallback:\n{}", b.profile);
    // B has lib + test units (2 crates); core/alloc/std all stand from `tool-base`!
    assert_eq!(b.entered(blaze_engine::Stage::Names), 2, "B enters only its own 2 units (all 3 toolchain crates stand from `tool-base`):\n{}", b.profile);
    assert!(b.entered(blaze_engine::Stage::Heads) < a1.entered(blaze_engine::Stage::Heads), "B reuses toolchain `ImplBucket`s from `tool-base`:\n{}", b.profile);
    assert_eq!(b.errors, 0, "B checks cleanly:\n{}", b.profile);
}

#[test]
fn toolchain_base_populates_cells_and_merges_from_work_rows() {
    BASE_PLANT.set(BasePlant::None);
    let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
    let shelf = lang_shelf(&io);
    // A1 is a plain Rust crate (populates `tool-base` from `kept.answer.rows` on cold check).
    repository_a(&io, A1);
    // B is a different unrelated repo with a second edition / fresh build on top of `tool-base`
    // (`from_last == true`, so newly computed rows live in `kept.answer.work().rows`).
    repository_b(&io, B);
    let check_with_shelf = |checkout: Checkout| {
        let opts = || {
            let mut opts = engine_check_tests::opts(&io, 2, false);
            opts.place.checkout = checkout.dir;
            opts.toolchain = shelf;
            opts
        };
        check_in::<fix::Atomic>(&io, checkout, &opts, View::Person, None)
    };
    let a1 = check_with_shelf(A1);
    assert_eq!(a1.errors, 0, "A1 checks cleanly:\n{}", a1.profile);
    // Verify `tool-base` actually stored `cells` for every toolchain row's `defines`.
    use crate::store::Store as _;
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
    let frame = Frame::with_reserve(16 << 20, bstd::mem::Tag::Other).expect("frame");
    let alloc = blaze_own::Erased::new(&frame);
    let opts = engine_check_tests::opts(&io, 2, false);
    let fs = io.need_fs().expect("files");
    let root = fs.open_dir(&cx, &Path::new(A1.dir).expect("a path")).expect("open");
    let err = io.need_err().expect("err");
    let clock = io.need_clock().expect("clock");
    let host = io.need_host().expect("host");
    let mut rng = io.need_rng().expect("rng");
    let opened = crate::store::open_for_command(&cx, &fs, &host, &mut rng, opts.place.home, None, &root, A1.dir, &clock, &err, opts.heap).expect("open store");
    let mut store = crate::store::CommandStore::new(&cx, &opened.lent, false, opened.open_ns, None, clock.clone(), &err, alloc.handle()).expect("store");
    let ptr = crate::engine_answer::load_toolchain_base(&cx, &mut store, shelf, &opts.tools, &frame)
        .expect("load tool-base")
        .expect("tool-base exists")
        .expect("tool-base readable");
    let decoded = blaze_engine::pages::decode(
        ptr.root,
        |k| match store.get(&cx, k) {
            Ok(Some(held)) => Some(Ok(blaze_engine::pages::Sealed::by_store(k, held.bytes, held.proof))),
            _ => None,
        },
        opts.heap.shared(),
    )
    .expect("decode tool-base");
    assert!(!decoded.answer.rows.is_empty(), "tool-base holds toolchain rows");
    let mut expected_cells = 0usize;
    for (&name, row) in decoded.answer.rows.iter() {
        for &(cell, digest) in row.defines.iter() {
            expected_cells += 1;
            let got = decoded.answer.cells.get(&blaze_engine::CellId { of: name, cell });
            assert_eq!(got.map(|&(d, _)| d), Some(digest), "tool-base must store cell digest for {name:?}/{cell:?}");
        }
    }
    assert!(expected_cells > 0, "toolchain rows define cells that must be stored in tool-base");
    drop(store);
    drop(opened);
    let b = check_with_shelf(B);
    assert!(b.engine().from_last, "B starts from tool-base:\n{}", b.profile);
    assert_eq!(b.entered(blaze_engine::Stage::Names), 2, "B reuses all toolchain Names from tool-base:\n{}", b.profile);
    assert_eq!(b.errors, 0, "B checks cleanly:\n{}", b.profile);
}

#[test]
fn incremental_edit_and_second_cold_repo_do_not_rewrite_toolchain_base() {
    BASE_PLANT.set(BasePlant::None);
    let io = Io::simulated(SimWorld::new(29, Limits::USUAL));
    let shelf = lang_shelf(&io);
    repository_a(&io, A1);
    repository_b(&io, B);
    let check_with_shelf = |checkout: Checkout| {
        let opts = || {
            let mut opts = engine_check_tests::opts(&io, 2, false);
            opts.place.checkout = checkout.dir;
            opts.toolchain = shelf;
            opts.driver = crate::engine_ready_edit::Driver::Auto;
            opts
        };
        check_in::<fix::Atomic>(&io, checkout, &opts, View::Person, None)
    };
    let read_tc_ptr = || {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let frame = Frame::with_reserve(16 << 20, bstd::mem::Tag::Other).expect("frame");
        let alloc = blaze_own::Erased::new(&frame);
        let opts = engine_check_tests::opts(&io, 2, false);
        let fs = io.need_fs().expect("files");
        let root = fs.open_dir(&cx, &Path::new(A1.dir).expect("a path")).expect("open");
        let err = io.need_err().expect("err");
        let clock = io.need_clock().expect("clock");
        let host = io.need_host().expect("host");
        let mut rng = io.need_rng().expect("rng");
        let opened = crate::store::open_for_command(&cx, &fs, &host, &mut rng, opts.place.home, None, &root, A1.dir, &clock, &err, opts.heap).expect("open store");
        let mut store = crate::store::CommandStore::new(&cx, &opened.lent, false, opened.open_ns, None, clock.clone(), &err, alloc.handle()).expect("store");
        let ptr = crate::engine_answer::load_toolchain_base(&cx, &mut store, shelf, &opts.tools, &frame)
            .expect("load tool-base")
            .expect("tool-base exists")
            .expect("tool-base readable");
        ptr.key
    };
    let a1 = check_with_shelf(A1);
    assert_eq!(a1.errors, 0, "A1 cold check clean:\n{}", a1.profile);
    let key_after_cold = read_tc_ptr();
    // 1-line body edit in A1 under Driver::Auto (takes ready path, 0 toolchain rows touched).
    write_in(&io, A1, "src/a.rs", "pub fn g() -> u8 {\n    42\n}\n");
    let a1_edit = check_with_shelf(A1);
    assert_eq!(a1_edit.errors, 0, "A1 body edit clean:\n{}", a1_edit.profile);
    assert_eq!(read_tc_ptr(), key_after_cold, "body edit did not rewrite tool-base");
    // Unrelated repo B on the same toolchain also leaves tool-base untouched.
    let b = check_with_shelf(B);
    assert_eq!(b.errors, 0, "B cold check clean:\n{}", b.profile);
    assert_eq!(read_tc_ptr(), key_after_cold, "second cold repo on same toolchain did not rewrite tool-base");
}
