//! The frame, in your terminal:
//! `demo [--caps truecolor|256|16|plain|lines] [--width N] [--bg dark|light|unknown]`.
//!
//! Each fixture finding is drawn as `blaze` draws it. The first two are the owner's own: /Volumes/Projects/tmp/hello-world/src/main.rs, read as it is and
//! never written (the two `?` `blaze run` refuses), and its Cargo.toml as it stood when he met
//! the finding (the dependency this build of blaze cannot read yet). Then a name deep in a
//! function, a span over lines, a label hung to the left, a lint with a fix, a borrow with
//! secondary spans, a warning and a note, and the closing line. `--bg` defaults to what
//! `COLORFGBG` says, as `blaze` reads it.

#![feature(allocator_api)]

use blaze_own::AString;
use blaze_report::Severity;
use blaze_report::frame::words::{Area, say, wording};
use blaze_report::frame::{
    Background, Caps, Closing, Colour, Counts, Edit, Finding, Fixes, Form, Input, LINK_FILE, Mark, Source, background, closing,
    render,
};

const PROJECT: &str = "/Volumes/Projects/tmp/hello-world";

fn at(text: &str, needle: &str, nth: usize) -> u32 {
    let mut from = 0;
    for _ in 0..nth {
        from += text[from..].find(needle).unwrap_or_else(|| panic!("{needle:?} is not in the file")) + needle.len();
    }
    (from + text[from..].find(needle).unwrap_or_else(|| panic!("{needle:?} is not in the file"))) as u32
}

fn span<'l>(text: &str, needle: &str, nth: usize, label: &'l str) -> Mark<'l> {
    let start = at(text, needle, nth);
    Mark { start, end: start + needle.len() as u32, label }
}

fn main() {
    let heap = blaze_own::Heap::process();
    let mut level = String::from("truecolor");
    let mut width: u16 = 100;
    let mut bg = background(std::env::var("COLORFGBG").ok().as_deref());
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--caps" => {
                level = args.next().unwrap_or_default();
                // The old names, kept.
                level = match level.as_str() {
                    "rich" => "truecolor".into(),
                    "ansi" => "16".into(),
                    _ => level,
                };
                if !matches!(level.as_str(), "truecolor" | "256" | "16" | "plain" | "lines") {
                    panic!("--caps takes truecolor, 256, 16, plain or lines, not {level:?}");
                }
            }
            "--width" => width = args.next().and_then(|it| it.parse().ok()).expect("--width takes a number"),
            "--bg" => {
                bg = match args.next().as_deref() {
                    Some("dark") => Background::Dark,
                    Some("light") => Background::Light,
                    Some("unknown") => Background::Unknown,
                    other => panic!("--bg takes dark, light or unknown, not {other:?}"),
                }
            }
            other => panic!("demo [--caps truecolor|256|16|plain|lines] [--width N] [--bg dark|light|unknown], not {other:?}"),
        }
    }
    let caps = match level.as_str() {
        "truecolor" => Caps::RICH,
        "256" => Caps { colour: Colour::Ansi256, ..Caps::ANSI },
        "16" => Caps::ANSI,
        "plain" => Caps::FRAMES,
        _ => Caps::LINES,
    };
    let caps = Caps { width, background: bg, ..caps };

    let main_path = format!("{PROJECT}/src/main.rs");
    let manifest_path = format!("{PROJECT}/Cargo.toml");
    let main_rs = std::fs::read_to_string(&main_path).unwrap_or_else(|error| panic!("{main_path}: {error}"));
    // The manifest as it stood when he ran `blaze check` (the file has since lost the
    // dependency), the same text the spec tests draw.
    let manifest = String::from(
        "[package]\nname = \"hello-world\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n\n[dependencies]\nbstd = { package = \"blaze-std\", path = \"/Volumes/Projects/blaze-macbuild-1112/crates/blaze-std\" }\n",
    );

    let lint = "fn names(people: &Vec<Person>) -> Vec<String> {\n    let people = people.clone();\n    people.iter().map(|p| p.name.to_string()).collect()\n}\n";
    let borrow = "fn last(mut v: Vec<u8>) -> u8 {\n    let first = &v[0];\n    v.push(1);\n    *first\n}\n";
    let ledger = "impl Ledger {\n    /// Adds every row.\n    pub fn total(&self, rows: &[Row]) -> u64 {\n        let mut sum = 0;\n        for row in rows {\n            sum += row.value * undefined_rate;\n        }\n        sum\n    }\n}\n";
    let run = "fn run() {\n    let task = spawn(async {\n        let a = fetch().await;\n        let b = parse(a);\n    });\n    task.wait();\n}\n";
    let area = "fn area(w: u32, h: u32) -> u32 {\n    let scale = 2;\n    w * h * scaling\n}\n";
    let mut long = String::from("/// Folds every row into the total.\npub fn fold(rows: &[Row]) -> Total {\n    let mut total = Total::default();\n");
    for i in 0..24 {
        long.push_str(&format!("    total.add(rows[{i}].value, rows[{i}].weight);\n"));
    }
    long.push_str("    total.finish(rows.len() as u64, \"rows folded into one total, ready for the report\")\n}\n");

    let sources = [
        Source { shown: "src/main.rs", absolute: &main_path, text: main_rs.as_bytes() },
        Source { shown: "Cargo.toml", absolute: &manifest_path, text: manifest.as_bytes() },
        Source { shown: "src/ledger.rs", absolute: "/tmp/demo/src/ledger.rs", text: ledger.as_bytes() },
        Source { shown: "src/run.rs", absolute: "/tmp/demo/src/run.rs", text: run.as_bytes() },
        Source { shown: "src/fold.rs", absolute: "/tmp/demo/src/fold.rs", text: long.as_bytes() },
        Source { shown: "src/names.rs", absolute: "/tmp/demo/src/names.rs", text: lint.as_bytes() },
        Source { shown: "src/last.rs", absolute: "/tmp/demo/src/last.rs", text: borrow.as_bytes() },
        Source { shown: "src/area.rs", absolute: "/tmp/demo/src/area.rs", text: area.as_bytes() },
    ];

    // The owner's `?`: worded by the table, as the CLI will.
    let refused = wording(Area::Ladder, 1).expect("ladder rule 1 has words");
    let question = "the `?` operator is not supported by this build of blaze yet";
    let first = Finding {
        severity: Severity::Error,
        rule: refused.name,
        message: question,
        file: 0,
        item: Some(0),
        primary: span(&main_rs, "?", 0, refused.label),
        secondary: &[],
        fix: &[],
    };
    let second = Finding { primary: span(&main_rs, "?", 1, refused.label), ..first };

    let mut headline = AString::new_in(heap.alloc());
    let mut label = AString::new_in(heap.alloc());
    let name = say(Area::Cli, 2, &["bstd"], &mut headline, &mut label).expect("cli rule 2 has words");
    let dependency = Finding {
        severity: Severity::Error,
        rule: name,
        message: headline.as_str(),
        file: 1,
        item: None,
        primary: span(&manifest, "bstd", 0, label.as_str()),
        secondary: &[],
        fix: &[],
    };

    let unknown = Finding {
        severity: Severity::Error,
        rule: "resolve:unknown",
        message: "cannot find `undefined_rate` in this scope",
        file: 2,
        item: Some(0),
        primary: span(ledger, "undefined_rate", 0, "not a name here"),
        secondary: &[],
        fix: &[],
    };

    let block_start = at(run, "async {", 0);
    let block = Finding {
        severity: Severity::Error,
        rule: "ladder:async",
        message: "an `async` block is not supported by this build of blaze yet",
        file: 3,
        item: Some(0),
        primary: Mark { start: block_start, end: at(run, "    });", 0) + 5, label: "this block" },
        secondary: &[],
        fix: &[],
    };

    let cast = Finding {
        severity: Severity::Warning,
        rule: "lint:casts",
        message: "a bare `as` cast to an integer",
        file: 4,
        item: Some(0),
        primary: span(&long, "rows.len() as u64", 0, "use `u64::try_from` and say what happens past 2^64 rows"),
        secondary: &[],
        fix: &[],
    };

    let clone_at = at(lint, ".clone()", 0);
    let fix = [Edit { start: clone_at, end: clone_at + 8, insert: "" }];
    let clone = Finding {
        severity: Severity::Warning,
        rule: "lint:clones",
        message: "this `.clone()` copies a vector that is only read",
        file: 5,
        item: Some(0),
        primary: Mark { start: clone_at, end: clone_at + 8, label: "not needed" },
        secondary: &[],
        fix: &fix,
    };

    let others = [span(borrow, "&v[0]", 0, "borrowed here"), span(borrow, "*first", 0, "and used here")];
    let shared = Finding {
        severity: Severity::Error,
        rule: "borrow:mut-while-shared",
        message: "`v` is changed while `first` still borrows it",
        file: 6,
        item: Some(0),
        primary: span(borrow, "v.push(1)", 0, "changed here"),
        secondary: &others,
        fix: &[],
    };

    let advice = Finding {
        severity: Severity::Note,
        rule: "resolve:unknown",
        message: "cannot find `scaling` in this scope",
        file: 7,
        item: Some(0),
        primary: span(area, "scaling", 0, "did you mean `scale`?"),
        secondary: &[],
        fix: &[],
    };

    let fixtures: [&[Finding<'_>]; 9] = [
        &[first, second],
        &[dependency],
        &[unknown],
        &[block],
        &[cast],
        &[clone],
        &[shared],
        &[Finding { severity: Severity::Warning, ..advice }],
        &[advice],
    ];
    let mut out = AString::new_in(heap.alloc());
    let mut counts = Counts { files: 8, ..Counts::default() };
    for findings in fixtures {
        render(&mut out, &Input { findings, sources: &sources, link: LINK_FILE, fixes: Fixes::ByHand, ids: &[] }, caps, heap.alloc())
            .expect("every range is in its file");
        out.push_str("\n");
        for finding in findings {
            counts.add(finding.severity, !finding.fix.is_empty());
        }
    }
    if caps.form == Form::Frames {
        out.push_str("── the closing line ──\n");
    }
    closing(&mut out, &Closing { counts, checked: 8, nanos: 2_400_000, changed: Some(0), fixes: Fixes::ByHand, coverage: None, fix_with: "" }, caps);
    print!("{}", out.as_str());
}
