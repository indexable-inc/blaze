//! The frame's design, stated as bytes: each test spells out the exact output, escapes and
//! all, from the design (one loud thing, three colour places, quiet code, the enclosing scope as
//! context, the label hung from its span); none is regenerated from the renderer.

use blaze_own::{AString};

use super::words::{Area, say};
use super::{
    Background, Caps, Closing, Colour, Counts, Edit, Finding, Fixes, Input, LINK_FILE, Mark, Source, TINT_DARK, TINT_LIGHT,
    TINT_UNKNOWN, background, closing, render, tint,
};
use crate::Severity;

// The escapes, each spelled out once: every one the renderer writes starts with a reset.
const R: &str = "\x1b[0m";
const D: &str = "\x1b[0;2m";
const B: &str = "\x1b[0;1m";
/// The severity words, bold in their colour.
const ERROR: &str = "\x1b[0;1;31m";
const WARNING: &str = "\x1b[0;1;33m";
const NOTE: &str = "\x1b[0;1;36m";
/// The label and the `^` row, in the severity's plain colour.
const RED: &str = "\x1b[0;31m";
const YELLOW: &str = "\x1b[0;33m";
const CYAN: &str = "\x1b[0;36m";
/// The span on a truecolor terminal: bold, curly underline in red.
const SPAN: &str = "\x1b[0;1;4:3;58;5;1m";
/// The literals' tint when the background is not known: #76869c.
const TINT: &str = "\x1b[0;38;2;118;134;156m";
const LINK_END: &str = "\x1b]8;;\x1b\\";

/// The owner's manifest, as it stood when he ran `blaze check`.
const MANIFEST: &str = "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nbstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n";

fn truecolor(width: u16) -> Caps {
    Caps { width, ..Caps::RICH }
}

fn ansi(width: u16) -> Caps {
    Caps { width, ..Caps::ANSI }
}

fn plain(width: u16) -> Caps {
    Caps { width, ..Caps::FRAMES }
}

fn draw(shown: &str, absolute: &str, text: &str, finding: Finding<'_>, caps: Caps) -> String {
    draw_fixes(shown, absolute, text, finding, caps, Fixes::ByHand)
}

fn draw_fixes(shown: &str, absolute: &str, text: &str, finding: Finding<'_>, caps: Caps, fixes: Fixes) -> String {
    let sources = [Source { shown, absolute, text: text.as_bytes() }];
    let findings = [finding];
    let input = Input { findings: &findings, sources: &sources, link: LINK_FILE, fixes, ids: &[] };
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    render(&mut out, &input, caps, blaze_own::testing::heap().alloc()).expect("the finding's ranges are in its file");
    out.as_str().to_string()
}

fn span(text: &str, needle: &str, label: &'static str) -> Mark<'static> {
    let start = text.find(needle).expect("the needle is in the text") as u32;
    Mark { start, end: start + needle.len() as u32, label }
}

fn finding<'a>(severity: Severity, rule: &'a str, message: &'a str, primary: Mark<'a>) -> Finding<'a> {
    Finding { severity, rule, message, file: 0, item: Some(0), primary, secondary: &[], fix: &[] }
}

/// The owner's finding, worded by the table as the CLI words it.
fn his(caps: Caps) -> String {
    let mut headline = AString::new_in(blaze_own::testing::heap().alloc());
    let mut label = AString::new_in(blaze_own::testing::heap().alloc());
    let rule = say(Area::Cli, 2, &["bstd"], &mut headline, &mut label).expect("rule 2 has words");
    let start = MANIFEST.find("bstd").expect("bstd is in the manifest") as u32;
    let primary = Mark { start, end: start + 4, label: label.as_str() };
    let mut dependency = finding(Severity::Error, rule, headline.as_str(), primary);
    dependency.item = None;
    draw("Cargo.toml", "/work/hello/Cargo.toml", MANIFEST, dependency, caps)
}

fn same(name: &str, got: &str, want: &str) {
    assert!(got == want, "{name}: the frame differs\n--- want\n{want}\n--- got\n{got}\n--- want, escaped\n{want:?}\n--- got, escaped\n{got:?}");
}

#[test]
fn his_finding_at_80() {
    let link = "\x1b]8;;file:///work/hello/Cargo.toml\x1b\\";
    let want = [
        ERROR, "error  ", R, "this build of blaze cannot read the dependency ", D, "`", B, "bstd", D, "` ", R, "yet\n",
        "  ", D, "╭─ ", link, R, "Cargo.toml", D, ":7:1", LINK_END, R, "\n",
        D, "6 │ [dependencies]", R, "\n",
        D, "7 │ ", SPAN, "bstd", R, " ", D, "= { ", R, "package ", D, "= ", TINT, "\"blaze-std\"", D, ", ", R, "path ", D, "= ",
        TINT, "\"/Volumes/Projects/blaze-macbuild-11", D, "…", R, "\n",
        "  ", D, "│ ╰─ ", RED, "nothing this package gets from it is checked", R, "\n",
        "  ", D, "╰─ blaze explain dependency-not-read", R, "\n",
    ]
    .concat();
    same("truecolor 80", &his(truecolor(80)), &want);

    let want = [
        ERROR, "error  ", R, "this build of blaze cannot read the dependency ", D, "`", B, "bstd", D, "` ", R, "yet\n",
        "  ", D, "╭─ ", R, "Cargo.toml", D, ":7:1", R, "\n",
        D, "6 │ [dependencies]", R, "\n",
        D, "7 │ ", B, "bstd ", D, "= { ", R, "package ", D, "= \"blaze-std\", ", R, "path ", D,
        "= \"/Volumes/Projects/blaze-macbuild-11…", R, "\n",
        "  ", D, "│ ", RED, "^^^^", R, "\n",
        "  ", D, "│ ╰─ ", RED, "nothing this package gets from it is checked", R, "\n",
        "  ", D, "╰─ blaze explain dependency-not-read", R, "\n",
    ]
    .concat();
    same("16 colours 80", &his(ansi(80)), &want);

    let want = "\
error  this build of blaze cannot read the dependency `bstd` yet
  ╭─ Cargo.toml:7:1
6 │ [dependencies]
7 │ bstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-11…
  │ ^^^^
  │ ╰─ nothing this package gets from it is checked
  ╰─ blaze explain dependency-not-read
";
    same("plain 80", &his(plain(80)), want);
}

/// At 60 the message would leave `yet` alone on its row: `bstd` comes down with it.
#[test]
fn his_finding_at_60() {
    let link = "\x1b]8;;file:///work/hello/Cargo.toml\x1b\\";
    let want = [
        ERROR, "error  ", R, "this build of blaze cannot read the dependency\n",
        "       ", D, "`", B, "bstd", D, "` ", R, "yet\n",
        "  ", D, "╭─ ", link, R, "Cargo.toml", D, ":7:1", LINK_END, R, "\n",
        D, "6 │ [dependencies]", R, "\n",
        D, "7 │ ", SPAN, "bstd", R, " ", D, "= { ", R, "package ", D, "= ", TINT, "\"blaze-std\"", D, ", ", R, "path ", D, "= ",
        TINT, "\"/Volumes/Projec", D, "…", R, "\n",
        "  ", D, "│ ╰─ ", RED, "nothing this package gets from it is checked", R, "\n",
        "  ", D, "╰─ blaze explain dependency-not-read", R, "\n",
    ]
    .concat();
    same("truecolor 60", &his(truecolor(60)), &want);

    let want = [
        ERROR, "error  ", R, "this build of blaze cannot read the dependency\n",
        "       ", D, "`", B, "bstd", D, "` ", R, "yet\n",
        "  ", D, "╭─ ", R, "Cargo.toml", D, ":7:1", R, "\n",
        D, "6 │ [dependencies]", R, "\n",
        D, "7 │ ", B, "bstd ", D, "= { ", R, "package ", D, "= \"blaze-std\", ", R, "path ", D, "= \"/Volumes/Projec…", R, "\n",
        "  ", D, "│ ", RED, "^^^^", R, "\n",
        "  ", D, "│ ╰─ ", RED, "nothing this package gets from it is checked", R, "\n",
        "  ", D, "╰─ blaze explain dependency-not-read", R, "\n",
    ]
    .concat();
    same("16 colours 60", &his(ansi(60)), &want);

    let want = "\
error  this build of blaze cannot read the dependency
       `bstd` yet
  ╭─ Cargo.toml:7:1
6 │ [dependencies]
7 │ bstd = { package = \"blaze-std\", path = \"/Volumes/Projec…
  │ ^^^^
  │ ╰─ nothing this package gets from it is checked
  ╰─ blaze explain dependency-not-read
";
    same("plain 60", &his(plain(60)), want);
}

#[test]
fn his_finding_at_120() {
    let link = "\x1b]8;;file:///work/hello/Cargo.toml\x1b\\";
    let want = [
        ERROR, "error  ", R, "this build of blaze cannot read the dependency ", D, "`", B, "bstd", D, "` ", R, "yet\n",
        "  ", D, "╭─ ", link, R, "Cargo.toml", D, ":7:1", LINK_END, R, "\n",
        D, "6 │ [dependencies]", R, "\n",
        D, "7 │ ", SPAN, "bstd", R, " ", D, "= { ", R, "package ", D, "= ", TINT, "\"blaze-std\"", D, ", ", R, "path ", D, "= ",
        TINT, "\"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" ", D, "}", R, "\n",
        "  ", D, "│ ╰─ ", RED, "nothing this package gets from it is checked", R, "\n",
        "  ", D, "╰─ blaze explain dependency-not-read", R, "\n",
    ]
    .concat();
    same("truecolor 120", &his(truecolor(120)), &want);

    let want = [
        ERROR, "error  ", R, "this build of blaze cannot read the dependency ", D, "`", B, "bstd", D, "` ", R, "yet\n",
        "  ", D, "╭─ ", R, "Cargo.toml", D, ":7:1", R, "\n",
        D, "6 │ [dependencies]", R, "\n",
        D, "7 │ ", B, "bstd ", D, "= { ", R, "package ", D, "= \"blaze-std\", ", R, "path ", D,
        "= \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }", R, "\n",
        "  ", D, "│ ", RED, "^^^^", R, "\n",
        "  ", D, "│ ╰─ ", RED, "nothing this package gets from it is checked", R, "\n",
        "  ", D, "╰─ blaze explain dependency-not-read", R, "\n",
    ]
    .concat();
    same("16 colours 120", &his(ansi(120)), &want);

    let want = "\
error  this build of blaze cannot read the dependency `bstd` yet
  ╭─ Cargo.toml:7:1
6 │ [dependencies]
7 │ bstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }
  │ ^^^^
  │ ╰─ nothing this package gets from it is checked
  ╰─ blaze explain dependency-not-read
";
    same("plain 120", &his(plain(120)), want);
}

const LEDGER: &str = "impl Ledger {\n    /// Adds every row.\n    pub fn total(&self, rows: &[Row]) -> u64 {\n        let mut sum = 0;\n        for row in rows {\n            sum += row.value * undefined_rate;\n        }\n        sum\n    }\n}\n";

fn ledger(caps: Caps) -> String {
    let unknown = finding(Severity::Error, "resolve:unknown", "cannot find `undefined_rate` in this scope", span(LEDGER, "undefined_rate", "not a name here"));
    draw("src/ledger.rs", "/work/src/ledger.rs", LEDGER, unknown, caps)
}

/// A span mid-line in a function: the innermost item enclosing it is the `fn` (not the
/// `impl`, not the `for`), its first line shown, a `⋮` for the two lines between; nothing
/// below the span.
#[test]
fn a_rust_span_inside_a_function() {
    let link = "\x1b]8;;file:///work/src/ledger.rs\x1b\\";
    let want = [
        ERROR, "error  ", R, "cannot find ", D, "`", B, "undefined_rate", D, "` ", R, "in this scope\n",
        "  ", D, "╭─ ", link, R, "src/ledger.rs", D, ":6:32", LINK_END, R, "\n",
        D, "3 │     pub fn total(&self, rows: &[Row]) -> u64 {", R, "\n",
        "  ", D, "⋮", R, "\n",
        D, "6 │             ", R, "sum ", D, "+= ", R, "row", D, ".", R, "value ", D, "* ", SPAN, "undefined_rate", D, ";", R, "\n",
        "  ", D, "│                                ╰─ ", RED, "not a name here", R, "\n",
        "  ", D, "╰─ blaze explain resolve:unknown", R, "\n",
    ]
    .concat();
    same("truecolor 80", &ledger(truecolor(80)), &want);

    let want = "\
error  cannot find `undefined_rate` in this scope
  ╭─ src/ledger.rs:6:32
3 │     pub fn total(&self, rows: &[Row]) -> u64 {
  ⋮
6 │             sum += row.value * undefined_rate;
  │                                ^^^^^^^^^^^^^^
  │                                ╰─ not a name here
  ╰─ blaze explain resolve:unknown
";
    same("plain 80", &ledger(plain(80)), want);
}

const AREA: &str = "fn area(w: u32, h: u32) -> u32 {\n    let scale = 2;\n    w * h * scaling\n}\n";

fn area(severity: Severity, text: &str, caps: Caps) -> String {
    let unknown = finding(severity, "resolve:unknown", "cannot find `scaling` in this scope", span(text, "scaling", "did you mean `scale`?"));
    draw("src/area.rs", "/work/src/area.rs", text, unknown, caps)
}

/// One line between the header and the span is shown, dim, not hidden behind a `⋮`; a blank
/// one is left out, the gutter's numbers telling the jump.
#[test]
fn a_one_line_gap_is_that_line_unless_blank() {
    let want = "\
error  cannot find `scaling` in this scope
  ╭─ src/area.rs:3:13
1 │ fn area(w: u32, h: u32) -> u32 {
2 │     let scale = 2;
3 │     w * h * scaling
  │             ^^^^^^^
  │             ╰─ did you mean `scale`?
  ╰─ blaze explain resolve:unknown
";
    same("one line", &area(Severity::Error, AREA, plain(80)), want);

    let blank = "fn area(w: u32, h: u32) -> u32 {\n\n    w * h * scaling\n}\n";
    let want = "\
error  cannot find `scaling` in this scope
  ╭─ src/area.rs:3:13
1 │ fn area(w: u32, h: u32) -> u32 {
3 │     w * h * scaling
  │             ^^^^^^^
  │             ╰─ did you mean `scale`?
  ╰─ blaze explain resolve:unknown
";
    same("blank line", &area(Severity::Error, blank, plain(80)), want);
}

/// The three severities' colours, each in its three places only: the word, the `^` row (the
/// underline where there is no curly one) and the label.
#[test]
fn a_warning_and_a_note_take_their_own_colour() {
    for (severity, word, word_esc, colour) in
        [(Severity::Error, "error  ", ERROR, RED), (Severity::Warning, "warning  ", WARNING, YELLOW), (Severity::Note, "note  ", NOTE, CYAN)]
    {
        let want = [
            word_esc, word, R, "cannot find ", D, "`", B, "scaling", D, "` ", R, "in this scope\n",
            "  ", D, "╭─ ", R, "src/area.rs", D, ":3:13", R, "\n",
            D, "1 │ fn area(w: u32, h: u32) -> u32 {", R, "\n",
            D, "2 │     let scale = 2;", R, "\n",
            D, "3 │     ", R, "w ", D, "* ", R, "h ", D, "* ", B, "scaling", R, "\n",
            "  ", D, "│             ", colour, "^^^^^^^", R, "\n",
            "  ", D, "│             ╰─ ", colour, "did you mean `scale`?", R, "\n",
            "  ", D, "╰─ blaze explain resolve:unknown", R, "\n",
        ]
        .concat();
        same(word.trim_end(), &area(severity, AREA, ansi(80)), &want);
    }
}

const RUN: &str = "fn run() {\n    let task = spawn(async {\n        let a = fetch().await;\n        let b = parse(a);\n    });\n    task.wait();\n}\n";

fn run(caps: Caps) -> String {
    let start = RUN.find("async {").expect("the block is in the text") as u32;
    let end = RUN.find("    });").expect("the block's end is in the text") as u32 + 5;
    let block = finding(
        Severity::Error,
        "ladder:async",
        "an `async` block is not supported by this build of blaze yet",
        Mark { start, end, label: "this block" },
    );
    draw("src/run.rs", "/work/src/run.rs", RUN, block, caps)
}

/// A span over lines: its first line and its last, a `⋮` between, the label under the last.
#[test]
fn a_span_over_several_lines() {
    let link = "\x1b]8;;file:///work/src/run.rs\x1b\\";
    let want = [
        ERROR, "error  ", R, "an ", D, "`", B, "async", D, "` ", R, "block is not supported by this build of blaze yet\n",
        "  ", D, "╭─ ", link, R, "src/run.rs", D, ":2:22", LINK_END, R, "\n",
        D, "1 │ fn run() {", R, "\n",
        D, "2 │     ", R, "let task ", D, "= ", R, "spawn", D, "(", SPAN, "async {", R, "\n",
        "  ", D, "⋮", R, "\n",
        D, "5 │     ", SPAN, "}", D, ");", R, "\n",
        "  ", D, "│     ╰─ ", RED, "this block", R, "\n",
        "  ", D, "╰─ blaze explain ladder:async", R, "\n",
    ]
    .concat();
    same("truecolor 80", &run(truecolor(80)), &want);

    let want = "\
error  an `async` block is not supported by this build of blaze yet
  ╭─ src/run.rs:2:22
1 │ fn run() {
2 │     let task = spawn(async {
  │                      ^^^^^^^
  ⋮
5 │     });
  │     ^
  │     ╰─ this block
  ╰─ blaze explain ladder:async
";
    same("plain 80", &run(plain(80)), want);
}

fn hung(text: &str, label: &'static str) -> String {
    let unknown = finding(Severity::Error, "resolve:unknown", "cannot find `gamma_term`", span(text, "gamma_term", label));
    draw("src/lib.rs", "/work/src/lib.rs", text, unknown, plain(40))
}

/// A label that does not fit to the right of its span hangs to the left, its corner still
/// under the span's first column; one that fits on neither side wraps right of its hook while
/// that leaves 24 columns, and below that starts flush left, wrapped with a hanging indent.
#[test]
fn a_label_hangs_right_left_wrapped_or_flush() {
    let want = "\
error  cannot find `gamma_term`
  ╭─ src/lib.rs:1:24
1 │ let v = alpha + beta + gamma_term;
  │                        ^^^^^^^^^^
  │       an unknown name ─╯
  ╰─ blaze explain resolve:unknown
";
    same("left", &hung("let v = alpha + beta + gamma_term;\n", "an unknown name"), want);

    let want = "\
error  cannot find `gamma_term`
  ╭─ src/lib.rs:1:5
1 │     gamma_term;
  │     ^^^^^^^^^^
  │     ╰─ this name is not in scope in
  │        this function
  ╰─ blaze explain resolve:unknown
";
    same("wrapped", &hung("    gamma_term;\n", "this name is not in scope in this function"), want);

    let want = "\
error  cannot find `gamma_term`
  ╭─ src/lib.rs:1:21
1 │                     gamma_term;
  │                     ^^^^^^^^^^
  │ this name is not in scope in
  │   this function
  ╰─ blaze explain resolve:unknown
";
    same("flush", &hung("                    gamma_term;\n", "this name is not in scope in this function"), want);
}

/// A message whose last row would hold one word: the word before it comes down too.
#[test]
fn a_message_never_ends_on_one_word() {
    let text = "fn f() {\n    use_after(value);\n}\n";
    let moved = finding(Severity::Error, "borrow:moved", "the value is used here after it moves", span(text, "value", "moved before"));
    let want = "\
error  the value is used here after
       it moves
  ╭─ src/f.rs:2:15
1 │ fn f() {
2 │     use_after(value);
  │               ^^^^^
  │               ╰─ moved before
  ╰─ blaze explain borrow:moved
";
    same("orphan", &draw("src/f.rs", "/work/src/f.rs", text, moved, plain(40)), want);
}

/// A fix is a preview either way; only a command that can apply it is named, in the footer and
/// in the closing line's count. Until `--fix` exists the caller says [`Fixes::ByHand`].
#[test]
fn a_fix_names_a_command_only_where_one_applies_it() {
    let text = "fn names(people: &Vec<Person>) -> Vec<String> {\n    let people = people.clone();\n    people.len()\n}\n";
    let clone = span(text, ".clone()", "not needed");
    let fix = [Edit { start: clone.start, end: clone.end, insert: "" }];
    let lint = Finding { fix: &fix, ..finding(Severity::Warning, "lint:clones", "this `.clone()` copies a vector that is only read", clone) };
    let frame = |footer: &str| {
        [
            "warning  this `.clone()` copies a vector that is only read\n",
            "  ╭─ src/names.rs:2:24\n",
            "1 │ fn names(people: &Vec<Person>) -> Vec<String> {\n",
            "2 │     let people = people.clone();\n",
            "  │                        ^^^^^^^^\n",
            "  │                        ╰─ not needed\n",
            "+ │     let people = people[-.clone()-];\n",
            "  ╰─ ", footer, " · blaze explain lint:clones\n",
        ]
        .concat()
    };
    let by_hand = draw_fixes("src/names.rs", "/work/src/names.rs", text, lint, plain(80), Fixes::ByHand);
    same("by hand", &by_hand, &frame("apply by hand"));
    let command = draw_fixes("src/names.rs", "/work/src/names.rs", text, lint, plain(80), Fixes::Command);
    same("by command", &command, &frame("fix: blaze check --fix"));

    let line = |fixes| {
        let counts = Counts { warnings: 1, files: 1, fixable: 1, ..Counts::default() };
        let mut out = AString::new_in(blaze_own::testing::heap().alloc());
        closing(&mut out, &Closing { counts, checked: 1, nanos: 1_090_000, changed: None, fixes, coverage: None, fix_with: "" }, ansi(80));
        out.as_str().to_string()
    };
    same("closing by hand", &line(Fixes::ByHand), &[WARNING, "1 warning", D, " · 1 file checked · 1.1 ms", R, "\n"].concat());
    same(
        "closing by command",
        &line(Fixes::Command),
        &[WARNING, "1 warning", D, " · 1 fixable: blaze check --fix · 1 file checked · 1.1 ms", R, "\n"].concat(),
    );
    assert_eq!(Fixes::default(), Fixes::ByHand, "a caller that says nothing claims no command");
}

/// WCAG 2 contrast of two sRGB colours.
fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let luminance = |rgb: [u8; 3]| {
        let channel = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
    };
    let (x, y) = (luminance(a) + 0.05, luminance(b) + 0.05);
    x.max(y) / x.min(y)
}

/// The literals' tint: on a dark background at least 4.5:1 on #1e1e1e and well below the ink
/// (#d4d4d4); on a light one at least 4.5:1 on #ffffff and well above black ink; unknown, one
/// mid-tone at least 3.5:1 on both. Only a truecolor terminal has a tint.
#[test]
fn the_literals_tint_reads_on_every_background() {
    let (dark, white, ink, black) = ([0x1e; 3], [0xff; 3], [0xd4; 3], [0; 3]);
    let says = |name: &str, value: f64, want: f64| assert!(value >= want, "{name}: {value:.2} < {want}");
    says("dark tint on #1e1e1e", contrast(TINT_DARK, dark), 4.5);
    says("ink over the dark tint", contrast(ink, TINT_DARK), 1.8);
    says("light tint on #ffffff", contrast(TINT_LIGHT, white), 4.5);
    says("the light tint over black ink", contrast(TINT_LIGHT, black), 3.0);
    says("unknown tint on #1e1e1e", contrast(TINT_UNKNOWN, dark), 3.5);
    says("unknown tint on #ffffff", contrast(TINT_UNKNOWN, white), 3.5);
    // The values the report states, to two places.
    let at = |value: f64| (value * 100.0).round() / 100.0;
    assert_eq!(
        [at(contrast(TINT_DARK, dark)), at(contrast(TINT_LIGHT, white)), at(contrast(TINT_UNKNOWN, dark)), at(contrast(TINT_UNKNOWN, white))],
        [5.82, 5.44, 4.49, 3.71]
    );

    let true_with = |background| Caps { background, ..truecolor(80) };
    assert_eq!(tint(true_with(Background::Dark)), Some(TINT_DARK));
    assert_eq!(tint(true_with(Background::Light)), Some(TINT_LIGHT));
    assert_eq!(tint(true_with(Background::Unknown)), Some(TINT_UNKNOWN));
    assert_eq!(tint(Caps { colour: Colour::Ansi256, ..true_with(Background::Dark) }), None);
    assert_eq!(tint(ansi(80)), None);
    assert!(his(true_with(Background::Dark)).contains("\x1b[0;38;2;138;154;176m\"blaze-std\""));
    assert!(his(true_with(Background::Light)).contains("\x1b[0;38;2;91;107;128m\"blaze-std\""));

    assert_eq!(background(Some("15;0")), Background::Dark);
    assert_eq!(background(Some("0;default;15")), Background::Light);
    assert_eq!(background(Some("12;8")), Background::Dark);
    assert_eq!(background(Some("default")), Background::Unknown);
    assert_eq!(background(None), Background::Unknown);
}
