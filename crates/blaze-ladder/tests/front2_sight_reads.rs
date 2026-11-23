//! WHAT A BODY'S READ LIST KEEPS OF ITS IMPL QUESTIONS: THE BUCKETS OF THE CRATES IT SEES
//! (front2/buckets.rs `Sight::sees_read`, reads.rs `facts_with_buckets`; lane RF13, 2026-11-18).
//!
//! A body of crate A takes no impl of a crate downstream of A (`Seen::From`), so a bucket of
//! such a crate cannot move its answer: it is no read of the body. Two crates: the toolchain's
//! `core` and the program's crate (index 0), which links core.
//! - before any trait question of core's bodies, no sight of core is made: refused by name
//!   (`Unseen::NotMade`), never the whole program's in its place;
//! - core's sight sees core and not the program's crate; its read rows keep core's buckets and
//!   the whole graph's fact (`krate` 0), and drop the program's crate's;
//! - the program's sight sees both;
//! - the control (rule 23): with `Buckets::plant_whole_program_sight` (the cause removed) core's
//!   sight keeps the program's crate's row again.
//!
//! Expected line:
//! ```text
//! sight reads: core keeps 2 of 3 rows, the program 3 of 3, planted 3 of 3
//! ```
//! It needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::buckets::{BucketRead, Buckets, Seen, Unseen};
use blaze_ladder::front2::source::NamesSource;
use blaze_mem::Frame;
use blaze_own::Erased;

const MAIN: &str = "pub struct Mine;

impl PartialEq<Mine> for u32 {
    fn eq(&self, _: &Mine) -> bool {
        true
    }
}

fn main() {
    let _ = 1u32 == Mine;
}
";

/// The first 16 bytes of a crate's key, big-endian: `BucketRead::krate`'s form.
fn id(key: blaze_id::CrateItemKey) -> u128 {
    let mut first = [0u8; 16];
    first.copy_from_slice(&key.0.0[..16]);
    u128::from_be_bytes(first)
}

#[test]
fn a_read_list_keeps_the_buckets_of_the_crates_its_body_sees() {
    let frame: &'static Frame = Box::leak(Box::new(Frame::with_reserve(32 << 30, blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let a = erased.handle();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/probe/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 is an edition");
    let user = UserCrate { name: "probe", dir: "/probe/", root: "/probe/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let core = crates.iter().position(|c| c.name == "core").expect("the toolchain's `core`");
    let program = 0usize;
    let core_id = id((&names).crate_key(core).expect("core's key"));
    let program_id = id((&names).crate_key(program).expect("the program's key"));
    // One read of each: the whole graph's fact, a bucket of core, a bucket of the program's crate.
    let rows = [BucketRead { krate: 0, bucket: 2, answer: 7 }, BucketRead { krate: core_id, bucket: 11, answer: 7 }, BucketRead { krate: program_id, bucket: 11, answer: 7 }];
    let kept = |b: &Buckets, seen: Seen| -> usize {
        let sight = b.sight_made(seen).unwrap_or_else(|e| panic!("{seen:?}: the sight was made: {e}"));
        rows.iter().filter(|r| sight.sees_read(r)).count()
    };

    let b = Buckets::lazy(&names);
    assert!(
        matches!(b.sight_made(Seen::From(core)), Err(Unseen::NotMade { krate }) if krate == core),
        "before any trait question of core's bodies, core's sight is refused by name, never the program's"
    );
    let sight = b.sight(&names, Seen::From(core), a).unwrap_or_else(|e| panic!("core's sight: {e}"));
    assert!(sight.sees(Some(core)) && !sight.sees(Some(program)), "core sees core and not the program's crate downstream of it");
    let core_kept = kept(&b, Seen::From(core));
    assert!(rows.iter().filter(|r| r.krate != program_id).all(|r| b.sight_made(Seen::From(core)).is_ok_and(|s| s.sees_read(r))), "core keeps the whole graph's fact and its own bucket");
    assert_eq!(core_kept, 2, "core's read rows drop the program's crate's bucket alone");
    let sight = b.sight(&names, Seen::From(program), a).unwrap_or_else(|e| panic!("the program's sight: {e}"));
    assert!(sight.sees(Some(core)) && sight.sees(Some(program)), "the program sees core and itself");
    let program_kept = kept(&b, Seen::From(program));
    assert_eq!(program_kept, 3, "the program's read rows keep every row");
    assert_eq!(kept(&b, Seen::Program), 3, "a ground question's rows are the whole program's");

    // The control: the cause removed, core's rows keep the program's crate's bucket again.
    let mut planted = Buckets::lazy(&names);
    planted.plant_whole_program_sight();
    let _ = planted.sight(&names, Seen::From(core), a).unwrap_or_else(|e| panic!("core's planted sight: {e}"));
    let planted_kept = kept(&planted, Seen::From(core));
    assert_eq!(planted_kept, 3, "the control: with every crate seen, core keeps the program's crate's row (else the sight is not what dropped it)");
    println!("sight reads: core keeps {core_kept} of 3 rows, the program {program_kept} of 3, planted {planted_kept} of 3");
}
