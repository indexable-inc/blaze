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

#[test]
fn stale_older_format_answer_in_worktree_falls_back_to_current_base_and_control_is_caught() {
    use blaze_cas::Key as CasKey;
    use blaze_keyed::kind::{Namespace, Tag};

    let run_case = |plant: BasePlant| -> Checked {
        BASE_PLANT.set(plant);
        let io = Io::simulated(SimWorld::new(31, Limits::USUAL));
        repository_a(&io, A1);
        repository_a(&io, A2);
        let a1 = at(&io, A1);
        assert!(a1.entered(blaze_engine::Stage::Bodies) > 0, "A1 cold check enters bodies");
        let a2_cold = at(&io, A2);
        assert_eq!(a2_cold.errors, 0);

        // Overwrite A2's own answer pointer root (`engine.<hex>`) with a sealed older-format
        // (`BEL7`) pointer record, simulating an existing 2nd worktree whose last check ran on an
        // older binary format.
        let cx = io.cx(BUDGET, TASK_FRAME).expect("root cx");
        let frame = Frame::with_reserve(16 << 20, bstd::mem::Tag::Other).expect("frame");
        let opts = engine_check_tests::opts(&io, 1, false);
        let fs = io.need_fs().expect("files");
        let root_dir = fs.open_dir(&cx, &Path::new(A2.dir).expect("a2 path")).expect("open");
        let err = io.need_err().expect("err");
        let clock = io.need_clock().expect("clock");
        let host = io.need_host().expect("host");
        let mut rng = io.need_rng().expect("rng");
        let opened = crate::store::open_for_command(&cx, &fs, &host, &mut rng, opts.place.home, None, &root_dir, A2.dir, &clock, &err, opts.heap).expect("open store");
        let answer_root_name = crate::engine_answer::root_name(A2.dir, &frame);
        assert!(opened.lent.has_root(answer_root_name.as_str()), "A2 must have its own pointer root");
        let mut stale_ptr = [0u8; 40];
        stale_ptr[..4].copy_from_slice(b"BEL7");
        let sealed = crate::seal::seal(&stale_ptr, &[], &frame).expect("seal stale ptr");
        let stale_key = CasKey(blaze_ids::ContentId::of("blaze-cli engine-check pointer", &sealed));
        opened.lent.put(&stale_key, Namespace::LOCAL, Tag(1), &sealed).expect("put stale ptr");
        opened.lent.root_for(answer_root_name.as_str(), &[stale_key], blaze_cas::plain::ROOT_LIFE_NS).expect("root stale ptr");
        opened.lent.release_lease();
        opened.lent.commit().expect("commit stale ptr");
        drop(opened);
        drop(sealed);
        drop(answer_root_name);
        drop(frame);

        let a2_after = at(&io, A2);
        BASE_PLANT.set(BasePlant::None);
        a2_after
    };

    let fixed = run_case(BasePlant::None);
    assert!(fixed.engine().from_last, "stale BEL7 own answer must fall back to A1's current base:\n{}", fixed.profile);
    assert_eq!(fixed.entered(blaze_engine::Stage::Bodies), 0, "stale BEL7 fallback enters 0 bodies:\n{}", fixed.profile);
    assert_eq!(fixed.entered(blaze_engine::Stage::Names), 0, "stale BEL7 fallback enters 0 names:\n{}", fixed.profile);
    assert_eq!(fixed.errors, 0);

    // Planted bug control (rule 43): disabling stale fallback rebuilds A2 from `no answer`.
    let control = run_case(BasePlant::NoStaleFallback);
    assert!(!control.engine().from_last, "control must start from no answer:\n{}", control.profile);
    assert!(control.entered(blaze_engine::Stage::Bodies) > 0, "control must rebuild bodies:\n{}", control.profile);
}

#[test]
fn stale_older_tools_answer_in_worktree_falls_back_to_current_binary_base_and_control_is_caught() {
    let check_with = |io: &Io, checkout: Checkout, older_tools: bool| {
        let opts = || {
            let mut opts = if older_tools {
                planted(io, Some((blaze_ids::producer::Row::BodyChecker, "an older body checker")))
            } else {
                planted(io, None)
            };
            opts.place.checkout = checkout.dir;
            opts
        };
        check_in::<fix::Atomic>(io, checkout, &opts, View::Person, None)
    };

    let run_case = |plant: BasePlant| -> Checked {
        BASE_PLANT.set(plant);
        let io = Io::simulated(SimWorld::new(37, Limits::USUAL));
        repository_a(&io, A1);
        repository_a(&io, A2);
        // A2 checked earlier with an older binary (older BodyChecker producer hash).
        let a2_old = check_with(&io, A2, true);
        assert!(a2_old.entered(blaze_engine::Stage::Bodies) > 0);
        // User upgrades binary and checks A1 first, publishing a current-binary workspace base.
        let a1_new = check_with(&io, A1, false);
        assert!(a1_new.entered(blaze_engine::Stage::Bodies) > 0);
        // Now user checks A2 with the current binary: A2's own answer is older-tools,
        // so it should automatically adopt A1's current-binary workspace base!
        let a2_new = check_with(&io, A2, false);
        BASE_PLANT.set(BasePlant::None);
        a2_new
    };

    let fixed = run_case(BasePlant::None);
    assert!(fixed.engine().from_last, "A2 must reuse A1's current-binary base:\n{}", fixed.profile);
    assert_eq!(fixed.entered(blaze_engine::Stage::Bodies), 0, "A2 must enter 0 bodies by adopting A1's current-binary base:\n{}", fixed.profile);
    assert_eq!(fixed.errors, 0);

    // Planted bug control (rule 43): without stale-tools fallback, A2 keeps its older-tools own
    // answer and re-enters every body task.
    let control = run_case(BasePlant::NoStaleFallback);
    assert!(control.entered(blaze_engine::Stage::Bodies) > 0, "control must re-enter bodies under older-tools own answer:\n{}", control.profile);
}

/// Regression test for Bug 2 (`CrateScope` `MissingEntry { table: WorldCrates }` after starting
/// from `tool-base` and hitting a layout/toolchain cell update):
/// 1. `keep_toolchain_base` must populate `by_file` / `root.files` for every non-inline toolchain
///    row so `Edit.files` retires any stale toolchain `CrateScope` not in today's `Names` world.
/// 2. A post-fix `tool-base` carrying an extra `CrateScope` row reading a cell defined by `core`
///    cleanly retires that row via `Edit.files` when repo B runs with a moved `Names` tool (so
///    `core` re-runs and updates its cells), never waking it into `MissingEntry { WorldCrates }`.
/// 3. A pre-fix legacy `tool-base` with empty `root.files` (`PagesPlant::FilesDropped`) is
///    rejected by `open_toolchain_base` and automatically rewritten with populated `root.files` on
///    the next check.
#[test]
fn toolchain_base_populates_by_file_and_retires_stale_cratescopes_on_layout_change() {
    use blaze_drive::{Kind, Name};
    use blaze_engine::{Cause, CellId, Phase, Row, Stage};
    use crate::store::Store as _;

    BASE_PLANT.set(BasePlant::None);
    let io = Io::simulated(SimWorld::new(59, Limits::USUAL));
    let shelf = lang_shelf(&io);
    repository_a(&io, A1);
    repository_b(&io, B);

    let check_with = |checkout: Checkout| {
        let opts = || {
            let mut opts = engine_check_tests::opts(&io, 2, false);
            opts.place.checkout = checkout.dir;
            opts.toolchain = shelf;
            opts
        };
        check_in::<fix::Atomic>(&io, checkout, &opts, View::Person, None)
    };

    // 1. Cold check of A1 populates `tool-base`. Verify EVERY non-inline row in `tool-base` is
    //    owned by its `row.file` in both `decoded.answer.by_file` and `root.files`.
    let a1 = check_with(A1);
    assert_eq!(a1.errors, 0, "A1 cold check clean:\n{}", a1.profile);

    let ghost_scope = Name::new(Kind::CrateScope, blaze_ids::ContentId::of("bug2 ghost cratescope", b"stale-edition-crate"));
    let mut core_cell: Option<(CellId, blaze_ids::ContentId)> = None;
    let orig_decoded_rows: usize;

    {
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
        let dir = crate::engine_answer::open_root(&cx, &mut store, ptr.root, opts.heap.shared())
            .expect("open root")
            .expect("valid root");
        assert!(!dir.files.is_empty(), "tool-base root.files must be populated so stale rows can be retired");

        let mut decoded = blaze_engine::pages::decode(
            ptr.root,
            |k| match store.get(&cx, k) {
                Ok(Some(held)) => Some(Ok(blaze_engine::pages::Sealed::by_store(k, held.bytes, held.proof))),
                _ => None,
            },
            opts.heap.shared(),
        )
        .expect("decode tool-base");
        orig_decoded_rows = decoded.answer.rows.len();
        assert!(orig_decoded_rows > 0, "tool-base holds toolchain rows");
        for (&name, row) in decoded.answer.rows.iter() {
            if !row.inline {
                assert!(
                    decoded.answer.by_file.get(&row.file).is_some_and(|members| members.binary_search(&name).is_ok()),
                    "tool-base row {name:?} must be listed under its file {:?} in by_file",
                    row.file
                );
                assert!(
                    dir.files.contains_key(&row.file),
                    "tool-base root.files must contain {:?} for row {name:?}",
                    row.file
                );
            }
            if name.kind == Kind::CrateScope && core_cell.is_none() {
                if let Some(&(cell, digest)) = row.defines.first() {
                    core_cell = Some((CellId { of: name, cell }, digest));
                }
            }
        }
        let (cell_id, _cell_digest) = core_cell.expect("at least one toolchain CrateScope defines a cell");

        // 2. Mutate `cell_id.of`'s stored key & cell digest in `tool-base` so repo B MUST re-enter
        //    `cell_id.of` in `Stage::Names` and fire `Engine::move_cell(cell_id)` -> `readers_of`,
        //    and inject a stale extra toolchain `CrateScope` (`ghost_scope`) reading `cell_id` AND
        //    properly listed in `by_file[ghost_scope] = [ghost_scope]`.
        let stale_digest = blaze_ids::ContentId([0xDD; 32]);
        if let Some(definer) = decoded.answer.rows.get_mut(&cell_id.of) {
            definer.key = blaze_drive::Key(blaze_ids::ContentId([0xCC; 32]));
            if let Some(slot) = definer.defines.iter_mut().find(|(c, _)| *c == cell_id.cell) {
                slot.1 = stale_digest;
            }
        }
        if let Some(&(_, serial)) = decoded.answer.cells.get(&cell_id) {
            decoded.answer.cells.insert(cell_id, (stale_digest, serial));
        }
        let ghost_payload = blaze_drive::key::content_key(b"ghost-cratescope-payload");
        store.put(&cx, ghost_payload, b"ghost-cratescope-payload").expect("put ghost payload");
        let ghost_src = blaze_ids::ContentId::of("ghost src", b"v1");
        let mut reads = blaze_own::Vec::new_in(opts.heap.shared());
        reads.push((cell_id, stale_digest));
        let mut by_cell = blaze_own::Vec::new_in(opts.heap.shared());
        by_cell.push(0);
        decoded.answer.rows.insert(ghost_scope, Row {
            key: opts.tools.key(ghost_scope, &ghost_src),
            phase: Phase::new(Stage::Names, 1),
            file: ghost_scope,
            source: ghost_src,
            out: ghost_payload.0,
            serial: decoded.answer.serial,
            defines: blaze_own::Vec::new_in(opts.heap.shared()),
            reads,
            by_cell,
            findings: 0,
            inline: false,
            failed: false,
        });
        decoded.answer.by_file.entry(ghost_scope).or_insert_with(|| blaze_own::Vec::new_in(opts.heap.shared())).push(ghost_scope);
        decoded.answer.readers = blaze_engine::engine::Readers::from_rows(&decoded.answer.rows, opts.heap.shared());
        decoded.records.insert(ghost_scope, ghost_payload);
        decoded.causes.insert(ghost_scope, (decoded.answer.serial, Cause::New));

        // Rewrite `tool-base` with `ghost_scope` present and properly owned in `root.files`.
        let encoded = blaze_engine::pages::encode(&decoded, None, opts.heap.shared());
        for (k, b) in encoded.puts.iter() {
            store.put(&cx, *k, b).expect("put updated tool-base page");
        }
        let mut page_idx = blaze_own::Vec::new_in(opts.heap.shared());
        for (page, page_rows) in encoded.page_records() {
            let mut keys = blaze_own::Vec::new_in(opts.heap.shared());
            keys.push(blaze_cas::Key(page.0));
            for &(_, rec) in page_rows {
                keys.push(blaze_cas::Key(rec.0));
            }
            keys.sort_unstable();
            keys.dedup();
            let beside = opened.lent.index_beside(blaze_cas::Key(page.0), &keys).expect("index beside");
            page_idx.push(beside);
        }
        page_idx.sort_unstable();
        let (empty_lines_key, empty_lines_bytes) = crate::engine_lines::Lines::new_in(opts.heap.shared()).record();
        store.put(&cx, empty_lines_key, &empty_lines_bytes).expect("put empty lines");
        let empty_rows = crate::sweep::Rows::new_in(&frame);
        let leaves = crate::sweep::Leaves::bytes_in(&empty_rows, &frame).expect("leaves");
        let mut pointer = blaze_own::Vec::new_in(&frame);
        pointer.extend_from_slice(crate::engine_answer::POINTER_MAGIC);
        pointer.extend_from_slice(&encoded.root.0.0);
        pointer.extend_from_slice(&empty_lines_key.0.0);
        pointer.extend_from_slice(&encoded.files.to_le_bytes());
        pointer.push(1); // names=true, bodies_whole=false, replayable=false
        pointer.extend_from_slice(&encoded.findings.to_le_bytes());
        pointer.extend_from_slice(&encoded.owed.to_le_bytes());
        pointer.extend_from_slice(&crate::engine_answer::tools_digest(&opts.tools).0);
        pointer.push(1); // HEADERS_ABSENT
        pointer.extend_from_slice(&decoded.answer.serial.0.to_le_bytes());
        pointer.extend_from_slice(&[0u8; 32 * blaze_engine::pages::HeaderPages::COUNT]);
        crate::journal_sweep::Stored::encode_units(core::iter::empty(), None, &mut pointer);
        let sealed = crate::seal::seal(&pointer, &leaves, &frame).expect("seal");
        let ptr_key = blaze_drive::Key(blaze_ids::ContentId::of("blaze-cli engine-check pointer", &sealed));
        store.put(&cx, ptr_key, &sealed).expect("put ptr");
        let mut direct = blaze_own::Vec::new_in(opts.heap.shared());
        direct.push(blaze_cas::Key(encoded.root.0));
        direct.extend(encoded.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
        direct.push(blaze_cas::Key(empty_lines_key.0));
        let ans_idx = opened.lent.index(&direct).expect("answer index");
        let tc_root = crate::engine_answer::toolchain_base_root(shelf, &opts.tools, &frame);
        let reach_name = bstd::mem::format_in!(&frame, "{}.{}", tc_root.as_str(), crate::engine_answer::REACH_ROOT);
        let answer_name = bstd::mem::format_in!(&frame, "{}.{}", tc_root.as_str(), crate::engine_answer::ANSWER_ROOT);
        let life = crate::store::roots::TOOLCHAIN_LIFE_NS;
        opened.lent.root_through(reach_name.as_str(), &page_idx, life).expect("pin reach");
        opened.lent.root_through(answer_name.as_str(), &[ans_idx], life).expect("pin answer");
        opened.lent.root_for(tc_root.as_str(), &[blaze_cas::Key(ptr_key.0)], life).expect("pin tc_root");
        opened.lent.commit().expect("commit updated tool-base");
    }

    // Repo B starts from that `tool-base`, re-enters `cell_id.of` (moving `cell_id`), AND
    // `Edit.files` retires `ghost_scope` cleanly — Stage::Names MUST NOT wake `ghost_scope` into
    // `MissingEntry { table: WorldCrates }`!
    let b = check_with(B);
    assert!(b.engine().from_last, "B starts from post-fix tool-base:\n{}", b.profile);
    assert!(b.entered(Stage::Names) >= 3, "B re-entered the mutated toolchain crate + its own 2 units:\n{}", b.profile);
    assert_eq!(b.errors, 0, "B cleanly retired ghost_scope without MissingEntry:\n{}", b.profile);

    // 3. Now simulate a pre-fix legacy `tool-base` with empty `root.files` (`PagesPlant::FilesDropped`):
    //    `open_toolchain_base` must reject it (`Ok(None)`) rather than leaking unowned ghost rows!
    {
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
        .expect("decode");
        // Encode with `PagesPlant::FilesDropped` to produce a pre-fix `tool-base` with empty `root.files`.
        let encoded = blaze_engine::pages::encode_planted(&decoded, None, blaze_engine::pages::PagesPlant::FilesDropped, opts.heap.shared());
        for (k, b) in encoded.puts.iter() {
            store.put(&cx, *k, b).expect("put legacy page");
        }
        let mut page_idx = blaze_own::Vec::new_in(opts.heap.shared());
        for (page, page_rows) in encoded.page_records() {
            let mut keys = blaze_own::Vec::new_in(opts.heap.shared());
            keys.push(blaze_cas::Key(page.0));
            for &(_, rec) in page_rows {
                keys.push(blaze_cas::Key(rec.0));
            }
            keys.sort_unstable();
            keys.dedup();
            page_idx.push(opened.lent.index_beside(blaze_cas::Key(page.0), &keys).expect("beside"));
        }
        page_idx.sort_unstable();
        let (empty_lines_key, empty_lines_bytes) = crate::engine_lines::Lines::new_in(opts.heap.shared()).record();
        store.put(&cx, empty_lines_key, &empty_lines_bytes).expect("put lines");
        let empty_rows = crate::sweep::Rows::new_in(&frame);
        let leaves = crate::sweep::Leaves::bytes_in(&empty_rows, &frame).expect("leaves");
        let mut pointer = blaze_own::Vec::new_in(&frame);
        pointer.extend_from_slice(crate::engine_answer::POINTER_MAGIC);
        pointer.extend_from_slice(&encoded.root.0.0);
        pointer.extend_from_slice(&empty_lines_key.0.0);
        pointer.extend_from_slice(&encoded.files.to_le_bytes());
        pointer.push(1);
        pointer.extend_from_slice(&encoded.findings.to_le_bytes());
        pointer.extend_from_slice(&encoded.owed.to_le_bytes());
        pointer.extend_from_slice(&crate::engine_answer::tools_digest(&opts.tools).0);
        pointer.push(1);
        pointer.extend_from_slice(&decoded.answer.serial.0.to_le_bytes());
        pointer.extend_from_slice(&[0u8; 32 * blaze_engine::pages::HeaderPages::COUNT]);
        crate::journal_sweep::Stored::encode_units(core::iter::empty(), None, &mut pointer);
        let sealed = crate::seal::seal(&pointer, &leaves, &frame).expect("seal");
        let ptr_key = blaze_drive::Key(blaze_ids::ContentId::of("blaze-cli engine-check pointer", &sealed));
        store.put(&cx, ptr_key, &sealed).expect("put ptr");
        let mut direct = blaze_own::Vec::new_in(opts.heap.shared());
        direct.push(blaze_cas::Key(encoded.root.0));
        direct.extend(encoded.tree_keys().iter().map(|k| blaze_cas::Key(k.0)));
        direct.push(blaze_cas::Key(empty_lines_key.0));
        let ans_idx = opened.lent.index(&direct).expect("answer index");
        let tc_root = crate::engine_answer::toolchain_base_root(shelf, &opts.tools, &frame);
        let reach_name = bstd::mem::format_in!(&frame, "{}.{}", tc_root.as_str(), crate::engine_answer::REACH_ROOT);
        let answer_name = bstd::mem::format_in!(&frame, "{}.{}", tc_root.as_str(), crate::engine_answer::ANSWER_ROOT);
        let life = crate::store::roots::TOOLCHAIN_LIFE_NS;
        opened.lent.root_through(reach_name.as_str(), &page_idx, life).expect("pin reach");
        opened.lent.root_through(answer_name.as_str(), &[ans_idx], life).expect("pin answer");
        opened.lent.root_for(tc_root.as_str(), &[blaze_cas::Key(ptr_key.0)], life).expect("pin tc_root");
        opened.lent.commit().expect("commit legacy tool-base");

        // `open_toolchain_base` must reject the legacy empty-files `tool-base`!
        let opened_tc = crate::engine_answer::open_toolchain_base(&cx, &mut store, &opened.lent, shelf, &opts.tools, opts.heap.shared(), &frame)
            .expect("open_toolchain_base");
        assert!(opened_tc.is_none(), "pre-fix tool-base with empty root.files must be rejected");
    }
}
