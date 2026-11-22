//! A warm `blaze run` under the same certificate (start_list.rs version 4; run.md, "Start list"
//! and "Statics in the run"): every first call the last run answered is answered from the start
//! list, a record naming statics included, with each static's `Data` before the first `Publish`
//! that names it, and no resolve: no front end built. The controls remove the cause (the
//! static-naming groups) and watch the resolver be asked again (rule 23).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use blaze_ladder::code_records::MemoryStore;
use blaze_ladder::run::special::{EntryForm, EntryLine};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

use super::tests::{ECHO, HUB, LEAF, S, SHOUT, T, ToyFolder, ToyFront, builder, need, static_of, statics_before_publish, symbol_of};
use super::*;
use crate::start_list::Line;

/// The statics of the fake run.
const S1: Symbol = Symbol([0x51; 32]);
const S2: Symbol = Symbol([0x61; 32]);

/// The fake run: fn 1 names nothing; fn 2 names `S1` and brings its datum; fn 3 (the THIRD
/// answer) names `S1` (sent by fn 2's answer) and `S2`, and is handed both data (the one
/// writer sends `S2` alone); fn 4 names nothing; anything else is unknown.
struct Statics {
    one: [Datum; 1],
    both: [Datum; 2],
    named_one: [Symbol; 1],
    named_both: [Symbol; 2],
    asks: u32,
}

impl Statics {
    fn new() -> Statics {
        let d1 = Datum { symbol: S1, record: RecordId([0x52; 32]) };
        let d2 = Datum { symbol: S2, record: RecordId([0x62; 32]) };
        Statics { one: [d1], both: [d1, d2], named_one: [S1], named_both: [S1, S2], asks: 0 }
    }
}

impl Resolve for Statics {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        self.asks += 1;
        let n = symbol.0[0];
        let key = CodeKey(blaze_ids::ContentId([n; 32]));
        let record = RecordId([n * 10; 32]);
        Ok(match (n, symbol.0 == [n; 32]) {
            (1 | 4, true) => Resolved::Record { key, record, declares: &[], data: &[], named: &[], sent: Sent::NONE },
            (2, true) => Resolved::Record { key, record, declares: &[], data: &self.one, named: &self.named_one, sent: Sent::NONE },
            (3, true) => Resolved::Record { key, record, declares: &[], data: &self.both, named: &self.named_both, sent: Sent::NONE },
            _ => Resolved::Unknown,
        })
    }
}

/// A resolver that FAILS THE TEST if it is asked anything: a replay must not resolve.
struct Refuses;

impl Resolve for Refuses {
    fn resolve(&mut self, symbol: &Symbol) -> Result<Resolved<'_>, ParentError> {
        panic!("the replay asked the resolver for symbol {:02x?}: a first call the list holds was resolved again", &symbol.0[..6]);
    }
}

/// One message as the child sees it, its slots read as their symbols (a `Declare` is dropped:
/// a replay numbers its slots in one `Declare`, the first run in one per answer).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Said {
    Data(std::vec::Vec<Datum>),
    Publish { symbol: Symbol, record: RecordId },
    Positioned { symbol: Symbol, record: RecordId, values: std::vec::Vec<u8> },
    ThreadLocals,
    Personality(Symbol),
    Failed,
    Native,
}

/// With `firsts`, keep the first publication of each record to each slot. Two slots
/// may share code bytes and still require separate placements.
fn said(bytes: &[u8], firsts: bool) -> std::vec::Vec<Said> {
    let mut slots: BTreeMap<u32, Symbol> = BTreeMap::new();
    let mut published = BTreeSet::new();
    let mut out = std::vec::Vec::new();
    let mut at = 0;
    while let Some((item, len)) = split(&bytes[at..]).expect("a frame") {
        let (_, down) = Down::decode(item).expect("a run wire message");
        match down {
            Down::Declare { pairs } => {
                for pair in pairs.iter() {
                    slots.insert(pair.slot.0, pair.symbol);
                }
            }
            Down::Data { data } => out.push(Said::Data(data.iter().collect())),
            Down::Publish { slot, record, .. } => {
                let symbol = *slots.get(&slot.0).expect("a Publish of a slot never declared");
                if published.insert((slot.0, record.0)) || !firsts {
                    out.push(Said::Publish { symbol, record });
                }
            }
            Down::Positioned { slot, record, values, .. } => {
                let symbol = *slots.get(&slot.0).expect("a Positioned publication of a slot never declared");
                if published.insert((slot.0, record.0)) || !firsts {
                    out.push(Said::Positioned { symbol, record, values: values.to_vec() });
                }
            }
            Down::ThreadLocals { .. } => out.push(Said::ThreadLocals),
            Down::Personality { slot } => out.push(Said::Personality(*slots.get(&slot.0).expect("a personality slot never declared"))),
            Down::Failed { .. } => out.push(Said::Failed),
            Down::Native { .. } => out.push(Said::Native),
            Down::Hooks { .. } | Down::Promote { .. } | Down::Bytes { .. } => {}
        }
        at += len;
    }
    out
}

/// The needs of the fake run: 1, 2, 3 (the third answer), 2 again by slot, 4.
fn inbox() -> std::vec::Vec<u8> {
    let mut inbox = need(Needed::Symbol(Symbol([1; 32])));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([2; 32]))));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([3; 32]))));
    inbox.extend_from_slice(&need(Needed::Slot(Slot(1))));
    inbox.extend_from_slice(&need(Needed::Symbol(Symbol([4; 32]))));
    inbox
}

const NEEDS: usize = 5;

const TODAY: start_list::Certificate = start_list::Certificate::for_tests([0x7a; 32]);

const HEAD: StartHead = StartHead { line: EntryLine { form: EntryForm::Unit, symbol: CodeSymbol([1; 32]) }, personality: None };

fn row(n: u8) -> Line {
    Line::Code(Row { symbol: Symbol([n; 32]), key: CodeKey(blaze_ids::ContentId([n; 32])), record: RecordId([n * 10; 32]) })
}

/// The fake first run: its messages and the list it writes.
fn first_run(alloc: Alloc<'_>) -> (std::vec::Vec<u8>, std::vec::Vec<Line>) {
    let mut resolver = Statics::new();
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.serve(&mut resolver, &inbox(), &mut out).expect("the first run served");
    (out, needs.first().lines().collect())
}

/// `lines` as a list under [`TODAY`] (the planted writer, start_list.rs `encode_lines`).
fn encoded(lines: &[Line], alloc: Alloc<'_>) -> std::vec::Vec<u8> {
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode_lines(&TODAY, HEAD, lines, &mut bytes).expect("the list fits");
    bytes.as_slice().to_vec()
}

/// Asserts (a): the list a run writes holds EVERY first answer in order, the third (which
/// names `S1`, sent by the second, and `S2`) included: `S1`'s datum once, in the second's group
/// before its row; the third's group carries `S2` alone and names both; a second encode is
/// byte-equal, and the bytes read back as the same rows. A parent that kept the old rule (no
/// row for a record naming a static) writes `[1, 4]`; one that recorded the data it was handed
/// rather than the batch sent writes `S1` twice (refused by `read` as `DatumTwice`).
/// Prints "run replay: the list holds 4 first answers, each datum once before its first namer; equal bytes".
#[test]
fn the_list_holds_every_first_answer_with_each_datum_once_before_its_namer() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let (_, lines) = first_run(alloc);
    let statics = Statics::new();
    let want = [row(1), Line::Datum(statics.one[0]), Line::Named(S1), row(2), Line::Datum(statics.both[1]), Line::Named(S1), Line::Named(S2), row(3), row(4)];
    assert_eq!(lines, want, "the list is not every first answer in order, each datum once before its first namer");
    let bytes = encoded(&lines, alloc);
    assert_eq!(encoded(&lines, alloc), bytes, "a second encode is other bytes");
    let view = start_list::read(&bytes, alloc).expect("the list reads");
    assert_eq!(view.lines().collect::<std::vec::Vec<_>>(), want, "the rows read are not the rows written");
    std::println!("run replay: the list holds 4 first answers, each datum once before its first namer; equal bytes");
}

/// Asserts (b), THE REPLAY: a new `Needs` given the first run's list under the same
/// certificate, and a resolver that fails the test if asked: every need of the first run is
/// answered (a `Publish` each, no `Failed`); the messages, slots read as symbols and each
/// record's first `Publish` alone, equal the first run's (so `S1`'s `Data` goes before fn 2's
/// `Publish` and `S2`'s before fn 3's); the list it would write equals the one read (nothing to
/// write at exit); the count says placed 4 of 4, 2 data, resolved 0, front end built: no.
/// Prints "run replay: 5 needs answered from the list, 0 resolved; Data before each namer's Publish".
#[test]
fn a_replay_answers_every_first_call_from_the_list_and_resolves_nothing() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let (first, lines) = first_run(alloc);
    let bytes = encoded(&lines, alloc);
    let start_list::Held::Today(today) = start_list::read(&bytes, alloc).expect("the list reads").under(&TODAY) else { panic!("today's list read as another's") };
    let placed: std::vec::Vec<TodayGroup<'_>> = today.groups().collect();
    let mut needs = Needs::new(alloc);
    let mut out = std::vec::Vec::new();
    needs.place_ahead(&placed, &mut out).expect("placed");
    let ahead = out.len();
    needs.serve(&mut Refuses, &inbox(), &mut out).expect("served from the list");
    // The needs' answers: every message after the placement's (the slots are told there).
    let all = said(&out, false);
    let again = &all[said(&out[..ahead], false).len()..];
    assert_eq!(again.len(), NEEDS, "a need was answered with more or less than one message: {again:?}");
    assert!(again.iter().all(|it| matches!(it, Said::Publish { .. })), "a need of a placed row was not answered by its Publish: {again:?}");
    let replay = said(&out, true);
    assert_eq!(replay, said(&first, true), "the replay's messages are not the first run's");
    let d1 = Said::Data(std::vec![Statics::new().one[0]]);
    let d2 = Said::Data(std::vec![Statics::new().both[1]]);
    let at = |it: &Said| replay.iter().position(|said| said == it).expect("a message of the replay");
    let publish = |n: u8| Said::Publish { symbol: Symbol([n; 32]), record: RecordId([n * 10; 32]) };
    assert!(at(&d1) < at(&publish(2)) && at(&d2) < at(&publish(3)) && at(&d1) < at(&publish(3)), "a Data came after the Publish that needs it: {replay:?}");
    assert_eq!(needs.first().lines().collect::<std::vec::Vec<_>>(), lines, "the replay's list is not the one it was given: it would be written again");
    let tally = needs.start_tally(Built::No, ListHeld::Today);
    assert_eq!((tally.placed, tally.answered, tally.data, tally.resolved), (4, 4, 2, 0));
    assert_eq!(std::format!("{tally}"), "start list: placed 4 of 4 first answers (2 data, 0 positioned); resolved after the list 0; front end built: no");
    std::println!("run replay: {NEEDS} needs answered from the list, 0 resolved; Data before each namer's Publish");
}

/// Asserts (c), THE CONTROL (rule 23): the same replay from the list with its static-naming
/// groups removed (fn 2's and fn 3's: data, names and rows) asks the resolver for exactly those
/// two, so test (b) passes because of the groups, not by accident; the count says so.
/// Prints "run replay control: without the static groups 2 symbols resolved again".
#[test]
fn without_the_static_groups_the_replay_resolves_again() {
    let frame = bstd::mem::Frame::with_reserve(1 << 20, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let (_, lines) = first_run(alloc);
    let cut = [row(1), row(4)];
    assert!(lines.contains(&row(2)) && lines.contains(&row(3)), "the first run's list lost the static groups: {lines:?}");
    let bytes = encoded(&cut, alloc);
    let start_list::Held::Today(today) = start_list::read(&bytes, alloc).expect("the cut list reads").under(&TODAY) else { panic!("today's list read as another's") };
    let placed: std::vec::Vec<TodayGroup<'_>> = today.groups().collect();
    let mut needs = Needs::new(alloc);
    let mut asked = Statics::new();
    let mut out = std::vec::Vec::new();
    needs.place_ahead(&placed, &mut out).expect("placed");
    needs.serve(&mut asked, &inbox(), &mut out).expect("served");
    assert_eq!(asked.asks, 2, "the replay without the static groups did not resolve fn 2 and fn 3 again");
    let tally = needs.start_tally(Built::No, ListHeld::Today);
    assert_eq!((tally.placed, tally.answered, tally.data, tally.resolved), (2, 4, 0, 2));
    std::println!("run replay control: without the static groups {} symbols resolved again", asked.asks);
}

/// Asserts, on the lazy parent over the toy program (run_parent_tests.rs): run 1 (`hub`, then
/// `echo`, the first to name `S` whose value names `T`, then `shout`, naming `S` sent by
/// `echo`'s answer, then `leaf`) builds the front end for its head and writes a list of four
/// groups; run 2 under the same certificate through `ahead_of` answers the same needs with the
/// front end NEVER built (`is_cold`, no roots asked, nothing made), every `Publish` standing on
/// its statics (the wire checker), the messages the first run's, and its count line saying
/// `front end built: no`. THE CONTROL: run 3 from the list with `echo`'s and `shout`'s groups
/// cut builds the front end at `echo`'s need, and its count line names that symbol.
/// Prints "run replay (lazy): run 2 built no front end, 4 of 4 placed; without the static groups the front end is built at echo".
#[test]
fn the_lazy_parent_replays_statics_with_no_front_end() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let mut store = MemoryStore::new_in(alloc);
    let mut folder = ToyFolder::default();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let mut inbox = need(Needed::Symbol(symbol_of(HUB)));
    for item in [ECHO, SHOUT, LEAF] {
        inbox.extend_from_slice(&need(Needed::Symbol(symbol_of(item))));
    }

    // Run 1: no list; the head from the roots.
    let mut first = std::vec::Vec::new();
    let (head, lines, built) = {
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB }, &mut folder, &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        let head = ahead_of(None, &mut lazy, &mut needs, &mut kept, &mut first).expect("the roots");
        needs.serve(&mut lazy, &inbox, &mut first).expect("run 1 served");
        (head, needs.first().lines().collect::<std::vec::Vec<Line>>(), lazy.built())
    };
    assert_eq!(built, Built::Head, "run 1 built the front end for something other than its head");
    let codes: std::vec::Vec<Symbol> = lines.iter().filter_map(Line::code).map(|row| row.symbol).collect();
    assert_eq!(codes, [symbol_of(HUB), symbol_of(ECHO), symbol_of(SHOUT), symbol_of(LEAF)], "run 1's list lost a first answer");
    let data: std::vec::Vec<Symbol> = lines.iter().filter_map(|line| if let Line::Datum(datum) = line { Some(datum.symbol) } else { None }).collect();
    assert_eq!(data, [static_of(S), static_of(T)], "run 1's list does not hold S and T once each");
    let mut bytes: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode_lines(&TODAY, head, &lines, &mut bytes).expect("the list fits");

    // Run 2: the same certificate; nothing of the front end may be built.
    starts.set(0);
    calls.set(0);
    let mut second = std::vec::Vec::new();
    let tally = {
        let start_list::Held::Today(today) = start_list::read(bytes.as_slice(), alloc).expect("the list reads").under(&TODAY) else { panic!("today's list read as another's") };
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB }, &mut folder, &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        let entry = ahead_of(Some(today), &mut lazy, &mut needs, &mut kept, &mut second).expect("placed");
        assert_eq!(entry, head, "the list's head is not run 1's");
        let kept_lines: std::vec::Vec<Line> = kept.iter().flat_map(|group| group.view().lines()).collect();
        assert_eq!(kept_lines, lines, "a group of the list was left out with every file there");
        needs.serve(&mut lazy, &inbox, &mut second).expect("run 2 served from the list");
        assert!(lazy.is_cold(), "run 2 built the front end");
        needs.start_tally(lazy.built(), ListHeld::Today)
    };
    assert_eq!((starts.get(), calls.get()), (0, 0), "run 2 asked for the roots or made a record");
    assert_eq!(statics_before_publish(&second, &folder), Ok(8), "run 2: a Publish before its statics");
    assert_eq!(said(&second, true), said(&first, true), "run 2's messages are not run 1's");
    assert_eq!(std::format!("{tally}"), "start list: placed 4 of 4 first answers (2 data, 0 positioned); resolved after the list 0; front end built: no");

    // Run 3, the control: echo's and shout's groups cut from the list.
    let mut cut: std::vec::Vec<Line> = std::vec::Vec::new();
    let mut group: std::vec::Vec<Line> = std::vec::Vec::new();
    for line in lines.iter() {
        group.push(*line);
        if let Line::Code(row) = line {
            if row.symbol != symbol_of(ECHO) && row.symbol != symbol_of(SHOUT) {
                cut.extend_from_slice(&group);
            }
            group.clear();
        }
    }
    let mut cut_bytes: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode_lines(&TODAY, head, &cut, &mut cut_bytes).expect("the cut list fits");
    let mut third = std::vec::Vec::new();
    let tally = {
        let start_list::Held::Today(today) = start_list::read(cut_bytes.as_slice(), alloc).expect("the cut list reads").under(&TODAY) else { panic!("today's list read as another's") };
        let mut lazy = Lazy::new(&cx, ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB }, &mut folder, &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(Some(today), &mut lazy, &mut needs, &mut kept, &mut third).expect("placed");
        needs.serve(&mut lazy, &inbox, &mut third).expect("run 3 served");
        assert!(!lazy.is_cold(), "run 3 resolved echo without building the front end");
        assert_eq!(lazy.built(), Built::Need { symbol: symbol_of(ECHO).0 }, "the front end was not built at echo's need");
        needs.start_tally(lazy.built(), ListHeld::Today)
    };
    // hub and leaf placed; then hub again, echo and shout resolved, leaf again.
    assert_eq!(statics_before_publish(&third, &folder), Ok(6), "run 3: a Publish before its statics");
    let mut short = std::string::String::new();
    for byte in &symbol_of(ECHO).0[..6] {
        short.push_str(&std::format!("{byte:02x}"));
    }
    assert_eq!(std::format!("{tally}"), std::format!("start list: placed 2 of 4 first answers (0 data, 0 positioned); resolved after the list 2; front end built: yes (first need: {short})"));
    std::println!("run replay (lazy): run 2 built no front end, 4 of 4 placed; without the static groups the front end is built at echo");
}
