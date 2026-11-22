//! The frame's goldens (exact bytes for each ability level, one file each under `golden/`;
//! `BLAZE_BLESS=1` writes them), the wording table's own check, and its properties on
//! blaze-sim with a planted-bug control.

use blaze_own::{AString};
use blaze_sim::{Broke, Config, Tape};

use super::text::Plant;
use super::words::{Area, WORDINGS, fill, say, wording};
use super::{Background, Caps, Closing, Colour, Counts, Edit, Finding, Fixes, Form, Input, LINK_FILE, Mark, Source, closing, render_planted};
use crate::Severity;

/// The owner's program, as it stood when he ran `blaze run` (his file is not read here).
const HELLO: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {\n    io.need_out()?.line(cx, \"Hello, world!\")?;\n    Ok(())\n}\n";

/// The owner's manifest, likewise.
const MANIFEST: &str = "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nbstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n";

/// The byte where the `nth` (from 0) `needle` starts in `text`.
fn at(text: &str, needle: &str, nth: usize) -> u32 {
    let mut from = 0;
    for _ in 0..nth {
        from += text[from..].find(needle).expect("the needle is in the case's text") + needle.len();
    }
    (from + text[from..].find(needle).expect("the needle is in the case's text")) as u32
}

fn mark(text: &str, needle: &str, nth: usize, label: &'static str) -> Mark<'static> {
    let start = at(text, needle, nth);
    Mark { start, end: start + needle.len() as u32, label }
}

fn leak<T>(items: Vec<T>) -> &'static [T] {
    Vec::leak(items)
}

fn finding(severity: Severity, rule: &'static str, message: &'static str, primary: Mark<'static>) -> Finding<'static> {
    Finding { severity, rule, message, file: 0, item: None, primary, secondary: &[], fix: &[] }
}

/// A case: a file and its findings.
struct Case {
    name: &'static str,
    path: &'static str,
    text: &'static [u8],
    findings: Vec<Finding<'static>>,
}

fn source(text: &str) -> &'static str {
    String::leak(text.to_string())
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();

    // The owner's `?`: two refusals on one line, worded by the table.
    let refusal = "the `?` operator is not supported by this build of blaze yet";
    let label = wording(Area::Ladder, 1).map_or("", |it| it.label);
    let mut question = finding(Severity::Error, "not-supported-yet", refusal, mark(HELLO, "?", 0, label));
    question.item = Some(0);
    let mut second = question;
    second.primary = mark(HELLO, "?", 1, label);
    cases.push(Case { name: "question", path: "src/main.rs", text: HELLO.as_bytes(), findings: vec![question, second] });

    // The owner's manifest: the dependency blaze cannot read yet.
    let mut headline = AString::new_in(blaze_own::testing::heap().alloc());
    let mut label = AString::new_in(blaze_own::testing::heap().alloc());
    let name = say(Area::Cli, 2, &["bstd"], &mut headline, &mut label).expect("rule 2 has words");
    let headline = source(&headline);
    let label = source(&label);
    let dependency = finding(Severity::Error, name, headline, mark(MANIFEST, "bstd", 0, label));
    cases.push(Case { name: "manifest", path: "Cargo.toml", text: MANIFEST.as_bytes(), findings: vec![dependency] });

    let token = source("fn total(items: &[u32]) -> u32 {\n    let sum = items.iter().sum::<u32>();\n    sum + undefined_name\n}\n");
    let mut one = finding(Severity::Error, "resolve:unknown", "cannot find `undefined_name` here", mark(token, "undefined_name", 0, "not in scope"));
    one.item = Some(0);
    cases.push(Case { name: "token", path: "src/total.rs", text: token.as_bytes(), findings: vec![one] });

    let eol = source("fn main() {\n    let answer = compute(1, 2) // and the rest of this line\n    println!(\"{answer}\");\n}\n");
    let start = at(eol, "// and", 0);
    let end = at(eol, "\n    println", 0);
    let mut to_eol = finding(Severity::Warning, "syntax:semicolon", "a `;` is missing after this statement", Mark { start: at(eol, "compute", 0), end, label: "add `;` here" });
    to_eol.primary.start = start.min(to_eol.primary.start);
    to_eol.item = Some(0);
    cases.push(Case { name: "to-eol", path: "src/main.rs", text: eol.as_bytes(), findings: vec![to_eol] });

    let multi = source("fn run() {\n    let task = spawn(async {\n        let a = fetch().await;\n        let b = parse(a);\n        let c = check(b);\n        let d = store(c);\n        d\n    });\n    task.wait();\n}\n");
    let start = at(multi, "async {", 0);
    let end = at(multi, "    })", 0) + 5;
    let mut many = finding(Severity::Error, "ladder:async", "an `async` block is not supported by this build of blaze yet", Mark { start, end, label: "this block" });
    many.item = Some(0);
    cases.push(Case { name: "multiline", path: "src/run.rs", text: multi.as_bytes(), findings: vec![many] });

    let two = source("fn pick(v: Vec<u8>) -> u8 {\n    let first = v[0] as u32 + v.len() as u32;\n    first as u8\n}\n");
    let mut cast = finding(Severity::Warning, "lint:casts", "a bare `as` cast to an integer", mark(two, "v[0] as u32", 0, "use `u32::from`"));
    cast.item = Some(0);
    let mut len = finding(Severity::Error, "lint:truncate", "`v.len() as u32` can lose bits", mark(two, "v.len() as u32", 0, "truncates past 4 GiB"));
    len.item = Some(0);
    cases.push(Case { name: "two-on-line", path: "src/pick.rs", text: two.as_bytes(), findings: vec![cast, len] });

    let mut long_fn = String::from("/// Folds the rows.\npub fn fold(rows: &[Row]) -> Total {\n    let mut total = Total::default();\n");
    for i in 0..30 {
        long_fn.push_str(&format!("    total.add(rows[{i}].value);\n"));
    }
    long_fn.push_str("    total.finish(rows.len() as u64)\n}\n");
    let long_fn = source(&long_fn);
    let mut deep = finding(Severity::Error, "lint:casts", "a bare `as` cast to an integer", mark(long_fn, "rows.len() as u64", 0, "use `u64::try_from`"));
    deep.item = Some(0);
    cases.push(Case { name: "deep", path: "src/fold.rs", text: long_fn.as_bytes(), findings: vec![deep] });

    let mut wide_line = String::from("const TABLE: [u32; 64] = [");
    for i in 0..40 {
        wide_line.push_str(&format!("{}, ", i * 1_000_003));
    }
    wide_line.push_str("oops, 7];\n");
    let wide_line = source(&wide_line);
    let windowed = finding(Severity::Error, "resolve:unknown", "cannot find `oops` here", mark(wide_line, "oops", 0, "not a number"));
    cases.push(Case { name: "long-line", path: "src/table.rs", text: wide_line.as_bytes(), findings: vec![windowed] });

    let wide = source("fn greet() {\n\tlet 名前 = \"世界🌍\";\tlet n = 名前.len() as u8;\n}\n");
    let mut tabs = finding(Severity::Warning, "lint:casts", "a bare `as` cast to an integer", mark(wide, "名前.len() as u8", 0, "truncates"));
    tabs.item = Some(0);
    cases.push(Case { name: "wide", path: "src/greet.rs", text: wide.as_bytes(), findings: vec![tabs] });

    let fixed = source("fn copy(names: &Vec<String>) -> usize {\n    let names = names.clone();\n    names.len()\n}\n");
    let clone_at = at(fixed, ".clone()", 0);
    let edits = leak(vec![Edit { start: clone_at, end: clone_at + 8, insert: "" }, ]);
    let mut fix = finding(Severity::Warning, "lint:clones", "this `.clone()` copies a vector only read", Mark { start: clone_at, end: clone_at + 8, label: "not needed" });
    fix.item = Some(0);
    fix.fix = edits;
    let vec_at = at(fixed, "&Vec<String>", 0);
    let edits = leak(vec![Edit { start: vec_at, end: vec_at + 12, insert: "&[String]" }]);
    let mut slice = finding(Severity::Note, "lint:slices", "take a slice: `&[String]`", Mark { start: vec_at, end: vec_at + 12, label: "" });
    slice.item = Some(0);
    slice.fix = edits;
    cases.push(Case { name: "fix", path: "src/copy.rs", text: fixed.as_bytes(), findings: vec![fix, slice] });

    let borrow = source("fn last(mut v: Vec<u8>) -> u8 {\n    let first = &v[0];\n    v.push(1);\n    *first\n}\n");
    let mut moved = finding(Severity::Error, "borrow:mut-while-shared", "`v` is changed while `first` still borrows it", mark(borrow, "v.push(1)", 0, "changed here"));
    moved.item = Some(0);
    moved.secondary = leak(vec![mark(borrow, "&v[0]", 0, "borrowed here"), mark(borrow, "*first", 0, "and used here")]);
    cases.push(Case { name: "secondary", path: "src/last.rs", text: borrow.as_bytes(), findings: vec![moved] });

    // A hostile file: a title set, the screen cleared, a forged link, a right-to-left override,
    // a byte that is not UTF-8, and the same in the message, the label and the path.
    let mut hostile: Vec<u8> = b"fn evil() {\n    let s = \"\x1b]0;pwned\x07\x1b[2J\";\n    let t = \"\x1b]8;;http://evil.example\x1b\\\\click\x1b]8;;\x1b\\\\\"; // ".to_vec();
    hostile.extend_from_slice("\u{202e}desrever\n    let u = b\"".as_bytes());
    hostile.extend_from_slice(b"\xff\xfe\";\n}\n");
    let hostile = Vec::leak(hostile);
    let link_at = hostile.windows(4).position(|w| w == b"\x1b]8;").expect("the link is in the hostile file") as u32;
    let mut bad = finding(
        Severity::Error,
        "evil\x1b[31m",
        "a message with \x1b]0;title\x07 and \x1b[2J in it",
        Mark { start: link_at, end: link_at + 12, label: "label \x1b[5m blinks" },
    );
    bad.item = Some(0);
    cases.push(Case { name: "hostile", path: "src/\x1b[2Jevil.rs", text: hostile, findings: vec![bad] });

    cases
}

fn levels() -> [(&'static str, Caps); 4] {
    [
        ("plain", Caps { width: 80, ..Caps::FRAMES }),
        ("ansi", Caps { width: 80, ..Caps::ANSI }),
        ("rich", Caps { width: 100, ..Caps::RICH }),
        ("narrow", Caps { width: 40, unicode: false, ..Caps::ANSI }),
    ]
}

fn draw(case: &Case, caps: Caps, plant: Plant) -> String {
    let sources = [Source { shown: case.path, absolute: "/work/hello world/src/main.rs", text: case.text }];
    // The goldens draw the day `--fix` exists; the spec tests draw both states.
    let input = Input { findings: &case.findings, sources: &sources, link: LINK_FILE, fixes: Fixes::Command, ids: &[] };
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    render_planted(&mut out, &input, caps, blaze_own::testing::heap().alloc(), plant).expect("the case's ranges are in its file");
    out.as_str().to_string()
}

fn golden(name: &str, got: &str) {
    let path = format!("{}/src/frame/golden/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("BLAZE_BLESS").is_some() {
        std::fs::create_dir_all(format!("{}/src/frame/golden", env!("CARGO_MANIFEST_DIR"))).expect("the golden directory is made");
        std::fs::write(&path, got).expect("the golden is written");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error} (BLAZE_BLESS=1 writes it)"));
    assert!(want == got, "{name}: the frame differs from {path} (BLAZE_BLESS=1 rewrites it):\n--- want\n{want}\n--- got\n{got}");
}

#[test]
fn goldens() {
    for case in cases() {
        for (level, caps) in levels() {
            golden(&format!("{}.{level}", case.name), &draw(&case, caps, Plant::Clean));
        }
    }
    // The pipe's form: every case's findings, one line each.
    let mut lines = String::new();
    for case in cases() {
        lines.push_str(&draw(&case, Caps::LINES, Plant::Clean));
    }
    golden("lines", &lines);
    let mut ends = String::new();
    let failed = Closing { counts: Counts { errors: 1, files: 1, ..Counts::default() }, checked: 1, nanos: 1_090_000, changed: None, fixes: Fixes::ByHand, coverage: None, fix_with: "" };
    let clean = Closing { counts: Counts::default(), checked: 14, nanos: 480_000, changed: Some(0), fixes: Fixes::ByHand, coverage: None, fix_with: "" };
    let mixed = Closing {
        coverage: None,
        counts: Counts { failures: 0, errors: 2, warnings: 3, notes: 0, files: 2, fixable: 1 },
        checked: 213,
        nanos: 2_430_000_000,
        changed: Some(12),
        fixes: Fixes::Command,
        fix_with: "",
    };
    for (_, caps) in levels().into_iter().chain([("lines", Caps::LINES)]) {
        for end in [failed, clean, mixed] {
            let mut out = AString::new_in(blaze_own::testing::heap().alloc());
            closing(&mut out, &end, caps);
            ends.push_str(out.as_str());
        }
    }
    golden("closing", &ends);
}

/// Every rule the tree emits has a headline and a label, and its templates take exactly the
/// arguments it says. (blaze-cli's test lists the numbers the tree emits against this table.)
#[test]
fn every_wording_is_whole() {
    for wording in WORDINGS {
        assert!(!wording.headline.is_empty() && !wording.label.is_empty(), "{}: empty words", wording.name);
        let args: Vec<String> = (0..wording.args).map(|n| format!("<{n}>")).collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut both = AString::new_in(blaze_own::testing::heap().alloc());
        fill(&mut both, wording.headline, &args);
        fill(&mut both, wording.label, &args);
        assert!(!both.contains('{'), "{}: a placeholder past its arguments", wording.name);
        for n in 0..wording.args {
            assert!(both.contains(&format!("<{n}>")), "{}: argument {n} is never said", wording.name);
        }
        let same = WORDINGS.iter().filter(|it| it.area == wording.area && it.number == wording.number).count();
        assert_eq!(same, 1, "{}: two rows for one rule", wording.name);
    }
}

/// Escapes stripped: what a person reads.
fn strip(text: &str) -> String {
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match bytes.get(i + 1) {
                Some(b'[') => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    i += 2;
                    while i < bytes.len() && !(bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\')) {
                        i += 1;
                    }
                    i += 2;
                }
                _ => i += 1,
            }
            continue;
        }
        let len = text[i..].chars().next().map_or(1, char::len_utf8);
        out.push_str(&text[i..i + len]);
        i += len;
    }
    out
}

#[test]
fn hostile_text_never_reaches_the_terminal() {
    let case = cases().into_iter().find(|case| case.name == "hostile").expect("the hostile case");
    for (level, caps) in levels() {
        let out = draw(&case, caps, Plant::Clean);
        // Our own escapes are SGR (`ESC [ .. m`) and, with links on, OSC 8 with our URL; no
        // other escape, no BEL, no clear.
        let bytes = out.as_bytes();
        for (i, &byte) in bytes.iter().enumerate() {
            assert!(byte >= 0x20 || byte == b'\n' || byte == 0x1b, "{level}: control byte {byte:#x} at {i}");
            if byte == 0x1b {
                let rest = &out[i + 1..];
                let ours = rest.starts_with("[0") && rest[1..].find('m').is_some_and(|m| rest[1..1 + m].bytes().all(|b| b.is_ascii_digit() || b == b';' || b == b':'))
                    || rest.starts_with("]8;;file:///work/hello%20world/") || rest.starts_with("]8;;\x1b\\") || rest.starts_with('\\');
                assert!(ours, "{level}: an escape not the renderer's own at {i}: {:?}", &rest[..rest.len().min(20)]);
            }
        }
        assert!(!strip(&out).contains('\u{202e}'), "{level}: the override reached the terminal");
    }
}

/// A random file and findings in it, for the properties.
struct Drawn {
    text: String,
    findings: Vec<Finding<'static>>,
    caps: Caps,
}

const ALPHABET: &str = "abcxyz_ (){};.=+&:\"'#!\t名前🌍é\u{301}\u{1b}\u{7}\u{202e}0123";

fn draw_case(tape: &Tape, with_fix: bool) -> Drawn {
    let lines = 1 + tape.index(12, "lines");
    let mut text = String::new();
    for _ in 0..lines {
        text.push_str(&tape.text_from(ALPHABET, 0..=40, "line"));
        text.push('\n');
    }
    // The token the planted bug's oracle finds: after the line's other text, once.
    let qq_line = tape.index(lines, "qq line");
    let mut with_qq = String::new();
    for (n, line) in text.split_inclusive('\n').enumerate() {
        if n == qq_line {
            with_qq.push_str(line.trim_end_matches('\n').replace("QQ", "").as_str());
            with_qq.push_str(" QQ\n");
        } else {
            with_qq.push_str(line);
        }
    }
    let text = with_qq;
    let qq = text.find("QQ").expect("QQ was placed") as u32;
    let len = text.len() as u32;
    let boundary = |tape: &Tape, label: &'static str| {
        let mut at = tape.below(u64::from(len) + 1, label) as u32;
        while !text.is_char_boundary(at as usize) {
            at -= 1;
        }
        at
    };
    let mut findings = Vec::new();
    findings.push(Finding {
        severity: Severity::Error,
        rule: "test:qq",
        message: "the `QQ` token",
        file: 0,
        item: None,
        primary: Mark { start: qq, end: qq + 2, label: "here" },
        secondary: &[],
        fix: &[],
    });
    let others = tape.index(4, "findings");
    for _ in 0..others {
        let a = boundary(tape, "start");
        let b = boundary(tape, "end");
        let (start, end) = (a.min(b), a.max(b));
        let severity = *tape.one_of(&[Severity::Error, Severity::Warning, Severity::Note], "severity");
        let label = *tape.one_of(&["", "x", "a longer label with `code`", "label \u{1b}[2J"], "label");
        let fix: &'static [Edit<'static>] = if with_fix && tape.coin("fix") {
            leak(vec![Edit { start, end, insert: "fixed" }])
        } else {
            &[]
        };
        let secondary: &'static [Mark<'static>] = if tape.coin("secondary") {
            let a = boundary(tape, "secondary");
            leak(vec![Mark { start: a, end: a, label: "other" }])
        } else {
            &[]
        };
        let item = tape.coin("item").then_some(0);
        findings.push(Finding { severity, rule: "test:any", message: "words and `code` and more words to wrap", file: 0, item, primary: Mark { start, end, label }, secondary, fix });
    }
    let colour = *tape.one_of(&[Colour::None, Colour::Ansi16, Colour::Ansi256, Colour::True], "colour");
    let curly = colour != Colour::None && tape.coin("curly");
    let caps = Caps {
        form: Form::Frames,
        colour,
        curly,
        underline_colour: curly,
        hyperlinks: curly && tape.coin("links"),
        unicode: tape.coin("unicode"),
        width: 20 + tape.below(181, "width") as u16,
        background: *tape.one_of(&[Background::Unknown, Background::Dark, Background::Light], "background"),
    };
    Drawn { text, findings, caps }
}

fn render_drawn(drawn: &Drawn, caps: Caps, plant: Plant) -> String {
    let text = String::leak(drawn.text.clone());
    let case = Case { name: "drawn", path: "src/a.rs", text: text.as_bytes(), findings: drawn.findings.clone() };
    draw(&case, caps, plant)
}

fn properties(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let drawn = draw_case(tape, true);
    let out = render_drawn(&drawn, drawn.caps, plant);
    let width = u32::from(drawn.caps.width);
    for line in strip(&out).lines() {
        let w = blaze_unicode::str_width(line) as u32;
        if w > width {
            return Err(Broke::Oracle(format!("a line {w} columns wide in {width}: {line:?}")));
        }
    }
    let plain = render_drawn(&drawn, Caps { colour: Colour::None, curly: false, underline_colour: false, hyperlinks: false, ..drawn.caps }, plant);
    if let Some(at) = plain.bytes().position(|b| b < 0x20 && b != b'\n') {
        return Err(Broke::Oracle(format!("a control byte with colour off at {at}")));
    }
    // With colour and `^` lines, what a person reads is the plain frame, but for a fix's
    // `[-` `-]` `{+` `+}` (colour shows those).
    if !drawn.caps.curly {
        let fixless: Vec<Finding<'static>> = drawn.findings.iter().map(|f| Finding { fix: &[], ..*f }).collect();
        let bare = Drawn { text: drawn.text.clone(), findings: fixless, caps: drawn.caps };
        let coloured = strip(&render_drawn(&bare, Caps { colour: Colour::Ansi16, ..bare.caps }, plant));
        let none = render_drawn(&bare, Caps { colour: Colour::None, ..bare.caps }, plant);
        if coloured != none {
            return Err(Broke::Oracle(format!("colour changed the text:\n{coloured}\n---\n{none}")));
        }
    }
    // The `^` under `QQ` starts where `QQ` does on the screen.
    let carets = render_drawn(&drawn, Caps { colour: Colour::None, curly: false, underline_colour: false, hyperlinks: false, ..drawn.caps }, plant);
    let lines: Vec<&str> = carets.lines().collect();
    for pair in lines.windows(2) {
        let (code, under) = (pair[0], pair[1]);
        let (Some(q), Some(c)) = (code.find(" QQ"), under.find('^')) else { continue };
        let q = blaze_unicode::str_width(&code[..q + 1]);
        let c = blaze_unicode::str_width(&under[..c]);
        if under[..under.find('^').unwrap_or(0)].contains(['-', '^']) || q == c {
            continue;
        }
        // Only a mark of QQ's alone: another span's carets may start first on this line.
        if drawn.findings.len() == 1 {
            return Err(Broke::Oracle(format!("caret column {c} under `QQ` at column {q}:\n{code}\n{under}")));
        }
    }
    emphasis(&out, drawn.caps)
}

/// What a printed character looks like: the SGR state it was drawn in.
#[derive(Clone, Copy, Default, Debug)]
struct Look {
    bold: bool,
    /// The SGR foreground code (31, 32, ..), or 38 for a 24-bit colour.
    fg: Option<u32>,
    rgb: Option<[u32; 3]>,
    curly: bool,
    under_colour: Option<u32>,
}

/// `out`'s rows: each character with its look and its display column. Every SGR the renderer
/// writes starts with `0`, a reset; OSC 8 links are passed over.
fn looks(out: &str) -> Vec<Vec<(char, Look, usize)>> {
    let mut rows = vec![Vec::new()];
    let mut look = Look::default();
    let mut col = 0;
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                let mut params = String::new();
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                    params.push(c);
                }
                let parts: Vec<&str> = params.split(';').collect();
                let mut at = 0;
                while at < parts.len() {
                    match parts[at] {
                        "0" => look = Look::default(),
                        "1" => look.bold = true,
                        "4:3" => look.curly = true,
                        "38" => {
                            let n = |k: usize| parts.get(at + k).and_then(|it| it.parse().ok()).unwrap_or(0);
                            look.fg = Some(38);
                            look.rgb = Some([n(2), n(3), n(4)]);
                            at += 4;
                        }
                        "58" => {
                            look.under_colour = parts.get(at + 2).and_then(|it| it.parse().ok());
                            at += 2;
                        }
                        code => {
                            if let Ok(code @ (30..=37 | 90..=97)) = code.parse::<u32>() {
                                look.fg = Some(code);
                            }
                        }
                    }
                    at += 1;
                }
            }
            '\x1b' if chars.peek() == Some(&']') => {
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '\x1b' && c == '\\' {
                        break;
                    }
                    last = c;
                }
            }
            '\n' => {
                rows.push(Vec::new());
                col = 0;
            }
            c => {
                let rows_len = rows.len();
                rows[rows_len - 1].push((c, look, col));
                col += blaze_unicode::char_width(c).unwrap_or(1);
            }
        }
    }
    rows
}

/// The emphasis budget (rule A and B of the frame's design): the severity's colour only on the
/// severity word, the underline (curly, or the `^` row) and the label; bold only on the
/// severity word, the span, the message's code and nothing else; no hue on code but the
/// literals' tint and a fix's green.
fn emphasis(out: &str, caps: Caps) -> Result<(), Broke> {
    let severe = |code: Option<u32>| matches!(code, Some(31 | 33 | 36 | 91 | 93 | 96));
    let rows = looks(out);
    for (index, row) in rows.iter().enumerate() {
        let text: String = row.iter().map(|it| it.0).collect();
        let body = text.trim_start_matches(' ');
        let lead = text.len() - body.len();
        let digits = body.trim_start_matches(|c: char| c.is_ascii_digit() || c == '+');
        let bar = |s: &str| s.starts_with('│') || s.starts_with('|');
        let code = digits.len() < body.len() && digits.strip_prefix(' ').is_some_and(bar);
        let under = bar(body);
        let frame_row = ["╭─", "╰─", ",-", "`-"].iter().any(|it| body.starts_with(it)) || body == "⋮" || body == ":";
        let headline = !code && !under && !frame_row;
        let word = if lead == 0 && headline { body.find(' ').unwrap_or(body.len()) } else { 0 };
        let mut ticks = 0;
        for (at, &(c, look, col)) in row.iter().enumerate() {
            if c == '`' {
                ticks += 1;
            }
            // A blank, or a mark of no width (a combining accent alone): nothing to see in any look.
            if c == ' ' || blaze_unicode::char_width(c) == Some(0) {
                continue;
            }
            let in_word = at < word;
            let say = |what: &str| Broke::Oracle(format!("emphasis: {what} at {c:?} (column {col}) of row {index}: {text:?}"));
            if severe(look.fg) && !(in_word || under) {
                return Err(say("the severity's colour off its three places"));
            }
            if code && look.fg.is_some_and(|fg| fg != 32 && fg != 38) {
                return Err(say("a hue on code"));
            }
            if look.rgb.is_some_and(|rgb| Some(rgb) != super::tint(caps).map(|it| it.map(u32::from))) {
                return Err(say("a 24-bit colour that is not the literals' tint"));
            }
            if look.under_colour.is_some() && !(code && look.bold && look.curly) {
                return Err(say("an underline colour off the span"));
            }
            if look.bold {
                let span = code
                    && if caps.curly && caps.colour != Colour::None {
                        look.curly && look.under_colour.is_some()
                    } else {
                        rows.get(index + 1).is_some_and(|next| next.iter().any(|it| it.2 == col && it.0 == '^'))
                    };
                let fragment = headline && ticks % 2 == 1 && c != '`';
                if !(in_word || span || fragment) {
                    return Err(say("bold off the severity word, the span and the message's code"));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn frames_keep_their_promises() {
    blaze_sim::check(Config::cases(400), |tape: &Tape| properties(tape, Plant::Clean));
}

#[test]
fn a_loud_name_breaks_the_emphasis_budget_and_is_caught() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::LoudNames, "emphasis:", |tape, plant| {
        properties(tape, plant)
    });
}

#[test]
fn a_mark_off_by_one_after_a_wide_character_is_caught() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::WideOffByOne, "caret column", |tape, plant| {
        properties(tape, plant)
    });
}

/// The owner's case (2026-11-16): the fix for `c` is `b`, the parameter nobody used; the help
/// says why and draws the line before and after, from the fix's data (the hole's range and the
/// bytes of `b`'s declaration). Plain: no escapes.
#[test]
fn the_owners_case_says_why_b_and_draws_the_line_before_and_after() {
    use super::help;
    use super::words::{HelpKind, help_words};
    let text = "fn add(a: u32, b: u32) -> u32 { a + c }\n";
    let hole = at(text, "c }", 0);
    let b = at(text, "b:", 0) as usize;
    let mut words = AString::new_in(blaze_own::testing::heap().alloc());
    help_words(&mut words, HelpKind::NeverUsed, &["b", "u32"]).expect("two arguments");
    let fix = Edit { start: hole, end: hole + 1, insert: &text[b..b + 1] };
    let mut out = AString::new_in(blaze_own::testing::heap().alloc());
    help(&mut out, words.as_str(), text.as_bytes(), Some(fix), Caps { width: 80, ..Caps::FRAMES }).expect("the fix is on one line");
    let want = "help: `b` is in scope, is a `u32`, and is never used\n\
                1 - fn add(a: u32, b: u32) -> u32 { a + c }\n\
                1 + fn add(a: u32, b: u32) -> u32 { a + b }\n";
    assert_eq!(out.as_str(), want, "\n{}", out.as_str());
}
