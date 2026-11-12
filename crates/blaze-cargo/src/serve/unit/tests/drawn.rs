//! What a unit said is kept in its record as what it says and drawn for each client
//! (`serve::unit::drawn`): a build that differs from the last only in how its client is shown
//! diagnostics finds every record, and prints what a build from nothing prints for it.
//!
//! In process, with no daemon: the record's key is the unit's real one
//! ([`compile::RecordInputs::key`], of a build script lowered over files in memory), the
//! record is encoded and read as the daemon's store keeps it ([`compile::record_of`],
//! [`compile::record_said`]), filed and found by what its code read (`reads`), and drawn by
//! the one function the daemon draws with ([`drawn::draw`]). What is the test's own is the
//! glue between them, which in the daemon is `compile::names` and `compile::store` and needs a
//! whole job (a database, a view, the store's reader): the revision case
//! `a_terminal_of_another_width_remakes_nothing` holds that glue to the same property with a
//! real daemon.

use blaze_api::{Diagnostic, Level};
use blaze_diagnostics::Diag;

use super::{SCRIPT, ScriptFront, compile, script_front};
use crate::serve::unit::{
    drawn::{self, Drawing, Item, Said},
    reads,
};

/// The file the unit's diagnostics point into: a line of 219 columns, which a terminal of 80
/// shows a window of and one of 200 nearly whole, and two short ones.
const TEXT: &str = concat!(
    "fn main() {\n",
    "    let first = 1; let second = 2; let third = 3; let fourth = 4; let fifth = 5; let sixth = 6; ",
    "let seventh = 7; let eighth = 8; let ninth = 9; let tenth = 10; let eleventh = 11; let unused_at_the_far_end = 12; ",
    "let after = 13;\n",
    "    missing();\n",
    "}\n",
);

/// Unit 1 as the API names it (`Job::unit_of`).
fn unit_of(unit: &blaze_units::schema::Unit) -> blaze_api::UnitOf {
    blaze_api::UnitOf {
        id: 1,
        test: false,
        doctest: false,
        package_id: unit.pkg_id.clone(),
        manifest_path: "/w/Cargo.toml".to_owned(),
        target: super::unit_target(unit).unwrap(),
    }
}

fn read(name: &str) -> Option<String> {
    (name == "build.rs").then(|| TEXT.to_owned())
}

fn span(of: &str) -> blaze_diagnostics::Span {
    let lo = TEXT.find(of).unwrap_or_else(|| panic!("no `{of}` in the text")) as u32;
    blaze_diagnostics::Span {
        file: "build.rs".to_owned(),
        lo,
        hi: lo + of.len() as u32,
        expansion: None,
    }
}

/// What the unit's compile says: a warning far along the long line, with a label just before
/// it, a note and a suggestion; an error with a code; a line of blaze's own; and
/// the refusal of twenty functions.
fn compiled() -> Said {
    let mut warning = Diag::error(
        None,
        "unused variable: `unused_at_the_far_end`".to_owned(),
        span("unused_at_the_far_end"),
    );
    warning.level = blaze_diagnostics::model::Level::Warning;
    warning.span.labels.push((span("let eleventh = 11;"), "the one before it".to_owned()));
    warning.note("`#[warn(unused_variables)]` on by default");
    warning.suggestions.push(blaze_diagnostics::model::Suggestion {
        message: "if this is intentional, prefix it with an underscore".to_owned(),
        substitutions: vec![vec![(
            span("unused_at_the_far_end"),
            "_unused_at_the_far_end".to_owned(),
        )]],
        applicability: blaze_api::Applicability::MachineApplicable,
        style: blaze_diagnostics::model::SuggestionStyle::ShowCode,
    });
    let mut error = Diag::error(
        Some("E0425"),
        "cannot find function `missing` in this scope".to_owned(),
        span("missing"),
    );
    error.span.labels.push((span("missing"), "not found in this scope".to_owned()));
    Said {
        items: vec![
            Item::Code(warning),
            Item::Code(error),
            Item::line(Level::Warning, "`target/debug/s` is running; start it again".to_owned()),
            Item::Refused {
                reached: 20,
                lines: (0..20).map(|it| format!("f{it}: why")).collect(),
            },
        ],
        texts: Vec::new(),
        places: None,
    }
}

/// The daemon's store, as these builds use it: slots and records by key.
#[derive(Default)]
struct Store {
    filed: reads::tests::Store,
    /// How many units' compiles ran.
    executed: usize,
}

/// The project as each build lowers it, kept by what its record's key was made with (the
/// width planted in it, when one is): a build of the same sources lowers the same front, so
/// these tests lower it once for each.
#[derive(Default)]
struct Fronts(Vec<(Option<Option<u32>>, ScriptFront)>);

impl Fronts {
    /// The front of a build whose client is shown `drawing`, its record's key made with the
    /// client's width when `planted` (the bug: the width is an input of the key, as it was).
    fn of(&mut self, drawing: Drawing, planted: bool) -> &ScriptFront {
        let with = planted.then_some(drawing.width);
        let at = match self.0.iter().position(|it| it.0 == with) {
            Some(at) => at,
            None => {
                compile::PLANTED_WIDTH.set(with);
                let front = script_front(SCRIPT, true, &[("FOO", "on")]);
                compile::PLANTED_WIDTH.set(None);
                self.0.push((with, front));
                self.0.len() - 1
            }
        };
        &self.0[at].1
    }
}

/// One build of the project for a client shown diagnostics as `drawing` says: the unit's
/// record looked for under its key and, when the store has none, its compile run and its
/// record kept; then what it said drawn for the client from the record. Whether its compile
/// ran, and what the client is shown.
fn build(
    fronts: &mut Fronts,
    store: &mut Store,
    drawing: Drawing,
    planted: bool,
) -> (bool, Vec<Diagnostic>) {
    let front = fronts.of(drawing, planted);
    let env = reads::tests::env(&[("FOO", "on")]);
    let found = store.filed.find(front.base, &env).map(|it| it.key);
    let (ran, key) = match found {
        Some(key) => (false, key),
        None => {
            store.executed += 1;
            let said = compiled();
            let read = reads::Reads::of(front.lowered.db(), front.lowered.krate(1).unwrap()).unwrap();
            let key = store.filed.keep(front.base, &read, &env);
            store.filed.0.insert(key, compile::record_of(!said.failed(), &said));
            (true, key)
        }
    };
    let (ok, said) = compile::record_said(&store.filed.0[&key]).unwrap();
    assert!(!ok, "the unit has an error");
    let u = &front.units.graph.units[1];
    let unit_of = unit_of(u);
    let named = compile::span_unit(u);
    let of = drawn::Of { unit: 1, target: &u.target.name, named: &named, of: &unit_of };
    (ran, drawn::draw(&said, &read, &of, drawing).unwrap())
}

fn text(shown: &[Diagnostic]) -> String {
    shown.iter().map(|it| it.rendered.as_str()).collect()
}

/// Every way a client is shown diagnostics that these tests build for: three widths, in
/// colour or not, a line each or whole, sixteen refused functions named or every one.
fn drawings() -> Vec<Drawing> {
    let mut all = Vec::new();
    for width in [Some(80), Some(200), None] {
        for color in [false, true] {
            for short in [false, true] {
                for refused in [Some(16), None] {
                    all.push(Drawing { color, short, width, refused });
                }
            }
        }
    }
    all
}

/// The oracle: of every pair of clients, a build for the second after a build for the first
/// runs no unit's compile, misses no record, and shows the second client exactly what a build
/// for it from an empty store shows, every byte of every diagnostic (its text, and its spans
/// and children as the JSON format gives them); and the record a build keeps is the same
/// bytes whoever it was built for. The first failure, by the pair.
fn held(planted: bool) -> Result<(), String> {
    let all = drawings();
    let mut fronts = Fronts::default();
    let mut records: Vec<Vec<u8>> = Vec::new();
    for first in &all {
        for second in &all {
            let mut store = Store::default();
            let (ran, _) = build(&mut fronts, &mut store, *first, planted);
            assert!(ran, "a store with no record of it: the unit is compiled");
            records.extend(store.filed.0.values().filter(|it| it.starts_with(compile::header::MAGIC)).cloned());
            let (ran, shown) = build(&mut fronts, &mut store, *second, planted);
            if ran || store.executed != 1 {
                return Err(format!(
                    "a build for {second:?} after one for {first:?} compiled the unit again"
                ));
            }
            let (_, fresh) = build(&mut fronts, &mut Store::default(), *second, planted);
            if shown != fresh {
                return Err(format!(
                    "a build for {second:?} after one for {first:?} shows\n{}\nwhere a build from nothing shows\n{}",
                    text(&shown),
                    text(&fresh)
                ));
            }
        }
    }
    records.dedup();
    match records.len() {
        1 => Ok(()),
        n => Err(format!("{n} different records of one unit, by whom it was built for")),
    }
}

/// The invariant: how a client is shown diagnostics is no input of a unit's compile. Two
/// builds of one project that differ only in the client's width (80, 200, none), colour,
/// short or long format and how many refused functions are named: the second runs no unit
/// and misses nothing, and shows what a build from nothing for that client shows, byte for
/// byte, drawn from the record the first one kept.
#[test]
fn a_build_for_another_client_runs_no_unit_and_shows_what_a_build_from_nothing_shows() {
    held(false).unwrap();
}

/// With the client's width back in the record's key, as it was, a build at another width
/// compiles the unit again: the oracle finds it.
#[test]
fn the_oracle_catches_the_width_put_back_in_the_key() {
    let caught = held(true).unwrap_err();
    assert!(caught.contains("compiled the unit again"), "{caught}");
    assert!(caught.contains("width: Some(200)") && caught.contains("width: Some(80)"), "{caught}");
}

/// The oracle compares what differs: the same record is drawn otherwise at each width, in
/// colour, a line each, and with every refused function named. And each is what the request
/// asks for: the long line is cut to a terminal of 80 and not to one of 200, colour is ANSI
/// escapes and none without, short is a line each with no quoted source, and sixteen of
/// twenty refused functions are named unless every one is asked for.
#[test]
fn each_client_is_shown_the_record_drawn_its_own_way() {
    let mut fronts = Fronts::default();
    let mut shown =
        |drawing: Drawing| text(&build(&mut fronts, &mut Store::default(), drawing, false).1);
    let plain = Drawing { color: false, short: false, width: Some(80), refused: Some(16) };
    let narrow = shown(plain);
    let wide = shown(Drawing { width: Some(200), ..plain });
    let unset = shown(Drawing { width: None, ..plain });
    assert_ne!(narrow, wide, "{narrow}");
    assert_ne!(wide, unset, "{wide}");
    let longest = |text: &str| text.lines().map(|it| it.chars().count()).max().unwrap_or(0);
    assert!(narrow.contains("...") && !narrow.contains("let first = 1; let second = 2; let third = 3; let fourth = 4; let fifth = 5; let sixth = 6;"), "the long line is cut at 80: {narrow}");
    assert!(longest(&wide) > 150, "and shown nearly whole at 200: {wide}");
    assert!(narrow.contains("error[E0425]: cannot find function `missing` in this scope"), "{narrow}");
    assert!(narrow.contains("help: if this is intentional, prefix it with an underscore"), "{narrow}");
    assert!(narrow.contains("warning: `target/debug/s` is running; start it again\n"), "{narrow}");
    assert!(narrow.contains("\n  f15: why\n  and 4 more (`--refused all` names every one)"), "{narrow}");
    assert!(!narrow.contains('\x1b'), "{narrow:?}");

    let colored = shown(Drawing { color: true, ..plain });
    assert!(colored.contains("\x1b["), "{colored:?}");
    let short = shown(Drawing { short: true, ..plain });
    assert!(short.contains("build.rs:2:"), "{short}");
    assert!(!short.contains("let first"), "a line each, no source quoted: {short}");
    let every = shown(Drawing { refused: None, ..plain });
    assert!(every.contains("\n  f19: why") && !every.contains("more (`--refused all`"), "{every}");
}

/// A record of what a unit said restores to it byte for byte: through its encoding and a
/// record's header check, the same value and the same bytes again; and it holds no text drawn
/// for any client (no escape of a colour, none of the emitter's margins).
#[test]
fn a_record_of_what_a_unit_said_restores_to_it() {
    let said = compiled();
    let bytes = compile::record_of(false, &said);
    let (ok, restored) = compile::record_said(&bytes).unwrap();
    assert!(!ok);
    assert_eq!(restored, said);
    assert_eq!(compile::record_of(false, &restored), bytes);
    assert!(!bytes.contains(&0x1b), "no colour in a record");
    assert!(!bytes.windows(4).any(|it| it == b" --> "), "no text of the emitter's in a record");
    assert!(compile::record_said(&bytes[..bytes.len() - 1]).is_err(), "cut short, it is refused");
}

/// A record of the layout before this one (11: its diagnostics as text drawn for the client
/// of the build that made it, or as places) is never read as one of this layout: it is under
/// a key of its own domain, which no lookup of this blaze makes, and its bytes, were they
/// found, are refused naming the field they lack, never read as a unit that said nothing.
#[test]
fn a_record_of_the_layout_before_is_refused_and_never_read_as_empty() {
    #[derive(serde::Serialize)]
    struct Layout11<'a> {
        ok: bool,
        open: bool,
        why: Option<&'a str>,
        diagnostics: &'a [Diagnostic],
        outputs: Vec<(&'a str, blaze_cas::Key, bool)>,
        artifact: Option<&'a blaze_api::Artifact>,
        made: &'a compile::Made,
        placed: Option<&'a compile::Placed>,
    }
    let drawn_once =
        build(&mut Fronts::default(), &mut Store::default(), Drawing::default(), false).1;
    let old = Layout11 {
        ok: false,
        open: false,
        why: None,
        diagnostics: &drawn_once,
        outputs: Vec::new(),
        artifact: None,
        made: &compile::Made::default(),
        placed: None,
    };
    let mut bytes = Vec::new();
    compile::header::write(&mut bytes, &[], &[], &[]);
    blaze_protocol::cbor::append(&mut bytes, &old).unwrap();
    let refused = compile::record_said(&bytes).unwrap_err();
    assert!(refused.contains("said"), "it names what the bytes lack: {refused}");
    assert_eq!(compile::RECORD_DOMAIN, "blaze v2 cargo unit record 12");
}

/// A record under its narrow key keeps the places of its diagnostics' spans, and is drawn
/// from the files as they are: after an edit of what no code text holds (a comment at a
/// line's end, a comment line after the last), the client is shown the lines as they are
/// now, the diagnostics at the tokens the check pointed at.
#[test]
fn a_record_kept_with_places_is_drawn_from_the_files_as_they_are() {
    let was = "fn f() {\n    let x = 1;\n    g();\n}\n";
    let now = "fn f() { // opens\n    let x = 1; // never read\n    g();  \n}\n// the end\n";
    let at = was.find('x').unwrap() as u32;
    let x = blaze_diagnostics::Span { file: "build.rs".to_owned(), lo: at, hi: at + 1, expansion: None };
    let mut warning = Diag::error(None, "unused variable: `x`".to_owned(), x);
    warning.level = blaze_diagnostics::model::Level::Warning;
    let diags = vec![warning];
    let read_was = |name: &str| (name == "build.rs").then(|| was.to_owned());
    let read_now = |name: &str| (name == "build.rs").then(|| now.to_owned());
    let placed = compile::placed_from(read_was, span::Edition::CURRENT, &diags).unwrap();
    let said = Said {
        items: diags.into_iter().map(Item::Code).collect(),
        texts: Vec::new(),
        places: Some(placed.files),
    };
    let (_, said) = compile::record_said(&compile::record_of(true, &said)).unwrap();
    let front = script_front(SCRIPT, true, &[]);
    let u = &front.units.graph.units[1];
    let unit_of = unit_of(u);
    let of = drawn::Of { unit: 1, target: &u.target.name, named: "s", of: &unit_of };
    let before = text(&drawn::draw(&said, &read_was, &of, Drawing::default()).unwrap());
    let after = text(&drawn::draw(&said, &read_now, &of, Drawing::default()).unwrap());
    assert!(before.contains("let x = 1;\n"), "{before}");
    assert!(after.contains("let x = 1; // never read\n"), "the line as it is now: {after}");
    let pointed = |text: &str| text.lines().find(|it| it.contains('^')).map(str::to_owned);
    assert_eq!(pointed(&before), pointed(&after), "the same token is pointed at");
    // A file that lacks the place is refused, naming it: nothing is drawn at other offsets.
    let gone = |name: &str| (name == "build.rs").then(|| "fn f() {}\n".to_owned());
    let refused = drawn::draw(&said, &gone, &of, Drawing::default()).unwrap_err();
    assert!(refused.what.contains("`build.rs`") && refused.what.contains("unit 1"), "{}", refused.what);
}

/// What no view holds the text of (a build script's output, a file of the toolchain's) is
/// drawn from the text the record keeps, and the job's report of a toolchain function the
/// front end failed on is an error of no unit with each of the front end's errors its child.
#[test]
fn a_file_no_view_holds_is_drawn_from_the_text_its_record_keeps() {
    let generated = "pub fn made() { broken }\n";
    let at = generated.find("broken").unwrap() as u32;
    let broken = blaze_diagnostics::Span {
        file: "target/out/gen.rs".to_owned(),
        lo: at,
        hi: at + 6,
        expansion: None,
    };
    let error = Diag::error(Some("E0425"), "cannot find value `broken`".to_owned(), broken);
    let mut said = Said::default();
    said.text("target/out/gen.rs", generated);
    said.text("target/out/gen.rs", "a second text of one name is not kept");
    said.items.push(Item::Toolchain {
        message: "the front end fails on `std::f` in the toolchain's `std`".to_owned(),
        diags: vec![error.clone()],
    });
    said.items.push(Item::Code(error));
    assert_eq!(said.texts.len(), 1);
    let (_, said) = compile::record_said(&compile::record_of(false, &said)).unwrap();
    let front = script_front(SCRIPT, true, &[]);
    let u = &front.units.graph.units[1];
    let unit_of = unit_of(u);
    let of = drawn::Of { unit: 1, target: &u.target.name, named: "s", of: &unit_of };
    // The view is asked for no file: every one the diagnostics point into is the record's.
    let none = |name: &str| -> Option<String> { panic!("the view is asked for `{name}`") };
    let shown = drawn::draw(&said, &none, &of, Drawing::default()).unwrap();
    assert_eq!(shown[0].unit, None, "the job's report is of no unit");
    assert_eq!(shown[0].children.len(), 1);
    assert!(shown[0].rendered.starts_with("error: the front end fails on `std::f`"), "{}", shown[0].rendered);
    assert!(shown[0].rendered.contains("pub fn made() { broken }"), "{}", shown[0].rendered);
    assert_eq!(shown[1].unit.as_ref(), Some(&unit_of));
    assert!(shown[1].rendered.contains("pub fn made() { broken }"), "{}", shown[1].rendered);
    // With no text of the file, the diagnostic is refused, naming it and the file.
    let lost = Said { texts: Vec::new(), ..said };
    let refused = drawn::draw(&lost, &|_| None, &of, Drawing::default()).unwrap_err();
    assert!(refused.what.contains("target/out/gen.rs"), "{}", refused.what);
}
