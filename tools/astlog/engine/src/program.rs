//! The rule language: S-expressions lowered into a checked [`Program`] over a fact source's atoms.
//!
//! Three top-level forms:
//!
//! ```text
//! (rule (name vars...) body-atom...)
//! (rewrite name body-atom... action...)
//! (lint relation-name <error|warning> "message template")
//! ```
//!
//! A body atom is a builtin (`text`, `parent`, ...), an atom of the fact source ([`Language`]), a
//! rule-defined relation, `(not (relation args...))` or `(count c (relation args...))`. Arguments
//! are variables (bare atoms), integers (bare numbers) or text (strings). Actions are
//! `(replace v "t")`, `(delete v)`, `(insert-before v "t")`, `(insert-after v "t")` and whatever
//! the source adds. Templates splice bound variables in the source's [`TemplateSyntax`].

use std::alloc::Global;
use std::fmt;

use blaze_script::regex;

use blaze_hash::{HashMap, HashSet};

use crate::error::Error;
use crate::sexpr::{self, Sexpr};

/// What a fact source adds to the language: its atoms, its actions and its template syntax.
pub trait Language {
    /// A parsed source atom (a tree-sitter query, a code pattern, a semantic relation).
    type Atom: fmt::Debug;
    /// A parsed source action (a rename through the front end).
    type Action: fmt::Debug;
    /// The source's own failures, carried by [`Error::Source`].
    type Error: std::error::Error + 'static;

    /// How templates splice variables in this source's rules files.
    const TEMPLATE: TemplateSyntax;

    /// Whether `name` is one of this source's atoms or actions, so no rule head may take it.
    fn claims(name: &str) -> bool;

    /// `(head args...)` as one of this source's atoms, or `None` when `head` is not one.
    ///
    /// # Errors
    ///
    /// A malformed atom of this source, named by its line.
    fn parse_atom(
        head: &str,
        args: &[Sexpr],
        line: usize,
    ) -> Result<Option<Parsed<Self::Atom>>, Error<Self::Error>>;

    /// `(head args...)` as one of this source's rewrite actions, or `None`.
    ///
    /// # Errors
    ///
    /// A malformed action of this source, named by its line.
    fn parse_action(
        head: &str,
        args: &[Sexpr],
        line: usize,
    ) -> Result<Option<Parsed<Self::Action>>, Error<Self::Error>>;
}

/// A source atom or action as its [`Language`] parsed it: the value, its argument terms in column
/// order, and the columns that must be bound by atoms to its left.
#[derive(Debug)]
pub struct Parsed<A> {
    pub value: A,
    pub args: Vec<Term>,
    pub needs: Needs,
}

/// Which arguments a source atom is demand-driven on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Needs {
    Nothing,
    /// Every listed column bound.
    All(&'static [usize]),
    /// At least one listed column bound.
    Any(&'static [usize]),
}

/// How a template marks a variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSyntax {
    /// `{var}`; `{{` and `}}` are literal braces (tree-sitter rules).
    Braces,
    /// `$var`, `${var}` or `$$$var`; `$$` is a literal `$` (Rust code patterns, where braces are
    /// code).
    Dollar,
}

/// The builtins every source has: name, arity, and the arguments that must be bound.
pub(crate) const BUILTINS: &[(&str, usize, &[usize])] = &[
    ("ancestor", 2, &[1]),
    ("parent", 2, &[1]),
    ("precedes", 2, &[0, 1]),
    ("text", 2, &[0]),
    ("kind", 2, &[0]),
    ("same-text", 2, &[0, 1]),
    ("same-file", 2, &[0, 1]),
    ("file-path", 2, &[0]),
    ("text-match", 2, &[0, 1]),
    // Two or more: the text, the pattern, then one variable per capture group.
    ("text-capture", 2, &[0, 1]),
    ("text-replace", 4, &[0, 1, 2]),
    ("case", 3, &[0, 1]),
    ("misgrouped-digits", 2, &[0]),
    ("lt", 2, &[0, 1]),
    ("le", 2, &[0, 1]),
];

/// The actions every source has.
const TEXT_ACTIONS: &[&str] = &["replace", "delete", "insert-before", "insert-after"];

/// A list S-expression destructured into its head atom and remaining items.
type ListForm<'a> = (&'a str, &'a [Sexpr]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(String),
    Text(String),
    Int(i64),
}

#[derive(Debug)]
pub struct AppAtom {
    pub name: String,
    pub args: Vec<Term>,
    pub line: usize,
    /// The compiled pattern of a builtin in [`REGEX_BUILTINS`], compiled once at load.
    pub regex: Option<regex::Regex>,
}

/// The builtins whose second argument is a regex, which must be a string literal.
pub(crate) const REGEX_BUILTINS: &[&str] = &["text-match", "text-capture", "text-replace"];

/// The identifier styles `case` writes.
pub(crate) const CASES: &[&str] = &["snake", "camel", "pascal", "upper-snake", "kebab"];

/// The builtins whose arity is a least: `text-capture` binds one variable per capture group.
const VARIADIC: &[&str] = &["text-capture"];

/// The most instructions one compiled regex may have. The matcher (`blaze_script::regex`) runs
/// in time linear in the text and holds a thread per instruction; this bounds the memory a rules
/// file's pattern takes.
pub(crate) const MAX_REGEX_INSTRUCTIONS: usize = 1 << 14;

/// `pattern` compiled within [`MAX_REGEX_INSTRUCTIONS`].
///
/// # Errors
///
/// [`Error::Regex`] naming the line and the builtin, for a pattern that does not compile or is
/// past the size limit.
fn compile_regex<S: std::error::Error + 'static>(
    name: &str,
    pattern: &str,
    line: usize,
) -> Result<regex::Regex, Error<S>> {
    regex::Regex::with_limit(pattern, MAX_REGEX_INSTRUCTIONS)
        .map_err(|source| Error::Regex {
            line,
            name: name.to_owned(),
            pattern: pattern.to_owned(),
            source,
        })
}

#[derive(Debug)]
pub struct SourceAtom<A> {
    pub name: String,
    pub value: A,
    pub args: Vec<Term>,
    pub needs: Needs,
    pub line: usize,
}

/// `(count c (relation args...))`: `c` is the number of rows of a lower-stratum relation that
/// agree with the arguments already bound; the others are local to the count.
#[derive(Debug)]
pub struct CountAtom {
    pub result: Term,
    pub inner: AppAtom,
    pub line: usize,
}

#[derive(Debug)]
pub enum BodyAtom<A> {
    Source(SourceAtom<A>),
    App(AppAtom),
    /// `(not (relation args...))`: succeeds for a binding iff no row of the named relation
    /// unifies with it. Stratified: the negated relation is computed in a lower stratum, so its
    /// rows are final before the negation runs.
    Negation(AppAtom),
    /// `(not (source-atom args...))`: succeeds for a binding iff the source gives no row for
    /// it. Every argument is bound by then, so it is a filter and needs no stratum.
    NegatedSource(SourceAtom<A>),
    Count(CountAtom),
}

#[derive(Debug)]
pub struct Rule<A> {
    pub name: String,
    pub head_vars: Vec<String>,
    pub body: Vec<BodyAtom<A>>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Lit(String),
    Var(String),
}

#[derive(Debug)]
pub struct Template {
    pub segments: Vec<Segment>,
    pub line: usize,
}

/// Which text edit a [`TextAction`] makes at its target node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Replace,
    Delete,
    InsertBefore,
    InsertAfter,
}

#[derive(Debug)]
pub struct TextAction {
    pub kind: TextKind,
    pub target: String,
    /// Empty for [`TextKind::Delete`].
    pub template: Template,
    pub line: usize,
}

#[derive(Debug)]
pub struct SourceAction<X> {
    pub name: String,
    pub value: X,
    pub args: Vec<Term>,
    pub line: usize,
}

#[derive(Debug)]
pub enum Action<X> {
    Text(TextAction),
    Source(SourceAction<X>),
}

#[derive(Debug)]
pub struct Rewrite<A, X> {
    pub name: String,
    pub body: Vec<BodyAtom<A>>,
    pub actions: Vec<Action<X>>,
    pub line: usize,
}

/// Severity of a lint declaration: errors gate a scan's exit code, warnings report without
/// failing (unless promoted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// A `(lint relation severity "message")` declaration: every row the named relation derives
/// becomes a finding rendered through the message template.
#[derive(Debug)]
pub struct Lint {
    pub relation: String,
    pub severity: Severity,
    pub message: Template,
    pub line: usize,
}

pub struct Program<L: Language> {
    pub rules: Vec<Rule<L::Atom>>,
    pub rewrites: Vec<Rewrite<L::Atom, L::Action>>,
    pub lints: Vec<Lint>,
}

impl<L: Language> fmt::Debug for Program<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program")
            .field("rules", &self.rules)
            .field("rewrites", &self.rewrites)
            .field("lints", &self.lints)
            .finish()
    }
}

type LError<L> = Error<<L as Language>::Error>;

impl<L: Language> Program<L> {
    /// Parse and check a rules file.
    ///
    /// # Errors
    ///
    /// [`Error::Dsl`] for malformed forms; [`Error::UnknownRelation`] / [`Error::ArityMismatch`]
    /// for applications that resolve to nothing; [`Error::UnboundBuiltinArg`],
    /// [`Error::UnboundSourceArg`], [`Error::UnsafeNegation`], [`Error::UnboundHeadVar`] and
    /// [`Error::TemplateVar`] when a variable is not bound where it is used;
    /// [`Error::LintSeverity`] / [`Error::LintVar`] for a malformed lint;
    /// [`Error::UnstratifiableProgram`] for negation through recursion; and the source's own
    /// errors for its atoms.
    pub fn parse(src: &str) -> Result<Self, LError<L>> {
        let forms = sexpr::parse(src)?;
        let mut rules = Vec::new();
        let mut rewrites = Vec::new();
        let mut lints = Vec::new();
        for form in forms {
            let (head, items) = expect_list(&form, "top-level form")?;
            match head {
                "rule" => rules.push(parse_rule::<L>(items, form.line())?),
                "rewrite" => rewrites.push(parse_rewrite::<L>(items, form.line())?),
                "lint" => lints.push(parse_lint::<L>(items, form.line())?),
                other => {
                    return Err(Error::Dsl {
                        line: form.line(),
                        message: format!(
                            "expected (rule ...), (rewrite ...), or (lint ...), got `{other}`"
                        ),
                    });
                }
            }
        }
        let program = Self { rules, rewrites, lints };
        program.check()?;
        Ok(program)
    }

    /// Arity of each defined relation, taken from its first rule head.
    #[must_use]
    pub fn arities(&self) -> HashMap<&str, usize, Global> {
        let mut arities = HashMap::new_in(Global);
        for rule in &self.rules {
            arities.entry(rule.name.as_str()).or_insert(rule.head_vars.len());
        }
        arities
    }

    fn check(&self) -> Result<(), LError<L>> {
        let arities = self.arities();
        for rule in &self.rules {
            if builtin(&rule.name).is_some() || L::claims(&rule.name) || is_form(&rule.name) {
                return Err(Error::Dsl {
                    line: rule.line,
                    message: format!("rule head `{}` shadows a builtin", rule.name),
                });
            }
            let expected = arities.get(rule.name.as_str()).copied();
            if expected != Some(rule.head_vars.len()) {
                return Err(Error::ArityMismatch {
                    name: rule.name.clone(),
                    expected: expected.unwrap_or(rule.head_vars.len()),
                    got: rule.head_vars.len(),
                    line: rule.line,
                });
            }
            check_atoms(&rule.body, &arities)?;
            let bound = check_modes(&rule.body)?;
            for var in &rule.head_vars {
                if !bound.contains(var.as_str()) {
                    return Err(Error::UnboundHeadVar { line: rule.line, var: var.clone() });
                }
            }
        }
        for rewrite in &self.rewrites {
            check_atoms(&rewrite.body, &arities)?;
            let bound = check_modes(&rewrite.body)?;
            for action in &rewrite.actions {
                check_action(action, &bound)?;
            }
        }
        for lint in &self.lints {
            self.check_lint(lint)?;
        }
        // Reject negation through recursion: the stratification must exist.
        self.strata()?;
        Ok(())
    }

    /// Rules grouped into strata: every relation a rule negates or counts is computed in an
    /// earlier stratum, so negation and count always read a final relation. Returns rule indices
    /// grouped by ascending stratum.
    ///
    /// # Errors
    ///
    /// [`Error::UnstratifiableProgram`] if a relation depends on itself through a negation or a
    /// count, naming the relations on that cycle.
    pub fn strata(&self) -> Result<Vec<Vec<usize>>, LError<L>> {
        let edges = self.edges();
        // Relax stratum levels: a positive edge a->b needs level(a) >= level(b); a negative edge
        // needs level(a) > level(b). A negative edge inside a cycle forces unbounded growth, so
        // if anything still changes after one pass per relation the program is unstratifiable.
        let mut level: HashMap<&str, usize, Global> =
            self.rules.iter().map(|r| (r.name.as_str(), 0)).collect();
        let passes = level.len() + 1;
        for pass in 0..=passes {
            let mut grew: Option<&str> = None;
            for &(a, b, negative) in &edges {
                let need = level[b] + usize::from(negative);
                if level[a] < need {
                    level.insert(a, need);
                    grew = Some(a);
                }
            }
            match grew {
                None => break,
                Some(name) if pass == passes => {
                    let line = self.rules.iter().find(|r| r.name == name).map_or(0, |r| r.line);
                    let cycle = negative_cycle(&edges).join(", ");
                    return Err(Error::UnstratifiableProgram { cycle, line });
                }
                Some(_) => {}
            }
        }
        let max_level = level.values().copied().max().unwrap_or(0);
        let mut strata = vec![Vec::new(); max_level + 1];
        for (index, rule) in self.rules.iter().enumerate() {
            strata[level[rule.name.as_str()]].push(index);
        }
        Ok(strata)
    }

    /// Relation dependency edges (head relation -> referenced relation), flagged when they pass
    /// through `(not ...)` or `(count ...)`.
    fn edges(&self) -> Vec<(&str, &str, bool)> {
        let mut edges = Vec::new();
        for rule in &self.rules {
            for atom in &rule.body {
                match atom {
                    BodyAtom::App(app) if builtin(&app.name).is_none() => {
                        edges.push((rule.name.as_str(), app.name.as_str(), false));
                    }
                    BodyAtom::Negation(neg) if builtin(&neg.name).is_none() => {
                        edges.push((&rule.name, &neg.name, true));
                    }
                    BodyAtom::Count(count) => edges.push((&rule.name, &count.inner.name, true)),
                    BodyAtom::App(_)
                    | BodyAtom::Negation(_)
                    | BodyAtom::Source(_)
                    | BodyAtom::NegatedSource(_) => {}
                }
            }
        }
        edges
    }

    /// A lint must name a defined relation, and its message may only splice that relation's
    /// head variables.
    fn check_lint(&self, lint: &Lint) -> Result<(), LError<L>> {
        let Some(rule) = self.rules.iter().find(|rule| rule.name == lint.relation) else {
            return Err(Error::UnknownRelation { name: lint.relation.clone(), line: lint.line });
        };
        for segment in &lint.message.segments {
            let Segment::Var(var) = segment else {
                continue;
            };
            if !rule.head_vars.contains(var) {
                return Err(Error::LintVar {
                    relation: lint.relation.clone(),
                    var: var.clone(),
                    line: lint.message.line,
                });
            }
        }
        Ok(())
    }
}

/// The relations on some cycle that passes through a negative edge, sorted by name: the strongly
/// connected component of a negative edge's two ends.
fn negative_cycle<'a>(edges: &[(&'a str, &'a str, bool)]) -> Vec<String> {
    let reaches = |from: &'a str, to: &'a str| {
        let mut seen: HashSet<&str, Global> = HashSet::new_in(Global);
        let mut stack = vec![from];
        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }
            if seen.insert(node) {
                stack.extend(edges.iter().filter(|(a, _, _)| *a == node).map(|(_, b, _)| *b));
            }
        }
        false
    };
    for &(a, b, negative) in edges {
        if negative && reaches(b, a) {
            let mut members: Vec<String> = edges
                .iter()
                .flat_map(|(x, y, _)| [*x, *y])
                .filter(|node| reaches(a, node) && reaches(node, a))
                .map(|node| format!("`{node}`"))
                .collect();
            members.sort();
            members.dedup();
            return members;
        }
    }
    Vec::new()
}

/// A builtin's arity and the arguments it needs bound.
#[must_use]
pub(crate) fn builtin(name: &str) -> Option<(usize, &'static [usize])> {
    BUILTINS.iter().find(|(builtin, ..)| *builtin == name).map(|(_, arity, needs)| (*arity, *needs))
}

/// Names the language itself reads as forms, never as relations.
fn is_form(name: &str) -> bool {
    matches!(name, "not" | "count") || TEXT_ACTIONS.contains(&name)
}

fn check_atoms<A, S: std::error::Error + 'static>(
    atoms: &[BodyAtom<A>],
    arities: &HashMap<&str, usize, Global>,
) -> Result<(), Error<S>> {
    for atom in atoms {
        let (relation, filter) = match atom {
            BodyAtom::Negation(neg) => (neg, "(not ...)"),
            BodyAtom::Count(count) => (&count.inner, "(count ...)"),
            BodyAtom::Source(_) | BodyAtom::NegatedSource(_) => continue,
            BodyAtom::App(app) => {
                if let Some((arity, _)) = builtin(&app.name) {
                    check_builtin(app, arity)?;
                    continue;
                }
                let expected = *arities.get(app.name.as_str()).ok_or_else(|| {
                    Error::UnknownRelation { name: app.name.clone(), line: app.line }
                })?;
                check_arity(app, expected, false)?;
                continue;
            }
        };
        // A negated builtin is a test of bound values (every variable is bound to its left, as
        // `check_modes` requires of any negation), so it holds when the builtin gives no row.
        if let (BodyAtom::Negation(neg), Some((arity, _))) = (atom, builtin(&relation.name)) {
            check_builtin(neg, arity)?;
            continue;
        }
        // A counted atom must name a defined relation (not a builtin: builtins are filters,
        // not finite relations to scan) with matching arity.
        if builtin(&relation.name).is_some() {
            return Err(Error::Dsl {
                line: relation.line,
                message: format!("{filter} cannot read the builtin `{}`", relation.name),
            });
        }
        let expected = *arities.get(relation.name.as_str()).ok_or_else(|| {
            Error::UnknownRelation { name: relation.name.clone(), line: relation.line }
        })?;
        check_arity(relation, expected, false)?;
    }
    Ok(())
}

/// A builtin's arity, and its literal arguments: a regex builtin's pattern is a string literal,
/// compiled at load (a variable pattern would make what matches depend on corpus text, which no
/// rule needs), and `text-capture` binds exactly one variable per capture group.
fn check_builtin<S: std::error::Error + 'static>(app: &AppAtom, arity: usize) -> Result<(), Error<S>> {
    let dsl = |message: String| Err(Error::Dsl { line: app.line, message });
    if VARIADIC.contains(&app.name.as_str()) {
        if app.args.len() < arity {
            return dsl(format!(
                "{} takes the text, a pattern and one variable per capture group",
                app.name
            ));
        }
    } else {
        check_arity(app, arity, true)?;
    }
    if app.name == "case" {
        return match &app.args[1] {
            Term::Text(style) if CASES.contains(&style.as_str()) => Ok(()),
            other => dsl(format!(
                "case style `{}` is none of {}: write one as a string literal",
                term_display(other),
                CASES.iter().map(|it| format!("\"{it}\"")).collect::<Vec<_>>().join(", ")
            )),
        };
    }
    if !REGEX_BUILTINS.contains(&app.name.as_str()) {
        return Ok(());
    }
    let (Some(regex), Some(Term::Text(pattern))) = (&app.regex, app.args.get(1)) else {
        return dsl(format!("{} pattern must be a string literal", app.name));
    };
    if app.name == "text-capture" {
        let groups = regex.captures_len() - 1;
        let vars = app.args.len() - 2;
        if groups != vars {
            return dsl(format!(
                "text-capture pattern `{pattern}` has {groups} capture groups, but {vars} \
                 variables follow it: one per group"
            ));
        }
    }
    if app.name == "text-replace" {
        let Some(Term::Text(replacement)) = app.args.get(2) else {
            return dsl("text-replace replacement must be a string literal".to_owned());
        };
        if let Err(message) = check_replacement(regex, replacement) {
            return dsl(format!("text-replace replacement `{replacement}`: {message}"));
        }
    }
    Ok(())
}

/// Every `$` of a `text-replace` replacement names a group of `regex`, or is `$$`.
///
/// The `regex` crate reads `$name` as the longest run of letters, digits and `_`, and writes
/// empty text for a name no group has: `$1_suffix` is group `1_suffix`, so the replacement
/// would silently drop the group. Here it is refused, naming the group and the `${1}` spelling.
fn check_replacement(regex: &regex::Regex, replacement: &str) -> Result<(), String> {
    let named = |name: &str| match name.parse::<usize>() {
        Ok(index) => index < regex.captures_len(),
        Err(_) => regex.capture_names().flatten().any(|it| it == name),
    };
    let groups = regex.captures_len() - 1;
    let mut rest = replacement;
    while let Some(at) = rest.find('$') {
        rest = &rest[at + 1..];
        if let Some(after) = rest.strip_prefix('$') {
            rest = after;
            continue;
        }
        let (name, after, braced) = match rest.strip_prefix('{') {
            Some(inner) => {
                let Some(end) = inner.find('}') else {
                    return Err("`${` has no closing `}`".to_owned());
                };
                (&inner[..end], &inner[end + 1..], true)
            }
            None => {
                let end = rest
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len());
                (&rest[..end], &rest[end..], false)
            }
        };
        if name.is_empty() {
            return Err("a `$` names no group: write `$$` for a literal `$`".to_owned());
        }
        if !named(name) {
            let hint = match name.find(|c: char| !c.is_ascii_digit()) {
                Some(cut) if !braced && cut > 0 => {
                    format!(": for group {} followed by `{}`, write `${{{}}}`", &name[..cut], &name[cut..], &name[..cut])
                }
                _ => String::new(),
            };
            return Err(format!(
                "`${name}` names no group of the pattern, which has {groups} groups{hint}"
            ));
        }
        rest = after;
    }
    Ok(())
}

fn check_arity<S: std::error::Error + 'static>(
    app: &AppAtom,
    expected: usize,
    builtin: bool,
) -> Result<(), Error<S>> {
    if app.args.len() == expected {
        return Ok(());
    }
    if builtin {
        return Err(Error::BuiltinArity {
            name: app.name.clone(),
            expected,
            got: app.args.len(),
            line: app.line,
        });
    }
    Err(Error::ArityMismatch { name: app.name.clone(), expected, got: app.args.len(), line: app.line })
}

/// The variables a body binds, checking left to right that every argument an atom needs is bound
/// by the atoms before it: a builtin's inputs, a source atom's demanded columns, every variable of
/// a `(not ...)` (range restriction) and the group keys of a `(count ...)`. The evaluator runs
/// atoms left to right, so a variable first bound later would still be unbound when the atom
/// runs.
///
/// # Errors
///
/// The first atom that uses a variable before it is bound.
pub(crate) fn check_modes<A, S: std::error::Error + 'static>(
    body: &[BodyAtom<A>],
) -> Result<HashSet<&str, Global>, Error<S>> {
    let mut bound: HashSet<&str, Global> = HashSet::new_in(Global);
    let is_bound = |bound: &HashSet<&str, Global>, term: &Term| match term {
        Term::Var(var) => bound.contains(var.as_str()),
        Term::Text(_) | Term::Int(_) => true,
    };
    for (at, atom) in body.iter().enumerate() {
        match atom {
            BodyAtom::App(app) => {
                if let Some((_, needs)) = builtin(&app.name) {
                    for &need in needs {
                        if let Term::Var(var) = &app.args[need]
                            && !bound.contains(var.as_str())
                        {
                            return Err(Error::UnboundBuiltinArg {
                                line: app.line,
                                name: app.name.clone(),
                                arg: var.clone(),
                            });
                        }
                    }
                }
                bound.extend(vars(&app.args));
            }
            BodyAtom::Source(source) => {
                let ok = match source.needs {
                    Needs::Nothing => true,
                    Needs::All(cols) => cols.iter().all(|&c| is_bound(&bound, &source.args[c])),
                    Needs::Any(cols) => cols.iter().any(|&c| is_bound(&bound, &source.args[c])),
                };
                if !ok {
                    let cols = match source.needs {
                        Needs::All(cols) | Needs::Any(cols) => cols,
                        Needs::Nothing => &[],
                    };
                    let arg = cols
                        .iter()
                        .map(|&c| term_display(&source.args[c]))
                        .collect::<Vec<_>>()
                        .join(if matches!(source.needs, Needs::Any(_)) {
                            "` or `"
                        } else {
                            "`, `"
                        });
                    return Err(Error::UnboundSourceArg {
                        line: source.line,
                        name: source.name.clone(),
                        arg,
                    });
                }
                bound.extend(vars(&source.args));
            }
            BodyAtom::Negation(neg) => {
                for var in vars(&neg.args) {
                    if !bound.contains(var) {
                        return Err(Error::UnsafeNegation { var: var.to_owned(), line: neg.line });
                    }
                }
            }
            BodyAtom::NegatedSource(source) => {
                for var in vars(&source.args) {
                    if !bound.contains(var) {
                        return Err(Error::UnsafeNegation { var: var.to_owned(), line: source.line });
                    }
                }
            }
            BodyAtom::Count(count) => {
                // Unbound inner variables are local to the count; one mentioned again after it
                // would read as a join with nothing it can join.
                for var in vars(&count.inner.args) {
                    if !bound.contains(var) && mentioned(&body[at + 1..], var) {
                        return Err(Error::Dsl {
                            line: count.line,
                            message: format!(
                                "`{var}` is local to the (count ...) and cannot be used after it"
                            ),
                        });
                    }
                }
                if let Term::Var(var) = &count.result {
                    // Unbound, it would be both the number and a column counted over. The
                    // column is local to the count and unset once the rows are counted, so the
                    // result has nothing to equal and the rule derives nothing.
                    if !bound.contains(var.as_str()) && vars(&count.inner.args).any(|it| it == var) {
                        return Err(Error::Dsl {
                            line: count.line,
                            message: format!(
                                "`{var}` is the (count ...)'s result and an argument it counts over; \
                                 name the counted column apart"
                            ),
                        });
                    }
                    bound.insert(var);
                }
            }
        }
    }
    Ok(bound)
}

fn mentioned<A>(body: &[BodyAtom<A>], var: &str) -> bool {
    body.iter().any(|atom| {
        let args: &[Term] = match atom {
            BodyAtom::Source(source) | BodyAtom::NegatedSource(source) => &source.args,
            BodyAtom::App(app) | BodyAtom::Negation(app) => &app.args,
            BodyAtom::Count(count) => {
                return count.result == Term::Var(var.to_owned())
                    || vars(&count.inner.args).any(|v| v == var);
            }
        };
        vars(args).any(|v| v == var)
    })
}

/// The variable names among `args`.
pub(crate) fn vars(args: &[Term]) -> impl Iterator<Item = &str> {
    args.iter().filter_map(|arg| match arg {
        Term::Var(var) => Some(var.as_str()),
        Term::Text(_) | Term::Int(_) => None,
    })
}

pub(crate) fn term_display(term: &Term) -> String {
    match term {
        Term::Var(var) => var.clone(),
        Term::Text(text) => format!("\"{text}\""),
        Term::Int(int) => int.to_string(),
    }
}

fn check_action<X, S: std::error::Error + 'static>(
    action: &Action<X>,
    bound: &HashSet<&str, Global>,
) -> Result<(), Error<S>> {
    let (line, mut used): (usize, Vec<&str>) = match action {
        Action::Text(text) => {
            let template = text.template.segments.iter().filter_map(|segment| match segment {
                Segment::Var(var) => Some(var.as_str()),
                Segment::Lit(_) => None,
            });
            (text.line, template.collect())
        }
        Action::Source(source) => (source.line, vars(&source.args).collect()),
    };
    if let Action::Text(text) = action {
        used.push(&text.target);
    }
    for var in used {
        if !bound.contains(var) {
            return Err(Error::TemplateVar { var: var.to_owned(), line });
        }
    }
    Ok(())
}

fn expect_list<'a, S: std::error::Error + 'static>(
    form: &'a Sexpr,
    what: &str,
) -> Result<ListForm<'a>, Error<S>> {
    let Sexpr::List { items, line } = form else {
        return Err(Error::Dsl { line: form.line(), message: format!("{what} must be a list") });
    };
    let Some((Sexpr::Atom { text, .. }, rest)) = items.split_first() else {
        return Err(Error::Dsl { line: *line, message: format!("{what} must start with an atom") });
    };
    Ok((text, rest))
}

fn parse_rule<L: Language>(items: &[Sexpr], line: usize) -> Result<Rule<L::Atom>, LError<L>> {
    let Some((head, body)) = items.split_first() else {
        return Err(Error::Dsl { line, message: "(rule ...) needs a head".to_owned() });
    };
    let (name, head_args) = expect_list(head, "rule head")?;
    let head_vars = head_args
        .iter()
        .map(|arg| match arg {
            Sexpr::Atom { text, .. } if parse_int(text).is_none() => Ok(text.clone()),
            other => Err(Error::Dsl {
                line: other.line(),
                message: "rule head arguments must be variables".to_owned(),
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rule { name: name.to_owned(), head_vars, body: parse_body::<L>(body)?, line })
}

fn parse_rewrite<L: Language>(
    items: &[Sexpr],
    line: usize,
) -> Result<Rewrite<L::Atom, L::Action>, LError<L>> {
    let Some((Sexpr::Atom { text: name, .. }, rest)) = items.split_first() else {
        return Err(Error::Dsl { line, message: "(rewrite ...) needs a name".to_owned() });
    };
    // The trailing forms that are actions; everything before them is the body.
    let mut actions = Vec::new();
    let mut body_end = rest.len();
    while let Some(form) = body_end.checked_sub(1).map(|at| &rest[at]) {
        let Some(action) = parse_action::<L>(form)? else {
            break;
        };
        actions.push(action);
        body_end -= 1;
    }
    if actions.is_empty() {
        return Err(Error::Dsl {
            line,
            message: "(rewrite ...) needs a final action: (replace var \"template\"), \
                      (delete var), (insert-before var \"template\") or (insert-after var \
                      \"template\")"
                .to_owned(),
        });
    }
    actions.reverse();
    Ok(Rewrite { name: name.clone(), body: parse_body::<L>(&rest[..body_end])?, actions, line })
}

fn parse_action<L: Language>(form: &Sexpr) -> Result<Option<Action<L::Action>>, LError<L>> {
    let Ok((head, args)) = expect_list::<L::Error>(form, "action") else {
        return Ok(None);
    };
    let kind = match head {
        "replace" => TextKind::Replace,
        "delete" => TextKind::Delete,
        "insert-before" => TextKind::InsertBefore,
        "insert-after" => TextKind::InsertAfter,
        other => {
            return Ok(L::parse_action(other, args, form.line())?.map(|parsed| {
                Action::Source(SourceAction {
                    name: other.to_owned(),
                    value: parsed.value,
                    args: parsed.args,
                    line: form.line(),
                })
            }));
        }
    };
    let (target, template) = match (kind, args) {
        (TextKind::Delete, [Sexpr::Atom { text: target, .. }]) => {
            (target, Template { segments: Vec::new(), line: form.line() })
        }
        (
            TextKind::Replace | TextKind::InsertBefore | TextKind::InsertAfter,
            [Sexpr::Atom { text: target, .. }, Sexpr::Str { text: template, line }],
        ) => (target, parse_template::<L::Error>(L::TEMPLATE, template, *line)?),
        _ => {
            let shape = if kind == TextKind::Delete { "var" } else { "var \"template\"" };
            return Err(Error::Dsl {
                line: form.line(),
                message: format!("{head} form is ({head} {shape})"),
            });
        }
    };
    Ok(Some(Action::Text(TextAction { kind, target: target.clone(), template, line: form.line() })))
}

fn parse_lint<L: Language>(items: &[Sexpr], line: usize) -> Result<Lint, LError<L>> {
    let [
        Sexpr::Atom { text: relation, .. },
        Sexpr::Atom { text: severity, line: severity_line },
        Sexpr::Str { text: message, line: message_line },
    ] = items
    else {
        return Err(Error::Dsl {
            line,
            message: "lint form is (lint <relation> <error|warning> \"message\")".to_owned(),
        });
    };
    let severity = match severity.as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        other => {
            return Err(Error::LintSeverity { got: other.to_owned(), line: *severity_line });
        }
    };
    Ok(Lint {
        relation: relation.clone(),
        severity,
        message: parse_template(L::TEMPLATE, message, *message_line)?,
        line,
    })
}

fn parse_body<L: Language>(atoms: &[Sexpr]) -> Result<Vec<BodyAtom<L::Atom>>, LError<L>> {
    atoms.iter().map(parse_body_atom::<L>).collect()
}

fn parse_body_atom<L: Language>(form: &Sexpr) -> Result<BodyAtom<L::Atom>, LError<L>> {
    let (name, args) = expect_list(form, "body atom")?;
    if name == "not" {
        let [inner] = args else {
            return Err(Error::Dsl {
                line: form.line(),
                message: "not form is (not (<relation> args...))".to_owned(),
            });
        };
        return match parse_body_atom::<L>(inner)? {
            BodyAtom::App(app) => Ok(BodyAtom::Negation(app)),
            BodyAtom::Source(source) => Ok(BodyAtom::NegatedSource(source)),
            _ => Err(Error::Dsl {
                line: form.line(),
                message: "(not ...) may only negate a relation or a source atom".to_owned(),
            }),
        };
    }
    if name == "count" {
        let [result, inner] = args else {
            return Err(Error::Dsl {
                line: form.line(),
                message: "count form is (count <var> (<relation> args...))".to_owned(),
            });
        };
        let BodyAtom::App(app) = parse_body_atom::<L>(inner)? else {
            return Err(Error::Dsl {
                line: form.line(),
                message: "(count ...) may only count a relation atom".to_owned(),
            });
        };
        let [result] = terms::<L::Error>(std::slice::from_ref(result))?.try_into().map_err(
            |_: Vec<Term>| {
                Error::Dsl { line: form.line(), message: "count result is one term".to_owned() }
            },
        )?;
        return Ok(BodyAtom::Count(CountAtom { result, inner: app, line: form.line() }));
    }
    if builtin(name).is_none()
        && let Some(parsed) = L::parse_atom(name, args, form.line())?
    {
        return Ok(BodyAtom::Source(SourceAtom {
            name: name.to_owned(),
            value: parsed.value,
            args: parsed.args,
            needs: parsed.needs,
            line: form.line(),
        }));
    }
    let args = terms(args)?;
    let regex = match args.get(1) {
        Some(Term::Text(pattern)) if REGEX_BUILTINS.contains(&name) => {
            Some(compile_regex(name, pattern, form.line())?)
        }
        _ => None,
    };
    Ok(BodyAtom::App(AppAtom { name: name.to_owned(), args, line: form.line(), regex }))
}

/// Atom arguments as terms: a bare number is an integer, any other bare atom a variable, a
/// string text.
///
/// # Errors
///
/// [`Error::Dsl`] for a nested list.
pub fn terms<S: std::error::Error + 'static>(args: &[Sexpr]) -> Result<Vec<Term>, Error<S>> {
    args.iter()
        .map(|arg| match arg {
            Sexpr::Atom { text, .. } => {
                Ok(parse_int(text).map_or_else(|| Term::Var(text.clone()), Term::Int))
            }
            Sexpr::Str { text, .. } => Ok(Term::Text(text.clone())),
            Sexpr::List { line, .. } => Err(Error::Dsl {
                line: *line,
                message: "atom arguments are variables, numbers or strings".to_owned(),
            }),
        })
        .collect()
}

fn parse_int(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A template in `syntax`.
///
/// # Errors
///
/// [`Error::Dsl`] for an unclosed or stray marker.
pub fn parse_template<S: std::error::Error + 'static>(
    syntax: TemplateSyntax,
    template: &str,
    line: usize,
) -> Result<Template, Error<S>> {
    match syntax {
        TemplateSyntax::Braces => parse_braces(template, line),
        TemplateSyntax::Dollar => parse_dollar(template, line),
    }
}

fn parse_braces<S: std::error::Error + 'static>(
    template: &str,
    line: usize,
) -> Result<Template, Error<S>> {
    let mut segments = Vec::new();
    let mut lit = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                if !lit.is_empty() {
                    segments.push(Segment::Lit(std::mem::take(&mut lit)));
                }
                let mut var = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => var.push(c),
                        None => {
                            return Err(Error::Dsl {
                                line,
                                message: "unclosed `{` in template".to_owned(),
                            });
                        }
                    }
                }
                segments.push(Segment::Var(var));
            }
            '}' => {
                return Err(Error::Dsl {
                    line,
                    message: "stray `}` in template (escape as `}}`)".to_owned(),
                });
            }
            other => lit.push(other),
        }
    }
    if !lit.is_empty() {
        segments.push(Segment::Lit(lit));
    }
    Ok(Template { segments, line })
}

/// `$name`, `${name}` and `$$$name` splice `name`; `$$` is a literal `$`.
fn parse_dollar<S: std::error::Error + 'static>(
    template: &str,
    line: usize,
) -> Result<Template, Error<S>> {
    // A variable name starts with a letter or `_`, so `$5` is no variable.
    let ident = |rest: &str| {
        if !rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return 0;
        }
        rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len())
    };
    let mut segments = Vec::new();
    let mut lit = String::new();
    let mut rest = template;
    while let Some(at) = rest.find('$') {
        lit.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        let (var, after) = if let Some(braced) = rest.strip_prefix('{') {
            let Some(end) = braced.find('}') else {
                return Err(Error::Dsl { line, message: "unclosed `${` in template".to_owned() });
            };
            (&braced[..end], &braced[end + 1..])
        } else if let Some(run) = rest.strip_prefix("$$")
            && ident(run) > 0
        {
            (&run[..ident(run)], &run[ident(run)..])
        } else if let Some(after) = rest.strip_prefix('$') {
            lit.push('$');
            rest = after;
            continue;
        } else if ident(rest) > 0 {
            (&rest[..ident(rest)], &rest[ident(rest)..])
        } else {
            return Err(Error::Dsl {
                line,
                message: "stray `$` in template (a literal `$` is `$$`)".to_owned(),
            });
        };
        if !lit.is_empty() {
            segments.push(Segment::Lit(std::mem::take(&mut lit)));
        }
        segments.push(Segment::Var(var.to_owned()));
        rest = after;
    }
    lit.push_str(rest);
    if !lit.is_empty() {
        segments.push(Segment::Lit(lit));
    }
    Ok(Template { segments, line })
}
