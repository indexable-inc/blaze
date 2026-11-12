//! The oracle's generator (docs/harness.md, "The oracle"): a small workspace rendered from a
//! [`Model`], and a drawn sequence of [`Turn`]s, each some edits of the model, how they are
//! saved, and what else happens before the build (a stop, a kill, a cancelled build, a second
//! checkout, a build of its own, a terminal of another width). [`test`] makes the sequence a
//! revision [`Test`], so either driver runs it and a failure is written out as a case
//! ([`write_case`]) that is checked in as it is.
//!
//! Every state of the model is a valid workspace, and the model says what its program prints
//! ([`Model::prints`]): a stale answer is seen without a clean build, and the clean build of
//! each revision then tells a build that is wrong only incrementally from one that is wrong
//! from nothing. The model also bounds the work ([`bound`]): a turn that changes nothing, or
//! only comments and layout, makes no function's code; one that changes a body makes that
//! function's and no other's.
//!
//! The workspace: the package `app` at the root (a `#![no_std]` library, which the simulated
//! driver builds alone, and a `main.rs` that prints), and, each where [`Parts`] says, `up` (a
//! library with a generic `app` instantiates), `vendor/made` (outside the workspace, with a
//! build script that reads two files, writes one into `OUT_DIR` for an `include!` and sets a
//! `cfg`), and `mac` (a proc macro). The library holds a struct whose layout changes, a trait
//! with a provided method and two impls, a call through `dyn`, a generic, a module made by
//! `macro_rules!`, a pair of functions under a `cfg`, and a pair under a feature.

use std::{collections::BTreeMap, fmt::Write as _, fs, path::Path};

use super::spec::{Action, Change, Expect, Revision, Step, Test};

/// Where the draws come from: Hegel's test case, which shrinks them.
pub trait Draw {
    /// A number below `n` (`n` at least 1); 0 is the simplest.
    fn below(&mut self, n: u64) -> u64;
}

impl<F: FnMut(u64) -> u64> Draw for F {
    fn below(&mut self, n: u64) -> u64 {
        self(n)
    }
}

/// Which packages the workspace has beside `app`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Parts {
    pub up: bool,
    pub made: bool,
    pub mac: bool,
}

impl Parts {
    /// The simulated driver's: the library alone.
    pub const LIBRARY: Parts = Parts { up: false, made: false, mac: false };
}

/// One edit of the workspace. A value is what the edit sets; setting what is there already
/// changes nothing, and setting an earlier value takes an edit back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edit {
    /// The body of the library's `shown`.
    Body(u8),
    /// The body of `main.rs`'s `local`.
    MainBody(u8),
    /// `scale`'s parameter, `u32` or `u64`: its callers' text stays and their code changes.
    Signature,
    /// A field more on `P`, up to three, then back to one.
    Layout,
    /// What `B`'s `area` returns.
    TraitImpl(u8),
    /// `B` overrides the trait's provided `sides`, or does not: its vtable's slot.
    Override,
    /// `shapes` calls through `dyn`, or through the generic: the only `dyn` call goes and
    /// comes back.
    Dyn,
    /// The function `extra` and its call, there or not.
    Item,
    /// `helper` under its other name, with its caller.
    Rename,
    /// Which of the two `mode`s the source's `cfg` chooses.
    SourceCfg,
    /// The feature `loud`, a default in the manifest or not.
    Feature,
    /// The value the `macro_rules!`-made module holds.
    Generated(u8),
    /// The body of `up::sum`'s closure.
    Up(u8),
    /// The build script's own text.
    Script(u8),
    /// The file the build script reads and tells cargo of.
    ScriptInput(u8),
    /// The file the build script sets its `cfg` from.
    ScriptCfg,
    /// What the proc macro expands to.
    Mac(u8),
    /// A comment's text, in every source file of `app`.
    Comment(u8),
    /// The indent of `entry`'s body.
    Whitespace,
    /// A comment line more above everything, to three, then none: every line moves.
    Lines,
}

/// How a turn's files are saved.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Save {
    Write,
    Rename,
    Truncate,
    Twice,
}

/// What else happens in a turn, before its build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Nothing,
    Stop,
    Kill,
    /// A build interrupted this many milliseconds in.
    Cancel(u64),
    /// The sources as they were, built in a second checkout by the same daemon.
    Beside,
    /// A build of its own, so the turn's build is the second of the same state.
    Build,
}

/// What a turn asks blaze for. The simplest is `run`, a dev build, the manifest's features.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Request {
    pub action: Ask,
    pub release: bool,
    /// `--features loud`, whatever the manifest's default is.
    pub loud: bool,
    /// `--message-format json-diagnostic-short` after the driver's own.
    pub short: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Ask {
    Run,
    Build,
    Check,
    Test,
}

impl Request {
    pub const RUN: Request = Request { action: Ask::Run, release: false, loud: false, short: false };

    fn args(self) -> Vec<String> {
        let mut args = Vec::new();
        if self.release {
            args.push("--release".to_owned());
        }
        if self.loud {
            args.extend(["--features".to_owned(), "loud".to_owned()]);
        }
        if self.short {
            args.extend(["--message-format".to_owned(), "json-diagnostic-short".to_owned()]);
        }
        args
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Turn {
    pub request: Request,
    pub edits: Vec<Edit>,
    pub save: Save,
    pub event: Event,
    /// The event comes after the save, not before it.
    pub event_last: bool,
    /// The terminal's width from this turn on.
    pub width: Option<u32>,
}

/// What the workspace is rendered from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Model {
    pub parts: Parts,
    body: u8,
    main_body: u8,
    wide: bool,
    fields: u8,
    impl_b: u8,
    overrides: bool,
    by_dyn: bool,
    extra: bool,
    renamed: bool,
    source_cfg: bool,
    loud: bool,
    generated: u8,
    up: u8,
    script: u8,
    input: u8,
    spicy: bool,
    mac: u8,
    comment: u8,
    indent: bool,
    lines: u8,
}

impl Model {
    pub fn new(parts: Parts) -> Model {
        Model {
            parts,
            body: 0,
            main_body: 0,
            wide: false,
            fields: 1,
            impl_b: 0,
            overrides: false,
            by_dyn: true,
            extra: false,
            renamed: false,
            source_cfg: true,
            loud: false,
            generated: 0,
            up: 0,
            script: 0,
            input: 0,
            spicy: false,
            mac: 0,
            comment: 0,
            indent: false,
            lines: 0,
        }
    }

    pub fn apply(&mut self, edit: Edit) {
        match edit {
            Edit::Body(value) => self.body = value,
            Edit::MainBody(value) => self.main_body = value,
            Edit::Signature => self.wide = !self.wide,
            Edit::Layout => self.fields = self.fields % 3 + 1,
            Edit::TraitImpl(value) => self.impl_b = value,
            Edit::Override => self.overrides = !self.overrides,
            Edit::Dyn => self.by_dyn = !self.by_dyn,
            Edit::Item => self.extra = !self.extra,
            Edit::Rename => self.renamed = !self.renamed,
            Edit::SourceCfg => self.source_cfg = !self.source_cfg,
            Edit::Feature => self.loud = !self.loud,
            Edit::Generated(value) => self.generated = value,
            Edit::Up(value) => self.up = value,
            Edit::Script(value) => self.script = value,
            Edit::ScriptInput(value) => self.input = value,
            Edit::ScriptCfg => self.spicy = !self.spicy,
            Edit::Mac(value) => self.mac = value,
            Edit::Comment(value) => self.comment = value,
            Edit::Whitespace => self.indent = !self.indent,
            Edit::Lines => self.lines = (self.lines + 1) % 4,
        }
    }

    fn shown(&self) -> u32 {
        200 + u32::from(self.body)
    }

    /// What the library's `entry` returns.
    fn entry(&self) -> u32 {
        let read = [5, 5 ^ 1, 5 ^ 1 ^ 2][usize::from(self.fields) - 1];
        let area_b = 40 + u32::from(self.impl_b);
        // Through `dyn`, `B`'s `sides` is read too: 4 where it overrides the provided one.
        let sides_b = if self.by_dyn && self.overrides { 4 } else { 0 };
        let shapes = 3 ^ sides_b ^ area_b;
        let first = 80 + u32::from(self.generated);
        let mode = if self.source_cfg { 1 } else { 2 };
        let loud = if self.loud { 1024 } else { 0 };
        let extra = if self.extra { 2048 } else { 0 };
        read ^ shapes ^ first ^ 7 ^ mode ^ loud ^ (9 ^ 8) ^ (1 ^ 16) ^ extra ^ self.shown()
    }

    /// What the program prints when built with `--features loud` or without.
    pub fn prints_with(&self, loud: bool) -> String {
        let mut built = self.clone(); // clone: the model as the command line's feature makes it
        built.loud |= loud;
        built.prints()
    }

    /// What the program prints.
    pub fn prints(&self) -> String {
        let local = 300 + u32::from(self.main_body);
        let sum = if self.parts.up { 1 ^ 2 ^ self.shown() ^ u32::from(self.up) } else { 0 };
        let made = match self.parts.made {
            true => u32::from(self.script ^ self.input) * 10 + if self.spicy { 1 } else { 2 },
            false => 0,
        };
        let mac = if self.parts.mac { 500 + u32::from(self.mac) } else { 0 };
        format!("{} {local} {sum} {made} {mac}\n", self.entry())
    }

    fn library(&self) -> String {
        let mut out = "// above\n".repeat(usize::from(self.lines));
        let fields = ["f0: u32", "f1: u8", "f2: u64"];
        let inits = ["f0: x", "f1: 1", "f2: 2"];
        let reads = ["p.f0", "p.f1 as u32", "p.f2 as u32"];
        let n = usize::from(self.fields);
        let declared: String = fields[..n].iter().map(|it| format!("    pub {it},\n")).collect();
        let on = if self.source_cfg { "all()" } else { "any()" };
        let helper = if self.renamed { "helper_renamed" } else { "helper" };
        let indent = if self.indent { "        " } else { "    " };
        _ = write!(
            out,
            "#![no_std]\n\
             // note {comment}\n\
             pub struct P {{\n{declared}}}\n\n\
             pub fn make(x: u32) -> P {{\n    P {{ {inits} }}\n}}\n\n\
             pub fn read(p: &P) -> u32 {{\n    {reads}\n}}\n\n\
             pub trait Shape {{\n    fn sides(&self) -> u32 {{\n        0\n    }}\n    fn area(&self) -> u32;\n}}\n\n\
             pub struct A;\npub struct B;\n\n\
             impl Shape for A {{\n    fn area(&self) -> u32 {{\n        3\n    }}\n}}\n\n\
             impl Shape for B {{\n{sides}    fn area(&self) -> u32 {{\n        {area_b}\n    }}\n}}\n\n\
             pub fn by_dyn(shape: &dyn Shape) -> u32 {{\n    shape.sides() ^ shape.area()\n}}\n\n\
             pub fn by_generic<S: Shape>(shape: &S) -> u32 {{\n    shape.area()\n}}\n\n\
             pub fn shapes() -> u32 {{\n    {shapes}\n}}\n\n\
             macro_rules! table {{\n    ($name:ident, $value:expr) => {{\n        pub mod $name {{\n            pub const K: u32 = $value;\n            pub fn get() -> u32 {{\n                K\n            }}\n        }}\n    }};\n}}\n\
             table!(first, {first});\ntable!(second, 7);\n\n\
             #[cfg({on})]\npub fn mode() -> u32 {{\n    1\n}}\n\n\
             #[cfg(not({on}))]\npub fn mode() -> u32 {{\n    2\n}}\n\n\
             #[cfg(feature = \"loud\")]\npub fn loud() -> u32 {{\n    1024\n}}\n\n\
             #[cfg(not(feature = \"loud\"))]\npub fn loud() -> u32 {{\n    0\n}}\n\n\
             pub fn scale(x: {scale}) -> u32 {{\n    (x as u32) ^ 8\n}}\n\n\
             pub fn {helper}(x: u32) -> u32 {{\n    x ^ 16\n}}\n\n\
             {extra}\
             pub fn shown() -> u32 {{\n    {shown}\n}}\n\n\
             pub fn entry() -> u32 {{\n{indent}read(&make(5)) ^ shapes() ^ first::get() ^ second::get() ^ mode() ^ loud() ^ scale(9) ^ {helper}(1) ^ {extra_call} ^ shown()\n}}\n",
            comment = self.comment,
            inits = inits[..n].join(", "),
            reads = reads[..n].join(" ^ "),
            sides = if self.overrides { "    fn sides(&self) -> u32 {\n        4\n    }\n" } else { "" },
            area_b = 40 + u32::from(self.impl_b),
            shapes = if self.by_dyn { "by_dyn(&A) ^ by_dyn(&B)" } else { "by_generic(&A) ^ by_generic(&B)" },
            first = 80 + u32::from(self.generated),
            scale = if self.wide { "u64" } else { "u32" },
            extra = if self.extra { "pub fn extra() -> u32 {\n    2048\n}\n\n" } else { "" },
            extra_call = if self.extra { "extra()" } else { "0" },
            shown = self.shown(),
        );
        out
    }

    fn main(&self) -> String {
        let sum = if self.parts.up { "up::sum(&[1, 2, up::pick(app::shown(), 0u32)])" } else { "0" };
        let made = if self.parts.made { "made::MADE * 10 + made::flavor()" } else { "0" };
        let mac = if self.parts.mac { "mac::answer!()" } else { "0" };
        format!(
            "// note {}\n\
             fn local() -> u32 {{\n    {}\n}}\n\n\
             fn main() {{\n    let unused_at_the_far_end = 9;\n    println!(\"{{}} {{}} {{}} {{}} {{}}\", app::entry(), local(), {sum}, {made}, {mac});\n}}\n",
            self.comment,
            300 + u32::from(self.main_body),
        )
    }

    /// The workspace's files, by path.
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        let mut files = BTreeMap::new();
        let mut put = |path: &str, text: String| {
            files.insert(path.to_owned(), text.into_bytes());
        };
        let mut manifest = format!(
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [features]\ndefault = [{}]\nloud = []\n\n[dependencies]\n",
            if self.loud { "\"loud\"" } else { "" }
        );
        let mut members = Vec::new();
        if self.parts.up {
            manifest.push_str("up = { path = \"up\" }\n");
            members.push("\"up\"");
            put("up/Cargo.toml", "[package]\nname = \"up\"\nversion = \"0.1.0\"\nedition = \"2024\"\n".to_owned());
            put(
                "up/src/lib.rs",
                format!(
                    "pub fn pick<T: Copy>(a: T, _b: T) -> T {{\n    a\n}}\n\n\
                     pub fn sum(xs: &[u32]) -> u32 {{\n    let each: Vec<u32> = xs.iter().map(|x| x ^ {}).collect();\n    each.iter().fold(0, |a, b| a ^ b)\n}}\n",
                    self.up
                ),
            );
        }
        if self.parts.made {
            manifest.push_str("made = { path = \"vendor/made\" }\n");
            put(
                "vendor/made/Cargo.toml",
                "[package]\nname = \"made\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n".to_owned(),
            );
            put(
                "vendor/made/build.rs",
                format!(
                    "fn main() {{\n    println!(\"cargo::rerun-if-changed=build.rs\");\n    println!(\"cargo::rerun-if-changed=input\");\n    println!(\"cargo::rerun-if-changed=mode\");\n    println!(\"cargo::rustc-check-cfg=cfg(spicy)\");\n    \
                     let input: u32 = std::fs::read_to_string(\"input\").expect(\"input\").trim().parse().expect(\"a number\");\n    \
                     if std::fs::read_to_string(\"mode\").expect(\"mode\").trim() == \"spicy\" {{\n        println!(\"cargo::rustc-cfg=spicy\");\n    }}\n    \
                     let out = std::env::var(\"OUT_DIR\").expect(\"OUT_DIR\");\n    \
                     std::fs::write(format!(\"{{out}}/made.rs\"), format!(\"pub const MADE: u32 = {{}};\\n\", input ^ {})).expect(\"writing made.rs\");\n}}\n",
                    self.script
                ),
            );
            put("vendor/made/input", format!("{}\n", self.input));
            put("vendor/made/mode", format!("{}\n", if self.spicy { "spicy" } else { "mild" }));
            put(
                "vendor/made/src/lib.rs",
                "include!(concat!(env!(\"OUT_DIR\"), \"/made.rs\"));\n\n#[cfg(spicy)]\npub fn flavor() -> u32 {\n    1\n}\n\n#[cfg(not(spicy))]\npub fn flavor() -> u32 {\n    2\n}\n".to_owned(),
            );
        }
        if self.parts.mac {
            manifest.push_str("mac = { path = \"mac\" }\n");
            members.push("\"mac\"");
            put(
                "mac/Cargo.toml",
                "[package]\nname = \"mac\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n".to_owned(),
            );
            put(
                "mac/src/lib.rs",
                format!(
                    "use proc_macro::TokenStream;\n\n#[proc_macro]\npub fn answer(_input: TokenStream) -> TokenStream {{\n    \"{}u32\".parse().unwrap()\n}}\n",
                    500 + u32::from(self.mac)
                ),
            );
        }
        _ = write!(manifest, "\n[workspace]\nmembers = [{}]\nexclude = [\"vendor\"]\nresolver = \"3\"\n", members.join(", "));
        put("Cargo.toml", manifest);
        put("src/lib.rs", self.library());
        put("src/main.rs", self.main());
        files
    }
}

/// The names `BLAZE_ORACLE_SKIP` lists (edit kinds and events, lower case, comma separated)
/// are never drawn: a kind with a known failure is set aside by name while the others run.
fn skipped(name: &str) -> bool {
    std::env::var("BLAZE_ORACLE_SKIP").is_ok_and(|all| all.split(',').any(|it| it.trim() == name))
}

/// The edit kinds `parts` has, by name, each given the value drawn for it.
fn kinds(parts: Parts, library_only: bool) -> Vec<(&'static str, fn(u8) -> Edit)> {
    let mut all: Vec<(&'static str, fn(u8) -> Edit)> = vec![
        ("body", Edit::Body),
        ("comment", Edit::Comment),
        ("whitespace", |_| Edit::Whitespace),
        ("lines", |_| Edit::Lines),
        ("signature", |_| Edit::Signature),
        ("layout", |_| Edit::Layout),
        ("traitimpl", Edit::TraitImpl),
        ("override", |_| Edit::Override),
        ("dyn", |_| Edit::Dyn),
        ("item", |_| Edit::Item),
        ("rename", |_| Edit::Rename),
        ("sourcecfg", |_| Edit::SourceCfg),
        ("generated", Edit::Generated),
    ];
    if !library_only {
        all.push(("mainbody", Edit::MainBody));
        all.push(("feature", |_| Edit::Feature));
        if parts.up {
            all.push(("up", Edit::Up));
        }
        if parts.made {
            all.push(("script", Edit::Script));
            all.push(("scriptinput", Edit::ScriptInput));
            all.push(("scriptcfg", |_| Edit::ScriptCfg));
        }
        if parts.mac {
            all.push(("mac", Edit::Mac));
        }
    }
    all.retain(|(name, _)| !skipped(name));
    all
}

/// The workspace's parts, drawn; none is the simplest.
pub fn parts(draw: &mut dyn Draw) -> Parts {
    let mut part = |name: &str| !skipped(name) && draw.below(2) == 1;
    Parts { up: part("up-part"), made: part("made-part"), mac: part("mac-part") }
}

/// One turn, drawn; the simplest is a build of what is there.
pub fn turn(draw: &mut dyn Draw, parts: Parts, library_only: bool) -> Turn {
    let kinds = kinds(parts, library_only);
    let count = if kinds.is_empty() { 0 } else { [0, 1, 1, 1, 1, 1, 2, 2][draw.below(8) as usize] };
    let edits = (0..count)
        .map(|_| {
            let (_, make) = kinds[draw.below(kinds.len() as u64) as usize];
            make(draw.below(3) as u8)
        })
        .collect();
    let save = [Save::Write, Save::Rename, Save::Truncate, Save::Twice][draw.below(4) as usize];
    let event = match draw.below(10) {
        5 if !skipped("stop") => Event::Stop,
        6 if !skipped("kill") => Event::Kill,
        7 if !skipped("cancel") => Event::Cancel(20 + 60 * draw.below(8)),
        8 if !skipped("beside") => Event::Beside,
        9 if !skipped("build") => Event::Build,
        _ => Event::Nothing,
    };
    let event_last = event != Event::Nothing && draw.below(2) == 1;
    let width = match draw.below(8) {
        6 if !library_only && !skipped("width") => Some(80),
        7 if !library_only && !skipped("width") => Some(200),
        _ => None,
    };
    // The alternations: another command, profile, feature set or message format than the
    // turn before, over the same daemon.
    let request = match library_only {
        true => Request::RUN,
        false => Request {
            action: match draw.below(8) {
                4 | 5 if !skipped("build-ask") => Ask::Build,
                6 if !skipped("check-ask") => Ask::Check,
                7 if !skipped("test-ask") => Ask::Test,
                _ => Ask::Run,
            },
            release: draw.below(8) == 7 && !skipped("release"),
            loud: draw.below(6) == 5 && !skipped("loud-flag"),
            short: draw.below(8) == 7 && !skipped("short"),
        },
    };
    Turn { request, edits, save, event, event_last, width }
}

/// A sequence of one to `most` turns, drawn.
pub fn turns(draw: &mut dyn Draw, parts: Parts, library_only: bool, most: u64) -> Vec<Turn> {
    let count = 1 + draw.below(most);
    (0..count).map(|_| turn(draw, parts, library_only)).collect()
}

/// What a turn from `before` to `after` may make again: `None` where the model has no bound.
/// Comments, the indent and lines moved make no function; a body makes its own.
fn bound(before: &Model, after: &Model) -> Option<Vec<String>> {
    let mut same = before.clone(); // clone: the state before, with the bounded fields moved
    (same.comment, same.indent, same.lines) = (after.comment, after.indent, after.lines);
    (same.body, same.main_body) = (after.body, after.main_body);
    if same != *after {
        return None;
    }
    let mut within = Vec::new();
    if before.body != after.body {
        within.push("app::shown".to_owned());
    }
    if before.main_body != after.main_body {
        within.push("app::local".to_owned());
    }
    Some(within)
}

/// The revision test of `turns` over a workspace of `parts`: the first revision builds the
/// workspace as [`Model::new`] renders it, and each turn is a revision after it.
pub fn test(name: &str, parts: Parts, turns: &[Turn], drivers: &[&str]) -> Test {
    let mut model = Model::new(parts);
    let mut files = model.files();
    let revision = |label: String, request: Request, changes, steps, env, expect| Revision {
        label,
        changes,
        steps,
        action: match request.action {
            Ask::Run => Action::Run,
            Ask::Build => Action::Build,
            Ask::Check => Action::Check,
            Ask::Test => Action::Test,
        },
        args: request.args(),
        env,
        expect,
        known: BTreeMap::new(),
    };
    // The requests made since the sources last changed, each with the terminal's width, and
    // those made at all: a request made again of the same sources is a no-op in units and
    // runs a number of queries that is not the dependencies'; an edit's bound holds for a
    // request the daemon has built before (the first `test` builds the tests).
    let mut width = None;
    let mut since_change = vec![(Request::RUN, width)];
    let mut ever = vec![Request::RUN];
    let all = |files: &BTreeMap<String, Vec<u8>>| {
        // clone: the files as the revision's changes
        files.iter().map(|(path, bytes)| Change { path: path.clone(), bytes: Some(bytes.clone()) }).collect()
    };
    let mut revisions = vec![revision(
        "rev1".to_owned(),
        Request::RUN,
        all(&files),
        vec![Step::Edit],
        Vec::new(),
        Expect { prints: Some(model.prints()), ..Expect::default() },
    )];
    for (index, turn) in turns.iter().enumerate() {
        let before = model.clone(); // clone: the state the turn starts from
        for edit in &turn.edits {
            model.apply(*edit);
        }
        let now = model.files();
        let changes: Vec<Change> = now
            .iter()
            .filter(|(path, bytes)| files.get(*path) != Some(bytes))
            // clone: the changed files as the revision's changes
            .map(|(path, bytes)| Change { path: path.clone(), bytes: Some(bytes.clone()) })
            .collect();
        files = now;
        let event = match turn.event {
            Event::Nothing => None,
            Event::Stop => Some(Step::Stop),
            Event::Kill => Some(Step::Kill),
            Event::Cancel(after) => Some(Step::Cancel(after)),
            Event::Beside => Some(Step::Beside("two".to_owned())),
            Event::Build => Some(Step::Do(Action::Build)),
        };
        let mut steps = Vec::new();
        // A build of its own is of the turn's state: it always follows the save.
        let last = turn.event_last || turn.event == Event::Build;
        if !last {
            steps.extend(event.clone()); // clone: the one event, before or after the save
        }
        if !changes.is_empty() {
            steps.push(match turn.save {
                Save::Write => Step::Edit,
                Save::Rename => Step::EditByRename,
                Save::Truncate => Step::EditTruncateWrite,
                Save::Twice => Step::EditTwice,
            });
        }
        if last {
            steps.extend(event);
        }
        let request = turn.request;
        width = turn.width.or(width);
        if before != model {
            since_change.clear();
        }
        // Made before of these very sources: whatever the width, no unit and no function,
        // and few queries (200,000 and more is what lowering a workspace over std runs).
        let again = since_change.iter().any(|(it, _)| *it == request);
        let within = match (again, ever.contains(&request)) {
            (true, _) => Some(Vec::new()),
            (false, true) => bound(&before, &model),
            (false, false) => None,
        };
        let expect = Expect {
            prints: (request.action == Ask::Run).then(|| model.prints_with(request.loud)),
            // Units: none where nothing changed. An edit's units are not bounded here: a
            // body edit of the library makes two (the library's and the program's, 2026-11-12),
            // and so does a comment that moves lines.
            max_units: (again || before == model).then_some(0),
            max_executed: again.then_some(2000),
            redone_within: within,
            ..Expect::default()
        };
        since_change.push((request, width));
        ever.push(request);
        let env = turn.width.map(|it| ("__CARGO_TEST_TTY_WIDTH_DO_NOT_USE_THIS".to_owned(), it.to_string()));
        revisions.push(revision(format!("rev{}", index + 2), request, changes, steps, env.into_iter().collect(), expect));
    }
    Test {
        name: name.to_owned(),
        about: "A sequence the oracle drew: each build prints what the model says and is what a clean build of the same sources is.".to_owned(),
        drivers: drivers.iter().map(|it| (*it).to_owned()).collect(),
        seeds: 1,
        revisions,
    }
}

/// A sequence written by hand, for a replay: turns apart by `;`, each of words. A word is an
/// edit kind's name with the value it sets (`body=1`, `item`), a save (`by-rename`,
/// `by-truncate`, `twice`), an event (`stop`, `kill`, `cancel=80`, `beside`, `build-first`),
/// `event-last`, `width=80`, or of the request (`build-ask`, `check-ask`, `test-ask`,
/// `release`, `loud-flag`, `short`). A first turn `parts up made mac` names the workspace's
/// parts (none otherwise). `"parts made; scriptcfg; body=1 kill"`.
pub fn parse(text: &str) -> (Parts, Vec<Turn>) {
    let mut parts = Parts::LIBRARY;
    let mut turns = Vec::new();
    let every = Parts { up: true, made: true, mac: true };
    for (index, words) in text.split(';').map(str::trim).enumerate() {
        let mut words = words.split_whitespace().peekable();
        if index == 0 && words.peek() == Some(&"parts") {
            for part in words.skip(1) {
                match part {
                    "up" => parts.up = true,
                    "made" => parts.made = true,
                    "mac" => parts.mac = true,
                    other => panic!("no part `{other}`: up, made, mac"),
                }
            }
            continue;
        }
        let mut turn =
            Turn { request: Request::RUN, edits: Vec::new(), save: Save::Write, event: Event::Nothing, event_last: false, width: None };
        for word in words {
            let (name, value) = word.split_once('=').unwrap_or((word, "0"));
            let value: u64 = value.parse().unwrap_or_else(|_| panic!("`{word}`: the value is a number"));
            match name {
                "by-rename" => turn.save = Save::Rename,
                "by-truncate" => turn.save = Save::Truncate,
                "twice" => turn.save = Save::Twice,
                "stop" => turn.event = Event::Stop,
                "kill" => turn.event = Event::Kill,
                "cancel" => turn.event = Event::Cancel(value),
                "beside" => turn.event = Event::Beside,
                "build-first" => turn.event = Event::Build,
                "event-last" => turn.event_last = true,
                "width" => turn.width = Some(value as u32),
                "build-ask" => turn.request.action = Ask::Build,
                "check-ask" => turn.request.action = Ask::Check,
                "test-ask" => turn.request.action = Ask::Test,
                "release" => turn.request.release = true,
                "loud-flag" => turn.request.loud = true,
                "short" => turn.request.short = true,
                _ => match kinds(every, false).iter().find(|(kind, _)| *kind == name) {
                    Some((_, make)) => turn.edits.push(make(value as u8)),
                    None => panic!("no word `{name}` (generate.rs, `parse`)"),
                },
            }
        }
        turns.push(turn);
    }
    (parts, turns)
}

/// `test` written as a case directory (`base/`, `rev2/`.., `test.toml`) under `into`, as
/// [`Test::read`] reads it back: what a failure is checked in as.
pub fn write_case(test: &Test, into: &Path) {
    let dir = into.join(&test.name);
    let _ = fs::remove_dir_all(&dir);
    let mut toml = format!(
        "about = {:?}\ndrivers = [{}]\nseeds = {}\n",
        test.about,
        test.drivers.iter().map(|it| format!("{it:?}")).collect::<Vec<_>>().join(", "),
        test.seeds
    );
    let list = |all: &[String]| all.iter().map(|it| format!("{it:?}")).collect::<Vec<_>>().join(", ");
    for (index, revision) in test.revisions.iter().enumerate() {
        let overlay = dir.join(if index == 0 { "base".to_owned() } else { revision.label.clone() }); // clone: a path's part
        for change in &revision.changes {
            let bytes = change.bytes.as_ref().expect("the oracle removes no file");
            let path = overlay.join(&change.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, bytes).unwrap();
        }
        let steps: Vec<String> = revision.steps.iter().map(Step::word).collect();
        _ = write!(toml, "\n[[revision]]\nsteps = [{}]\naction = {:?}\n", list(&steps), revision.action.word());
        if !revision.env.is_empty() {
            let env: Vec<String> = revision.env.iter().map(|(name, value)| format!("{name} = {value:?}")).collect();
            _ = writeln!(toml, "env = {{ {} }}", env.join(", "));
        }
        if let Some(prints) = &revision.expect.prints {
            _ = writeln!(toml, "prints = {prints:?}");
        }
        if let Some(within) = &revision.expect.redone_within {
            _ = writeln!(toml, "redone_within = [{}]", list(within));
        }
        if let Some(most) = revision.expect.max_units {
            _ = writeln!(toml, "max_units = {most}");
        }
        if let Some(most) = revision.expect.max_executed {
            _ = writeln!(toml, "max_executed = {most}");
        }
        if !revision.args.is_empty() {
            _ = writeln!(toml, "args = [{}]", list(&revision.args));
        }
    }
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("test.toml"), toml).unwrap();
}

/// A name for the case of `turns`: a function of the sequence alone (FNV-1a of its debug
/// form), so one failure is written to one place however often it is found.
pub fn name(parts: Parts, turns: &[Turn]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in format!("{parts:?}{turns:?}").bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("oracle_{hash:016x}")
}
