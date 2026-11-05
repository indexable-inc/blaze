use std::time::Duration;

use ratatui::{buffer::Buffer, layout::Rect, text::Line};

use super::{BinaryEnd, Event, Paint, TestView, Verdict, ansi, fit, wrap};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// The cells of `buf` as text, a line a row, trailing blanks cut.
fn text(buf: &Buffer) -> String {
    let area = buf.area;
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        for x in area.left()..area.right() {
            row.push_str(buf.cell((x, y)).map_or(" ", |it| it.symbol()));
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

fn lines(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| line.spans.iter().map(|it| it.content.as_ref()).collect::<String>() + "\n")
        .collect()
}

const PANIC: &str = "some stdout\nthread 'math::adds' panicked at src/lib.rs:7:9:\nassertion `left == right` failed\n  left: 3\n right: 4\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";

/// A run as the driver feeds it: two units, a test executable ready while the second still
/// compiles, its tests (one failing, one slow), then the build's end and a second executable.
fn transcript(view: &mut TestView) {
    let queued = |binary: u32, label: &str, order: &str| Event::BinaryQueued {
        binary,
        label: label.to_owned(),
        order: order.to_owned(),
        rerun: format!("-p {label}"),
    };
    let ended =
        |binary: u32, name: &str, verdict: Verdict, took: u64, output: &str| Event::TestEnded {
            binary,
            name: name.to_owned(),
            verdict,
            took: Some(ms(took)),
            output: output.to_owned(),
        };
    let events = [
        (0, Event::UnitBegan { id: 0, label: "calc lib test".to_owned() }),
        (5, Event::UnitBegan { id: 1, label: "calc test it".to_owned() }),
        (900, Event::UnitEnded { id: 0 }),
        (910, queued(0, "calc lib", "calc 0 calc")),
        (910, Event::BinaryStarted { binary: 0 }),
        (920, Event::SuiteTotal { binary: 0, tests: 3 }),
        (920, Event::TestStarted { binary: 0, name: "math::adds".to_owned() }),
        (920, Event::TestStarted { binary: 0, name: "math::waits".to_owned() }),
        (921, Event::TestStarted { binary: 0, name: "math::skipped".to_owned() }),
        (930, ended(0, "math::adds", Verdict::Failed, 10, PANIC)),
        (931, ended(0, "math::skipped", Verdict::Ignored, 0, "")),
    ];
    for (at, event) in events {
        view.apply(ms(at), event);
    }
}

#[test]
fn a_run_shows_its_build_and_tests_together_then_a_sorted_summary() {
    let mut view = TestView::new(Paint::Plain);
    transcript(&mut view);
    view.tick(ms(2400));
    let area = Rect::new(0, 0, 80, 8);
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    let shown = text(&buf);
    assert!(
        shown.contains("blaze test")
            && shown.contains("compiling and testing")
            && shown.contains("2.40s"),
        "{shown}"
    );
    assert!(
        shown.contains("✔ 0")
            && shown.contains("✘ 1")
            && shown.contains("⊘ 1")
            && shown.contains("2/3+"),
        "{shown}"
    );
    assert!(
        shown.contains("1/2") && shown.contains("calc test it"),
        "the build row names the unit under way: {shown}"
    );
    assert!(
        shown.contains("math::waits") && shown.contains("calc lib"),
        "the test under way, and its executable: {shown}"
    );

    let above = lines(&view.take_above(60));
    assert!(above.starts_with("╭─ ✘ FAIL  math::adds"), "{above}");
    assert!(
        above.contains("▸ src/lib.rs:7:9") && above.contains("assertion `left == right` failed"),
        "{above}"
    );
    assert!(above.contains("some stdout") && !above.contains("RUST_BACKTRACE"), "{above}");
    assert!(
        above.lines().all(|it| unicode_width::UnicodeWidthStr::width(it) <= 60),
        "every line fits: {above}"
    );
    assert!(view.take_above(60).is_empty(), "a block is printed once");

    // The second executable finishes first; the summary still lists by executable and name.
    view.apply(ms(2500), Event::BuildEnded { ok: true });
    view.apply(
        ms(2500),
        Event::BinaryQueued {
            binary: 1,
            label: "calc it".into(),
            order: "calc 2 it".into(),
            rerun: "-p calc --test it".into(),
        },
    );
    view.apply(ms(2500), Event::BinaryStarted { binary: 1 });
    view.apply(
        ms(2510),
        Event::TestEnded {
            binary: 1,
            name: "b".into(),
            verdict: Verdict::Failed,
            took: Some(ms(3)),
            output: String::new(),
        },
    );
    view.apply(
        ms(2600),
        Event::BinaryEnded {
            binary: 1,
            end: BinaryEnd::Failed { status: "exit status: 101".into() },
            output: String::new(),
        },
    );
    view.apply(
        ms(7000),
        Event::TestEnded {
            binary: 0,
            name: "math::waits".into(),
            verdict: Verdict::Passed,
            took: None,
            output: String::new(),
        },
    );
    view.apply(
        ms(7001),
        Event::BinaryEnded {
            binary: 0,
            end: BinaryEnd::Failed { status: "exit status: 101".into() },
            output: String::new(),
        },
    );
    let summary = lines(&view.summary(80));
    let first = summary.find("✘ math::adds").expect(&summary);
    let second = summary.find("✘ b").expect(&summary);
    assert!(first < second, "sorted by executable order, not by finish: {summary}");
    assert!(
        summary.contains("1 passed")
            && summary.contains("2 failed")
            && summary.contains("1 ignored"),
        "{summary}"
    );
    assert!(
        summary.contains("slowest") && summary.contains("6.08s") && summary.contains("math::waits"),
        "{summary}"
    );
    assert!(
        summary.contains("src/lib.rs:7:9") && summary.contains("blaze test -p calc lib"),
        "{summary}"
    );
    assert!(summary.trim_end().ends_with("FAIL  some tests failed"), "{summary}");
}

#[test]
fn drawing_never_panics_at_any_size_and_is_the_same_twice() {
    let mut view = TestView::new(Paint::Color);
    transcript(&mut view);
    view.tick(ms(61_000));
    for (width, height) in [(0, 0), (1, 1), (3, 2), (10, 1), (19, 4), (40, 12), (200, 3)] {
        let area = Rect::new(2, 1, width, height);
        let mut once = Buffer::empty(Rect::new(0, 0, width + 4, height + 2));
        let mut twice = once.clone();
        view.render(area, &mut once);
        view.render(area, &mut twice);
        assert_eq!(once, twice, "{width}x{height}");
        let _ = wrap(ansi("\x1b[1;31merror\x1b[0m: x", Paint::Color), width as usize);
    }
    for width in [0, 1, 5, 15, 16, 17, 30] {
        let mut again = TestView::new(Paint::Plain);
        transcript(&mut again);
        let above = again.take_above(width);
        let summary = again.summary(width);
        assert!(
            above.iter().chain(&summary).all(|it| it.width() <= (width as usize).max(1)),
            "{width}: {}",
            lines(&above)
        );
    }
}

#[test]
fn ansi_keeps_the_text_and_fit_keeps_the_end() {
    let spans = ansi("\x1b[1m\x1b[38;5;12mwarning\x1b[0m: unused\tvar\r\n", Paint::Color);
    let text: String = spans.iter().map(|it| it.content.as_ref()).collect();
    assert_eq!(text, "warning: unused    var");
    assert_eq!(fit("crate::module::tests::name", 12), "…tests::name");
    assert_eq!(fit("short", 12), "short");
}

/// A cold build that compiles nothing yet says what it waits on, never "compiling"; a unit
/// open a while says what it is inside; most workers idle behind one busy one says what it
/// runs and what waits on it; and past ten seconds the hint is under the view.
#[test]
fn a_build_that_waits_says_on_what() {
    let mut view = TestView::new(Paint::Plain);
    let doing = |on: Option<&str>, units: Vec<(u32, &str)>| Event::Doing {
        on: on.map(str::to_owned),
        busy: 1,
        idle: 15,
        running: Some("codegen `std::fmt::write` in std (4 min 30 s)".to_owned()),
        units: units.into_iter().map(|(id, it)| (id, it.to_owned())).collect(),
    };
    view.apply(ms(2000), doing(Some("downloading llvm.tar.xz: 34.0 of 410.0 MB (8%)"), vec![]));
    let area = Rect::new(0, 0, 160, 8);
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    let shown = text(&buf);
    assert!(shown.contains("waiting") && !shown.contains("compiling"), "{shown}");
    assert!(shown.contains("0/0  downloading llvm.tar.xz: 34.0 of 410.0 MB (8%)"), "{shown}");

    view.apply(ms(3000), Event::UnitBegan { id: 0, label: "std build script".to_owned() });
    view.apply(ms(3000), Event::UnitBegan { id: 1, label: "libc build script".to_owned() });
    view.apply(ms(3000), Event::UnitBegan { id: 2, label: "core lib".to_owned() });
    view.apply(ms(12_000), doing(None, vec![(2, "codegen `std::fmt::write` in std (9 s)")]));
    let mut buf = Buffer::empty(area);
    view.render(area, &mut buf);
    let shown = text(&buf);
    assert!(shown.contains("compiling"), "{shown}");
    assert!(shown.contains("core lib: codegen `std::fmt::write` in std (9 s) 9.00s"), "{shown}");
    assert!(
        shown.contains(
            "held   1 worker busy, 15 idle: waiting on codegen `std::fmt::write` in std (4 min 30 s) \
             · blocks libc build script, std build script"
        ),
        "{shown}"
    );
    assert!(shown.contains("what each step does: `blaze explore`; live spans: `blaze debug live`"), "{shown}");
}
