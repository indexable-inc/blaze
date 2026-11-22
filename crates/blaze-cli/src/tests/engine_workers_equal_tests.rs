//! ONE WORKER AND EIGHT GIVE EQUAL ANSWERS (rule 25; the property the old machine's request
//! tests held, ported to the engine on 2026-11-19, wave F6): the engine's check of a workspace
//! with three crates, a body a macro wrote, a type error and a borrow finding, run once with 1
//! worker and once with 8 on two fresh worlds of the same files: the printed lines (but the
//! closing one, which holds the time), the listed findings, the exit status, the tasks planned,
//! entered and put, the body notes, the digest of every record put (key, length and bytes, in
//! key order a phase: engine_store.rs `put_all`, `Ledger::digest`) and every question the stored
//! answer's rows answer (engine_answer_rows_tests.rs `ask`) are equal.
//!
//! CONTROL (rule 23): no plant reachable from the command's options orders by completion
//! (blaze-engine engine.rs `Plant` has none; its code pool's "publication in completion order"
//! is planted in code_pool_sim.rs, inside blaze-engine, and `engine_check::Opts` takes no engine
//! plant). So the control here is that the comparison sees a difference at all: the same
//! workspace with its type error fixed, on 1 worker, differs from both in its printed lines and
//! its records' digest; were the compared values constant, the equality above would prove
//! nothing.
//!
//! A child of main.rs's tests, beside engine_check_tests.rs, whose helpers it uses.

use super::*;
use super::engine_answer_rows_tests::{Asked, ask, universe, with_store};
use super::engine_check_tests::{engine_opts, error_lines, opts};

use blaze_answer::TreeDigest;

use crate::engine_answer_rows::PagesAnswer;

/// `a`'s root: `f` written by a macro, `one` by hand.
const A: &str = "macro_rules! write_f {\n    () => {\n        pub fn f(x: u8) -> u8 { x }\n    };\n}\nwrite_f!();\n\npub fn one() -> u8 {\n    1\n}\n";
/// `b`'s root: `h` calls both of `a`'s fns; `wrong` returns a `bool` where a `u8` is declared
/// (E0308, at line 6).
const B_WRONG: &str = "pub fn h() -> u8 {\n    a::one() + a::f(1)\n}\n\npub fn wrong() -> u8 {\n    true\n}\n";
/// [`B_WRONG`] fixed: the control's text.
const B_FIXED: &str = "pub fn h() -> u8 {\n    a::one() + a::f(1)\n}\n\npub fn wrong() -> u8 {\n    0\n}\n";
/// `c`'s root: `bad` takes a shared borrow of `v` while a mutable one is live and used after
/// (E0502, as engine_borrow_forms_tests.rs `SERDE_WRONG`'s visitor), and calls into `b`.
const C: &str = "pub fn keep(_: &u8) {}\n\npub fn bad() -> u8 {\n    let mut v = 1u8;\n    let a = &mut v;\n    let s = &v;\n    keep(s);\n    *a = 2;\n    b::h()\n}\n";

/// The workspace: `a`, `b` over `a`, `c` over `b`, `b`'s root as given.
fn workspace(io: &Io, b: &str) {
    write(io, "Cargo.toml", "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n");
    write(io, "a/Cargo.toml", "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n");
    write(io, "a/src/lib.rs", A);
    write(io, "b/Cargo.toml", "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n");
    write(io, "b/src/lib.rs", b);
    write(io, "c/Cargo.toml", "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nb = { path = \"../b\" }\n");
    write(io, "c/src/lib.rs", C);
}

/// Everything one check answered that must not depend on how many workers ran it.
#[derive(PartialEq, Eq, Debug)]
struct Answered {
    /// The printed lines, the closing one (its time and count entered) dropped.
    lines: std::vec::Vec<std::string::String>,
    /// Each listed finding's (code, id), in the order listed.
    ids: std::vec::Vec<(std::string::String, std::string::String)>,
    status: u8,
    /// Tasks planned, entered, and records put.
    counts: (u32, u32, u32),
    /// Each phase run, its tasks planned and entered.
    phases: std::vec::Vec<(blaze_engine::Phase, u32, u32)>,
    /// The body notes and the bodies checked clean.
    bodies: (std::vec::Vec<std::string::String>, u32),
    /// The digest of every record put: its key, length and bytes.
    records: blaze_ids::ContentId,
    /// Every question the stored answer's rows answer, serials zeroed.
    rows: Asked,
}

/// The tree digests the stored answer's certificate is asked for (any two distinct ones: the
/// forms are compared, not the trees).
const TREE: TreeDigest = TreeDigest([7; 32]);
const OTHER: TreeDigest = TreeDigest([8; 32]);

/// One fresh check of the workspace (`b`'s root as given) on `workers` workers, in a world of
/// its own, on the shelf whose core declares the borrow check's lang items (main.rs
/// `lang_shelf`), then its stored answer read back.
fn answered(workers: u32, b: &str) -> Answered {
    let io = Io::simulated(SimWorld::new(11, Limits::USUAL));
    workspace(&io, b);
    let mut o = opts(&io, workers, true);
    o.toolchain = lang_shelf(&io);
    let before = stdout(&io).len();
    let (ids, checked) = engine_opts(&io, o);
    let said = stdout(&io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = said.lines().map(str::to_owned).collect();
    lines.pop();
    let rows = with_store(&io, |cx, store, opts, _| {
        let frame = cx.frame();
        let name = crate::engine_answer::root_name(opts.place.checkout, frame);
        let pointer = crate::engine_answer::load_pointer(cx, store, &name).expect("the store reads").expect("engine-check stored an answer").expect("its pointer reads");
        let (kept, root) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("the store reads").expect("its pages read");
        let paged = PagesAnswer::new(&kept, root.bytes, pointer.facts, Some(TREE));
        let (names, cells, files) = universe(&kept.answer);
        ask(&paged, &names, &cells, &files, &TREE, &OTHER)
    });
    Answered {
        lines,
        ids,
        status: checked.status.0,
        counts: (checked.tasks, checked.entered, checked.puts),
        phases: checked.phases.iter().copied().collect(),
        bodies: (checked.bodies.0.iter().map(|note| note.to_string()).collect(), checked.bodies.1),
        records: checked.records.digest(),
        rows,
    }
}

#[test]
fn one_worker_and_eight_workers_give_equal_answers() {
    let one = answered(1, B_WRONG);
    let said = one.lines.join("\n");
    println!("1 worker: status {}, counts {:?}, ids {:?}, notes {:?}, records {:?}\n{said}", one.status, one.counts, one.ids, one.bodies.0, one.records);

    // The fixture holds what the property is about (rule 23: each check ran on its body).
    let errors = error_lines(&said);
    assert!(errors.iter().any(|line| line.contains("b/src/lib.rs:6:")), "`wrong`'s type error is printed at `true`'s line:\n{said}");
    assert!(one.ids.iter().any(|(code, _)| code == "borrows"), "`bad`'s borrow finding is listed: {:?}\n{said}", one.ids);
    assert!(one.bodies.0.iter().all(|note| !note.contains("not checked")), "no body is left unchecked, the macro's `f` among them: {:?}", one.bodies.0);
    assert_eq!(one.status, Status::FAILED.0, "a type error fails the command:\n{said}");
    assert!(!one.rows.rows.is_empty(), "the stored answer has rows to compare");

    let eight = answered(8, B_WRONG);
    assert_eq!(eight.lines, one.lines, "1 and 8 workers print the same lines");
    assert_eq!(eight.ids, one.ids, "1 and 8 workers list the same findings in the same order");
    assert_eq!((eight.status, eight.counts), (one.status, one.counts), "1 and 8 workers: the status, the tasks planned, entered and put");
    assert_eq!(eight.phases, one.phases, "1 and 8 workers run the same phases");
    assert_eq!(eight.bodies, one.bodies, "1 and 8 workers: the body notes and the clean count");
    assert_eq!(eight.records, one.records, "1 and 8 workers put the same records, key and bytes (rule 25)");
    assert_eq!(eight.rows, one.rows, "1 and 8 workers store the same answer rows");

    // CONTROL: the comparison sees a change: the type error fixed moves the lines and the records.
    let fixed = answered(1, B_FIXED);
    assert_ne!(fixed.lines, one.lines, "control: fixing the type error changes what is printed");
    assert_ne!(fixed.records, one.records, "control: fixing the type error changes the records put");
}
