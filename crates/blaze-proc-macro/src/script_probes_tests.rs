//! The probe checker's seam (script_probes.rs) driven through `ScriptSandbox::spawn`, the one
//! host call a wasm build script's spawn makes, with the core's fake checker: the real line of
//! bigdecimal's autocfg probe that stopped turso's scripts on 2026-11-21 (`CannotAnswerYet`,
//! reports/turso/c1647) answered by the checker and kept once; a rejection is exit 1 with its
//! first error; a checker that cannot say stops the script by name and keeps nothing.

use core::cell::Cell;
use std::sync::{Arc, Mutex};

use blaze_build_script::key::CMode;
use blaze_build_script::probe_check::fake::{FakeChecker, FakeRow, FakeVerdict};
use blaze_build_script::probe_check::{CheckerId, ProbeAnswer, ProbeChecker, Question};
use blaze_ids::ContentId;
use blaze_own::Alloc;

use crate::script_probes::{MemProbeStore, ScriptProbes};
use crate::script_sandbox::{Refusal, Script, ScriptSandbox, SpawnRefusal};
use crate::spawn_route::SpawnSetting;
use crate::spawn_route_tests::{script, shim};

const HOST: &str = "aarch64-apple-darwin";

/// bigdecimal's autocfg `probe_expression("0i64.unsigned_abs()")`, as turso's check printed it.
const BIGDECIMAL: &[&str] = &[
    "rustc",
    "--crate-name",
    "autocfg_dca8b2ac880013e3_1",
    "--crate-type=lib",
    "--out-dir",
    "/out",
    "--emit=llvm-ir",
    "--target",
    "aarch64-apple-darwin",
    "-",
];
const BIGDECIMAL_SNIPPET: &[u8] = b"pub fn probe() { let _ = 0i64.unsigned_abs(); }";
const REMOVED_SNIPPET: &[u8] = b"#![feature(box_syntax)]";
const REMOVED_ERROR: &str = "error[E0557]: feature has been removed";
/// A snippet no row answers: the fake's `Incomplete`.
const UNKNOWN_SNIPPET: &[u8] = b"pub fn probe() { let _ = 0i64.isqrt(); }";

static ROWS: [FakeRow<'static>; 2] = [
    FakeRow { source: BIGDECIMAL_SNIPPET, target: HOST, verdict: FakeVerdict::Accepted },
    FakeRow { source: REMOVED_SNIPPET, target: HOST, verdict: FakeVerdict::Rejected(REMOVED_ERROR) },
];

/// The core's fake behind a lock: a command's checker is shared by its runs (`Sync`).
struct Shared(Mutex<FakeChecker<'static>>);

impl Shared {
    fn calls(&self) -> u32 {
        crate::host::lock(&self.0).calls.get()
    }
}

impl ProbeChecker for Shared {
    fn id(&self) -> CheckerId {
        crate::host::lock(&self.0).id
    }

    fn check<'a>(&self, question: &Question<'_, '_>, alloc: Alloc<'a>) -> ProbeAnswer<'a> {
        crate::host::lock(&self.0).check(question, alloc)
    }
}

/// A script whose probes go to `checker` and `store` (`None`: the checker is off).
fn probing(probes: Option<(&Arc<Shared>, &Arc<MemProbeStore>)>) -> Arc<Script> {
    let base = script(CMode::Deferred, None, SpawnSetting::off());
    let probes = probes.map(|(checker, store)| ScriptProbes { checker: Arc::clone(checker) as _, records: Arc::clone(store) as _ });
    Arc::new(Script { probes, ..Script::clone(&base) })
}

/// The spawn of `argv` with `stdin`: its exit code and standard error, or the script's stop.
fn spawn(script: &Arc<Script>, argv: &[&str], stdin: &[u8]) -> Result<(i32, String), (String, Option<Refusal>)> {
    let mut sandbox = ScriptSandbox::new(Arc::clone(script));
    match sandbox.spawn(&shim(argv, false, &[], None, stdin, [2, 2])) {
        Ok(()) => {
            let reply = &sandbox.reply;
            let code = i32::from_le_bytes(reply[..4].try_into().expect("the reply's code"));
            let out_len = u32::from_le_bytes(reply[4..8].try_into().expect("stdout's length")) as usize;
            let err_at = 8 + out_len;
            let err_len = u32::from_le_bytes(reply[err_at..err_at + 4].try_into().expect("stderr's length")) as usize;
            Ok((code, String::from_utf8_lossy(&reply[err_at + 4..err_at + 4 + err_len]).into_owned()))
        }
        Err(stop) => Err((stop.0, sandbox.refused.take())),
    }
}

/// The real case: bigdecimal's probe, with the checker on, is answered by it (exit 0) and its
/// answer kept once: a second script asking it reads the record, and the checker is not asked
/// again (rule 77). A removed feature's probe is exit 1 with the checker's first error. The
/// twin: a snippet the checker cannot take to its end stops the script naming the probe
/// (`ProbeUnknown`), and nothing is kept. CONTROL (rule 23): the same probe with the checker
/// off is the pinned table's, which (empty here) cannot answer it: the checker is what answered.
#[test]
fn a_probe_with_the_checker_on_is_answered_by_it_and_kept_once() {
    let checker = Arc::new(Shared(Mutex::new(FakeChecker { id: CheckerId(ContentId([9; 32])), rows: &ROWS, calls: Cell::new(0) })));
    let store = Arc::new(MemProbeStore::default());
    let on = probing(Some((&checker, &store)));

    assert_eq!(spawn(&on, BIGDECIMAL, BIGDECIMAL_SNIPPET).map(|(code, _)| code), Ok(0), "accepted: exit 0");
    assert_eq!((checker.calls(), store.len()), (1, 1), "asked once, kept once");
    assert_eq!(spawn(&on, BIGDECIMAL, BIGDECIMAL_SNIPPET).map(|(code, _)| code), Ok(0), "the next script reads the record");
    assert_eq!((checker.calls(), store.len()), (1, 1), "the record answered: the checker was not asked again");

    let rejected = spawn(&on, BIGDECIMAL, REMOVED_SNIPPET);
    assert!(rejected.as_ref().is_ok_and(|(code, stderr)| *code == 1 && stderr.contains("E0557")), "{rejected:?}");

    let kept = store.len();
    match spawn(&on, BIGDECIMAL, UNKNOWN_SNIPPET) {
        Err((words, Some(Refusal::Spawn(SpawnRefusal::ProbeUnknown { .. })))) => assert!(words.contains("cannot answer"), "{words}"),
        other => panic!("a probe the checker cannot say stops the script by name: {other:?}"),
    }
    assert_eq!(store.len(), kept, "an unknown is never kept");

    let off = probing(None);
    match spawn(&off, BIGDECIMAL, BIGDECIMAL_SNIPPET) {
        Err((_, Some(Refusal::Spawn(SpawnRefusal::CannotAnswerYet { .. })))) => {}
        other => panic!("CONTROL: with the checker off the empty table cannot answer: {other:?}"),
    }
}

/// A line the checker does not model is refused naming its argument, before any question is
/// asked: `-Z` other than `unstable-options` (`probe_check::Why::Unstable`).
#[test]
fn a_rustc_line_the_checker_does_not_model_is_refused_by_its_argument() {
    let checker = Arc::new(Shared(Mutex::new(FakeChecker { id: CheckerId(ContentId([9; 32])), rows: &ROWS, calls: Cell::new(0) })));
    let store = Arc::new(MemProbeStore::default());
    let on = probing(Some((&checker, &store)));
    let mut argv = BIGDECIMAL.to_vec();
    argv.insert(1, "-Zthreads=8");
    match spawn(&on, &argv, BIGDECIMAL_SNIPPET) {
        Err((words, Some(Refusal::Spawn(SpawnRefusal::ProbeLine { why: blaze_build_script::probe_check::Why::Unstable, .. })))) => assert!(words.contains("-Zthreads=8"), "{words}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(checker.calls(), 0, "no question was asked");
}
