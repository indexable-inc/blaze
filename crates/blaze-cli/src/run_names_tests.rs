//! The form of a program's names rows (run_names.rs FORM): encoded from the answer's rows, read
//! in place, refused whole when its bytes are not of the form.

use blaze_drive::{Input, Key, Kind, Name};
use blaze_ids::ContentId;
use blaze_own::AVec;

use super::{Answered, FormError, MAGIC, RunNamesError, Stood, encode};

fn input(n: u8) -> Input {
    Input { name: Name::new(Kind::CrateScope, ContentId([n; 32])), at: Key(ContentId([n.wrapping_add(100); 32])), out: ContentId([n.wrapping_add(200); 32]) }
}

fn graph() -> Input {
    Input { name: Name::unit_graph(), at: Key(ContentId([7; 32])), out: ContentId([8; 32]) }
}

fn written(rows: &[(usize, Input)]) -> AVec<'static, u8> {
    let heap = blaze_own::testing::heap();
    let mut out = AVec::new_in(heap.alloc());
    encode(graph(), rows, &mut out, heap.alloc()).expect("the rows encode");
    out
}

/// A walk's rows, in reader order (not sorted), read back by index on a hit: each the row it
/// was written with, under the name the caller gives; an index no row has is no row.
#[test]
fn the_rows_a_walk_writes_a_hit_reads_by_index() {
    let rows = [(5, input(5)), (0, input(0)), (12, input(12))];
    let bytes = written(&rows);
    assert_eq!(&bytes[..16], &MAGIC);
    let stood = Stood::open(&bytes).expect("the record opens");
    assert_eq!(stood.graph(), Some(graph()));
    for (at, row) in rows.iter() {
        assert_eq!(stood.crate_input(*at, row.name), Some(*row));
    }
    assert_eq!(stood.crate_input(3, input(3).name), None);
    assert_eq!(stood.crate_input(13, input(13).name), None);
}

/// Bytes not of the form are refused whole, by what is wrong: never read as rows.
#[test]
fn a_record_not_of_the_form_is_refused() {
    let bytes = written(&[(1, input(1)), (2, input(2))]);
    assert_eq!(Stood::open(&bytes[..10]).err(), Some(FormError::Short { len: 10 }));
    let mut other = bytes.to_vec();
    other[0] ^= 1;
    assert_eq!(Stood::open(&other).err(), Some(FormError::Magic));
    let cut = &bytes[..bytes.len() - 1];
    assert!(matches!(Stood::open(cut), Err(FormError::Length { .. })));
    // The second row's index set below the first's.
    let mut unsorted = bytes.to_vec();
    let second = super::HEAD + super::ROW;
    unsorted[second..second + 4].copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(Stood::open(&unsorted).err(), Some(FormError::Unsorted { row: 1 }));
    // One crate written twice is refused at the write.
    let heap = blaze_own::testing::heap();
    let mut out = AVec::new_in(heap.alloc());
    assert!(matches!(encode(graph(), &[(4, input(4)), (4, input(4))], &mut out, heap.alloc()), Err(RunNamesError::Form(FormError::Unsorted { row: 1 }))));
}

/// Cargo runs a harnessless test target's main exactly once, including when another package
/// has a target with the same name. The two rows have the same spelling and distinct owners.
#[test]
fn harnessless_targets_each_have_one_main() {
    for n in [1u8, 2] {
        let main = blaze_id::ItemKey(blaze_id::ContentId([n; 32]));
        let tests = super::TestRows::Main(main);
        assert_eq!(tests.len(), 1);
        let row = tests.row(0).expect("valid test row").expect("one invocation");
        assert_eq!(
            (row.key, row.path, row.ignore, row.should_panic, row.bench),
            (main, "main", None, blaze_scope::tests_record::ShouldPanic::No, false)
        );
        assert_eq!(tests.row(1), Ok(None));
    }
}

/// `ScopeCrates::ByNeed` opens ZERO dependency crates during setup or when querying only the
/// owner crate, opens a dependency crate at most once when a question actually reaches it, and
/// produces identical `NamesSource` answers to `ScopeCrates::Opened` (rules 74, 77).
#[test]
fn by_need_scope_defers_dependency_opens_and_matches_opened_answers() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use blaze_id::CrateItemKey;
    use blaze_ladder::front2::source::NamesSource as _;
    use blaze_own::Alloc;
    use blaze_scope::graph::Sysroot;
    use blaze_scope::resolve::{CrateInput, SourceTexts};
    use blaze_surface::Edition;
    use blaze_unit_graph::{Implicit, Prelude, StdIs};
    use bstd::io::{Io, Limits, SimWorld};

    const APP_SRC: &str = "#![no_core]\npub fn entry() -> u32 { 1 }\n";
    const DEP_A_SRC: &str = "#![no_core]\npub struct Alpha;\npub fn alpha() -> Alpha { Alpha }\n";
    const DEP_B_SRC: &str = "#![no_core]\npub struct Beta;\npub fn beta() -> Beta { Beta }\n";

    struct MemTexts;
    impl<'a> SourceTexts<'a> for MemTexts {
        fn read(&self, _krate: usize, path: &str) -> Option<&'a str> {
            match path {
                "/0/app/lib.rs" => Some(APP_SRC),
                "/1/dep_a/lib.rs" => Some(DEP_A_SRC),
                "/2/dep_b/lib.rs" => Some(DEP_B_SRC),
                _ => None,
            }
        }
    }

    const IMPLICIT: Implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false };
    let mk_input = |name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]| CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-cli run_names by_need test crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: IMPLICIT,
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    };

    let inputs = [
        mk_input("app", "/0/app/lib.rs", &[("dep_a", 1), ("dep_b", 2)]),
        mk_input("dep_a", "/1/dep_a/lib.rs", &[]),
        mk_input("dep_b", "/2/dep_b/lib.rs", &[]),
    ];
    let sysroot = Sysroot { core: None, alloc: None, std: None, bstd: None };
    let solved = blaze_scope::resolve::resolve_crates(&inputs, sysroot, &MemTexts, &blaze_scope::task::Serial, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("resolve test crates: {e:?}"));

    let g = Alloc::global();
    let mut crate_recs = std::vec::Vec::new();
    for (idx, c) in solved.crates().iter().enumerate() {
        let n = blaze_scope::names_record::encode(c, g).expect("names record");
        let i = blaze_scope::items_record::encode(c, g).expect("items record");
        let split = blaze_scope::expansion_records::write(c, g).expect("expansions");
        let payload = crate::names::sections::write(0, [&[], &[], &n[..], &i[..], &[], &split.directory[..], &[], &[]]);
        let rec = crate::front_load::crate_records(
            blaze_drive::key::crate_scope_name(&inputs[idx].key),
            &payload,
            inputs[idx].key,
            blaze_own::testing::bump(),
        )
        .expect("crate records");
        crate_recs.push(rec);
    }

    struct CountingByNeed<'a> {
        recs: &'a [crate::front_load::CrateRecords<'a>],
        opened_dep_a: AtomicUsize,
        opened_dep_b: AtomicUsize,
    }
    impl crate::front_load::OpenByNeed for CountingByNeed<'_> {
        fn open<'s>(&'s self, one: &crate::front_load::Unopened) -> Result<crate::front_load::CrateRecords<'s>, &'static str> {
            match one.world_at {
                1 => {
                    self.opened_dep_a.fetch_add(1, Ordering::Relaxed);
                }
                2 => {
                    self.opened_dep_b.fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
            self.recs.get(one.world_at).copied().ok_or("missing test crate")
        }
    }

    let io = Io::simulated(SimWorld::new(73, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("root cx");
    let fs = io.need_fs().expect("fs");
    let root = fs.root();
    let frame = cx.frame();
    let heap = blaze_own::testing::heap();

    let own = super::Crate { records: crate_recs[0], name: "app", package: "/", world_at: 0 };
    let dep_a_crate = super::Crate { records: crate_recs[1], name: "dep_a", package: "/", world_at: 1 };
    let dep_b_crate = super::Crate { records: crate_recs[2], name: "dep_b", package: "/", world_at: 2 };
    let opened_slice = [own, dep_a_crate, dep_b_crate];

    let unopened = [
        crate::front_load::Unopened {
            input: input(1),
            key: inputs[1].key,
            world_at: 1,
            name: "dep_a",
            package: "/",
        },
        crate::front_load::Unopened {
            input: input(2),
            key: inputs[2].key,
            world_at: 2,
            name: "dep_b",
            package: "/",
        },
    ];
    let counter = CountingByNeed {
        recs: &crate_recs,
        opened_dep_a: AtomicUsize::new(0),
        opened_dep_b: AtomicUsize::new(0),
    };

    let empty_packs = crate::job_texts::Packs::Open(&[]);
    let by_need_scope = super::ScopeCrates::ByNeed(super::ByNeedScope {
        own,
        rest: &unopened,
        records: &counter,
    });
    let opened_by_need = super::names_records_envs(
        &cx,
        &root,
        by_need_scope,
        (empty_packs, None),
        &[],
        crate::job_texts::Procs::None,
        std::vec::Vec::new(),
        super::Envs { owner: None, rest: None },
        None,
        None,
        frame,
        heap.alloc(),
    )
    .expect("by_need names records");

    // Setup must open ZERO dependencies.
    assert_eq!(counter.opened_dep_a.load(Ordering::Relaxed), 0, "dep_a must not open at setup");
    assert_eq!(counter.opened_dep_b.load(Ordering::Relaxed), 0, "dep_b must not open at setup");

    // Querying the owner crate must still open ZERO dependencies.
    let app_root_by_need = opened_by_need.records.root_module(0);
    assert!(app_root_by_need.is_some(), "owner root module must answer");
    assert_eq!(counter.opened_dep_a.load(Ordering::Relaxed), 0, "owner query must not open dep_a");
    assert_eq!(counter.opened_dep_b.load(Ordering::Relaxed), 0, "owner query must not open dep_b");

    // Querying dep_a opens dep_a once and leaves dep_b completely untouched.
    let dep_a_root_by_need = opened_by_need.records.root_module(1);
    assert!(dep_a_root_by_need.is_some(), "dep_a root module must answer");
    assert_eq!(counter.opened_dep_a.load(Ordering::Relaxed), 1, "dep_a opens once on first query");
    assert_eq!(counter.opened_dep_b.load(Ordering::Relaxed), 0, "untouched dep_b must stay closed");

    // Compare against ScopeCrates::Opened oracle: answers must match identically.
    let opened_eager = super::names_records_envs(
        &cx,
        &root,
        super::ScopeCrates::Opened(&opened_slice),
        (empty_packs, None),
        &[],
        crate::job_texts::Procs::None,
        std::vec::Vec::new(),
        super::Envs { owner: None, rest: None },
        None,
        None,
        frame,
        heap.alloc(),
    )
    .expect("eager names records");

    assert_eq!(opened_by_need.records.crate_count(), opened_eager.records.crate_count());
    assert_eq!(app_root_by_need, opened_eager.records.root_module(0));
    assert_eq!(dep_a_root_by_need, opened_eager.records.root_module(1));
    assert_eq!(counter.opened_dep_b.load(Ordering::Relaxed), 0, "dep_b still never opened");
}
