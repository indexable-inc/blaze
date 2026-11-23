use super::*;
use blaze_cas::plain::Settings;
use blaze_drive::{Kind, Tool};
use blaze_engine::{Buffer, Cause, CellId, Done, Edit, Engine, Everything, Gen, Phase, Planned, Plant, Row, Stage};
use blaze_own::{BTreeMap, Vec};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::test::BUDGET;

const ROWS: u32 = 50_000;
const MEMBERS: u32 = 256;

fn task(i: u32) -> Name {
    if i == ROWS - 1 {
        return Name::unit_graph();
    }
    let mut subject = ContentId::of("answer page edit task", &i.to_le_bytes());
    // Keep a second row on the edited page whatever the encoder's page size.
    if i == 1 {
        subject.0[..2].copy_from_slice(&task(0).subject.0[..2]);
    }
    Name::new(Kind::BodyCheck, subject)
}

fn file(group: u32) -> Name {
    Name::new(Kind::FileItems, ContentId::of("answer page edit file", &group.to_le_bytes()))
}

#[test]
fn editing_one_of_fifty_thousand_rows_reads_only_its_file_and_row_pages() {
    let io = Io::simulated(SimWorld::new(31, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
    let home = crate::store::Home::of(&cx, &io.need_fs().expect("filesystem"), Some("/h")).expect("home");
    let heap = blaze_own::testing::heap();
    let alloc = heap.shared();
    let frame = cx.frame();
    let erased = blaze_own::Erased::new(frame);
    let clock = io.need_clock().expect("clock");
    let err = io.need_err().expect("stderr");
    let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 31 }, heap.alloc()).expect("store");
    let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock.clone(), &err, erased.handle()).expect("command store");
    let journal = crate::journal_sweep::take(&cx, home.dir(), home.dir(), &clock, crate::journal_sweep::Stored::NONE, frame).expect("journal");
    let lines = crate::engine_lines::Lines::new_in(alloc).record();
    let leaves = Rows::new_in(frame);
    let tools = Tools([Tool(ContentId([3; 32])); blaze_drive::key::KINDS]);
    let toolchain = blaze_toolchain::ToolchainId([1; 32]);
    let phase = Phase::new(Stage::Bodies, 0);
    let first = Gen(1);
    let old_source = ContentId([1; 32]);
    let old_payload = blaze_drive::key::content_key(b"before");
    store.put(&cx, old_payload, b"before").expect("old task payload");
    let mut cold = Kept::new_in(alloc);
    cold.answer.serial = first;
    let page_of = blaze_drive::answers::page_of;
    let edited_file = (0..ROWS).map(|n| file(ROWS + n)).find(|f| page_of(*f, 1) != page_of(task(0), 1)).expect("file and row on distinct pages");
    let observed = CellId { of: task(ROWS - 2), cell: crate::engine_tasks::WHOLE };
    for i in 0..ROWS {
        let name = task(i);
        let file = if i < MEMBERS { edited_file } else { file(i / 128) };
        let mut reads = Vec::new_in(alloc);
        let mut by_cell = Vec::new_in(alloc);
        let phase = if i == ROWS - 1 {
            reads.push((observed, blaze_drive::key::ABSENT));
            by_cell.push(0);
            cold.answer.readers.entry(observed).or_insert_with(|| blaze_own::BTreeSet::new_in(alloc)).insert(name);
            Phase::new(Stage::Units, 1)
        } else {
            phase
        };
        cold.answer.by_file.entry(file).or_insert_with(|| Vec::new_in(alloc)).push(name);
        cold.answer.rows.insert(name, Row {
            key: tools.key(name, &old_source), phase, file, source: old_source, out: old_payload.0, serial: first,
            defines: Vec::new_in(alloc), reads, by_cell,
            findings: 0, inline: false, failed: false,
        });
        cold.causes.insert(name, (first, Cause::New));
        cold.records.insert(name, old_payload);
    }
    for (_, names) in cold.answer.by_file.iter_mut() {
        names.sort_unstable();
    }
    let saved = save_edit(&cx, &mut store, &lent, "answer", toolchain, &tools, &cold, Printed::default(), (lines.0, &lines.1),
        None, &leaves, &journal, alloc, frame, &[], (None, core::num::NonZeroU32::MIN)).expect("save all rows");
    assert_eq!(saved.rekeyed.whole, Some(WhyWhole::NoLast), "a first commit keys every page");
    assert!(saved.new_pages > 1, "the answer spans multiple pages");
    lent.commit().expect("commit original answer");
    drop(cold);

    let pointer = load_pointer(&cx, &mut store, "answer").expect("store reads").expect("pointer").expect("valid pointer");
    let directory = open_root(&cx, &mut store, pointer.root, alloc).expect("store reads").expect("valid directory");
    let snapshot = store.snapshot_in(alloc).expect("snapshot");
    let stored = Stored::new(&directory, &snapshot, &pointer.headers, pages::Check::Store, alloc);
    assert_eq!(stored.pages_read(), 0, "opening the directory reads no task pages");
    let plan = Planned { name: task(0), source: ContentId([2; 32]), file: edited_file, phase };
    let mut members = Vec::new_in(alloc);
    members.extend((0..MEMBERS).map(|i| Planned { name: task(i), source: if i == 0 { plan.source } else { old_source }, ..plan }));
    members.sort_unstable_by_key(|one| one.name);
    let scattered: std::collections::BTreeSet<_> = members.iter().map(|one| page_of(one.name, directory.bits)).collect();
    assert!(scattered.len() > 8, "reading each unchanged sibling's row would exceed the budget");
    let start = stored.answer();
    let edit = Edit { files: &[(plan.file, &members)] };
    let mut engine = Engine::new_in(tools, start, &edit, Everything, Plant::None, alloc).expect("start edit");
    assert_eq!(engine.next_phase().expect("open phase"), Some(phase));
    assert_eq!(engine.tasks().len(), 1);
    let run = engine.tasks()[0];
    assert_eq!(run.planned, plan);
    let after = blaze_drive::key::content_key(b"after");
    store.put(&cx, after, b"after").expect("changed task payload");
    let mut batch = Buffer::new_in(alloc);
    batch.done.push(Done {
        name: plan.name, key: run.key, out: after.0, defines: Vec::new_in(alloc), reads: Vec::new_in(alloc), findings: 0, failed: false,
    });
    engine.merge(core::slice::from_mut(&mut batch)).expect("file changed task");
    assert_eq!(engine.next_phase().expect("all tasks settled"), None);
    let finished = engine.finish().expect("finish edit");
    let mut payloads = BTreeMap::new_in(alloc);
    payloads.insert(plan.name, after);
    let changed = Kept::after(BTreeMap::new_in(alloc), finished.answer, &finished.causes, payloads);
    let saved = save_edit(&cx, &mut store, &lent, "answer", toolchain, &tools, &changed, Printed::default(), (lines.0, &lines.1),
        Some(&stored), &leaves, &journal, alloc, frame, &[], (Some(pointer.root), core::num::NonZeroU32::MIN)).expect("publish edit");
    assert_eq!(stored.pages_read(), 2, "planning, merging and publication read the file plan and the changed row");
    assert!((1..=2).contains(&saved.new_pages), "only the edited row and file plan can require a new page");
    lent.commit().expect("commit edited answer");

    let pointer = load_pointer(&cx, &mut store, "answer").expect("store reads").expect("updated pointer").expect("valid pointer");
    let next = open_root(&cx, &mut store, pointer.root, alloc).expect("store reads").expect("updated directory");
    let edited_page = page_of(plan.name, next.bits) as usize;
    let file_page = page_of(edited_file, next.bits) as usize;
    assert_eq!(next.bits, directory.bits);
    assert_ne!(edited_page, file_page);
    assert_ne!(next.pages[edited_page], directory.pages[edited_page]);
    for (index, (&old, &new)) in directory.pages.iter().zip(&next.pages).enumerate() {
        if index != edited_page && index != file_page {
            assert_eq!(old, new, "reuse the page reference at {index}");
        }
    }
    let reopened = store.snapshot_in(alloc).expect("snapshot after commit");
    let answer = Stored::new(&next, &reopened, &pointer.headers, pages::Check::Store, alloc);
    // The same answer read with every byte hashed and every entry walked (`--verify`): the
    // control the store's proof must equal, row for row.
    let verified = Stored::new(&next, &reopened, &pointer.headers, pages::Check::Hashed, alloc);
    let other = (2..MEMBERS).map(task).find(|name| ![edited_page, file_page].contains(&(page_of(*name, next.bits) as usize))).expect("unchanged member on another page");
    let reach = store.named("answer.reach").expect("read published reach root").expect("published reach root");
    // THE RE-KEY COUNT: the edit keyed into the reach root exactly the pages whose key moved,
    // and the root it carried equals the one made from every page (the oracle: a carried index
    // lost or kept stale would differ).
    let moved = directory.pages.iter().zip(&next.pages).filter(|(old, new)| old != new).count();
    assert_eq!(saved.rekeyed, Rekeyed { pages: u32::try_from(moved).expect("a page count"), whole: None }, "only the {moved} moved page(s) re-keyed");
    let mut whole: std::vec::Vec<[u8; 32]> = next.pages.iter().filter(|key| **key != Key::default()).map(|key| blaze_cas::plain::beside_key(blaze_cas::Key(key.0)).0.0).collect();
    whole.sort_unstable();
    let carried: std::vec::Vec<[u8; 32]> = reach.chunks_exact(32).map(|bytes| bytes.try_into().expect("a whole key")).collect();
    assert_eq!(carried, whole, "the carried reach root is the whole one");
    for (name, payload, source, entered, cause) in [
        (plan.name, after, plan.source, Gen(first.0 + 1), Cause::KeyMoved),
        (task(1), old_payload, old_source, first, Cause::New),
        (other, old_payload, old_source, first, Cause::New),
    ] {
        let page = answer.page(name).expect("valid backing page").expect("stored page");
        let row = page.row(name).expect("stored task").facts;
        assert_eq!((row.key, row.source, row.out, row.serial), (tools.key(name, &source), source, payload.0, entered));
        assert_eq!(page.cause(name), Some((entered, cause)));
        assert_eq!(page.record(name), Some(payload));
        let key = next.pages[page_of(name, next.bits) as usize];
        let beside = blaze_cas::plain::beside_key(blaze_cas::Key(key.0));
        assert!(reach.chunks_exact(32).any(|bytes| bytes == &beside.0.0), "the new root keeps this page's reach index");
        let held = lent.get(&beside).expect("read reach index").expect("page reach index");
        for keep in [key, payload] {
            assert!(held.bytes.chunks_exact(32).any(|bytes| bytes == &keep.0.0), "cleanup must retain the live page and each standing payload");
        }
    }
    assert_eq!(answer.pages_read(), 2, "three rows share two verified pages");
    let graph = Name::unit_graph();
    let header = answer.header(graph).expect("read producer header").expect("the graph stands");
    assert_eq!(header.record, Some(old_payload));
    let row = answer.planner_row(graph, alloc).expect("read producer inputs").expect("stored graph");
    assert_eq!(row.reads.as_slice(), &[(observed, blaze_drive::key::ABSENT)]);
    assert_eq!(answer.read_counts(), (2, 1), "the graph's payload key and inputs share one header page proof");
    let line = answer.pages_read_line();
    assert_eq!((line.task, line.header, line.hashed), (2, 1, 0), "on the store's proof no page byte is hashed: {line}");
    assert!(line.bytes > 0, "the pages read are counted in bytes: {line}");
    for name in [plan.name, task(1), other] {
        let proved = answer.page(name).expect("store proof").expect("page").row(name).expect("row").facts;
        let hashed = verified.page(name).expect("hashed and walked").expect("page").row(name).expect("row").facts;
        assert_eq!((proved.key, proved.source, proved.out, proved.serial), (hashed.key, hashed.source, hashed.out, hashed.serial));
    }
    let checked = verified.pages_read_line();
    assert_eq!(checked.hashed, checked.bytes, "the verify path hashes every page byte it reads: {checked}");
    // PLANTED: the same edit keying every page (never committed): the count names it.
    let planted = save_edit_rekeying_all(&cx, &mut store, &lent, "answer", toolchain, &tools, &changed, Printed::default(), (lines.0, &lines.1),
        Some(&stored), &leaves, &journal, alloc, frame, None).expect("publish edit, every page keyed");
    assert_eq!(planted.rekeyed.whole, Some(WhyWhole::Planted));
    assert_ne!(planted.rekeyed.pages as usize, moved, "CONTROL: a commit keying every page is caught by the count");
    println!("50,000 rows, one edited in a 256-task file: zero pages at open, two read through publication");
}

/// THE STORED ANSWER'S HANDLE IS SMALL ON THE STACK: `Stored` lives by value in the command's
/// top-level check frame (engine_check.rs) and is moved there more than once; its 128 producer
/// header slots held inline made it some 12 KiB, copied at each move in a debug build, and
/// `a_changed_archive_checksum_enters_the_body_through_the_graph` overflowed the test thread's
/// stack (wave-1119 t26; it passed on a 64 MiB stack). The slots are in the request's memory
/// now (engine_answer.rs `HeaderSlots`). CONTROL: with the slots inline again this is 12 KiB.
#[test]
fn the_stored_answer_handle_keeps_its_header_slots_off_the_stack() {
    let size = core::mem::size_of::<crate::engine_answer::Stored<'static>>();
    assert!(size <= 1 << 10, "`Stored` is {size} bytes by value, more than 1 KiB: a slot table is held inline again");
}
