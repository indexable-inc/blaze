//! The hosted world over the simulated driver, both sides compiled here: a hosted `hello`
//! writes its line to the simulated standard output and ends 0; a grant the host withholds
//! ends the start as refused; an escaped effect (a timer) is waited for by the host; and a
//! hostile program's calls (unknown handles, ranges past the exchange, bad escape bytes) are
//! each refused with a typed answer and write nothing.

use std::time::Duration;

use blaze_sim::{Config, Tape};

use super::{check, control};
use crate::{
    Plant,
    cx::Cx,
    entry::{Error, Status},
    guest,
    hosting::Hosting,
    io::Io,
    wire::{self, Answer, HostTable, code},
};

const EXCHANGE: usize = 4096;

fn hello(cx: &Cx, out: crate::out::Out) -> Result<(), Error> {
    out.line(cx, "Hello, world!")?;
    Ok(())
}

/// What `#[bstd::main]` makes of `hello` for a hosted start.
fn hosted_hello(cx: &Cx, io: Io) -> Status {
    let ran = hello(cx, match guest::grant::<crate::out::Out>(&io, "out") {
        Some(granted) => granted,
        None => return Status::REFUSED,
    });
    guest::finish(&io, ran)
}

/// A program that asks the endpoint `api`, which a host gives only by a grant.
fn hosted_net(_cx: &Cx, io: Io) -> Status {
    match guest::grant::<crate::net::Net>(&io, "api") {
        Some(_) => Status::OK,
        None => Status::REFUSED,
    }
}

fn host(io: Io, grants: &[(&str, &str)]) -> Box<Hosting> {
    let grants = grants.iter().map(|(kind, name)| ((*kind).to_owned(), (*name).to_owned())).collect();
    Hosting::new(io, vec!["hello".to_owned()], grants, EXCHANGE, blaze_own::testing::heap().shared())
}

#[test]
fn a_hosted_hello_writes_its_line_and_ends_ok() {
    let io = Io::sim(1);
    let hosting = host(io.clone(), &[]);
    let table = hosting.table();
    let status = guest::start(&table, hosted_hello);
    assert_eq!(status, 0);
    assert_eq!(io.script(|host| host.out.clone()), Some(b"Hello, world!\n".to_vec()));
    assert_eq!(hosting.exited(), None, "main returned; nothing called exit");
}

/// What `#[bstd::main]` makes of `fn main(cx: &Cx, out: Out)` (the owner's hello, D104) for a
/// hosted start.
fn hosted_granted_hello(cx: &Cx, io: Io) -> Status {
    let out = match guest::grant::<crate::out::Out>(&io, "out") {
        Some(granted) => granted,
        None => return Status::REFUSED,
    };
    let ran: Result<(), Error> = out.line(cx, "Hello, world!").map_err(Error::from);
    guest::finish(&io, ran)
}

/// `out` fills itself: the host answers it with no grant, as the launcher does. With the host's
/// old rule (only what `grants` lists) the start is refused and this fails.
#[test]
fn a_kind_that_fills_itself_is_answered_with_no_grant() {
    let io = Io::sim(5);
    let hosting = host(io.clone(), &[]);
    assert_eq!(guest::start(&hosting.table(), hosted_granted_hello), 0);
    assert_eq!(io.script(|host| host.out.clone()), Some(b"Hello, world!\n".to_vec()));
}

/// What `#[bstd::main]` makes of `fn main(cx: &Cx, heap: Heap)` for a hosted start: a value
/// boxed in the heap it was handed, read back and dropped (7 when all of it held).
fn hosted_heap(_cx: &Cx, io: Io) -> Status {
    let heap = match guest::grant::<crate::mem::Heap>(&io, "heap") {
        Some(granted) => granted,
        None => return Status::REFUSED,
    };
    let boxed = crate::boxed::Box::new_in(7u8, heap);
    let status = Status(*boxed);
    drop(boxed);
    status
}

/// The hosted start hands a `heap: Heap` parameter the heap it took (D120), with no grant:
/// the heap fills itself, so the host answers it unasked, and the value boxed in it is read
/// back. Without `Kind::Heap` in `Kind::FILLS_ITSELF` the host refuses `heap` and the start
/// ends refused.
/// Prints `test tests::guest::the_hosted_start_hands_main_its_heap ... ok`.
#[test]
fn the_hosted_start_hands_main_its_heap() {
    let io = Io::sim(6);
    let hosting = host(io.clone(), &[]);
    assert_eq!(guest::start(&hosting.table(), hosted_heap), 7);
}

#[test]
fn a_grant_the_host_withholds_refuses_the_start() {
    let io = Io::sim(2);
    let hosting = host(io.clone(), &[("out", "io")]);
    let status = guest::start(&hosting.table(), hosted_net);
    assert_eq!(status, i32::from(Status::REFUSED.0));
    assert_eq!(io.script(|host| host.out.clone()), Some(Vec::new()));
}

#[test]
fn a_table_of_another_version_or_size_or_exchange_is_refused_before_any_call() {
    let hosting = host(Io::sim(3), &[]);
    let failed = i32::from(Status::FAILED.0);
    assert_eq!(guest::start(core::ptr::null(), hosted_hello), failed);
    let mut table = hosting.table();
    table.version = wire::VERSION + 1;
    assert_eq!(guest::start(&table, hosted_hello), failed);
    let mut table = hosting.table();
    table.size -= 1;
    assert_eq!(guest::start(&table, hosted_hello), failed);
    let mut table = hosting.table();
    table.exchange_len = guest::EXCHANGE_LEAST - 1;
    assert_eq!(guest::start(&table, hosted_hello), failed);
}

std::thread_local! {
    /// Where `smuggler` leaves its hosted `Io`, for the test to use outside the run.
    static SMUGGLED: core::cell::RefCell<Option<Io>> = const { core::cell::RefCell::new(None) };
}

/// Keeps its `Io` past the run, and tries it from another thread during the run: what the
/// invariant behind the lockless hosted world (`guest::Run`) refuses.
fn smuggler(_cx: &Cx, io: Io) -> Status {
    let elsewhere = io.clone();
    let other = std::thread::spawn(move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || elsewhere.now())).is_err());
    let refused_elsewhere = other.join().expect("the other thread ends");
    SMUGGLED.with(|kept| *kept.borrow_mut() = Some(io));
    if refused_elsewhere { Status::OK } else { Status::FAILED }
}

/// The invariant that lets a hosted world go without a lock: its state is reached only by
/// the thread inside its start, during the call. A hosted `Io` reached from another thread
/// during the run, or from the same thread after it, stops with a panic naming that, and
/// the host's exchange is never touched. (The control: inside the run, the same `Io` writes,
/// as the hello above shows.)
#[test]
fn a_hosted_world_is_reached_only_inside_its_run() {
    let io = Io::sim(5);
    let hosting = host(io.clone(), &[]);
    assert_eq!(guest::start(&hosting.table(), smuggler), 0, "another thread was refused during the run");
    let kept = SMUGGLED.with(|kept| kept.borrow_mut().take()).expect("the program kept its Io");
    let out = kept.out().expect("a hosted world offers out");
    let cx = super::root(&io);
    let said = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| out.line(&cx, "late")))
        .expect_err("a write after the run is refused");
    let said = said.downcast_ref::<&str>().copied().unwrap_or("");
    assert!(said.contains("outside its run"), "{said}");
    assert_eq!(io.script(|host| host.out.clone()), Some(Vec::new()), "nothing reached the host");
}

/// A sleep is an escaped `Effect::Timer`: the host waits for it on its driver, whose clock
/// moves by the sleep; the program's clock is the host's.
fn sleeper(cx: &Cx, io: Io) -> Status {
    let clock = match guest::grant::<crate::clock::Clock>(&io, "clock") {
        Some(clock) => clock,
        None => return Status::REFUSED,
    };
    let before = clock.now();
    if clock.sleep(cx, Duration::from_millis(30)).is_err() {
        return Status::FAILED;
    }
    let slept = clock.now().since_start() - before.since_start();
    if slept >= Duration::from_millis(30) { Status::OK } else { Status::FAILED }
}

#[test]
fn an_escaped_timer_is_waited_for_by_the_host() {
    let io = Io::sim(4);
    let hosting = host(io.clone(), &[]);
    let before = io.clock().expect("the simulation's clock").now();
    assert_eq!(guest::start(&hosting.table(), sleeper), 0);
    let after = io.clock().expect("the simulation's clock").now();
    assert!(after.since_start() - before.since_start() >= Duration::from_millis(30));
}

/// One call a hostile program makes.
#[derive(Clone, Copy, Debug)]
enum Call {
    Write { stream: u32, at: usize, len: usize },
    Clock { which: u32 },
    Arg { index: u64 },
    Grant { at: usize, len: usize },
    Effect { at: usize, len: usize, bytes: [u8; 3] },
}

/// An offset or a length: in range, at its edges, or far past them.
fn place(tape: &Tape) -> usize {
    *tape.one_of(&[0, 1, 7, EXCHANGE - 1, EXCHANGE, EXCHANGE + 1, usize::MAX / 2, usize::MAX], "a place")
}

fn plan(tape: &Tape) -> Vec<Call> {
    let mut calls = Vec::new();
    while tape.more(12, "another call") {
        let call = match tape.below(5, "an entry") {
            0 => Call::Write { stream: tape.below(4, "a stream") as u32, at: place(tape), len: place(tape) },
            1 => Call::Clock { which: tape.below(4, "a clock") as u32 },
            2 => Call::Arg { index: tape.one_of(&[0, 1, u64::MAX], "an index").to_owned() },
            3 => Call::Grant { at: place(tape), len: place(tape) },
            _ => Call::Effect {
                at: place(tape),
                len: place(tape),
                bytes: [tape.below(256, "a byte") as u8, tape.below(256, "a byte") as u8, tape.below(256, "a byte") as u8],
            },
        };
        calls.push(call);
    }
    calls
}

/// The exchange's byte at `at`, as the test fills it.
fn pattern(at: usize) -> u8 {
    (at % 251) as u8
}

/// `call` made through `table`.
#[expect(unsafe_code, reason = "a hostile program's calls through the host's table")]
fn make(table: &HostTable, call: Call) -> Answer {
    // SAFETY: the table's own entries and pointer, its `Hosting` alive for the test.
    unsafe {
        match call {
            Call::Write { stream, at, len } => (table.write)(table.host, stream, at, len),
            Call::Clock { which } => (table.clock)(table.host, which),
            Call::Arg { index } => (table.arg)(table.host, index),
            Call::Grant { at, len } => (table.grant)(table.host, at, len, 0, 0),
            Call::Effect { at, len, .. } => (table.effect)(table.host, at, len, 0),
        }
    }
}

/// Whether `at..at + len` is inside the exchange, said without the host's code.
fn inside(at: usize, len: usize) -> bool {
    at <= EXCHANGE && len <= EXCHANGE - at
}

/// Every call is answered with the code an independent reading of the wire says, and the
/// simulated standard output holds exactly the bytes of the writes that were in range to a
/// known stream.
fn run(calls: &Vec<Call>, plant: Plant) -> Result<(), String> {
    let io = super::world(9, Plant::None);
    // The host's plant is its own (`Hosting`'s), carried here by any plant the control sets.
    let hosting = match plant {
        Plant::None => host(io.clone(), &[]),
        _ => host(io.clone(), &[]).plant_stream_unchecked(),
    };
    let table = hosting.table();
    let mut out = Vec::new();
    for call in calls {
        let fill: Vec<u8> = (0..EXCHANGE).map(pattern).collect();
        hosting.put(0, &fill);
        if let Call::Effect { at, bytes, .. } = *call
            && at <= EXCHANGE - 3
        {
            hosting.put(at, &bytes);
        }
        let answer = make(&table, *call);
        let expected = match *call {
            Call::Write { stream, at, len } => {
                if stream != wire::stream::OUT && stream != wire::stream::ERR {
                    code::UNKNOWN
                } else if !inside(at, len) {
                    code::OUT_OF_RANGE
                } else {
                    if stream == wire::stream::OUT {
                        out.extend((at..at + len).map(pattern));
                    }
                    code::OK
                }
            }
            Call::Clock { which } => {
                if which == wire::clock::MONOTONIC || which == wire::clock::WALL { code::OK } else { code::UNKNOWN }
            }
            Call::Arg { index } => if index == 0 { code::OK } else { code::END },
            Call::Grant { at, len } => if inside(at, len) { code::REFUSED } else { code::OUT_OF_RANGE },
            // Any three bytes or fewer: no effect of this wire is that short and that valid
            // but the ones `effect_bytes` tells apart below.
            Call::Effect { at, len, bytes } => {
                if !inside(at, len) {
                    code::OUT_OF_RANGE
                } else {
                    let given: Vec<u8> = if at <= EXCHANGE - 3 {
                        (at..at + len).map(|it| if it - at < 3 { bytes[it - at] } else { pattern(it) }).collect()
                    } else {
                        (at..at + len).map(pattern).collect()
                    };
                    match guest::codec::read_effect(&given) {
                        Ok(_) => answer.code,
                        Err(_) => code::BAD_BYTES,
                    }
                }
            }
        };
        if answer.code != expected {
            return Err(format!("{call:?} answered {answer:?}, and the wire says code {expected}"));
        }
    }
    let written = io.script(|host| host.out.clone()).unwrap_or_default();
    if written != out {
        return Err(format!("standard output holds {} bytes; the in-range writes were {}", written.len(), out.len()));
    }
    Ok(())
}

/// The invariant: the hosted world refuses every hostile guest call (including unknown streams, out-of-bounds ranges, and invalid escapes) with a typed error without corrupting host state.
#[test]
fn a_hostile_programs_calls_are_each_refused_with_a_typed_answer() {
    blaze_sim::check(Config::cases(300), |tape| check(plan(tape), run));
}

#[test]
fn the_oracle_catches_a_host_that_takes_an_unknown_stream() {
    // `OffersIgnored` stands for the host's own plant (see `run`): a handle not checked.
    control(Plant::OffersIgnored, plan, run);
}
