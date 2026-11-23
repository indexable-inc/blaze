//! The records' promises, each test failing without the rule it names. The expected line of the
//! whole run: `test blaze-spawn: exit 0; 7 passed, 0 failed`.

use core::fmt::Write as _;
use std::format;

use blaze_ids::ContentId;
use blaze_own::{AString, AVec, Alloc};
use blaze_sim::{Config, Tape, check};

use crate::{
    Executor, Files, Input, MACHINE_V1, Machine, Malformed, NoSandbox, Off, Personality, RequestError, Seen,
    SpawnAnswer, SpawnRefused, SpawnRequest, Status, Tool, Written, fixed_env,
};

fn g() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn s(text: &str) -> AString<'static> {
    AString::from_str_in(text, g())
}

fn named(prefix: &str, n: u32) -> AString<'static> {
    let mut name = s(prefix);
    write!(name, "{n}").expect("blaze-spawn tests: writing a number into a String does not fail");
    name
}

fn id(n: u64) -> ContentId {
    ContentId::of("blaze-spawn test", &n.to_le_bytes())
}

fn tool(name: &str) -> Tool<'static> {
    let mut guest_path = s("/t/");
    guest_path.push_str(name);
    Tool { name: s(name), digest: id(name.len() as u64).0, guest_path }
}

fn list<T>(items: impl IntoIterator<Item = T>) -> AVec<'static, T> {
    let mut out = AVec::new_in(g());
    for item in items {
        out.push(item);
    }
    out
}

/// The 13 fields in declared order, as `base` changes them.
const FIELDS: [&str; 13] = [
    "step",
    "personality",
    "machine",
    "userland",
    "program",
    "tools",
    "args",
    "env",
    "cwd",
    "stdin",
    "reads",
    "writes",
    "present",
];

/// One fixed request (a `perl` run), with field `change` (an index into `FIELDS`) given another
/// value.
fn base(change: Option<usize>) -> SpawnRequest<'static> {
    let c = |field: usize| change == Some(field);
    SpawnRequest::new(
        id(if c(0) { 1 } else { 0 }),
        if c(1) { Personality::Linux } else { Personality::Xnu },
        if c(2) { Machine { cpus: 2, ..MACHINE_V1 } } else { MACHINE_V1 },
        id(if c(3) { 11 } else { 10 }),
        tool(if c(4) { "cc" } else { "perl" }),
        list([tool(if c(5) { "make" } else { "sh" })]),
        list([s(if c(6) { "-x" } else { "-e" })]),
        list([(s("CC"), s(if c(7) { "clang" } else { "cc" }))]),
        s(if c(8) { "/w/sub" } else { "/w" }),
        list(if c(9) { *b"b" } else { *b"a" }),
        list([Input::File { path: s("/r/a"), digest: id(if c(10) { 2 } else { 1 }) }]),
        list([s(if c(11) { "/out" } else { "/w" })]),
        list([(s("/w/a"), id(if c(12) { 4 } else { 3 }))]),
    )
    .expect("blaze-spawn tests: the base request is well formed")
}

/// A request around `env` and `writes`, every other field fixed.
fn request_with(
    env: AVec<'static, (AString<'static>, AString<'static>)>,
    writes: AVec<'static, AString<'static>>,
) -> Result<SpawnRequest<'static>, RequestError<'static>> {
    SpawnRequest::new(
        id(0),
        Personality::Xnu,
        MACHINE_V1,
        id(10),
        tool("perl"),
        list([tool("sh"), tool("cc")]),
        list([s("-e")]),
        env,
        s("/w"),
        AVec::new_in(g()),
        AVec::new_in(g()),
        writes,
        AVec::new_in(g()),
    )
}

/// A short text, non-ASCII and the separators among its letters.
fn draw_text(tape: &Tape) -> AString<'static> {
    const LETTERS: [char; 7] = ['a', 'z', '=', ' ', '/', 'é', '\u{1F600}'];
    let mut text = s("");
    for _ in 0..tape.int_in(0u32..=6, "text length") {
        text.push(LETTERS[tape.index(LETTERS.len(), "letter")]);
    }
    text
}

/// A drawn environment: unique names, any values, in a drawn order.
fn draw_env(tape: &Tape) -> AVec<'static, (AString<'static>, AString<'static>)> {
    let mut env = AVec::new_in(g());
    for n in 0..tape.int_in(0u32..=6, "env length") {
        env.push((named("V", n), draw_text(tape)));
    }
    if tape.coin("restate TZ") {
        env.push((s("TZ"), s("UTC0")));
    }
    tape.shuffle(&mut env, "env order");
    env
}

/// A drawn request: every list unique by its key and in a drawn order.
fn draw_request(tape: &Tape) -> SpawnRequest<'static> {
    let mut tools = AVec::new_in(g());
    for n in 0..tape.int_in(0u32..=4, "tools") {
        tools.push(tool(&named("t", n)));
    }
    tape.shuffle(&mut tools, "tools order");
    let mut args = AVec::new_in(g());
    for _ in 0..tape.int_in(0u32..=4, "args") {
        args.push(draw_text(tape));
    }
    let mut stdin = AVec::new_in(g());
    stdin.resize(tape.int_in(0usize..=16, "stdin length"), 0u8);
    tape.fill(&mut stdin, "stdin");
    let mut reads = AVec::new_in(g());
    for n in 0..tape.int_in(0u32..=4, "reads") {
        let (path, digest) = (named("/r/", n), id(tape.int::<u64>("read digest")));
        reads.push(if tape.coin("tree") { Input::Tree { path, digest } } else { Input::File { path, digest } });
    }
    tape.shuffle(&mut reads, "reads order");
    let mut writes = AVec::new_in(g());
    for n in 0..tape.int_in(0u32..=3, "writes") {
        writes.push(named("/w/", n));
    }
    tape.shuffle(&mut writes, "writes order");
    let mut present = AVec::new_in(g());
    for n in 0..tape.int_in(0u32..=3, "present") {
        present.push((named("/w/p", n), id(tape.int::<u64>("present digest"))));
    }
    tape.shuffle(&mut present, "present order");
    SpawnRequest::new(
        id(tape.int::<u64>("step")),
        if tape.coin("linux") { Personality::Linux } else { Personality::Xnu },
        Machine { cpus: tape.int_in(1u16..=64, "cpus"), page: 16384, memory: tape.int::<u64>("memory") },
        id(tape.int::<u64>("userland")),
        tool("perl"),
        tools,
        args,
        draw_env(tape),
        s("/w"),
        stdin,
        reads,
        writes,
        present,
    )
    .expect("blaze-spawn tests: a drawn request is well formed")
}

fn encoded_request(request: &SpawnRequest<'_>) -> AVec<'static, u8> {
    let mut out = AVec::new_in(g());
    request.encode(&mut out);
    out
}

fn encoded_answer(answer: &SpawnAnswer<'_>) -> AVec<'static, u8> {
    let mut out = AVec::new_in(g());
    answer.encode(&mut out);
    out
}

/// Prints `test tests::a_request_round_trips ... ok`. Any request through `new`, `encode`,
/// `decode`, `encode` gives equal bytes and an equal request; a decode that drops or reorders a
/// field fails it.
#[test]
fn a_request_round_trips() {
    check(Config::cases(64), |tape| {
        let request = draw_request(tape);
        let first = encoded_request(&request);
        let decoded = SpawnRequest::decode(&first, g()).expect("blaze-spawn: a canonical request decodes");
        assert_eq!(decoded, request, "the decoded request is the request");
        assert_eq!(encoded_request(&decoded), first, "the decoded request encodes to the same bytes");
    });
}

/// The first field whose change leaves `key` unchanged, or `None` when every field moves it.
fn missed(key: impl Fn(&SpawnRequest<'_>) -> ContentId) -> Option<&'static str> {
    let of_base = key(&base(None));
    (0..FIELDS.len()).find(|&field| key(&base(Some(field))) == of_base).map(|field| FIELDS[field])
}

/// Prints `test tests::the_key_covers_every_field ... ok`. Each of the 13 fields changed alone
/// gives another key (rule 21: a key covers every input). The control: a key function written
/// here that skips `stdin` is caught, naming `stdin`.
#[test]
fn the_key_covers_every_field() {
    assert_eq!(missed(the_key), None, "every field is in the key");
    assert_eq!(missed(planted_key_without_stdin), Some("stdin"), "the planted key that skips stdin is caught");
}

fn the_key(request: &SpawnRequest<'_>) -> ContentId {
    request.key(g())
}

/// PLANTED: a key function that skips `stdin`.
fn planted_key_without_stdin(request: &SpawnRequest<'_>) -> ContentId {
    let mut without_stdin = request.clone();
    without_stdin.stdin.clear();
    without_stdin.key(g())
}

/// Prints `test tests::env_order_does_not_change_the_key ... ok`. An environment given in any
/// order is one request with one key (rule 25: no key holds the caller's order).
#[test]
fn env_order_does_not_change_the_key() {
    check(Config::cases(64), |tape| {
        let env = draw_env(tape);
        let mut shuffled = env.clone();
        tape.shuffle(&mut shuffled, "shuffled env");
        let one = request_with(env, AVec::new_in(g())).expect("blaze-spawn: a drawn env is well formed");
        let other = request_with(shuffled, AVec::new_in(g())).expect("blaze-spawn: a drawn env is well formed");
        assert_eq!(one.key(g()), other.key(g()), "a shuffled env gives one key");
    });
}

/// Prints `test tests::fixed_env_is_always_there ... ok`. A request made with no environment
/// holds `LANG=C`, `LC_ALL=C`, `SOURCE_DATE_EPOCH=0` and `TZ=UTC0` (READINESS.md 1d); restating
/// one is allowed; `TZ=Europe/Berlin` is refused as `FixedEnv`.
#[test]
fn fixed_env_is_always_there() {
    let request = request_with(AVec::new_in(g()), AVec::new_in(g())).expect("blaze-spawn: an empty env is well formed");
    let held: std::vec::Vec<(&str, &str)> =
        request.env.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
    assert_eq!(held, fixed_env().to_vec(), "an empty env holds the four fixed entries, sorted");
    assert!(request_with(list([(s("TZ"), s("UTC0"))]), AVec::new_in(g())).is_ok(), "restating TZ=UTC0 is allowed");
    let berlin = request_with(list([(s("TZ"), s("Europe/Berlin"))]), AVec::new_in(g()));
    assert_eq!(berlin, Err(RequestError::FixedEnv { name: "TZ" }), "another time zone is refused");
}

/// Prints `test tests::an_unsorted_record_is_refused ... ok`. A record whose writable folders or
/// environment are out of order is refused, naming the field: a record is canonical or refused.
#[test]
fn an_unsorted_record_is_refused() {
    let request = request_with(AVec::new_in(g()), list([s("/b"), s("/a")])).expect("blaze-spawn: two folders");
    assert!(SpawnRequest::decode(&encoded_request(&request), g()).is_ok(), "the canonical record decodes");

    let mut writes_swapped = request.clone();
    writes_swapped.writes.swap(0, 1);
    assert_eq!(
        SpawnRequest::decode(&encoded_request(&writes_swapped), g()),
        Err(Malformed::Unsorted { field: "writes" }),
    );

    let mut env_reversed = request.clone();
    env_reversed.env.reverse();
    assert_eq!(
        SpawnRequest::decode(&encoded_request(&env_reversed), g()),
        Err(Malformed::Unsorted { field: "env" }),
    );
}

/// A model executor: replays one recorded answer for any request (rule 78's other strategy).
struct Scripted {
    answer: &'static [u8],
}

impl Executor for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn answer<'a>(
        &self,
        _request: &SpawnRequest<'_>,
        _files: &dyn Files,
        alloc: Alloc<'a>,
    ) -> Result<SpawnAnswer<'a>, SpawnRefused<'a>> {
        match SpawnAnswer::decode(self.answer, alloc) {
            Ok(answer) => Ok(answer),
            Err(malformed) => {
                let mut what = AString::from_str_in("its recorded answer is malformed: ", alloc);
                write!(what, "{malformed}").expect("blaze-spawn tests: writing into a String does not fail");
                Err(SpawnRefused::Refused { program: AString::from_str_in("scripted", alloc), what })
            }
        }
    }
}

/// The bytes of no file.
struct NoFiles;

impl Files for NoFiles {
    fn bytes(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

/// Prints `test tests::an_answer_round_trips_and_its_digest_ignores_order ... ok`. An answer made
/// from shuffled lists (and a repeated read-list row) has the digest of the one made in order;
/// it round-trips through its encoding, and through the scripted executor.
#[test]
fn an_answer_round_trips_and_its_digest_ignores_order() {
    check(Config::cases(64), |tape| {
        let mut written = AVec::new_in(g());
        for n in 0..tape.int_in(0u32..=4, "written") {
            let bytes = if tape.coin("removed") {
                None
            } else {
                let mut bytes = AVec::new_in(g());
                bytes.resize(tape.int_in(0usize..=8, "file length"), 0u8);
                tape.fill(&mut bytes, "file bytes");
                Some(bytes)
            };
            written.push(Written { path: named("/w/", n), bytes });
        }
        let mut seen = AVec::new_in(g());
        for n in 0..tape.int_in(0u32..=4, "seen") {
            let path = named("/r/", n / 2);
            seen.push(match tape.index(3, "kind") {
                0 => Seen::Read { path, digest: id(u64::from(n / 2)) },
                1 => Seen::Miss { path },
                _ => Seen::List { path, digest: id(100 + u64::from(n / 2)) },
            });
        }
        if let Some(row) = seen.first().cloned() {
            seen.push(row);
        }
        let status = if tape.coin("signal") { Status::Signal(tape.int("signal")) } else { Status::Code(tape.int("code")) };
        let stdout = list(*b"out");
        let stderr = list(*b"err");

        let mut written_shuffled = written.clone();
        tape.shuffle(&mut written_shuffled, "written order");
        let mut seen_shuffled = seen.clone();
        tape.shuffle(&mut seen_shuffled, "seen order");
        // Two rows of one path and kind with different digests are refused; the drawn rows can
        // hold such a pair, and then both orders must refuse it alike.
        let ordered = SpawnAnswer::new(status, stdout.clone(), stderr.clone(), written, seen);
        let shuffled = SpawnAnswer::new(status, stdout, stderr, written_shuffled, seen_shuffled);
        let (ordered, shuffled) = match (ordered, shuffled) {
            (Ok(ordered), Ok(shuffled)) => (ordered, shuffled),
            (Err(_), Err(_)) => return,
            (one, other) => panic!("one order refused and the other did not: {one:?} / {other:?}"),
        };
        assert_eq!(ordered, shuffled, "the answer does not hold its lists' order");
        assert_eq!(ordered.digest(g()), shuffled.digest(g()), "the digest does not hold its lists' order");

        let bytes = encoded_answer(&ordered);
        let decoded = SpawnAnswer::decode(&bytes, g()).expect("blaze-spawn: a canonical answer decodes");
        assert_eq!(decoded, ordered, "the decoded answer is the answer");
        assert_eq!(encoded_answer(&decoded), bytes, "the decoded answer encodes to the same bytes");

        let scripted = Scripted { answer: bytes.clone().leak_static() };
        let replayed = scripted.answer(&base(None), &NoFiles, g()).expect("blaze-spawn: the scripted executor answers");
        assert_eq!(replayed.digest(g()), ordered.digest(g()), "the scripted executor gives the recorded answer");
    });
}

/// Prints `test tests::off_refuses_naming_the_program ... ok`. With no sandbox, a `perl` spawn is
/// refused by name, saying why (D94), and is never run natively (rule 3).
#[test]
fn off_refuses_naming_the_program() {
    let off = Off { why: NoSandbox::OwnerHasNotAllowed };
    let refused = match off.answer(&base(None), &NoFiles, g()) {
        Ok(answer) => panic!("blaze-spawn: `Off` answered a spawn: {answer:?}"),
        Err(refused) => refused,
    };
    assert!(
        matches!(&refused, SpawnRefused::NoSandbox { program, why: NoSandbox::OwnerHasNotAllowed } if program == "perl"),
        "the refusal is NoSandbox for perl: {refused:?}"
    );
    let text = format!("{refused}");
    assert!(text.starts_with("perl"), "the refusal names the program first: {text}");
    assert!(text.contains("D94"), "the refusal says why: {text}");
}
