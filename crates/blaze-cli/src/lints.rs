//! Lints in `blaze check` (docs/v2/plan/design/lint.md; key.rs `Kind::Lint`). A file's lint
//! units are blaze-lint's ([`blaze_lint::Units`]): unit 0 the file, its top-level items left out
//! as holes; unit `i + 1` the outline's entry `i`, its children left out. Each is a `Lint` part
//! of its file's `FileItems` record ([`parts_in`]), named by its file and its entry's chain
//! ([`unit_name`]) and located by its own bytes, its holes and the scope its parent's text gives
//! it ([`unit_source`]); its kind's tool holds the rule set's id (home.rs `tools`). So its key is
//! computed before the work from what its findings are a function of, and the task reads nothing:
//! an edit of one item that keeps its length relints that item alone; one that moves its length
//! relints its parent too (the parent's holes moved).
//!
//! A unit's record ([`lint_units_in`]): one drive finding a lint finding, numbered [`LINT_BASE`] plus
//! its rule's id, its range item-relative (rule 71), its message the engine's words
//! ([`blaze_lint::show_message`]); its payload the engine's findings with their fixes, for
//! `--fix` ([`payload`]). No level is stored (rule 81): print.rs gives each the level the step's
//! [`blaze_lint::Levels`] say, at print time.
//!
//! Which rules run is what the root `Cargo.toml` names under `[workspace.metadata.blaze]`
//! ([`lints_of`]): the step (`lints = "rust" | "suggest" | "edition" | "strict"`; none named is
//! [`Step::Rust`]) and the rules it adds by name (`lints-add = ["ambient-heap", ..]`, rules no
//! step runs). An empty rule set lists no `Lint` part and runs no task.

use blaze_drive::{Kind, Locate, Name, Part, Why};
use blaze_ids::{ContentId, Hasher};
use blaze_lint::{Findings, Level, OwnHeads, RootMarks, Rule, RuleSet, Scope, Step, TestBits, Units, Variants, statics_of_path};
use blaze_own::{AVec, Alloc, Vec};
use std::alloc::Global;

use crate::work::Record;

/// The first finding number of a lint: a finding numbered `LINT_BASE + rule.id()` is the lint
/// `rule`'s. Far from every other number the cli and the drive use (1..=0x40, 0xC0F6..).
pub const LINT_BASE: u16 = 0x4C00;

const UNIT_NAME_DOMAIN: &str = "blaze-cli lint unit name 1";
const UNIT_SOURCE_DOMAIN: &str = "blaze-cli lint unit source 1";
const UNIT_OUT_DOMAIN: &str = "blaze-cli lint unit out 1";

/// The finding number of lint `rule`: the one derivation, for a typed rule's findings that the
/// body check files (body_types.rs, `args-inline`) as for the token pass's.
pub(crate) const fn number_of(rule: Rule) -> u16 {
    LINT_BASE + rule.id()
}

/// The lint rule a finding number is, if it is one.
pub(crate) fn rule_of(number: u16) -> Option<Rule> {
    let id = number.checked_sub(LINT_BASE)?;
    Rule::ALL.into_iter().find(|rule| rule.id() == id)
}

/// Whether finding number `number` is a lint's.
pub(crate) fn is_lint(number: u16) -> bool {
    rule_of(number).is_some()
}

/// The level a lint finding numbered `number` prints at under `step`, in a file whose crate root
/// and own top say `marks` (rule 81: decided now, from the configuration and the crate's own
/// words, never stored): the step's level, an error where the crate denies the rule's lint
/// (blaze-lint `RootMarks::levels`).
pub(crate) fn level_in(step: Step, marks: RootMarks, number: u16) -> Level {
    match rule_of(number) {
        Some(rule) => marks.levels(step.levels()).of(rule),
        None => Level::Off,
    }
}

/// The marks a file lints under: its crate root's inner attributes (`#![blaze::sans_io]`,
/// `#![blaze::driver]`, `#![deny(blaze::..)]`) joined with its own top's (a module file may be
/// a core in a crate that is not). For the crate root itself both texts are the same file.
pub(crate) fn marks_of(crate_root_text: &str, text: &str) -> RootMarks {
    RootMarks::read(crate_root_text).join(RootMarks::read(text))
}

/// Why the root manifest's lints could not be read: each names the key (rule 19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepError {
    /// The root `Cargo.toml` is not TOML: the layout's finding names where.
    Manifest,
    /// `lints` is not a string; at its line and column.
    NotAString { line: usize, column: usize },
    /// `lints` names no step; at its line and column.
    NoSuchStep { line: usize, column: usize },
    /// `lints-add` is not an array; at its line and column.
    AddNotAnArray { line: usize, column: usize },
    /// An entry of `lints-add` is not a string; at its line and column.
    AddNotAString { line: usize, column: usize },
    /// An entry of `lints-add` names no rule: the word, at its line and column.
    NoSuchRule { line: usize, column: usize, word: blaze_own::String<Global> },
    /// An entry of `lints-add` names a typed rule (`args-inline`), which the step turns on, never
    /// a name (blaze-lint `Rule::typed`); at its line and column.
    TypedRule { line: usize, column: usize, rule: Rule },
}

impl core::fmt::Display for StepError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StepError::Manifest => f.write_str("the root `Cargo.toml` is not TOML, so its `[workspace.metadata.blaze] lints` could not be read"),
            StepError::NotAString { line, column } => {
                write!(f, "Cargo.toml:{line}:{column}: `[workspace.metadata.blaze] lints` is not a string: name one of `rust`, `suggest`, `edition`, `strict`")
            }
            StepError::NoSuchStep { line, column } => {
                write!(f, "Cargo.toml:{line}:{column}: `[workspace.metadata.blaze] lints` names no step: name one of `rust`, `suggest`, `edition`, `strict`")
            }
            StepError::AddNotAnArray { line, column } => {
                write!(f, "Cargo.toml:{line}:{column}: `[workspace.metadata.blaze] lints-add` is not an array: write the rules' names, `lints-add = [\"ambient-heap\"]`")
            }
            StepError::AddNotAString { line, column } => {
                write!(f, "Cargo.toml:{line}:{column}: an entry of `[workspace.metadata.blaze] lints-add` is not a string: write a rule's name")
            }
            StepError::NoSuchRule { line, column, word } => {
                write!(f, "Cargo.toml:{line}:{column}: `[workspace.metadata.blaze] lints-add` names `{word}`, which is no rule: name one of")?;
                for (at, rule) in Rule::ALL.into_iter().enumerate() {
                    write!(f, "{} `{}`", if at == 0 { "" } else { "," }, rule.name())?;
                }
                Ok(())
            }
            StepError::TypedRule { line, column, rule } => write!(
                f,
                "Cargo.toml:{line}:{column}: `[workspace.metadata.blaze] lints-add` names `{}`, a rule the body check decides from types: the step turns it on (`edition` as advice, `strict` as an error), so set `lints` instead",
                rule.name()
            ),
        }
    }
}

/// What a command lints with: the step (its levels, print.rs) and the rules that run, the step's
/// and those `lints-add` names. The rule set's id is in the `FileItems` and `Lint` kinds' tools
/// (home.rs `tools`), so an edit of either key relints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lints {
    pub step: Step,
    pub rules: RuleSet,
}

impl Lints {
    /// No rule: every command but `check`.
    pub const NONE: Lints = Lints { step: Step::Rust, rules: Step::Rust.rules() };
}

/// What the root manifest's text names under `[workspace.metadata.blaze]`: the step (`lints`;
/// none named is [`Step::Rust`]) and the rules `lints-add` adds to it (absent: none). A bad value
/// or an unknown name is refused by name, never read as the default or skipped (rule 3).
pub fn lints_of(manifest: &str) -> Result<Lints, StepError> {
    let doc = blaze_toml::Document::parse_in(manifest, blaze_toml::Limits::CARGO, Global).map_err(|_| StepError::Manifest)?;
    let blaze = doc.root().get("workspace").and_then(|it| it.get("metadata")).and_then(|it| it.get("blaze"));
    let step = match blaze.and_then(|it| it.get("lints")) {
        None => Step::Rust,
        Some(item) => {
            let (line, column) = item.line_col();
            let name = item.as_str().ok_or(StepError::NotAString { line, column })?;
            Step::from_name(name).ok_or(StepError::NoSuchStep { line, column })?
        }
    };
    let mut rules = step.rules();
    if let Some(added) = blaze.and_then(|it| it.get("lints-add")) {
        let (line, column) = added.line_col();
        if !added.is_array() {
            return Err(StepError::AddNotAnArray { line, column });
        }
        for entry in added.children() {
            let (line, column) = entry.line_col();
            let word = entry.as_str().ok_or(StepError::AddNotAString { line, column })?;
            let rule = Rule::from_name(word).ok_or_else(|| StepError::NoSuchRule { line, column, word: blaze_own::String::from_str_in(word, Global) })?;
            if rule.typed() {
                return Err(StepError::TypedRule { line, column, rule });
            }
            rules = rules.with(rule);
        }
    }
    Ok(Lints { step, rules })
}

/// What the checkout at `root` lints with: its root `Cargo.toml` read (in `frame`) and
/// [`lints_of`] it. The one reading of the step for `check` and `engine-check` alike.
pub(crate) fn of_root(cx: &bstd::Cx, root: &bstd::fs::Dir<bstd::fs::ReadWrite>, frame: &bstd::mem::Frame) -> Result<Lints, crate::CliError> {
    let mut manifest = bstd::mem::Bytes::new_in(frame, "the root manifest, for its lint step");
    root.read_all(cx, &bstd::path::Path::new("Cargo.toml")?, &mut manifest)?;
    let text = core::str::from_utf8(manifest.as_slice()).map_err(|_| crate::CliError::Text { what: "the root `Cargo.toml` is not UTF-8" })?;
    Ok(lints_of(text)?)
}

/// A file's root scope from its path, as blaze-lint's example reads it: a test file (under
/// `tests/` or `benches/`, or `tests.rs`, `test.rs`), a binary's root, a build script. A module
/// declared in test code in ANOTHER file is not test code here (owed: its parent's `Lint` record
/// would say so, which is a read of another item).
/// The scope a file's units are linted in: its path's ([`file_scope`]), in a core where its
/// marks say so, with the facts of its own text and crate (blaze-lint `Scope::with_file_facts`):
/// the IO module names the file makes its own (`OwnHeads::read`) and its crate's rows of the
/// allowed statics (`statics_of_path`, by `path` relative to the workspace's root). All of it is
/// in the scope's key, so it is in every unit's [`unit_source`].
pub(crate) fn file_scope_in(marks: RootMarks, path: &str, text: &str) -> Scope {
    marks.scope(file_scope(path)).with_file_facts(OwnHeads::read(text), statics_of_path(path))
}

pub(crate) fn file_scope(path: &str) -> Scope {
    let mut parts = path.split('/');
    let name = parts.next_back().unwrap_or("");
    let dirs: Vec<&str, Global> = {
        let mut dirs = Vec::new_in(Global);
        dirs.extend(parts);
        dirs
    };
    let test = dirs.iter().any(|dir| matches!(*dir, "tests" | "benches")) || matches!(name, "tests.rs" | "test.rs");
    let build_script = name == "build.rs" && dirs.last().is_some_and(|dir| *dir != "src");
    let bin_dir = |dir: &str| matches!(dir, "bin" | "examples");
    let bin_root = match dirs.as_slice() {
        [.., up, _] if name == "main.rs" && bin_dir(*up) => true,
        [.., dir] if name == "main.rs" && *dir == "src" => true,
        [.., dir] => bin_dir(*dir),
        _ => false,
    };
    Scope::file(if test { TestBits::CFG } else { TestBits::NONE }, bin_root, build_script)
}

/// Unit `at`'s name: its file's subject (signature.rs `file_subject`, of its real path) and,
/// for an item, its entry's chain (work.rs
/// `item_chain`), so it stands while other items move and its own text changes.
pub(crate) fn unit_name(file: ContentId, outline: &blaze_file_items::Outline<'_>, at: usize, text: &str) -> Name {
    let mut h = Hasher::new(UNIT_NAME_DOMAIN);
    h.id(&file);
    match at.checked_sub(1).and_then(|i| outline.items().get(i)) {
        None => h.update(b"file"),
        Some(item) => h.update(b"item").update(&crate::work::item_chain(outline, item, text).to_le_bytes()),
    };
    Name::new(Kind::Lint, h.finish())
}

/// Unit `at`'s source: what its findings are a function of, but the rule set: its own bytes,
/// its holes (item-relative, rule 71), its scope's key, and what `variant-count` reads of the
/// rest of its file (the enums of its literals' counts, [`Variants::unit_key`]; nothing when none),
/// and its file's marks ([`RootMarks::key`]: the rules a crate's deny or core mark adds to the
/// workspace's set are a function of them, and the workspace's set alone is in the task's key).
pub(crate) fn unit_source(units: &Units<'_>, at: usize, text: &str, scope: Scope, marks: RootMarks, variants: &Variants<'_>) -> ContentId {
    let plan = units.list[at];
    let own = &text.as_bytes()[plan.base as usize..plan.end as usize];
    let mut h = Hasher::new(UNIT_SOURCE_DOMAIN);
    h.update(&(own.len() as u32).to_le_bytes()).update(&scope.key().to_le_bytes()).update(&marks.key().to_le_bytes());
    if let Some(read) = variants.unit_key(plan.base, plan.end - plan.base, units.holes_of(at)) {
        h.update(b"variant-count").update(&read);
    }
    let mut from = 0;
    for hole in units.holes_of(at) {
        h.update(&own[from..hole.start as usize]).update(&hole.start.to_le_bytes()).update(&hole.end.to_le_bytes());
        from = hole.end as usize;
    }
    h.update(&own[from..]);
    h.finish()
}

/// Why a file's units could not be planned or linted: blaze-lint's refusal, named.
const UNPLANNED: &str = "a file's outline is not one of lint units (blaze-lint `Units::plan`)";
const UNLINTED: &str = "a lint unit could not be linted (blaze-lint `lint`)";
const UNGATHERED: &str = "a file's `variant-count` fact could not be gathered (blaze-lint `Variants::gather`)";

/// Each unit of `text` as a `Lint` part, pushed on `parts`: the scopes walked in pre-order by a
/// pass of no rule (one lex of the file; what a scope is does not depend on the rules) from the
/// file's scope as `marks` make it (in a core or not), each unit's place its start in the file.
/// Scratch in `alloc`.
pub(crate) fn parts_in<A: core::alloc::Allocator + Copy>(
    outline: &blaze_file_items::Outline<'_>,
    text: &str,
    path: &str,
    file: ContentId,
    marks: RootMarks,
    alloc: Alloc<'_>,
    parts: &mut Vec<Part, A>,
) -> Result<(), &'static str> {
    let units = Units::plan(outline.items(), text.len(), alloc).map_err(|_| UNPLANNED)?;
    let none = RuleSet::only(&[]);
    let mut findings = Findings::new_in(alloc);
    let mut scopes: AVec<'_, Scope> = AVec::new_in(alloc);
    units.lint_all(&none, text, file_scope_in(marks, path, text), alloc, &mut findings, &mut scopes, |_| {}).map_err(|_| UNLINTED)?;
    // Gathered whatever the rules, as `lint_units_in` does: the keys of both must agree.
    let variants = Variants::gather(text, alloc).map_err(|_| UNGATHERED)?;
    for at in 0..units.list.len() {
        let place = units.list[at].base;
        let source = unit_source(&units, at, text, scopes[at], marks, &variants);
        parts.push(Part { name: unit_name(file, outline, at, text), locate: Locate::Source(source), place });
    }
    Ok(())
}

/// The asked units of one file (each a `Lint` task's name and source), linted by `rules` and
/// what `marks` add to them (each rule whose lint the crate denies, `sans-io-core` in a core):
/// each its record, or why it failed, pushed on `did` in the order asked. Every unit is walked in
/// pre-order for its children's scopes; only an asked one runs the rules. A source that is not
/// the unit's today fails that task by name (no relint under another key, rule 3).
#[expect(clippy::too_many_arguments, reason = "one file's facts, each named; a struct would be built at its one caller")]
pub(crate) fn lint_units_in(
    outline: &blaze_file_items::Outline<'_>,
    text: &str,
    path: &str,
    file: ContentId,
    asked: &[(Name, Name, ContentId)],
    rules: &RuleSet,
    marks: RootMarks,
    alloc: Alloc<'_>,
    did: &mut Vec<Option<Result<Record<Global>, Why>>, Global>,
) -> Result<(), &'static str> {
    let marked = marks.rules(*rules);
    let rules = &marked;
    let units = Units::plan(outline.items(), text.len(), alloc).map_err(|_| UNPLANNED)?;
    let none = RuleSet::only(&[]);
    let count = units.list.len();
    // Which asked task each unit is, by its name: one search a unit.
    let mut wanted: AVec<'_, Option<usize>> = AVec::with_capacity_in(count, alloc);
    for at in 0..count {
        let name = unit_name(file, outline, at, text);
        wanted.push(asked.iter().position(|(_, base, _)| *base == name));
    }
    let mut made: AVec<'_, Option<Result<Record<Global>, Why>>> = AVec::with_capacity_in(asked.len(), alloc);
    for _ in 0..asked.len() {
        made.push(None);
    }
    let mut scopes: AVec<'_, Scope> = AVec::with_capacity_in(count, alloc);
    scopes.resize(count, file_scope_in(marks, path, text));
    let mut findings = Findings::new_in(alloc);
    let variants = Variants::gather(text, alloc).map_err(|_| UNGATHERED)?;
    for at in 0..count {
        let scope = scopes[at];
        let run = match wanted[at] {
            Some(_) => rules,
            None => &none,
        };
        let unit = units.unit(at, text, scope);
        let linted = blaze_lint::lint_in(run, &unit, &variants, units.list[at].base, alloc, &mut findings).map_err(|_| UNLINTED)?;
        for (child, given) in units.children_of(at).iter().zip(linted.children.iter()) {
            scopes[*child as usize] = *given;
        }
        let Some(task) = wanted[at] else { continue };
        let (name, _, source) = asked[task];
        made[task] = Some(match unit_source(&units, at, text, scope, marks, &variants) == source {
            true => Ok(record(name, rules, unit.text, &findings, &linted)),
            false => Err(Why::Refused { what: "a `Lint` unit whose text or scope is not the one its key was made from", rule: "blaze-cli lints.rs, lint_units_in" }),
        });
    }
    for slot in made.iter_mut() {
        did.push(Some(slot.take().unwrap_or(Err(Why::Refused { what: "a `Lint` part whose file holds no unit of its name", rule: "blaze-cli lints.rs, lint_units_in" }))));
    }
    Ok(())
}

/// One unit's record, named `name`: each finding of the pass `linted` (in `findings`) a drive
/// finding with its message's words; the payload the findings in the engine's form ([`payload`])
/// and then the pass's cost ([`COST_BYTES`]); the out a digest of the findings and words alone,
/// so a reader moves only when what it prints moves (a comment that adds a token moves the cost,
/// never the out).
fn record(name: Name, rules: &RuleSet, item: &str, findings: &Findings<'_>, linted: &blaze_lint::Linted<'_>) -> Record<Global> {
    let mut made = Record::new_in(name, Global);
    let run = linted.findings;
    let list = &findings.list[run.from as usize..run.to as usize];
    for finding in list {
        let mut words = blaze_own::String::new_in(Global);
        blaze_lint::show_message(rules, item, findings, finding, &mut words);
        made.find(LINT_BASE + finding.rule().id(), (finding.range.start, finding.range.end), &[words.as_str()]);
    }
    payload(findings, list, &mut made.payload);
    let mut h = Hasher::new(UNIT_OUT_DOMAIN);
    h.update(&made.payload).update(&made.messages);
    made.out = h.finish();
    made.payload.extend_from_slice(&linted.tokens.to_le_bytes());
    for sites in linted.cost.0 {
        made.payload.extend_from_slice(&sites.to_le_bytes());
    }
    made
}

/// The bytes a unit's cost takes at the end of its record's payload: its own tokens, then the
/// sites of each rule by id, each a little-endian `u32`.
pub(crate) const COST_BYTES: usize = 4 * (1 + Rule::COUNT);

/// A unit record's cost, read back from the end of its `payload` ([`record`]): its tokens and
/// each rule's sites. `None` for a payload shorter than a cost (not a unit's record).
pub(crate) fn cost_of(payload: &[u8]) -> Option<(u32, blaze_lint::Cost)> {
    let at = payload.len().checked_sub(COST_BYTES)?;
    let tail = &payload[at..];
    let word = |i: usize| u32::from_le_bytes([tail[4 * i], tail[4 * i + 1], tail[4 * i + 2], tail[4 * i + 3]]);
    let mut cost = blaze_lint::Cost::NONE;
    for (rule, sites) in cost.0.iter_mut().enumerate() {
        *sites = word(rule + 1);
    }
    Some((word(0), cost))
}

/// The engine's findings `list` as bytes (each little-endian): their count; then each finding's
/// template, range and fixes, each fix its edits, each edit its range and replacement text. What
/// `--fix` reads back to apply a fix (blaze-lint `apply`); the drive's findings hold only the
/// words.
fn payload(findings: &Findings<'_>, list: &[blaze_lint::Finding], out: &mut Vec<u8, Global>) {
    let word = |out: &mut Vec<u8, Global>, value: u32| out.extend_from_slice(&value.to_le_bytes());
    word(out, list.len() as u32);
    for finding in list {
        out.extend_from_slice(&(finding.template as u16).to_le_bytes());
        word(out, finding.range.start);
        word(out, finding.range.end);
        let fixes = findings.fixes_of(finding);
        word(out, fixes.len() as u32);
        for fix in fixes {
            let edits = findings.edits_of(fix);
            word(out, edits.len() as u32);
            for edit in edits {
                let text = findings.text_of(edit);
                word(out, edit.range.start);
                word(out, edit.range.end);
                word(out, text.len() as u32);
                out.extend_from_slice(text.as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected: "test lints::tests::the_step_is_read_and_a_bad_one_is_refused_by_name ... ok".
    #[test]
    fn the_step_is_read_and_a_bad_one_is_refused_by_name() {
        let step_of = |manifest: &str| lints_of(manifest).map(|lints| lints.step);
        assert_eq!(step_of("[workspace]\nmembers = []\n"), Ok(Step::Rust));
        assert_eq!(lints_of("[workspace]\nmembers = []\n"), Ok(Lints::NONE));
        assert_eq!(step_of("[workspace.metadata.blaze]\nlints = \"strict\"\n"), Ok(Step::Strict));
        assert!(matches!(step_of("[workspace.metadata.blaze]\nlints = \"strct\"\n"), Err(StepError::NoSuchStep { line: 2, .. })));
        assert!(matches!(step_of("[workspace.metadata.blaze]\nlints = 3\n"), Err(StepError::NotAString { .. })));
        assert_eq!(rule_of(LINT_BASE + Rule::Stops.id()), Some(Rule::Stops));
        assert_eq!(rule_of(crate::work::SYNTAX), None);

        // `lints-add`: two names read and run beside the step's rules, and the rule set's id (in
        // the `FileItems` and `Lint` tools) moves with them, so an edit of the key relints.
        let heap = Rule::from_name("ambient-heap").expect("`ambient-heap` is a rule");
        let no_async = Rule::from_name("no-async").expect("`no-async` is a rule");
        let added = lints_of("[workspace.metadata.blaze]\nlints = \"strict\"\nlints-add = [\"ambient-heap\", \"no-async\"]\n").expect("two rules added");
        assert_eq!(added, Lints { step: Step::Strict, rules: Step::Strict.rules().with(heap).with(no_async) });
        assert!(added.rules.runs(heap) && added.rules.runs(no_async) && !Step::Strict.rules().runs(heap));
        assert_ne!(added.rules.id(), Step::Strict.rules().id(), "`lints-add` moves the rule set's id");
        // No step named: the empty set and the named rules.
        let alone = lints_of("[workspace.metadata.blaze]\nlints-add = [\"no-async\"]\n").expect("one rule added");
        assert_eq!(alone, Lints { step: Step::Rust, rules: Step::Rust.rules().with(no_async) });
        assert_ne!(alone.rules.id(), Lints::NONE.rules.id());
        // An unknown name is refused naming the word and its line; nothing is skipped.
        let unknown = lints_of("[workspace.metadata.blaze]\nlints-add = [\"no-async\",\n  \"ambient-heep\"]\n");
        assert!(matches!(&unknown, Err(StepError::NoSuchRule { line: 3, word, .. }) if *word == "ambient-heep"), "{unknown:?}");
        assert!(matches!(lints_of("[workspace.metadata.blaze]\nlints-add = [3]\n"), Err(StepError::AddNotAString { line: 2, .. })));
        assert!(matches!(lints_of("[workspace.metadata.blaze]\nlints-add = \"no-async\"\n"), Err(StepError::AddNotAnArray { line: 2, .. })));
    }

    /// Expected: "test lints::tests::a_crates_marks_reach_its_rules_levels_and_keys ... ok".
    #[test]
    fn a_crates_marks_reach_its_rules_levels_and_keys() {
        let root = "#![no_std]\n#![blaze::sans_io]\n#![deny(blaze::std_use)]\n\nmod m;\n";
        let module = "//! A module of the core.\nfn f() {}\n";
        let marks = marks_of(root, module);
        assert!(marks.sans_io && marks.denies(Rule::Ambient));
        // The crate's deny makes its own finding an error; another crate's stays the step's.
        let ambient = number_of(Rule::Ambient);
        assert_eq!(level_in(Step::Strict, marks, ambient), Level::Error);
        assert_eq!(level_in(Step::Strict, RootMarks::NONE, ambient), Step::Strict.levels().of(Rule::Ambient));
        assert_eq!(level_in(Step::Strict, marks_of("", module), ambient), level_in(Step::Strict, RootMarks::NONE, ambient));
        // A core's files are in the core, and the core's rule runs there.
        assert!(marks.scope(file_scope("crates/c/src/m.rs")).sans_io);
        assert!(file_scope_in(marks, "crates/c/src/m.rs", module).sans_io);
        // The file's own facts reach its scope: a declared `mod io` and its crate's statics.
        let facts = file_scope_in(RootMarks::NONE, "crates/blaze-trace/src/record.rs", "mod io;\n");
        assert!(facts.own_heads != OwnHeads::NONE && facts.statics != 0);
        assert!(marks.rules(Step::Rust.rules()).runs(Rule::SansIoCore));
        // The marks are in the key: a unit of a marked file is not the unmarked one's.
        assert_ne!(marks.key(), marks_of("", module).key());
    }
}
