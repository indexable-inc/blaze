use blaze_protocol::now::{
    Blocker, Busy, Download, Expansion, Memory, Now, Phase, Running, Span, Tally, Task, Took,
};
use expect_test::expect_file;
use ratatui::{buffer::Buffer, layout::Rect};

use super::{Board, Form, INLINE_ROWS, Key};
use crate::test_view::Paint;

const GIB: u64 = 1 << 30;

fn task(id: u64, kind: &str, name: &str) -> Task {
    Task { id, kind: kind.to_owned(), name: name.to_owned() }
}

/// Andrew's `blaze test` of 2026-11-05: 15 of 16 workers idle while one spent four and a half
/// minutes in codegen of `core::ptr::is_freeze`, with 82 tasks behind it; 99 GiB held, no
/// limit.
fn andrew() -> Now {
    let span = Span {
        name: "is_freeze".to_owned(),
        item: Some("core::ptr::is_freeze".to_owned()),
        krate: Some("core".to_owned()),
        awaits: None,
        for_ms: 270_000,
    };
    Now {
        at_ms: 914_000,
        phase: Phase::Compiling,
        stage: String::new(),
        stall: None,
        tally: Tally { waiting: 80, ready: 0, running: 3, done: 487, failed: 0, total: 570 },
        workers: 16,
        busy: vec![Busy {
            thread: "blaze-worker-3".to_owned(),
            task: Some(task(7, "codegen", "core")),
            span: span.clone(),
            path: vec!["task".to_owned(), "codegen".to_owned(), "copy_prop".to_owned()],
        }],
        recent: 0,
        running: vec![
            Running {
                task: task(7, "codegen", "core 0.0.0 lib (host)"),
                for_ms: 270_000,
                thread: Some("blaze-worker-3".to_owned()),
                span: Some(span.clone()),
                path: vec!["copy_prop".to_owned()],
            },
            Running {
                task: task(9, "test", "blaze-ui 0.1.0 lib test (host)"),
                for_ms: 121_000,
                thread: Some("blaze-worker-5".to_owned()),
                span: Some(Span {
                    name: "test run".to_owned(),
                    item: None,
                    krate: None,
                    awaits: Some("the test process".to_owned()),
                    for_ms: 121_000,
                }),
                path: Vec::new(),
            },
            Running {
                task: task(11, "script-run", "ring 0.17.8 build-script (host)"),
                for_ms: 31_000,
                thread: None,
                span: None,
                path: Vec::new(),
            },
        ],
        failures: Vec::new(),
        expanding: Expansion {
            threads: 32,
            busy: vec![Busy {
                thread: "blaze wasm expansion".to_owned(),
                task: None,
                span: Span {
                    name: "proc macro expansion".to_owned(),
                    item: Some("serde_derive::Serialize".to_owned()),
                    krate: Some("blaze-api".to_owned()),
                    awaits: None,
                    for_ms: 1_200,
                },
                path: Vec::new(),
            }],
        },
        blockers: vec![Blocker {
            task: task(7, "codegen", "core"),
            running: true,
            for_ms: 270_000,
            span: Some(span),
            holds: 82,
            chain: 41,
            behind: vec![
                "std build script".to_owned(),
                "libc build script".to_owned(),
                "alloc lib".to_owned(),
            ],
        }],
        downloads: Vec::new(),
        slowest: vec![
            Took { task: task(3, "bodies", "hir-ty"), ms: 72_000 },
            Took { task: task(4, "codegen", "syntax"), ms: 48_000 },
            Took { task: task(5, "script-run", "ring"), ms: 31_000 },
        ],
        quiet_ms: 270_000,
        memory: Memory {
            held: 99 * GIB,
            peak: 99 * GIB,
            limit: 0,
            over: None,
            held_back: 0,
            releases: 0,
        },
    }
}

/// Andrew's build under a 24 GiB ceiling: 99 GiB is over it.
fn over() -> Now {
    let mut now = andrew();
    now.memory.limit = 24 * GIB;
    now
}

/// Andrew's build under a 24 GiB ceiling at 21.5 GiB, MIR over its share and three tasks held
/// back.
fn squeezed() -> Now {
    let mut now = andrew();
    now.memory = Memory {
        held: 43 * GIB / 2,
        peak: 45 * GIB / 2,
        limit: 24 * GIB,
        over: Some("mir".to_owned()),
        held_back: 3,
        releases: 2,
    };
    now
}

/// A cold build fetching LLVM before it has a task.
fn cold() -> Now {
    Now {
        at_ms: 62_000,
        phase: Phase::Fetching,
        stage: "waits for its pinned native inputs, 1 of 1 layouts left".to_owned(),
        workers: 16,
        downloads: vec![Download {
            what: "llvm-21.1.8-aarch64-apple-darwin.tar.xz".to_owned(),
            done: 34_000_000,
            total: Some(410_000_000),
            rate: Some(260_000),
            idle_ms: 100,
            elapsed_ms: 60_000,
        }],
        quiet_ms: 300,
        memory: Memory {
            held: 1_200_000_000,
            peak: 1_200_000_000,
            limit: 24_000_000_000,
            over: None,
            held_back: 0,
            releases: 0,
        },
        ..Now::default()
    }
}

/// A stall: nothing runs, nothing arrives, and one task is ready with nowhere said to run it.
fn stall() -> Now {
    Now {
        at_ms: 200_000,
        phase: Phase::Compiling,
        tally: Tally { waiting: 19, ready: 1, running: 0, done: 100, failed: 0, total: 120 },
        workers: 16,
        blockers: vec![Blocker {
            task: task(40, "link", "blaze-cargo blaze"),
            running: false,
            for_ms: 45_000,
            span: None,
            holds: 19,
            chain: 3,
            behind: vec!["blaze-cargo test".to_owned(), "blaze-ui test".to_owned()],
        }],
        quiet_ms: 45_000,
        memory: Memory {
            held: 8_000_000_000,
            peak: 8_000_000_000,
            limit: 24_000_000_000,
            over: None,
            held_back: 0,
            releases: 0,
        },
        ..Now::default()
    }
}

fn board(now: Now) -> Board {
    let mut board = Board::new("blaze test", Paint::Plain);
    board.take(now);
    board
}

/// The board drawn in `form` into a `width` by `height` buffer, as text: a line a row, each
/// row's trailing blanks cut, the blank rows at the bottom dropped.
fn draw(board: &Board, form: Form, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    board.render(form, area, &mut buf);
    let mut rows = Vec::new();
    for y in 0..height {
        let mut row = String::new();
        for x in 0..width {
            row.push_str(buf.cell((x, y)).map_or(" ", |it| it.symbol()));
        }
        rows.push(row.trim_end().to_owned());
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows.join("\n")
}

/// The compact line, which must be due.
fn line(board: &mut Board) -> String {
    let mut out = String::new();
    assert!(board.line(&mut out), "a line is due");
    assert!(!out.ends_with(' ') && !out.contains('\n'), "{out:?}");
    out
}

/// Named sections of a snapshot: `-- name`, then its text.
fn sections(parts: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (at, (name, text)) in parts.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        out.push_str("-- ");
        out.push_str(name);
        out.push('\n');
        out.push_str(text);
        out.push('\n');
    }
    out
}

#[test]
fn andrews_build_names_the_one_running_job_and_what_it_holds_back() {
    let mut board = board(andrew());
    let full = draw(&board, Form::Full, 100, 30);
    let short = draw(&board, Form::Full, 100, 8);
    board.key(Key::Other);
    assert!(!board.expanded());
    board.key(Key::Expand);
    assert!(board.expanded());
    let expanded = draw(&board, Form::Full, 100, 30);
    board.key(Key::Expand);
    let height = board.height(Form::Inline, 100);
    assert!(usize::from(height) <= INLINE_ROWS);
    let inline = draw(&board, Form::Inline, 100, height);
    let line = line(&mut board);
    expect_file!["snapshots/andrew.txt"].assert_eq(&sections(&[
        ("full 100x30", &full),
        ("full 100x8", &short),
        ("expanded 100x30", &expanded),
        ("inline 100", &inline),
        ("line", &line),
    ]));
}

#[test]
fn the_memory_line_says_over_and_squeezed() {
    let mut over = board(over());
    let mut squeezed = board(squeezed());
    let (over_full, squeezed_full) =
        (draw(&over, Form::Full, 100, 30), draw(&squeezed, Form::Full, 100, 30));
    let over_inline = draw(&over, Form::Inline, 100, 6);
    let (over_line, squeezed_line) = (line(&mut over), line(&mut squeezed));
    expect_file!["snapshots/memory.txt"].assert_eq(&sections(&[
        ("over full 100x30", &over_full),
        ("over inline 100x6", &over_inline),
        ("over line", &over_line),
        ("squeezed full 100x30", &squeezed_full),
        ("squeezed line", &squeezed_line),
    ]));
}

#[test]
fn a_cold_build_shows_its_download_and_what_its_stage_waits_for() {
    let mut board = board(cold());
    let full = draw(&board, Form::Full, 100, 20);
    let line = line(&mut board);
    expect_file!["snapshots/cold.txt"]
        .assert_eq(&sections(&[("full 100x20", &full), ("line", &line)]));
}

#[test]
fn a_stall_says_nothing_runs_and_names_the_ready_blocker() {
    let mut board = board(stall());
    let full = draw(&board, Form::Full, 100, 20);
    let line = line(&mut board);
    expect_file!["snapshots/stall.txt"]
        .assert_eq(&sections(&[("full 100x20", &full), ("line", &line)]));
}

/// The daemon's stall in its wait-for graph is said in every form while it lasts: a row under
/// the warning in the full and inline forms, and at the end of the compact line, which is due
/// as it appears. Drop the row or the line's part and these fail.
#[test]
fn a_lock_stall_is_said_in_every_form_while_it_lasts() {
    const CONVOY: &str =
        "convoy: 61 behind thread 7 on lock flight.intern (oldest 2400 ms); the head is running";
    let mut board = board(andrew());
    let mut out = String::new();
    assert!(board.line(&mut out));
    assert!(!out.contains("convoy"), "{out}");
    let mut now = andrew();
    now.at_ms += 1;
    now.stall = Some(CONVOY.to_owned());
    board.take(now);
    out.clear();
    assert!(board.line(&mut out), "a stall appearing makes a line due");
    assert!(out.ends_with(&format!(" · {CONVOY}")), "{out}");
    let said = format!("⚠ {CONVOY}");
    let full = draw(&board, Form::Full, 120, 30);
    assert!(full.contains(&said), "{full}");
    let short = draw(&board, Form::Full, 120, 4);
    assert!(short.contains(&said), "kept while there is room: {short}");
    let height = board.height(Form::Inline, 120);
    assert!(usize::from(height) <= INLINE_ROWS);
    let inline = draw(&board, Form::Inline, 120, height);
    assert!(inline.contains(&said), "{inline}");
}

#[test]
fn a_line_is_due_on_a_change_or_every_ten_seconds() {
    let mut out = String::new();
    assert!(!Board::new("blaze test", Paint::Plain).line(&mut out), "nothing taken, nothing said");
    let mut board = board(andrew());
    assert!(board.line(&mut out));
    out.clear();
    assert!(!board.line(&mut out) && out.is_empty(), "the same model again: {out}");

    let mut now = andrew();
    now.at_ms += 9_999;
    board.take(now.clone());
    assert!(!board.line(&mut out), "under ten seconds later: {out}");
    now.at_ms = andrew().at_ms + 10_000;
    board.take(now.clone());
    assert!(board.line(&mut out), "ten seconds after the last line");
    out.clear();

    now.blockers[0].task = task(40, "link", "blaze-cargo blaze");
    board.take(now.clone());
    assert!(board.line(&mut out), "the first blocker changed");
    assert!(out.contains("on link blaze-cargo blaze"), "{out}");
    out.clear();
    assert!(!board.line(&mut out));

    now.memory.limit = 24 * GIB;
    board.take(now.clone());
    assert!(board.line(&mut out), "the pressure changed");
    assert!(out.contains("over the limit"), "{out}");
    out.clear();

    now.quiet_ms = 0;
    board.take(now.clone());
    assert!(!board.line(&mut out), "no longer slow is not due: {out}");
    now.quiet_ms = 270_000;
    board.take(now.clone());
    assert!(board.line(&mut out), "slow again");
    assert!(out.ends_with("slow: nothing settled for 4m30s"), "{out}");
}

#[test]
fn drawing_never_panics_at_any_size_and_is_the_same_twice() {
    const WIDTHS: [u16; 10] = [0, 1, 2, 5, 13, 40, 79, 80, 100, 120];
    const HEIGHTS: [u16; 10] = [0, 1, 2, 3, 5, 8, 13, 20, 30, 40];
    for now in [andrew(), over(), squeezed(), cold(), stall(), Now::default()] {
        let mut board = Board::new("blaze test", Paint::Color);
        board.take(now);
        for _ in 0..2 {
            board.key(Key::Expand);
            for form in [Form::Full, Form::Inline] {
                for (width, height) in WIDTHS.into_iter().flat_map(|w| HEIGHTS.map(|h| (w, h))) {
                    let _ = board.height(form, width);
                    let area = Rect::new(2, 1, width, height);
                    let mut once = Buffer::empty(Rect::new(0, 0, width + 4, height + 2));
                    let mut twice = once.clone();
                    board.render(form, area, &mut once);
                    board.render(form, area, &mut twice);
                    board.render(form, area, &mut twice);
                    assert_eq!(once, twice, "{form:?} {width}x{height}");
                    for y in 0..height + 2 {
                        for x in 0..width + 4 {
                            let inside = area.contains((x, y).into());
                            let blank = once.cell((x, y)).is_some_and(|it| it.symbol() == " ");
                            assert!(inside || blank, "{form:?} {width}x{height}: wrote ({x}, {y})");
                        }
                    }
                }
            }
        }
        // An area past the buffer's edge is clipped to it.
        let mut small = Buffer::empty(Rect::new(0, 0, 10, 3));
        board.render(Form::Full, Rect::new(5, 1, 120, 40), &mut small);
        board.render(Form::Inline, Rect::new(5, 1, 120, 40), &mut small);
    }
}
