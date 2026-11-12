//! Golden frames: recorded builds replayed through the core, drawn into the reference
//! terminal, and what it shows at a few moments compared with the frames kept under
//! `tests/golden/`. A transcript is the event stream of a real build, as `BLAZE_UI_TRANSCRIPT`
//! recorded it on 2026-11-12 (one [`Stamped`] a line): hello-world from an empty store, an
//! edit of it, a failure of it, a test run, ripgrep from an empty store (423 tasks), and the
//! first 26 seconds of blaze building blaze-thin (1504 tasks). `UPDATE_EXPECT=1` rewrites
//! the frames; read the diff.

mod vt;

use blaze_report::terminal::Terminal;
use blaze_screen::{Caps, Command, Event, Options, Screen, Stamped, encode};
use expect_test::expect_file;

fn transcript(name: &str) -> Vec<Stamped> {
    let path = format!("{}/tests/transcripts/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    text.lines()
        .filter(|it| !it.is_empty())
        .map(|it| serde_json::from_str(it).unwrap_or_else(|error| panic!("{path}: {error}: {it}")))
        .collect()
}

/// A terminal that has colour, links and a progress report, so the frames' bytes carry all of
/// them through the reference terminal.
fn terminal() -> Terminal {
    let config = blaze_report::terminal::Config::DEFAULT;
    let var = |name: &str| match name {
        "TERM_PROGRAM" => Some("ghostty".to_owned()),
        "TERM_PROGRAM_VERSION" => Some("1.2.0".to_owned()),
        "TERM" => Some("xterm-ghostty".to_owned()),
        _ => None,
    };
    blaze_report::terminal::detect(&config, var, true, Some(80))
}

/// `name`'s transcript on a terminal of `cols` by `rows`: what the terminal shows at `shots`
/// moments spread over the build, and at its end, with the time of each. The clock ticks once
/// a second between events, as the driver's thread does.
fn replay(name: &str, command: Command, cols: u16, rows: u16, shots: usize) -> String {
    let events = transcript(name);
    let caps = Caps { live: true, cols, rows, unicode: true };
    let mut screen = Screen::new(Options::of(command), caps);
    let mut vt = vt::Vt::new(cols, rows);
    let terminal = terminal();
    let mut out = String::new();
    let mut bytes = String::new();
    let mut draw = |screen: &mut Screen, vt: &mut vt::Vt| {
        if let Some(frame) = screen.frame() {
            bytes.clear();
            encode(&frame, &terminal, &mut bytes);
            vt.write(&bytes);
        }
    };
    let end = events.last().map_or(0, |it| it.at_ms);
    let mut shot = 1;
    let mut now = 0;
    for stamped in &events {
        while let Some(due) = screen.next_deadline().filter(|due| *due < stamped.at_ms) {
            now = due;
            screen.advance_clock(now);
            draw(&mut screen, &mut vt);
            // A shot at each of `shots` moments spread evenly over the build.
            if shot <= shots && now >= end * shot as u64 / (shots as u64 + 1) {
                shot += 1;
                out.push_str(&format!("== {now} ms\n{}\n", vt.shown()));
            }
        }
        now = now.max(stamped.at_ms);
        screen.advance_clock(now);
        screen.take(stamped.event.clone());
        draw(&mut screen, &mut vt);
    }
    screen.take(Event::Clear);
    draw(&mut screen, &mut vt);
    out.push_str(&format!("== end\n{}\n", vt.shown()));
    assert!(!vt.cursor_hidden, "the cursor is shown again at the end");
    assert_eq!(vt.progress().unwrap_or("0;0"), "0;0", "the progress report is cleared at the end");
    out
}

/// `name`'s transcript to a pipe: every byte written. The transcripts were recorded on a
/// terminal, so the lines that stay were drawn for it in colour; a pipe's client asks for none,
/// which is said here by taking the colour out of them.
fn piped(name: &str, command: Command) -> String {
    let mut screen = Screen::new(Options::of(command), Caps::PIPE);
    let mut out = String::new();
    for stamped in transcript(name) {
        screen.advance_clock(stamped.at_ms);
        screen.take(match stamped.event {
            Event::Said(text) => Event::Said(plain(&text)),
            // A pause of the build is a line to a pipe, made by the printer, not an event.
            Event::Waits(_) => continue,
            event => event,
        });
        if let Some(frame) = screen.frame() {
            encode(&frame, &Terminal::PIPE, &mut out);
        }
    }
    assert!(!out.contains('\x1b'), "a pipe is written no escape");
    out
}

/// `text` without its SGR sequences.
fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("\x1b[") {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        rest = rest.find('m').map_or("", |end| &rest[end + 1..]);
    }
    out + rest
}

#[test]
fn hello_world_from_nothing() {
    expect_file!["golden/hello-cold-80x24.txt"].assert_eq(&replay("hello-cold", Command::Run, 80, 24, 4));
    expect_file!["golden/hello-cold-200x60.txt"].assert_eq(&replay("hello-cold", Command::Run, 200, 60, 4));
    expect_file!["golden/hello-cold-piped.txt"].assert_eq(&piped("hello-cold", Command::Run));
}

/// One function's body edited, then `blaze run`: no region was ever due, and one line says
/// what was recompiled, why, and how long it took.
#[test]
fn an_edit() {
    expect_file!["golden/edit-80x24.txt"].assert_eq(&replay("edit", Command::Run, 80, 24, 1));
    expect_file!["golden/edit-piped.txt"].assert_eq(&piped("edit", Command::Run));
}

/// A build that fails: the diagnostics stay, then one line says what failed and what to run.
#[test]
fn a_failure() {
    expect_file!["golden/fail-80x24.txt"].assert_eq(&replay("fail", Command::Run, 80, 24, 2));
    expect_file!["golden/fail-piped.txt"].assert_eq(&piped("fail", Command::Run));
}

/// `blaze test` of a library with a failing test, an ignored one and slow ones.
#[test]
fn a_test_run() {
    expect_file!["golden/test-80x24.txt"].assert_eq(&replay("test", Command::Test, 80, 24, 6));
    expect_file!["golden/test-200x60.txt"].assert_eq(&replay("test", Command::Test, 200, 60, 6));
}

#[test]
fn ripgrep_from_nothing() {
    expect_file!["golden/rg-cold-80x24.txt"].assert_eq(&replay("rg-cold", Command::Build, 80, 24, 6));
    expect_file!["golden/rg-cold-200x60.txt"].assert_eq(&replay("rg-cold", Command::Build, 200, 60, 6));
    expect_file!["golden/rg-cold-40x10.txt"].assert_eq(&replay("rg-cold", Command::Build, 40, 10, 6));
    expect_file!["golden/rg-cold-piped.txt"].assert_eq(&piped("rg-cold", Command::Build));
}

/// The first 26 seconds of blaze building blaze-thin from an empty store (1504 tasks, up to
/// 21 units at once), then an interrupt: the transcript is cut there to keep it small.
#[test]
fn blaze_building_blaze() {
    expect_file!["golden/blaze-80x24.txt"].assert_eq(&replay("blaze-26s", Command::Build, 80, 24, 5));
    expect_file!["golden/blaze-200x60.txt"].assert_eq(&replay("blaze-26s", Command::Build, 200, 60, 5));
}
