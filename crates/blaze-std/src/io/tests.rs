//! The tests of [`super::fast`] (std.md, "End state: efficiency", pieces 6, 7 and 11). Each
//! prints its line once it passed:
//!
//! - `native_now_takes_no_lock`: `native now: 0 locks in 1000000 reads`; its control, a
//!   simulated world's reads, each counted, shows the count sees a locked read.
//! - `the_simulated_clock_moves_only_in_the_drivers_calls`: `simulated clock: unmoved by
//!   reads, submits and takes in 256 scripts; the planted real clock caught`. Its plant gives a
//!   simulated world the native route's clock.
//! - `a_queued_call_still_ends_at_its_deadline`: `a timer queued beside inline calls: stopped
//!   at its deadline after <n> ms`.
//! - `the_two_native_routes_answer_equal` (macOS, the one native driver with inline calls):
//!   `native routes: 32 scripts, inline and queued equal; <n> calls inline`. Its plant, an
//!   inline call that swallows an error, is blaze-core's (`inline_and_queued_calls_end_equal`,
//!   threads/inline.rs), where the inline calls are made.
//! - `a_lent_write_copies_nothing_natively` (macOS): `lent writes: 0 copies natively, 2 in
//!   the simulation's queue; the bytes equal`.
//! - `a_lent_append_is_torn_by_a_crash_as_the_model_says`: `lent append through 64 crashes:
//!   each the durable bytes then a torn prefix`.
//!
//! The borrow of a lent write cannot end before the system holds its bytes: the write ends
//! inside the call that borrows them, so no planted early release can be written (a
//! `Pending` that outlived the call is the form that could; `Io::write_lent`'s doc says why it
//! is not used).

#[cfg(target_os = "macos")]
use std::string::String;
use std::{
    format, println,
    time::{Duration, Instant},
    vec,
    vec::Vec,
};

use core::sync::atomic::Ordering;

#[cfg(target_os = "macos")]
use blaze_core::Done;
use blaze_core::{Effect, FileName, Tag, wait::Deadline};
use blaze_sim::{Broke, Config, Tape};

use super::{Io, Limits, NativeWorld, Reach, fast::Time};
use crate::{
    Stopped,
    clock::SleepError,
    cx::Cx,
    fs::Unsynced,
    mem::TASK_FRAME,
    path::Path,
};

mod read_at;

/// The reads `native_now_takes_no_lock` makes.
const READS: u64 = 1_000_000;

/// A test's root budget: longer than any of these tests waits.
const BUDGET: Duration = Duration::from_secs(600);

/// A native world over `root`, with two file threads.
fn native(root: &std::path::Path) -> Io {
    Io::native(NativeWorld {
        root,
        threads: 2,
        env: crate::env::Vars::given(Vec::new()),
        args: crate::args::ArgList::given(Vec::new()),
        host_name: alloc::sync::Arc::from(""),
        seed: Some(7),
        above: 0,
        limits: Limits::USUAL,
    })
    .expect("a native world over a test's directory")
}

fn root(io: &Io) -> Cx {
    io.cx(BUDGET, TASK_FRAME).expect("a test's root frame is made")
}

fn path(raw: &str) -> Path {
    Path::new(raw).expect("a test's path is relative and plain")
}

fn name(raw: &str) -> FileName {
    FileName::new(raw).expect("a test's name is plain")
}

#[test]
fn native_now_takes_no_lock() {
    let dir = blaze_core::temp::TempDir::for_test("bstd-now-");
    let io = native(&dir);
    let seen = io.seen().expect("a native world is shared");
    let before = seen.locks.load(Ordering::Relaxed);
    let mut last = Duration::ZERO;
    for _ in 0..READS {
        let now = io.now();
        assert!(now >= last, "the native clock went back: {now:?} after {last:?}");
        last = now;
    }
    let locks = seen.locks.load(Ordering::Relaxed) - before;
    assert_eq!(locks, 0, "native now took the world's lock {locks} times in {READS} reads");

    // The control: a simulated world's clock is under its lock, and the count sees each read.
    let sim = Io::sim(5);
    let counted = sim.seen().expect("a simulated world is shared");
    let before = counted.locks.load(Ordering::Relaxed);
    for _ in 0..1000 {
        sim.now();
    }
    assert_eq!(counted.locks.load(Ordering::Relaxed) - before, 1000, "each simulated read takes the lock once");
    println!("native now: {locks} locks in {READS} reads");
}

/// What a step of the clock's script did to it, said when it broke.
fn moved(step: u64, what: &str, was: Duration, is: Duration) -> Broke {
    Broke::Oracle(format!("step {step}: {what} moved the clock from {was:?} to {is:?}"))
}

/// One script on a simulated world: reads, submits and takes leave the clock as it was; a
/// poll moves it forward and never past its deadline, and one that moved nothing ends at its
/// deadline (`idle_until`). `real_clock` is the plant: the world reads the native route's
/// clock.
fn clock_script(tape: &Tape, real_clock: bool) -> Result<(), Broke> {
    let mut io = Io::sim(tape.next_u64("seed"));
    if real_clock && let Reach::Shared { beside, .. } = &mut io.reach {
        beside.time = Time::Native(blaze_core::wait::Monotonic::new());
    }
    let mut tags: Vec<Tag> = Vec::new();
    let mut last = io.now();
    let steps = 1 + tape.below(40, "steps");
    for step in 0..steps {
        match tape.below(5, "act") {
            0 => {
                let (one, two) = (io.now(), io.now());
                if one != last || two != last {
                    return Err(moved(step, "a read", last, if one != last { one } else { two }));
                }
            }
            1 => {
                let after = Duration::from_millis(tape.below(50, "timer ms"));
                tags.push(io.submit(|tag| Effect::Timer { after, tag }));
                let now = io.now();
                if now != last {
                    return Err(moved(step, "a timer's submit", last, now));
                }
            }
            2 => {
                let file = name(["a", "b"][tape.index(2, "file")]);
                tags.push(io.submit(|tag| Effect::Stat { file, tag }));
                let now = io.now();
                if now != last {
                    return Err(moved(step, "a stat's submit", last, now));
                }
            }
            3 => {
                tags.retain(|tag| io.take(*tag).is_none());
                let now = io.now();
                if now != last {
                    return Err(moved(step, "a take", last, now));
                }
            }
            _ => {
                let until = Deadline::after(last, Duration::from_millis(tape.below(30, "poll ms")));
                let count = io.poll(until);
                let now = io.now();
                let most = last.max(until.get());
                if now < last || now > most {
                    return Err(Broke::Oracle(format!(
                        "step {step}: a poll to {:?} took the clock from {last:?} to {now:?}",
                        until.get()
                    )));
                }
                if count == 0 && now != most {
                    return Err(Broke::Oracle(format!(
                        "step {step}: a poll that moved nothing ended at {now:?}, not at its deadline {most:?} (idle_until)"
                    )));
                }
                last = now;
            }
        }
    }
    Ok(())
}

#[test]
fn the_simulated_clock_moves_only_in_the_drivers_calls() {
    blaze_sim::check(Config::cases(256), |tape| clock_script(tape, false));
    blaze_sim::control(Config::cases(256), false, true, "moved the clock", clock_script);
    println!("simulated clock: unmoved by reads, submits and takes in 256 scripts; the planted real clock caught");
}

#[test]
fn a_queued_call_still_ends_at_its_deadline() {
    let dir = blaze_core::temp::TempDir::for_test("bstd-deadline-");
    let io = native(&dir);
    let cx = root(&io);
    // An inline call first, so the world is one whose file calls go inline where it can.
    let fs = io.need_fs().expect("a native world offers its files");
    fs.root().stat(&cx, &path("nothing-here")).expect("a stat of a missing name answers None");
    let clock = io.need_clock().expect("a native world offers its clock");
    let short = cx.until(Deadline::after(io.now(), Duration::from_millis(50)));
    let began = Instant::now();
    let slept = clock.sleep(&short, Duration::from_secs(10));
    let took = began.elapsed();
    assert!(
        matches!(slept, Err(SleepError::Stopped(Stopped::Deadline { .. }))),
        "a 10 s timer under a 50 ms deadline: {slept:?}"
    );
    assert!(took < Duration::from_secs(5), "the deadline ended the wait only after {took:?}");
    println!("a timer queued beside inline calls: stopped at its deadline after {} ms", took.as_millis());
}

/// One step drawn: what both twins are asked.
#[cfg(target_os = "macos")]
fn draw_step(tape: &Tape) -> (Effect, bool) {
    let file = name(["a", "b", "d/c", "missing"][tape.index(4, "file")]);
    let tag = Tag(0);
    let effect = match tape.below(7, "kind") {
        0 => Effect::Stat { file, tag },
        1 => Effect::Lstat { file, tag },
        2 => Effect::ReadAll { file, tag },
        3 => Effect::Read { file, at: tape.below(8, "at"), len: tape.below(8, "len") as usize, tag },
        4 => Effect::Append { file, bytes: vec![b'x'; 1 + tape.below(5, "n") as usize], tag },
        5 => Effect::WriteAt { file, at: tape.below(10, "at"), bytes: vec![b'y'; 1 + tape.below(4, "n") as usize], tag },
        // A queued effect among them: it keeps its route on both twins.
        _ => Effect::MakeDir { dir: name("made"), tag },
    };
    (effect, tape.coin("lent"))
}

/// What a call's end says, without what twin directories differ in (times, the root's path
/// in an error's text).
#[cfg(target_os = "macos")]
fn view(done: Result<Done, Stopped>) -> String {
    match done {
        Ok(Done::Stat(Some(meta))) => format!("stat {} {}", meta.len, meta.dir),
        Ok(Done::Failed(_)) => String::from("failed"),
        Ok(done) => format!("{done:?}"),
        Err(stopped) => format!("stopped {stopped:?}"),
    }
}

/// `effect` asked of `io` under `cx`: through `Io::call`, or as a lent write when `lent`.
#[cfg(target_os = "macos")]
fn ask(io: &Io, cx: &Cx, effect: Effect, lent: bool) -> Result<Done, Stopped> {
    match effect {
        Effect::Append { file, bytes, .. } if lent => io.write_lent(cx, &file, super::Lent::End, &bytes),
        Effect::WriteAt { file, at, bytes, .. } if lent => io.write_lent(cx, &file, super::Lent::At(at), &bytes),
        effect => io.call(cx, move |tag| retag(effect, tag)),
    }
}

/// `effect` with `tag`: the script draws its effects before the world numbers them.
#[cfg(target_os = "macos")]
fn retag(effect: Effect, tag: Tag) -> Effect {
    match effect {
        Effect::Stat { file, .. } => Effect::Stat { file, tag },
        Effect::Lstat { file, .. } => Effect::Lstat { file, tag },
        Effect::ReadAll { file, .. } => Effect::ReadAll { file, tag },
        Effect::Read { file, at, len, .. } => Effect::Read { file, at, len, tag },
        Effect::Append { file, bytes, .. } => Effect::Append { file, bytes, tag },
        Effect::WriteAt { file, at, bytes, .. } => Effect::WriteAt { file, at, bytes, tag },
        Effect::MakeDir { dir, .. } => Effect::MakeDir { dir, tag },
        other => panic!("the script draws no {other:?}"),
    }
}

#[cfg(target_os = "macos")]
#[test]
fn the_two_native_routes_answer_equal() {
    let inline_calls = core::cell::Cell::new(0u64);
    blaze_sim::check(Config::cases(32), |tape| -> Result<(), Broke> {
        let (one, two) = (
            blaze_core::temp::TempDir::for_test("bstd-routes-queued-"),
            blaze_core::temp::TempDir::for_test("bstd-routes-inline-"),
        );
        let (queued, inline) = (native(&one), native(&two));
        queued.queued_only(true);
        let (qcx, icx) = (root(&queued), root(&inline));
        let steps = 1 + tape.below(24, "steps");
        for step in 0..steps {
            let (effect, lent) = draw_step(tape);
            let asked = format!("{effect:?}");
            let (a, b) = (view(ask(&queued, &qcx, effect.clone(), lent)), view(ask(&inline, &icx, effect, lent)));
            if a != b {
                return Err(Broke::Oracle(format!("step {step}, {asked} (lent {lent}): queued {a}, inline {b}")));
            }
        }
        let counted = |io: &Io| io.seen().map_or(0, |seen| seen.inline.load(Ordering::Relaxed));
        if counted(&queued) != 0 {
            return Err(Broke::Oracle(format!("the switched-off world made {} calls inline", counted(&queued))));
        }
        inline_calls.set(inline_calls.get() + counted(&inline));
        Ok(())
    });
    assert!(inline_calls.get() > 0, "no call went inline: the comparison compared one route with itself");
    println!("native routes: 32 scripts, inline and queued equal; {} calls inline", inline_calls.get());
}

/// The lent writes' script on `io`: 100 bytes appended, 100 written after them, read back.
#[cfg(target_os = "macos")]
fn lent_writes(io: &Io) -> Vec<u8> {
    let cx = root(io);
    let fs = io.need_fs().expect("a world with files");
    let file = fs.root().create(&cx, &path("lent.bin")).expect("lent.bin is made");
    let data: Vec<u8> = (0..200u32).map(|it| (it * 7 % 251) as u8).collect();
    file.append_unsynced(&cx, &data[..100], Unsynced::No).expect("the first half appended");
    file.write_at_unsynced(&cx, 100, &data[100..], Unsynced::No).expect("the second half written at 100");
    let mut into = cx.bytes();
    file.read_all(&cx, &mut into).expect("lent.bin read back");
    assert_eq!(into.as_slice(), &data[..], "the file holds the lent bytes");
    file.close_unsynced(Unsynced::No).expect("lent.bin closed");
    data
}

#[cfg(target_os = "macos")]
#[test]
fn a_lent_write_copies_nothing_natively() {
    let dir = blaze_core::temp::TempDir::for_test("bstd-lent-");
    let io = native(&dir);
    lent_writes(&io);
    let seen = io.seen().expect("a native world is shared");
    let copies = seen.copies.load(Ordering::Relaxed);
    assert_eq!(copies, 0, "a native lent write copied its bytes {copies} times");
    assert!(seen.inline.load(Ordering::Relaxed) >= 2, "the two writes went inline");

    // The control: the simulation's queue keeps its own copy of each.
    let sim = Io::sim(3);
    lent_writes(&sim);
    let queued = sim.seen().expect("a simulated world is shared").copies.load(Ordering::Relaxed);
    assert_eq!(queued, 2, "the simulation copies each lent write into its effect once");
    println!("lent writes: {copies} copies natively, {queued} in the simulation's queue; the bytes equal");
}

/// `File::read_all` read straight into `into` (`Io::read_all_lent`) answers as the queued
/// `Effect::ReadAll` does: the same bytes after what `into` held, for a file of 0, 1, 4095,
/// 4096 and 70,001 bytes; and it went inline (the control: the switched-off world made no
/// inline call, so the two routes really are two).
#[cfg(target_os = "macos")]
#[test]
fn a_whole_read_is_lent_natively_and_equals_the_queued_one() {
    let (one, two) = (
        blaze_core::temp::TempDir::for_test("bstd-whole-queued-"),
        blaze_core::temp::TempDir::for_test("bstd-whole-lent-"),
    );
    let (queued, lent) = (native(&one), native(&two));
    queued.queued_only(true);
    for (n, len) in [0usize, 1, 4095, 4096, 70_001].into_iter().enumerate() {
        let data: Vec<u8> = (0..len as u32).map(|it| (it * 13 % 251) as u8).collect();
        let mut got = Vec::new();
        for io in [&queued, &lent] {
            let cx = root(io);
            let top = io.need_fs().expect("a world with files").root();
            let name = path(&format!("whole-{n}"));
            top.publish(&cx, &name, crate::fs::Content::bytes(&data), crate::fs::Durable::No).expect("the file is written");
            let file = top.open_read(&cx, &name).expect("the file is opened");
            let mut into = cx.bytes();
            into.try_extend(b"pre").expect("room for the prefix");
            let read = file.read_all(&cx, &mut into).expect("the whole read");
            assert_eq!(read, len, "read_all says it read the file's {len} bytes");
            got.push(into.as_slice().to_vec());
        }
        assert_eq!(got[0], got[1], "a {len}-byte file: the lent read and the queued read differ");
        assert_eq!(&got[1][..3], b"pre", "what `into` held is kept");
        assert_eq!(&got[1][3..], &data[..], "a {len}-byte file read whole");
    }
    let counted = |io: &Io| io.seen().map_or(0, |seen| seen.inline.load(Ordering::Relaxed));
    assert_eq!(counted(&queued), 0, "the switched-off world made calls inline");
    assert!(counted(&lent) >= 5, "the whole reads did not go inline");
    println!("whole reads: 5 lengths, lent and queued equal; {} calls inline", counted(&lent));
}

#[test]
fn a_lent_append_is_torn_by_a_crash_as_the_model_says() {
    let durable: Vec<u8> = vec![9; 700];
    let lent: Vec<u8> = (0..1500u32).map(|it| (it % 253) as u8).collect();
    for seed in 0..64 {
        let io = Io::sim(seed);
        let cx = root(&io);
        let fs = io.need_fs().expect("a world with files");
        // Published synced, its name too: a `create`d file's name is not durable, so a crash
        // would take the whole file (seed 0 did), which is not the lent write's to answer for.
        fs.root()
            .publish_synced(&cx, &path("log"), crate::fs::Content::bytes(&durable))
            .expect("the durable bytes, published synced");
        let file = fs.root().open(&cx, &path("log")).expect("log is opened for the append");
        file.append_unsynced(&cx, &lent, Unsynced::No).expect("the lent bytes, appended unsynced");
        assert!(io.crash(), "the world is simulated");
        let mut into = cx.bytes();
        file.read_all(&cx, &mut into).expect("log read after the crash");
        let got = into.as_slice();
        assert!(
            blaze_core::sim::appended_and_torn(&durable, &lent, got),
            "seed {seed}: after a crash log holds {} bytes: not the {} durable then the {} lent torn by sectors",
            got.len(),
            durable.len(),
            lent.len()
        );
        file.close_unsynced(Unsynced::No).expect("log closed");
    }
    println!("lent append through 64 crashes: each the durable bytes then a torn prefix");
}
