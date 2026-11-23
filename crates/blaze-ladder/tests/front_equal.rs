//! ONE FRONT END (docs/v2/plan/design/one-front-end.md): the whole program (`Front2Program`,
//! every fn `main` reaches tabled by a census) and the lazy one (`LazyProgram`, one body at a
//! time, D106) answer every question the record walk asks alike, and build byte-equal code
//! records (rule 78: one seam, each strategy tested equal).
//!
//! Each program (every `x0-*` twin rung, then front_equal/fixtures.rs) is solved ONCE; both
//! front ends are made over the same names, each behind the recording wrapper
//! (front_equal/asked.rs), and each is walked by the same record walk with the same maker
//! (front_equal/over.rs) and builder into its own store. Then:
//! - every question both were asked is compared by its LAST answer (a lazy row's answers can
//!   change as rows are made; the count of questions whose answer changed is printed);
//! - every walk row both made is compared by its code record's key and bytes.
//!
//! Lines (with `--show-output`), per program, then the totals:
//!   "<program>: asked: whole W, lazy L, common C, differing D; changed whole X, lazy Y;
//!    code records: both B, equal E, differing F, only whole OW, only lazy OL; <walks>"
//!   "<program>: <question> <key>: whole <answer> / lazy <answer>"   (its first difference)
//!   "asked: whole W, lazy L, common C, differing D (floor 0)"
//!   "code records: both built B, equal E, differing F (floor 0); only whole OW, only lazy OL"
//!   "programs: P built by both, G with a gap, S skipped"
//! A GAP is one front end refusing what the other built (the doc's gap list); a program whose
//! names do not solve over no toolchain (a twin on `std`) is SKIPPED.
//!
//! THE CONTROL (rule 23), `a_planted_abi_is_caught`: the lazy side's `fn_abi` answer for one
//! key is flipped; the comparison must see that very question differ.

#![feature(allocator_api)]

#[path = "front_equal/asked.rs"]
mod asked;
#[path = "front_equal/fixtures.rs"]
mod fixtures;
#[path = "front_equal/over.rs"]
mod over;
#[path = "front_equal/shown.rs"]
mod shown;
mod rungs;

use core::cmp::Ordering;
use core::fmt::{Display, Write as _};

use blaze_build::OverflowChecks;
use blaze_emit::{Isa, Os};
use blaze_ids::Hasher;
use blaze_ids::producer::{CodeBuilder, ProducerId, Producers};
use blaze_ladder::body_ids::{BodyId, BodyIds, Read, Walk};
use blaze_ladder::code_records::{Builder, CodeStore, MemoryStore, Settings, TierName};
use blaze_ladder::crates::{Inputs, Toolchain};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::program::lazy::LazyProgram;
use blaze_ladder::program_texts::ProgramTexts;
use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_ladder::run::make_one::walk_records;
use blaze_ladder::seam::FrontEnd;
use blaze_mem::Frame;
use blaze_own::{AString, AVec, Alloc, Erased};
use blaze_toolchain::ToolchainId;
use blaze_typeck::ItemKey;

use asked::{Asked, Entry};
use over::Over;

const WRITES: &str = "a line writes into the test's frame";

/// The test's one allocator: a frame, its memory from the OS, kept for the process (the
/// record walk takes `Alloc<'static>`; tests/front2_cast_infer.rs makes it so).
struct World {
    frame: &'static Frame,
    a: Alloc<'static>,
}

fn world() -> World {
    let frame: &'static Frame = Box::leak(Box::new(Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    World { frame, a: erased.handle() }
}

fn builder(overflow: OverflowChecks) -> Builder {
    Builder {
        isa: Isa { lse: true, os: Os::MacOs },
        tier: TierName::Reference,
        settings: Settings::baseline(overflow, blaze_build::Panic::Unwind),
        toolchain: ToolchainId([7; 32]),
        producer: producer(),
        edition: edition::PackageEdition::of_manifest("2024").expect("2024 is an edition"),
    }
}

fn producer() -> ProducerId<CodeBuilder> {
    Producers::for_tests(b"front equal").get::<CodeBuilder>()
}

const READS: [Read; 1] = [Read { fact: [3; 32], digest: [4; 32] }];

/// A stand-in for the command's BodyCheck keys that answers for every item (as
/// tests/front2_cast_infer.rs `EveryItem`): the same for both programs.
struct EveryItem;

impl BodyIds for EveryItem {
    fn body_check(&self, item: ItemKey) -> Option<BodyId<'_>> {
        Some(BodyId { key: Hasher::new("front equal every item").part(&item.0.to_le_bytes()[..]).finish(), reads: &READS })
    }

    fn today(&self, fact: &[u8; 32]) -> Option<[u8; 32]> {
        (*fact == READS[0].fact).then_some(READS[0].digest)
    }

    fn crate_fact(&self, _item: ItemKey) -> Option<Read> {
        Some(Read { fact: [8; 32], digest: [9; 32] })
    }
}

/// Whether the lazy side's `fn_abi` is planted wrong (the control).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Plant {
    No,
    /// Flip the answer for the first fn the whole side's walk asked the ABI of.
    FirstAbi,
}

#[derive(Default, Clone, Copy)]
struct Tally {
    whole: usize,
    lazy: usize,
    common: usize,
    differing: usize,
    changed_whole: usize,
    changed_lazy: usize,
    /// The planted question was among the differing ones.
    planted_seen: bool,
}

#[derive(Default, Clone, Copy)]
struct Bytes {
    both: usize,
    equal: usize,
    differing: usize,
    only_whole: usize,
    only_lazy: usize,
}

#[derive(Default)]
struct Outcome {
    tally: Tally,
    bytes: Bytes,
    built_both: bool,
    gap: bool,
    skipped: bool,
    planted: Option<ItemKey>,
}

#[derive(Default)]
struct Total {
    tally: Tally,
    bytes: Bytes,
    built_both: usize,
    gaps: usize,
    skipped: usize,
}

impl Total {
    fn add(&mut self, o: &Outcome) {
        let (t, b) = (&mut self.tally, &mut self.bytes);
        t.whole += o.tally.whole;
        t.lazy += o.tally.lazy;
        t.common += o.tally.common;
        t.differing += o.tally.differing;
        b.both += o.bytes.both;
        b.equal += o.bytes.equal;
        b.differing += o.bytes.differing;
        b.only_whole += o.bytes.only_whole;
        b.only_lazy += o.bytes.only_lazy;
        self.built_both += usize::from(o.built_both);
        self.gaps += usize::from(o.gap);
        self.skipped += usize::from(o.skipped);
    }
}

/// `v`'s text, its first line only (a refusal's later lines are its detail).
fn first_line<'a>(a: Alloc<'a>, v: &impl Display) -> AString<'a> {
    let mut all = AString::new_in(a);
    write!(all, "{v}").expect(WRITES);
    let mut first = AString::new_in(a);
    first.push_str(all.as_str().lines().next().unwrap_or(""));
    first
}

/// Each question's last answer: the log's indices, one per question, sorted by question; and
/// how many questions were given more than one answer.
fn last_answers(log: &[Entry<'_>], a: Alloc<'static>) -> (AVec<'static, usize>, usize) {
    let mut order: AVec<'static, usize> = AVec::new_in(a);
    order.extend(0..log.len());
    // Unstable is enough: the index breaks every tie, so the order is total (and the sort
    // allocates nothing).
    order.sort_unstable_by(|&x, &y| log[x].question.as_str().cmp(log[y].question.as_str()).then(x.cmp(&y)));
    let mut last: AVec<'static, usize> = AVec::new_in(a);
    let mut changed = 0;
    let mut n = 0;
    while n < order.len() {
        let q = log[order[n]].question.as_str();
        let mut end = n + 1;
        while end < order.len() && log[order[end]].question.as_str() == q {
            end += 1;
        }
        let kept = order[end - 1];
        if order[n..end].iter().any(|&i| log[i].answer.as_str() != log[kept].answer.as_str()) {
            changed += 1;
        }
        last.push(kept);
        n = end;
    }
    (last, changed)
}

/// Every question both logs hold, compared by its last answer; the first difference printed
/// and kept. `planted`: the question the control flipped (empty with none).
fn compare_logs(program: &str, whole: &[Entry<'_>], lazy: &[Entry<'_>], planted: &str, a: Alloc<'static>, firsts: &mut AVec<'static, AString<'static>>) -> Tally {
    let (w, changed_whole) = last_answers(whole, a);
    let (l, changed_lazy) = last_answers(lazy, a);
    let mut t = Tally { whole: w.len(), lazy: l.len(), changed_whole, changed_lazy, ..Tally::default() };
    let (mut i, mut j) = (0, 0);
    while i < w.len() && j < l.len() {
        let (we, le) = (&whole[w[i]], &lazy[l[j]]);
        match we.question.as_str().cmp(le.question.as_str()) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                t.common += 1;
                if we.answer.as_str() != le.answer.as_str() {
                    t.differing += 1;
                    t.planted_seen |= !planted.is_empty() && we.question.as_str() == planted;
                    if t.differing == 1 {
                        let mut line = AString::new_in(a);
                        write!(line, "{program}: {}: whole {} / lazy {}", we.question.as_str(), we.answer.as_str(), le.answer.as_str()).expect(WRITES);
                        println!("{}", line.as_str());
                        firsts.push(line);
                    }
                }
                i += 1;
                j += 1;
            }
        }
    }
    t
}

/// A walk row's name for a line: its bytes as text where they are UTF-8.
fn name_shown(o: &mut AString<'_>, name: &[u8]) -> core::fmt::Result {
    match core::str::from_utf8(name) {
        Ok(s) => o.write_str(s),
        Err(_) => write!(o, "{name:?}"),
    }
}

/// Every row both walks made, by its symbol: its code record's key and bytes.
fn compare_bytes(program: &str, ww: &Walk<'_>, sw: &MemoryStore<'_>, wl: &Walk<'_>, sl: &MemoryStore<'_>, a: Alloc<'static>, firsts: &mut AVec<'static, AString<'static>>) -> Bytes {
    let mut b = Bytes::default();
    let mut matched = 0;
    for r in 0..ww.len() {
        let Some(lr) = wl.row_of(&ww.symbol(r)) else {
            b.only_whole += 1;
            continue;
        };
        matched += 1;
        let (kw, kl) = (ww.key(r), wl.key(lr));
        if kw.is_none() && kl.is_none() {
            continue;
        }
        b.both += 1;
        let same = match (kw, kl) {
            (Some(kw), Some(kl)) => kw == kl && matches!((sw.get(kw), sl.get(kl)), (Ok(Some(x)), Ok(Some(y))) if x == y),
            _ => false,
        };
        if same {
            b.equal += 1;
            continue;
        }
        b.differing += 1;
        if b.differing == 1 {
            let mut line = AString::new_in(a);
            write!(line, "{program}: code record ").expect(WRITES);
            name_shown(&mut line, ww.name(r)).expect(WRITES);
            write!(line, ": whole {kw:?} / lazy {kl:?}").expect(WRITES);
            println!("{}", line.as_str());
            firsts.push(line);
        }
    }
    b.only_lazy = wl.len() - matched;
    b
}

/// One program solved once, made by both front ends, both walked; its line printed.
fn compare_one(w: &World, program: &str, files: &[SourceFile<'_>], edition: &str, overflow: OverflowChecks, plant: Plant, firsts: &mut AVec<'static, AString<'static>>) -> Outcome {
    let a = w.a;
    let mut out = Outcome::default();
    let tc = Toolchain::none(a);
    let new = NewFront { package: rungs::x::PACKAGE, edition, shelf: None, overflow };
    let texts = match ProgramTexts::new(files, rungs::x::ROOT, new, &tc, blaze_build::Panic::Unwind) {
        Ok(texts) => texts,
        Err(e) => {
            println!("{program}: SKIPPED: its texts: {}", first_line(a, &e).as_str());
            out.skipped = true;
            return out;
        }
    };
    let user = texts.user();
    let inputs = match Inputs::new(&user, &tc) {
        Ok(inputs) => inputs,
        Err(e) => {
            println!("{program}: SKIPPED: its crates: {}", first_line(a, &e).as_str());
            out.skipped = true;
            return out;
        }
    };
    let crates = inputs.crates();
    let names = match inputs.resolve(&crates, blaze_own::testing::bump()) {
        Ok(names) => names,
        Err(e) => {
            println!("{program}: SKIPPED: its names over no toolchain: {}", first_line(a, &e).as_str());
            out.skipped = true;
            return out;
        }
    };
    let whole = match Front2Program::new(&names, w.frame, files, a) {
        Ok(whole) => whole,
        Err(e) => {
            println!("{program}: GAP: the whole program refused: {}", first_line(a, &e).as_str());
            out.gap = true;
            return out;
        }
    };
    let Some(entry) = whole.entry().map(|i| whole.function(i).key) else {
        println!("{program}: GAP: the whole program has no entry");
        out.gap = true;
        return out;
    };
    let mut lazy = match LazyProgram::new(&names, w.frame, files, Some(entry), a) {
        Ok(lazy) => lazy,
        Err(e) => {
            println!("{program}: GAP: the lazy program refused: {}", first_line(a, &e).as_str());
            out.gap = true;
            return out;
        }
    };
    if let Err(e) = lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule) {
        println!("{program}: GAP: the lazy program's start: {}", first_line(a, &e).as_str());
        out.gap = true;
        return out;
    }
    let b = builder(overflow);
    let mut whole = Over::new(Asked::new(whole, None, a), &b, a);
    let mut whole_store = MemoryStore::new_in(a);
    let whole_walk = walk_records(entry, &EveryItem, &mut whole_store, &b, &producer(), &mut whole, a);
    let planted = match plant {
        Plant::No => None,
        Plant::FirstAbi => whole.program.first_abi(),
    };
    if plant == Plant::FirstAbi && planted.is_none() {
        println!("{program}: SKIPPED: the whole side asked no fn_abi, so the control has no key here");
        out.skipped = true;
        return out;
    }
    out.planted = planted;
    let mut lazy = Over::new(Asked::new(lazy, planted, a), &b, a);
    let mut lazy_store = MemoryStore::new_in(a);
    let lazy_walk = walk_records(entry, &EveryItem, &mut lazy_store, &b, &producer(), &mut lazy, a);

    let mut want = AString::new_in(a);
    if let Some(k) = planted {
        write!(want, "fn_abi {k:?}").expect(WRITES);
    }
    out.tally = compare_logs(program, whole.program.log().as_slice(), lazy.program.log().as_slice(), want.as_str(), a, firsts);
    let mut walks = AString::new_in(a);
    match (&whole_walk, &lazy_walk) {
        (Ok((ww, cw)), Ok((wl, cl))) => {
            out.built_both = true;
            out.bytes = compare_bytes(program, ww, &whole_store, wl, &lazy_store, a, firsts);
            write!(walks, "built by both (whole {cw}; lazy {cl})")
        }
        (Ok((_, cw)), Err(e)) => {
            out.gap = true;
            write!(walks, "GAP: the lazy walk refused (the whole one built: {cw}): {}", first_line(a, e).as_str())
        }
        (Err(e), Ok((_, cl))) => {
            out.gap = true;
            write!(walks, "GAP: the whole walk refused (the lazy one built: {cl}): {}", first_line(a, e).as_str())
        }
        (Err(ew), Err(el)) => write!(walks, "both refused: whole {} / lazy {}", first_line(a, ew).as_str(), first_line(a, el).as_str()),
    }
    .expect(WRITES);
    let (t, by) = (out.tally, out.bytes);
    println!(
        "{program}: asked: whole {}, lazy {}, common {}, differing {}; changed whole {}, lazy {}; code records: both {}, equal {}, differing {}, only whole {}, only lazy {}; {}",
        t.whole,
        t.lazy,
        t.common,
        t.differing,
        t.changed_whole,
        t.changed_lazy,
        by.both,
        by.equal,
        by.differing,
        by.only_whole,
        by.only_lazy,
        walks.as_str()
    );
    out
}

/// Every `x0-*` twin rung, in name order, with its files, edition and overflow setting.
fn each_twin(w: &World, mut each: impl FnMut(&str, &[SourceFile<'_>], &str, OverflowChecks) -> bool) {
    let front2 = rungs::ladder().join("front2");
    for rung in rungs::x::x_rungs(&front2) {
        if rungs::x::family(&rung) != "twin" {
            continue;
        }
        let dir = front2.join(&rung);
        let files = rungs::sources(&dir);
        let mut views: AVec<'static, SourceFile<'_>> = AVec::new_in(w.a);
        for (path, text) in files.iter() {
            views.push(SourceFile { path: path.as_str(), text: text.as_str() });
        }
        if !each(&rung, views.as_slice(), &rungs::x::edition_of(&dir), rungs::x::overflow_of(&dir)) {
            return;
        }
    }
}

/// Runs `body` on a thread with room for the build's frames (the check and the builder recurse
/// by the program's depth: a test thread's 2 MiB stack overflowed), as
/// crates/blaze-emit/src/reach_tests.rs `on_big_stack` does (256 MiB); a panic of `body` is
/// the test's, its payload resumed unchanged.
fn on_big_stack(body: fn()) {
    let ran = std::thread::Builder::new().stack_size(256 << 20).spawn(body).expect("a thread for the test's body").join();
    if let Err(payload) = ran {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn whole_and_lazy_answer_alike() {
    on_big_stack(answer_alike);
}

#[test]
fn a_static_struct_calls_both_fn_fields_on_both_front_ends() {
    on_big_stack(|| {
        let src = r#"#![feature(no_core, lang_items)]
#![no_core]
#![no_main]
#[lang = "pointee_sized"] trait PointeeSized {}
#[lang = "meta_sized"] trait MetaSized: PointeeSized {}
#[lang = "sized"] trait Sized: MetaSized {}
#[lang = "copy"] trait Copy {}
impl Copy for u8 {}
#[lang = "sync"] unsafe trait Sync {}

struct Ops { first: fn(u8) -> u8, second: fn(u8) -> u8 }
unsafe impl Sync for Ops {}
fn twice(x: u8) -> u8 { x * 2 }
fn plus_three(x: u8) -> u8 { x + 3 }
static OPS: Ops = Ops { first: twice, second: plus_three };
#[unsafe(no_mangle)]
pub extern "C-unwind" fn main() -> i32 {
    (OPS.second)((OPS.first)(17)) as i32
}
"#;
        let files = [SourceFile { path: rungs::x::ROOT, text: src }];
        let w = world();
        let mut firsts = AVec::new_in(w.a);
        let compared = compare_one(&w, "static fn fields", &files, "2024", OverflowChecks::Off, Plant::No, &mut firsts);
        assert!(compared.built_both, "both walks must reach main, OPS, twice and plus_three");
        assert_eq!(compared.tally.differing, 0);
        let b = compared.bytes;
        assert!(b.equal >= 4);
        assert_eq!((b.differing, b.only_whole, b.only_lazy), (0, 0, 0));

        use blaze_ladder::run::{FrontChoice, run_with};
        let new = NewFront { package: rungs::x::PACKAGE, edition: "2024", shelf: None, overflow: OverflowChecks::Off };
        for front in [FrontChoice::Old(OverflowChecks::Off), FrontChoice::New(new)] {
            let status = run_with(&files, rungs::x::ROOT, None, front, blaze_own::testing::heap().shared())
                .unwrap_or_else(|why| panic!("{front:?}: {why}"));
            assert_eq!(status, 37, "the second field adds three after the first field doubles 17");
        }
        println!("both front ends called both static fn fields: status 37");
    });
}

#[test]
fn a_planted_abi_is_caught() {
    on_big_stack(planted_abi_caught);
}

/// Execute the code and data records of this walk. The fixtures have an exported
/// C-unwind entry and no native imports.
#[cfg(target_arch = "aarch64")]
fn record_status(front: &impl FrontEnd, walk: &Walk<'_>, store: &MemoryStore<'_>, a: Alloc<'_>) -> i32 {
    use blaze_code::image::{Image, ImageError, Kind, Member, lay_out_positions};
    use blaze_code::record::{Space, Target};
    use blaze_ladder::body_ids::RowKind;
    use blaze_ladder::code_records::{EntryView, statics::StaticEntryView};
    use blaze_launch_rt::sys::{Code, page};

    struct Statics<'i, 'a>(&'i Image<'a>);
    impl blaze_code::place::Words for Statics<'_, '_> {
        fn word(&mut self, target: Target, _: bool) -> Result<u64, ImageError> {
            Err(ImageError::Unbound { target })
        }
        fn value(&mut self, target: Target) -> Result<u64, ImageError> {
            self.0.home(target).ok_or(ImageError::Unbound { target })
        }
    }

    let mut members = Vec::new();
    let mut statics = Vec::new();
    for row in 0..walk.len() {
        let bytes = store.get(walk.key(row).expect("fixture record key")).unwrap().expect("fixture record");
        let symbol = walk.symbol(row);
        match walk.kind(row) {
            RowKind::Instance => members.push(Member { symbol, record: EntryView::read(bytes).unwrap().record }),
            RowKind::Static => statics.push((symbol, StaticEntryView::read(bytes).unwrap().datum)),
            RowKind::ThreadLocal => panic!("unexpected thread-local in the fixture"),
        }
    }
    let page = page().unwrap();
    let segment = lay_out_positions(&members, Kind::Bundle, page as u64, a, |_, row| {
        Ok(front.source_position(ItemKey(row.item), row.site).expect("the call in the fixture's file"))
    }).unwrap();
    let mut memory = Code::map(segment.size as usize).unwrap();
    let base = memory.base() as u64;
    let mut image = Image::new(base, page as u64, a).unwrap();
    let mut data = Vec::new();
    for (symbol, datum) in statics {
        // Each mapping begins on a page, which satisfies the datum's alignment.
        let mapping = Code::map((datum.len() as usize).next_multiple_of(page)).unwrap();
        image.define(Target { space: Space::Static, symbol }, mapping.base() as u64).unwrap();
        data.push((symbol, datum, mapping));
    }
    // WORDS points into another static, so every address must exist before filling.
    for (symbol, datum, mapping) in &mut data {
        let mut bytes = vec![0; datum.bytes().len()];
        blaze_code::datum::fill(datum, *symbol, mapping.base() as u64, &mut Statics(&image), &mut bytes).unwrap();
        mapping.write(0, &bytes).unwrap();
    }
    let placed = image.append(&segment).unwrap();
    assert!(placed.outside.is_empty(), "unexpected native import in the fixture");
    memory.write(0, &segment.code).unwrap();
    memory.write(segment.relro_at as usize, &segment.relro).unwrap();
    for write in placed.writes.iter() {
        memory.write((write.at - base) as usize, &write.value.to_le_bytes()).unwrap();
    }
    let entry = image.home(Target { space: Space::Func, symbol: walk.symbol(0) }).expect("main");
    // SAFETY: row zero is the exported extern "C-unwind" fn main() -> i32 in
    // each fixture. All its imports have been bound and all mappings live across the call.
    let main: extern "C-unwind" fn() -> i32 = unsafe { std::mem::transmute(entry as usize) };
    main()
}

#[cfg(target_arch = "aarch64")]
fn check_record_status(files: &[SourceFile<'_>], expected: i32) {
    let w = world();
    let root = rungs::x::ROOT;
    let tc = Toolchain::none(w.a);
    let new = NewFront { package: rungs::x::PACKAGE, edition: "2024", shelf: None, overflow: OverflowChecks::Off };
    let texts = ProgramTexts::new(files, root, new, &tc, blaze_build::Panic::Unwind).unwrap();
    let user = texts.user();
    let inputs = Inputs::new(&user, &tc).unwrap();
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap();
    let whole = Front2Program::new(&names, w.frame, files, w.a).unwrap();
    let entry = whole.function(whole.entry().expect("exported main")).key;
    let mut lazy = LazyProgram::new(&names, w.frame, files, Some(entry), w.a).unwrap();
    lazy.decide_start(blaze_ladder::run::special::MakerStart::ByRule).unwrap();

    fn execute(front: impl FrontEnd, entry: ItemKey, w: &World) -> i32 {
        let b = builder(OverflowChecks::Off);
        let mut maker = Over::new(Asked::new(front, None, w.a), &b, w.a);
        let mut store = MemoryStore::new_in(w.a);
        let (walk, _) = walk_records(entry, &EveryItem, &mut store, &b, &producer(), &mut maker, w.a).unwrap();
        record_status(&maker.program.inner, &walk, &store, w.a)
    }
    assert_eq!(execute(whole, entry, &w), expected, "whole front end, {root}");
    assert_eq!(execute(lazy, entry, &w), expected, "lazy front end, {root}");
}

/// Both record walks and both in-process runners for fixtures using the common front-end API.
#[cfg(target_arch = "aarch64")]
fn check_pointer_status(text: &str, expected: u8) {
    use blaze_ladder::run::{FrontChoice, run_with};

    let files = [SourceFile { path: rungs::x::ROOT, text }];
    check_record_status(&files, i32::from(expected));
    let overflow = OverflowChecks::Off;
    let new = NewFront { package: rungs::x::PACKAGE, edition: "2024", shelf: None, overflow };
    for front in [FrontChoice::Old(overflow), FrontChoice::New(new)] {
        let status = run_with(&files, rungs::x::ROOT, None, front, blaze_own::testing::heap().shared())
            .unwrap_or_else(|why| panic!("{front:?}: {why}"));
        assert_eq!(status, expected, "{front:?}");
    }
}

#[cfg(target_arch = "aarch64")]
const POINTER_HEAD: &str = r#"#![feature(no_core, lang_items, intrinsics, rustc_attrs)]
#![no_core]
#![no_main]
#[lang = "pointee_sized"] trait PointeeSized {}
#[lang = "meta_sized"] trait MetaSized: PointeeSized {}
#[lang = "sized"] trait Sized: MetaSized {}
#[lang = "copy"] trait Copy {}
impl Copy for u8 {}
impl<T: PointeeSized> Copy for &T {}
impl<T: PointeeSized> Copy for *const T {}
#[repr(transparent)] struct NonNull<T: PointeeSized> { pointer: *const T }
impl<T: PointeeSized> Copy for NonNull<T> {}
#[rustc_intrinsic]
const unsafe fn transmute<S, D>(s: S) -> D;
fn bytes(s: &str) -> &[u8] {
    // SAFETY: str and its UTF-8 byte slice have the same data, length and lifetime.
    unsafe { transmute::<&str, &[u8]>(s) }
}
"#;

#[test]
#[cfg(target_arch = "aarch64")]
fn a_tracked_callers_filename_on_both_front_ends() {
    on_big_stack(|| {
        let main = format!("{POINTER_HEAD}{}", r#"
mod prob;
#[lang = "legacy_receiver"] trait LegacyReceiver: PointeeSized {}
impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> NonNull<T> {
    fn as_ptr(self) -> *mut T { unsafe { transmute::<Self, *mut T>(self) } }
    unsafe fn as_ref<'a>(&self) -> &'a T { unsafe { &*self.as_ptr() } }
}
#[lang = "phantom_data"] struct PhantomData<T: PointeeSized>;
#[lang = "panic_location"]
struct Location<'a> { filename: NonNull<str>, line: u32, col: u32, marker: PhantomData<&'a str> }
#[rustc_intrinsic]
const fn caller_location() -> &'static Location<'static>;
impl<'a> Location<'a> {
    #[track_caller] fn caller() -> &'static Location<'static> { caller_location() }
    fn file(&self) -> &'a str { unsafe { self.filename.as_ref() } }
}
#[unsafe(no_mangle)] pub extern "C-unwind" fn main() -> i32 { prob::probe() }
"#);
        let probe = r#"#[track_caller]
pub fn probe() -> i32 {
    match crate::bytes(crate::Location::caller().file()) {
        [115, 114, 99, 47, 109, 97, 105, 110, 46, 114, 115] => 37, // src/main.rs
        _ => 1,
    }
}
"#;
        let check = |probe, expected| {
            check_record_status(&[SourceFile { path: rungs::x::ROOT, text: &main }, SourceFile { path: "src/prob.rs", text: probe }], expected);
        };
        check(probe, 37);
        // Without forwarding, Location::caller() observes the call inside src/prob.rs.
        // Its name has exactly as many bytes as src/main.rs.
        check(probe.strip_prefix("#[track_caller]").unwrap(), 1);
        println!("both front ends read src/main.rs: status 37; untracked probe: status 1");
    });
}

#[test]
#[cfg(target_arch = "aarch64")]
fn a_static_string_slices_second_element_on_both_front_ends() {
    on_big_stack(|| {
        let text = format!("{POINTER_HEAD}{}", r#"
#[lang = "sync"] unsafe trait Sync {}
unsafe impl Sync for &[&str] {}
#[repr(C)]
struct Bank { first: [&'static str; 2], second: [&'static str; 2] }
unsafe impl Sync for Bank {}
static BANK: Bank = Bank { first: ["south", "north"], second: ["north", "south"] };
static WORDS: &[&str] = &BANK.second;
#[unsafe(no_mangle)] pub extern "C-unwind" fn main() -> i32 {
    match WORDS {
        [_, second] => match bytes(*second) {
            [115, 111, 117, 116, 104] => 37, // south
            _ => 1,
        },
        _ => 2,
    }
}
"#);
        check_pointer_status(&text, 37);
        // An erased addend on the pointer in WORDS selects the first array in BANK.
        let first = text.replace("&BANK.second", "&BANK.first");
        check_pointer_status(&first, 1);
        println!("both front ends read the second string: status 37; zero-offset array: status 1");
    });
}

#[test]
#[cfg(target_arch = "aarch64")]
fn an_erased_formatter_receives_the_referents_address_on_both_front_ends() {
    on_big_stack(|| {
        let text = format!("{POINTER_HEAD}{}", r#"
#[rustc_intrinsic] const fn ptr_guaranteed_cmp<T>(a: *const T, b: *const T) -> u8;
fn address(x: &u8) -> *const u8 { x as *const u8 }
fn apply(erased: unsafe fn(NonNull<()>) -> *const u8, x: &u8) -> *const u8 {
    let pointer = x as *const u8 as *const ();
    unsafe { erased(NonNull { pointer }) }
}
#[unsafe(no_mangle)] pub extern "C-unwind" fn main() -> i32 {
    let value = 17u8;
    let other = 29u8;
    let typed: fn(&u8) -> *const u8 = address;
    // SAFETY: the wrapper has its pointer's ABI and apply passes a pointer to a live u8.
    // This is the function-pointer erasure in core::fmt::rt's formatting argument.
    let erased = unsafe { transmute::<fn(&u8) -> *const u8, unsafe fn(NonNull<()>) -> *const u8>(typed) };
    let actual = apply(erased, &value);
    match ptr_guaranteed_cmp::<u8>(actual, &value as *const u8) { 1 => 37, _ => 1 }
}
"#);
        check_pointer_status(&text, 37);
        let other = text.replace("apply(erased, &value)", "apply(erased, &other)");
        check_pointer_status(&other, 1);
        // Plant the old indirect signature at the call itself. Its argument is the
        // address of the wrapper slot. The formatter only returns that address.
        let old = text.replace("unsafe { erased(NonNull { pointer }) }", r#"
    let slot = NonNull { pointer };
    let indirect = unsafe {
        transmute::<unsafe fn(NonNull<()>) -> *const u8, unsafe fn(&NonNull<()>) -> *const u8>(erased)
    };
    unsafe { indirect(&slot) }
"#);
        check_pointer_status(&old, 1);
        println!("erased formatter: referent 37, distinct referent 1, planted slot address 1");
    });
}

#[test]
#[cfg(target_arch = "aarch64")]
fn transparent_arguments_and_results_on_both_front_ends() {
    on_big_stack(|| {
        let text = format!("{POINTER_HEAD}{}", r#"
impl Copy for u64 {}
#[repr(transparent)] struct Wrapper(u64);
#[repr(transparent)] enum Nested { Value((), Wrapper) }
fn id(x: Wrapper) -> Wrapper { x }
fn nested(x: Nested) -> Nested { x }
extern "C" fn c_id(x: Wrapper) -> Wrapper { x }
extern "C-unwind" fn unwind_id(x: Wrapper) -> Wrapper { x }
#[unsafe(no_mangle)] pub extern "C-unwind" fn main() -> i32 {
    let raw: fn(Wrapper) -> Wrapper = id;
    let deep: fn(Nested) -> Nested = nested;
    let c: extern "C" fn(Wrapper) -> Wrapper = c_id;
    let unwind: extern "C-unwind" fn(Wrapper) -> Wrapper = unwind_id;
    // SAFETY: each cast preserves the calling convention of both argument and result.
    let raw = unsafe { transmute::<fn(Wrapper) -> Wrapper, fn(u64) -> u64>(raw) };
    let deep = unsafe { transmute::<fn(Nested) -> Nested, fn(u64) -> u64>(deep) };
    let c = unsafe { transmute::<extern "C" fn(Wrapper) -> Wrapper, extern "C" fn(u64) -> u64>(c) };
    let unwind = unsafe { transmute::<extern "C-unwind" fn(Wrapper) -> Wrapper, extern "C-unwind" fn(u64) -> u64>(unwind) };
    let w = c_id(id(Wrapper(0x123456789abcdef0u64)));
    match unwind(c(deep(raw(w.0)))) { 0x123456789abcdef0 => 37, _ => 1 }
}
"#);
        check_pointer_status(&text, 37);
        println!("direct Rust, erased Rust, C and C-unwind returned the full u64: status 37");
    });
}

#[test]
#[cfg(target_arch = "aarch64")]
fn nullable_transparent_pointers_on_both_front_ends() {
    on_big_stack(|| {
        // A shared reference gives the no-core fixture the same non-null niche on both
        // front ends, including the legacy reader without scalar-valid-range attributes.
        let text = format!("{POINTER_HEAD}{}", r#"
mod ptr {
    #[repr(transparent)] pub struct NonNull<T: 'static>(pub &'static T);
}
enum Option<T> { None, Some(T) }
#[lang = "sync"] unsafe trait Sync {}
unsafe impl Sync for u8 {}
static BYTE: u8 = 19;
fn nonnull(x: ptr::NonNull<u8>) -> ptr::NonNull<u8> { x }
fn optional(x: Option<ptr::NonNull<u8>>) -> Option<ptr::NonNull<u8>> { x }
extern "C" fn c_optional(x: Option<ptr::NonNull<u8>>) -> Option<ptr::NonNull<u8>> { x }
#[rustc_intrinsic] const fn ptr_guaranteed_cmp<T>(a: *const T, b: *const T) -> u8;
#[unsafe(no_mangle)] pub extern "C-unwind" fn main() -> i32 {
    let p = &BYTE as *const u8;
    let null = 0usize as *const u8;
    let n: fn(ptr::NonNull<u8>) -> ptr::NonNull<u8> = nonnull;
    let o: fn(Option<ptr::NonNull<u8>>) -> Option<ptr::NonNull<u8>> = optional;
    let c: extern "C" fn(Option<ptr::NonNull<u8>>) -> Option<ptr::NonNull<u8>> = c_optional;
    // SAFETY: the transparent reference and its nullable niche carry a single pointer.
    let n = unsafe { transmute::<fn(ptr::NonNull<u8>) -> ptr::NonNull<u8>, fn(*const u8) -> *const u8>(n) };
    let o = unsafe { transmute::<fn(Option<ptr::NonNull<u8>>) -> Option<ptr::NonNull<u8>>, fn(*const u8) -> *const u8>(o) };
    let c = unsafe { transmute::<extern "C" fn(Option<ptr::NonNull<u8>>) -> Option<ptr::NonNull<u8>>, extern "C" fn(*const u8) -> *const u8>(c) };
    match ptr_guaranteed_cmp::<u8>(c(o(n(p))), p) {
        1 => match ptr_guaranteed_cmp::<u8>(o(c(null)), null) { 1 => 37, _ => 2 },
        _ => 1,
    }
}
"#);
        check_pointer_status(&text, 37);
        println!("NonNull and nullable NonNull preserved the address and null: status 37");
    });
}

#[test]
#[cfg(target_arch = "aarch64")]
fn transparent_wide_and_indirect_fields_on_both_front_ends() {
    on_big_stack(|| {
        let text = format!("{POINTER_HEAD}{}", r#"
impl Copy for u64 {}
#[repr(transparent)] struct Text(&'static str);
struct Triple(u64, u64, u64);
#[repr(transparent)] struct Wrapped(Triple);
struct Plain(u64);
fn text(x: Text) -> Text { x }
fn wrapped(x: Wrapped) -> Wrapped { x }
fn plain(x: Plain) -> Plain { x }
#[unsafe(no_mangle)] pub extern "C-unwind" fn main() -> i32 {
    let wide: fn(Text) -> Text = text;
    let indirect: fn(Wrapped) -> Wrapped = wrapped;
    // SAFETY: each transparent argument and result inherits its field's ABI.
    let wide = unsafe { transmute::<fn(Text) -> Text, fn(&'static str) -> &'static str>(wide) };
    let indirect = unsafe { transmute::<fn(Wrapped) -> Wrapped, fn(Triple) -> Triple>(indirect) };
    let triple = indirect(Triple(11, 23, 67));
    match (triple.0, triple.1, triple.2, plain(Plain(91)).0) {
        (11, 23, 67, 91) => match bytes(wide("rust")) { [114, 117, 115, 116] => 37, _ => 2 },
        _ => 1,
    }
}
"#);
        check_pointer_status(&text, 37);
        println!("wide string and indirect aggregates preserved every field: status 37");
    });
}

fn answer_alike() {
    let w = world();
    let mut firsts: AVec<'static, AString<'static>> = AVec::new_in(w.a);
    let mut total = Total::default();
    each_twin(&w, |rung, files, edition, overflow| {
        total.add(&compare_one(&w, rung, files, edition, overflow, Plant::No, &mut firsts));
        true
    });
    let prelude_files = rungs::sources(&rungs::ladder().join("front2").join(fixtures::PRELUDE_OF));
    let Some((_, prelude)) = prelude_files.iter().find(|(path, _)| path == "src/prelude.rs") else {
        panic!("the twin {} has no src/prelude.rs", fixtures::PRELUDE_OF);
    };
    for f in fixtures::FIXTURES {
        let files = [SourceFile { path: "src/main.rs", text: f.main }, SourceFile { path: "src/prelude.rs", text: prelude.as_str() }];
        total.add(&compare_one(&w, f.name, &files, "2024", OverflowChecks::Off, Plant::No, &mut firsts));
    }
    let (t, b) = (total.tally, total.bytes);
    println!("asked: whole {}, lazy {}, common {}, differing {} (floor 0)", t.whole, t.lazy, t.common, t.differing);
    println!("code records: both built {}, equal {}, differing {} (floor 0); only whole {}, only lazy {}", b.both, b.equal, b.differing, b.only_whole, b.only_lazy);
    println!("programs: {} built by both, {} with a gap, {} skipped", total.built_both, total.gaps, total.skipped);
    assert!(total.built_both > 0, "no program was built by both front ends: nothing was compared");
    assert!(t.differing == 0 && b.differing == 0, "the two front ends answered or built otherwise: {} programs' first differences are printed above", firsts.len());
}

fn planted_abi_caught() {
    let w = world();
    let mut firsts: AVec<'static, AString<'static>> = AVec::new_in(w.a);
    let mut caught: Option<(ItemKey, Tally)> = None;
    each_twin(&w, |rung, files, edition, overflow| {
        let o = compare_one(&w, rung, files, edition, overflow, Plant::FirstAbi, &mut firsts);
        match o.planted {
            Some(key) => {
                caught = Some((key, o.tally));
                false
            }
            None => true,
        }
    });
    let Some((key, t)) = caught else {
        panic!("no twin rung's walk asked fn_abi: the control found no key to plant");
    };
    println!("planted (fn_abi {key:?} flipped on the lazy side): differing {}, the planted question among them: {}", t.differing, t.planted_seen);
    assert!(t.differing > 0 && t.planted_seen, "the planted ABI was not caught: differing {}", t.differing);
}
