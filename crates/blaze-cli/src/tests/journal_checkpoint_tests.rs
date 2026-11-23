//! The journal's checkpoint (journal_checkpoint.rs) under a deterministic simulation (rule 43):
//! a model of the system's journal of changes (edits, other writes on the volume, a reset, a
//! truncation; it survives a crash, as FSEvents' store does) and of the store (a commit is all
//! or nothing, as blaze-cas proves for a record and its root: plain/root_commit_tests.rs), and
//! commands that load the answer and its checkpoint, replay, stat and commit in steps, two at
//! once, cut before any commit or killed by a crash between any two steps. The code under test
//! is the module's own: `decode`, `choose`, `Places::merged`, `due`, `encode`, and the Stored
//! form's encoder and reader (journal_sweep.rs).
//!
//! THE ORACLE, at each command's stats: every file that differed from the command's stored
//! answer when it replayed is statted (a replay from the chosen places named it, or its unit is
//! swept whole); and where nothing was written between its replay and its stats, its verdict
//! (moved or the certificate) is the verdict of a replay from the answer's own places, the
//! whole suffix. So a checkpoint never yields a state a whole replay would not.
//!
//! Planted controls, each caught by the oracle: a checkpoint used for any answer (a concurrent
//! certifier's, of an answer the root no longer names), a writer that takes its places after its
//! stats instead of from its replay, and a checkpoint written by a command whose certificate did
//! not stand, then cut before its answer was stored.

use super::*;
use blaze_sim::{Broke, Config, Tape, check, control};

/// Files of the modelled checkout: each is one bit, an edit flips it.
const FILES: usize = 2;
/// journal_sweep.rs `Where` as the Stored form writes it.
const CHECKOUT: u8 = 0;
const HOME: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    /// The reader takes a checkpoint whichever pointer it names (journal_checkpoint.rs
    /// `Plant::AnyAnswer`).
    AnyAnswer,
    /// The writer's places are the journal's at its stats, not its replay's.
    CursorAfterStats,
    /// A command whose files moved writes a checkpoint of the answer it loaded.
    WrittenUncertified,
}

/// The system's journal and the files.
struct World {
    journal: u64,
    /// The oldest event kept: a cursor before it is truncated.
    first: u64,
    /// Each event in order: the file it names, or `None` for a write elsewhere on the volume.
    events: std::vec::Vec<Option<usize>>,
    files: [bool; FILES],
}

impl World {
    fn now(&self) -> Cursor {
        Cursor::Sim { journal: self.journal, next: self.events.len() as u64 }
    }

    fn edit(&mut self, file: usize) {
        self.files[file] = !self.files[file];
        self.events.push(Some(file));
    }

    fn noise(&mut self, events: u64) {
        for _ in 0..events {
            self.events.push(None);
        }
    }

    /// Another journal: its IDs begin again.
    fn reset(&mut self) {
        self.journal += 1;
        self.events.clear();
        self.first = 0;
    }

    fn truncate(&mut self) {
        self.first = self.events.len() as u64;
    }

    /// The files a replay from `since` names; `None` when every row is statted (no place,
    /// another journal, a truncated one), as journal_sweep.rs `take` refuses them.
    fn marks(&self, since: Option<Cursor>) -> Option<[bool; FILES]> {
        let Some(Cursor::Sim { journal, next }) = since else { return None };
        let len = self.events.len() as u64;
        if journal != self.journal || next < self.first || next > len {
            return None;
        }
        let mut marked = [false; FILES];
        for file in self.events[next as usize..].iter().flatten() {
            marked[*file] = true;
        }
        Some(marked)
    }

    /// Both units' places now, in the Stored form, as a replay's `Taken::encode` writes them.
    fn places(&self, frame: &Frame) -> std::vec::Vec<u8> {
        let mut bytes = Vec::new_in(frame);
        Stored::encode_units([(CHECKOUT, "", self.now()), (HOME, "", self.now())].into_iter(), None, &mut bytes);
        std::vec::Vec::from(&bytes[..])
    }
}

/// A stored answer: its sealed pointer's key, the files it read, its places.
#[derive(Clone)]
struct Answer {
    key: Key,
    files: [bool; FILES],
    places: std::vec::Vec<u8>,
}

/// What the store has committed: the answer its root names, the checkpoint its `.journal` root
/// names.
#[derive(Default)]
struct Store {
    answer: Option<Answer>,
    checkpoint: Option<std::vec::Vec<u8>>,
    made: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Loaded,
    Replayed,
}

/// One command between its steps.
struct Command {
    answer: Option<Answer>,
    /// The places it replays from (journal_checkpoint.rs `choose`).
    chosen: std::vec::Vec<u8>,
    step: Step,
    /// Its replay's places.
    now: std::vec::Vec<u8>,
    /// The files its replay named (`None`: all statted), and a replay's from the answer's own
    /// places.
    marked: Option<[bool; FILES]>,
    whole: Option<[bool; FILES]>,
    /// The files that differed from its answer when it replayed.
    truth: [bool; FILES],
    /// A file was written between its replay and its stats.
    edited: bool,
}

fn answer_key(n: u64) -> Key {
    Key(ContentId::of("blaze-cli journal checkpoint test answer", &n.to_le_bytes()))
}

fn place_of(stored: Stored<'_>, at: u8) -> Option<Cursor> {
    stored.units().find(|(it, _, _)| *it == at).map(|(_, _, cursor)| cursor)
}

fn whole(bytes: &[u8]) -> Stored<'_> {
    Stored::of(bytes).expect("the model keeps whole places")
}

fn load(store: &Store, fault: Fault, frame: &Frame) -> Command {
    let chosen = match &store.answer {
        None => std::vec::Vec::from(Stored::NONE.bytes()),
        Some(answer) => {
            let plant = if fault == Fault::AnyAnswer { Plant::AnyAnswer } else { Plant::None };
            let found = store.checkpoint.as_deref().map(|bytes| decode(bytes, answer.key, plant));
            std::vec::Vec::from(choose(whole(&answer.places), found, frame).places().bytes())
        }
    };
    Command { answer: store.answer.clone(), chosen, step: Step::Loaded, now: std::vec::Vec::new(), marked: None, whole: None, truth: [true; FILES], edited: false }
}

fn replay(command: &mut Command, world: &World, frame: &Frame) {
    command.marked = world.marks(place_of(whole(&command.chosen), CHECKOUT));
    command.whole = command.answer.as_ref().and_then(|answer| world.marks(place_of(whole(&answer.places), CHECKOUT)));
    command.now = world.places(frame);
    command.truth = match &command.answer {
        Some(answer) => core::array::from_fn(|f| world.files[f] != answer.files[f]),
        None => [true; FILES],
    };
    command.step = Step::Replayed;
    command.edited = false;
}

/// The stats, the oracle, and the commit (`cut`: the power fails before it).
fn finish(command: Command, world: &World, store: &mut Store, fault: Fault, every: u64, cut: bool, frame: &Frame) -> Result<(), Broke> {
    let statted = command.marked.unwrap_or([true; FILES]);
    for f in 0..FILES {
        if command.truth[f] && !statted[f] {
            return Err(Broke::Oracle(format!("file {f} differed from the stored answer at the replay and was not statted: the chosen places hid a change ({fault:?})")));
        }
    }
    let Some(answer) = &command.answer else {
        if !cut {
            store.made += 1;
            store.answer = Some(Answer { key: answer_key(store.made), files: world.files, places: command.now });
        }
        return Ok(());
    };
    let moved = (0..FILES).any(|f| statted[f] && world.files[f] != answer.files[f]);
    if !command.edited {
        let all = command.whole.unwrap_or([true; FILES]);
        let moved_whole = (0..FILES).any(|f| all[f] && world.files[f] != answer.files[f]);
        if moved != moved_whole {
            return Err(Broke::Oracle(format!("the verdict from the chosen places (moved {moved}) is not a whole replay's (moved {moved_whole}) ({fault:?})")));
        }
    }
    let chosen = whole(&command.chosen);
    let now_bytes = if fault == Fault::CursorAfterStats { world.places(frame) } else { command.now.clone() };
    let now = whole(&now_bytes);
    let checkpoint = |frame: &Frame| {
        let places = Places::merged(chosen, now, frame);
        let (_, bytes) = encode(answer.key, places.stored(), frame);
        std::vec::Vec::from(&bytes[..])
    };
    if !moved {
        // The certificate stood: written when due, in the commit after the answer.
        if due(chosen, now, every).is_some() && !cut {
            store.checkpoint = Some(checkpoint(frame));
        }
        return Ok(());
    }
    if fault == Fault::WrittenUncertified {
        // Planted: a checkpoint rides an earlier commit than the answer's.
        store.checkpoint = Some(checkpoint(frame));
    }
    if !cut {
        store.made += 1;
        store.answer = Some(Answer { key: answer_key(store.made), files: world.files, places: command.now });
    }
    Ok(())
}

/// The world, the store and two command slots of one case.
struct Case<'f> {
    world: World,
    store: Store,
    slots: [Option<Command>; 2],
    fault: Fault,
    every: u64,
    frame: &'f Frame,
}

impl Case<'_> {
    /// Slot `slot`'s next step: a load when empty, a replay, then the stats and the commit.
    fn advance(&mut self, slot: usize, cut: bool) -> Result<(), Broke> {
        match self.slots[slot].take() {
            None => self.slots[slot] = Some(load(&self.store, self.fault, self.frame)),
            Some(mut command) if command.step == Step::Loaded => {
                replay(&mut command, &self.world, self.frame);
                self.slots[slot] = Some(command);
            }
            Some(command) => finish(command, &self.world, &mut self.store, self.fault, self.every, cut, self.frame)?,
        }
        Ok(())
    }

    /// A whole command in `slot`, its commit cut when `cut`.
    fn command(&mut self, slot: usize, cut: bool) -> Result<(), Broke> {
        for _ in 0..3 {
            self.advance(slot, cut)?;
        }
        Ok(())
    }

    fn edit(&mut self, file: usize) {
        self.world.edit(file);
        for command in self.slots.iter_mut().flatten() {
            if command.step == Step::Replayed {
                command.edited = true;
            }
        }
    }
}

/// The scripted openings: the shapes each plant needs, then the drawn history.
const OPENINGS: u64 = 4;

fn history(tape: &Tape, fault: Fault) -> Result<(), Broke> {
    let frame = Frame::with_reserve(crate::engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a test frame");
    let opening = tape.below(OPENINGS, "the opening");
    let every = if opening == 0 { 1 + tape.below(4, "the checkpoint's threshold of event ids") } else { 1 };
    let world = World { journal: 1, first: 0, events: std::vec::Vec::new(), files: [false; FILES] };
    let mut case = Case { world, store: Store::default(), slots: [None, None], fault, every, frame: &frame };
    // The first command stores the first answer.
    case.command(0, false)?;
    let file = tape.index(FILES, "the file the opening edits");
    match opening {
        // A concurrent certifier: X loads A; an edit; Y stores B; the edit undone; X certifies
        // A after it and writes A's checkpoint; then a command on B.
        1 => {
            case.advance(0, false)?;
            case.edit(file);
            case.command(1, false)?;
            case.edit(file);
            case.advance(0, false)?;
            case.advance(0, false)?;
            case.command(1, false)?;
        }
        // A command whose file moved, cut before its answer is stored; then another.
        2 => {
            case.edit(file);
            case.command(0, true)?;
            case.command(0, false)?;
        }
        // A write between a certificate's replay and its stats; then another command.
        3 => {
            case.world.noise(1);
            case.advance(0, false)?;
            case.advance(0, false)?;
            case.edit(file);
            case.advance(0, false)?;
            case.command(0, false)?;
        }
        _ => {}
    }
    let ops = tape.below(28, "the history's length");
    for _ in 0..ops {
        match tape.below(16, "the next event") {
            0..=3 => {
                let file = tape.index(FILES, "the file edited");
                case.edit(file);
            }
            4..=6 | 7..=9 => {
                let slot = tape.index(2, "the command advanced");
                let cut = tape.below(8, "the power cut before this commit") == 0;
                case.advance(slot, cut)?;
            }
            10 | 11 => {
                let events = 1 + tape.below(2 * every, "writes elsewhere on the volume");
                case.world.noise(events);
            }
            12 => case.world.reset(),
            13 => case.world.truncate(),
            // A crash: the commands in flight die; the journal and the committed store stay.
            _ => case.slots = [None, None],
        }
    }
    // Every command still in flight runs to its end, and one more runs whole.
    for slot in 0..2 {
        while case.slots[slot].is_some() {
            case.advance(slot, false)?;
        }
    }
    case.command(0, false)
}

/// Expected: passes over every history, cut and crash the tape draws.
#[test]
fn a_replay_from_the_checkpoint_misses_nothing_a_whole_replay_names() {
    check(Config::cases(256), |tape| history(tape, Fault::None));
}

#[test]
fn a_checkpoint_of_another_answer_is_caught() {
    control(Config::cases(256), Fault::None, Fault::AnyAnswer, "the chosen places hid a change", history);
}

#[test]
fn a_checkpoint_placed_after_its_stats_is_caught() {
    control(Config::cases(256), Fault::None, Fault::CursorAfterStats, "the chosen places hid a change", history);
}

#[test]
fn a_checkpoint_written_without_a_certificate_is_caught() {
    control(Config::cases(256), Fault::None, Fault::WrittenUncertified, "the chosen places hid a change", history);
}

/// The record reads back as written; every cut of it (each prefix) and a byte more are refused
/// by form; another answer's is stale. Rule 26: the bytes of a restore equal the write's.
#[test]
fn a_checkpoint_reads_back_whole_and_a_cut_one_is_refused() {
    let frame = Frame::with_reserve(crate::engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a test frame");
    let mut places = Vec::new_in(&frame);
    let cursor = Cursor::Sim { journal: 3, next: 41 };
    Stored::encode_units([(CHECKOUT, "crates", cursor), (HOME, "", cursor)].into_iter(), None, &mut places);
    let stored = whole(&places);
    let (key, bytes) = encode(answer_key(1), stored, &frame);
    let read = decode(&bytes, answer_key(1), Plant::None).expect("the record reads back");
    assert_eq!((read.answer, read.places.bytes()), (answer_key(1), &places[..]), "what was written");
    let (again, _) = encode(answer_key(1), read.places, &frame);
    assert_eq!(again, key, "the record's key is a function of its bytes");
    for len in 0..bytes.len() {
        assert!(matches!(decode(&bytes[..len], answer_key(1), Plant::None), Err(Ignored::Form { .. })), "a cut at {len} of {} bytes is refused", bytes.len());
    }
    let mut longer = std::vec::Vec::from(&bytes[..]);
    longer.push(0);
    assert!(matches!(decode(&longer, answer_key(1), Plant::None), Err(Ignored::Form { .. })), "a byte past its places is refused");
    assert_eq!(decode(&bytes, answer_key(2), Plant::None).err(), Some(Ignored::OtherAnswer), "another answer's checkpoint is stale");
    // CONTROL: the plant takes it.
    assert!(decode(&bytes, answer_key(2), Plant::AnyAnswer).is_ok(), "the plant uses another answer's checkpoint");
}

/// The journal helper's place is kept with the checkout place of ITS side (journal_ask.rs: a
/// helper's place answers "since then" only beside the journal place of its flush): the side
/// whose checkout place `merged` keeps gives the helper's, a side without one gives none (the
/// pair is never split), and equal checkout places keep the newer's. Rule 26: the merged bytes
/// read back.
#[test]
fn the_helper_place_stays_with_its_checkout_place() {
    let frame = Frame::with_reserve(crate::engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a test frame");
    let sim = |journal, next| Cursor::Sim { journal, next };
    let helper = |seq: u8| [seq; crate::journal_sweep::HELPER_PLACE];
    let encoded = |checkout: Cursor, place: Option<[u8; crate::journal_sweep::HELPER_PLACE]>| {
        let mut bytes = Vec::new_in(&frame);
        Stored::encode_units([(CHECKOUT, "", checkout), (HOME, "", sim(1, 1))].into_iter(), Some(7), &mut bytes);
        if let Some(place) = place {
            Stored::encode_helper(place, &mut bytes);
        }
        std::vec::Vec::from(&bytes[..])
    };
    let kept = |older: &[u8], newer: &[u8]| {
        let merged = Places::merged(whole(older), whole(newer), &frame);
        let stored = merged.stored();
        (stored.units().find(|(at, name, _)| *at == CHECKOUT && name.is_empty()).map(|(_, _, cursor)| cursor), stored.helper_place())
    };
    let answer = encoded(sim(1, 10), Some(helper(1)));
    let later = encoded(sim(1, 20), Some(helper(2)));
    let earlier = encoded(sim(1, 5), Some(helper(3)));
    let later_alone = encoded(sim(1, 20), None);
    let equal = encoded(sim(1, 10), Some(helper(4)));
    assert_eq!(kept(&answer, &later), (Some(sim(1, 20)), Some(helper(2))), "the checkpoint's later place, with its helper's");
    assert_eq!(kept(&answer, &earlier), (Some(sim(1, 10)), Some(helper(1))), "the answer's later place, with its helper's");
    assert_eq!(kept(&answer, &later_alone), (Some(sim(1, 20)), None), "a later place with no helper's: the answer's is not paired with it");
    assert_eq!(kept(&answer, &equal), (Some(sim(1, 10)), Some(helper(4))), "equal places: the newer's helper");
    assert_eq!(kept(&later_alone, &answer), (Some(sim(1, 20)), None), "the older side's later place, which had no helper's");
}

/// Per unit the later place on one journal, the checkpoint's across journals, the answer's
/// where the checkpoint has none; a unit only the checkpoint has is added. And `due`: a new
/// place on a reset journal is due whatever the threshold; a short replay is not.
#[test]
fn the_later_place_is_replayed_and_a_new_place_is_due() {
    let frame = Frame::with_reserve(crate::engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a test frame");
    let sim = |journal, next| Cursor::Sim { journal, next };
    let encoded = |units: &[(u8, &'static str, Cursor)]| {
        let mut bytes = Vec::new_in(&frame);
        Stored::encode_units(units.iter().copied(), None, &mut bytes);
        std::vec::Vec::from(&bytes[..])
    };
    let answer = encoded(&[(CHECKOUT, "a", sim(1, 10)), (CHECKOUT, "b", sim(1, 10)), (HOME, "", sim(1, 10))]);
    let checkpoint = encoded(&[(CHECKOUT, "a", sim(1, 20)), (CHECKOUT, "b", sim(1, 5)), (HOME, "", sim(2, 3)), (CHECKOUT, "c", sim(1, 20))]);
    let merged = Places::merged(whole(&answer), whole(&checkpoint), &frame);
    let units: std::vec::Vec<(u8, std::string::String, Cursor)> = merged.stored().units().map(|(at, name, cursor)| (at, name.to_owned(), cursor)).collect();
    assert_eq!(
        units,
        [(CHECKOUT, "a".to_owned(), sim(1, 20)), (CHECKOUT, "b".to_owned(), sim(1, 10)), (HOME, std::string::String::new(), sim(2, 3)), (CHECKOUT, "c".to_owned(), sim(1, 20))],
        "the later on one journal, the checkpoint's across, the answer's when the checkpoint's is earlier, a new unit added"
    );
    let now = encoded(&[(CHECKOUT, "a", sim(1, 22)), (CHECKOUT, "b", sim(1, 22)), (HOME, "", sim(2, 4))]);
    assert_eq!(due(merged.stored(), whole(&now), 64), None, "a replay of 15 ids is not due at 64");
    assert_eq!(due(merged.stored(), whole(&now), 15), Some(Due::Replayed { ids: 15 }), "15 ids at 15 is due");
    assert_eq!(due(whole(&answer), whole(&now), u64::MAX), Some(Due::NewPlace { units: 1 }), "the home's place on the reset journal is new");
    let chosen = choose(whole(&answer), Some(Ok(Checkpoint { answer: answer_key(1), places: whole(&checkpoint) })), &frame);
    let counts = Counts::of(whole(&answer), &chosen, whole(&now));
    assert_eq!((counts.replayed, counts.whole, counts.unplaced, counts.at, counts.status), (15, 24, 0, Some(20), Status::Used), "{counts}");
    assert!(counts.to_string().starts_with("journal: replayed 15 of 24 event ids (checkpoint at 20; floor: the ids appended since the last command)"), "{counts}");
}

/// Acquiring a helper place for the first time, or getting one from a new helper instance after a
/// restart/reboot, marks a checkpoint due (`Due::NewPlace`) even when zero journal events fired,
/// while a same-instance vouch with zero events writes no unnecessary checkpoint.
#[test]
fn a_new_helper_place_or_instance_is_due_even_with_zero_events() {
    let frame = Frame::with_reserve(crate::engine_check::REQUEST_FRAME, bstd::mem::Tag::Other).expect("a test frame");
    let sim = |journal, next| Cursor::Sim { journal, next };
    let helper = |instance: u64, seq: u64| blaze_watch::since::Place { instance, seq }.encode();
    let encoded = |cursor: Cursor, place: Option<[u8; crate::journal_sweep::HELPER_PLACE]>| {
        let mut bytes = Vec::new_in(&frame);
        Stored::encode_units([(CHECKOUT, "", cursor)].into_iter(), Some(7), &mut bytes);
        if let Some(place) = place {
            Stored::encode_helper(place, &mut bytes);
        }
        std::vec::Vec::from(&bytes[..])
    };
    let no_helper = encoded(sim(1, 10), None);
    let no_helper_moved = encoded(sim(1, 11), None);
    let first_helper = encoded(sim(1, 10), Some(helper(0xAA, 1)));
    let same_instance = encoded(sim(1, 10), Some(helper(0xAA, 2)));
    let same_instance_small_tick = encoded(sim(1, 10 + 5_000), Some(helper(0xAA, 3)));
    let same_instance_large_tick = encoded(sim(1, 10 + HELPER_DUE_EVENTS), Some(helper(0xAA, 4)));
    let restarted_helper = encoded(sim(1, 10), Some(helper(0xBB, 1)));
    assert_eq!(due(whole(&no_helper), whole(&first_helper), DUE_EVENTS), Some(Due::NewPlace { units: 1 }), "first helper place triggers checkpoint");
    assert_eq!(due(whole(&no_helper), whole(&no_helper_moved), DUE_EVENTS), Some(Due::Replayed { ids: 1 }), "without helper, 1 event still triggers checkpoint");
    assert_eq!(due(whole(&first_helper), whole(&same_instance), DUE_EVENTS), None, "same helper instance with 0 events writes no checkpoint");
    assert_eq!(due(whole(&first_helper), whole(&same_instance_small_tick), DUE_EVENTS), None, "same helper instance under HELPER_DUE_EVENTS writes no checkpoint");
    assert_eq!(due(whole(&first_helper), whole(&same_instance_large_tick), DUE_EVENTS), Some(Due::Replayed { ids: HELPER_DUE_EVENTS }), "same helper instance at HELPER_DUE_EVENTS checkpoints");
    assert_eq!(due(whole(&same_instance), whole(&restarted_helper), DUE_EVENTS), Some(Due::NewPlace { units: 1 }), "restarted helper instance triggers checkpoint");
}

/// Resetting `<answer_root>.journal` to `[answer]` skips `store.get` and returns `Status::Absent`,
/// and `after_certificate` heals an existing stale `Ignored::OtherAnswer` checkpoint even when
/// `due()` returns `None` (same helper instance, under `HELPER_DUE_EVENTS`).
#[test]
fn reset_sentinel_avoids_stale_checkpoint_read_and_heals_on_certificate() {
    use blaze_cas::plain::Settings;
    use bstd::io::{Io, Limits, SimWorld};
    use bstd::mem::TASK_FRAME;
    use bstd::test::BUDGET;

    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    let cx = io.cx(BUDGET, TASK_FRAME).expect("root context");
    let home = crate::store::Home::of(&cx, &io.need_fs().expect("fs"), Some("/h")).expect("home");
    let heap = blaze_own::testing::heap();
    let frame = cx.frame();
    let erased = blaze_own::Erased::new(frame);
    let clock = io.need_clock().expect("clock");
    let err = io.need_err().expect("stderr");
    let lent = home.open_store(&Settings::default(), crate::store::Facts { boot: Some(1), nonce: 41 }, heap.alloc()).expect("store");
    let mut store = CommandStore::new(&cx, &lent, false, 0, None, clock, &err, erased.handle()).expect("command store");

    let sim = |journal, next| Cursor::Sim { journal, next };
    let helper = |instance: u64, seq: u64| blaze_watch::since::Place { instance, seq }.encode();
    let encoded = |cursor: Cursor, place: [u8; crate::journal_sweep::HELPER_PLACE]| {
        let mut bytes = Vec::new_in(frame);
        Stored::encode_units([(CHECKOUT, "", cursor)].into_iter(), Some(7), &mut bytes);
        Stored::encode_helper(place, &mut bytes);
        std::vec::Vec::from(&bytes[..])
    };
    let old_answer = answer_key(1);
    let new_answer = answer_key(2);
    let old_places = encoded(sim(1, 10), helper(0xAA, 1));
    let new_places = encoded(sim(1, 20), helper(0xAA, 2));
    let now_places = encoded(sim(1, 25), helper(0xAA, 3));

    // Plant a legacy stale checkpoint extending `old_answer` under `answer.journal`, and put
    // `new_answer` as the stored answer's sealed pointer record.
    store.put(&cx, new_answer, b"new answer pointer").expect("put new answer pointer");
    let (stale_key, stale_bytes) = encode(old_answer, whole(&old_places), frame);
    store.put(&cx, stale_key, &stale_bytes).expect("put stale checkpoint");
    store.pin(name("answer", frame).as_str(), &[stale_key]).expect("pin stale checkpoint");
    lent.commit().expect("commit stale checkpoint");

    // 1. Reading for `new_answer` sees `Ignored::OtherAnswer` and does 2 gets (root + checkpoint record).
    let before_stale = lent.counts().gets;
    let chosen = read(&cx, &mut store, "answer", new_answer, whole(&new_places), frame).expect("read stale");
    assert_eq!(chosen.status, Status::Ignored(Ignored::OtherAnswer), "legacy stale checkpoint is reported ignored");
    assert_eq!(lent.counts().gets, before_stale + 2, "stale checkpoint reads both root and checkpoint record");

    // 2. `after_certificate` with `due == None` heals `<answer>.journal` to `[new_answer]`.
    let wrote = after_certificate(&cx, &mut store, "answer", new_answer, &chosen, whole(&now_places), DUE_EVENTS, frame).expect("heal");
    assert_eq!(wrote, None, "no new checkpoint record is written when under threshold");
    lent.commit().expect("commit healed sentinel");

    // 3. The next read sees `[new_answer]` sentinel and returns `Status::Absent` with only the 1 root read (no `store.get` of a checkpoint record).
    let before = lent.counts().gets;
    let next = read(&cx, &mut store, "answer", new_answer, whole(&new_places), frame).expect("read healed");
    assert_eq!(next.status, Status::Absent, "healed sentinel reads as Absent, never Ignored");
    assert_eq!(lent.counts().gets, before + 1, "sentinel reads only the root record and skips store.get");
}
