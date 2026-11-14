//! The certificate's record: its parts' bytes, its keys and the local evidence beside it.
//! The check and the write are tested against a model in `request/tests.rs`.

use blaze_sim::{Config, Tape};

use super::*;

fn id(n: u8) -> ContentId {
    ContentId::of("test certificate", &[n])
}

fn reads() -> Vec<(Read, Digest)> {
    vec![
        (Read::Outside("/usr/include/z.h".to_owned()), Digest::Nothing),
        (Read::Env("CC".to_owned()), Digest::Is(id(1))),
        (Read::Node("crates/a".to_owned()), Digest::Is(id(2))),
        (Read::Drawn, Digest::Is(id(3))),
        (Read::Listing("/opt/lib".to_owned()), Digest::Is(id(4))),
        (Read::Entry("/opt/lib/crtn.o".to_owned()), Digest::Is(id(5))),
        (Read::Node(String::new()), Digest::Is(id(9))),
    ]
}

fn answer() -> Answer {
    Answer {
        outputs: vec![
            Output { base: 1, path: "debug/b".to_owned(), content: id(5) },
            Output { base: 0, path: "debug/a".to_owned(), content: id(6) },
        ],
        units: vec![id(7), id(8)],
        said: b"said".to_vec(),
    }
}

fn shape() -> Shape {
    Shape { plan: id(20), config: id(21), request: id(22) }
}

/// Rule 26: an answer's bytes read back are the answer, and encode to the same bytes.
#[test]
fn an_answer_round_trips_with_identical_bytes() {
    let certificate = Certificate::new(reads(), answer()).unwrap();
    let bytes = certificate.answer().encode();
    let back = Answer::decode(&bytes).unwrap();
    assert_eq!(&back, certificate.answer());
    assert_eq!(back.encode(), bytes);
    assert_eq!(Answer::decode(&Answer::default().encode()).unwrap(), Answer::default());
}

/// Rule 26: an answer and a list of a layout before this one are refused by that layout's
/// name, one of a layout not known by its number, other bytes as no certificate; none is
/// read as an empty record.
#[test]
fn another_layout_is_refused_by_name() {
    let bytes = Certificate::new(reads(), answer()).unwrap().answer().encode();
    for (layout, _) in PREVIOUS {
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&layout.to_le_bytes());
        match Answer::decode(&old) {
            Err(why) => {
                assert!(matches!(why, Undecoded::Previous(n, _) if n == *layout), "{why:?}");
                assert!(why.to_string().contains(&format!("layout {layout} (")), "{why}");
            }
            Ok(read) => panic!("an answer of layout {layout} was read: {read:?}"),
        }
        let base = base(&shape(), b"blaze 1");
        let mut list = Vec::new();
        blaze_record::write_list(&mut list, &base, &head_of(*layout), [Read::Drawn.name()]);
        let opened = ListRef::open(&list, &base).unwrap();
        assert_eq!(reads_of(opened), Err(Undecoded::Previous(*layout, PREVIOUS[*layout as usize].1)));
    }
    let mut later = bytes.clone();
    later[8..12].copy_from_slice(&77_u32.to_le_bytes());
    assert_eq!(Answer::decode(&later), Err(Undecoded::Unknown(77)));
    assert_eq!(Answer::decode(b"not a certificate"), Err(Undecoded::NotOne));
    assert_eq!(Answer::decode(b""), Err(Undecoded::NotOne));
}

/// Every cut of an answer's bytes, and bytes past its end, are refused at a field.
#[test]
fn every_cut_is_refused() {
    let mut bytes = Certificate::new(reads(), answer()).unwrap().answer().encode();
    for cut in 0..bytes.len() {
        assert!(Answer::decode(&bytes[..cut]).is_err(), "a cut at {cut} was read");
    }
    bytes.push(0);
    assert_eq!(Answer::decode(&bytes), Err(Undecoded::Malformed("the end")));
}

/// Rule 25: equal builds are equal bytes whatever order their steps read and placed in: the
/// list names the reads sorted, each once, with no value; the outputs are sorted.
#[test]
fn equal_builds_are_equal_bytes_in_any_order() {
    let base = base(&shape(), b"blaze 1");
    let one = Certificate::new(reads(), answer()).unwrap();
    let mut turned = reads();
    turned.reverse();
    turned.push(turned[0].clone());
    let mut placed = answer();
    placed.outputs.reverse();
    let two = Certificate::new(turned, placed).unwrap();
    let list = one.list(&base);
    assert_eq!(list, two.list(&base));
    assert_eq!(one.answer_key(&list), two.answer_key(&list));
    assert_eq!(one.answer().encode(), two.answer().encode());
    let named = reads_of(ListRef::open(&list, &base).unwrap()).unwrap();
    let mut sorted: Vec<Read> = reads().into_iter().map(|(read, _)| read).collect();
    sorted.sort();
    assert_eq!(named, sorted);
    // A value is in the answer's key, not in the list.
    let mut moved = reads();
    moved[1].1 = Digest::Is(id(99));
    let three = Certificate::new(moved, answer()).unwrap();
    assert_eq!(three.list(&base), list);
    assert_ne!(three.answer_key(&list), one.answer_key(&list));
    let mut gone = reads();
    gone[1].1 = Digest::Nothing;
    let four = Certificate::new(gone, answer()).unwrap();
    assert_ne!(four.answer_key(&list), one.answer_key(&list));
}

/// A read seen at two values has no one digest: that build has no certificate, by the
/// read's name.
#[test]
fn a_read_at_two_values_is_refused_by_name() {
    let mut twice = reads();
    twice.push((Read::Env("CC".to_owned()), Digest::Nothing));
    assert_eq!(Certificate::new(twice, answer()), Err(Read::Env("CC".to_owned())));
}

/// The base is the request's shape and the tool: each part of either gives another base.
/// The tree's content is no parameter of it.
#[test]
fn the_base_is_the_shape_and_the_tool() {
    let one = base(&shape(), b"blaze 1");
    assert_eq!(one, base(&shape(), b"blaze 1"));
    assert_ne!(one, base(&shape(), b"blaze 2"));
    assert_ne!(one, base(&Shape { plan: id(0), ..shape() }, b"blaze 1"));
    assert_ne!(one, base(&Shape { config: id(0), ..shape() }, b"blaze 1"));
    assert_ne!(one, base(&Shape { request: id(0), ..shape() }, b"blaze 1"));
}

/// A read's name reads back as the read; other bytes are refused.
#[test]
fn a_read_round_trips_by_name() {
    for (read, _) in reads() {
        assert_eq!(Read::named(&read.name()), Ok(read));
    }
    assert!(Read::named(b"").is_err());
    assert!(Read::named(b"\x09x").is_err());
    assert!(Read::named(b"\x04x").is_err());
    assert!(Read::named(b"\x00\xff").is_err());
}

/// Two checkouts of one content make one text of each path, and each gets its own back.
#[test]
fn the_checkout_is_relocated_and_put_back() {
    let (a, b) = ("/home/a/checkout", "/srv/b");
    let text = |root: &str| format!("{root}/target --cfg=x:{root} \"{root}/src\" PATH={root}/bin:/usr/bin {root}");
    assert_eq!(relocated(&text(a), a), relocated(&text(b), b));
    assert!(!relocated(&text(a), a).contains(a));
    // A longer name that begins as the checkout's does is another directory.
    assert_eq!(relocated("/home/a/checkout-other/x", a), "/home/a/checkout-other/x");
    assert_eq!(relocated("in/home/a/checkout/x", a), "in/home/a/checkout/x");
    let said = format!("{a}/target/debug/app");
    assert_eq!(replaced(&relocated(&said, a), b), "/srv/b/target/debug/app");
    assert_eq!(relocated("x", ""), "x");
}

const SECOND: u64 = 1_000_000_000;

/// A regular file's stamp of `size` bytes last changed at `changed_ns`, on inode 7.
fn file(size: u64, changed_ns: u64) -> Stamp {
    Stamp { ino: 7, size, mtime_ns: changed_ns, ctime_ns: changed_ns, mode: blaze_core::S_IFREG | 0o644 }
}

/// The invariant: a stamp vouches for the content hashed under it only when it was looked at
/// its racy window (`blaze_vfs::window_ns`, asked of `blaze_vfs::racy`) past its change; a
/// closer look keeps nothing and drops what was kept. Control: the same keep a nanosecond
/// later vouches, so the refusal is the window's and not the keep's.
#[test]
fn a_stamp_seen_too_close_to_its_change_does_not_vouch() {
    let mut evidence = Evidence::default();
    // A whole second: the coarse window. One past it: the fine one.
    for changed_ns in [10 * SECOND, 10 * SECOND + 1] {
        let stamp = file(4, changed_ns);
        let window = blaze_vfs::window_ns(&stamp);
        assert_eq!(window, if changed_ns % SECOND == 0 { blaze_vfs::COARSE_NS } else { blaze_vfs::FINE_NS });
        evidence.keep(0, "debug/a", stamp, id(1), changed_ns + window - 1);
        assert_eq!(evidence.vouched(0, "debug/a", &stamp), None);
        evidence.keep(0, "debug/a", stamp, id(1), changed_ns + window);
        assert_eq!(evidence.vouched(0, "debug/a", &stamp), Some(id(1)), "the control");
        assert_eq!(evidence.vouched(1, "debug/a", &stamp), None);
        assert_eq!(evidence.vouched(0, "debug/a", &file(5, changed_ns)), None);
        let later = file(4, changed_ns + SECOND);
        evidence.keep(0, "debug/a", later, id(2), later.mtime_ns);
        assert_eq!(evidence.vouched(0, "debug/a", &stamp), None);
        assert_eq!(evidence.vouched(0, "debug/a", &later), None);
    }
    // The bound: only the last answer's outputs stay.
    let stamp = file(4, 10 * SECOND);
    evidence.keep(0, "debug/a", stamp, id(1), u64::MAX);
    evidence.keep(1, "debug/b", stamp, id(1), u64::MAX);
    evidence.only(&[Output { base: 1, path: "debug/b".to_owned(), content: id(1) }]);
    assert_eq!(evidence.vouched(0, "debug/a", &stamp), None);
    assert_eq!(evidence.vouched(1, "debug/b", &stamp), Some(id(1)));
    evidence.forget(1, "debug/b");
    assert_eq!(evidence.vouched(1, "debug/b", &stamp), None);
}

/// The invariant: a stamp vouches only when it is the kept one whole, every field: a file
/// whose inode, size, change time (`chmod`, a rename onto it, a hard link), modified time or
/// mode moved vouches for nothing. Control: the kept stamp itself vouches; the stamp that
/// differs only in its change time is equal in the size and modified time the old stamp
/// compared, the planted bug this refuses.
#[test]
fn a_stamp_that_differs_only_in_ctime_vouches_for_nothing() {
    let mut evidence = Evidence::default();
    let stamp = file(4, 10 * SECOND);
    evidence.keep(0, "debug/a", stamp, id(1), u64::MAX);
    assert_eq!(evidence.vouched(0, "debug/a", &stamp), Some(id(1)), "the control");
    let ctime = Stamp { ctime_ns: stamp.ctime_ns + 1, ..stamp };
    assert_eq!((ctime.size, ctime.mtime_ns), (stamp.size, stamp.mtime_ns));
    assert_eq!(evidence.vouched(0, "debug/a", &ctime), None);
    for moved in [
        Stamp { ino: 8, ..stamp },
        Stamp { size: 5, ..stamp },
        Stamp { mtime_ns: stamp.mtime_ns + 1, ..stamp },
        Stamp { mode: blaze_core::S_IFREG | 0o755, ..stamp },
    ] {
        assert_eq!(evidence.vouched(0, "debug/a", &moved), None, "{moved:?}");
    }
}

/// What a test does to the evidence and to the files it is of.
#[derive(Clone, Copy, Debug)]
enum Op {
    /// The clock moves on this many milliseconds.
    Tick(u16),
    /// The file `.0` is written with content `.1`: its stamp is the clock to the whole second
    /// (a coarse file system), so two writes in one second leave one stamp.
    Write(u8, u8),
    /// The file `.0` is looked at and its content kept under its stamp now.
    Look(u8),
    Forget(u8),
    /// What changed is written as marks, unsynced.
    Drain,
    Sync,
    /// The machine stops: of the marks not synced, this many of the first stay (a torn
    /// suffix); the next start seeds its evidence from the marks that stayed.
    Crash(u8),
}

fn draw_op(tape: &Tape) -> Op {
    let at = tape.int_in(0..=2u8, "file");
    match tape.int_in(0..=6u8, "op") {
        0 => Op::Tick(tape.int_in(0..=3_000u16, "tick ms")),
        1 => Op::Write(at, tape.int_in(0..=3u8, "content")),
        2 => Op::Look(at),
        3 => Op::Forget(at),
        4 => Op::Drain,
        5 => Op::Sync,
        _ => Op::Crash(tape.int_in(0..=8u8, "marks that stay")),
    }
}

/// The path of file `at`.
fn path_of(at: u8) -> String {
    format!("/w/target/debug/out{at}")
}

/// A placed output's mark as the driver decodes it: its stamp, when that was looked at, its
/// content.
type Mark = (Stamp, u64, ContentId);

/// The marks `marks` (by output path) as [`Evidence::seeded`] takes them.
fn placed(marks: &std::collections::BTreeMap<String, Mark>) -> impl Iterator<Item = (&str, Stamp, u64, ContentId)> {
    marks.iter().map(|(path, (stamp, looked_ns, content))| (path.as_str(), *stamp, *looked_ns, *content))
}

/// Runs `ops` against model files, an evidence and the marks it writes, and checks the
/// oracle after each: every content the evidence (the live one, or the one a start seeded
/// from the marks that survived) vouches for at a file's stamp now is that file's content.
/// `plant`: the evidence keeps every look, racy or not ([`Evidence::keep_racy`]).
fn simulate(ops: &[Op], plant: bool) -> Result<(), String> {
    use std::collections::BTreeMap;

    let mut clock_ns = 100 * SECOND;
    // Each file: its stamp and the content it holds.
    let mut files: BTreeMap<u8, (Stamp, ContentId)> = BTreeMap::new();
    let mut evidence = Evidence::default();
    // The marks, by output path, as the journal holds them decoded (the driver's own layout,
    // blaze-cargo's `serve::local::Placed`): those synced, and the log since, in order.
    let mut synced: BTreeMap<String, Mark> = BTreeMap::new();
    let mut log: Vec<(String, Option<Mark>)> = Vec::new();
    for (step, op) in ops.iter().enumerate() {
        match *op {
            Op::Tick(ms) => clock_ns += u64::from(ms) * 1_000_000,
            Op::Write(at, content) => {
                let changed_ns = clock_ns - clock_ns % SECOND;
                files.insert(at, (file(1, changed_ns), id(content)));
            }
            Op::Look(at) => {
                let Some(&(stamp, content)) = files.get(&at) else { continue };
                match plant {
                    true => evidence.keep_racy(0, &path_of(at), stamp, content),
                    false => evidence.keep(0, &path_of(at), stamp, content, clock_ns),
                }
            }
            Op::Forget(at) => evidence.forget(0, &path_of(at)),
            Op::Drain => evidence.drain_changed(|base, path, kept| {
                assert_eq!(base, 0);
                log.push((path.to_owned(), kept.map(|(stamp, looked_ns, content)| (*stamp, looked_ns, *content))));
            }),
            Op::Sync => {
                for (name, value) in log.drain(..) {
                    match value {
                        Some(value) => synced.insert(name, value),
                        None => synced.remove(&name),
                    };
                }
            }
            Op::Crash(stay) => {
                log.truncate(usize::from(stay));
                for (name, value) in log.drain(..) {
                    match value {
                        Some(value) => synced.insert(name, value),
                        None => synced.remove(&name),
                    };
                }
                evidence = Evidence::seeded(0, placed(&synced));
            }
        }
        for (&at, (stamp, content)) in &files {
            if let Some(vouched) = evidence.vouched(0, &path_of(at), stamp)
                && vouched != *content
            {
                return Err(format!(
                    "after step {step} ({op:?}): {} vouched for {vouched:?} at {stamp:?}, and holds {content:?}",
                    path_of(at)
                ));
            }
        }
    }
    Ok(())
}

/// The invariant (rule 43): through any keeps, forgets, drains, syncs and crashes that tear
/// the unsynced marks, the evidence (live, or seeded at a start from the marks that stayed)
/// gives an output's content only under the stamp that content was looked at under, steady:
/// never a content the file does not hold. Control: the planted bug of keeping racy looks
/// (`Evidence::keep_racy`) vouches for a file's old content after a second write in the same
/// second, and across a crash and start, and the oracle catches both.
#[test]
fn marks_through_crashes_vouch_only_for_what_the_files_hold() {
    blaze_sim::check(Config::DEFAULT, |tape| {
        let count = tape.int_in(0..=40usize, "ops");
        let ops: Vec<Op> = (0..count).map(|_| draw_op(tape)).collect();
        if let Err(why) = simulate(&ops, false) {
            panic!("{why}\n{ops:?}");
        }
        let rewritten = [Op::Write(0, 1), Op::Tick(100), Op::Look(0), Op::Tick(100), Op::Write(0, 2)];
        assert!(simulate(&rewritten, true).is_err(), "the control");
        assert!(simulate(&rewritten, false).is_ok());
        // The racy mark synced, the machine stopped and started, the file written again in the
        // second it was first written: the seeded evidence of the plant vouches for the old.
        let restarted =
            [Op::Write(0, 1), Op::Tick(100), Op::Look(0), Op::Drain, Op::Sync, Op::Crash(0), Op::Write(0, 2)];
        assert!(simulate(&restarted, true).is_err(), "the control, across a start");
        assert!(simulate(&restarted, false).is_ok());
    });
}

/// The invariant (rule 26): evidence seeded from the marks an evidence's changes were
/// drained into vouches as that evidence does, for every output and stamp, and its own
/// marks, drained after a keep of everything again, are the same bytes; the seeding changes
/// nothing (a drain after it hands nothing). Control: a mark kept inside its racy window
/// with the planted `Evidence::keep_racy` is seeded as steady, but the real keep writes no
/// such mark. (A mark of another layout is the driver's to refuse by name before seeding:
/// blaze-cargo's `serve::local::Placed::decode`.)
#[test]
fn evidence_seeded_from_marks_vouches_as_the_evidence_that_wrote_them() {
    blaze_sim::check(Config::DEFAULT, evidence_seeded_from_marks_vouches_as_the_evidence_that_wrote_them_case);
}

fn evidence_seeded_from_marks_vouches_as_the_evidence_that_wrote_them_case(tape: &Tape) {
    use std::collections::BTreeMap;

    let mut evidence = Evidence::default();
    let mut stamps: Vec<(String, Stamp)> = Vec::new();
    for at in 0..tape.int_in(0..=6u8, "outputs") {
        let path = path_of(at);
        let changed_ns = tape.int_in(0..=1u64 << 40, "changed ns");
        let stamp = file(tape.int::<u64>("size"), changed_ns);
        let looked_ns = changed_ns + tape.int_in(0..=3 * SECOND, "looked after ns");
        evidence.keep(0, &path, stamp, id(at), looked_ns);
        stamps.push((path, stamp));
    }
    let mut marks: BTreeMap<String, Mark> = BTreeMap::new();
    let drain = |evidence: &mut Evidence, marks: &mut BTreeMap<String, Mark>| {
        evidence.drain_changed(|_, path, kept| {
            match kept {
                Some((stamp, looked_ns, content)) => marks.insert(path.to_owned(), (*stamp, looked_ns, *content)),
                None => marks.remove(path),
            };
        });
    };
    drain(&mut evidence, &mut marks);
    let mut seeded = Evidence::seeded(0, placed(&marks));
    for (path, stamp) in &stamps {
        assert_eq!(seeded.vouched(0, path, stamp), evidence.vouched(0, path, stamp), "{path}");
    }
    let mut again = BTreeMap::new();
    drain(&mut seeded, &mut again);
    assert!(again.is_empty(), "a seeding changes no mark");

    // The control: the racy keep the real one refuses.
    let stamp = file(1, 10 * SECOND);
    let mut real = Evidence::default();
    real.keep(0, "/w/racy", stamp, id(9), 10 * SECOND);
    let mut planted = Evidence::default();
    planted.keep_racy(0, "/w/racy", stamp, id(9));
    let (mut real_marks, mut planted_marks) = (BTreeMap::new(), BTreeMap::new());
    drain(&mut real, &mut real_marks);
    drain(&mut planted, &mut planted_marks);
    assert!(real_marks.is_empty());
    let from_planted = Evidence::seeded(0, placed(&planted_marks));
    assert_eq!(from_planted.vouched(0, "/w/racy", &stamp), Some(id(9)), "the control");
}
