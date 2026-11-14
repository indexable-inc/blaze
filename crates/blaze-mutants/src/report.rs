//! Surviving mutants as diagnostics, and scores.
//!
//! A survivor is shown like a lint: the rule (one per operator, `mutants::cmp_boundary`), the
//! item and the item-relative position (a file and line only when printed, rule 71), the original
//! and the mutated line, the tests that reached it and passed, and the test it asks for. Its id
//! is the item's form: the rule, the item, the item-relative position and the mutation, so it is
//! stable across edits elsewhere in the file. A function no test reaches is one diagnostic,
//! `mutants::not_reached`, not one per mutant.
//!
//! A [`Score`] is per function and per change, for people and agents to read: never a gate.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::BTreeMap;
use std::fmt;

use blaze_ids::{ContentId, Hasher};
use blaze_program::key::ItemId;
use blaze_program::record::TestKey;

use crate::arid::EquivRule;
use crate::operator::{Mutation, Operator, decimal, mutated_line, token_text};
use crate::run::Outcome;
use crate::select::{Skipped, Universe};
use crate::site::{FnFacts, ItemPos, Site, SiteKind};
use crate::verdict::Verdict;

const ID_DOMAIN: &str = "blaze mutants diagnostic 1";

/// The rule of a function no test reaches.
pub const NOT_REACHED: &str = "mutants::not_reached";

/// One finding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Diagnostic {
    /// [`Operator::rule`], or [`NOT_REACHED`].
    pub rule: &'static str,
    pub item: ItemId,
    pub path: Vec<u8, Global>,
    pub pos: ItemPos,
    pub original: Vec<u8, Global>,
    pub mutated: Vec<u8, Global>,
    /// The names of the tests that reached it and passed, in key order.
    pub tests: Vec<Vec<u8, Global>, Global>,
    pub headline: Vec<u8, Global>,
    pub suggestion: Vec<u8, Global>,
    pub id: ContentId,
}

/// Why a diagnostic could not be made or printed: the thing it names is missing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReportError {
    /// A verdict of an item the universe has no facts of.
    UnknownItem { item: ItemId },
    /// A verdict of a site the item's facts do not have.
    UnknownSite { item: ItemId, site: u32 },
    /// A survivor's test the universe does not list.
    UnknownTest { test: TestKey },
    /// No file position for an item-relative one.
    Unlocated { item: ItemId, pos: ItemPos },
}

impl fmt::Display for ReportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReportError::UnknownItem { item } => write!(f, "a mutant verdict of function {}, of which there are no facts", item.0),
            ReportError::UnknownSite { item, site } => write!(f, "a mutant verdict of site {site} of function {}, which has no such site", item.0),
            ReportError::UnknownTest { test } => write!(f, "a surviving mutant reached test {}, which is not listed", test.0),
            ReportError::Unlocated { item, pos } => {
                write!(f, "no file position for line {} column {} of function {}", pos.line, pos.col, item.0)
            }
        }
    }
}

/// The diagnostics of an outcome, in path, position and rule order.
pub fn diagnostics(outcome: &Outcome, universe: &Universe<'_>) -> Result<Vec<Diagnostic, Global>, ReportError> {
    let mut functions = BTreeMap::new_in(Global);
    for function in universe.functions {
        functions.insert(function.item, function);
    }
    let mut names = BTreeMap::new_in(Global);
    for test in universe.tests {
        names.insert(test.key, &test.name);
    }
    let mut out = Vec::new_in(Global);
    for (at, verdict, _) in &outcome.verdicts {
        let shown = match verdict {
            Verdict::Survived { tests } => Some(&tests[..]),
            Verdict::NotReached => None,
            _ => continue,
        };
        let function: &FnFacts = functions.get(&at.item).ok_or(ReportError::UnknownItem { item: at.item })?;
        let site =
            function.sites.get(at.site as usize).ok_or(ReportError::UnknownSite { item: at.item, site: at.site })?;
        let mut tests = Vec::new_in(Global);
        for test in shown.unwrap_or(&[]) {
            let name = names.get(test).ok_or(ReportError::UnknownTest { test: *test })?;
            tests.push(name.to_vec_in(Global));
        }
        let operator = at.mutation.operator(site);
        let rule = match shown {
            Some(_) => operator.rule(),
            None => NOT_REACHED,
        };
        let mut mutated = Vec::new_in(Global);
        mutated_line(site, &at.mutation, &mut mutated);
        let mut headline = Vec::new_in(Global);
        let mut suggestion = Vec::new_in(Global);
        match shown {
            Some(_) => explain(site, &at.mutation, operator, &mut headline, &mut suggestion),
            None => {
                headline.extend_from_slice(b"no test reaches `");
                headline.extend_from_slice(&function.path);
                headline.extend_from_slice(b"`: a change to it would go unnoticed");
                suggestion.extend_from_slice(b"add a test that calls it and asserts on what it returns");
            }
        }
        let mut id = Hasher::new(ID_DOMAIN);
        let mut mutation = Vec::new_in(Global);
        at.mutation.encode(&mut mutation);
        id.part(rule.as_bytes())
            .id(&at.item.0)
            .update(&site.pos.line.to_le_bytes())
            .update(&site.pos.col.to_le_bytes())
            .part(&mutation);
        out.push(Diagnostic {
            rule,
            item: at.item,
            path: function.path.clone(),
            pos: site.pos,
            original: site.text.clone(),
            mutated,
            tests,
            headline,
            suggestion,
            id: id.finish(),
        });
    }
    out.sort_by(|a, b| (&a.path, a.pos, a.rule, a.id).cmp(&(&b.path, b.pos, b.rule, b.id)));
    Ok(out)
}

/// What a survivor of `mutation` says, and the test it asks for.
fn explain(site: &Site, mutation: &Mutation, operator: Operator, headline: &mut Vec<u8, Global>, suggestion: &mut Vec<u8, Global>) {
    let token = token_text(site);
    let mut replaced = Vec::new_in(Global);
    crate::operator::replacement(site, mutation, &mut replaced);
    match operator {
        Operator::ReturnDefault => {
            put(headline, &[b"the function can return `", &replaced, b"` and every test that runs it still passes"]);
            put(suggestion, &[b"its tests run it and check nothing of what it returns: assert on the result"]);
        }
        Operator::ReturnArg => {
            put(headline, &[b"the function can return its argument `", &replaced, b"` unchanged and no test notices"]);
            put(suggestion, &[b"add a case whose expected result differs from that argument"]);
        }
        Operator::ReturnField => {
            put(headline, &[b"no test checks a field of the result: ", &replaced]);
            put(suggestion, &[b"assert on the whole result, or on that field, not on part of it"]);
        }
        Operator::NegateCondition => {
            put(headline, &[b"no test notices the condition `", token, b"` negated"]);
            put(suggestion, &[b"add a case for each side of the branch, asserting what differs"]);
        }
        Operator::CmpBoundary | Operator::CmpEquality => {
            put(headline, &[b"no test distinguishes `", token, b"` from `", &replaced, b"` at this line"]);
            match operator {
                Operator::CmpBoundary => put(suggestion, &[b"add a case where the operands are equal"]),
                _ => put(suggestion, &[b"add a case for equal operands and one for unequal ones"]),
            }
        }
        Operator::ArithNeighbour => {
            put(headline, &[b"no test distinguishes `", token, b"` from `", &replaced, b"` at this line"]);
            put(suggestion, &[b"add a case whose operands give different results under the two, and assert the value"]);
        }
        Operator::ConstAdjacent => {
            put(headline, &[b"no test depends on the constant `", token, b"`: `", &replaced, b"` passes too"]);
            put(suggestion, &[b"add a case at the limit the constant sets, or say why it is tuning"]);
        }
        Operator::DeleteEffect => {
            let what: &[u8] = match site.kind {
                SiteKind::Effect(crate::site::Effectful::Write) => b"write",
                _ => b"call",
            };
            put(headline, &[b"the ", what, b" `", token, b"` can be deleted and no test notices"]);
            put(suggestion, &[b"assert on the state or output it changes"]);
        }
        Operator::ArmBody => {
            put(headline, &[b"no test tells this arm from `", &replaced, b"`"]);
            put(suggestion, &[b"add a case that takes this arm and asserts what only it does"]);
        }
        Operator::SwapArgs => {
            put(headline, &[b"the arguments can be swapped, `", &replaced, b"`, and no test notices"]);
            put(suggestion, &[b"add a case where the swapped arguments differ and the result shows it"]);
        }
        Operator::SkipEarlyReturn => {
            put(headline, &[b"the early return `", token, b"` can be skipped and no test notices"]);
            put(suggestion, &[b"add a case that takes the guard and asserts on what it returns"]);
        }
        Operator::OtherVariant => {
            put(headline, &[b"`", token, b"` can be `", &replaced, b"` and no test notices"]);
            put(suggestion, &[b"assert which variant, not only `is_err()` or `is_some()`"]);
        }
    }
}

fn put(out: &mut Vec<u8, Global>, parts: &[&[u8]]) {
    for part in parts {
        out.extend_from_slice(part);
    }
}

/// Where an item-relative position is in a file.
pub trait LineTable {
    /// The file's path, and the 1-based line and column.
    fn locate(&self, item: ItemId, pos: ItemPos) -> Option<(&[u8], u32, u32)>;
}

/// `d` as text, the way a lint prints.
pub fn render(d: &Diagnostic, lines: &impl LineTable, out: &mut Vec<u8, Global>) -> Result<(), ReportError> {
    let (file, line, col) = lines.locate(d.item, d.pos).ok_or(ReportError::Unlocated { item: d.item, pos: d.pos })?;
    out.extend_from_slice(b"warning[");
    out.extend_from_slice(d.rule.as_bytes());
    out.extend_from_slice(b"]: ");
    out.extend_from_slice(&d.headline);
    out.extend_from_slice(b"\n  --> ");
    out.extend_from_slice(file);
    out.push(b':');
    decimal(i128::from(line), out);
    out.push(b':');
    decimal(i128::from(col), out);
    out.extend_from_slice(b" in ");
    out.extend_from_slice(&d.path);
    out.extend_from_slice(b"\n   - ");
    out.extend_from_slice(&d.original);
    if d.rule != NOT_REACHED {
        out.extend_from_slice(b"\n   + ");
        out.extend_from_slice(&d.mutated);
        out.extend_from_slice(b"\n   = passed: ");
        for (index, test) in d.tests.iter().enumerate() {
            if index > 0 {
                out.extend_from_slice(b", ");
            }
            out.extend_from_slice(test);
        }
    }
    out.extend_from_slice(b"\n   = help: ");
    out.extend_from_slice(&d.suggestion);
    out.push(b'\n');
    Ok(())
}

/// What became of one function's mutants.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Score {
    pub killed: u32,
    pub survived: u32,
    pub not_reached: u32,
    pub equivalent: u32,
    /// Of `equivalent`, those marked by `#[blaze::equivalent_mutant]`: what the ratchet counts.
    pub marked: u32,
    pub not_run: u32,
    /// Sites not mutated, by an arid rule.
    pub arid: u32,
}

impl Score {
    /// Killed per thousand of the mutants that ran or could not be reached; `None` when there are
    /// none.
    pub fn permille(&self) -> Option<u32> {
        let decided = u64::from(self.killed) + u64::from(self.survived) + u64::from(self.not_reached);
        (decided > 0).then(|| (u64::from(self.killed) * 1000 / decided) as u32)
    }

    pub fn add(&mut self, other: &Score) {
        self.killed += other.killed;
        self.survived += other.survived;
        self.not_reached += other.not_reached;
        self.equivalent += other.equivalent;
        self.marked += other.marked;
        self.not_run += other.not_run;
        self.arid += other.arid;
    }
}

/// The score of each function, in item order, and of all of them.
pub fn scores(outcome: &Outcome, arid: &[Skipped]) -> (Vec<(ItemId, Score), Global>, Score) {
    let mut by_item: BTreeMap<ItemId, Score, Global> = BTreeMap::new_in(Global);
    for (at, verdict, _) in &outcome.verdicts {
        let score = by_item.entry(at.item).or_default();
        match verdict {
            Verdict::Killed { .. } => score.killed += 1,
            Verdict::Survived { .. } => score.survived += 1,
            Verdict::NotReached => score.not_reached += 1,
            Verdict::Equivalent(rule) => {
                score.equivalent += 1;
                if *rule == EquivRule::Marked {
                    score.marked += 1;
                }
            }
            Verdict::NotRun(_) => score.not_run += 1,
        }
    }
    for skipped in arid {
        by_item.entry(skipped.item).or_default().arid += 1;
    }
    let mut total = Score::default();
    let mut out = Vec::with_capacity_in(by_item.len(), Global);
    for (item, score) in by_item {
        total.add(&score);
        out.push((item, score));
    }
    (out, total)
}
