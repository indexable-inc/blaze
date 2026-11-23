//! Lints in `blaze check` (docs/v2/plan/design/lint.md; key.rs `Kind::Lint`). A file's lint
//! units are blaze-lint's ([`blaze_lint::Units`]): unit 0 the file, its top-level items left out
//! as holes; unit `i + 1` the outline's entry `i`, its children left out. Each is a `Lint` part
//! of its file's `FileItems` record ([`parts_in`]), named by its file and its entry's chain
//! ([`unit_name`]) and located by what the unit IS ([`UnitKey`], [`Structural`]): its OWN text
//! (blaze-lint `Units::own`: its bytes with each child one `HOLE_MARK`, never a child's offset
//! or length), the scope its parent's text gives it and what `variant-count` reads of the file;
//! its kind's tool holds the rule set's id (home.rs `tools`). So its key is computed before the
//! work from exactly what its pass reads, and the task reads nothing: an edit inside one item
//! relints that item alone, whatever it does to the item's length; a child added or removed
//! relints its parent (a mark more or less), never its siblings.
//!
//! A unit is NOT a task (rule 77, sparse rows): the file is ([`file_lints_name`], [`file_part`],
//! one row a file), its record listing every unit's name and key and the findings of the units
//! that have any (engine_lints.rs `file_record`). A unit listed with its key and no findings is
//! clean and valid by its key: no row of its own, nothing entered for it on a replay; an
//! entered file relints only the units whose key its last record does not list.
//!
//! A unit's record ([`lint_batch`], [`record`]): one drive finding a lint finding, numbered [`LINT_BASE`]
//! plus its rule's id, its range a range of the unit's OWN text (rule 71: no child's length is
//! in it, so a child that grows moves no stored place; the printer gives each child before it
//! its length again from today's outline, [`own_to_item`]), its message the engine's words
//! ([`blaze_lint::show_message`]); its payload the engine's findings with their fixes, for
//! `--fix` ([`payload`]), their ranges own ranges too. No level is stored (rule 81): print.rs
//! gives each the level the step's [`blaze_lint::Levels`] say, at print time.
//!
//! The Lints stage reads of a file only what its asked units need ([`lint_units_in`]): each asked
//! unit's own text, and its ancestors' own texts walked with no rule for the scope each gives its
//! child; `OwnHeads` and `variant-count`'s whole-file lexes only when a rule of the set reads
//! them. What it did is counted ([`LintCounts`]) and printed in the profile.
//!
//! Which rules run is what the root `Cargo.toml` names under `[workspace.metadata.blaze]`
//! ([`lints_of`]): the step (`lints = "rust" | "suggest" | "edition" | "strict"`; none named is
//! [`Step::Rust`]) and the rules it adds by name (`lints-add = ["ambient-heap", ..]`, rules no
//! step runs). An empty rule set lists no `Lint` part and runs no task.

use blaze_drive::{Kind, Locate, Name, Part, Why};
use blaze_ids::{ContentId, Hasher};
use blaze_lint::{Findings, Geometry, Level, OwnHeads, OwnUnit, RootMarks, Rule, RuleSet, Scope, Span, Step, TestBits, UnitPlan, Units, Variants, VariantsView, statics_of_path};
use blaze_own::{AVec, Alloc, Vec};
use std::alloc::Global;

use crate::work::Record;

/// The first finding number of a lint: a finding numbered `LINT_BASE + rule.id()` is the lint
/// `rule`'s. Far from every other number the cli and the drive use (1..=0x40, 0xC0F6..).
pub const LINT_BASE: u16 = 0x4C00;

const UNIT_NAME_DOMAIN: &str = "blaze-cli lint unit name 1";
/// 3: `variant-count`'s read is in a unit's source only when the rule runs ([`parts_in`]: the
/// outline is handed the rule set); 2 held it whatever the rules; 2: a unit's source is its own
/// text ([`Structural`]); 1 hashed its children's offsets.
const UNIT_SOURCE_DOMAIN: &str = "blaze-cli lint unit source 3";
/// The name of a file's ONE `Lint` task ([`file_lints_name`]).
const FILE_LINTS_NAME_DOMAIN: &str = "blaze-cli lint file name 1";
/// The source of a file's `Lint` part ([`file_part`]): its units' names and keys.
const FILE_LINTS_SOURCE_DOMAIN: &str = "blaze-cli lint file source 1";
/// 2: a record's ranges are ranges of the unit's own text; in 1 they held its children's lengths.
const UNIT_OUT_DOMAIN: &str = "blaze-cli lint unit out 2";

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
/// the IO module names the file makes its own (`OwnHeads::read`, only where a rule reads them:
/// [`reads_own_heads`]) and its crate's rows of the allowed statics (`statics_of_path`, by
/// `path` relative to the workspace's root). All of it is in the scope's key, so it is in every
/// unit's source ([`UnitKey`]).
pub(crate) fn file_scope_in(marks: RootMarks, path: &str, text: &str) -> Scope {
    let heads = if reads_own_heads(marks, path) { OwnHeads::read(text) } else { OwnHeads::NONE };
    marks.scope(file_scope(path)).with_file_facts(heads, statics_of_path(path))
}

/// Whether a file's passes read the IO module names its text makes its own (`OwnHeads::read`,
/// a lex of the whole file): only in a sans-IO core, where its marks run `sans-io-core` whatever
/// the step (blaze-lint `RootMarks::rules`), the one rule that reads them
/// (`Scope::reads_own_heads`). Elsewhere the fact is `OwnHeads::NONE` in the key and the pass
/// alike, and no lex is made for it (rule 77). The one decision, for the outline's keys (work.rs
/// `outline_file`) and the Lints stage.
pub(crate) fn reads_own_heads(marks: RootMarks, path: &str) -> bool {
    marks.scope(file_scope(path)).reads_own_heads(&marks.rules(RuleSet::only(&[])))
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
    unit_name_of(file, at.checked_sub(1).and_then(|i| outline.items().get(i)).map(|item| crate::work::item_chain(outline, item, text)))
}

/// A unit's name from its item's chain (work.rs `item_chain`; `None`: the file's unit): the one
/// derivation, for an outline ([`unit_name`]) and for a stored outline (engine_lints.rs).
pub(crate) fn unit_name_of(file: ContentId, chain: Option<u64>) -> Name {
    let mut h = Hasher::new(UNIT_NAME_DOMAIN);
    h.id(&file);
    match chain {
        None => h.update(b"file"),
        Some(chain) => h.update(b"item").update(&chain.to_le_bytes()),
    };
    Name::new(Kind::Lint, h.finish())
}

/// The name of file `file`'s ONE `Lint` task (engine_lints.rs `plan`): its units are not tasks,
/// they are listed with their keys in the file's `FileItems` record (each a `Lint` part) and in
/// its lint record, and a clean one has no row (rule 77: valid by its key).
pub(crate) fn file_lints_name(file: ContentId) -> Name {
    let mut h = Hasher::new(FILE_LINTS_NAME_DOMAIN);
    h.id(&file);
    Name::new(Kind::Lint, h.finish())
}

/// File `file`'s `Lint` part over its unit parts `units` (as [`parts_with`] pushed them, in unit
/// order): named [`file_lints_name`], located by every unit's name and key in that order, so it
/// moves exactly when one unit's key does, or a unit comes or goes (the forward wake wakes it by
/// its source, engine_ready_edit.rs `keyed`). At place 0, as the file's unit.
pub(crate) fn file_part(file: ContentId, units: &[Part]) -> Part {
    let mut h = Hasher::new(FILE_LINTS_SOURCE_DOMAIN);
    h.update(&(units.len() as u32).to_le_bytes());
    for unit in units.iter() {
        h.update(&unit.name.subject.0);
        match unit.locate {
            Locate::Source(key) => h.update(&[1]).update(&key.0),
            Locate::File { .. } | Locate::Key(_) => h.update(&[0]),
        };
    }
    Part { name: file_lints_name(file), locate: Locate::Source(h.finish()), place: 0 }
}

/// What a unit's key (its `Lint` part's source) reads of it: the one seam where how a unit is
/// keyed is chosen (rule 78). [`Keyed`] is the build's; [`ByOffsets`], the key before it, is the
/// planted control of the tests (rule 23). A source is what the unit's findings are a function
/// of, but the rule set (in the task's tool): the unit, its scope's key, its file's marks
/// ([`RootMarks::key`]: the rules a crate's deny or core mark adds to the workspace's set are a
/// function of them) and what `variant-count` reads of the rest of its file (the enums of its
/// literals' counts; nothing when none, or when the rule does not run: `variants` is `None`).
pub(crate) trait UnitKey {
    /// Unit `at` of `text` (`units` planned from it), whose own text is `own`, in `scope`.
    fn source(units: &Units<'_>, at: usize, text: &str, own: &OwnUnit<'_>, scope: Scope, marks: RootMarks, variants: Option<&Variants<'_>>) -> ContentId;
}

/// The unit as its pass reads it ([`OwnUnit`]): its own text, each child one mark, with the
/// marks' places in it; `variant-count`'s read at its own places (blaze-lint
/// `VariantsView::unit_key_own`). No child's offset or length is in it: an edit inside a child
/// moves that child's key alone, whatever it does to the child's length, and a child added or
/// removed moves its parent's (a mark more or less), never a sibling's.
pub(crate) struct Structural;

impl UnitKey for Structural {
    fn source(units: &Units<'_>, at: usize, _text: &str, own: &OwnUnit<'_>, scope: Scope, marks: RootMarks, variants: Option<&Variants<'_>>) -> ContentId {
        let read = variants.and_then(|variants| variants.view().unit_key_own(geometry(units, at)));
        let mut h = Hasher::new(UNIT_SOURCE_DOMAIN);
        h.update(&(own.text.len() as u32).to_le_bytes()).update(&scope.key().to_le_bytes()).update(&marks.key().to_le_bytes());
        match read {
            Some(read) => h.update(&[1]).update(&read),
            None => h.update(&[0]),
        };
        // The marks' places: a literal `{}` of the unit's text is not a child's mark.
        h.update(&(own.holes.len() as u32).to_le_bytes());
        for mark in own.holes.iter() {
            h.update(&mark.start.to_le_bytes());
        }
        h.update(own.text.as_bytes());
        h.finish()
    }
}

/// The key before [`Structural`], kept as the tests' planted control (rule 23): the unit's
/// whole range, each child's bytes left out but its unit-relative start and end hashed, the
/// unit's length, and `variant-count`'s read at unit-relative places. A child that grows moves
/// its parent's key and every ancestor's (their holes moved).
#[cfg(test)]
pub(crate) struct ByOffsets;

#[cfg(test)]
impl UnitKey for ByOffsets {
    fn source(units: &Units<'_>, at: usize, text: &str, _own: &OwnUnit<'_>, scope: Scope, marks: RootMarks, variants: Option<&Variants<'_>>) -> ContentId {
        let plan = units.list[at];
        let own = &text.as_bytes()[plan.base as usize..plan.end as usize];
        let mut h = Hasher::new("blaze-cli lint unit source 1");
        h.update(&(own.len() as u32).to_le_bytes()).update(&scope.key().to_le_bytes()).update(&marks.key().to_le_bytes());
        if let Some(read) = variants.and_then(|variants| variants.unit_key(plan.base, plan.end - plan.base, units.holes_of(at))) {
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
}

/// THE SWITCH of how a unit is keyed ([`UnitKey`]).
pub(crate) type Keyed = Structural;

/// Unit `at`'s place in its file, as `variant-count` finds its sites by.
fn geometry<'u>(units: &'u Units<'_>, at: usize) -> Geometry<'u> {
    let plan = units.list[at];
    Geometry { base: plan.base, len: plan.end - plan.base, holes: units.holes_of(at) }
}

/// Why a file's units could not be planned or linted: blaze-lint's refusal, named.
const UNPLANNED: &str = "a file's outline is not one of lint units (blaze-lint `Units::plan`)";
const UNLINTED: &str = "a lint unit could not be linted (blaze-lint `lint`)";
const UNGATHERED: &str = "a file's `variant-count` fact could not be gathered (blaze-lint `Variants::gather`)";

/// Each unit of `text` as a `Lint` part, pushed on `parts`, keyed by [`Keyed`] ([`parts_with`]),
/// then the file's own `Lint` part over them ([`file_part`]): the file's one task.
#[expect(clippy::too_many_arguments, reason = "one file's facts, each named; its one caller is the outline job")]
pub(crate) fn parts_in<A: core::alloc::Allocator + Copy>(
    outline: &blaze_file_items::Outline<'_>,
    text: &str,
    path: &str,
    file: ContentId,
    marks: RootMarks,
    rules: &RuleSet,
    alloc: Alloc<'_>,
    parts: &mut Vec<Part, A>,
) -> Result<(), &'static str> {
    let from = parts.len();
    parts_with::<Keyed, A>(outline, text, path, file, marks, rules, alloc, parts)?;
    let whole = file_part(file, &parts[from..]);
    parts.push(whole);
    Ok(())
}

/// Each unit of `text` as a `Lint` part keyed by `K`, pushed on `parts` in unit order: each
/// unit's own text walked in pre-order by a pass of no rule for the scope it gives each child
/// (what a scope is does not depend on the rules; the units' own texts together are the file
/// once), from the file's scope as `marks` make it (in a core or not); each unit's place its
/// start in the file. `variant-count`'s whole-file fact (a lex of the file) gathered only when
/// `rules` with what `marks` add run it, as the Lints stage gathers it (rule 77). Scratch in
/// `alloc`.
#[expect(clippy::too_many_arguments, reason = "one file's facts, each named")]
pub(crate) fn parts_with<K: UnitKey, A: core::alloc::Allocator + Copy>(
    outline: &blaze_file_items::Outline<'_>,
    text: &str,
    path: &str,
    file: ContentId,
    marks: RootMarks,
    rules: &RuleSet,
    alloc: Alloc<'_>,
    parts: &mut Vec<Part, A>,
) -> Result<(), &'static str> {
    let units = Units::plan(outline.items(), text.len(), alloc).map_err(|_| UNPLANNED)?;
    let none = RuleSet::only(&[]);
    let count = units.list.len();
    let mut scopes: AVec<'_, Scope> = AVec::with_capacity_in(count, alloc);
    scopes.resize(count, file_scope_in(marks, path, text));
    let variants = match marks.rules(*rules).runs(Rule::VariantCount) {
        true => Some(Variants::gather(text, alloc).map_err(|_| UNGATHERED)?),
        false => None,
    };
    let mut findings = Findings::new_in(alloc);
    for at in 0..count {
        let own = units.own(at, text, alloc).map_err(|_| UNLINTED)?;
        let scope = scopes[at];
        findings.clear();
        let linted = blaze_lint::lint_own_in(&none, &own, scope, geometry(&units, at), None, alloc, &mut findings).map_err(|_| UNLINTED)?;
        for (child, given) in units.children_of(at).iter().zip(linted.children.iter()) {
            scopes[*child as usize] = *given;
        }
        let mut source = K::source(&units, at, text, &own, scope, marks, variants.as_ref());
        // Top-level `use` items under `Rule::UnusedImport` read identifier uses from sibling
        // items in the file, so mix the whole-file digest into their unit key (rule 65).
        if at > 0 && marks.rules(*rules).runs(Rule::UnusedImport) && own.text.contains("use") {
            let mut h = Hasher::new(UNIT_SOURCE_DOMAIN);
            h.update(&source.0).update(&file.0);
            source = h.finish();
        }
        parts.push(Part { name: unit_name(file, outline, at, text), locate: Locate::Source(source), place: units.list[at].base });
    }
    Ok(())
}

const UNSCOPED: &str = "a lint unit's parent was not walked before it (blaze-cli lints.rs `lint_units_in`)";
const UNPOOLED: &str = "a batch of a file's lint units failed on the pool (a panic or a stop: its task's error names it)";

/// One asked `Lint` task of a file: its name (its unit's: the part's) and its unit's first byte
/// in the file (the part's `place`), by which its unit is found and then checked by its name.
pub(crate) type AskedUnit = (Name, u32);

/// What the Lints stage did, counted per file job and summed for the profile
/// (engine_lints.rs `profile`). The floor of an edit inside one function (rule 67): its file
/// read 1 (the outline job's read is the other, owed: the outline keeps no text), outlined 0
/// (the planner's outline, handed over), units linted 1, units walked = its ancestors (no rule: the scope each
/// gives its child), bytes lexed = their own texts and its own (each byte of the file once at
/// most), the whole-file facts 0 but where a rule of the set reads them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LintCounts {
    /// Files read (each checked against its outline's id).
    pub(crate) files_read: u64,
    /// Files the lint job outlined again: none since the job is handed its planner's outline
    /// record (its entries and chains read in place, engine_lints.rs `lint_text`); kept as the
    /// count that shows it.
    pub(crate) files_outlined: u64,
    /// Ancestors of asked units walked with no rule, for the scopes they give.
    pub(crate) units_walked: u64,
    /// Asked units linted by the rules: each its record.
    pub(crate) units_linted: u64,
    /// The own bytes of every unit walked or linted: what the passes lexed.
    pub(crate) bytes_lexed: u64,
    /// Whole-file `variant-count` facts gathered (a lex of the file each).
    pub(crate) variants_gathered: u64,
    /// Whole-file `OwnHeads` read (a lex of the file each).
    pub(crate) heads_read: u64,
    /// Pool tasks a file's asked units were split into ([`Split`]); 0 when linted in the file's
    /// own task.
    pub(crate) batches: u64,
}

impl LintCounts {
    /// The counts as one row, in the order the fields are written: the one list a sum reads.
    pub(crate) const fn row(&self) -> [u64; 8] {
        [self.files_read, self.files_outlined, self.units_walked, self.units_linted, self.bytes_lexed, self.variants_gathered, self.heads_read, self.batches]
    }

    /// [`LintCounts::row`]'s inverse.
    pub(crate) const fn of_row(row: [u64; 8]) -> LintCounts {
        let [files_read, files_outlined, units_walked, units_linted, bytes_lexed, variants_gathered, heads_read, batches] = row;
        LintCounts { files_read, files_outlined, units_walked, units_linted, bytes_lexed, variants_gathered, heads_read, batches }
    }
}

/// How a file's asked units are spread over the workers: the one seam (rule 78), [`Splitting`]
/// the build's. Either way each unit's record is a function of its own inputs alone, and the
/// records are merged in unit order, so both answer the same bytes (lint_holes_tests.rs).
pub(crate) trait Split {
    /// The most asked units one pool task lints; `None`: every one in its file's own task.
    const BATCH: Option<usize>;
}

/// Every asked unit of a file in the file's own task: the control.
pub(crate) struct Serial;

impl Split for Serial {
    const BATCH: Option<usize> = None;
}

/// A file with more asked units than [`UNITS_A_BATCH`] split into pool tasks of that many, so
/// one huge file (a cold check of a generated file of thousands of items) is not one worker's.
pub(crate) struct Batched;

impl Split for Batched {
    const BATCH: Option<usize> = Some(UNITS_A_BATCH);
}

/// THE SWITCH of how a file's asked units are spread ([`Split`]).
pub(crate) type Splitting = Batched;

/// The asked units one pool task lints: a few milliseconds of passes (a function's unit is tens
/// of microseconds), far above a task's cost, and small enough that a file of 10,000 items is
/// 157 tasks for the pool's workers.
const UNITS_A_BATCH: usize = 64;

/// A batch's frame: its units' own texts and findings (each unit's at most its file's bytes).
const BATCH_FRAME: usize = 1 << 26;

/// One asked unit as a batch lints it: plain values a worker can take.
#[derive(Clone, Copy)]
struct Lone {
    name: Name,
    plan: UnitPlan,
    scope: Scope,
}

/// What every batch of a file reads: plain borrows a worker can take.
#[derive(Clone, Copy)]
struct Shared<'s> {
    text: &'s str,
    /// Every unit's holes ([`Units::holes`]).
    holes: &'s [Span],
    rules: &'s RuleSet,
    variants: Option<VariantsView<'s>>,
}

/// The asked units of one file, linted by `rules` and what `marks` add to them (each rule whose
/// lint the crate denies, `sans-io-core` in a core): each its unit's number and record, or why
/// it failed, pushed on `did` in the order asked. Only what the asked units need is read
/// ([`LintCounts`]): each asked unit found by its place and checked by its name; its ancestors'
/// own texts walked once each with no rule for the scope each gives its child; the asked units'
/// own texts linted, split by `S` ([`Split`]); `variant-count`'s and `OwnHeads`' whole-file
/// facts only where a rule of the set reads them. A unit's source is not computed again: the
/// file's text was checked against the id the outline's keys were made from (rule 74).
/// `items` are the outline's entries (an outline's, or its stored record's: the planner's,
/// handed over, never outlined again); `chain_of` entry `i`'s chain (work.rs `item_chain`), by
/// which an asked unit is checked by its name.
#[expect(clippy::too_many_arguments, reason = "one file's facts, each named; a struct would be built at its one caller")]
pub(crate) fn lint_units_in<S: Split>(
    cx: &bstd::cx::Cx,
    items: &[blaze_surface::Item],
    chain_of: &dyn Fn(usize) -> u64,
    text: &str,
    path: &str,
    file: ContentId,
    asked: &[AskedUnit],
    rules: &RuleSet,
    marks: RootMarks,
    alloc: Alloc<'_>,
    counts: &mut LintCounts,
    did: &mut Vec<Result<(usize, Record<Global>), Why>, Global>,
) -> Result<(), &'static str> {
    let marked = marks.rules(*rules);
    let rules = &marked;
    let units = Units::plan(items, text.len(), alloc).map_err(|_| UNPLANNED)?;
    let count = units.list.len();
    // 1. Each asked task's unit, in unit order: (unit, task).
    let mut jobs: AVec<'_, (usize, usize)> = AVec::with_capacity_in(asked.len(), alloc);
    for (task, (name, place)) in asked.iter().enumerate() {
        if let Some(unit) = unit_at(items, chain_of, file, *name, *place) {
            jobs.push((unit, task));
        }
    }
    jobs.sort_unstable();
    // 2. Each asked unit's scope: its ancestors walked outermost first, each once, no rule.
    let mut scopes: AVec<'_, Option<Scope>> = AVec::with_capacity_in(count, alloc);
    scopes.resize(count, None);
    scopes[0] = Some(file_scope_in(marks, path, text));
    if reads_own_heads(marks, path) {
        counts.heads_read += 1;
    }
    let mut walked: AVec<'_, bool> = AVec::with_capacity_in(count, alloc);
    walked.resize(count, false);
    let none = RuleSet::only(&[]);
    let mut findings = Findings::new_in(alloc);
    let mut stack: AVec<'_, usize> = AVec::new_in(alloc);
    for &(unit, _) in jobs.iter() {
        stack.clear();
        let mut up = Units::parent_of(items, unit);
        // Bounded: one turn an ancestor.
        while let Some(parent) = up {
            if walked[parent] {
                break;
            }
            stack.push(parent);
            up = Units::parent_of(items, parent);
        }
        while let Some(parent) = stack.pop() {
            let scope = scopes[parent].ok_or(UNSCOPED)?;
            let own = units.own(parent, text, alloc).map_err(|_| UNLINTED)?;
            findings.clear();
            let linted = blaze_lint::lint_own_in(&none, &own, scope, geometry(&units, parent), None, alloc, &mut findings).map_err(|_| UNLINTED)?;
            for (child, given) in units.children_of(parent).iter().zip(linted.children.iter()) {
                scopes[*child as usize] = Some(*given);
            }
            walked[parent] = true;
            counts.units_walked += 1;
            counts.bytes_lexed += own.text.len() as u64;
        }
    }
    // 3. The file's facts the rules read.
    let variants = match rules.runs(Rule::VariantCount) {
        true => {
            counts.variants_gathered += 1;
            Some(Variants::gather(text, alloc).map_err(|_| UNGATHERED)?)
        }
        false => None,
    };
    let mut lone: AVec<'_, Lone> = AVec::with_capacity_in(jobs.len(), alloc);
    for &(unit, task) in jobs.iter() {
        lone.push(Lone { name: asked[task].0, plan: units.list[unit], scope: scopes[unit].ok_or(UNSCOPED)? });
    }
    let shared = Shared { text, holes: &units.holes, rules, variants: variants.as_ref().map(Variants::view) };
    // 4. The asked units linted, merged in unit order.
    let mut made: Vec<Record<Global>, Global> = Vec::with_capacity_in(lone.len(), Global);
    match S::BATCH {
        Some(batch) if lone.len() > batch => {
            let scope = cx.scope();
            let mut running = Vec::with_capacity_in(lone.len().div_ceil(batch), Global);
            for chunk in lone.chunks(batch) {
                running.push(scope.may_overlap("lint units, a batch of a file's", BATCH_FRAME, move |cx: &bstd::cx::Cx| {
                    let erased = blaze_own::Erased::new(cx.frame());
                    Ok::<_, core::convert::Infallible>(lint_batch(shared, chunk, erased.handle()))
                }));
            }
            counts.batches += running.len() as u64;
            let mut ran = Vec::with_capacity_in(running.len(), Global);
            for handle in running.into_iter() {
                ran.push(handle.join(cx).ok());
            }
            scope.join(cx).map_err(|_| UNPOOLED)?;
            for one in ran.into_iter() {
                let (records, bytes) = one.ok_or(UNPOOLED)??;
                made.extend(records);
                counts.bytes_lexed += bytes;
            }
        }
        _ => {
            let (records, bytes) = lint_batch(shared, &lone, alloc)?;
            made.extend(records);
            counts.bytes_lexed += bytes;
        }
    }
    counts.units_linted += made.len() as u64;
    // 5. Each answer in the order asked.
    let mut answers: AVec<'_, Option<(usize, Record<Global>)>> = AVec::with_capacity_in(asked.len(), alloc);
    answers.resize_with(asked.len(), || None);
    for (&(unit, task), record) in jobs.iter().zip(made) {
        answers[task] = Some((unit, record));
    }
    for answer in answers.iter_mut() {
        did.push(answer.take().ok_or(Why::Refused { what: "a `Lint` part whose file holds no unit of its name at its place", rule: "blaze-cli lints.rs, lint_units_in" }));
    }
    Ok(())
}

/// The unit of `items` whose `Lint` part is `name` at `place`: the entry starting there (an
/// outline's entries start in pre-order, so their starts rise), or the file's unit at 0,
/// checked by its name ([`unit_name_of`], each entry's chain by `chain_of`). `None`: no unit of
/// that name starts there (its task fails by name).
fn unit_at(items: &[blaze_surface::Item], chain_of: &dyn Fn(usize) -> u64, file: ContentId, name: Name, place: u32) -> Option<usize> {
    let from = items.partition_point(|item| item.range.start < place);
    let entries = items[from..].iter().take_while(|item| item.range.start == place).enumerate().map(|(k, _)| from + k + 1);
    entries.chain((place == 0).then_some(0)).find(|&at| unit_name_of(file, at.checked_sub(1).map(chain_of)) == name)
}

/// The units `lone` linted by `shared.rules` in `alloc`: each one's record, in order, and the
/// own bytes lexed. A function of its inputs alone, so a batch on a worker answers what the
/// file's own task would ([`Split`]).
fn lint_batch(shared: Shared<'_>, lone: &[Lone], alloc: Alloc<'_>) -> Result<(Vec<Record<Global>, Global>, u64), &'static str> {
    let mut records = Vec::with_capacity_in(lone.len(), Global);
    let mut bytes = 0u64;
    let mut findings = Findings::new_in(alloc);
    for one in lone {
        let holes = blaze_lint::holes_in(one.plan, shared.holes);
        let own = blaze_lint::own_text(one.plan, holes, shared.text, alloc).map_err(|_| UNLINTED)?;
        findings.clear();
        let at = Geometry { base: one.plan.base, len: one.plan.end - one.plan.base, holes };
        let mut linted = blaze_lint::lint_own_in(shared.rules, &own, one.scope, at, shared.variants, alloc, &mut findings).map_err(|_| UNLINTED)?;
        if shared.rules.runs(Rule::UnusedImport) && holes.is_empty() && own.text.contains("use") {
            blaze_lint::unused_imports_for_range(shared.text, one.scope, one.plan.base, one.plan.end, alloc, &mut findings, &mut linted).map_err(|_| UNLINTED)?;
        }
        bytes += own.text.len() as u64;
        records.push(record(one.name, shared.rules, own.text.as_str(), &findings, &linted));
    }
    Ok((records, bytes))
}

/// A stored own range of a unit's record placed in its file as it is today: the unit found at
/// `start` (its item's start, the item found by its chain; `None`: the file's unit), and each of
/// its children's lengths given back to the places after it (blaze-lint `real_place`, walking
/// today's outline: rule 71). The unit's start and the range from it, as print.rs places a
/// finding; `None` when no entry starts there or its children do not walk (it is listed
/// unplaced, as a finding whose item is gone).
pub(crate) fn own_to_item(outline: &blaze_file_items::Outline<'_>, start: Option<u32>, own: (u32, u32)) -> Option<(u32, (u32, u32))> {
    let items = outline.items();
    let (unit, base) = match start {
        None => (0, 0),
        Some(start) => {
            let at = items.partition_point(|item| item.range.start < start);
            (items.get(at).filter(|item| item.range.start == start).map(|_| at + 1)?, start)
        }
    };
    let place = |own: u32| {
        let mut holes = blaze_lint::ChildHoles::of(items, unit);
        let real = blaze_lint::real_place(holes.by_ref().map(|(_, hole)| hole), own);
        real.filter(|_| holes.bad.is_none())
    };
    Some((base, (place(own.0)?, place(own.1)?)))
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

/// A unit record's findings in the engine's form ([`payload`]), its `payload` before the cost:
/// what its file's lint record keeps of it (engine_lints.rs `file_record`). `None` for a
/// payload shorter than a cost.
pub(crate) fn fixes_of(payload: &[u8]) -> Option<&[u8]> {
    payload.len().checked_sub(COST_BYTES).map(|at| &payload[..at])
}

/// Mechanical single-edit fix for a lint finding at file-relative `start..end`, copied into
/// `frame`. Runs only on files that actually produced an auto-fixable lint finding, keeping
/// clean checks at zero cost.
pub(crate) fn fix_at<'f>(
    rule: Rule,
    text: &str,
    start: u32,
    end: u32,
    frame: &'f bstd::mem::Frame,
    scratch: &bstd::mem::Frame,
) -> Option<crate::fix::Change<'f>> {
    if !matches!(rule, Rule::UnusedImport | Rule::UnusedMut | Rule::Renamed | Rule::VariantCount) {
        return None;
    }
    let erased = blaze_own::Erased::new(scratch);
    let alloc = erased.handle();
    let mut findings = Findings::new_in(alloc);
    let scope = Scope::file(TestBits::NONE, false, false);
    if rule == Rule::UnusedImport {
        let _ = blaze_lint::lint_file_imports(text, scope, alloc, &mut findings);
    }
    let only = RuleSet::only(&[rule]);
    let _ = blaze_lint::lint(&only, &blaze_lint::Unit { text, holes: &[], scope }, alloc, &mut findings);
    let want = Span::new(start, end);
    let finding = findings.list.iter().find(|f| f.range == want)?;
    let fix = findings.fixes_of(finding).first()?;
    let edits = findings.edits_of(fix);
    let [edit] = edits else { return None };
    let insert = if edit.text.is_empty() {
        ""
    } else {
        frame.alloc_str(findings.text_of(edit))
    };
    Some(crate::fix::Change {
        start: edit.range.start,
        end: edit.range.end,
        insert,
        reason: blaze_report::frame::words::HelpKind::LintFix,
        args: [rule.name(), ""],
        given: 1,
    })
}

/// The engine's findings `list` as bytes (each little-endian): their count; then each finding's
/// template, range and fixes, each fix its edits, each edit its range and replacement text, every
/// range one of the unit's OWN text (a reader places it as [`own_to_item`] does). What `--fix`
/// is to read back to apply a fix (blaze-lint `apply`; nothing reads it yet); the drive's
/// findings hold only the words.
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
        // The file's own facts reach its scope: its crate's statics, and in a core (the one place
        // a rule reads it) a declared `mod io`. Outside a core no lex is made for it.
        let facts = file_scope_in(RootMarks::NONE, "crates/blaze-trace/src/record.rs", "mod io;\n");
        assert!(facts.own_heads == OwnHeads::NONE && facts.statics != 0);
        assert!(!reads_own_heads(RootMarks::NONE, "crates/blaze-trace/src/record.rs"));
        let core = file_scope_in(marks_of(root, "mod io;\n"), "crates/c/src/m.rs", "mod io;\n");
        assert!(core.own_heads != OwnHeads::NONE, "a core's file reads its own heads");
        assert!(marks.rules(Step::Rust.rules()).runs(Rule::SansIoCore));
        // The marks are in the key: a unit of a marked file is not the unmarked one's.
        assert_ne!(marks.key(), marks_of("", module).key());
    }
}
