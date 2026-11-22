//! A run's records sent inline (run_bytes.rs; run schema 6 `Down::Bytes`) against the files
//! control (run_parent.rs `RecordsDir`), on the lazy parent over the toy program
//! (run_parent_tests.rs). The child's refusals of a wrong digest, a truncated total, an
//! out-of-order part and an unexpected record are blaze-launch-rt run_mode/received_tests.rs;
//! here the parent's side: what it frames, what it files, and what a replay finds.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use blaze_ladder::code_records::MemoryStore;
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;

use super::tests::{DYN, ECHO, HUB, LEAF, SHOUT, ToyFront, W, builder, frames, need, symbol_of};
use super::*;
use crate::run_bytes::{Chosen, Delivery, RunBytes, SentWhy};
use crate::start_list::Line;

const TODAY: start_list::Certificate = start_list::Certificate::for_tests([0x6b; 32]);

/// What a run's messages hand the child inline, read by the child's rules (run_mode/received.rs)
/// restated for the parent's side: each record's parts in order from 0, one record open at a
/// time and no other message while it is, the whole of `total` and no more, its BLAKE3 the
/// id it was sent under, each id once; and every record a `Data`, `Publish`, `Positioned` or
/// `Promote` names already whole. The first breach, named.
struct Inline {
    /// Each record received whole, by its id.
    bytes: BTreeMap<[u8; 32], std::vec::Vec<u8>>,
    /// Every record a message named.
    named: BTreeSet<[u8; 32]>,
    /// `Publish` and `Positioned` messages.
    publishes: usize,
}

fn short(id: &[u8; 32]) -> std::string::String {
    id[..6].iter().map(|byte| std::format!("{byte:02x}")).collect()
}

fn inline(out: &[u8]) -> Result<Inline, std::string::String> {
    let mut bytes: BTreeMap<[u8; 32], std::vec::Vec<u8>> = BTreeMap::new();
    let mut named = BTreeSet::new();
    let mut publishes = 0;
    // The record being assembled: its id, its total and the bytes so far.
    let mut open: Option<([u8; 32], u64, std::vec::Vec<u8>)> = None;
    let mut at = 0;
    while let Some((item, len)) = split(&out[at..]).map_err(|why| std::format!("a frame at {at}: {why:?}"))? {
        let (_, down) = Down::decode(item).map_err(|why| std::format!("a message at {at}: {why:?}"))?;
        let mut names: std::vec::Vec<RecordId> = std::vec::Vec::new();
        match down {
            Down::Bytes { record, at: from, total, part } => {
                let (id, want, mut got) = match open.take() {
                    Some(open) => open,
                    None if bytes.contains_key(&record.0) => return Err(std::format!("record {} sent twice", short(&record.0))),
                    None => (record.0, total, std::vec::Vec::new()),
                };
                if id != record.0 || want != total {
                    return Err(std::format!("a part of record {} (total {total}) while record {} (total {want}) is open", short(&record.0), short(&id)));
                }
                if from != got.len() as u64 {
                    return Err(std::format!("record {}: a part at {from} after {} bytes", short(&id), got.len()));
                }
                got.extend_from_slice(part);
                if got.len() as u64 > want {
                    return Err(std::format!("record {}: {} bytes, past its total {want}", short(&id), got.len()));
                }
                if got.len() as u64 == want {
                    if blaze_blake3::hash(&got) != id {
                        return Err(std::format!("record {}: its bytes are not its digest's", short(&id)));
                    }
                    bytes.insert(id, got);
                } else {
                    open = Some((id, want, got));
                }
            }
            other => {
                if let Some((id, ..)) = &open {
                    return Err(std::format!("a message while record {} is assembling", short(id)));
                }
                match other {
                    Down::Data { data } => names.extend(data.iter().map(|datum| datum.record)),
                    Down::Publish { record, .. } | Down::Positioned { record, .. } => {
                        names.push(record);
                        publishes += 1;
                    }
                    Down::Promote { record, .. } => names.push(record),
                    _ => {}
                }
            }
        }
        for record in names {
            if !bytes.contains_key(&record.0) {
                return Err(std::format!("record {} named before its bytes were whole", short(&record.0)));
            }
            named.insert(record.0);
        }
        at += len;
    }
    if let Some((id, ..)) = open {
        return Err(std::format!("record {} left open at the end", short(&id)));
    }
    Ok(Inline { bytes, named, publishes })
}

/// The needs of the wire model: `hub`, then `echo` (the first to name `S`, whose value names
/// `T`), `shout` (naming `S` again) and `leaf`.
fn hub_inbox() -> std::vec::Vec<u8> {
    let mut inbox = need(Needed::Symbol(symbol_of(HUB)));
    for item in [ECHO, SHOUT, LEAF] {
        inbox.extend_from_slice(&need(Needed::Symbol(symbol_of(item))));
    }
    inbox
}

/// The file of `id` in `dir`: there, and its bytes.
fn file_of(cx: &Cx, dir: &Dir<ReadWrite>, id: &[u8; 32], frame: &bstd::mem::Frame) -> Option<std::vec::Vec<u8>> {
    let mut name = [0u8; 64];
    let path = file_name(id, &mut name).expect("a record's file name");
    dir.stat(cx, &path).expect("stat")?;
    let mut bytes = bstd::mem::Bytes::new_in(frame, "a record file the test reads");
    dir.read_all(cx, &path, &mut bytes).expect("the file reads");
    Some(bytes.as_slice().to_vec())
}

/// Asserts, one run of the wire model each way through the callers' seam (`Chosen::of`):
/// INLINE (`Delivery::Inline`, `RunBytes`) hands the
/// child exactly the records its messages name, each once, whole, under its own digest and
/// before the first message naming it ([`inline`]), and creates nothing under the home's
/// `run/records`; THE CONTROL (`Delivery::Files`, `RecordsDir`) sends no `Bytes`, files the same N records with
/// the same bytes, and its other messages are the inline run's. A parent that framed a
/// record after its `Publish`, sent one twice, sent one no message names, or still filed
/// files fails one of them.
/// Prints "run bytes: 6 records inline, each once before its namer, 0 files; the control files 6, no Bytes".
#[test]
fn inline_sends_each_named_record_once_and_files_none() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let fs = io.need_fs().expect("files");
    let records = Path::new(RECORDS).expect("a path");
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let toy = || ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB };

    // Both through the callers' seam (`Chosen::of`), the inline run's records folder handed as
    // the callers hand theirs.
    let home = fs.home(&cx, "/h", &records).expect("the inline run's records folder");
    let mut sent = std::vec::Vec::new();
    {
        let mut store = MemoryStore::new_in(alloc);
        let mut lazy = Lazy::new(&cx, toy(), Chosen::of(Delivery::Inline, RecordsDir { cx: &cx, dir: &home }, alloc), &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(None, &mut lazy, &mut needs, &mut kept, &mut sent).expect("the head");
        needs.serve(&mut lazy, &hub_inbox(), &mut sent).expect("served inline");
    }
    let got = inline(&sent).unwrap_or_else(|why| panic!("inline: {why}; {:?}", frames(&sent)));
    let ids: BTreeSet<[u8; 32]> = got.bytes.keys().copied().collect();
    assert_eq!(ids, got.named, "the child was sent records no message names, or named records it was never sent");
    assert_eq!(got.bytes.len(), 6, "hub, echo, S, T, shout and leaf are six records: {:?}", frames(&sent));
    for id in ids.iter() {
        assert!(file_of(&cx, &home, id, &frame).is_none(), "the inline run filed record {}", short(id));
    }
    assert!(fs.root().stat(&cx, &Path::new("h/.blaze").expect("a path")).expect("stat").is_none(), "the inline run made a folder under the home");

    // The control: the same needs through the files.
    let dir = fs.home(&cx, "/c", &records).expect("the control's records folder");
    let mut filed = std::vec::Vec::new();
    {
        let mut store = MemoryStore::new_in(alloc);
        let mut lazy = Lazy::new(&cx, toy(), Chosen::of(Delivery::Files, RecordsDir { cx: &cx, dir: &dir }, alloc), &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(None, &mut lazy, &mut needs, &mut kept, &mut filed).expect("the head");
        needs.serve(&mut lazy, &hub_inbox(), &mut filed).expect("served from files");
    }
    let control = frames(&filed);
    assert!(!control.iter().any(|frame| frame.starts_with("Bytes")), "the control sent Bytes: {control:?}");
    let others: std::vec::Vec<std::string::String> = frames(&sent).into_iter().filter(|frame| !frame.starts_with("Bytes")).collect();
    assert_eq!(others, control, "inline, the messages other than Bytes are not the control's");
    let mut files = 0;
    for (id, bytes) in got.bytes.iter() {
        let file = file_of(&cx, &dir, id, &frame).unwrap_or_else(|| panic!("the control did not file record {}", short(id)));
        assert_eq!(&file, bytes, "record {}: the bytes sent inline are not the control's file", short(id));
        files += 1;
    }
    std::println!("run bytes: {} records inline, each once before its namer, 0 files; the control files {files}, no Bytes", got.bytes.len());
}

/// One inline run over `store`, no list: its messages, the list it writes and its head.
fn first_inline(cx: &Cx, front: ToyFront<'_>, store: &mut MemoryStore<'_>, inbox: &[u8], alloc: Alloc<'_>) -> (std::vec::Vec<u8>, std::vec::Vec<Line>, StartHead) {
    let mut out = std::vec::Vec::new();
    let mut lazy = Lazy::new(cx, front, RunBytes::new(alloc), store, builder(), alloc);
    let mut needs = Needs::new(alloc);
    let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
    let head = ahead_of(None, &mut lazy, &mut needs, &mut kept, &mut out).expect("the head");
    needs.serve(&mut lazy, inbox, &mut out).expect("run 1 served");
    let lines = needs.first().lines().collect();
    (out, lines, head)
}

fn codes(lines: &[Line]) -> std::vec::Vec<Symbol> {
    lines.iter().filter_map(Line::code).map(|row| row.symbol).collect()
}

/// Asserts, THE REPLAY: run 2 inline over run 1's store and list, through `ahead_of`, keeps
/// every group (each static's datum found by the pointer run 1 wrote from its id to its
/// entry, each code row by its key), builds no front end and makes nothing, hands the child
/// the same records as run 1 by [`inline`]'s rules, and reads a crash's code record back
/// (`Lazy::read_record`) as sent, refusing one never sent by name. THE CONTROL (rule 23):
/// run 1 through the files writes the same list and no pointer, and run 2 inline over that
/// store keeps only the groups with no data (`hub`, `leaf`), building the front end at
/// `echo`'s need.
/// Prints "run bytes replay: 4 of 4 groups from the store, 6 records, nothing built; with no pointers 2 kept, built at echo".
#[test]
fn an_inline_replay_finds_every_group_through_the_stores_pointers() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let toy = || ToyFront { calls: &calls, starts: &starts, clean: true, root: HUB };
    let mut store = MemoryStore::new_in(alloc);
    let (first, lines, head) = first_inline(&cx, toy(), &mut store, &hub_inbox(), alloc);
    assert_eq!(codes(&lines), [symbol_of(HUB), symbol_of(ECHO), symbol_of(SHOUT), symbol_of(LEAF)], "run 1's list lost a first answer");
    let one = inline(&first).unwrap_or_else(|why| panic!("run 1: {why}"));
    let mut list: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode_lines(&TODAY, head, &lines, &mut list).expect("the list fits");
    let hub = lines.iter().filter_map(Line::code).find(|row| row.symbol == symbol_of(HUB)).expect("hub's row").record;

    starts.set(0);
    calls.set(0);
    let mut second = std::vec::Vec::new();
    let tally = {
        let start_list::Held::Today(today) = start_list::read(list.as_slice(), alloc).expect("the list reads").under(&TODAY) else { panic!("today's list read as another's") };
        let mut lazy = Lazy::new(&cx, toy(), RunBytes::new(alloc), &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(Some(today), &mut lazy, &mut needs, &mut kept, &mut second).expect("placed");
        let kept_lines: std::vec::Vec<Line> = kept.iter().flat_map(|group| group.view().lines()).collect();
        assert_eq!(kept_lines, lines, "a group was left out with its entries in the store");
        needs.serve(&mut lazy, &hub_inbox(), &mut second).expect("run 2 served from the list");
        assert!(lazy.is_cold(), "run 2 built the front end");
        // A crash's read of a code record sent this run, and of one never sent.
        let mut struck = bstd::mem::Bytes::new_in(&frame, "the code record a crash struck in");
        lazy.read_record(&hub, &mut struck).expect("hub's record reads back");
        assert_eq!(Some(struck.as_slice()), one.bytes.get(&hub.0).map(std::vec::Vec::as_slice), "the crash read is not the record sent");
        match lazy.read_record(&RecordId([0xee; 32]), &mut struck) {
            Err(ParentError::Sent { record, why: SentWhy::NotKept }) => assert_eq!(record, [0xee; 32]),
            other => panic!("a record never sent was read: {other:?}"),
        }
        needs.start_tally(lazy.built(), ListHeld::Today)
    };
    assert_eq!((starts.get(), calls.get()), (0, 0), "run 2 asked for the roots or made a record");
    let two = inline(&second).unwrap_or_else(|why| panic!("run 2: {why}; {:?}", frames(&second)));
    assert_eq!(two.bytes, one.bytes, "run 2 sent other records than run 1");
    assert_eq!(two.publishes, 4 + 4, "run 2: the four groups placed, then four needs of placed rows");
    assert_eq!(std::format!("{tally}"), "start list: placed 4 of 4 first answers (2 data, 0 positioned); resolved after the list 0; front end built: no");

    // The control: run 1 through the files, so no pointer is written.
    let fs = io.need_fs().expect("files");
    let dir = fs.home(&cx, "/c", &Path::new(RECORDS).expect("a path")).expect("the control's records folder");
    let mut bare = MemoryStore::new_in(alloc);
    let mut filed = std::vec::Vec::new();
    let control_lines: std::vec::Vec<Line> = {
        let mut lazy = Lazy::new(&cx, toy(), RecordsDir { cx: &cx, dir: &dir }, &mut bare, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(None, &mut lazy, &mut needs, &mut kept, &mut filed).expect("the head");
        needs.serve(&mut lazy, &hub_inbox(), &mut filed).expect("served from files");
        needs.first().lines().collect()
    };
    assert_eq!(control_lines, lines, "the files control wrote another list");
    let mut third = std::vec::Vec::new();
    {
        let start_list::Held::Today(today) = start_list::read(list.as_slice(), alloc).expect("the list reads").under(&TODAY) else { panic!("today's list read as another's") };
        let mut lazy = Lazy::new(&cx, toy(), RunBytes::new(alloc), &mut bare, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(Some(today), &mut lazy, &mut needs, &mut kept, &mut third).expect("placed");
        let kept_codes: std::vec::Vec<Symbol> = kept.iter().map(|group| group.code().symbol).collect();
        assert_eq!(kept_codes, [symbol_of(HUB), symbol_of(LEAF)], "with no pointers a group with data was kept");
        needs.serve(&mut lazy, &hub_inbox(), &mut third).expect("run 3 served");
        assert_eq!(lazy.built(), Built::Need { symbol: symbol_of(ECHO).0 }, "the front end was not built at echo's need");
    }
    inline(&third).unwrap_or_else(|why| panic!("run 3: {why}; {:?}", frames(&third)));
    std::println!("run bytes replay: 4 of 4 groups from the store, {} records, nothing built; with no pointers 2 kept, built at echo", two.bytes.len());
}

/// Asserts: a vtable's datum (`dyn`'s `W`) is sent inline before the `Data` naming it, and a
/// replay finds it through the pointer from its id to the code entry that carries it (the
/// datum is no entry's own record): run 2 keeps `dyn`'s whole group, builds nothing and sends
/// the datum's bytes (`W`'s, the toy's datum) again.
/// Prints "run bytes replay: a vtable's datum found through its carrier, nothing built".
#[test]
fn an_inline_replay_finds_a_vtables_datum_through_its_carrier() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("a root context");
    let frame = bstd::mem::Frame::with_reserve(1 << 22, bstd::mem::Tag::Other).expect("the test's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let (calls, starts) = (Cell::new(0), Cell::new(0));
    let toy = || ToyFront { calls: &calls, starts: &starts, clean: true, root: DYN };
    let inbox = need(Needed::Symbol(symbol_of(DYN)));
    let mut store = MemoryStore::new_in(alloc);
    let (first, lines, head) = first_inline(&cx, toy(), &mut store, &inbox, alloc);
    let datum = W.to_le_bytes();
    let id = blaze_blake3::hash(&datum);
    let one = inline(&first).unwrap_or_else(|why| panic!("run 1: {why}; {:?}", frames(&first)));
    assert_eq!(one.bytes.get(&id).map(std::vec::Vec::as_slice), Some(&datum[..]), "the vtable's datum was not sent inline");
    let mut list: AVec<'_, u8> = Vec::new_in(alloc);
    start_list::encode_lines(&TODAY, head, &lines, &mut list).expect("the list fits");

    starts.set(0);
    calls.set(0);
    let mut second = std::vec::Vec::new();
    {
        let start_list::Held::Today(today) = start_list::read(list.as_slice(), alloc).expect("the list reads").under(&TODAY) else { panic!("today's list read as another's") };
        let mut lazy = Lazy::new(&cx, toy(), RunBytes::new(alloc), &mut store, builder(), alloc);
        let mut needs = Needs::new(alloc);
        let mut kept: AVec<'_, TodayGroup<'_>> = Vec::new_in(alloc);
        ahead_of(Some(today), &mut lazy, &mut needs, &mut kept, &mut second).expect("placed");
        let kept_lines: std::vec::Vec<Line> = kept.iter().flat_map(|group| group.view().lines()).collect();
        assert_eq!(kept_lines, lines, "dyn's group was left out with its carrier in the store");
        needs.serve(&mut lazy, &inbox, &mut second).expect("run 2 served from the list");
        assert!(lazy.is_cold(), "run 2 built the front end");
    }
    assert_eq!((starts.get(), calls.get()), (0, 0), "run 2 asked for the roots or made a record");
    let two = inline(&second).unwrap_or_else(|why| panic!("run 2: {why}; {:?}", frames(&second)));
    assert_eq!(two.bytes, one.bytes, "run 2 sent other records than run 1");
    std::println!("run bytes replay: a vtable's datum found through its carrier, nothing built");
}
