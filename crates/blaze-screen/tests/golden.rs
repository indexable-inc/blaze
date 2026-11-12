//! Golden frames: recorded builds replayed through the core, drawn into the reference
//! terminal, and what it shows at a few moments compared with the frames kept under
//! `tests/golden/`. A transcript is the event stream of a real build, as `BLAZE_UI_TRANSCRIPT`
//! recorded it (one [`Stamped`] a line): hello-world from an empty store, ripgrep from an
//! empty store (423 tasks), an edit, a failure and a test run. `UPDATE_EXPECT=1` rewrites
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

#[test]
fn hello_world_from_nothing() {
    expect_file!["golden/hello-cold-80x24.txt"].assert_eq(&replay("hello-cold", Command::Run, 80, 24, 4));
    expect_file!["golden/hello-cold-200x60.txt"].assert_eq(&replay("hello-cold", Command::Run, 200, 60, 4));
}

#[test]
fn ripgrep_from_nothing() {
    expect_file!["golden/rg-cold-80x24.txt"].assert_eq(&replay("rg-cold", Command::Build, 80, 24, 6));
    expect_file!["golden/rg-cold-200x60.txt"].assert_eq(&replay("rg-cold", Command::Build, 200, 60, 6));
    expect_file!["golden/rg-cold-40x10.txt"].assert_eq(&replay("rg-cold", Command::Build, 40, 10, 6));
}
