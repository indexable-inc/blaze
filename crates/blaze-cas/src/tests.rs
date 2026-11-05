use std::{collections::BTreeMap, time::Duration};

use blaze_core::{
    Driver, run_until_idle,
    sim::{Faults, Rng, Sim},
};

use super::*;
use crate::read::Found;

mod model_check;
mod place_race;

fn name(it: &str) -> FileName {
    FileName::new(it).unwrap()
}

fn config(dir: &str) -> Config {
    Config { dir: name(dir), layers: Vec::new(), tags: 1 << 40 }
}

fn key(n: u64) -> Key {
    Key(ContentId::of("blaze v2 store test key 1", &n.to_le_bytes()))
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// Bytes of a run's output: alike between runs, as the outputs of build scripts are.
fn run_output(n: u64) -> Vec<u8> {
    format!(
        "cargo:rustc-link-lib=static=crate{n}\ncargo:rustc-cfg=has_feature_{}\n\
         cargo:rerun-if-changed=src/native/{n}.c\ncargo:rustc-env=BUILT_AT=/build/{n}/out\n\
         cargo:warning=compiled {} objects\n",
        n % 13,
        n * 31 % 97
    )
    .repeat(6)
    .into_bytes()
}

/// Runs everything asked for, handing the store its completions; returns the tickets made
/// durable.
fn drive(sim: &mut Sim, store: &mut Store, effects: &mut Vec<Effect>) -> Vec<Ticket> {
    let mut durable = Vec::new();
    run_until_idle(sim, effects, |completion, effects| {
        if store.owns(completion.tag) {
            durable.extend(store.complete(completion, effects));
        }
    }).expect("the effects end");
    durable
}

fn open_with(sim: &mut Sim, config: Config) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open(config, &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

fn open(sim: &mut Sim, dir: &str) -> Store {
    open_with(sim, config(dir))
}

fn flush(sim: &mut Sim, store: &mut Store) -> Vec<Ticket> {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive(sim, store, &mut effects)
}

/// `len` bytes of text that compresses, different for each `n`.
fn text(n: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 512);
    let mut at = n * 1000;
    while out.len() < len {
        out.extend(run_output(at));
        at += 1;
    }
    out.truncate(len);
    out
}

fn settle(sim: &mut Sim, store: &mut Store) -> Settled {
    let mut effects = Vec::new();
    let settled = store.settle(&mut effects);
    drive(sim, store, &mut effects);
    settled
}

impl Store {
    /// What a lookup of `key` finds; it must be there and whole.
    fn found_of(&self, key: &Key) -> Found<'_> {
        self.look().find(key).expect("the store has the key").expect("its record is whole")
    }

    /// Where the record of `key` is in its pack: once a commit has moved it out of its segment.
    fn loc_of(&self, key: &Key) -> Loc {
        match self.found_of(key).place {
            Place::Pack(loc) => loc,
            Place::Segment(_) => panic!("{key:?} is still in its segment"),
        }
    }

    /// The form of the record of `key`, and the bytes it stores and holds, wherever it is.
    fn sizes_of(&self, key: &Key) -> (Form, u64, u64) {
        match self.found_of(key).place {
            Place::Pack(loc) => (loc.form, loc.stored, loc.raw),
            Place::Segment(seg) => (seg.form, seg.stored, seg.raw),
        }
    }

    fn flag_of(&self, key: &Key, flag: Flag) -> bool {
        self.found_of(key).at.get(flag)
    }

    fn delta_len(&self) -> usize {
        self.tiers[0].delta.len()
    }
}

fn form_of(store: &Store, key: &Key) -> Form {
    store.found_of(key).place.form()
}

fn quiet() -> Faults {
    Faults::default()
}

fn bytes_of(store: &Store, key: &Key) -> Option<Vec<u8>> {
    store.get(key).unwrap().map(|it| it.bytes.to_vec())
}

#[test]
fn a_put_is_read_at_once_durable_after_a_flush_and_there_after_reopening() {
    let mut sim = Sim::new(1, quiet());
    let mut store = open(&mut sim, "store");
    let values: Vec<(Key, Kind, Vec<u8>)> = vec![
        (key(1), Kind::Query, b"a query's result".to_vec()),
        (key(2), Kind::Code, noise(5000, 2)),
        (key(3), Kind::Debug, b"debug info".to_vec()),
        (key(4), Kind::Run, run_output(4)),
        (key(5), Kind::Output, noise(1000, 5)),
        (key(6), Kind::Image, Vec::new()),
    ];
    let mut tickets = Vec::new();
    for (key, kind, bytes) in &values {
        tickets.push(store.put(*key, *kind, bytes).unwrap());
        let entry = store.get(key).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()));
        assert_eq!(entry.content, content_of(bytes));
    }
    assert!(tickets.iter().all(|it| !store.durable(*it)));
    let durable = flush(&mut sim, &mut store);
    assert_eq!(durable, tickets);
    assert!(tickets.iter().all(|it| store.durable(*it)));
    for (key, _, bytes) in &values {
        assert_eq!(bytes_of(&store, key).as_ref(), Some(bytes));
    }
    drop(store);
    let store = open(&mut sim, "store");
    for (key, kind, bytes) in &values {
        let entry = store.get(key).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()));
    }
    assert!(store.get(&key(99)).unwrap().is_none());
    assert!(!store.has(&key(99)));
    assert_eq!(store.verify(None), Vec::new());
}

/// A waiter on puts of two segments (the store's own and a worker writer's) is durable only
/// once each is: tickets of different segments don't compare, and one `max` of them names the
/// writer's (the higher segment) alone, which a commit made durable while the store's own
/// later put is not yet. `Tickets` keeps each segment's last and lets go of each as it is
/// durable.
#[test]
fn tickets_wait_on_each_segment_where_one_max_of_them_would_not() {
    let mut sim = Sim::new(31, quiet());
    let mut store = open(&mut sim, "store");
    let mut writer = store.writer();
    let guard = enter();
    let early = writer.put(&guard, key(1), Kind::Query, b"a writer's put").unwrap();
    let earlier = writer.put(&guard, key(2), Kind::Query, b"a writer's first").unwrap();
    drop(guard);
    flush(&mut sim, &mut store);
    assert!(store.durable(early) && store.durable(earlier));
    let late = store.put(key(3), Kind::Query, b"the store's own, after").unwrap();
    assert!(!store.durable(late));
    // One max is no bound: it stands for the writer's put, durable, and not the store's.
    assert!(store.durable(early.max(late)), "the writer's segment is above the store's own");
    let mut tickets = Tickets::default();
    for ticket in [earlier, late, Ticket::DURABLE, early] {
        tickets.add(ticket);
    }
    assert_eq!(
        tickets.iter().collect::<Vec<_>>(),
        [earlier.max(early), late],
        "one per segment, none for DURABLE"
    );
    assert_eq!(tickets.pending(|it| store.durable(it)), Some(late));
    tickets.retain_pending(|it| store.durable(it));
    assert_eq!(tickets.iter().collect::<Vec<_>>(), [late]);
    flush(&mut sim, &mut store);
    assert_eq!(tickets.pending(|it| store.durable(it)), None);
    tickets.retain_pending(|it| store.durable(it));
    assert!(tickets.is_empty());
}

#[test]
fn a_second_put_keeps_the_first_and_other_bytes_are_a_mismatch() {
    let mut sim = Sim::new(2, quiet());
    let mut store = open(&mut sim, "store");
    let first = store.put(key(1), Kind::Query, b"one").unwrap();
    let again = store.put(key(1), Kind::Query, b"one").unwrap();
    assert!(again >= first);
    let mismatch = store.put(key(1), Kind::Query, b"two").unwrap_err();
    assert_eq!(mismatch.kept, content_of(b"one"));
    assert_eq!(mismatch.given, content_of(b"two"));
    flush(&mut sim, &mut store);
    let durable_again = store.put(key(1), Kind::Query, b"one").unwrap();
    assert!(store.durable(durable_again), "a value already durable needs no wait");
    assert!(store.put(key(1), Kind::Query, b"two").is_err());
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"one");
}

/// A put of what a commit in flight already writes is durable with that commit.
#[test]
fn a_put_of_a_value_in_flight_is_durable_with_its_commit() {
    let mut sim = Sim::new(3, quiet());
    let mut store = open(&mut sim, "store");
    let ticket = store.put(key(1), Kind::Code, b"code").unwrap();
    let mut effects = Vec::new();
    store.flush(&mut effects);
    let again = store.put(key(1), Kind::Code, b"code").unwrap();
    let durable = drive(&mut sim, &mut store, &mut effects);
    assert!(durable.contains(&ticket));
    assert!(store.durable(again));
}

/// A pause hook ([`Own::pause`]) that holds a put at `stage` until the test lets it go: the
/// first `wait` of the returned barrier is the put arriving there, the second lets it on.
/// How long a test's thread waits at a [`Gate`] for the other before the test fails.
const GATE_WAIT: Duration = Duration::from_secs(30);

/// Two threads' meeting point, met by each in turn (a put's hook and the test driving it): a
/// wait returns once the other thread has come to its matching wait, and fails the test,
/// naming the gate's count, if it has not within [`GATE_WAIT`]. A hook a change skips fails
/// the test rather than hang it (rule 15).
#[derive(Default)]
struct Gate {
    arrived: std::sync::atomic::AtomicU64,
}

impl Gate {
    fn wait(&self) {
        use std::sync::atomic::Ordering;
        let mine = self.arrived.fetch_add(1, Ordering::AcqRel) + 1;
        let met = mine.div_ceil(2) * 2;
        let deadline = std::time::Instant::now() + GATE_WAIT;
        let mut spins = 0u32;
        while self.arrived.load(Ordering::Acquire) < met {
            assert!(
                std::time::Instant::now() < deadline,
                "a gate's other thread never came to wait {met} within {GATE_WAIT:?} (this thread's is {mine})"
            );
            spins += 1;
            match spins < 64 {
                true => std::thread::yield_now(),
                false => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }
}

fn hold_at(
    stage: crate::segment::PutAt,
) -> (std::sync::Arc<Gate>, std::sync::Arc<dyn Fn(crate::segment::PutAt) + Send + Sync>) {
    let gate = std::sync::Arc::new(Gate::default());
    let at = gate.clone();
    let hook = move |it| {
        if it == stage {
            at.wait();
            at.wait();
        }
    };
    (gate, std::sync::Arc::new(hook))
}

/// Review 6 C1: two writers put one key at once and both pass the look. The winner's record
/// is drained and committed while the loser is still in its put; the loser then loses the
/// claim, writes nothing and ends. Its count (which `last_puts` hands a waiter) is durable
/// after the next flush with nothing else to write, not only after some unrelated commit.
#[test]
fn a_put_that_lost_its_key_to_another_writers_is_durable_after_a_flush_with_nothing_to_write() {
    use crate::segment::PutAt;
    let mut sim = Sim::new(41, quiet());
    let mut store = open(&mut sim, "store");
    let mut winner = store.writer();
    let mut loser = store.writer();
    let (gate, hook) = hold_at(PutAt::Counted);
    loser.own.pause = Some(hook);
    let racing = std::thread::spawn(move || {
        let guard = enter();
        let ticket = loser.put(&guard, key(1), Kind::Query, b"one value").unwrap();
        drop(guard);
        ticket
    });
    // The loser is counted and past its look, and has claimed nothing.
    gate.wait();
    let waits = store.last_puts();
    let lost = waits.iter().collect::<Vec<_>>();
    assert_eq!(lost.len(), 1, "the loser's put is counted: {lost:?}");
    let lost = lost[0];
    let guard = enter();
    let won = winner.put(&guard, key(1), Kind::Query, b"one value").unwrap();
    drop(guard);
    flush(&mut sim, &mut store);
    assert!(store.durable(won), "the winner's put is committed");
    assert!(!store.durable(lost), "the loser has not ended: its count is ahead of its drain");
    gate.wait();
    let given = racing.join().unwrap();
    assert_eq!(given, won, "the loser is handed the winner's ticket");
    flush(&mut sim, &mut store);
    assert!(
        store.durable(lost),
        "a flush with nothing to write leaves the loser's count pending for ever"
    );
    assert!(store.last_puts().is_empty());
}

/// Review 6 C2: a put that panics after it was counted (here past its claim, as an assert of
/// the writer's would) still ends: its count is published, so a wait on everything put
/// (`last_puts`, a job's gate) ends with the next flush, and the writer goes on. Without the
/// end, the count stays ahead of what the writer publishes and every later wait on the
/// segment hangs until the writer's next put.
#[test]
fn a_put_that_panics_after_it_is_counted_still_ends_and_its_wait_ends() {
    use crate::segment::PutAt;
    let mut sim = Sim::new(42, quiet());
    let mut store = open(&mut sim, "store");
    let mut writer = store.writer();
    writer.own.pause = Some(std::sync::Arc::new(|it| {
        assert_ne!(it, PutAt::Written, "a bug of the put's, past its claim")
    }));
    let guard = enter();
    let put = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer.put(&guard, key(1), Kind::Query, b"a put that panicked")
    }));
    drop(guard);
    assert!(put.is_err(), "the put panicked");
    writer.own.pause = None;
    let waits = store.last_puts();
    assert!(!waits.is_empty(), "the panicked put was counted");
    flush(&mut sim, &mut store);
    assert_eq!(
        waits.pending(|it| store.durable(it)),
        None,
        "the wait on everything put ends after a flush"
    );
    assert!(store.last_puts().is_empty());
    let guard = enter();
    let next = writer.put(&guard, key(2), Kind::Query, b"after it").unwrap();
    drop(guard);
    flush(&mut sim, &mut store);
    assert!(store.durable(next), "the writer goes on after a panic of its put");
}

/// Review 6 T3, the cross-thread half of D2: a put held between its claim and its end on
/// another thread has its key readable, and is counted (`last_puts`) by then, so a reader's
/// wait covers it. A flush and a whole commit meanwhile (of another put) leave it pending: its
/// record is not published to the drain. Once it ends, the next flush makes it durable. Without
/// the count before the claim, `last_puts` misses a put whose key was read.
#[test]
fn a_put_read_on_another_thread_before_it_ends_is_waited_on_until_a_later_commit() {
    use crate::segment::PutAt;
    let mut sim = Sim::new(43, quiet());
    let mut store = open(&mut sim, "store");
    let mut writer = store.writer();
    let (gate, hook) = hold_at(PutAt::Written);
    writer.own.pause = Some(hook);
    let putting = std::thread::spawn(move || {
        let guard = enter();
        let ticket = writer.put(&guard, key(1), Kind::Query, b"put on another thread").unwrap();
        drop(guard);
        ticket
    });
    gate.wait();
    let reader = store.reader();
    let guard = enter();
    let read =
        reader.get(&guard, &key(1)).unwrap().is_some_and(|it| it.bytes == b"put on another thread");
    drop(guard);
    assert!(read, "its key is readable once claimed");
    let waits = store.last_puts();
    let counted = waits.iter().collect::<Vec<_>>();
    assert_eq!(counted.len(), 1, "a put whose key was read is counted: {counted:?}");
    let counted = counted[0];
    let other = store.put(key(2), Kind::Query, b"the loop's own").unwrap();
    flush(&mut sim, &mut store);
    assert!(store.durable(other), "a commit ran");
    assert!(
        !store.durable(counted),
        "a put not ended is not drained, so no commit made it durable"
    );
    flush(&mut sim, &mut store);
    assert!(!store.durable(counted), "still not ended");
    gate.wait();
    let ticket = putting.join().unwrap();
    assert_eq!(ticket, counted, "the put's ticket is the one counted");
    flush(&mut sim, &mut store);
    assert!(store.durable(counted), "the flush after it ended drains and commits it");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"put on another thread");
}

/// An output a little past [`CHUNK_FROM`] whose last chunk is 100 bytes: so short that a
/// writer's record of it shares a segment chunk (8 KiB in tests) with its payload, and stays
/// unpublished until the put ends. Every other chunk record is longer than a segment chunk and
/// rolls to a chunk of its own, which publishes it, so only the last chunk can be a record in
/// another writer's memory alone.
fn output_with_a_short_last_chunk() -> Vec<u8> {
    output_ending_in(|rest| rest[..100].to_vec())
}

/// An output of noise whose last chunk, as [`output_with_a_short_last_chunk`]'s, shares a
/// segment chunk with its payload, but is 5 KiB of text: at least [`DEMOTE_FROM`], and it
/// compresses, so a settle moves it cold once it is unread for two periods, which points its
/// key at a record of the store's own segment (`Store::rehome`).
fn output_with_a_last_chunk_a_settle_moves() -> Vec<u8> {
    output_ending_in(|_| text(7, 5 << 10))
}

/// Text up to its first chunk cut past [`CHUNK_FROM`], then 100 bytes of it as its last
/// chunk: every chunk but the last compresses and is at least [`DEMOTE_FROM`], so a settle
/// moves them cold once unread for two periods; the last is under [`DEMOTE_FROM`] and stays.
fn text_output_with_a_short_last_chunk() -> Vec<u8> {
    output_of(&text(441, 2 * CHUNK_FROM), |rest| rest[..100].to_vec())
}

/// Noise up to its first chunk cut past [`CHUNK_FROM`], then `tail(the noise after the cut)`
/// as its last chunk, shorter than a chunk's least.
fn output_ending_in(tail: impl FnOnce(&[u8]) -> Vec<u8>) -> Vec<u8> {
    output_of(&noise(2 * CHUNK_FROM, 440), tail)
}

/// `long` up to its first chunk cut past [`CHUNK_FROM`], then `tail(the rest)` as its last
/// chunk, shorter than a chunk's least.
fn output_of(long: &[u8], tail: impl FnOnce(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let cut = chunk::cuts(long).map(|it| it.end).find(|&end| end >= CHUNK_FROM).unwrap();
    let tail = tail(&long[cut..]);
    let mut out = long[..cut].to_vec();
    out.extend_from_slice(&tail);
    assert_eq!(chunk::cuts(&out).last(), Some(cut..cut + tail.len()), "the last chunk is the tail");
    out
}

/// [`drive`], with the completions of the maps `hold` picks kept back in `held`, in the order
/// they came: what a driver does whose maps answer out of order (the sim's poll shuffles what
/// is due, a thread pool answers as each map ends), for as long as a test wants.
fn drive_holding(
    sim: &mut Sim,
    store: &mut Store,
    effects: &mut Vec<Effect>,
    held: &mut Vec<Completion>,
    hold: impl Fn(&Mapped) -> bool,
) {
    run_until_idle(sim, effects, |completion, effects| {
        if !store.owns(completion.tag) {
            return;
        }
        if let Some(Op::Map { what, .. }) = store.tags.ops.get(&completion.tag)
            && hold(what)
        {
            held.push(completion);
            return;
        }
        store.complete(completion, effects);
    }).expect("the effects end");
}

/// Every map a commit asks for before its batch's moves are made.
fn batch_map(what: &Mapped) -> bool {
    matches!(what, Mapped::Pack { batch: Some(_), .. } | Mapped::External { batch: Some(_), .. })
}

/// A flush whose maps `hold` picks are kept back in `held`: its commit is durable, and its
/// batch waits for them.
fn flush_holding(
    sim: &mut Sim,
    store: &mut Store,
    held: &mut Vec<Completion>,
    hold: impl Fn(&Mapped) -> bool,
) {
    let mut effects = Vec::new();
    store.flush(&mut effects);
    drive_holding(sim, store, &mut effects, held, hold);
}

/// Hands the store the completions held back, then runs what they ask for.
fn release(sim: &mut Sim, store: &mut Store, held: Vec<Completion>) {
    let mut effects = Vec::new();
    for completion in held {
        store.complete(completion, &mut effects);
    }
    drive(sim, store, &mut effects);
}

/// Two writers race to put `out` (an output whose last chunk is short, as
/// [`output_with_a_short_last_chunk`]'s) under keys 1 (B) and 2 (A), after `before` small puts
/// of the store's own that no flush took. B claims every chunk but the last, looks the last up
/// and misses it, and holds. A then finds B's chunks kept and not durable, so it copies them,
/// claims the last chunk and holds at [`PutAt::Written`]: its record of the last chunk and its
/// payload are only in its unpublished memory. B's claim of the last chunk loses to A, B keeps
/// a copy of it (a [`segment::COPY`] record), ends, and a flush makes B's ticket durable. Gives
/// the store, its sim, the output and A, still held: release A with `a_gate.wait()` and join
/// `a_put`.
#[allow(clippy::type_complexity)]
fn a_chunk_claim_race(
    seed: u64,
    before: u64,
    out: Vec<u8>,
) -> (Sim, Store, Vec<u8>, std::sync::Arc<Gate>, std::thread::JoinHandle<Ticket>) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use crate::segment::PutAt;
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..before {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    let chunks = chunk::cuts(&out).count();
    let mut b = store.writer();
    let mut a = store.writer();
    // B holds at its miss of the last chunk only: nothing was kept, so it misses every one.
    let b_gate = Arc::new(Gate::default());
    let (at, misses) = (b_gate.clone(), AtomicUsize::new(0));
    b.own.pause = Some(Arc::new(move |it| {
        if it == PutAt::ChunkMissed && misses.fetch_add(1, Ordering::Relaxed) + 1 == chunks {
            at.wait();
            at.wait();
        }
    }));
    let (a_gate, hook) = hold_at(PutAt::Written);
    a.own.pause = Some(hook);
    let b_out = out.clone();
    let b_put = std::thread::spawn(move || {
        let guard = enter();
        let ticket = b.put(&guard, key(1), Kind::Output, &b_out).unwrap();
        drop(guard);
        ticket
    });
    b_gate.wait();
    let a_out = out.clone();
    let a_put = std::thread::spawn(move || {
        let guard = enter();
        let ticket = a.put(&guard, key(2), Kind::Output, &a_out).unwrap();
        drop(guard);
        ticket
    });
    // A has claimed the last chunk and holds before it ends.
    a_gate.wait();
    b_gate.wait();
    let b_ticket = b_put.join().unwrap();
    let last = *chunk_keys(&out).last().unwrap();
    assert!(
        matches!(store.found_of(&last).place, Place::Segment(seg) if seg.segment != b_ticket.segment()),
        "the last chunk's key names A's unpublished record"
    );
    flush(&mut sim, &mut store);
    assert!(store.durable(b_ticket), "B ended and was committed");
    assert!(
        store.last_puts().iter().any(|it| it.segment() != b_ticket.segment() && !store.durable(it)),
        "A has not ended"
    );
    (sim, store, out, a_gate, a_put)
}

/// The keys of the chunks of `out`, a chunked output.
fn chunk_keys(out: &[u8]) -> Vec<Key> {
    chunk::cuts(out).map(|range| Key(ContentId::of(disk::CHUNK_DOMAIN, &out[range]))).collect()
}

/// Review 7: B's claim of a chunk loses to A's put, not durable, between B's look and its
/// claim; B must keep a copy of the chunk (as it does for one it finds kept, not durable, at
/// the look). After a crash while A still holds, the store reopens with B's value whole.
/// Without the copy, B's payload names a chunk no pack holds.
#[test]
fn a_chunk_claimed_by_another_writer_after_the_look_is_copied_before_the_put_turns_durable() {
    let (mut sim, store, out, a_gate, a_put) =
        a_chunk_claim_race(44, 0, output_with_a_short_last_chunk());
    sim.crash();
    a_gate.wait();
    a_put.join().unwrap();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(
        bytes_of(&store, &key(1)).as_ref(),
        Some(&out),
        "B's durable value lost a chunk in the crash"
    );
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 8 C1: a copy record that is a chunk's only durable home is found on reopen only by
/// the scan past the index, so when the commit that drains it also writes the index (the
/// delta holds `INDEX_AFTER` keys), the index must hold the chunk at the copy: B's copy is the
/// chunk's home while its key points at A's unpublished segment. Without that, the index
/// leaves the chunk out, its head marks every pack indexed past the copy, and after a crash
/// before A ends, B's durable value names a chunk the store does not have.
#[test]
fn a_copy_that_is_a_chunks_only_durable_record_survives_an_index_write_and_a_crash() {
    let (mut sim, store, out, a_gate, a_put) =
        a_chunk_claim_race(45, INDEX_AFTER as u64, output_with_a_short_last_chunk());
    assert!(store.tiers[0].head.index.is_some(), "B's flush wrote an index");
    sim.crash();
    a_gate.wait();
    a_put.join().unwrap();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(
        bytes_of(&store, &key(1)).as_ref(),
        Some(&out),
        "B's durable value lost a chunk to the index write"
    );
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 8 C1, the settle half: a settle retires the pack that holds B's copy of the last
/// chunk while A still holds, and rewrites the records a key finds. B's other chunks are text
/// a settle moves cold, which leaves their pack mostly dead; the last chunk is too short to
/// move and stays at B's copy. Were B's copy not the chunk's home, the retire would not rewrite
/// it, and the pack's removal would lose the chunk while A still holds. As the home, it is
/// rewritten, and B's value reads whole after a crash.
#[test]
fn a_copy_that_is_a_chunks_only_durable_record_survives_a_settle_and_a_crash() {
    let (mut sim, mut store, out, a_gate, a_put) =
        a_chunk_claim_race(46, 0, text_output_with_a_short_last_chunk());
    let last = *chunk_keys(&out).last().unwrap();
    let home = store.loc_of(&last);
    // A later pack of the unit, so B's copy's pack may be retired.
    store.put(key(3), Kind::Output, &noise(CHUNK_FROM - 1, 3)).unwrap();
    flush(&mut sim, &mut store);
    let unit = store.tiers[0].packs[home.pack as usize].unit;
    assert!(
        store.tiers[0].packs.iter().rposition(|it| it.unit == unit) != Some(home.pack as usize),
        "a later pack of the unit than B's copy's"
    );
    let mut retired = 0;
    for _ in 0..4 {
        retired += settle(&mut sim, &mut store).retired;
        if store.tiers[0].packs[home.pack as usize].removed {
            break;
        }
    }
    assert!(retired >= 1, "a settle retired a pack");
    assert!(
        store.tiers[0].packs[home.pack as usize].removed,
        "the pack of B's copy was retired and removed"
    );
    assert_ne!(store.found_of(&last).place, Place::Pack(home), "the retire rewrote B's copy");
    sim.crash();
    a_gate.wait();
    a_put.join().unwrap();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(
        bytes_of(&store, &key(1)).as_ref(),
        Some(&out),
        "B's durable value lost a chunk to a retire"
    );
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 8 C1 and review 9 C3, the accounting: B's copy of the last chunk is the chunk's home,
/// and every other record of a chunk drained after its home (A's copies of B's chunks, A's own
/// record of the last chunk once A ends) is a duplicate, never written to a pack. So no pack
/// holds a record no key finds: dead bytes stay exact and zero, and the reopen and a later
/// replace have no second record of a chunk to land on or count again.
#[test]
fn a_chunk_record_drained_after_its_chunks_home_is_never_written() {
    let (mut sim, mut store, out, a_gate, a_put) =
        a_chunk_claim_race(47, 0, output_with_a_short_last_chunk());
    let keys = chunk_keys(&out);
    let dead = |store: &Store| store.tiers[0].packs.iter().map(|it| it.dead).sum::<u64>();
    let last = keys.last().unwrap();
    assert_dead_is_exact(&store);
    assert_eq!(dead(&store), 0, "no pack holds a record no key finds while A holds");
    let home = store.loc_of(last);
    a_gate.wait();
    let a_ticket = a_put.join().unwrap();
    flush(&mut sim, &mut store);
    flush(&mut sim, &mut store);
    assert!(store.durable(a_ticket), "A ended and was committed");
    assert_eq!(store.loc_of(last), home, "the last chunk stays at B's copy");
    assert_dead_is_exact(&store);
    assert_eq!(dead(&store), 0, "A's records of B's chunks were not written");
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out));
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(store.loc_of(last), home, "the reopen lands on the chunk's one record");
    assert_dead_is_exact(&store);
    assert_eq!(bytes_of(&store, &key(1)).as_ref(), Some(&out));
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out));
    assert_eq!(store.verify(None), Vec::new());
}

/// Dead bytes are exact: in every pack not removed, they are the spans of the records no key
/// finds (`Pack::dead`). Run once every batch's moves are made, or a record drained and not
/// yet moved to counts as one no key finds.
fn assert_dead_is_exact(store: &Store) {
    assert!(store.pending.is_empty(), "a batch's moves are not made yet");
    let look = store.look();
    for (at, pack) in store.tiers[0].packs.iter().enumerate().filter(|(_, it)| !it.removed) {
        let mut unfound = 0;
        let mut offset = PACK_HEADER;
        while offset < pack.end {
            if pack.bytes(offset, 4).unwrap() == [0; 4] {
                offset = (offset + 1).next_multiple_of(disk::BLOCK);
                continue;
            }
            let header = RecordHeader::decode(pack.bytes(offset, RECORD_HEADER).unwrap()).unwrap();
            let found = matches!(
                look.find(&header.key),
                Some(Ok(found)) if matches!(found.place, Place::Pack(loc) if loc.pack as usize == at && loc.offset == offset)
            );
            if header.form != Form::Dictionary && !found {
                unfound += header.span();
            }
            offset += header.span();
        }
        assert_eq!(
            pack.dead, unfound,
            "{}: its dead bytes are not the records no key finds",
            pack.file
        );
    }
}

/// Three settles, the last on a commit that writes the index: 48 ([`INDEX_AFTER`]) small puts
/// of the store's own fill the delta for it. By then `chunk` (an output's last chunk,
/// [`output_with_a_last_chunk_a_settle_moves`]), unread for two periods, was moved cold, or
/// rewritten with its pack, either way to a record of the store's own segment that waits for
/// the batches before it.
fn settle_writing_the_index(sim: &mut Sim, store: &mut Store, chunk: &Key) {
    settle(sim, store);
    settle(sim, store);
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    assert!(store.tiers[0].head.index.is_none(), "no index was written before");
    let settled = settle(sim, store);
    assert!(!settled.more, "{settled:?}");
    assert!(
        matches!(store.found_of(chunk).place, Place::Segment(seg) if seg.segment == 0),
        "a settle moved the chunk to a record of its own segment"
    );
    assert!(store.tiers[0].head.index.is_some(), "the last settle's commit wrote the index");
}

/// Review 9 C1: A puts output V (key 2) and holds before it ends, its last chunk X only in its
/// memory. B puts V (key 1), finds X kept and not durable, keeps a copy, and holds. A ends, and
/// a flush drains A's record of X (batch k), its maps held back; B ends, and a flush drains
/// B's copy (batch k+1), maps held too, while X still points at A's segment. A's record was
/// drained first, so B's copy is a duplicate and not written (review 10): X's one waiting move
/// is A's. Batch k's maps come, and X points at A's record. Settles then move X (cold, or with
/// its pack) while batch k+1 waits, the last on a commit that writes the index: the index must
/// hold X where the settle put it. Had B's copy been written, its move (from where X no longer
/// is) would be X's first, and must not keep X out. After a crash both values read back.
#[test]
fn a_key_a_settle_moves_while_a_later_batch_waits_on_its_maps_is_in_the_index_the_settle_writes() {
    use crate::segment::PutAt;
    let out = output_with_a_last_chunk_a_settle_moves();
    let last = *chunk_keys(&out).last().unwrap();
    let mut sim = Sim::new(48, quiet());
    let mut store = open(&mut sim, "store");
    let mut a = store.writer();
    let mut b = store.writer();
    let (a_gate, hook) = hold_at(PutAt::Written);
    a.own.pause = Some(hook);
    let (b_gate, hook) = hold_at(PutAt::Written);
    b.own.pause = Some(hook);
    let put = |mut writer: Writer, n: u64, out: Vec<u8>| {
        std::thread::spawn(move || {
            let guard = enter();
            let ticket = writer.put(&guard, key(n), Kind::Output, &out).unwrap();
            drop(guard);
            ticket
        })
    };
    let a_put = put(a, 2, out.clone());
    a_gate.wait();
    let b_put = put(b, 1, out.clone());
    b_gate.wait();
    let a_segment = match store.found_of(&last).place {
        Place::Segment(seg) => seg.segment,
        Place::Pack(_) => panic!("X is in A's segment"),
    };
    a_gate.wait();
    let a_ticket = a_put.join().unwrap();
    let mut first = Vec::new();
    flush_holding(&mut sim, &mut store, &mut first, batch_map);
    assert!(store.durable(a_ticket), "A's batch is committed");
    b_gate.wait();
    let b_ticket = b_put.join().unwrap();
    let mut second = Vec::new();
    flush_holding(&mut sim, &mut store, &mut second, batch_map);
    assert!(store.durable(b_ticket), "B's batch is committed");
    assert!(
        matches!(store.found_of(&last).place, Place::Segment(seg) if seg.segment == a_segment),
        "X points at A's segment while both batches wait"
    );
    let waiting: Vec<u32> = store
        .pending
        .iter()
        .flat_map(|it| &it.moves)
        .filter(|it| it.key == last)
        .map(|it| it.from.segment)
        .collect();
    assert_eq!(
        waiting,
        vec![a_segment],
        "X's one waiting move is A's record's: B's copy of X was not written"
    );
    release(&mut sim, &mut store, first);
    assert_eq!(store.pending.len(), 1, "B's batch waits on its maps");
    let home = store.loc_of(&last);
    settle_writing_the_index(&mut sim, &mut store, &last);
    sim.crash();
    drop(second);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).as_ref(), Some(&out), "B's value lost X to the index");
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out), "A's value lost X to the index");
    assert_ne!(store.loc_of(&last), home, "X is the settle's record");
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 9 C1, the other order: B's copy of X is X's home (the claim race,
/// [`a_chunk_claim_race`]), then A ends and a flush drains A's record of X, whose batch waits on
/// its maps. Settles move X out of B's copy, the last on a commit that writes the index: A's
/// record (X's first move, from where X no longer is) must not keep X out of it.
#[test]
fn a_key_a_settle_moves_while_a_later_record_of_it_waits_on_its_maps_is_in_the_index_the_settle_writes()
 {
    let (mut sim, mut store, out, a_gate, a_put) =
        a_chunk_claim_race(49, 0, output_with_a_last_chunk_a_settle_moves());
    let last = *chunk_keys(&out).last().unwrap();
    let home = store.loc_of(&last);
    a_gate.wait();
    let a_ticket = a_put.join().unwrap();
    let mut held = Vec::new();
    flush_holding(&mut sim, &mut store, &mut held, batch_map);
    assert!(store.durable(a_ticket), "A's batch is committed");
    assert_eq!(store.pending.len(), 1, "A's batch waits on its maps");
    assert_eq!(store.loc_of(&last), home, "X stays at B's copy");
    settle_writing_the_index(&mut sim, &mut store, &last);
    sim.crash();
    drop(held);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).as_ref(), Some(&out), "B's value lost X to the index");
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out), "A's value lost X to the index");
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 10 C1: B's copy of X is X's home (the claim race), its move made. While A holds, a
/// settle moves X cold, to a record of the store's own segment, and its batch waits on its
/// maps. A ends, and a flush that writes the index drains A's record of X while X points at
/// the settle's record, drained and waiting: A's record is a duplicate and is never written.
/// Written, its move would take no spot and its bytes would be counted nowhere, and a reopen
/// (which takes a key's last record past the index) or an index that named a key's last move
/// would land on it. X stays at the settle's record through the maps, a crash and the reopen,
/// dead bytes stay the records no key finds, and both values read back.
#[test]
fn a_record_drained_while_its_key_is_at_a_settles_waiting_record_is_never_written() {
    let (mut sim, mut store, out, a_gate, a_put) =
        a_chunk_claim_race(53, 0, output_with_a_last_chunk_a_settle_moves());
    let last = *chunk_keys(&out).last().unwrap();
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let (mut held, mut effects) = (Vec::new(), Vec::new());
    let settled = store.settle(&mut effects);
    drive_holding(&mut sim, &mut store, &mut effects, &mut held, batch_map);
    assert!(settled.demoted >= 1, "{settled:?}");
    assert!(
        matches!(store.found_of(&last).place, Place::Segment(seg) if seg.segment == 0),
        "the settle moved X to a record of its own segment"
    );
    assert_eq!(store.pending.len(), 1, "the settle's batch waits on its maps");
    let settled_at = store.pending[0].moves.iter().find(|it| it.key == last).unwrap().to;
    a_gate.wait();
    let a_ticket = a_put.join().unwrap();
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(2000 + n), Kind::Query, &noise(64, 2000 + n)).unwrap();
    }
    flush_holding(&mut sim, &mut store, &mut held, batch_map);
    assert!(store.durable(a_ticket), "A's batch is committed");
    assert!(store.tiers[0].head.index.is_some(), "the flush wrote the index");
    release(&mut sim, &mut store, held);
    assert_eq!(store.loc_of(&last), settled_at, "X is at the settle's record");
    assert_dead_is_exact(&store);
    sim.crash();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(store.loc_of(&last), settled_at, "the reopen finds X at the settle's record");
    assert_dead_is_exact(&store);
    assert_eq!(bytes_of(&store, &key(1)).as_ref(), Some(&out), "B's value lost X");
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out), "A's value lost X");
    assert_eq!(store.verify(None), Vec::new());
}

/// Calls `tidy` until the index swap's grace has passed and `end_migration` folded `prev`
/// into the delta. Other tests of this process hold guards, so the grace can take a moment.
fn end_the_migration(store: &mut Store) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while store.migrating.is_some() {
        store.tidy();
        assert!(std::time::Instant::now() < deadline, "the index swap's grace never passed");
        std::thread::yield_now();
    }
    assert!(
        store.tiers[0].prev.is_none(),
        "the swap ended: {:?}",
        store.broken().map(ToString::to_string)
    );
}

/// Where each table of the store's own tier holds `key`, checked against the record there: a
/// spot never names a record of another value than its own (review 11 M1).
fn assert_spots_hold_their_value(store: &Store, key: &Key) {
    let own = &store.tiers[0];
    let held = Held::writer(&*store.shared);
    for (name, table) in
        std::iter::once(("the delta", &own.delta)).chain(own.prev.iter().map(|it| ("prev", it)))
    {
        let Some((_, spot)) = table.get(held, key) else { continue };
        let header = match spot.place {
            Place::Pack(loc) => own.packs[loc.pack as usize].bytes(loc.offset, RECORD_HEADER),
            // SAFETY: a chunk a table's spot names is freed only after a move takes the spot and
            // a grace passes, and this thread, the store's, makes the moves.
            Place::Segment(seg) => unsafe { seg.chunk.get().bytes(seg.offset, RECORD_HEADER) },
        }
        .map(RecordHeader::decode);
        let Some(Ok(header)) = header else {
            panic!("{name}'s spot of {key:?} at {} names no whole record", spot.place)
        };
        assert_eq!(
            (header.key, header.content),
            (*key, spot.content),
            "{name}'s spot of {key:?} at {} names a record of another value",
            spot.place
        );
    }
}

/// Review 11 S1 and S2. W loads the view and holds before it claims X. A flush writes the index
/// and swaps it in, leaving the delta W loaded as `prev`. D, a task begun after the swap,
/// looks X up, misses it, and holds before it claims. W claims X in `prev` and ends.
/// `during`: a flush drains W's record while the swap lasts (a task begun before it keeps it
/// open), and its move points `prev`'s spot at W's record in a pack; otherwise that flush
/// comes after `end_migration`. D then claims X as `d_value` and holds before it ends, its
/// record only in its memory. The swap ends, a flush writes the index again, and the store
/// crashes: W's put of X was durable, so X must read back as W's value.
///
/// Round 11 claimed in the delta alone, where D met no spot and kept its record beside W's.
/// `end_migration` then kept the delta's spot (D's, not durable) over `prev`'s (S1); or, for
/// another value, W's record, drained after it, was a duplicate of no key's value and was not
/// written (S2). The index left X out, and the crash lost W's put. A claim goes into `prev`
/// first, so D meets W's spot there: it takes W's ticket (the same value) or is refused
/// (another), and X stays at W's record.
fn a_key_put_twice_across_an_index_swap(seed: u64, during: bool, d_value: &[u8]) {
    use std::sync::Arc;

    use crate::segment::PutAt;
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    let mut w = store.writer();
    let mut d = store.writer();
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    let x = key(9);
    let w_value = noise(64, 9);
    let (w_gate, hook) = hold_at(PutAt::Claiming);
    w.own.pause = Some(hook);
    // D holds at its count (its look missed X, its claim not made) and before it ends.
    let d_gate = Arc::new(Gate::default());
    let at = d_gate.clone();
    d.own.pause = Some(Arc::new(move |it| {
        if matches!(it, PutAt::Counted | PutAt::Written) {
            at.wait();
            at.wait();
        }
    }));
    let put = |mut writer: Writer, value: Vec<u8>| {
        std::thread::spawn(move || {
            let guard = enter();
            let result = writer.put(&guard, x, Kind::Query, &value);
            drop(guard);
            result
        })
    };
    let w_put = put(w, w_value.clone());
    w_gate.wait();
    // A task open since before the swap: its grace can't pass until this drops.
    let pin = during.then(enter);
    flush(&mut sim, &mut store);
    let first_index = store.tiers[0].head.index.expect("the flush wrote the index");
    assert!(
        store.tiers[0].prev.is_some() && store.migrating.is_some(),
        "the index was swapped in, W still in its put"
    );
    let d_put = put(d, d_value.to_vec());
    d_gate.wait();
    w_gate.wait();
    let w_ticket = w_put.join().unwrap().expect("W's put is X's first");
    if during {
        flush(&mut sim, &mut store);
        assert!(store.durable(w_ticket), "W's batch is committed");
        let place = store.tiers[0]
            .prev
            .as_ref()
            .unwrap()
            .get(Held::writer(&*store.shared), &x)
            .map(|(_, spot)| spot.place);
        assert!(
            matches!(place, Some(Place::Pack(_))),
            "prev's spot moved with W's record: {place:?}"
        );
    }
    d_gate.wait();
    d_gate.wait();
    assert_spots_hold_their_value(&store, &x);
    drop(pin);
    end_the_migration(&mut store);
    if !during {
        flush(&mut sim, &mut store);
        assert!(store.durable(w_ticket), "W's batch is committed");
    }
    assert_spots_hold_their_value(&store, &x);
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(2000 + n), Kind::Query, &noise(64, 2000 + n)).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_ne!(
        store.tiers[0].head.index,
        Some(first_index),
        "the flush wrote the index again, D still in its put"
    );
    assert_spots_hold_their_value(&store, &x);
    sim.crash();
    drop(store);
    d_gate.wait();
    let d_result = d_put.join().unwrap();
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &x), Some(w_value.clone()), "W's durable put of X was lost");
    assert_spots_hold_their_value(&store, &x);
    match d_value == w_value.as_slice() {
        true => {
            assert_eq!(d_result, Ok(w_ticket), "D's claim met W's spot in prev and took its ticket")
        }
        false => assert!(
            matches!(d_result, Err(Mismatch { .. })),
            "D's put of another value is refused: {d_result:?}"
        ),
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_key_claimed_after_its_record_in_prev_was_made_durable_keeps_that_record_through_the_end_of_the_swap()
 {
    a_key_put_twice_across_an_index_swap(55, true, &noise(64, 9));
}

#[test]
fn a_record_drained_after_the_swap_ended_is_the_home_of_the_key_another_writer_claimed_in_it() {
    a_key_put_twice_across_an_index_swap(54, false, &noise(64, 9));
}

#[test]
fn a_second_value_put_across_an_index_swap_while_the_first_is_durable_is_refused() {
    a_key_put_twice_across_an_index_swap(58, true, &noise(64, 10));
}

#[test]
fn a_second_value_put_across_an_index_swap_before_the_first_drains_is_refused() {
    a_key_put_twice_across_an_index_swap(59, false, &noise(64, 10));
}

/// Review 11 M1: C loads the view while an index swap lasts and holds before it claims X;
/// `end_migration` folds `prev` and E, after it, claims X with another value and holds before
/// it ends. C's claim wins in the `prev` its view still holds, which is no store's table now,
/// and meets E's spot in the delta: its put is refused. C's record, kept for a stale reader,
/// drains while E's is not drained yet, and is no key's home: X stays at E's record, and reads
/// back as E's value. Were the value not checked (`record_home`), C's record would stand in
/// for E's spot, and X would name a record of another value than its own.
#[test]
fn a_record_kept_in_a_folded_prev_never_stands_in_for_a_record_of_another_value() {
    use crate::segment::PutAt;
    let mut sim = Sim::new(56, quiet());
    let mut store = open(&mut sim, "store");
    let mut c = store.writer();
    let mut e = store.writer();
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].prev.is_some() && store.migrating.is_some(), "the index was swapped in");
    let x = key(9);
    let (c_value, e_value) = (noise(64, 9), noise(64, 10));
    let (c_gate, hook) = hold_at(PutAt::Claiming);
    c.own.pause = Some(hook);
    let (e_gate, hook) = hold_at(PutAt::Written);
    e.own.pause = Some(hook);
    let put = |mut writer: Writer, value: Vec<u8>| {
        std::thread::spawn(move || {
            let guard = enter();
            let result = writer.put(&guard, x, Kind::Query, &value);
            drop(guard);
            result
        })
    };
    let c_put = put(c, c_value);
    c_gate.wait();
    // C began after the swap: its task does not hold the swap's grace.
    end_the_migration(&mut store);
    let e_put = put(e, e_value.clone());
    e_gate.wait();
    c_gate.wait();
    let c_result = c_put.join().unwrap();
    assert!(
        matches!(c_result, Err(Mismatch { .. })),
        "C's put of another value is refused: {c_result:?}"
    );
    flush(&mut sim, &mut store);
    assert_spots_hold_their_value(&store, &x);
    e_gate.wait();
    let e_ticket = e_put.join().unwrap().expect("E's put is X's first in the store's delta");
    flush(&mut sim, &mut store);
    assert!(store.durable(e_ticket), "E's batch is committed");
    assert_spots_hold_their_value(&store, &x);
    assert_eq!(bytes_of(&store, &x), Some(e_value.clone()), "X is E's value");
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &x), Some(e_value), "X is E's value after a reopen");
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 12 L1: C and D load the view while an index swap lasts and hold before they claim X;
/// `end_migration` folds `prev`, and E, after it, claims X with another value in the delta and
/// holds. D wins the folded `prev` (no store's table now), meets E's spot in the delta and is
/// refused. C, putting D's value, then loses `prev`'s slot to D's spot: taking D's ticket would
/// end C's put `Ok` on a record X never reads. The delta decides, so C is refused too, and X
/// reads back as E's value.
#[test]
fn a_claim_that_loses_a_folded_prev_to_a_refused_put_is_answered_by_the_delta() {
    use crate::segment::PutAt;
    let mut sim = Sim::new(60, quiet());
    let mut store = open(&mut sim, "store");
    let (mut c, mut d, mut e) = (store.writer(), store.writer(), store.writer());
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].prev.is_some() && store.migrating.is_some(), "the index was swapped in");
    let x = key(9);
    let (d_value, e_value) = (noise(64, 9), noise(64, 10));
    let (c_gate, hook) = hold_at(PutAt::Claiming);
    c.own.pause = Some(hook);
    let (d_gate, hook) = hold_at(PutAt::Claiming);
    d.own.pause = Some(hook);
    let (e_gate, hook) = hold_at(PutAt::Written);
    e.own.pause = Some(hook);
    let put = |mut writer: Writer, value: Vec<u8>| {
        std::thread::spawn(move || {
            let guard = enter();
            let result = writer.put(&guard, x, Kind::Query, &value);
            drop(guard);
            result
        })
    };
    let c_put = put(c, d_value.clone());
    c_gate.wait();
    let d_put = put(d, d_value);
    d_gate.wait();
    // C and D began after the swap: their tasks do not hold its grace.
    end_the_migration(&mut store);
    let e_put = put(e, e_value.clone());
    e_gate.wait();
    d_gate.wait();
    let d_result = d_put.join().unwrap();
    assert!(
        matches!(d_result, Err(Mismatch { .. })),
        "D won the folded prev and met E's value in the delta: {d_result:?}"
    );
    c_gate.wait();
    let c_result = c_put.join().unwrap();
    assert!(
        matches!(c_result, Err(Mismatch { kept, .. }) if kept == content_of(&e_value)),
        "C lost the folded prev to D's refused put: the delta's value decides: {c_result:?}"
    );
    e_gate.wait();
    let e_ticket = e_put.join().unwrap().expect("E's put is X's first in the store's delta");
    flush(&mut sim, &mut store);
    assert!(store.durable(e_ticket), "E's batch is committed");
    assert_spots_hold_their_value(&store, &x);
    sim.crash();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &x), Some(e_value), "X is E's value after a reopen");
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 12 L1, its narrower window: D, its view loaded while an index swap lasted, wins the
/// folded `prev` and holds before its claim reaches the delta. C, from the same view, loses
/// `prev`'s slot to D's spot while the delta lacks the key. Were C to take D's ticket and end,
/// E, after it, could win the delta with another value and D be refused: C's put would be
/// `Ok` on a record X never reads. C puts D's spot into the delta before it ends, so E is
/// refused, D is kept, and X reads back as D's value, C's ticket D's.
#[test]
fn a_claim_that_meets_a_put_between_prev_and_the_delta_fixes_the_deltas_value_before_it_ends() {
    use std::sync::Arc;

    use crate::segment::PutAt;
    let mut sim = Sim::new(61, quiet());
    let mut store = open(&mut sim, "store");
    let (mut c, mut d, e) = (store.writer(), store.writer(), store.writer());
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].prev.is_some() && store.migrating.is_some(), "the index was swapped in");
    let x = key(9);
    let (value, e_value) = (noise(64, 9), noise(64, 10));
    let (c_gate, hook) = hold_at(PutAt::Claiming);
    c.own.pause = Some(hook);
    // D holds before it claims, and again once it won `prev`.
    let d_gate = Arc::new(Gate::default());
    let at = d_gate.clone();
    d.own.pause = Some(Arc::new(move |it| {
        if matches!(it, PutAt::Claiming | PutAt::InPrev) {
            at.wait();
            at.wait();
        }
    }));
    let put = |mut writer: Writer, value: Vec<u8>| {
        std::thread::spawn(move || {
            let guard = enter();
            let result = writer.put(&guard, x, Kind::Query, &value);
            drop(guard);
            result
        })
    };
    let c_put = put(c, value.clone());
    c_gate.wait();
    let d_put = put(d, value.clone());
    d_gate.wait();
    end_the_migration(&mut store);
    d_gate.wait();
    d_gate.wait();
    c_gate.wait();
    let c_ticket = c_put.join().unwrap().expect("C met D's spot of its value");
    let e_result = put(e, e_value).join().unwrap();
    assert!(
        matches!(e_result, Err(Mismatch { .. })),
        "E met D's spot, put into the delta by C: {e_result:?}"
    );
    d_gate.wait();
    let d_ticket = d_put.join().unwrap().expect("D met its own spot in the delta");
    assert_eq!(c_ticket, d_ticket, "C took D's ticket");
    flush(&mut sim, &mut store);
    assert!(store.durable(c_ticket), "D's batch is committed");
    assert_spots_hold_their_value(&store, &x);
    sim.crash();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &x), Some(value), "X is D's value after a reopen");
    assert_eq!(store.verify(None), Vec::new());
}

/// Where the store's `prev` and its delta hold `key`.
fn places_of(store: &Store, key: &Key) -> (Option<Place>, Option<Place>) {
    let held = Held::writer(&*store.shared);
    let own = &store.tiers[0];
    let place = |table: &Table<Spot>| table.get(held, key).map(|(_, spot)| spot.place);
    (own.prev.as_deref().and_then(place), place(&own.delta))
}

/// When, in [`a_move_of_prevs_spot_meets_a_claim_that_lost_to_it`], C's copy of `prev`'s spot
/// lands against the move of W's record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CopyLands {
    /// Before the move: the move finds W's segment spot in the delta and replaces it.
    First,
    /// Between the move's probe of the delta (a miss) and its insert, which then meets the
    /// copy and replaces it.
    Between,
    /// After the move: C meets the pack spot in the delta and copies nothing.
    After,
}

/// Review 12b H1: W loads the view before an index swap and claims X after the swap's copy, in
/// `prev` alone, so the delta lacks X. C, a task begun after the swap, misses X, loses `prev`'s
/// slot to W's segment spot and holds before it reads the delta. A flush drains W's record and
/// its move points `prev`'s spot at the pack; C's copy of W's segment spot lands in the delta
/// as `lands` says. Had the move left the delta alone, a copy that lands after the move's
/// probe would keep W's segment spot there, into a chunk the batch frees, and the end of the
/// swap would find the two tables apart. The move puts its pack spot into the delta as well,
/// so it and the copy meet at the delta's one slot: the two tables hold X at one place, and
/// the swap ends whole.
fn a_move_of_prevs_spot_meets_a_claim_that_lost_to_it(seed: u64, lands: CopyLands) {
    use std::sync::Arc;

    use crate::segment::PutAt;
    let mut sim = Sim::new(seed, quiet());
    let mut store = open(&mut sim, "store");
    let (mut w, mut c) = (store.writer(), store.writer());
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    let x = key(9);
    let value = noise(64, 9);
    let (w_gate, hook) = hold_at(PutAt::Claiming);
    w.own.pause = Some(hook);
    // C holds at its count (its look missed X) and once it lost `prev`'s slot, and meets
    // `c_done` once its put returned.
    let (c_gate, c_done) = (Arc::new(Gate::default()), Arc::new(Gate::default()));
    let at = c_gate.clone();
    c.own.pause = Some(Arc::new(move |it| {
        if matches!(it, PutAt::Counted | PutAt::LostInPrev) {
            at.wait();
            at.wait();
        }
    }));
    let w_put = std::thread::spawn({
        let value = value.clone();
        move || {
            let guard = enter();
            let result = w.put(&guard, x, Kind::Query, &value);
            drop(guard);
            result
        }
    });
    w_gate.wait();
    // A task open since before the swap: its grace can't pass until this drops.
    let pin = enter();
    flush(&mut sim, &mut store);
    assert!(
        store.tiers[0].prev.is_some() && store.migrating.is_some(),
        "the index was swapped in, W still in its put"
    );
    let c_put = std::thread::spawn({
        let (value, done) = (value.clone(), c_done.clone());
        move || {
            let guard = enter();
            let result = c.put(&guard, x, Kind::Query, &value);
            drop(guard);
            done.wait();
            result
        }
    });
    c_gate.wait();
    w_gate.wait();
    let w_ticket = w_put.join().unwrap().expect("W's put is X's first");
    assert!(
        matches!(places_of(&store, &x), (Some(Place::Segment(_)), None)),
        "W claimed X in prev alone: {:?}",
        places_of(&store, &x)
    );
    c_gate.wait();
    c_gate.wait();
    match lands {
        CopyLands::First => {
            c_gate.wait();
            c_done.wait();
            assert!(
                matches!(places_of(&store, &x), (Some(Place::Segment(a)), Some(Place::Segment(b))) if a == b),
                "C copied W's spot"
            );
            flush(&mut sim, &mut store);
        }
        CopyLands::Between => {
            let (gate, done, mut once) = (c_gate.clone(), c_done.clone(), true);
            store.move_probed = Some(Box::new(move |moved| {
                if moved == x && std::mem::take(&mut once) {
                    gate.wait();
                    done.wait();
                }
            }));
            flush(&mut sim, &mut store);
            assert!(store.move_probed.take().is_some());
        }
        CopyLands::After => {
            flush(&mut sim, &mut store);
            c_gate.wait();
            c_done.wait();
        }
    }
    assert!(store.durable(w_ticket), "W's batch is committed");
    assert_eq!(c_put.join().unwrap(), Ok(w_ticket), "C met W's spot and took its ticket");
    let (in_prev, in_delta) = places_of(&store, &x);
    drop(pin);
    end_the_migration(&mut store);
    assert!(
        matches!(in_prev, Some(Place::Pack(_))),
        "the move pointed prev's spot at the pack: {in_prev:?}"
    );
    assert_eq!(in_delta, in_prev, "the delta holds X where prev does");
    assert_spots_hold_their_value(&store, &x);
    sim.crash();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &x), Some(value), "X is W's value after a reopen");
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_move_of_prevs_spot_and_a_claim_that_lost_to_it_meet_at_the_deltas_slot() {
    a_move_of_prevs_spot_meets_a_claim_that_lost_to_it(62, CopyLands::After);
}

#[test]
fn a_move_of_prevs_spot_replaces_the_copy_a_claim_that_lost_to_it_put_in_the_delta_first() {
    a_move_of_prevs_spot_meets_a_claim_that_lost_to_it(64, CopyLands::First);
}

#[test]
fn a_move_of_prevs_spot_replaces_a_copy_that_lands_between_its_probe_and_its_insert() {
    a_move_of_prevs_spot_meets_a_claim_that_lost_to_it(65, CopyLands::Between);
}

/// Review 12c M2: D, a task begun after an index swap, wins chunk X of an output in `prev` and
/// holds before its claim reaches the delta. P puts an output sharing X: it finds D's record,
/// not durable, and keeps a copy of X, and a flush drains it, its move standing in for D's
/// record (`Store::copy_home`) and taking `prev`'s spot while D is between its inserts. The
/// move puts its pack spot into the delta too; D then meets it there, of the same value, and
/// is kept, its record drained later as a duplicate. Had the move left the delta alone, D
/// would win the delta with its segment spot, and the end of the swap would find the two
/// tables apart.
#[test]
fn a_chunks_copy_moved_while_its_first_claim_is_between_prev_and_the_delta_leaves_both_at_the_pack()
{
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use crate::segment::PutAt;
    let mut sim = Sim::new(63, quiet());
    let mut store = open(&mut sim, "store");
    let (mut d, p) = (store.writer(), store.writer());
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    let out = output_with_a_short_last_chunk();
    let x = chunk_keys(&out)[0];
    // D holds once it won its first claim, X's, in `prev`.
    let d_gate = Arc::new(Gate::default());
    let (at, held) = (d_gate.clone(), AtomicBool::new(false));
    d.own.pause = Some(Arc::new(move |it| {
        if it == PutAt::InPrev && !held.swap(true, Ordering::Relaxed) {
            at.wait();
            at.wait();
        }
    }));
    // A task open since before the swap: its grace can't pass until this drops.
    let pin = enter();
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].prev.is_some() && store.migrating.is_some(), "the index was swapped in");
    let put = |mut writer: Writer, at: Key, out: Vec<u8>| {
        std::thread::spawn(move || {
            let guard = enter();
            let result = writer.put(&guard, at, Kind::Output, &out);
            drop(guard);
            result
        })
    };
    let d_put = put(d, key(1), out.clone());
    d_gate.wait();
    assert!(
        matches!(places_of(&store, &x), (Some(Place::Segment(_)), None)),
        "D won X in prev alone: {:?}",
        places_of(&store, &x)
    );
    let p_ticket =
        put(p, key(2), out.clone()).join().unwrap().expect("P's output is key 2's first");
    flush(&mut sim, &mut store);
    assert!(store.durable(p_ticket), "P's batch is committed");
    let (in_prev, in_delta) = places_of(&store, &x);
    d_gate.wait();
    let d_ticket = d_put.join().unwrap().expect("D's output is key 1's first");
    drop(pin);
    end_the_migration(&mut store);
    assert!(
        matches!(in_prev, Some(Place::Pack(_))),
        "the copy's move pointed prev's spot at the pack: {in_prev:?}"
    );
    assert_eq!(in_delta, in_prev, "the delta holds X where prev does");
    flush(&mut sim, &mut store);
    assert!(store.durable(d_ticket), "D's batch is committed");
    assert_spots_hold_their_value(&store, &x);
    sim.crash();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).as_ref(), Some(&out), "D's output lost X");
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out), "P's output lost X");
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 12c M1: a move of `prev`'s spot during an index swap that finds the delta holding
/// the key at another place (planted here by hand, as no claim, copy or move puts one) breaks
/// the store at once, naming the batch, the key, the move and both places, before readers,
/// which find the delta first, are left on it until the swap ends. The spot is planted after
/// the drain, which checks the two tables agree (`Store::record_home`), and before the move.
#[test]
fn a_move_of_prevs_spot_that_finds_the_delta_apart_breaks_the_store() {
    let mut sim = Sim::new(66, quiet());
    let mut store = open(&mut sim, "store");
    let _other = store.writer();
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(1000 + n), Kind::Query, &noise(64, 1000 + n)).unwrap();
    }
    // A task open since before the swap: its grace can't pass until this drops.
    let pin = enter();
    flush(&mut sim, &mut store);
    let x = key(9);
    store.put(x, Kind::Query, &noise(64, 9)).unwrap();
    assert!(store.tiers[0].prev.is_some(), "the index was swapped in");
    let (Some(Place::Segment(_)), Some(Place::Segment(_))) = places_of(&store, &x) else {
        panic!("X is claimed in both tables")
    };
    // X's record drained, its move waiting on its maps: the drain checked the tables agree.
    let mut held = Vec::new();
    flush_holding(&mut sim, &mut store, &mut held, batch_map);
    assert!(store.pending.iter().any(|it| it.moves.iter().any(|it| it.key == x)), "X's move waits");
    let elsewhere = store.loc_of(&key(1000));
    {
        let held = Held::writer(&*store.shared);
        let delta = &store.tiers[0].delta;
        let (_, spot) = delta.get(held, &x).unwrap();
        let planted =
            Spot::new(x, content_of(&noise(64, 1000)), Place::Pack(elsewhere), spot.ticket);
        assert!(delta.replace(held, planted).is_ok());
    }
    release(&mut sim, &mut store, held);
    let broken = store.broken().expect("the move broke the store").to_string();
    assert!(broken.contains(&format!("moved {:x} from segment", x.0)), "{broken}");
    assert!(
        broken.contains(&format!(
            "during an index swap, and the delta holds it as {:x} at {}",
            content_of(&noise(64, 1000)),
            Place::Pack(elsewhere)
        )),
        "{broken}"
    );
    let mut effects = Vec::new();
    store.flush(&mut effects);
    assert!(effects.is_empty(), "a broken store commits nothing");
    drop(pin);
}

/// Review 11 C1: a lost move (a batch's move into a pack a settle retired, which `retirable`
/// rules out, broken here by hand) breaks the store, naming the batch. A map of a later batch
/// that comes after the break makes no move and frees no chunk (the lost batch's chunks are
/// still first in the segment, and freeing past them panicked), and nothing more is
/// committed: no drain, no head. A commit in flight at the break finishes its pack writes and
/// writes no head (review 12): a crash and a reopen read the head from before the break.
#[test]
fn a_map_that_comes_after_a_lost_move_broke_the_store_changes_nothing() {
    let mut sim = Sim::new(57, quiet());
    let mut store = open(&mut sim, "store");
    let other = store.unit(writer(1));
    // Each record is longer than a segment chunk: each put rolls the store's segment, so each
    // batch frees a chunk of it.
    store.put(key(1), Kind::Query, &noise(12 << 10, 1)).unwrap();
    let (mut first, mut second) = (Vec::new(), Vec::new());
    flush_holding(&mut sim, &mut store, &mut first, batch_map);
    store.put_in(other, key(2), Kind::Query, &noise(12 << 10, 2)).unwrap();
    flush_holding(&mut sim, &mut store, &mut second, batch_map);
    assert_eq!(store.pending.len(), 2, "both batches wait on their maps");
    let (id, lost) = (store.pending[0].id, store.pending[0].moves[0].to.pack);
    assert!(store.pending.iter().all(|it| !it.chunks.is_empty()), "each batch frees a chunk");
    assert!(
        store.pending[1].moves.iter().all(|it| it.to.pack != lost),
        "the second batch moves into another pack"
    );
    store.tiers[0].packs[lost as usize].retiring = true;
    // A third commit in flight at the break: its steps are asked for and held.
    let head = store.tiers[0].head.clone();
    store.put(key(3), Kind::Query, &noise(64, 3)).unwrap();
    let mut third = Vec::new();
    store.flush(&mut third);
    assert!(!third.is_empty(), "the third commit started");
    release(&mut sim, &mut store, first);
    let broken = store.broken().expect("the lost move broke the store").to_string();
    // Its steps finish after the break: it writes no head.
    let mut head_asked = false;
    run_until_idle(&mut sim, &mut third, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
            head_asked |= store.tags.ops.values().any(|op| matches!(op, Op::WriteHead));
        }
    }).expect("the effects end");
    assert!(!head_asked, "a commit in flight at the break writes no head");
    assert!(
        store.committing(),
        "the withheld commit stays in flight: no later commit starts behind it"
    );
    assert_eq!(store.tiers[0].head, head, "the store's head is the one from before the break");
    let steps = store.commit_steps();
    release(&mut sim, &mut store, second);
    assert!(broken.contains(&format!("batch {id} moved {:x} into", key(1).0)), "{broken}");
    assert_eq!(store.broken().map(ToString::to_string), Some(broken), "the first break is kept");
    assert_eq!(store.pending.len(), 1, "the later batch's moves are not made");
    let mut effects = Vec::new();
    store.flush(&mut effects);
    assert!(effects.is_empty(), "a broken store commits nothing");
    assert_eq!(store.commit_steps(), steps, "no step of a commit ran after the break");
    assert!(store.committing(), "the withheld commit is still in flight");
    sim.crash();
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(store.tiers[0].head, head, "the reopen reads the head from before the break");
    assert_eq!(bytes_of(&store, &key(3)), None, "the third commit's put was never committed");
    assert_eq!(bytes_of(&store, &key(1)), Some(noise(12 << 10, 1)));
    assert_eq!(bytes_of(&store, &key(2)), Some(noise(12 << 10, 2)));
    assert_eq!(store.verify(None), Vec::new());
}

/// Rule 20 for review 9 C1: of a key's moves, `finish_batches` makes the first from where the
/// key is, and an index written while they wait names that move's record: not the key's first
/// move (one from a place it left, as a stale copy's), nor its last (one more from where it is).
#[test]
fn the_index_names_the_record_of_the_first_move_from_where_its_key_is() {
    let mut sim = Sim::new(50, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, &noise(64, 1)).unwrap();
    let Place::Segment(at) = store.found_of(&key(1)).place else {
        panic!("key 1 is in the segment")
    };
    let mut held = Vec::new();
    flush_holding(&mut sim, &mut store, &mut held, batch_map);
    let batch = store.pending.back_mut().unwrap();
    let made = *batch.moves.iter().find(|it| it.key == key(1)).unwrap();
    assert_eq!(made.from, at);
    let to = |by: u64| Loc { offset: made.to.offset + by, ..made.to };
    batch.moves.insert(
        0,
        Move { key: key(1), from: Seg { offset: at.offset + 4096, ..at }, to: to(4096) },
    );
    batch.moves.push(Move { key: key(1), from: at, to: to(8192) });
    let indexed = store.index_entries(&mut Vec::new()).into_iter().find(|(it, _)| *it == key(1));
    assert_eq!(
        indexed,
        Some((key(1), made.to)),
        "the index names the first move from where the key is"
    );
    release(&mut sim, &mut store, held);
    assert_eq!(store.loc_of(&key(1)), made.to, "the first move from where the key is is made");
}

/// Review 9 C3: a record is counted dead once. B's copy of the last chunk is its home (the
/// claim race), and A's record of it, drained later, a duplicate. A reopen with no index since
/// reads every record past what its head indexes and takes the last of each key: had A's
/// record been written to a pack and counted dead there, the reopen would point the chunk at
/// it, and a settle moving it cold would count it dead again, until a pack's dead bytes pass
/// its length and its head no longer opens. A put between the two flushes rolls the pack, so
/// B's copy and A's record would be in packs of their own. Dead bytes here stay exactly the
/// records no key finds, through the reopen and the settle, and the store opens after.
#[test]
fn a_record_is_counted_dead_once_across_a_reopen_and_a_settle_that_moves_it() {
    let (mut sim, mut store, out, a_gate, a_put) =
        a_chunk_claim_race(51, 0, output_with_a_last_chunk_a_settle_moves());
    let last = *chunk_keys(&out).last().unwrap();
    let home = store.loc_of(&last);
    store.put(key(3), Kind::Output, &noise(CHUNK_FROM - 1, 3)).unwrap();
    a_gate.wait();
    a_put.join().unwrap();
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].packs.len() > home.pack as usize + 1, "the pack rolled past B's copy");
    assert_dead_is_exact(&store);
    drop(store);
    let mut store = open(&mut sim, "store");
    assert_eq!(store.loc_of(&last), home, "the reopen finds the chunk at B's copy");
    assert_dead_is_exact(&store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let settled = settle(&mut sim, &mut store);
    assert!(settled.demoted >= 1, "{settled:?}");
    assert_eq!(form_of(&store, &last), Form::Zstd, "the settle moved the chunk cold");
    assert_dead_is_exact(&store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).as_ref(), Some(&out));
    assert_eq!(bytes_of(&store, &key(2)).as_ref(), Some(&out));
    assert_eq!(store.verify(None), Vec::new());
}

/// Review 9 (notes): maps answer in any order, so a pack a batch moves records into can be
/// mapped while the batch still waits on another file's map. A settle then must not retire the
/// pack: the batch's records are not yet what their keys find, so the rewrite would leave
/// them out, and the batch would then point their keys into a removed pack. Once the batch's
/// moves are made, the pack is retired as any other.
#[test]
fn a_pack_a_batch_waiting_on_its_maps_moves_records_into_is_not_retired() {
    let mut sim = Sim::new(52, quiet());
    let mut store = open(&mut sim, "store");
    let value = |n: u64| if n < 100 { text(n, 12 << 10) } else { noise(16 << 10, n) };
    let keys: Vec<u64> = (0..12).chain(100..116).collect();
    for &n in &keys[..12] {
        store.put(key(n), Kind::Code, &value(n)).unwrap();
    }
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let settled = settle(&mut sim, &mut store);
    assert_eq!(settled.demoted, 12, "{settled:?}");
    for &n in &keys[12..] {
        store.put(key(n), Kind::Code, &value(n)).unwrap();
    }
    let mut held = Vec::new();
    flush_holding(&mut sim, &mut store, &mut held, |what| {
        matches!(what, Mapped::Pack { pack: 1, batch: Some(_), .. })
    });
    assert_eq!(store.pending.len(), 1, "the batch waits on its map of pack 1");
    let pack = &store.tiers[0].packs[0];
    assert!(store.tiers[0].packs.len() == 2 && store.tiers[0].dictionaries.is_empty());
    assert!(pack.usable == pack.committed, "pack 0 is mapped");
    assert!(pack.dead * 2 >= pack.committed - PACK_HEADER, "pack 0 is half dead");
    assert!(
        store.pending[0].moves.iter().any(|it| it.to.pack == 0),
        "the batch moves records into pack 0"
    );
    let settled = settle(&mut sim, &mut store);
    release(&mut sim, &mut store, held);
    for &n in &keys {
        assert_eq!(bytes_of(&store, &key(n)), Some(value(n)), "{n}");
    }
    assert_eq!(settled.retired, 0, "{settled:?}");
    let settled = settle(&mut sim, &mut store);
    assert!(settled.retired >= 1 && store.tiers[0].packs[0].removed, "{settled:?}");
    drop(store);
    let store = open(&mut sim, "store");
    for &n in &keys {
        assert_eq!(bytes_of(&store, &key(n)), Some(value(n)), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

/// The crash test: at any point of any number of commits, under injected IO failures, a crash
/// loses no put that was reported durable, and the store opens with only whole values.
#[test]
fn a_crash_at_any_point_loses_nothing_durable_and_serves_nothing_torn() {
    for seed in 0..120 {
        let mut sim = Sim::new(seed, Faults { fail_one_in: 9, ..Faults::default() });
        let mut expected: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        let mut durable_keys: Vec<Key> = Vec::new();
        let mut next = 0u64;
        for round in 0..4 {
            let mut store = open(&mut sim, "store");
            for (key, bytes) in &expected {
                if let Some(found) = bytes_of(&store, key) {
                    assert_eq!(&found, bytes, "seed {seed} round {round}");
                }
            }
            for key in &durable_keys {
                assert!(store.has(key), "seed {seed} round {round}: a durable put was lost");
            }
            assert_eq!(store.verify(None), Vec::new(), "seed {seed} round {round}");
            let mut tickets = BTreeMap::new();
            let count = 5 + sim.rng().below(40);
            for _ in 0..count {
                next += 1;
                let (kind, bytes) = match next % 5 {
                    0 => (Kind::Run, run_output(next)),
                    1 => (Kind::Code, noise(200 + (next as usize * 37) % 3000, next)),
                    2 if seed % 4 == 0 => (Kind::Output, noise(300 << 10, next)),
                    3 if seed % 4 == 1 => (Kind::Query, noise(100 << 10, next)),
                    4 if seed % 3 == 2 => (Kind::Code, text(next, 6 << 10)),
                    _ => (Kind::Query, noise(64, next)),
                };
                let ticket = store.put(key(next), kind, &bytes).unwrap();
                tickets.insert(ticket, key(next));
                expected.insert(key(next), bytes);
            }
            let mut effects = Vec::new();
            if seed % 3 == 2 {
                for _ in 0..2 {
                    store.settle(&mut effects);
                    for ticket in drive(&mut sim, &mut store, &mut effects) {
                        durable_keys.extend(tickets.get(&ticket).copied());
                    }
                }
                store.settle(&mut effects);
            }
            store.flush(&mut effects);
            let polls = sim.rng().below(40);
            for _ in 0..polls {
                sim.submit(&mut effects);
                let mut out = Vec::new();
                sim.poll(&mut out, blaze_core::wait::Deadline::NEVER);
                for completion in out {
                    if store.owns(completion.tag) {
                        for ticket in store.complete(completion, &mut effects) {
                            durable_keys.extend(tickets.get(&ticket).copied());
                        }
                    }
                }
            }
            if let Some(broken) = store.broken() {
                // Its commits met an injected failure `FAILURES_MOST` times in a row: it stops, as
                // on a failing disk, and the next round checks it lost nothing durable.
                assert!(
                    broken.what == "injected by the simulation" && store.failures >= FAILURES_MOST,
                    "seed {seed}: {broken:?} after {} failures in a row",
                    store.failures
                );
            }
            sim.crash();
        }
    }
}

#[test]
fn a_flipped_byte_of_a_value_is_never_served() {
    let mut sim = Sim::new(4, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, &[b'a'; 1000]).unwrap();
    store.put(key(2), Kind::Query, &[b'b'; 1000]).unwrap();
    flush(&mut sim, &mut store);
    let loc = store.loc_of(&key(2));
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[(loc.offset + RECORD_HEADER) as usize + 500] ^= 1;
    sim.change(&pack, &bytes);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), vec![b'a'; 1000]);
    let corrupt = store.get(&key(2)).unwrap_err();
    assert_eq!(corrupt.pack, pack);
    assert!(corrupt.what.contains("hash"), "{corrupt}");
    let found = store.verify(None);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].key, key(2));
    assert_eq!(store.verify(Some(Kind::Code)), Vec::new());
}

#[test]
fn a_record_header_that_is_not_whole_stops_the_store_naming_its_pack() {
    let mut sim = Sim::new(5, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &[7; 100]).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[PACK_HEADER as usize + 40] ^= 0xff;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    assert_eq!(store.broken().unwrap().file, pack);
}

#[test]
fn a_head_of_another_format_is_refused_by_name() {
    let mut sim = Sim::new(6, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &[1]).unwrap();
    flush(&mut sim, &mut store);
    let head = name("store/head");
    let mut bytes = sim.contents(&head);
    bytes[8] = 99;
    sim.change(&head, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().unwrap();
    assert_eq!(broken.file, head);
    assert!(broken.what.contains("format 99"), "{broken}");
}

#[test]
fn near_identical_outputs_share_their_chunks() {
    let mut sim = Sim::new(7, quiet());
    let mut store = open(&mut sim, "store");
    let output_bytes =
        |store: &Store| -> u64 { store.tiers[0].packs.iter().map(|it| it.end).sum() };
    let first = noise(2 << 20, 70);
    let mut second = first.clone();
    second.splice(5000..5000, *b"a patched function").for_each(drop);
    second[1 << 20] ^= 1;
    store.put(key(1), Kind::Output, &first).unwrap();
    flush(&mut sim, &mut store);
    let after_first = output_bytes(&store);
    store.put(key(2), Kind::Output, &second).unwrap();
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), first);
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), second, "read from its segment");
    flush(&mut sim, &mut store);
    let added = output_bytes(&store) - after_first;
    assert!(added * 4 < first.len() as u64, "the second output added {added} bytes");
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), second);
    assert_eq!(store.verify(Some(Kind::Output)), Vec::new());
}

#[test]
fn a_cold_kind_trains_its_dictionary_and_compresses_what_follows() {
    let mut sim = Sim::new(8, quiet());
    let mut store = open(&mut sim, "store");
    let count = cold::TRAIN_VALUES as u64 + 40;
    for n in 0..count {
        store.put(key(n), Kind::Run, &run_output(n)).unwrap();
    }
    assert_eq!(store.problems(), &[] as &[String]);
    assert_eq!(store.tiers[0].dictionaries.len(), 1, "one dictionary for runs");
    let (form, stored, raw) = store.sizes_of(&key(count - 1));
    assert_eq!(form, Form::Zstd);
    assert!(stored * 3 < raw, "{stored} stored of {raw}");
    assert_eq!(form_of(&store, &key(0)), Form::Raw);
    for n in 0..count {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), run_output(n));
    }
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..count {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), run_output(n), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

/// A kind of short values (debug info, a few hundred bytes each) fills the samples kept,
/// [`cold::SAMPLES_MOST`], before their bytes reach [`cold::TRAIN_BYTES`]: it is trained then,
/// not left raw for good. A failed training waits for twice its samples, which a full set
/// never gets.
#[test]
fn a_kind_of_short_values_trains_once_its_samples_are_full() {
    let short = (cold::TRAIN_BYTES / cold::SAMPLES_MOST as u64 / 2).max(1);
    let kept = |samples: usize, after: usize| Training {
        samples: vec![Vec::new(); samples],
        bytes: samples as u64 * short,
        after,
        ..Training::default()
    };
    assert!(!kept(cold::TRAIN_VALUES, 0).ready(), "too few bytes, and samples to keep");
    assert!(!kept(cold::SAMPLES_MOST - 1, 0).ready());
    assert!(kept(cold::SAMPLES_MOST, 0).ready(), "the samples kept are full");
    assert!(
        !kept(cold::SAMPLES_MOST, cold::SAMPLES_MOST * 2).ready(),
        "it failed on these samples"
    );
    let long = Training {
        samples: vec![Vec::new(); cold::TRAIN_VALUES],
        bytes: cold::TRAIN_BYTES,
        ..Training::default()
    };
    assert!(long.ready(), "values and bytes enough");
    assert!(!Training { samples: vec![Vec::new(); cold::TRAIN_VALUES - 1], ..long }.ready());
}

/// A short cold record of a kind with no dictionary is met by every settle, to be sampled
/// after a reopen: it is sampled once, as a value put while the store is open is, so settles
/// never fill the samples with copies and train on fewer values than a training needs.
#[test]
fn settles_sample_each_short_cold_value_once() {
    let mut sim = Sim::new(61, quiet());
    let mut store = open(&mut sim, "store");
    let value = |n: u64| text(n, 200);
    for n in 0..100 {
        store.put(key(n), Kind::Debug, &value(n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let debug = Kind::Debug.code() as usize;
    assert_eq!(store.training[debug].samples.len(), 100, "each put is sampled once");
    for _ in 0..50 {
        settle(&mut sim, &mut store);
    }
    assert_eq!(
        store.training[debug].samples.len(),
        100,
        "a settle samples nothing sampled already"
    );
    drop(store);
    let mut store = open(&mut sim, "store");
    for _ in 0..50 {
        settle(&mut sim, &mut store);
    }
    assert_eq!(
        store.training[debug].samples.len(),
        100,
        "after a reopen, settles sample each record once"
    );
    assert!(
        store.tiers[0].dictionaries.is_empty(),
        "100 values neither fill the samples nor reach the bytes"
    );
    assert_eq!(store.problems(), &[] as &[String]);
    for n in 0..100 {
        assert_eq!(form_of(&store, &key(n)), Form::Raw, "{n}: no dictionary, so it stays");
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), value(n));
    }
}

#[test]
fn a_large_value_is_a_file_of_its_own_placed_by_sharing_blocks() {
    let mut sim = Sim::new(9, quiet());
    let mut store = open(&mut sim, "store");
    let big = noise(EXTERNAL_FROM + 10, 90);
    store.put(key(1), Kind::Image, &big).unwrap();
    store.put(key(2), Kind::Code, b"small").unwrap();
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), big);
    flush(&mut sim, &mut store);
    let file = name(&format!("store/{}", disk::external_file(&content_of(&big))));
    assert_eq!(sim.contents(&file), big);
    let mut effects = Vec::new();
    let a = store.place(&key(1), name("target/image"), &mut effects).unwrap();
    let b = store.place(&key(2), name("target/small"), &mut effects).unwrap();
    assert_eq!(store.place(&key(3), name("target/none"), &mut effects), None);
    drive(&mut sim, &mut store, &mut effects);
    let placed: BTreeMap<Tag, Result<(), String>> =
        store.placed().into_iter().map(|it| (it.tag, it.result)).collect();
    assert_eq!(placed[&a], Ok(()));
    assert_eq!(placed[&b], Ok(()));
    assert_eq!(sim.contents(&name("target/image")), big);
    assert_eq!(sim.contents(&name("target/small")), b"small");
    // Placing again over what is there replaces it.
    let again = store.place(&key(1), name("target/image"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let cloned = PlacedBytes { cloned: big.len() as u64, ..PlacedBytes::default() };
    assert_eq!(
        store.placed()[0],
        Placed { tag: again, to: name("target/image"), result: Ok(()), bytes: cloned }
    );
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), big);
}

#[test]
fn where_blocks_cannot_be_shared_a_place_fails_and_copies_nothing() {
    let mut sim = Sim::new(10, Faults { no_clone: true, ..Faults::default() });
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, &noise(EXTERNAL_FROM, 100)).unwrap();
    flush(&mut sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("out/q"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert!(matches!(&placed[0].result, Err(why) if why.contains("does not copy")), "{placed:?}");
    assert!(!sim.exists(&name("out/q")));
}

#[test]
fn an_output_is_written_once_and_cloned_to_every_place() {
    let mut sim = Sim::new(11, quiet());
    let mut store = open(&mut sim, "store");
    let out = noise(CHUNK_FROM as usize + 10, 110);
    let len = out.len() as u64;
    store.put(key(1), Kind::Output, &out).unwrap();
    flush(&mut sim, &mut store);
    let file = name(&format!("store/placed/{:x}", content_of(&out)));
    let mut effects = Vec::new();
    let a = store.place(&key(1), name("one/build/a"), &mut effects).unwrap();
    let b = store.place(&key(1), name("two/build/a"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed: BTreeMap<Tag, Placed> = store.placed().into_iter().map(|it| (it.tag, it)).collect();
    assert_eq!(placed[&a].result, Ok(()));
    assert_eq!(placed[&b].result, Ok(()));
    let mut bytes = placed[&a].bytes;
    bytes += placed[&b].bytes;
    assert_eq!(
        bytes,
        PlacedBytes { cloned: 2 * len, written: len, copied: 0 },
        "written once, cloned twice"
    );
    assert_eq!(sim.contents(&name("one/build/a")), out);
    assert_eq!(sim.contents(&name("two/build/a")), out);
    assert_eq!(sim.mode(&file), Some(PLACED_MODE), "the store's copy is read-only");
    assert_eq!(sim.mode(&name("one/build/a")), Some(0o644), "a clone is a written file's mode");
    // Let go of, the whole file goes; the value and every clone stay, and placing again
    // writes it again.
    store.unplace(&key(1), &key(9), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!sim.exists(&file));
    assert_eq!(sim.contents(&name("two/build/a")), out);
    let again = store.place(&key(1), name("one/build/a"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert_eq!(placed[0].tag, again);
    assert_eq!(placed[0].bytes, PlacedBytes { cloned: len, written: len, copied: 0 });
    assert_eq!(store.problems(), &[] as &[String]);
}

/// One output of `len` bytes put as key 1 and placed at `one/build/a`: the store, its bytes and
/// the name of the whole file it is cloned from.
fn placed_once(sim: &mut Sim, seed: u64) -> (Store, Vec<u8>, FileName) {
    let mut store = open(sim, "store");
    let out = noise(CHUNK_FROM as usize + 10, seed);
    store.put(key(1), Kind::Output, &out).unwrap();
    flush(sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("one/build/a"), &mut effects).unwrap();
    drive(sim, &mut store, &mut effects);
    assert_eq!(store.placed()[0].result, Ok(()));
    let file = name(&format!("store/placed/{:x}", content_of(&out)));
    (store, out, file)
}

/// The one place of key 1 at `to`, driven to its end.
fn place_one(sim: &mut Sim, store: &mut Store, to: &str) -> Placed {
    let mut effects = Vec::new();
    let tag = store.place(&key(1), name(to), &mut effects).unwrap();
    drive(sim, store, &mut effects);
    let placed = store.placed();
    assert_eq!(placed.len(), 1, "{placed:?}");
    assert_eq!(placed[0].tag, tag);
    placed.into_iter().next().unwrap()
}

#[test]
fn a_placed_file_is_kept_across_an_open_and_cloned_from_again_unwritten() {
    let mut sim = Sim::new(13, quiet());
    let (store, out, file) = placed_once(&mut sim, 130);
    drop(store);
    let temporary = name("store/placed/.cut-short.123-4.tmp");
    sim.change(&temporary, b"half a write");
    let mut store = open(&mut sim, "store");
    let len = out.len() as u64;
    assert_eq!(store.placed_on_open(), PlacedOnOpen { kept: 1, kept_bytes: len, swept: 1 });
    assert!(!sim.exists(&temporary), "a temporary file of an earlier run goes");
    let placed = place_one(&mut sim, &mut store, "two/build/a");
    assert_eq!(placed.result, Ok(()));
    assert_eq!(
        placed.bytes,
        PlacedBytes { cloned: len, written: 0, copied: 0 },
        "checked, not written again"
    );
    assert_eq!(sim.contents(&name("two/build/a")), out);
    assert_eq!(sim.mode(&file), Some(PLACED_MODE));
    assert_eq!(store.problems(), &[] as &[String]);
}

/// Pins C3 and C1 of review 4: the first place of a kept whole file after an open looks at the
/// name without following a link (`Lstat`), hashes it, seals it on the name itself (`Seal`) and
/// then clones it: never a `Stat` or a `Mode` of it, either of which follows a link put there.
#[test]
fn the_first_place_after_an_open_looks_at_and_seals_the_placed_file_without_following_a_link() {
    let mut sim = Sim::new(25, quiet());
    let (store, _, file) = placed_once(&mut sim, 250);
    drop(store);
    let mut store = open(&mut sim, "store");
    let mut effects = Vec::new();
    store.place(&key(1), name("two/build/a"), &mut effects).unwrap();
    // Every effect asked, in order, of the whole file: the kind of each.
    let of_file = |effects: &[Effect], seen: &mut Vec<&'static str>| {
        for effect in effects {
            seen.extend(match effect {
                Effect::Lstat { file: it, .. } if *it == file => Some("Lstat"),
                Effect::Stat { file: it, .. } if *it == file => Some("Stat"),
                Effect::Map { file: it, .. } if *it == file => Some("Map"),
                Effect::Seal { file: it, .. } if *it == file => Some("Seal"),
                Effect::Mode { file: it, .. } if *it == file => Some("Mode"),
                Effect::Write { file: it, .. } if *it == file => Some("Write"),
                Effect::Remove { file: it, .. } if *it == file => Some("Remove"),
                Effect::Clone { from, .. } if *from == file => Some("Clone"),
                _ => None,
            });
        }
    };
    let mut seen = Vec::new();
    of_file(&effects, &mut seen);
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            let before = effects.len();
            _ = store.complete(completion, effects);
            of_file(&effects[before..], &mut seen);
        }
    }).expect("the effects end");
    assert_eq!(store.placed()[0].result, Ok(()));
    assert_eq!(seen, ["Lstat", "Map", "Seal", "Clone"], "the kept file's effects, in order");
}

#[test]
fn a_placed_file_with_other_bytes_is_written_again_before_it_is_cloned() {
    for (seed, shorter) in [(14, false), (15, true)] {
        let mut sim = Sim::new(seed, quiet());
        let (store, out, file) = placed_once(&mut sim, seed * 10);
        drop(store);
        let mut other = out.clone();
        match shorter {
            true => other.truncate(out.len() - 1),
            false => other[out.len() / 2] ^= 1,
        }
        sim.change(&file, &other);
        let mut store = open(&mut sim, "store");
        let placed = place_one(&mut sim, &mut store, "two/build/a");
        assert_eq!(placed.result, Ok(()));
        assert_eq!(placed.bytes.written, out.len() as u64, "written again");
        assert_eq!(sim.contents(&file), out);
        assert_eq!(sim.contents(&name("two/build/a")), out);
        let why = if shorter { "bytes, not the" } else { "not of its name" };
        assert!(store.problems().iter().any(|it| it.contains(why)), "{:?}", store.problems());
    }
}

#[test]
fn a_placed_file_removed_outside_the_daemon_is_written_again() {
    let mut sim = Sim::new(16, quiet());
    let (mut store, out, file) = placed_once(&mut sim, 160);
    sim.delete(&file);
    let placed = place_one(&mut sim, &mut store, "two/build/a");
    assert_eq!(placed.result, Ok(()));
    assert_eq!(placed.bytes.written, out.len() as u64);
    assert_eq!(sim.contents(&name("two/build/a")), out);
    assert!(
        store.problems().iter().any(|it| it.contains("could not be cloned from")),
        "{:?}",
        store.problems()
    );
}

#[test]
fn a_placed_file_swapped_for_a_symbolic_link_is_written_again_and_never_cloned_through() {
    let mut sim = Sim::new(18, quiet());
    let (mut store, out, file) = placed_once(&mut sim, 180);
    sim.change(&name("elsewhere/a"), &out);
    sim.symlink(&file, "../../elsewhere/a");
    let placed = place_one(&mut sim, &mut store, "two/build/a");
    assert_eq!(placed.result, Ok(()));
    assert_eq!(
        placed.bytes.written,
        out.len() as u64,
        "written again, not cloned through the link"
    );
    assert_eq!(sim.contents(&name("two/build/a")), out);
    assert_eq!(sim.mode(&file), Some(PLACED_MODE), "a file again, read-only");
    let why = blaze_core::CLONE_THROUGH_A_LINK;
    assert!(store.problems().iter().any(|it| it.contains(why)), "{:?}", store.problems());
}

#[test]
fn a_placed_file_let_go_of_while_a_clone_of_it_is_in_flight_goes_after_it() {
    let mut sim = Sim::new(17, quiet());
    let (mut store, out, file) = placed_once(&mut sim, 170);
    let mut effects = Vec::new();
    let tag = store.place(&key(1), name("two/build/a"), &mut effects).unwrap();
    store.unplace(&key(1), &key(9), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert_eq!((placed[0].tag, &placed[0].result), (tag, &Ok(())));
    assert_eq!(sim.contents(&name("two/build/a")), out);
    assert!(!sim.exists(&file), "it goes once the clone ends");
    // Let go of for a value of the same bytes, it stays.
    store.put(key(2), Kind::Output, &out).unwrap();
    flush(&mut sim, &mut store);
    place_one(&mut sim, &mut store, "three/build/a");
    store.unplace(&key(1), &key(2), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(sim.exists(&file), "the same bytes under another key keep it");
}

#[test]
fn a_place_while_the_open_looks_through_placed_waits_for_the_look_and_is_not_swept() {
    let mut sim = Sim::new(19, quiet());
    let (store, _, kept) = placed_once(&mut sim, 190);
    drop(store);
    let temporary = name("store/placed/.cut-short.123-4.tmp");
    sim.change(&temporary, b"half a write");
    sim.hold_lists(name("store/placed"));
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    assert_eq!(sim.lists_held(), 1, "the open's look through placed/ is still going");
    // A new output, whose whole file is to be written while the look goes on.
    let out = noise(CHUNK_FROM as usize + 20, 191);
    store.put(key(2), Kind::Output, &out).unwrap();
    flush(&mut sim, &mut store);
    let file = name(&format!("store/placed/{:x}", content_of(&out)));
    let tag = store.place(&key(2), name("two/build/b"), &mut effects).unwrap();
    assert!(
        !format!("{effects:?}").contains("store/placed"),
        "nothing in placed/ is touched yet: {effects:?}"
    );
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.placed(), Vec::new(), "the place waits for the look");
    assert!(!sim.exists(&file), "no write into placed/ began");
    sim.release_lists();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert_eq!((placed.len(), placed[0].tag, &placed[0].result), (1, tag, &Ok(())));
    assert_eq!(placed[0].bytes.written, out.len() as u64);
    assert_eq!(sim.contents(&file), out, "the place's own file stays");
    assert_eq!(sim.contents(&name("two/build/b")), out);
    assert!(
        sim.exists(&kept) && !sim.exists(&temporary),
        "the look kept the earlier file and swept the leftover"
    );
    assert_eq!(store.placed_on_open().swept, 1);
}

#[test]
fn a_placed_directory_that_cannot_be_looked_through_breaks_the_store() {
    let mut sim = Sim::new(20, quiet());
    let (store, _, file) = placed_once(&mut sim, 200);
    drop(store);
    sim.delete(&file);
    sim.change(&name("store/placed"), b"not a directory");
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("broken").to_string();
    assert!(broken.contains("store/placed") && broken.contains("not a directory"), "{broken}");
    // A place is refused, naming the directory (or, where the broken store no longer reads its
    // index, not taken at all).
    if store.place(&key(1), name("two/build/a"), &mut effects).is_some() {
        drive(&mut sim, &mut store, &mut effects);
        let placed = store.placed();
        assert!(
            matches!(&placed[0].result, Err(why) if why.contains("store/placed")),
            "{placed:?}"
        );
    }
    assert!(!sim.exists(&name("two/build/a")));
}

#[test]
fn a_look_through_placed_that_fails_is_asked_again_and_its_places_go_on() {
    let mut sim = Sim::new(21, quiet());
    let (store, out, _) = placed_once(&mut sim, 210);
    drop(store);
    let temporary = name("store/placed/.cut-short.123-4.tmp");
    sim.change(&temporary, b"half a write");
    // One short of the bound: the look is asked again each time, and the last one sweeps.
    sim.fail_lists(name("store/placed"), FAILURES_MOST - 1);
    let mut store = open(&mut sim, "store");
    assert!(store.broken().is_none(), "{:?}", store.broken());
    let len = out.len() as u64;
    assert_eq!(store.placed_on_open(), PlacedOnOpen { kept: 1, kept_bytes: len, swept: 1 });
    assert!(!sim.exists(&temporary));
    let placed = place_one(&mut sim, &mut store, "two/build/a");
    assert_eq!(placed.result, Ok(()));
    assert_eq!(placed.bytes, PlacedBytes { cloned: len, written: 0, copied: 0 });
}

#[test]
fn a_look_through_placed_that_keeps_failing_breaks_the_store_naming_it() {
    let mut sim = Sim::new(22, quiet());
    let (store, _, _) = placed_once(&mut sim, 220);
    drop(store);
    sim.fail_lists(name("store/placed"), FAILURES_MOST);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("broken").to_string();
    let times = format!("failed {FAILURES_MOST} times");
    for part in
        ["store/placed", "looking through it as the store opened", "Permission denied", &times]
    {
        assert!(broken.contains(part), "{part:?} not in {broken}");
    }
    if store.place(&key(1), name("two/build/a"), &mut effects).is_some() {
        drive(&mut sim, &mut store, &mut effects);
        let placed = store.placed();
        assert!(
            matches!(&placed[0].result, Err(why) if why.contains("store/placed")),
            "{placed:?}"
        );
    }
    assert!(!sim.exists(&name("two/build/a")));
}

/// Pins L1 of round 4: an unplace asked while the open's look through `placed/` runs is
/// held, so no Remove there races the listing; it goes once the look ends.
#[test]
fn an_unplace_while_the_open_looks_through_placed_removes_nothing_until_the_look_ends() {
    let mut sim = Sim::new(24, quiet());
    let (store, _, kept) = placed_once(&mut sim, 240);
    drop(store);
    sim.hold_lists(name("store/placed"));
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    assert_eq!(sim.lists_held(), 1, "the open's look through placed/ is still going");
    store.unplace(&key(1), &key(9), &mut effects);
    let placed_dir = name("store/placed");
    let removes = effects
        .iter()
        .any(|it| matches!(it, Effect::Remove { file, .. } if file.is_under(&placed_dir)));
    assert!(!removes, "no remove in placed/ while the look goes on: {effects:?}");
    drive(&mut sim, &mut store, &mut effects);
    assert!(sim.exists(&kept), "the whole file stays while the look goes on");
    sim.release_lists();
    drive(&mut sim, &mut store, &mut effects);
    assert!(store.broken().is_none(), "{:?}", store.broken());
    assert!(!sim.exists(&kept), "the unplace held for the look goes once it ends");
}

/// Pins L3 of round 4 (and C2 of review 4): a place held for the open's look is refused, naming
/// why, when the store breaks before the look ends, here by its commits failing until the bound.
#[test]
fn a_place_held_for_the_look_is_refused_when_the_store_breaks_meanwhile() {
    let mut sim = Sim::new(23, quiet());
    let (store, _, _) = placed_once(&mut sim, 230);
    drop(store);
    sim.hold_lists(name("store/placed"));
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    let tag = store.place(&key(1), name("two/build/a"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.placed(), Vec::new(), "the place waits for the look");
    // Every commit fails at its sync, until the store breaks.
    sim.fail_syncs(name("store"));
    store.put(key(2), Kind::Output, &noise(100, 231)).unwrap();
    flush(&mut sim, &mut store);
    let broken = store.broken().expect("broken").to_string();
    assert!(broken.contains("Input/output error"), "{broken}");
    let placed = store.placed();
    assert_eq!(
        (placed.len(), placed.first().map(|it| it.tag)),
        (1, Some(tag)),
        "refused, not left waiting: {placed:?}"
    );
    assert!(
        matches!(&placed[0].result, Err(why) if why.contains("Input/output error")),
        "{placed:?}"
    );
    sim.release_lists();
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.placed(), Vec::new(), "nothing is placed after the refusal");
    assert!(!sim.exists(&name("two/build/a")));
}

#[test]
fn where_an_output_cannot_be_cloned_its_place_fails_and_copies_nothing() {
    let mut sim = Sim::new(12, Faults { no_clone: true, ..Faults::default() });
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Output, &noise(CHUNK_FROM as usize, 120)).unwrap();
    flush(&mut sim, &mut store);
    let mut effects = Vec::new();
    store.place(&key(1), name("out/a"), &mut effects).unwrap();
    drive(&mut sim, &mut store, &mut effects);
    let placed = store.placed();
    assert!(matches!(&placed[0].result, Err(why) if why.contains("does not copy")), "{placed:?}");
    assert_eq!(placed[0].bytes, PlacedBytes::default());
    assert!(!sim.exists(&name("out/a")));
}

#[test]
fn a_layer_is_read_after_the_store_and_never_written() {
    let mut sim = Sim::new(11, quiet());
    let mut base = open(&mut sim, "base");
    base.put(key(1), Kind::Code, b"from the base").unwrap();
    base.put(key(2), Kind::Output, &noise(400 << 10, 2)).unwrap();
    flush(&mut sim, &mut base);
    drop(base);
    let before = sim.contents(&name("base/head"));
    let mut store = open_with(
        &mut sim,
        Config { dir: name("mine"), layers: vec![name("base")], tags: 1 << 40 },
    );
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"from the base");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), noise(400 << 10, 2));
    let ticket = store.put(key(1), Kind::Code, b"from the base").unwrap();
    assert!(store.durable(ticket));
    assert!(store.put(key(1), Kind::Code, b"other").is_err());
    store.put(key(3), Kind::Code, b"mine").unwrap();
    flush(&mut sim, &mut store);
    assert_eq!(sim.contents(&name("base/head")), before);
    assert!(sim.exists(&name("mine/head")));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_missing_layer_stops_the_store() {
    let mut sim = Sim::new(12, quiet());
    let mut effects = Vec::new();
    let mut store = Store::open(
        Config { dir: name("mine"), layers: vec![name("nowhere")], tags: 0 },
        &mut effects,
    );
    drive(&mut sim, &mut store, &mut effects);
    assert_eq!(store.broken().unwrap().file, name("nowhere/head"));
}

fn writer(n: u64) -> ContentId {
    ContentId::of("blaze v2 store test writer 1", &n.to_le_bytes())
}

/// Writer `n` of the shared directory `shared`, with tags of its own.
fn shared(n: u64) -> SharedConfig {
    SharedConfig { root: name("shared"), writer: writer(n), tags: (1 << 40) + (n << 33) }
}

fn writer_file(n: u64, file: &str) -> FileName {
    name(&format!("shared/w-{:x}/{file}", writer(n)))
}

fn open_shared(sim: &mut Sim, n: u64) -> Store {
    let mut effects = Vec::new();
    let mut store = Store::open_shared(shared(n), &mut effects);
    drive(sim, &mut store, &mut effects);
    assert!(store.ready(), "{:?}", store.broken());
    store
}

/// Runs everything asked for, handing each store its completions.
fn drive_all(sim: &mut Sim, stores: &mut [&mut Store], effects: &mut Vec<Effect>) {
    run_until_idle(sim, effects, |completion, effects| {
        if let Some(store) = stores.iter_mut().find(|it| it.owns(completion.tag)) {
            store.complete(completion, effects);
        }
    }).expect("the effects end");
}

/// A get as a daemon makes it of a shared store: a miss looks again, and gets once that look
/// is in.
fn get_after_miss(sim: &mut Sim, store: &mut Store, key: &Key) -> Option<Vec<u8>> {
    if let Some(bytes) = bytes_of(store, key) {
        return Some(bytes);
    }
    let mut effects = Vec::new();
    let look = store.look_again(&mut effects);
    drive(sim, store, &mut effects);
    assert!(store.looked() >= look, "{:?}", store.broken());
    bytes_of(store, key)
}

#[test]
fn two_processes_committing_at_once_lose_neither_commit() {
    let mut sim = Sim::new(13, quiet());
    let mut a = open_shared(&mut sim, 1);
    let mut b = open_shared(&mut sim, 2);
    for n in 0..60 {
        a.put(key(n), Kind::Code, &text(n, 300)).unwrap();
        b.put(key(1000 + n), Kind::Code, &text(1000 + n, 300)).unwrap();
    }
    // One key with its one bytes, put by both before either sees the other's.
    a.put(key(5000), Kind::Output, &text(5000, 300)).unwrap();
    b.put(key(5000), Kind::Output, &text(5000, 300)).unwrap();
    let mut effects = Vec::new();
    a.flush(&mut effects);
    b.flush(&mut effects);
    drive_all(&mut sim, &mut [&mut a, &mut b], &mut effects);
    assert!(a.broken().is_none() && b.broken().is_none(), "{:?} {:?}", a.broken(), b.broken());
    drop((a, b));
    let a = open_shared(&mut sim, 1);
    let c = open_shared(&mut sim, 3);
    for store in [&a, &c] {
        for n in (0..60).chain(1000..1060).chain([5000]) {
            assert_eq!(bytes_of(store, &key(n)), Some(text(n, 300)), "{n}");
        }
        assert_eq!(store.verify(None), Vec::new());
    }
}

#[test]
fn a_reader_sees_the_others_commit_after_a_miss() {
    let mut sim = Sim::new(14, quiet());
    let mut a = open_shared(&mut sim, 1);
    let mut b = open_shared(&mut sim, 2);
    b.put(key(1), Kind::Code, b"from b").unwrap();
    flush(&mut sim, &mut b);
    assert_eq!(bytes_of(&a, &key(1)), None, "not before it looks");
    assert_eq!(get_after_miss(&mut sim, &mut a, &key(1)).unwrap(), b"from b");
    // A writer that came after it opened, and one that moved on past a new index table.
    let mut d = open_shared(&mut sim, 4);
    d.put(key(2), Kind::Code, b"from d").unwrap();
    flush(&mut sim, &mut d);
    for n in 10..70 {
        b.put(key(n), Kind::Query, &text(n, 200)).unwrap();
    }
    flush(&mut sim, &mut b);
    assert_eq!(get_after_miss(&mut sim, &mut a, &key(2)).unwrap(), b"from d");
    for n in 10..70 {
        assert_eq!(get_after_miss(&mut sim, &mut a, &key(n)), Some(text(n, 200)), "{n}");
    }
    assert_eq!(bytes_of(&a, &key(1)).unwrap(), b"from b");
    assert_eq!(get_after_miss(&mut sim, &mut a, &key(9999)), None);
    assert_eq!(a.verify(None), Vec::new());
}

#[test]
fn a_value_another_writer_committed_is_one_entry() {
    let mut sim = Sim::new(15, quiet());
    let mut a = open_shared(&mut sim, 1);
    a.put(key(1), Kind::Code, b"made once").unwrap();
    flush(&mut sim, &mut a);
    let mut b = open_shared(&mut sim, 2);
    let ticket = b.put(key(1), Kind::Code, b"made once").unwrap();
    assert!(b.durable(ticket));
    assert_ne!(b.found_of(&key(1)).tier, 0);
    assert!(b.put(key(1), Kind::Code, b"other").is_err());
    flush(&mut sim, &mut b);
    assert!(!sim.exists(&writer_file(2, "pack-000000")));
}

#[test]
fn a_damaged_value_in_another_writers_store_is_never_served() {
    let mut sim = Sim::new(16, quiet());
    let mut a = open_shared(&mut sim, 1);
    a.put(key(1), Kind::Query, &[b'a'; 1000]).unwrap();
    a.put(key(2), Kind::Query, &[b'b'; 1000]).unwrap();
    flush(&mut sim, &mut a);
    let loc = a.loc_of(&key(2));
    drop(a);
    let pack = writer_file(1, "pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[(loc.offset + RECORD_HEADER) as usize + 500] ^= 1;
    sim.change(&pack, &bytes);
    let b = open_shared(&mut sim, 2);
    assert_eq!(bytes_of(&b, &key(1)).unwrap(), vec![b'a'; 1000]);
    let corrupt = b.get(&key(2)).unwrap_err();
    assert_eq!(corrupt.pack, pack);
    assert!(corrupt.what.contains("hash"), "{corrupt}");
}

#[test]
fn another_writers_head_that_cannot_be_read_stops_the_store_naming_it() {
    let mut sim = Sim::new(17, quiet());
    let mut a = open_shared(&mut sim, 1);
    a.put(key(1), Kind::Code, b"from a").unwrap();
    flush(&mut sim, &mut a);
    drop(a);
    let head = writer_file(1, HEAD_FILE);
    sim.change(&head, b"not a head");
    let mut effects = Vec::new();
    let mut b = Store::open_shared(shared(2), &mut effects);
    drive(&mut sim, &mut b, &mut effects);
    assert!(!b.ready());
    assert_eq!(b.broken().unwrap().file, head);
}

#[test]
fn a_shared_directory_others_can_write_to_is_refused() {
    let mut sim = Sim::new(18, quiet());
    sim.set_access(&name("shared"), blaze_core::Access::OthersWrite);
    let mut effects = Vec::new();
    let mut store = Store::open_shared(shared(1), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    assert!(!store.ready());
    assert_eq!(store.broken().unwrap().file, name("shared"));
}

/// Opened, and refused with what of it is not the user's alone and why; nothing in it is read.
fn refused(sim: &mut Sim) -> Broken {
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let mut asked = Vec::new();
    run_until_idle(sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        asked.extend(effects.iter().map(|it| format!("{it:?}")));
    }).expect("the effects end");
    assert!(!store.ready());
    assert_eq!(asked, Vec::<String>::new(), "nothing is read from a store that is refused");
    let Broken { file, what } = store.broken().expect("refused");
    Broken { file: file.clone(), what: what.clone() }
}

#[test]
fn a_store_of_another_user_is_refused() {
    let mut sim = Sim::new(40, quiet());
    sim.set_access(&name("store"), blaze_core::Access::OtherUser);
    let broken = refused(&mut sim);
    assert_eq!(broken.file, name("store"));
    assert!(broken.what.contains("belongs to another user"), "{broken}");
}

#[test]
fn a_store_others_can_write_to_is_refused() {
    let mut sim = Sim::new(41, quiet());
    sim.set_access(&name("store"), blaze_core::Access::OthersWrite);
    let broken = refused(&mut sim);
    assert_eq!(broken.file, name("store"));
    assert!(broken.what.contains("can be written to by other users"), "{broken}");
}

/// A value is checked against its own hash and never against what its key says made it, so a
/// pack another user rewrote is refused before any of it is read.
#[test]
fn a_pack_others_can_write_to_is_not_read() {
    let mut sim = Sim::new(42, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"code").unwrap();
    store.put(key(2), Kind::Query, b"a result").unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    assert!(sim.exists(&pack));
    sim.chmod(&pack, 0o646);
    let broken = refused(&mut sim);
    assert_eq!(broken.file, pack);
    assert!(broken.what.contains("can be written to by other users"), "{broken}");
    sim.chmod(&pack, 0o644);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), b"a result");
}

/// On this machine's disk: the store's directory is made the user's alone, closed to others if
/// it was open to their reading, and refused with a file in it others may write to.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn the_store_and_its_files_are_the_users_alone() {
    use std::os::unix::fs::PermissionsExt as _;
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let chmod = |path: &std::path::Path, mode| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let root = std::env::temp_dir().join(format!("blaze-cas-private-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    let dir = root.join("store");
    {
        let mut driven = Driven::open(&root).unwrap();
        driven.store().put(key(1), Kind::Query, b"kept").unwrap();
        driven.flush().unwrap();
    }
    assert_eq!(mode(&dir), 0o700);
    chmod(&dir, 0o705);
    drop(Driven::open(&root).unwrap());
    assert_eq!(mode(&dir), 0o700, "closed to others' reading");
    let head = dir.join("head");
    chmod(&head, 0o666);
    let refused = Driven::open(&root).err().unwrap();
    assert!(
        refused.contains("store/head") && refused.contains("can be written to by other users"),
        "{refused}"
    );
    assert_eq!(mode(&head), 0o666, "what is refused is never mended");
    chmod(&dir, 0o777);
    let refused = Driven::open(&root).err().unwrap();
    assert!(refused.contains("store: can be written to by other users"), "{refused}");
    _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_index_is_rewritten_into_the_other_slot_and_the_head_flipped() {
    let mut sim = Sim::new(13, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..INDEX_AFTER as u64 + 5 {
        store.put(key(n), Kind::Query, &n.to_le_bytes()).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(store.tiers[0].head.index.unwrap().slot, 0);
    assert_eq!(store.delta_len(), 0, "the new table holds every record");
    for n in 1000..1000 + INDEX_AFTER as u64 {
        store.put(key(n), Kind::Code, &n.to_le_bytes()).unwrap();
    }
    flush(&mut sim, &mut store);
    let index = store.tiers[0].head.index.unwrap();
    assert_eq!(index.slot, 1);
    assert_eq!(index.count, 2 * INDEX_AFTER as u64 + 5);
    store.put(key(5000), Kind::Code, b"after").unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(store.delta_len(), 1, "only what came after the table is read at open");
    for n in (0..INDEX_AFTER as u64 + 5).chain(1000..1000 + INDEX_AFTER as u64) {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), n.to_le_bytes(), "{n}");
    }
    assert_eq!(bytes_of(&store, &key(5000)).unwrap(), b"after");
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn packs_roll_over_when_full() {
    let mut sim = Sim::new(14, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..80 {
        store.put(key(n), Kind::Code, &noise(10 << 10, n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let packs = store.tiers[0].packs.len();
    assert!(packs >= 3, "{packs} packs");
    assert!(store.tiers[0].packs.iter().all(|it| it.end <= PACK_MOST));
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..80 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), noise(10 << 10, n));
    }
}

#[test]
fn a_head_round_trips_and_refuses_any_change() {
    let head = Head {
        generation: 7,
        index: Some(IndexRef { slot: 1, len: 64 + 1024 * 64, check: 0xdead_beef, count: 3 }),
        packs: vec![
            PackRef { unit: 0, committed: 4096, indexed: 16, dead: 0, removed: true },
            PackRef { unit: 2, committed: 3 * 4096, indexed: 4096, dead: 112, removed: false },
        ],
        dictionaries: vec![DictionaryRef { kind: Kind::Run, pack: 1, offset: 16 }],
        externals: vec![ExternalRef { content: content_of(b"y"), len: 9 }],
        units: vec![content_of(b"crate a 1.0.0"), content_of(b"crate b 2.1.0")],
    };
    let bytes = head.encode();
    assert_eq!(Head::decode(&bytes).unwrap(), head);
    for at in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[at] ^= 1;
        assert!(Head::decode(&changed).is_err(), "byte {at}");
    }
    assert!(Head::decode(&bytes[..bytes.len() - 1]).is_err());
    let mut torn = head.clone();
    torn.packs[0].committed = 4096 + 16;
    assert!(Head::decode(&torn.encode()).is_err(), "a commit ends on a block");
    let mut torn = head.clone();
    torn.packs[1].indexed = 4096 - 1;
    assert!(Head::decode(&torn.encode()).is_err(), "the index ends on a record");
    let mut torn = head.clone();
    torn.packs[1].dead = 100;
    assert!(Head::decode(&torn.encode()).is_err(), "dead bytes are whole records");
}

/// Lengths in a record header are not trusted before they are checked: one whose record would
/// be longer than any number is refused, not wrapped into a record of no bytes.
#[test]
fn a_record_header_of_a_length_no_record_has_is_refused() {
    let header = |stored| RecordHeader {
        kind: Kind::Run,
        form: Form::Raw,
        dictionary: NO_DICTIONARY,
        stored,
        raw: 0,
        key: key(1),
        content: content_of(b""),
    };
    for stored in [0, 1 << 40, u64::MAX - RECORD_HEADER - 15] {
        let decoded = RecordHeader::decode(&header(stored).encode()).unwrap();
        assert_eq!(decoded.span(), (RECORD_HEADER + stored).next_multiple_of(disk::ALIGN));
    }
    for stored in [u64::MAX - RECORD_HEADER - 14, u64::MAX - RECORD_HEADER + 1, u64::MAX] {
        assert!(RecordHeader::decode(&header(stored).encode()).is_err(), "{stored}");
        assert_eq!(disk::span_of(stored), None);
    }
}

#[test]
fn keys_hash_the_schema_the_producer_and_every_input() {
    let schema = SchemaHash(content_of(b"schema"));
    let code = content_of(b"code");
    let (a, b) = (content_of(b"a"), content_of(b"b"));
    let key = Key::of(schema, code, &[a, b]);
    assert_eq!(key, Key::of(schema, code, &[a, b]));
    assert_ne!(key, Key::of(schema, code, &[b, a]));
    assert_ne!(key, Key::of(schema, code, &[a]));
    assert_ne!(key, Key::of(schema, content_of(b"other code"), &[a, b]));
    assert_ne!(key, Key::of(SchemaHash(content_of(b"schema 2")), code, &[a, b]));
}

/// A remote that records what it is asked, and knows without asking it has none of `absent`.
struct Recorded(Vec<(Key, Tag)>, Vec<Key>);

impl Remote for Recorded {
    fn fetch(&mut self, key: &Key, tag: Tag, _: &mut Vec<Effect>) -> Asked {
        if self.1.contains(key) {
            return Asked::Missing;
        }
        self.0.push((*key, tag));
        Asked::Sent
    }
}

#[test]
fn a_fetch_the_remote_knows_is_missing_is_answered_at_once() {
    let mut sim = Sim::new(16, quiet());
    let mut store = open(&mut sim, "store");
    let mut remote = Recorded(Vec::new(), vec![key(5)]);
    let mut effects = Vec::new();
    assert_eq!(store.fetch(&key(5), &mut remote, any, &mut effects), None);
    assert!(remote.0.is_empty() && effects.is_empty());
    assert_eq!(store.fetched(), [(key(5), Fetched::Missing)]);
    assert!(
        store.fetch(&key(5), &mut Recorded(Vec::new(), Vec::new()), any, &mut effects).is_some(),
        "not left fetching"
    );
}

#[test]
fn keys_lists_every_key_and_new_keys_what_each_batch_put() {
    let mut sim = Sim::new(17, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"one").unwrap();
    assert_eq!(store.new_keys(), (0, Vec::new()), "nothing is kept before watching");
    store.watch_keys();
    assert!(store.watching_keys());
    store.put(key(2), Kind::Code, b"two").unwrap();
    let mut listed = store.keys();
    listed.sort();
    assert_eq!(listed, {
        let mut both = vec![key(1), key(2)];
        both.sort();
        both
    });
    flush(&mut sim, &mut store);
    let (epoch, mut new) = store.new_keys();
    assert!(epoch > 0, "a batch was moved into packs");
    new.sort();
    new.dedup();
    assert!(new.contains(&key(2)), "{new:?}");
    assert_eq!(store.new_keys(), (epoch, Vec::new()), "each key is taken once");
    store.put(key(3), Kind::Code, b"three").unwrap();
    flush(&mut sim, &mut store);
    let (later, new) = store.new_keys();
    assert!(later > epoch);
    assert!(new.contains(&key(3)) && !new.contains(&key(2)), "{new:?}");
    assert_eq!(store.keys().len(), 3);
}

#[test]
fn a_fetched_value_is_verified_before_it_is_kept() {
    let mut sim = Sim::new(15, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(9), Kind::Code, b"here").unwrap();
    let mut remote = Recorded(Vec::new(), Vec::new());
    let mut effects = Vec::new();
    assert_eq!(store.fetch(&key(9), &mut remote, any, &mut effects), None, "it has it");
    let good = store.fetch(&key(1), &mut remote, any, &mut effects).unwrap();
    assert_eq!(store.fetch(&key(1), &mut remote, any, &mut effects), None, "in flight");
    let bad = store.fetch(&key(2), &mut remote, any, &mut effects).unwrap();
    let missing = store.fetch(&key(3), &mut remote, any, &mut effects).unwrap();
    let failed = store.fetch(&key(4), &mut remote, any, &mut effects).unwrap();
    assert_eq!(remote.0.len(), 4);
    let answer = |answer: remote::Answer| {
        let body = answer.encode();
        Done::Read([(body.len() as u64).to_le_bytes().as_slice(), &body].concat())
    };
    let value = |key, bytes: &[u8], content| remote::Answer::Value {
        key,
        kind: Kind::Code,
        content,
        bytes: bytes.to_vec(),
    };
    store.complete(
        Completion { tag: good, done: answer(value(key(1), b"one", content_of(b"one"))) },
        &mut effects,
    );
    store.complete(
        Completion { tag: bad, done: answer(value(key(2), b"evil", content_of(b"two"))) },
        &mut effects,
    );
    store.complete(
        Completion { tag: missing, done: answer(remote::Answer::Missing { key: key(3) }) },
        &mut effects,
    );
    store.complete(
        Completion { tag: failed, done: Done::Failed(blaze_core::IoError("no route".into())) },
        &mut effects,
    );
    let fetched: BTreeMap<Key, Fetched> = store.fetched().into_iter().collect();
    assert!(matches!(fetched[&key(1)], Fetched::Kept(_)));
    assert!(matches!(&fetched[&key(2)], Fetched::Refused(why) if why.contains("hash")));
    assert_eq!(fetched[&key(3)], Fetched::Missing);
    assert!(matches!(&fetched[&key(4)], Fetched::Refused(why) if why.contains("no route")));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"one");
    assert!(!store.has(&key(2)));
    let durable = flush(&mut sim, &mut store);
    assert_eq!(durable.len(), 2);
}

/// What lets a reader of machine code prove it is its own: a fetch never keeps a
/// `Kind::Machine`, and `get_from` says which directory a value is in.
#[test]
fn machine_code_is_never_kept_from_a_fetch_and_a_value_says_its_directory() {
    let mut sim = Sim::new(18, quiet());
    let mut base = open(&mut sim, "base");
    base.put(key(7), Kind::Machine, b"a layer's code").unwrap();
    flush(&mut sim, &mut base);
    drop(base);
    let mut store = open_with(
        &mut sim,
        Config { dir: name("mine"), layers: vec![name("base")], tags: 1 << 40 },
    );
    store.put(key(8), Kind::Machine, b"my code").unwrap();
    let mut remote = Recorded(Vec::new(), Vec::new());
    let mut effects = Vec::new();
    let machine = store.fetch(&key(1), &mut remote, any, &mut effects).unwrap();
    let run = store.fetch(&key(2), &mut remote, any, &mut effects).unwrap();
    let value = |key, kind, bytes: &[u8]| {
        Done::Read(
            remote::Answer::Value { key, kind, content: content_of(bytes), bytes: bytes.to_vec() }
                .encode_framed(),
        )
    };
    store.complete(
        Completion { tag: machine, done: value(key(1), Kind::Machine, b"far code") },
        &mut effects,
    );
    store.complete(
        Completion { tag: run, done: value(key(2), Kind::Run, b"far run") },
        &mut effects,
    );
    let fetched: BTreeMap<Key, Fetched> = store.fetched().into_iter().collect();
    assert!(
        matches!(&fetched[&key(1)], Fetched::Refused(why) if why.contains("machine code")),
        "{fetched:?}"
    );
    assert!(matches!(fetched[&key(2)], Fetched::Kept(_)));
    assert!(!store.has(&key(1)), "refused machine code is not kept");

    let (reader, guard) = (store.reader(), enter());
    let from =
        |key| reader.get_from(&guard, &key).unwrap().map(|(origin, entry)| (origin, entry.kind));
    assert_eq!(from(key(8)), Some((Origin::Own, Kind::Machine)));
    assert_eq!(
        from(key(2)),
        Some((Origin::Own, Kind::Run)),
        "a fetched value is the store's own, of the kind it was sent as"
    );
    assert_eq!(from(key(7)), Some((Origin::Behind(name("base")), Kind::Machine)));
    assert_eq!(from(key(1)), None);
}

#[test]
fn a_store_that_cannot_write_stops_and_says_where() {
    let mut sim = Sim::new(16, Faults { fail_one_in: 1, ..Faults::default() });
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("every effect fails");
    assert_eq!(broken.file, name("store"), "the first thing asked is the check of the directory");
    assert!(broken.what.contains("injected"), "{broken}");
}

#[test]
fn a_driver_may_hand_over_mappings_by_itself() {
    let mut sim = Sim::new(17, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, b"mapped").unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    let mut maps = Vec::new();
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        maps.extend(effects.extract_if(.., |it| matches!(it, Effect::Map { .. })));
    }).expect("the effects end");
    assert!(!store.ready());
    for effect in maps {
        let Effect::Map { file, .. } = effect else { unreachable!() };
        let bytes = sim.contents(&file);
        store.mapped(&file, Mapping::copy(&bytes));
    }
    assert!(store.ready(), "{:?}", store.broken());
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), b"mapped");
}

/// Every commit leaves each pack a whole number of blocks long, so every append can be written
/// with `O_DIRECT`; the zeros that end a commit are read past at open and never anything else.
#[test]
fn every_commit_ends_each_pack_on_a_block() {
    let mut sim = Sim::new(18, quiet());
    let mut store = open(&mut sim, "store");
    for round in 0..3u64 {
        for n in 0..5 {
            store.put(key(round * 10 + n), Kind::Code, &noise(700 + n as usize * 300, n)).unwrap();
        }
        flush(&mut sim, &mut store);
        for pack in &store.tiers[0].packs {
            assert_eq!(pack.committed % disk::BLOCK, 0, "round {round}");
            assert_eq!(sim.contents(&pack.file).len() as u64, pack.committed, "round {round}");
        }
    }
    drop(store);
    let store = open(&mut sim, "store");
    for round in 0..3u64 {
        for n in 0..5 {
            let found = bytes_of(&store, &key(round * 10 + n)).unwrap();
            assert_eq!(found, noise(700 + n as usize * 300, n));
        }
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_changed_byte_in_the_zeros_ending_a_commit_stops_the_store_naming_its_pack() {
    let mut sim = Sim::new(19, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &[7; 100]).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    let last = bytes.len() - 1;
    bytes[last] = 1;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("the zeros are checked");
    assert_eq!(broken.file, pack);
    assert!(broken.what.contains("zeros"), "{broken}");
}

#[test]
fn an_index_that_does_not_match_its_crc_stops_the_store() {
    let mut sim = Sim::new(20, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..INDEX_AFTER as u64 {
        store.put(key(n), Kind::Query, &n.to_le_bytes()).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(store.tiers[0].head.index.unwrap().slot, 0);
    drop(store);
    let index = name("store/index-0");
    let mut bytes = sim.contents(&index);
    let at = bytes.len() / 2;
    bytes[at] ^= 0x10;
    sim.change(&index, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("the index is checked at open");
    assert_eq!(broken.file, index);
    assert!(broken.what.contains("CRC-32C"), "{broken}");
}

#[test]
fn a_long_cold_value_is_compressed_without_a_dictionary_over_a_long_window() {
    let mut sim = Sim::new(21, quiet());
    let mut store = open(&mut sim, "store");
    let mut long = Vec::new();
    let mut n = 0;
    while long.len() < cold::LONG_FROM + 1000 {
        long.extend(run_output(n));
        n += 1;
    }
    store.put(key(1), Kind::Debug, &long).unwrap();
    let (form, stored, raw) = store.sizes_of(&key(1));
    assert_eq!(form, Form::Zstd);
    assert!(stored * 4 < raw, "{stored} stored of {raw}");
    assert!(store.tiers[0].dictionaries.is_empty(), "a long value is not a sample");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), long);
    flush(&mut sim, &mut store);
    let loc = store.loc_of(&key(1));
    let header = RecordHeader::decode(
        store.tiers[0].packs[loc.pack as usize].bytes(loc.offset, RECORD_HEADER).unwrap(),
    );
    assert_eq!(header.unwrap().dictionary, NO_DICTIONARY);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), long);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn chunks_that_shrink_are_kept_in_lz4_and_the_rest_raw() {
    let mut sim = Sim::new(22, quiet());
    let mut store = open(&mut sim, "store");
    let mut text = Vec::new();
    let mut n = 0;
    while text.len() < 600 << 10 {
        text.extend(run_output(n));
        n += 1;
    }
    let mut both = text.clone();
    both.extend(noise(600 << 10, 220));
    store.put(key(1), Kind::Output, &both).unwrap();
    let forms: Vec<Form> = store.tiers[0]
        .delta
        .iter(Held::writer(&store))
        .filter(|(_, it)| it.key != key(1))
        .map(|(_, it)| it.place.form())
        .collect();
    assert!(forms.contains(&Form::Lz4), "{forms:?}");
    assert!(forms.contains(&Form::Raw), "{forms:?}");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), both);
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), both);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn what_is_unread_for_two_periods_goes_cold_and_what_is_read_stays_hot() {
    let mut sim = Sim::new(30, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..10 {
        store.put(key(n), Kind::Code, &text(n, 8 << 10)).unwrap();
    }
    flush(&mut sim, &mut store);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "one period is not two");
    assert_eq!(
        settle(&mut sim, &mut store).demoted,
        0,
        "a record made in the last two periods stays"
    );
    assert_eq!(bytes_of(&store, &key(0)).unwrap(), text(0, 8 << 10));
    let settled = settle(&mut sim, &mut store);
    assert_eq!(settled.demoted, 9, "{settled:?}");
    assert_eq!(form_of(&store, &key(0)), Form::Raw, "it was read");
    for n in 1..10 {
        assert_eq!(form_of(&store, &key(n)), Form::Zstd, "{n}");
    }
    let durable = store.put(key(3), Kind::Code, &text(3, 8 << 10)).unwrap();
    assert!(store.durable(durable), "a moved value is as durable as it was");
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..10 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), text(n, 8 << 10), "{n}");
    }
    assert_eq!(form_of(&store, &key(4)), Form::Zstd);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_cold_value_read_in_two_periods_running_comes_back_hot() {
    let mut sim = Sim::new(31, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &text(1, 16 << 10)).unwrap();
    store.put(key(2), Kind::Output, &text(2, 16 << 10)).unwrap();
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    assert_eq!(settle(&mut sim, &mut store).demoted, 2);
    for round in 0..2 {
        assert_eq!(bytes_of(&store, &key(1)).unwrap(), text(1, 16 << 10));
        assert_eq!(bytes_of(&store, &key(2)).unwrap(), text(2, 16 << 10));
        let settled = settle(&mut sim, &mut store);
        assert_eq!(settled.promoted, round * 2, "{settled:?}");
    }
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    assert_eq!(form_of(&store, &key(2)), Form::Lz4, "an output comes back as LZ4");
    drop(store);
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), text(2, 16 << 10));
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_value_that_does_not_compress_is_tried_once() {
    let mut sim = Sim::new(32, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Code, &noise(8 << 10, 320)).unwrap();
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    assert!(store.flag_of(&key(1), Flag::Tried));
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    assert_eq!(settle(&mut sim, &mut store), Settled::default());
}

#[test]
fn packs_that_are_mostly_moved_out_are_rewritten_and_removed() {
    let mut sim = Sim::new(33, quiet());
    let mut store = open(&mut sim, "store");
    let value = |n: u64| if n % 2 == 0 { text(n, 12 << 10) } else { noise(4 << 10, n) };
    for n in 0..100 {
        store.put(key(n), Kind::Code, &value(n)).unwrap();
    }
    flush(&mut sim, &mut store);
    let before: Vec<FileName> = store.tiers[0].packs.iter().map(|it| it.file.clone()).collect();
    assert!(before.len() >= 3, "{} packs", before.len());
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let settled = settle(&mut sim, &mut store);
    assert_eq!(settled.demoted, 50, "{settled:?}");
    assert!(settled.retired >= 2, "{settled:?}");
    let last = before.len() - 1;
    for (at, file) in before.iter().enumerate() {
        let removed = store.tiers[0].packs[at].removed;
        assert_eq!(removed, !sim.exists(file), "{file}");
        if at == last {
            assert!(!removed, "the pack its kind appends to stays");
        }
    }
    assert!(store.tiers[0].packs.iter().filter(|it| it.removed).count() as u64 == settled.retired);
    for n in 0..100 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), value(n), "{n}");
    }
    drop(store);
    let store = open(&mut sim, "store");
    for n in 0..100 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), value(n), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_pack_left_behind_by_a_crash_is_removed_at_the_next_open() {
    let mut sim = Sim::new(34, quiet());
    let mut store = open(&mut sim, "store");
    for n in 0..60 {
        store.put(key(n), Kind::Code, &text(n, 12 << 10)).unwrap();
    }
    flush(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    let mut effects = Vec::new();
    assert!(store.settle(&mut effects).retired >= 1);
    run_until_idle(&mut sim, &mut effects, |completion, effects| {
        if store.owns(completion.tag) {
            store.complete(completion, effects);
        }
        effects.retain(|it| !matches!(it, Effect::Remove { .. }));
    }).expect("the effects end");
    let gone: Vec<FileName> =
        store.tiers[0].packs.iter().filter(|it| it.removed).map(|it| it.file.clone()).collect();
    assert!(!gone.is_empty() && gone.iter().all(|it| sim.exists(it)), "the removes were dropped");
    drop(store);
    let store = open(&mut sim, "store");
    assert!(gone.iter().all(|it| !sim.exists(it)));
    for n in 0..60 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), text(n, 12 << 10), "{n}");
    }
}

/// A schema of one `u32` column, encoded by blaze-format's tiers: what a query's result made
/// of columns is to the store. `broken` encodings give back other values from warm.
pub(super) struct OneColumn {
    schema: SchemaHash,
    warm_schema: SchemaHash,
    broken: bool,
}

const ONE_COLUMN_HEAD: usize = 8 + 32 + 8;
const WARM_HEAD: usize = 8 + 32 + 4 + 4 + 8;

fn one_column(broken: bool) -> OneColumn {
    OneColumn {
        schema: SchemaHash(ContentId::of("blaze v2 store test schema", b"one column")),
        warm_schema: SchemaHash(ContentId::of("blaze v2 store test schema", b"one column, warm")),
        broken,
    }
}

fn word<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    bytes[at..at + N].try_into().unwrap()
}

impl OneColumn {
    fn hot(&self, values: &[u32]) -> Vec<u8> {
        let mut out = blaze_format::MAGIC.to_vec();
        out.extend(self.schema.0.0);
        out.extend((values.len() as u64).to_le_bytes());
        out.extend(values.iter().flat_map(|it| it.to_le_bytes()));
        out
    }

    fn values(&self, hot: &[u8]) -> Result<Vec<u32>, String> {
        if hot.len() < ONE_COLUMN_HEAD || hot[8..40] != self.schema.0.0 {
            return Err("not a value of the schema".into());
        }
        let len = u64::from_le_bytes(word(hot, 40)) as usize;
        let rest = &hot[ONE_COLUMN_HEAD..];
        if rest.len() != len * 4 {
            return Err("not as long as it says".into());
        }
        Ok(rest.chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap())).collect())
    }
}

impl OneColumn {
    pub(super) fn schema(&self) -> SchemaHash {
        self.schema
    }

    pub(super) fn warm(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        let packed = blaze_format::tiers::Packed::pack(&self.values(hot)?);
        let mut out = blaze_format::MAGIC.to_vec();
        out.extend(self.warm_schema.0.0);
        out.extend(packed.min.to_le_bytes());
        out.extend(packed.bits.to_le_bytes());
        out.extend((packed.len as u64).to_le_bytes());
        out.extend(packed.words.iter().flat_map(|it| it.to_le_bytes()));
        Ok(out)
    }

    pub(super) fn hot_from_warm(&self, warm: &[u8]) -> Result<Vec<u8>, String> {
        if warm.len() < WARM_HEAD || warm[8..40] != self.warm_schema.0.0 {
            return Err("not warm columns of the schema".into());
        }
        let words =
            warm[WARM_HEAD..].chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap()));
        let packed = blaze_format::tiers::Packed {
            min: u32::from_le_bytes(word(warm, 40)),
            bits: u32::from_le_bytes(word(warm, 44)),
            len: u64::from_le_bytes(word(warm, 48)) as usize,
            words: words.collect(),
        };
        let mut values = packed.unpack();
        if self.broken {
            values[0] += 1;
        }
        Ok(self.hot(&values))
    }

    pub(super) fn cold(&self, hot: &[u8]) -> Result<Vec<u8>, String> {
        Ok(blaze_format::tiers::encode_cold(&self.values(hot)?, None))
    }

    pub(super) fn hot_from_cold(&self, cold: &[u8]) -> Result<Vec<u8>, String> {
        let values = blaze_format::tiers::decode_cold(cold, None).map_err(|it| it.to_string())?;
        Ok(self.hot(&values))
    }
}

/// A column of 4096 values in a range of 50: 16 KiB hot, 6 bits a value warm.
fn column_values(n: u32) -> Vec<u32> {
    (0..4096).map(|at| 1000 + n + at * 7 % 50).collect()
}

fn with_columns(sim: &mut Sim, broken: bool) -> Store {
    let mut store = open(sim, "store");
    store.add_columns(Columns::Test(one_column(broken)));
    store
}

#[test]
fn a_value_of_columns_goes_warm_then_cold_then_comes_back_hot() {
    let mut sim = Sim::new(40, quiet());
    let mut store = with_columns(&mut sim, false);
    let columns = one_column(false);
    let hot = columns.hot(&column_values(0));
    store.put(key(1), Kind::Query, &hot).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..2 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    assert!(store.sizes_of(&key(1)).1 < hot.len() as u64 / 4);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "a warm record made last period stays");
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    for round in 0..2 {
        assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
        let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
        assert_eq!(
            (heat, entry.bytes, entry.content),
            (Heat::Hot, hot.as_slice(), content_of(&hot))
        );
        assert_eq!(settle(&mut sim, &mut store).promoted, round);
    }
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    drop(store);
    let store = with_columns(&mut sim, false);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn warm_columns_are_read_in_place_and_refused_without_their_encoding() {
    let mut sim = Sim::new(41, quiet());
    let mut store = with_columns(&mut sim, false);
    let columns = one_column(false);
    let hot = columns.hot(&column_values(3));
    store.put(key(1), Kind::Query, &hot).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..3 {
        settle(&mut sim, &mut store);
    }
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    let warm = columns.warm(&hot).unwrap();
    let (heat, first) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!((heat, first.bytes, first.content), (Heat::Warm, warm.as_slice(), content_of(&hot)));
    let (_, again) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!(first.bytes.as_ptr(), again.bytes.as_ptr(), "read where it is kept");
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot, "a get gives the value as put");
    drop(store);
    let store = open(&mut sim, "store");
    let refused = store.get(&key(1)).unwrap_err();
    assert!(refused.what.contains("no encoding"), "{refused}");
    assert!(store.get_in_place(&key(1)).is_err());
    assert_eq!(store.verify(None).len(), 1);
    drop(store);
    let store = with_columns(&mut sim, false);
    assert_eq!(store.get_in_place(&key(1)).unwrap().unwrap().1.bytes, warm.as_slice());
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn columns_that_do_not_decode_to_the_value_are_not_kept() {
    let mut sim = Sim::new(42, quiet());
    let mut store = with_columns(&mut sim, true);
    let hot = one_column(true).hot(&column_values(5));
    store.put(key(1), Kind::Query, &hot).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..3 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(form_of(&store, &key(1)), Form::Raw);
    assert!(
        store.problems().iter().any(|it| it.contains("do not decode")),
        "{:?}",
        store.problems()
    );
    assert!(store.flag_of(&key(1), Flag::Tried));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
}

#[test]
fn a_cold_kind_of_columns_is_put_in_cold_columns() {
    let mut sim = Sim::new(43, quiet());
    let mut store = with_columns(&mut sim, false);
    let hot = one_column(false).hot(&column_values(9));
    store.put(key(1), Kind::Run, &hot).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    flush(&mut sim, &mut store);
    drop(store);
    let store = with_columns(&mut sim, false);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

/// A span table whose starts climb by 12 and whose lengths cycle through eight: a `blaze_format`
/// buffer of the kind every schema given to [`Store::encode_columns`] is.
fn spans_buffer() -> Vec<u8> {
    let mut spans = blaze_format::Spans::new(ContentId::of("blaze v2 store test", b"spans"));
    for at in 0..4096 {
        _ = spans.push(at * 12, 4 + at % 8);
    }
    blaze_format::to_bytes(&spans)
}

fn with_spans(sim: &mut Sim) -> Store {
    let mut store = open(sim, "store");
    store.encode_columns(blaze_format::Spans::columns());
    store
}

#[test]
fn a_buffer_given_its_buffer_columns_goes_warm_then_cold_and_reads_back() {
    let mut sim = Sim::new(44, quiet());
    let mut store = with_spans(&mut sim);
    let hot = spans_buffer();
    store.put(key(1), Kind::Query, &hot).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..2 {
        assert_eq!(settle(&mut sim, &mut store).demoted, 0);
    }
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::Warm);
    let warm = blaze_format::Spans::columns().warm(&hot).unwrap();
    let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
    assert_eq!((heat, entry.bytes, entry.content), (Heat::Warm, warm.as_slice(), content_of(&hot)));
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "a warm record made last period stays");
    assert_eq!(settle(&mut sim, &mut store).demoted, 0, "nor one read the period before");
    assert_eq!(settle(&mut sim, &mut store).demoted, 1);
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    flush(&mut sim, &mut store);
    drop(store);
    let store = open(&mut sim, "store");
    assert!(store.get(&key(1)).unwrap_err().what.contains("no encoding"));
    drop(store);
    let store = with_spans(&mut sim);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_cold_kind_of_buffer_is_put_in_its_cold_columns() {
    let mut sim = Sim::new(45, quiet());
    let mut store = with_spans(&mut sim);
    let hot = spans_buffer();
    store.put(key(1), Kind::Run, &hot).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::ColdColumns);
    assert_eq!(bytes_of(&store, &key(1)).unwrap(), hot);
}

#[test]
fn each_crate_version_has_packs_of_its_own_and_keeps_them_across_an_open() {
    let mut sim = Sim::new(50, quiet());
    let mut store = open(&mut sim, "store");
    let (a, b) = (content_of(b"crate a 1.0.0"), content_of(b"crate b 2.1.0"));
    let unit_a = store.unit(a);
    let unit_b = store.unit(b);
    assert_eq!(store.unit(a), unit_a, "a crate version is numbered once");
    assert_ne!(unit_a, unit_b);
    for n in 0..6 {
        let unit = if n % 2 == 0 { unit_a } else { unit_b };
        store
            .put_in(
                unit,
                key(n),
                [Kind::Query, Kind::Code, Kind::Debug][n as usize % 3],
                &noise(5000, n),
            )
            .unwrap();
    }
    store.put(key(10), Kind::Image, &noise(3000, 10)).unwrap();
    flush(&mut sim, &mut store);
    let pack_of = |store: &Store, n: u64| store.loc_of(&key(n)).pack as usize;
    let units: Vec<u32> = (0..6).map(|n| store.tiers[0].packs[pack_of(&store, n)].unit).collect();
    assert_eq!(units, [unit_a.0, unit_b.0, unit_a.0, unit_b.0, unit_a.0, unit_b.0]);
    assert_eq!(pack_of(&store, 0), pack_of(&store, 2), "kinds of one crate version share its file");
    assert_eq!(store.tiers[0].packs[pack_of(&store, 10)].unit, Unit::SHARED.0);
    assert_eq!(store.tiers[0].packs.len(), 3);
    drop(store);
    let mut store = open(&mut sim, "store");
    assert_eq!(store.unit(b), unit_b, "the head keeps the numbering");
    assert_eq!(store.tiers[0].units, [a, b]);
    for n in 0..6 {
        assert_eq!(bytes_of(&store, &key(n)).unwrap(), noise(5000, n), "{n}");
    }
    store.put_in(unit_a, key(20), Kind::Query, &noise(100, 20)).unwrap();
    flush(&mut sim, &mut store);
    assert_eq!(pack_of(&store, 20), pack_of(&store, 0), "a unit appends to its own pack");
    assert_eq!(store.verify(None), Vec::new());
}

#[test]
fn a_pack_whose_header_names_another_unit_stops_the_store() {
    let mut sim = Sim::new(51, quiet());
    let mut store = open(&mut sim, "store");
    let unit = store.unit(content_of(b"crate a 1.0.0"));
    store.put_in(unit, key(1), Kind::Query, &noise(100, 1)).unwrap();
    flush(&mut sim, &mut store);
    drop(store);
    let pack = name("store/pack-000000");
    let mut bytes = sim.contents(&pack);
    bytes[8] = 0;
    sim.change(&pack, &bytes);
    let mut effects = Vec::new();
    let mut store = Store::open(config("store"), &mut effects);
    drive(&mut sim, &mut store, &mut effects);
    let broken = store.broken().expect("the unit is checked");
    assert_eq!(broken.file, pack);
    assert!(broken.what.contains("unit"), "{broken}");
}

/// `column` repeated to at least 8 KiB, so the tier mover (which leaves records under
/// `DEMOTE_FROM` where they are) moves every shape but the empty one.
fn movable_column(column: &[u32]) -> Vec<u32> {
    let times = match column.len() {
        0 => 1,
        len => (8usize << 10).div_ceil(4 * len),
    };
    column.repeat(times)
}

/// The record's form and values, read without counting as a read, so looking does not change
/// where the mover sends it.
fn peek(store: &Store, key: &Key) -> (Form, Vec<u32>) {
    let found = store.found_of(key);
    let (_, bytes) = store.look().value(found, key, Use::Check).unwrap();
    (found.place.form(), one_column(false).values(&bytes).unwrap())
}

/// What a column read as at each step of its record's life in the store.
struct Trip {
    len: usize,
    stages: Vec<(&'static str, Form, Vec<u32>)>,
}

/// Puts `column` as a value of columns, lets the mover take it as cold as it goes (two
/// unread periods a tier), reads it in two periods running so it comes back hot, and reopens.
fn through_the_tiers(column: &[u32], forms: &std::cell::RefCell<Vec<Form>>) -> Trip {
    let mut sim = Sim::new(60, quiet());
    let mut store = with_columns(&mut sim, false);
    let hot = one_column(false).hot(&movable_column(column));
    store.put(key(1), Kind::Query, &hot).unwrap();
    flush(&mut sim, &mut store);
    let mut trip = Trip { len: column.len(), stages: Vec::new() };
    let (form, values) = peek(&store, &key(1));
    trip.stages.push(("put", form, values));
    for (stage, settles) in [("first move", 3), ("second move", 2)] {
        for _ in 0..settles {
            settle(&mut sim, &mut store);
        }
        let (form, values) = peek(&store, &key(1));
        forms.borrow_mut().push(form);
        trip.stages.push((stage, form, values));
    }
    for _ in 0..2 {
        store.get(&key(1)).unwrap().unwrap();
        settle(&mut sim, &mut store);
    }
    let (form, values) = peek(&store, &key(1));
    trip.stages.push(("read back hot", form, values));
    drop(store);
    let store = with_columns(&mut sim, false);
    let (form, values) = peek(&store, &key(1));
    trip.stages.push(("reopened", form, values));
    assert_eq!(store.verify(None), Vec::new());
    trip
}

/// The column every stage of `trip` read, when they all read the same and it came back raw.
fn column_of(trip: &Trip) -> Result<Vec<u32>, String> {
    let (_, _, first) = &trip.stages[0];
    for (stage, form, values) in &trip.stages {
        if values != first {
            return Err(format!("{stage} ({form:?}) read other values than were put"));
        }
    }
    let (_, last, _) = trip.stages.last().unwrap();
    if trip.len > 0 && *last != Form::Raw {
        return Err(format!("read in two periods running, it stayed {last:?}"));
    }
    Ok(first[..trip.len].to_vec())
}

#[test]
fn a_record_moved_hot_to_warm_to_cold_and_back_round_trips_every_column_shape() {
    for seed in [1, 2, 3] {
        let forms = std::cell::RefCell::new(Vec::new());
        let checked = blaze_v2_proof::columns::round_trips(
            |column| through_the_tiers(column, &forms),
            column_of,
            seed,
        )
        .unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(checked > 100, "{checked} shapes");
        let forms = forms.into_inner();
        assert!(forms.contains(&Form::Warm), "no shape went warm: {forms:?}");
        assert!(forms.contains(&Form::ColdColumns), "no shape went cold: {forms:?}");
    }
}

/// A column as the store leaves it after the mover's first move: read in place where it is.
enum InPlace {
    Hot(Vec<u8>),
    Warm(blaze_format::tiers::Packed),
}

fn moved_once(column: &[u32]) -> InPlace {
    let mut sim = Sim::new(61, quiet());
    let mut store = with_columns(&mut sim, false);
    store.put(key(1), Kind::Query, &one_column(false).hot(&movable_column(column))).unwrap();
    flush(&mut sim, &mut store);
    for _ in 0..3 {
        settle(&mut sim, &mut store);
    }
    let (heat, entry) = store.get_in_place(&key(1)).unwrap().unwrap();
    match heat {
        Heat::Hot => InPlace::Hot(entry.bytes.to_vec()),
        Heat::Warm => {
            let warm = entry.bytes;
            let words = warm[WARM_HEAD..]
                .chunks_exact(4)
                .map(|it| u32::from_le_bytes(it.try_into().unwrap()));
            InPlace::Warm(blaze_format::tiers::Packed {
                min: u32::from_le_bytes(word(warm, 40)),
                bits: u32::from_le_bytes(word(warm, 44)),
                len: u64::from_le_bytes(word(warm, 48)) as usize,
                words: words.collect(),
            })
        }
    }
}

fn element(column: &InPlace, at: usize) -> u32 {
    match column {
        InPlace::Hot(bytes) => u32::from_le_bytes(word(bytes, ONE_COLUMN_HEAD + 4 * at)),
        InPlace::Warm(packed) => packed.as_ref().get(at),
    }
}

#[test]
fn what_the_mover_leaves_warm_or_hot_reads_in_place_for_every_column_shape() {
    for seed in [1, 2, 3] {
        let checked = blaze_v2_proof::columns::reads_in_place(moved_once, element, seed)
            .unwrap_or_else(|why| panic!("seed {seed}: {why}"));
        assert!(checked > 100, "{checked} shapes");
    }
}

/// Calls `tidy` until the store's limbo and its delta tables hold nothing swapped out and no
/// index swap is waiting on writers. Other tests of this process hold guards of their own, so a
/// grace can take a moment to pass.
fn reclaim_all(store: &mut Store) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let retired = |store: &Store| {
        let own = &store.tiers[0];
        std::iter::once(&own.delta).chain(&own.prev).map(|it| it.retired()).sum::<usize>()
    };
    while !store.limbo.is_empty() || store.migrating.is_some() || retired(store) > 0 {
        store.tidy();
        assert!(
            std::time::Instant::now() < deadline,
            "a grace never passed with no task of this test open"
        );
        std::thread::yield_now();
    }
}

/// A worker's task reads each value it puts and holds what it read, while the store takes one
/// batch of completions at a time through commits that move those values out of their
/// segments, swap the index and retire views, spots and chunks. What the task read stays whole
/// until it ends; then all of it is freed, every put is durable, and the store reopens with
/// every value.
#[test]
fn what_a_task_read_stays_whole_through_every_step_of_commits_and_is_freed_after() {
    for seed in 0..16 {
        let mut sim = Sim::new(200 + seed, quiet());
        let mut store = open(&mut sim, "store");
        let mut writer = store.writer();
        let reader = store.reader();
        let guard = enter();
        let (mut values, mut read, mut tickets) = (Vec::new(), Vec::new(), Vec::new());
        let (mut effects, mut out) = (Vec::new(), Vec::new());
        for n in 0..INDEX_AFTER as u64 * 3 {
            let (kind, bytes) = match n % 9 {
                // Outputs alike but for their ends: their chunks are put once.
                0 => (Kind::Output, noise(CHUNK_FROM + 1000 + n as usize, seed)),
                1 => (Kind::Query, noise(EXTERNAL_FROM + n as usize, n)),
                _ => (Kind::Query, noise(40 + (n as usize * 53) % 3000, n)),
            };
            tickets.push(writer.put(&guard, key(n), kind, &bytes).unwrap());
            let entry = reader.get(&guard, &key(n)).unwrap().expect("a put is read at once");
            assert_eq!(entry.bytes, bytes.as_slice(), "seed {seed}");
            read.push(entry.bytes);
            values.push(bytes);
            match sim.rng().below(3) {
                0 => store.flush(&mut effects),
                _ => {
                    sim.submit(&mut effects);
                    sim.poll(&mut out, blaze_core::wait::Deadline::NEVER);
                    for completion in out.drain(..) {
                        if store.owns(completion.tag) {
                            store.complete(completion, &mut effects);
                        }
                    }
                }
            }
            if n % 16 == 15 {
                for (at, bytes) in read.iter().enumerate() {
                    assert_eq!(
                        *bytes,
                        values[at].as_slice(),
                        "seed {seed}: value {at} changed under its task"
                    );
                }
            }
        }
        drive(&mut sim, &mut store, &mut effects);
        flush(&mut sim, &mut store);
        for (at, bytes) in read.iter().enumerate() {
            assert_eq!(
                *bytes,
                values[at].as_slice(),
                "seed {seed}: value {at} changed under its task"
            );
        }
        assert!(store.tiers[0].head.index.is_some(), "seed {seed}: an index was written");
        assert!(!store.limbo.is_empty(), "seed {seed}: what the task can still read is held");
        drop(read);
        drop(guard);
        reclaim_all(&mut store);
        assert!(store.tiers[0].prev.is_none(), "seed {seed}: the swap ended");
        flush(&mut sim, &mut store);
        assert!(tickets.iter().all(|it| store.durable(*it)), "seed {seed}: a put is not durable");
        drop(writer);
        drop(store);
        let store = open(&mut sim, "store");
        for (n, bytes) in values.iter().enumerate() {
            assert_eq!(bytes_of(&store, &key(n as u64)).as_ref(), Some(bytes), "seed {seed}: {n}");
        }
        assert_eq!(store.verify(None), Vec::new(), "seed {seed}");
    }
}

/// Four writers on threads of their own put every key at once, each from another place in
/// the list, so they race to claim keys and the chunks their outputs share; two readers read
/// every key the while; and the store's thread commits, settles and swaps its index under
/// them. Every value read is whole and what was put, every put is durable after the last
/// flush, and the store reopens with every value.
#[test]
fn writers_and_readers_on_other_threads_go_on_through_commits_settles_and_index_swaps() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const WRITERS: u64 = 4;
    const KEYS: u64 = 240;
    let values: Arc<Vec<(Kind, Vec<u8>)>> = Arc::new(
        (0..KEYS)
            .map(|n| match n % 20 {
                0 => {
                    let mut bytes = noise(CHUNK_FROM + (200 << 10), 7);
                    let at = (n as usize * 4099) % bytes.len();
                    bytes[at] ^= 0x5a;
                    (Kind::Output, bytes)
                }
                1 => (Kind::Query, noise(EXTERNAL_FROM + n as usize, n)),
                2 | 3 => (Kind::Run, run_output(n)),
                _ => (Kind::Query, noise(32 + (n as usize * 37) % 2000, n)),
            })
            .collect(),
    );
    let mut sim = Sim::new(300, quiet());
    let mut store = open(&mut sim, "store");
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..2u64)
        .map(|first| {
            let (reader, values, stop) = (store.reader(), values.clone(), stop.clone());
            std::thread::spawn(move || {
                let (mut found, mut at) = (0u64, first);
                while !stop.load(Ordering::Relaxed) {
                    let guard = enter();
                    for _ in 0..16 {
                        at = (at + 31) % KEYS;
                        let (kind, bytes) = &values[at as usize];
                        let entry = reader
                            .get(&guard, &key(at))
                            .unwrap_or_else(|corrupt| panic!("{corrupt}"));
                        if let Some(entry) = entry {
                            assert_eq!(
                                (entry.kind, entry.bytes),
                                (*kind, bytes.as_slice()),
                                "key {at}"
                            );
                            found += 1;
                        }
                    }
                    drop(guard);
                }
                found
            })
        })
        .collect();
    let writers: Vec<_> = (0..WRITERS)
        .map(|number| {
            let (mut writer, values) = (store.writer(), values.clone());
            std::thread::spawn(move || {
                let mut tickets = Vec::new();
                for i in 0..KEYS {
                    let n = (i + number * KEYS / WRITERS) % KEYS;
                    let (kind, bytes) = &values[n as usize];
                    let guard = enter();
                    tickets.push(writer.put(&guard, key(n), *kind, bytes).unwrap());
                }
                tickets
            })
        })
        .collect();
    let mut rounds = 0u32;
    while !writers.iter().all(|it| it.is_finished()) {
        flush(&mut sim, &mut store);
        if rounds % 4 == 3 {
            settle(&mut sim, &mut store);
        }
        rounds += 1;
        std::thread::yield_now();
    }
    let tickets: Vec<Ticket> = writers.into_iter().flat_map(|it| it.join().unwrap()).collect();
    flush(&mut sim, &mut store);
    assert!(store.broken().is_none(), "{:?}", store.broken());
    assert_eq!(tickets.len() as u64, WRITERS * KEYS);
    assert!(
        tickets.iter().all(|it| store.durable(*it)),
        "every put is durable after the last flush"
    );
    stop.store(true, Ordering::Relaxed);
    let found: u64 = readers.into_iter().map(|it| it.join().unwrap()).sum();
    assert!(found > 0, "the readers read");
    for (n, (kind, bytes)) in values.iter().enumerate() {
        let entry = store.get(&key(n as u64)).unwrap().unwrap();
        assert_eq!((entry.kind, entry.bytes), (*kind, bytes.as_slice()), "{n}");
    }
    assert!(store.tiers[0].head.index.is_some(), "the index was swapped under them");
    drop(store);
    let store = open(&mut sim, "store");
    for (n, (_, bytes)) in values.iter().enumerate() {
        assert_eq!(bytes_of(&store, &key(n as u64)).as_ref(), Some(bytes), "{n}");
    }
    assert_eq!(store.verify(None), Vec::new());
}

/// A lookup that hits allocates nothing wherever its record is (a writer's segment, a pack a
/// delta spot points at, the mapped index), the first read of a record (which verifies and
/// flags it) included; nor does a warm writer's put, past chunk rolls and past the delta
/// table's high-water mark into a new level.
#[test]
fn hits_and_warm_puts_allocate_nothing() {
    let mut sim = Sim::new(31, quiet());
    let mut store = open(&mut sim, "store");
    let mut writer = store.writer();
    let reader = store.reader();
    let count = INDEX_AFTER as u64 * 3;
    let values: Vec<Vec<u8>> =
        (0..count).map(|n| noise(40 + (n as usize * 53) % 3000, n)).collect();
    let guard = enter();
    let hits = |store: &Store, upto: u64, what: &str| {
        blaze_alloc_count::assert_at_most(0, what, || {
            for n in 0..upto {
                let want = values[n as usize].as_slice();
                let entry = reader.get(&guard, &key(n)).unwrap().expect("put before");
                assert!(entry.bytes == want, "{what}: {n}");
                assert!(
                    store.get(&key(n)).unwrap().is_some_and(|it| it.bytes == want),
                    "{what}: {n}"
                );
            }
        });
    };
    // The first put warms what a thread keeps once: its writer's first chunk is made already.
    writer.put(&guard, key(0), Kind::Query, &values[0]).unwrap();
    let mut pending: Vec<(Key, Vec<u8>)> =
        (1..count - 16).map(|n| (key(n), values[n as usize].clone())).collect();
    let before = store.tiers[0].delta.len();
    blaze_alloc_count::assert_at_most(0, "warm puts", || {
        for (key, bytes) in pending.drain(..) {
            writer.put(&guard, key, Kind::Query, &bytes).unwrap();
        }
    });
    assert!(
        store.tiers[0].delta.len() > before + DELTA_FIRST / 2,
        "the puts went past the delta's first level"
    );
    hits(&store, count - 16, "hits in segments");
    drop(guard);
    flush(&mut sim, &mut store);
    reclaim_all(&mut store);
    let guard = enter();
    let mut rest: Vec<(Key, Vec<u8>)> =
        (count - 16..count).map(|n| (key(n), values[n as usize].clone())).collect();
    blaze_alloc_count::assert_at_most(0, "warm puts after a commit", || {
        for (key, bytes) in rest.drain(..) {
            writer.put(&guard, key, Kind::Query, &bytes).unwrap();
        }
    });
    drop(guard);
    flush(&mut sim, &mut store);
    assert!(store.tiers[0].head.index.is_some(), "an index was written");
    let guard = enter();
    let hits = |store: &Store, what: &str| {
        blaze_alloc_count::assert_at_most(0, what, || {
            for n in 0..count {
                let entry = reader.get(&guard, &key(n)).unwrap().expect("put before");
                assert!(entry.bytes == values[n as usize].as_slice(), "{what}: {n}");
                let in_place = reader.get_in_place(&guard, &key(n)).unwrap();
                assert!(
                    in_place.is_some_and(|(_, it)| it.bytes == values[n as usize].as_slice()),
                    "{what}: {n}"
                );
                assert!(store.has(&key(n)), "{what}: {n}");
            }
        });
    };
    hits(&store, "hits in packs and the index");
    hits(&store, "hits again");
}

/// A pinned value is the value put, at a 16-byte boundary, held past the task and the store:
/// in place (the store's own bytes) for a raw record of a mapped pack at a 16-byte boundary,
/// a copy for one in a writer's segment or kept compressed. A pinned hit in place allocates
/// nothing but the count of its mapping.
#[test]
fn a_pinned_value_is_held_in_place_past_its_task_and_its_store() {
    let mut sim = Sim::new(33, quiet());
    let mut store = open(&mut sim, "store");
    let values: Vec<Vec<u8>> = (0..8).map(|n| text(n, (8 << 10) + n as usize * 7)).collect();
    for (n, value) in values.iter().enumerate() {
        store.put(key(n as u64), Kind::Code, value).unwrap();
    }
    let aligned = |bytes: &[u8]| (bytes.as_ptr() as usize).is_multiple_of(16);
    let in_segment = store.pinned(&key(0)).unwrap().unwrap();
    assert!(!in_segment.in_place(), "a record in a writer's segment is copied");
    assert_eq!(&*in_segment, values[0].as_slice());
    assert!(aligned(&in_segment));
    flush(&mut sim, &mut store);
    let mut pinned = Vec::new();
    for (n, value) in values.iter().enumerate() {
        let it = store.pinned(&key(n as u64)).unwrap().unwrap();
        assert_eq!(
            (&*it, it.kind, it.content),
            (value.as_slice(), Kind::Code, content_of(value)),
            "{n}"
        );
        assert!(aligned(&it), "{n}");
        let kept = store.get(&key(n as u64)).unwrap().unwrap().bytes;
        assert_eq!(
            it.in_place(),
            aligned(kept),
            "{n}: in place exactly when the record is aligned"
        );
        if it.in_place() {
            assert_eq!(it.as_ptr(), kept.as_ptr(), "{n}: the store's own bytes");
        }
        pinned.push(it);
    }
    assert!(store.pinned(&key(99)).unwrap().is_none());
    // Unread for two periods, all but key 0 go to zstd; a pinned read of one is a copy.
    settle(&mut sim, &mut store);
    settle(&mut sim, &mut store);
    assert_eq!(bytes_of(&store, &key(0)).unwrap(), values[0]);
    settle(&mut sim, &mut store);
    assert_eq!(form_of(&store, &key(1)), Form::Zstd);
    let cold = store.pinned(&key(1)).unwrap().unwrap();
    assert!(!cold.in_place());
    assert_eq!(&*cold, values[1].as_slice());
    let reader = store.reader();
    let guard = enter();
    let read = reader.pinned(&guard, &key(0)).unwrap().unwrap();
    drop(guard);
    reclaim_all(&mut store);
    drop(store);
    for (n, it) in pinned.iter().enumerate() {
        assert_eq!(&**it, values[n].as_slice(), "{n}: held past the settle and the store");
    }
    assert_eq!(&*read, values[0].as_slice());
}

/// A module of machine code is a file of its own however short, never moved colder, and once
/// committed its pin names that file, whose bytes are exactly the value's; a record of a pack
/// names none.
#[test]
fn a_module_of_machine_code_is_a_file_of_its_own_that_its_pin_names() {
    let mut sim = Sim::new(34, quiet());
    let mut store = open(&mut sim, "store");
    let code = noise(5000, 34);
    store.put(key(1), Kind::Machine, &code).unwrap();
    store.put(key(2), Kind::Code, &text(2, 5000)).unwrap();
    assert_eq!(form_of(&store, &key(1)), Form::External);
    let in_segment = store.pinned(&key(1)).unwrap().unwrap();
    assert_eq!(in_segment.file(), None, "no file until its commit writes it");
    assert_eq!(&*in_segment, code.as_slice());
    flush(&mut sim, &mut store);
    let file = name(&format!("store/{}", disk::external_file(&content_of(&code))));
    assert_eq!(sim.contents(&file), code);
    let pinned = store.pinned(&key(1)).unwrap().unwrap();
    assert!(pinned.in_place());
    assert_eq!(pinned.file(), Some(&file));
    assert_eq!(&*pinned, code.as_slice());
    assert_eq!(
        store.pinned(&key(2)).unwrap().unwrap().file(),
        None,
        "a record of a pack is no file of its own"
    );
    for _ in 0..4 {
        settle(&mut sim, &mut store);
    }
    assert_eq!(form_of(&store, &key(1)), Form::External, "machine code is never compressed");
    drop((pinned, in_segment, store));
    let store = open(&mut sim, "store");
    let reopened = store.pinned(&key(1)).unwrap().unwrap();
    assert_eq!((reopened.file(), &*reopened), (Some(&file), code.as_slice()));
}

/// A handle reads beside the store and puts through a writer of its own: what it puts is read
/// at once, by it and by the store, refused with other bytes under the same key, durable with
/// the store's next flush and there after reopening. A hit through it allocates nothing.
#[test]
fn a_handle_reads_and_puts_beside_the_store_and_its_puts_are_durable_with_a_flush() {
    let mut sim = Sim::new(35, quiet());
    let mut store = open(&mut sim, "store");
    store.put(key(1), Kind::Query, b"the store's").unwrap();
    let root = std::path::PathBuf::from("/state");
    let handle = Handle::new(&mut store, root.clone());
    assert_eq!(handle.read(&key(1), |it| it.bytes.to_vec()).unwrap().unwrap(), b"the store's");
    assert!(handle.has(&key(1)) && !handle.has(&key(2)));
    assert!(handle.read(&key(2), |_| ()).unwrap().is_none());
    let code = noise(3000, 35);
    let ticket = handle.put(key(2), Kind::Machine, &code).unwrap();
    assert!(!handle.durable(ticket));
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), code);
    assert_eq!(&*handle.pinned(&key(2)).unwrap().unwrap(), code.as_slice());
    assert_eq!(
        handle.put(key(2), Kind::Machine, &code),
        Ok(ticket),
        "the same bytes again are the first put"
    );
    assert!(handle.put(key(2), Kind::Machine, b"other").is_err());
    flush(&mut sim, &mut store);
    assert!(handle.durable(ticket));
    let pinned = handle.pinned(&key(2)).unwrap().unwrap();
    let file = pinned.file().expect("a module of machine code is a file of its own");
    assert_eq!(handle.path_of(file), root.join(file.as_str()));
    assert_eq!(sim.contents(file), code);
    _ = handle.read(&key(1), |it| it.bytes.len());
    blaze_alloc_count::assert_at_most(0, "hits through a handle", || {
        assert_eq!(handle.read(&key(1), |it| it.bytes.len()).unwrap(), Some(11));
        assert!(handle.has(&key(2)));
    });
    drop((pinned, handle, store));
    let store = open(&mut sim, "store");
    assert_eq!(bytes_of(&store, &key(2)).unwrap(), code);
}

/// On this machine's disk, under the native driver: a module of machine code put through a
/// handle is, after a flush, a file at the handle's path of its pin holding exactly its bytes,
/// and there in a store opened again.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn a_driven_store_keeps_machine_code_as_a_file_on_disk() {
    let root = std::env::temp_dir().join(format!("blaze-cas-driven-{}", std::process::id()));
    _ = std::fs::remove_dir_all(&root);
    let code = noise(70_000, 37);
    {
        let mut driven = Driven::open(&root).unwrap();
        let handle = driven.handle();
        handle.put(key(1), Kind::Machine, &code).unwrap();
        handle.put(key(2), Kind::Query, b"a query's result").unwrap();
        driven.flush().unwrap();
        let pinned = handle.pinned(&key(1)).unwrap().unwrap();
        let file = pinned.file().expect("committed machine code is a file of its own");
        assert_eq!(std::fs::read(handle.path_of(file)).unwrap(), code);
    }
    let mut driven = Driven::open(&root).unwrap();
    let handle = driven.handle();
    let pinned = handle.pinned(&key(1)).unwrap().unwrap();
    assert_eq!(&*pinned, code.as_slice());
    assert_eq!(std::fs::read(handle.path_of(pinned.file().unwrap())).unwrap(), code);
    assert_eq!(handle.read(&key(2), |it| it.bytes.to_vec()).unwrap().unwrap(), b"a query's result");
    drop((pinned, handle, driven));
    _ = std::fs::remove_dir_all(&root);
}

/// A process has one installed handle: the first, which [`Handle::installed`] gives; a second
/// is refused, naming the first.
#[test]
fn a_process_installs_one_handle() {
    let mut sim = Sim::new(36, quiet());
    let mut store = open(&mut sim, "store");
    let first = Handle::install(Handle::new(&mut store, "/first".into())).unwrap();
    assert!(std::ptr::eq(first, Handle::installed().unwrap()));
    let refused = Handle::install(Handle::new(&mut store, "/second".into())).err().unwrap();
    assert!(refused.contains("/first"), "{refused}");
}

/// What a test's fetches let in: everything that hashes as sent.
fn any(_: Kind, _: &[u8]) -> Result<(), String> {
    Ok(())
}
