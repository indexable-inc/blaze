//! Restart from persisted bytes, using the same encoder, block lookup and writer as the
//! command. The oracle is the semantic printer over the current text, plus independent
//! expectations for the moved line, the fix command, the grouped heads and the tally.

use super::*;
use blaze_report::frame::Caps;
use blaze_report::frame::plain::View;
use blaze_sim::{Broke, Config, Tape, check, control};
use bstd::fs::{Content, Durable};
use bstd::io::{Io, Limits, SimWorld};
use bstd::mem::TASK_FRAME;
use bstd::path::Path;
use bstd::test::BUDGET;
use std::io::Write as _;
use std::vec::Vec as Bytes;

const FILE: &str = "lib.rs";
const BODY: &str = "fn broken() {}\n";
const MOVED: &str = "// comment above the item\nfn broken() {}\n";
const MANIFEST: &str = "core = \"1\"\n";

fn tools() -> blaze_engine::Tools {
    blaze_engine::Tools([blaze_drive::Tool(ContentId([5; 32])); blaze_drive::key::KINDS])
}

fn shown(view: View, frame: &Frame) -> print::Shown<'_> {
    let mut shown = print::Shown::new(Caps::LINES, view, "/ws", frame);
    shown.output_only = true;
    shown
}

struct Snapshot {
    record: Bytes<u8>,
    lines: Key,
    semantic: Key,
    presentation: ContentId,
    expected: Bytes<u8>,
    trailer: Bytes<u8>,
    publication: Bytes<Op>,
}

#[derive(Clone)]
enum Op {
    Put(Key, Bytes<u8>),
    Pin(Key),
}

#[derive(Default, Clone)]
struct Store {
    records: std::collections::BTreeMap<Key, Bytes<u8>>,
    root: Option<Key>,
}

impl Store {
    fn apply(&mut self, op: &Op) {
        match op {
            Op::Put(key, bytes) => {
                if let Some(previous) = self.records.insert(*key, bytes.clone()) {
                    assert_eq!(previous, *bytes, "a content key always names the same bytes");
                }
            }
            Op::Pin(key) => self.root = Some(*key),
        }
    }

    fn reopen(&self) -> Result<Rendered<'_>, Broke> {
        let key = self.root.ok_or_else(|| Broke::Oracle("lost the previously committed render root".into()))?;
        let bytes = self.records.get(&key).ok_or_else(|| Broke::Oracle("render pin survived without its bytes".into()))?;
        Rendered::open(bytes).map_err(|why| Broke::Oracle(why.to_string()))
    }
}

fn stdout(io: &Io) -> Bytes<u8> {
    io.script(|host| host.out.clone()).expect("simulated stdout")
}

fn snapshot(moved: bool, view: View, previous: Option<&Snapshot>) -> Snapshot {
    let io = Io::simulated(SimWorld::new(17, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("request context");
    let root = io.need_fs().expect("filesystem").root();
    let out = io.need_out().expect("stdout");
    let frame = cx.frame();
    let alloc = blaze_own::testing::heap().shared();
    let source = if moved { MOVED } else { BODY };
    let mut rows = crate::sweep::Rows::new_in(frame);
    for (kind, path, text) in [(crate::sweep::LeafKind::Slot, FILE, source), (crate::sweep::LeafKind::Manifest, "Cargo.toml", MANIFEST)] {
        root.publish(&cx, &Path::new(path).unwrap(), Content::bytes(text.as_bytes()), Durable::No).unwrap();
        rows.push(kind, 0, path, None, ContentId::of(crate::sweep::FILE_DOMAIN, text.as_bytes()));
    }
    let sources = Sources::new(frame);
    let mut lines = Lines::new_in(alloc);
    let edition = crate::fix::ITEM_GRAMMAR;
    let (_, _, chain) = crate::fix::item_of(BODY, 3, Some(edition), frame);
    lines.placed(Name::file(FILE.as_bytes()), crate::work::SYNTAX, 3, 9, FILE.as_bytes(), chain, edition, b"expected an expression");
    let manifest = Name::file(b"Cargo.toml");
    lines.finding(manifest, crate::units::TOOLCHAIN_NAMED, 0, 4, Some(b"Cargo.toml"), b"core");
    lines.note(manifest, Class::Borrows, b"cannot check this borrow");
    for (i, head) in ["a::one", "a::two", "a::one"].into_iter().enumerate() {
        let name = Name::new(Kind::Signature, ContentId([i as u8; 32]));
        lines.note(name, Class::Head, crate::engine_head_notes::seed_words(head, "missing type", frame).as_bytes());
    }
    let mut fresh = shown(view, frame);
    let tally = print_all(&cx, &root, lines.blocks(), &mut fresh, alloc, frame, &out).unwrap();
    crate::engine_head_notes::print_grouped(&cx, lines.blocks(), &fresh, frame, &out).unwrap();
    let expected = stdout(&io);
    let said = core::str::from_utf8(&expected).unwrap();
    let row = if moved { 2 } else { 1 };
    match view {
        View::Json => assert!(said.contains(&format!("\"path\":\"lib.rs\",\"line\":{row},\"col\":4")), "{said}"),
        View::Agent => assert!(said.contains(&format!("lib.rs:{row}:4")), "{said}"),
        View::Person => unreachable!("the history uses the two deterministic plain views"),
    }
    assert!(said.contains("heads: 2 head(s) failed: missing type: first `a::one`, `a::two`"), "{said}");
    assert_eq!((tally.heads, tally.borrows, fresh.counts.errors, fresh.fixes.len()), (3, 1, 2, 1));
    let coverage = blaze_report::frame::Coverage { ran: &["types"], not: &["borrows"] };
    print::stage_notes(&cx, coverage.not, &fresh, frame, &out).unwrap();
    print::closing(&cx, &fresh, 2, 77, Some(3), None, Some(coverage), frame, &out).unwrap();
    let trailer = stdout(&io)[expected.len()..].to_vec();
    assert!(core::str::from_utf8(&trailer).unwrap().contains(&format!("blaze check --fix {}", fresh.fixes[0])));

    let slot = ViewSlot {
        name: blaze_own::String::new_in(frame), tools: tools(),
        previous: previous.map(|snapshot| Rendered::open(&snapshot.record).unwrap()),
    };
    let mut actual = shown(view, frame);
    let (actual_tally, pending) = slot.draw(&cx, &root, lines.blocks(), &sources, &mut actual, alloc, frame).unwrap();
    assert_eq!(actual_tally.listed, tally.listed);
    if let Some(old) = previous {
        let Rendering::Published { blocks, reused, .. } = actual_tally.rendering else { panic!("draw produces a new rendering") };
        if old.presentation == actual.presentation(&tools()) {
            assert!(reused > 0 && reused < blocks, "the manifest and head group stand while the moved finding is redrawn");
        } else {
            assert_eq!(reused, 0, "a changed view renders every block for that view");
        }
    }
    sources.cover(&mut rows).unwrap();
    let (binding, encoded) = lines.record_for(pending.placement);
    let last = decode(&encoded, alloc).unwrap();
    assert!(last.blocks().eq(lines.blocks()), "the semantic blocks survive independently of the view");
    let mut publication = Bytes::new();
    pending.deliver(binding, |bytes| Ok(out.write_all(&cx, bytes)?), |effect| {
        match effect {
            RenderWrite::Put(key, parts) => publication.push(Op::Put(key, parts.concat())),
            RenderWrite::Pin(key) => publication.push(Op::Pin(key)),
        }
        Ok(())
    }).unwrap();
    let mut store = Store::default();
    for op in &publication {
        store.apply(op);
    }
    let record = store.records.remove(&store.root.expect("committed root")).expect("the pinned bytes");
    assert_eq!(Rendered::open(&record).unwrap().stream, expected.as_slice(), "edit rendering equals the fresh printer");
    Snapshot { record, lines: binding.key, semantic: lines.record().0, presentation: actual.presentation(&tools()), expected, trailer, publication }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Plant {
    Rule,
    OldView,
    OldPlacement,
    LoseSuffix,
}

struct ShortWriter {
    bytes: Bytes<u8>,
    limit: usize,
}

impl std::io::Write for ShortWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let len = self.limit.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..len]);
        Ok(len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn history(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let view = if tape.coin("first view") { View::Agent } else { View::Json };
    let other = if view == View::Json { View::Agent } else { View::Json };
    let first = snapshot(false, view, None);
    let edited = snapshot(true, view, Some(&first));
    let switched = snapshot(false, other, Some(&first));
    assert_eq!(first.semantic, edited.semantic, "a comment leaves the item-relative finding intact");
    assert_ne!(first.lines, edited.lines, "the placement witness moved");
    assert_eq!(first.lines, switched.lines, "semantic keys are independent of the presentation");

    // Two writers of the SAME view. The plain store may commit a prefix of puts
    // before the final root flip. Fail a sync or crash after any effect, and reopen
    // using either reader's already-acquired answer pointer.
    let mut committed = Store::default();
    for op in &first.publication {
        committed.apply(op);
    }
    let mut staged = committed.clone();
    let mut writers = [edited.publication.iter().peekable(), first.publication.iter().peekable()];
    while writers.iter_mut().any(|w| w.peek().is_some()) {
        let preferred = tape.below(2, "next writer") as usize;
        let at = if writers[preferred].peek().is_some() { preferred } else { 1 - preferred };
        staged.apply(writers[at].next().unwrap());
        if tape.coin("commit prefix") {
            committed = staged.clone();
        }
        if tape.coin("failed sync and restart") {
            staged = committed.clone();
            // A process whose transaction was lost cannot issue the remaining pin.
            break;
        }
        let stored = committed.reopen()?;
        for request in [&first, &edited] {
            if stored.matches(request.lines, request.presentation) && stored.stream != request.expected {
                return Err(Broke::Oracle("a cut published a different answer's bytes".into()));
            }
        }
    }
    let _ = staged.reopen()?;

    let stored = Rendered::open(&first.record).unwrap();
    for (request, label) in [(&switched, "incompatible presentation"), (&edited, "stale line placement")] {
        let presentation = if plant == Plant::OldView { first.presentation } else { request.presentation };
        let lines = if plant == Plant::OldPlacement { first.lines } else { request.lines };
        if stored.matches(lines, presentation) && stored.stream != request.expected {
            return Err(Broke::Oracle(label.into()));
        }
    }
    let io = Io::simulated(SimWorld::new(18, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
    let mut restored = shown(view, cx.frame());
    let mut writer = ShortWriter { bytes: Bytes::new(), limit: 1 + tape.below(13, "accepted prefix") as usize };
    let mut bulk_calls = 0;
    let tally = stored.replay(&mut restored, cx.frame(), |bytes| {
        bulk_calls += 1;
        if plant == Plant::LoseSuffix {
            let _accepted = writer.write(bytes).unwrap();
        } else {
            writer.write_all(bytes).unwrap();
        }
        Ok(())
    }).unwrap();
    if writer.bytes != first.expected {
        return Err(Broke::Oracle("partial write lost the suffix".into()));
    }
    assert_eq!(bulk_calls, 1);
    assert_eq!((tally.heads, tally.borrows, restored.borrows_unchecked, restored.counts.errors), (3, 1, 1, 2));
    let out = io.need_out().unwrap();
    let coverage = blaze_report::frame::Coverage { ran: &["types"], not: &["borrows"] };
    print::stage_notes(&cx, coverage.not, &restored, cx.frame(), &out).unwrap();
    print::closing(&cx, &restored, 2, 99, Some(0), None, Some(coverage), cx.frame(), &out).unwrap();
    assert_eq!(stdout(&io), first.trailer, "plain closing preserves counts and fix ids after restart");
    Ok(())
}

#[test]
fn restarted_render_equals_fresh_after_edits_and_short_writes() {
    check(Config::cases(16), |tape| history(tape, Plant::Rule));
}

#[test]
fn incompatible_presentation_is_caught() {
    control(Config::cases(4), Plant::Rule, Plant::OldView, "incompatible presentation", history);
}

#[test]
fn stale_line_placement_is_caught() {
    control(Config::cases(4), Plant::Rule, Plant::OldPlacement, "stale line placement", history);
}

#[test]
fn dropped_partial_write_is_caught() {
    control(Config::cases(4), Plant::Rule, Plant::LoseSuffix, "partial write lost the suffix", history);
}

#[test]
fn certified_stdout_needs_no_source_and_fixes_keep_the_listing_path() {
    let saved = snapshot(false, View::Agent, None);
    // A new world, with neither source file. A reopen or a re-placement cannot succeed.
    let io = Io::simulated(SimWorld::new(19, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
    let frame = cx.frame();
    let out = io.need_out().unwrap();
    let slot = ViewSlot {
        name: blaze_own::String::new_in(frame), tools: tools(),
        previous: Some(Rendered::open(&saved.record).unwrap()),
    };
    let mut request = shown(View::Agent, frame);
    request.quiet = true;
    assert!(slot.certified(&cx, saved.lines, &mut request, frame, &out).unwrap().is_none());
    request.quiet = false;
    request.each = true;
    assert!(slot.certified(&cx, saved.lines, &mut request, frame, &out).unwrap().is_none());
    request.each = false;
    request.output_only = false;
    assert!(slot.certified(&cx, saved.lines, &mut request, frame, &out).unwrap().is_none());
    let mut relocated = print::Shown::new(Caps::LINES, View::Agent, "/other", frame);
    relocated.output_only = true;
    assert_eq!(relocated.presentation(&tools()), request.presentation(&tools()), "an agent view carries no absolute link");
    let links = Caps { form: blaze_report::frame::Form::Frames, hyperlinks: true, ..Caps::LINES };
    let one = print::Shown::new(links, View::Person, "/ws", frame);
    let two = print::Shown::new(links, View::Person, "/other", frame);
    assert_ne!(one.presentation(&tools()), two.presentation(&tools()), "clickable headers embed the checkout");
    assert!(stdout(&io).is_empty(), "a miss transfers nothing");
    request.output_only = true;
    let tally = slot.certified(&cx, saved.lines, &mut request, frame, &out).unwrap().unwrap();
    assert_eq!(stdout(&io), saved.expected);
    assert_eq!((tally.listed, tally.heads, tally.borrows, request.counts.errors), (2, 3, 1, 2));
    assert_eq!(tally.tail, Tail::default());

    let mut failed = shown(View::Agent, frame);
    let mut stopped = ShortWriter { bytes: Bytes::new(), limit: 1 };
    let error = slot.previous.unwrap().replay(&mut failed, frame, |bytes| {
        let prefix = stopped.write(bytes).unwrap();
        stopped.limit = 0;
        stopped.write_all(&bytes[prefix..]).map_err(|error| CliError::Out(bstd::out::OutError {
            stream: bstd::out::Stream::Out,
            why: bstd::out::OutWhy::Host(error.to_string()),
        }))
    }).unwrap_err();
    assert!(matches!(error, CliError::Out(_)));
    assert_eq!(stopped.bytes, saved.expected[..1]);
    assert!(failed.ready_to_replay(), "a failed transfer must not install a successful printing tally");
}

#[test]
fn placement_reads_belong_to_the_certificate() {
    use crate::sweep::{AT_MOD_RS, LeafKind, MODULE, Rows};
    let io = Io::simulated(SimWorld::new(20, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
    let frame = cx.frame();
    let sources = Sources::new(frame);
    let id = ContentId::of(crate::sweep::FILE_DOMAIN, BODY.as_bytes());
    sources.observe("m/mod.rs", BODY.as_bytes()).unwrap();
    let mut rows = Rows::new_in(frame);
    rows.push(LeafKind::Slot, MODULE | AT_MOD_RS, "m.rs", None, id);
    sources.cover(&mut rows).unwrap();
    assert_eq!(rows.rows.len(), 1, "the module slot witnesses its physical path");
    sources.observe("included.rs", BODY.as_bytes()).unwrap();
    sources.cover(&mut rows).unwrap();
    let added = rows.rows.last().unwrap();
    assert_eq!((rows.path(added), added.kind, added.flags, added.id), ("included.rs", LeafKind::Slot, 0, id));
    assert!(sources.observe("included.rs", MOVED.as_bytes()).is_err());
    rows.push(LeafKind::Slot, 0, "included.rs", None, ContentId::of(crate::sweep::FILE_DOMAIN, MOVED.as_bytes()));
    assert!(matches!(sources.cover(&mut rows), Err(CliError::Draw(print::DrawError::Changed { .. }))));
}

#[test]
fn a_new_view_is_pinned_and_read_after_reopening_the_store() {
    use crate::store::Store as _;
    let io = Io::simulated(SimWorld::new(21, Limits::USUAL));
    let mut lines_key = None;
    let mut expected = Bytes::new();
    for (pass, view) in [View::Agent, View::Agent, View::Json, View::Json, View::Person, View::Person].into_iter().enumerate() {
        let cx = io.cx(BUDGET, TASK_FRAME).unwrap();
        let frame = cx.frame();
        let root = io.need_fs().unwrap().open_dir(&cx, &Path::new("ws").unwrap()).unwrap();
        let clock = io.need_clock().unwrap();
        let out = io.need_out().unwrap();
        let err = io.need_err().unwrap();
        let heap = blaze_own::testing::heap();
        let fs = io.need_fs().unwrap();
        let host = io.need_host().unwrap();
        let mut rng = io.need_rng().unwrap();
        let opened = crate::store::open_for_command(&cx, &fs, &host, &mut rng, Some("/h"), None, &root, "ws", &clock, &err, heap).unwrap();
        let erased = blaze_own::Erased::new(frame);
        let mut store = crate::store::CommandStore::new(&cx, &opened.lent, false, opened.open_ns, None, clock.clone(), &err, erased.handle()).unwrap();
        let mut request = shown(view, frame);
        let slot = ViewSlot::load(&opened.lent, "engine.replay.fixture", false, &request, &tools(), frame).unwrap();
        let tally = if let Some(key) = lines_key {
            if pass == 2 || pass == 4 {
                assert!(slot.certified(&cx, key, &mut request, frame, &out).unwrap().is_none());
                slot.standing(&cx, &root, &mut store, key, &mut request, heap.shared(), frame, &out).unwrap().unwrap()
            } else {
                slot.certified(&cx, key, &mut request, frame, &out).unwrap().expect("the previous process published this view")
            }
        } else {
            let mut lines = Lines::new_in(heap.shared());
            lines.note(Name::file(b"lib.rs"), Class::Unchecked, b"not checked");
            let mut rows = crate::sweep::Rows::new_in(frame);
            let (tally, (key, bytes)) = slot.render(&cx, &root, &mut store, &lines, &mut rows, &mut request, frame, &out).unwrap();
            store.put(&cx, key, &bytes).unwrap();
            store.pin("engine.replay.fixture.semantic", &[key]).unwrap();
            lines_key = Some(key);
            tally
        };
        assert_eq!(tally.unchecked, 1);
        expected.extend_from_slice(if view == View::Json { b"{\"note\":\"not checked\"}\n" } else { b"note: not checked\n" });
        assert_eq!(stdout(&io), expected);
        if view == View::Person {
            let (nanos, entered) = if pass == 4 { (1_100_000, 5) } else { (500_000, 0) };
            print::closing(&cx, &request, 1, nanos, Some(entered), None, None, frame, &out).unwrap();
            let bytes = stdout(&io);
            let closing = core::str::from_utf8(&bytes[expected.len()..]).unwrap();
            assert!(closing.contains(if pass == 4 { "1.1 ms" } else { "0.5 ms" }), "{closing}");
            assert!(closing.contains(if pass == 4 { "5 rebuilt" } else { "nothing changed" }), "{closing}");
            expected = bytes;
        }
        store.after_answer(&cx, &mut blaze_drive::profile::Profile::default()).unwrap();
        drop(store);
        crate::store::after_answer_printed(&cx, opened.lent, &opened.home, false, &clock, &err).unwrap();
    }
}
