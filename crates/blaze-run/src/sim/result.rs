//! The result record and the `Flaky` finding (docs/v2/plan/design/test.md, "End state", piece
//! 4; rules 25, 26 and 3): every value round-trips to equal bytes; every cut of a record is a
//! typed refusal; a failure is filed beside a pass at one address, both stay readable, and the
//! pair is exactly one `Flaky` finding, never a retry.
//!
//! Expected lines (`lanecheck.sh blaze-run test result`):
//! `test sim::result::every_result_round_trips ... ok`,
//! `test sim::result::every_cut_of_a_record_is_refused ... ok`,
//! `test sim::result::a_failure_beside_a_pass_is_one_flaky_finding ... ok`.

use blaze_own::Alloc;

use super::key;
use crate::plan::{Address, Failure, NotKept};
use crate::postings::FnName;
use crate::result::{
    AtAddress, Captured, Ended, FLAKY_RULE, FileError, Filing, Finding, Flaky, OUTPUT_CAP, Passed, Place, Refusal,
    ResultError, Slot, TEXT_CAP, Verdict, file, flaky, open,
};
use crate::{CheckoutKey, Nanos, TestId};

const TEST: TestId = TestId(key_const(11));
const ADDRESS: Address = Address(key_const(42));

/// `super::key` as a const: the first eight bytes are `n`.
const fn key_const(n: u8) -> [u8; 32] {
    let mut out = [0; 32];
    out[0] = n;
    out
}

fn pass(run: u64, kept: Result<(), NotKept>) -> Passed {
    Passed { run: CheckoutKey(key(run)), micros: 1_234, kept }
}

fn failed<'r>(run: u64, ended: Ended<'r>, output: Captured<'r>) -> Verdict<'r> {
    Verdict::Fail(crate::result::Failed { run: CheckoutKey(key(run)), micros: 9_876, ended, output })
}

/// Every pass (kept, and not kept for each reason), and every ending with and without a place,
/// with no output, a short output and an output over the cap.
fn table<'r>(short: &'r [u8], long: &'r [u8], refused: &'r str) -> Vec<Verdict<'r>> {
    let mut out = vec![Verdict::Pass(pass(1, Ok(())))];
    for why in [
        NotKept::Clock,
        NotKept::Network,
        NotKept::WroteOutside,
        NotKept::Foreign,
        NotKept::Unsandboxed,
        NotKept::Descriptor,
        NotKept::Threads,
        NotKept::Untraced,
    ] {
        out.push(Verdict::Pass(pass(2, Err(why))));
    }
    let here = Some(Place { item: FnName(key(7)), offset: 31 });
    let endings = [
        Ended::Panicked { message: "assertion failed: x == 1", at: here },
        Ended::Panicked { message: "", at: None },
        Ended::DidNotPanic,
        Ended::PanicTextMissing { message: "it went bang ü", at: here },
        Ended::PanicTextMissing { message: "bang", at: None },
        Ended::Signal { number: 11 },
        Ended::Signal { number: -6 },
        Ended::TimedOut { after: Nanos(60_000_000_123) },
        Ended::Refused { refusal: Refusal::Write, what: refused },
        Ended::Refused { refusal: Refusal::Connect, what: "203.0.113.9:443" },
        Ended::Refused { refusal: Refusal::Spawn, what: "/bin/sh" },
        Ended::Refused { refusal: Refusal::Call, what: "ptrace" },
        Ended::NoReport { code: 0, torn: 0 },
        Ended::NoReport { code: -1, torn: 37 },
    ];
    for ended in endings {
        out.push(failed(3, ended, Captured::NONE));
        out.push(failed(3, ended, Captured::of([5; 32], short)));
        out.push(failed(3, ended, Captured::of([6; 32], long)));
    }
    out
}

/// A record is read back as the value written, and writes the same bytes again (rule 26).
#[test]
fn every_result_round_trips() {
    let long = vec![b'y'; OUTPUT_CAP + 1_000];
    let refused = "/Users/someone/.ssh/known_hosts";
    for value in table(b"hello\n\xff", &long, refused) {
        let bytes = value.encode(Alloc::global()).expect("a table value encodes");
        let opened = open(bytes.as_slice()).expect("written bytes open");
        assert_eq!(opened, value);
        let again = opened.encode(Alloc::global()).expect("an opened value encodes");
        assert_eq!(again.as_slice(), bytes.as_slice());
    }
    // An output over the cap keeps its last bytes and says how many it cut.
    let captured = Captured::of([6; 32], &long);
    assert_eq!((captured.tail.len(), captured.cut()), (OUTPUT_CAP, 1_000));
    // A text over its cap is refused at write, by name (the driver files `capped(text)`).
    let huge = "z".repeat(TEXT_CAP + 1);
    let over = failed(3, Ended::Panicked { message: &huge, at: None }, Captured::NONE);
    assert_eq!(
        over.encode(Alloc::global()).map(|b| b.len()),
        Err(ResultError::TooLong { field: crate::result::Capped::Text, len: TEXT_CAP + 1, cap: TEXT_CAP })
    );
    assert_eq!(crate::result::capped(&huge).len(), TEXT_CAP);
}

/// Every prefix of every record is refused as too short or of the wrong length, never read as
/// another value; a byte its form does not allow is refused by its place.
#[test]
fn every_cut_of_a_record_is_refused() {
    let long = vec![b'y'; OUTPUT_CAP + 10];
    for value in table(b"out", &long, "/etc/passwd") {
        let bytes = value.encode(Alloc::global()).expect("a table value encodes");
        let bytes = bytes.as_slice();
        for len in 0..bytes.len() {
            let refused = open(&bytes[..len]);
            assert!(
                matches!(refused, Err(ResultError::Short { .. } | ResultError::Length { .. })),
                "a record cut to {len} of {} bytes gave {refused:?}",
                bytes.len()
            );
        }
        // One more byte is the wrong length too.
        let mut more = bytes.to_vec();
        more.push(0);
        assert!(matches!(open(&more), Err(ResultError::Length { .. })));
    }
    // Padding that is not zero, an unknown kind, an unknown reason: each named by its byte.
    let pass = Verdict::Pass(pass(1, Ok(()))).encode(Alloc::global()).expect("a pass encodes");
    let mut bad = pass.as_slice().to_vec();
    bad[60] = 1;
    assert_eq!(open(&bad), Err(ResultError::NotZero { at: 60 }));
    let mut bad = pass.as_slice().to_vec();
    bad[8] = 2;
    assert_eq!(open(&bad), Err(ResultError::Tag { at: 8, byte: 2 }));
    let mut bad = pass.as_slice().to_vec();
    // 9: one past the last reason (`NotKept::Untraced` is 8).
    bad[56] = 9;
    assert_eq!(open(&bad), Err(ResultError::Tag { at: 56, byte: 9 }));
    // A signal number on a panic is a field its ending does not use.
    let panic = failed(3, Ended::DidNotPanic, Captured::NONE).encode(Alloc::global()).expect("a failure encodes");
    let mut bad = panic.as_slice().to_vec();
    bad[60] = 9;
    assert_eq!(open(&bad), Err(ResultError::NotZero { at: 60 }));
}

/// An address's two slots, as the store holds them: each written once.
#[derive(Default)]
struct Store {
    pass: Option<Vec<u8>>,
    failure: Option<Vec<u8>>,
}

impl Store {
    fn at(&self) -> AtAddress<'_> {
        let pass = self.pass.as_deref().map(|bytes| match open(bytes) {
            Ok(Verdict::Pass(pass)) => pass,
            other => panic!("the pass slot holds {other:?}"),
        });
        let failure = self.failure.as_deref().map(|bytes| match open(bytes) {
            Ok(Verdict::Fail(failure)) => failure,
            other => panic!("the failure slot holds {other:?}"),
        });
        AtAddress { pass, failure }
    }

    /// Files `new` as the driver does; the finding, when there is one.
    fn file(&mut self, new: &Verdict<'_>) -> Option<Finding> {
        let filed = file(TEST, ADDRESS, &self.at(), new).expect("a kept pass or a failure files");
        let bytes = new.encode(Alloc::global()).expect("encodes").as_slice().to_vec();
        match filed.filing {
            Filing::Write(Slot::Pass) => {
                assert!(self.pass.is_none(), "a pass written over a pass");
                self.pass = Some(bytes);
            }
            Filing::Write(Slot::Failure) => {
                assert!(self.failure.is_none(), "a failure written over a failure");
                self.failure = Some(bytes);
            }
            Filing::Held(_) => {}
        }
        filed.flaky
    }
}

/// test.md: "a failure is filed beside the pass at its address, and both at one address is a
/// `Flaky` finding naming the two runs"; rule 3: nothing is retried.
#[test]
fn a_failure_beside_a_pass_is_one_flaky_finding() {
    let passed = pass(1, Ok(()));
    let first = failed(2, Ended::Panicked { message: "boom", at: None }, Captured::of([1; 32], b"first\n"));
    let second = failed(3, Ended::Signal { number: 11 }, Captured::of([2; 32], b"second\n"));
    let expected = Finding {
        rule: FLAKY_RULE,
        start: 0,
        end: 0,
        flaky: Flaky {
            test: TEST,
            address: ADDRESS,
            passed: CheckoutKey(key(1)),
            failed: CheckoutKey(key(2)),
            why: Failure::Panicked,
        },
    };

    // A pass, then a failure: both readable, one finding; a second failure adds none and
    // leaves the first in its slot; a second pass adds none.
    let mut store = Store::default();
    let mut found = Vec::new();
    for value in [Verdict::Pass(passed), first, second, Verdict::Pass(passed)] {
        found.extend(store.file(&value));
    }
    assert_eq!(found, vec![expected]);
    assert_eq!(open(store.pass.as_deref().expect("the pass slot")), Ok(Verdict::Pass(passed)));
    assert_eq!(open(store.failure.as_deref().expect("the failure slot")), Ok(first));
    assert_eq!(found[0], flaky(TEST, ADDRESS, &passed, &match first {
        Verdict::Fail(f) => f,
        Verdict::Pass(_) => unreachable!("`first` is a failure"),
    }));

    // A failure, then a pass: the same one finding.
    let mut store = Store::default();
    let mut found = Vec::new();
    for value in [first, Verdict::Pass(passed), second] {
        found.extend(store.file(&value));
    }
    assert_eq!(found, vec![expected]);

    // A pass not kept is never filed.
    let unkept = Verdict::Pass(pass(1, Err(NotKept::Clock)));
    assert_eq!(
        file(TEST, ADDRESS, &AtAddress::EMPTY, &unkept),
        Err(FileError::NotKept { test: TEST, why: NotKept::Clock })
    );

    // Its words, exactly.
    let mut words = String::new();
    expected.message(&mut words).expect("a String takes the words");
    assert_eq!(
        words,
        "test 0b00000000000000 passed and failed at address 2a00000000000000: it passed on checkout \
         0100000000000000 and panicked on checkout 0200000000000000, so its result rests on something its \
         asks do not name"
    );
}
