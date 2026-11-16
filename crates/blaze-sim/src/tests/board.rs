//! The case board (board.rs, test.md "The case board"; piece 1): what the runner and the child
//! write reads back as written, and a cut, foreign or torn board is refused by name, never read.
//!
//! Each test runs through `blaze_sim::check` or `control`; `cargo test -p blaze-sim board`
//! prints, for the two checks (n is the choices the cases drew):
//!
//! ```text
//! blaze-sim: tests::board::board_round_trips: 256 cases, <n> choices
//! blaze-sim: tests::board::a_cut_board_is_refused: 256 cases, <n> choices
//! ```
//!
//! and `a_torn_board_is_refused`, a control, prints nothing when it passes: its clean run passes
//! every case, and its plant (`Plant::OpensTorn`, a reader that takes "writing the case's tape"
//! as running) is caught saying "a torn board opened".

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use core::fmt::Debug;

use crate::board::{
    Board, BoardError, Budget, Bytes, CaseOrigin, Counts, Declared, End, Ending, Page, PageError,
    Plant, Progress, Seeds, Shape, Spec, Tally, TailWord, TEXT_CAP, Writer, encode,
};
use crate::tape::{Choice, Record, Span};
use crate::{Broke, Config, Tape, check, control};

/// A plan drawn from a tape, its tapes owned.
struct Drawn {
    target_check: u32,
    given: Vec<Vec<u64>>,
    seeds: Seeds,
    budget: Budget,
    record_cap: u32,
    coverage_bytes: u32,
    /// Bytes past the board's end on the page: a page set is rounded up to pages.
    slack: usize,
}

impl Drawn {
    fn draw(tape: &Tape) -> Drawn {
        let record_cap = tape.int_in(1..=40u32, "record cap");
        let given = tape.vec(0..=4, "given", |t| {
            t.vec(0..=to_usize(record_cap), "tape", |v| v.int::<u64>("value"))
        });
        let seeds = if tape.coin("range") {
            Seeds::Range { first: tape.int("first"), count: tape.int("count") }
        } else {
            Seeds::Declared
        };
        Drawn {
            target_check: tape.int_in(1..=3u32, "target"),
            given,
            seeds,
            budget: Budget {
                most_choices: tape.int("most choices"),
                deadline_ms: tape.int("deadline"),
                instructions: tape.int("instructions"),
            },
            record_cap,
            coverage_bytes: 8 * tape.int_in(0..=4u32, "coverage words"),
            slack: to_usize(tape.int_in(0..=64u32, "slack")),
        }
    }

    fn spec<'s>(&self, given: &'s [&'s [u64]]) -> Spec<'s> {
        Spec {
            target_check: self.target_check,
            given,
            seeds: self.seeds,
            budget: self.budget,
            record_cap: self.record_cap,
            coverage_bytes: self.coverage_bytes,
        }
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("a u32 fits a usize")
}

fn oracle(why: String) -> Broke {
    Broke::Oracle(why)
}

fn harness(error: BoardError) -> Broke {
    Broke::Harness(format!("{error}"))
}

fn same<T: PartialEq + Debug>(what: &str, read: &T, wrote: &T) -> Result<(), Broke> {
    if read == wrote { Ok(()) } else { Err(oracle(format!("{what}: read {read:?}, wrote {wrote:?}"))) }
}

/// A fresh board of a drawn plan on a page with slack, and its plan's tapes.
fn fresh(tape: &Tape) -> Result<(Vec<u8>, Shape, Vec<Vec<u64>>), Broke> {
    let drawn = Drawn::draw(tape);
    let given: Vec<&[u64]> = drawn.given.iter().map(Vec::as_slice).collect();
    let spec = drawn.spec(&given);
    let shape = Shape::of(&spec).map_err(harness)?;
    let mut page = vec![0xA5u8; shape.end() + drawn.slack];
    let encoded = encode(&spec, &mut page).map_err(harness)?;
    same("the shape `encode` gave", &encoded, &shape)?;
    Ok((page, shape, drawn.given))
}

/// The child's first step, drawn.
fn begin(tape: &Tape, shape: Shape, page: &mut [u8]) -> Result<Counts, Broke> {
    let check = tape.int_in(1..=3u32, "check");
    let given_run = if check == shape.target_check { shape.given_tapes } else { 0 };
    let declared = Declared {
        first: tape.int("declared first"),
        cases: tape.int("declared cases"),
        shrink_runs: tape.int("declared shrink runs"),
        most_choices: tape.int("most choices"),
    };
    Writer::new(Bytes(page), shape).begin(check, given_run, declared).map_err(harness)?;
    Ok(Counts { check, given_run, declared, tally: Tally::ZERO })
}

fn tally(tape: &Tape) -> Tally {
    Tally { passed: tape.int("passed"), discarded: tape.int("discarded"), choices: tape.int("choices") }
}

/// A case's start, drawn: its number, its tail and its prefix.
fn case(tape: &Tape, shape: Shape) -> (u32, TailWord, Vec<u64>) {
    let case = tape.int_in(1..=1000u32, "case");
    let tail = if tape.coin("seeded") { TailWord::Seeded(tape.int("seed")) } else { TailWord::Calm };
    let prefix = tape.vec(0..=to_usize(shape.record_cap), "prefix", |t| t.int::<u64>("value"));
    (case, tail, prefix)
}

/// A record that may pass the board's capacity, in values and in spans.
fn record(tape: &Tape, cap: u32) -> Record {
    let mut record = Record::new();
    let len = tape.below(2 * u64::from(cap) + 1, "record length");
    for _ in 0..len {
        record.choices.push(Choice { value: tape.int("value"), most: u64::MAX, label: "value" });
    }
    for _ in 0..tape.below(u64::from(cap) + 2, "spans") {
        let (a, b) = (tape.below(len + 1, "start"), tape.below(len + 1, "end"));
        let (start, end) = (a.min(b), a.max(b));
        record.spans.push(Span {
            label: "span",
            start: u32::try_from(start).expect("a record's length is a u32"),
            end: u32::try_from(end).expect("a record's length is a u32"),
        });
    }
    record
}

/// The texts a failure may say: none, short, and one past the cap whose cap falls inside a
/// three-byte character.
fn text(tape: &Tape) -> String {
    match tape.below(3, "text") {
        0 => String::new(),
        1 => "the oracle: x broke".to_owned(),
        _ => "€".repeat(1400),
    }
}

/// `text` cut as the board cuts it: at the cap, back to a character boundary.
fn cut_text(text: &str) -> (&str, bool) {
    let mut take = text.len().min(TEXT_CAP);
    while !text.is_char_boundary(take) {
        take -= 1;
    }
    (&text[..take], take < text.len())
}

fn round_trips(tape: &Tape) -> Result<(), Broke> {
    let (mut page, shape, given) = fresh(tape)?;
    let board = Board::open(&page).map_err(|error| oracle(format!("a fresh board did not open: {error}")))?;
    same("the shape", &board.shape(), &shape)?;
    let tapes: Vec<Vec<u64>> = board.plan().tapes().map(|t| t.values().collect()).collect();
    same("the given tapes", &tapes, &given)?;
    same("a fresh board's progress", &board.progress(), &Progress::Untouched)?;

    let begun = begin(tape, shape, &mut page)?;
    let board = Board::open(&page).map_err(|error| oracle(format!("a begun board did not open: {error}")))?;
    same("a begun board's progress", &board.progress(), &Progress::Begun(begun))?;

    let before = tally(tape);
    let (case, tail, prefix) = case(tape, shape);
    Writer::new(Bytes(&mut page), shape).start_case(case, before, tail, &prefix).map_err(harness)?;
    let board = Board::open(&page).map_err(|error| oracle(format!("a running board did not open: {error}")))?;
    let counts = Counts { tally: before, ..begun };
    let Progress::Running { counts: read, case: read_case, tape: read_tape } = board.progress() else {
        return Err(oracle(format!("a started case read as {:?}", board.progress())));
    };
    same("the running counts", &read, &counts)?;
    same("the running case", &read_case, &case)?;
    same("the running tail", &read_tape.tail, &tail)?;
    same("the running prefix", &read_tape.prefix().collect::<Vec<u64>>(), &prefix)?;

    let failed = record(tape, shape.record_cap);
    let said = text(tape);
    let origin = *tape
        .pick(&[CaseOrigin::Oracle, CaseOrigin::Harness, CaseOrigin::Error, CaseOrigin::Overran], "origin")
        .expect("four origins");
    let after = tally(tape);
    Writer::new(Bytes(&mut page), shape)
        .end(case, after, Ending::Failed { origin, record: &failed, text: &said })
        .map_err(harness)?;
    let board = Board::open(&page).map_err(|error| oracle(format!("an ended board did not open: {error}")))?;
    let Progress::Ended { counts: read, case: read_case, tape: read_tape, end } = board.progress() else {
        return Err(oracle(format!("an ended run read as {:?}", board.progress())));
    };
    same("the ended counts", &read, &Counts { tally: after, ..begun })?;
    same("the ended case", &read_case, &case)?;
    same("the last case's tail", &read_tape.tail, &tail)?;
    let End::Failed { origin: read_origin, record: read_record, text: read_text, text_cut } = end else {
        return Err(oracle(format!("a failed end read as {end:?}")));
    };
    let cap = to_usize(shape.record_cap);
    let kept = failed.choices.len().min(cap);
    let values: Vec<u64> = failed.choices[..kept].iter().map(|choice| choice.value).collect();
    let spans: Vec<(u32, u32)> = failed
        .spans
        .iter()
        .filter(|span| to_usize(span.end) <= kept)
        .take(cap)
        .map(|span| (span.start, span.end))
        .collect();
    let (want_text, want_cut) = cut_text(&said);
    same("the origin", &read_origin, &origin)?;
    same("the record's values", &read_record.values().collect::<Vec<u64>>(), &values)?;
    same("the record's spans", &read_record.spans().collect::<Vec<(u32, u32)>>(), &spans)?;
    same("whether the record was cut", &read_record.cut, &(kept < failed.choices.len() || spans.len() < failed.spans.len()))?;
    same("the text", &read_text, &want_text)?;
    same("whether the text was cut", &text_cut, &want_cut)?;

    let bits: Vec<u8> = (0..shape.coverage_bytes).map(|_| tape.int::<u8>("coverage")).collect();
    Writer::new(Bytes(&mut page), shape).coverage(&bits).map_err(harness)?;
    let board = Board::open(&page).map_err(|error| oracle(format!("a covered board did not open: {error}")))?;
    same("the coverage", &board.coverage(), &bits.as_slice())
}

/// What the runner writes and the child writes reads back as written: the shape, the given
/// tapes, a fresh, begun, running and failed progress (a record past the capacity cut to it with
/// the spans inside it, a text past its cap cut on a character boundary), and the coverage.
#[test]
fn board_round_trips() {
    check(Config::cases(256), round_trips);
}

/// A board cut anywhere short of its length, its header included, is refused as short; a
/// changed magic is foreign; another version is refused naming it.
#[test]
fn a_cut_board_is_refused() {
    check(Config::cases(256), |tape: &Tape| -> Result<(), Broke> {
        let (mut page, shape, _given) = fresh(tape)?;
        begin(tape, shape, &mut page)?;
        let (case, tail, prefix) = case(tape, shape);
        Writer::new(Bytes(&mut page), shape).start_case(case, tally(tape), tail, &prefix).map_err(harness)?;
        let end = shape.end();
        let cut = usize::try_from(tape.below(u64::try_from(end).expect("a board's length"), "cut"))
            .expect("below a usize");
        match Board::open(&page[..cut]) {
            Err(BoardError::Short { .. }) => {}
            other => return Err(oracle(format!("a board cut to {cut} of its {end} bytes opened as {other:?}"))),
        }
        let mut foreign = page.clone();
        foreign[0] = b'X';
        match Board::open(&foreign) {
            Err(BoardError::Foreign { magic: [b'X', b'Z', b'S', b'B'] }) => {}
            other => return Err(oracle(format!("a board starting `XZSB` opened as {other:?}"))),
        }
        let mut later = page;
        later[4] = 2;
        match Board::open(&later) {
            Err(BoardError::Version { found: 2 }) => Ok(()),
            other => Err(oracle(format!("a board of version 2 opened as {other:?}"))),
        }
    });
}

/// A page that takes `left` writes and refuses the rest: a child killed between two writes.
struct Dying<'p> {
    page: Bytes<'p>,
    left: u32,
    took: u32,
}

impl Page for Dying<'_> {
    fn put(&mut self, at: usize, bytes: &[u8]) -> Result<(), PageError> {
        if self.left == 0 {
            return Err(PageError { at, len: bytes.len(), os: None });
        }
        self.left -= 1;
        self.took += 1;
        self.page.put(at, bytes)
    }
}

/// A case started on a begun board, the child killed after some of its writes: whole (none, or
/// all) opens; anything between is refused as torn, naming the case.
fn opens_only_whole(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let (mut page, shape, _given) = fresh(tape)?;
    begin(tape, shape, &mut page)?;
    let before = tally(tape);
    let (case, tail, prefix) = case(tape, shape);
    let mut copy = page.clone();
    let mut counting = Writer::new(Dying { page: Bytes(&mut copy), left: u32::MAX, took: 0 }, shape);
    counting.start_case(case, before, tail, &prefix).map_err(harness)?;
    let total = counting.into_page().took;
    let left = u32::try_from(tape.below(u64::from(total) + 1, "writes before the kill"))
        .expect("at most the writes");
    let mut dying = Writer::new(Dying { page: Bytes(&mut page), left, took: 0 }, shape);
    let started = dying.start_case(case, before, tail, &prefix);
    if left < total && started.is_ok() {
        return Err(Broke::Harness(format!("a page that took {left} of {total} writes said all went")));
    }
    let whole = left == 0 || left == total;
    match (whole, Board::open_planted(&page, plant)) {
        (true, Ok(_)) => Ok(()),
        (false, Err(BoardError::Torn { case: torn, .. })) if torn == case => Ok(()),
        (false, Ok(board)) => Err(oracle(format!(
            "a torn board opened: killed after {left} of {total} writes, it read as {:?}",
            board.progress()
        ))),
        (_, other) => Err(Broke::Harness(format!(
            "killed after {left} of {total} writes, the board opened as {other:?}"
        ))),
    }
}

#[test]
fn a_torn_board_is_refused() {
    control(Config::cases(256), Plant::None, Plant::OpensTorn, "a torn board opened", opens_only_whole);
}
