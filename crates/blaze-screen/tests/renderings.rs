//! One message, each reader: five of the screen's messages as a 24-bit terminal on a dark
//! background takes them (escapes written `\e`), as a terminal with no colour takes them, as a
//! pipe takes them, and their text as JSON, kept in `tests/golden/renderings.txt`.

use blaze_report::{
    palette::{Depth, Ground},
    say,
    say::{At, Cmd, Code, Count},
    terminal::Terminal,
};
use blaze_screen::{Built, Caps, Command, Event, Options, Screen, Test, Verdict, Why, encode};
use expect_test::expect_file;

fn why(wasted: bool) -> Why {
    Why {
        wasted,
        cause: say!(At("/w/src/main.rs"), " changed"),
        headline: match wasted {
            false => say!("recomputed ", Count(2), " steps because ", At("/w/src/main.rs"), " changed"),
            true => say!(
                "recomputed ",
                Count(1),
                " step because ",
                At("/w/src/main.rs"),
                " changed, though no item in it did (",
                Code("fn main"),
                " is as it was)"
            ),
        },
        functions: vec!["hello::add".to_owned()],
        more: Some("blaze explain why:input".to_owned()),
    }
}

/// The five messages, each as the events that make it.
fn messages() -> Vec<(&'static str, Command, Vec<Event>)> {
    let built = |ok: bool| Built {
        ok,
        compiled: 1,
        cached: 31,
        tier: Some("tier 0".to_owned()),
        why_follows: ok,
        ..Built::default()
    };
    vec![
        (
            "a build with nothing to do",
            Command::Build,
            vec![Event::Built(Built {
                compiled: 0,
                why_follows: false,
                program: Some(
                    "/home/me/.cache/blaze/build/hello-world-5373d6c45d4151d3dfd664b10fb56112/debug/hello-world"
                        .to_owned(),
                ),
                size_bytes: Some(4_123_456),
                ..built(true)
            })],
        ),
        ("an edit", Command::Run, vec![Event::Built(built(true)), Event::Why(vec![why(false)])]),
        (
            "a failure",
            Command::Run,
            vec![
                Event::Diagnosed { error: true, unit: Some("hello (bin)".to_owned()) },
                Event::Built(Built { next: Some("blaze explain hello".to_owned()), ..built(false) }),
            ],
        ),
        ("work that should not have been done", Command::Run, vec![
            Event::Built(Built { compiled: 0, ..built(true) }),
            Event::Why(vec![why(true)]),
        ]),
        (
            "warnings with a fix",
            Command::Build,
            vec![
                Event::Diagnosed { error: false, unit: Some("hello (bin)".to_owned()) },
                Event::Diagnosed { error: false, unit: Some("hello (bin)".to_owned()) },
                Event::Built(Built { fixable: 2, why_follows: false, ..built(true) }),
            ],
        ),
        (
            "a failed test",
            Command::Test,
            vec![
                Event::Test(Test::Queued { binary: 1, label: "calc lib".to_owned(), rerun: "blaze test -p calc --lib".to_owned() }),
                Event::Test(Test::Started { binary: 1 }),
                Event::Test(Test::TestEnded {
                    binary: 1,
                    name: "tests::adds_wrong".to_owned(),
                    verdict: Verdict::Failed,
                    took_ms: Some(3),
                    output: String::new(),
                }),
                Event::Test(Test::Over),
            ],
        ),
    ]
}

fn drawn(command: Command, events: &[Event], caps: Caps, terminal: &Terminal) -> String {
    let options = Options {
        root: Some("/w".to_owned()),
        cwd: Some("/w".to_owned()),
        home: Some("/home/me".to_owned()),
        ..Options::of(command)
    };
    let mut screen = Screen::new(options, caps);
    screen.advance_clock(700);
    let mut out = String::new();
    for event in events {
        screen.take(event.clone());
        if let Some(frame) = screen.frame() {
            encode(&frame, terminal, &mut out);
        }
    }
    // The update's brackets, the cursor and the progress report are the same for every
    // message: the lines are what is compared.
    out.lines()
        .map(|it| it.replace("\x1b[?2026h", "").replace("\x1b[?2026l", "").replace("\x1b[?25h", "").replace('\x1b', "\\e"))
        .filter(|it| !it.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ghostty as it says it is (`TERM=xterm-ghostty`, `COLORTERM=truecolor`): 24-bit colour,
/// links and a progress report; and a line is drawn there with hue, not faint text alone.
#[test]
fn ghostty_is_given_colour_and_links() {
    let var = |name: &str| match name {
        "TERM" => Some("xterm-ghostty".to_owned()),
        "COLORTERM" => Some("truecolor".to_owned()),
        "TERM_PROGRAM" => Some("ghostty".to_owned()),
        "TERM_PROGRAM_VERSION" => Some("1.2.3".to_owned()),
        _ => None,
    };
    let terminal = blaze_report::terminal::detect(&blaze_report::terminal::Config::DEFAULT, var, true, Some(120));
    assert_eq!((terminal.depth(), terminal.caps.links, terminal.progress), (Depth::True, true, true));
    let live = Caps { live: true, cols: 120, rows: 40, unicode: true };
    let (_, command, events) = messages().remove(0);
    let line = drawn(command, &events, live, &terminal);
    assert!(line.contains("\\e[38;2;63;185;80m✓ up to date"), "{line}");
    assert!(line.contains("]8;;file:///home/me/.cache/blaze/build/hello-world-5373d6c45d4151d3dfd664b10fb56112/debug/hello-world"), "{line}");
    assert!(line.contains("~/.cache/blaze/build/hello-world-5373"), "{line}");
}

#[test]
fn each_reader_is_drawn_the_same_message() {
    let live = Caps { live: true, cols: 100, rows: 30, unicode: true };
    let colour = Terminal {
        caps: blaze_report::Caps { color: true, links: true, width: Some(100) },
        colors: Depth::True,
        ground: Ground::Dark,
        ..Terminal::PIPE
    };
    let light16 = Terminal { colors: Depth::Ansi16, ground: Ground::Light, ..colour };
    let plain = Terminal { caps: blaze_report::Caps { color: false, links: false, width: Some(100) }, ..colour };
    let mut out = String::new();
    for (name, command, events) in messages() {
        out.push_str(&format!("== {name}\n-- a terminal, 24-bit, dark\n{}\n", drawn(command, &events, live, &colour)));
        out.push_str(&format!("-- a terminal, 16 colours\n{}\n", drawn(command, &events, live, &light16)));
        let bare = drawn(command, &events, live, &plain);
        assert!(!bare.contains("\\e"), "no colour, no escape: {bare}");
        out.push_str(&format!("-- a terminal, NO_COLOR\n{bare}\n"));
        let piped = drawn(command, &events, Caps::PIPE, &Terminal::PIPE);
        assert!(!piped.contains("\\e"), "a pipe is written no escape: {piped}");
        out.push_str(&format!("-- a pipe\n{piped}\n"));
        for event in &events {
            if let Event::Why(whys) = event {
                out.push_str(&format!("-- its why as JSON\n{}\n", serde_json::to_string(whys).unwrap()));
            }
        }
    }
    expect_file!["golden/renderings.txt"].assert_eq(&out);
}
