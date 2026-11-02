//! The rule DSL: S-expressions lowered into a checked [`Program`].
//!
//! Three top-level forms:
//!
//! ```text
//! (rule (name vars...) body-atom...)
//! (rewrite name body-atom... (replace var "template"))
//! (lint relation-name <error|warning> "message template")
//! ```
//!
//! Body atoms are either `(match <lang> "<tree-sitter query>")`, which binds
//! every `@capture` in the query as a variable, or an application
//! `(name args...)` of a builtin or a rule-defined relation. Atom arguments
//! are variables (bare atoms) or text literals (strings). Templates splice
//! bound variables with `{var}`; `{{` and `}}` escape literal braces.

use ast_merge_langs::Lang;
use blaze_hash::{HashMap, HashSet};

use crate::error::{
    ArityMismatchSnafu, BuiltinAritySnafu, DslSnafu, Error, LintSeveritySnafu, LintVarSnafu,
    UnknownLangNameSnafu, UnknownRelationSnafu, UnsafeNegationSnafu, UnstratifiableProgramSnafu,
};
use crate::sexpr::{self, Sexpr};

/// Builtin atom names with their arity. Resolved before relation lookup, so a
/// rule head may not shadow them (rejected at load).
pub(crate) const BUILTINS: &[(&str, usize)] = &[
    ("ancestor", 2),
    ("parent", 2),
    ("text", 2),
    ("kind", 2),
    ("same-text", 2),
    ("same-file", 2),
    ("file-path", 2),
    ("text-match", 2),
    ("misgrouped-digits", 2),
    ("no-descendant", 3),
    ("attached-sibling", 3),
];

/// A list S-expression destructured into its head atom and remaining items.
type ListForm<'a> = (&'a str, &'a [Sexpr]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(String),
    Text(String),
}

#[derive(Debug)]
pub struct AppAtom {
    pub name: String,
    pub args: Vec<Term>,
    pub line: usize,
}

#[derive(Debug)]
pub struct MatchAtom {
    pub lang: Lang,
    pub query: String,
    pub line: usize,
}

#[derive(Debug)]
pub enum BodyAtom {
    Match(MatchAtom),
    App(AppAtom),
    /// `(not (relation args...))`: succeeds for a binding iff no row of the named
    /// relation unifies with it. Stratified — the negated relation is always
    /// computed in a lower stratum, so its rows are final before the negation runs.
    Negation(AppAtom),
}

#[derive(Debug)]
pub struct Rule {
    pub name: String,
    pub head_vars: Vec<String>,
    pub body: Vec<BodyAtom>,
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

#[derive(Debug)]
pub struct Rewrite {
    pub name: String,
    pub body: Vec<BodyAtom>,
    pub target: String,
    pub template: Template,
    pub line: usize,
}

/// Severity of a lint declaration: errors gate `astlog scan`'s exit code,
/// warnings report without failing (unless promoted with `--error`).
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

/// A `(lint relation severity "message")` declaration: every row the named
/// relation derives becomes a scan finding rendered through the message
/// template.
#[derive(Debug)]
pub struct Lint {
    pub relation: String,
    pub severity: Severity,
    pub message: Template,
    pub line: usize,
}

#[derive(Debug)]
pub struct Program {
    pub rules: Vec<Rule>,
    pub rewrites: Vec<Rewrite>,
    pub lints: Vec<Lint>,
}

impl Program {
    /// Parse and check a rules file.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Dsl`] for malformed forms, [`Error::UnknownLangName`]
    /// for an unrecognized `match` language, [`Error::UnknownRelation`] /
    /// [`Error::ArityMismatch`] for applications that resolve to nothing,
    /// [`Error::TemplateVar`] when a rewrite template references a variable
    /// its body does not mention, [`Error::LintSeverity`] for a lint severity
    /// other than `error` / `warning`, and [`Error::LintVar`] when a lint
    /// message references a variable outside its relation's head.
    pub fn parse(src: &str) -> Result<Self, Error> {
        let forms = sexpr::parse(src)?;
        let mut rules = Vec::new();
        let mut rewrites = Vec::new();
        let mut lints = Vec::new();
        for form in forms {
            let (head, items) = expect_list(&form, "top-level form")?;
            match head {
                "rule" => rules.push(parse_rule(items, form.line())?),
                "rewrite" => rewrites.push(parse_rewrite(items, form.line())?),
                "lint" => lints.push(parse_lint(items, form.line())?),
                other => {
                    return DslSnafu {
                        line: form.line(),
                        message: format!(
                            "expected (rule ...), (rewrite ...), or (lint ...), got `{other}`"
                        ),
                    }
                    .fail();
                }
            }
        }
        let program = Self { rules, rewrites, lints };
        program.check()?;
        Ok(program)
    }

    /// Arity of each defined relation, taken from its first rule head.
    #[must_use]
    pub fn arities(&self) -> HashMap<&str, usize> {
        let mut arities = HashMap::default();
        for rule in &self.rules {
            arities.entry(rule.name.as_str()).or_insert(rule.head_vars.len());
        }
        arities
    }

    fn check(&self) -> Result<(), Error> {
        let arities = self.arities();
        for rule in &self.rules {
            if builtin_arity(&rule.name).is_some() {
                return DslSnafu {
                    line: rule.line,
                    message: format!("rule head `{}` shadows a builtin", rule.name),
                }
                .fail();
            }
            let expected = arities.get(rule.name.as_str()).copied();
            if expected != Some(rule.head_vars.len()) {
                return ArityMismatchSnafu {
                    name: rule.name.clone(),
                    expected: expected.unwrap_or(rule.head_vars.len()),
                    got: rule.head_vars.len(),
                    line: rule.line,
                }
                .fail();
            }
            check_atoms(&rule.body, &arities)?;
            check_negation_safety(&rule.body)?;
        }
        for rewrite in &self.rewrites {
            check_atoms(&rewrite.body, &arities)?;
            check_negation_safety(&rewrite.body)?;
            check_template(rewrite)?;
        }
        for lint in &self.lints {
            self.check_lint(lint)?;
        }
        // Reject negation through recursion: the stratification must exist.
        self.strata()?;
        Ok(())
    }

    /// Rules grouped into strata: every relation a rule negates is computed in an
    /// earlier stratum, so negation always reads a final relation. Returns rule
    /// indices grouped by ascending stratum.
    ///
    /// # Errors
    ///
    /// [`Error::UnstratifiableProgram`] if a relation depends on itself through a
    /// negation (no stratification exists).
    pub fn strata(&self) -> Result<Vec<Vec<usize>>, Error> {
        // Relation dependency edges (head relation -> referenced relation), with a
        // flag for edges that pass through `(not ...)`.
        let mut edges: Vec<(&str, &str, bool)> = Vec::new();
        for rule in &self.rules {
            for atom in &rule.body {
                match atom {
                    BodyAtom::App(app) if builtin_arity(&app.name).is_none() => {
                        edges.push((&rule.name, &app.name, false));
                    }
                    BodyAtom::Negation(neg) => edges.push((&rule.name, &neg.name, true)),
                    _ => {}
                }
            }
        }
        // Relax stratum levels: a positive edge a->b needs level(a) >= level(b);
        // a negative edge needs level(a) > level(b). A negative edge inside a cycle
        // forces unbounded growth, so if anything still changes after one pass per
        // relation the program is unstratifiable.
        let mut level: HashMap<&str, usize> =
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
                // A relation whose level still grows after a full pass per
                // relation sits on a negation cycle: report it, not rule #1.
                Some(name) if pass == passes => {
                    let line = self.rules.iter().find(|r| r.name == name).map_or(0, |r| r.line);
                    return UnstratifiableProgramSnafu { rule: name.to_owned(), line }.fail();
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

    /// A lint must name a defined relation, and its message may only splice
    /// that relation's head variables.
    fn check_lint(&self, lint: &Lint) -> Result<(), Error> {
        let Some(rule) = self.rules.iter().find(|rule| rule.name == lint.relation) else {
            return UnknownRelationSnafu { name: lint.relation.clone(), line: lint.line }.fail();
        };
        for segment in &lint.message.segments {
            let Segment::Var(var) = segment else {
                continue;
            };
            if !rule.head_vars.contains(var) {
                return LintVarSnafu {
                    relation: lint.relation.clone(),
                    var: var.clone(),
                    line: lint.message.line,
                }
                .fail();
            }
        }
        Ok(())
    }
}

#[must_use]
pub(crate) fn builtin_arity(name: &str) -> Option<usize> {
    BUILTINS.iter().find(|(builtin, _)| *builtin == name).map(|(_, arity)| *arity)
}

fn check_atoms(atoms: &[BodyAtom], arities: &HashMap<&str, usize>) -> Result<(), Error> {
    for atom in atoms {
        if let BodyAtom::Negation(neg) = atom {
            // A negated atom must name a defined relation (not a builtin: builtins
            // are filters, not finite relations to scan) with matching arity.
            if builtin_arity(&neg.name).is_some() {
                return DslSnafu {
                    line: neg.line,
                    message: format!("(not ...) cannot negate the builtin `{}`", neg.name),
                }
                .fail();
            }
            let expected = *arities.get(neg.name.as_str()).ok_or_else(|| {
                UnknownRelationSnafu { name: neg.name.clone(), line: neg.line }.build()
            })?;
            check_relation_arity(neg, expected)?;
            continue;
        }
        let BodyAtom::App(app) = atom else {
            continue;
        };
        if let Some(expected) = builtin_arity(&app.name) {
            check_arity(app, expected, true)?;
            // The pattern is compiled once at evaluator setup, which requires
            // it to be a literal; a variable pattern would also make match
            // results depend on corpus text, which no rule legitimately needs.
            if app.name == "text-match" && !matches!(app.args.get(1), Some(Term::Text(_))) {
                return DslSnafu {
                    line: app.line,
                    message: "text-match pattern must be a string literal".to_owned(),
                }
                .fail();
            }
            continue;
        }
        let expected = *arities.get(app.name.as_str()).ok_or_else(|| {
            UnknownRelationSnafu { name: app.name.clone(), line: app.line }.build()
        })?;
        check_relation_arity(app, expected)?;
    }
    Ok(())
}

fn check_relation_arity(app: &AppAtom, expected: usize) -> Result<(), Error> {
    check_arity(app, expected, false)
}

fn check_arity(app: &AppAtom, expected: usize, builtin: bool) -> Result<(), Error> {
    if app.args.len() != expected {
        if builtin {
            return BuiltinAritySnafu {
                name: app.name.clone(),
                expected,
                got: app.args.len(),
                line: app.line,
            }
            .fail();
        }
        return ArityMismatchSnafu {
            name: app.name.clone(),
            expected,
            got: app.args.len(),
            line: app.line,
        }
        .fail();
    }
    Ok(())
}

/// Every variable a negated atom uses must be bound by a positive atom that
/// appears BEFORE it in the body (range restriction). The evaluator runs atoms
/// left-to-right, so a variable first bound after the negation is still unbound
/// when the negation runs and would be existentially bound per row (a wildcard)
/// rather than filtering -- this check rejects exactly that. Applied to rule and
/// rewrite bodies alike.
fn check_negation_safety(body: &[BodyAtom]) -> Result<(), Error> {
    let mut bound: HashSet<&str> = HashSet::default();
    for atom in body {
        match atom {
            BodyAtom::Match(m) => bound.extend(capture_names(&m.query)),
            BodyAtom::App(app) => {
                for arg in &app.args {
                    if let Term::Var(var) = arg {
                        bound.insert(var);
                    }
                }
            }
            BodyAtom::Negation(neg) => {
                for arg in &neg.args {
                    if let Term::Var(var) = arg
                        && !bound.contains(var.as_str())
                    {
                        return UnsafeNegationSnafu { var: var.clone(), line: neg.line }.fail();
                    }
                }
            }
        }
    }
    Ok(())
}

fn check_template(rewrite: &Rewrite) -> Result<(), Error> {
    let mut mentioned: HashSet<&str> = HashSet::default();
    for atom in &rewrite.body {
        match atom {
            BodyAtom::Match(m) => mentioned.extend(capture_names(&m.query)),
            BodyAtom::App(app) => {
                for arg in &app.args {
                    if let Term::Var(var) = arg {
                        mentioned.insert(var);
                    }
                }
            }
            // A negated atom only filters; it binds nothing, so it contributes no
            // mentioned variables (its vars are bound by positive atoms via the
            // safety check).
            BodyAtom::Negation(_) => {}
        }
    }
    let template_vars = rewrite.template.segments.iter().filter_map(|segment| match segment {
        Segment::Var(var) => Some(var.as_str()),
        Segment::Lit(_) => None,
    });
    for var in template_vars.chain([rewrite.target.as_str()]) {
        if !mentioned.contains(var) {
            return Err(crate::error::TemplateVarSnafu {
                var: var.to_owned(),
                line: rewrite.template.line,
            }
            .build());
        }
    }
    Ok(())
}

/// Capture names mentioned in a tree-sitter query source (`@name`).
///
/// Lexical scan, skipping string literals and `;` comments (which run to end
/// of line); the query is validated for real by `tree_sitter::Query::new` at
/// evaluation setup.
fn capture_names(query: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = query;
    let mut in_string = false;
    let mut chars = rest.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '\\' if in_string => {
                chars.next();
            }
            ';' if !in_string => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '@' if !in_string => {
                rest = &query[at + 1..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
                    .unwrap_or(rest.len());
                if end > 0 {
                    names.push(&rest[..end]);
                }
            }
            _ => {}
        }
    }
    names
}

fn expect_list<'a>(form: &'a Sexpr, what: &str) -> Result<ListForm<'a>, Error> {
    let Sexpr::List { items, line } = form else {
        return DslSnafu { line: form.line(), message: format!("{what} must be a list") }.fail();
    };
    let Some((Sexpr::Atom { text, .. }, rest)) = items.split_first() else {
        return DslSnafu { line: *line, message: format!("{what} must start with an atom") }.fail();
    };
    Ok((text, rest))
}

fn parse_rule(items: &[Sexpr], line: usize) -> Result<Rule, Error> {
    let Some((head, body)) = items.split_first() else {
        return DslSnafu { line, message: "(rule ...) needs a head".to_owned() }.fail();
    };
    let (name, head_args) = expect_list(head, "rule head")?;
    let head_vars = head_args
        .iter()
        .map(|arg| match arg {
            Sexpr::Atom { text, .. } => Ok(text.clone()),
            other => DslSnafu {
                line: other.line(),
                message: "rule head arguments must be variables".to_owned(),
            }
            .fail(),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rule { name: name.to_owned(), head_vars, body: parse_body(body)?, line })
}

fn parse_rewrite(items: &[Sexpr], line: usize) -> Result<Rewrite, Error> {
    let Some((Sexpr::Atom { text: name, .. }, rest)) = items.split_first() else {
        return DslSnafu { line, message: "(rewrite ...) needs a name".to_owned() }.fail();
    };
    let Some((replace, body)) = rest.split_last() else {
        return DslSnafu {
            line,
            message: "(rewrite ...) needs a final (replace var \"template\")".to_owned(),
        }
        .fail();
    };
    let (replace_head, replace_args) = expect_list(replace, "replace form")?;
    let [Sexpr::Atom { text: target, .. }, Sexpr::Str { text: template, line: template_line }] =
        replace_args
    else {
        return DslSnafu {
            line: replace.line(),
            message: "replace form is (replace var \"template\")".to_owned(),
        }
        .fail();
    };
    if replace_head != "replace" {
        return DslSnafu {
            line: replace.line(),
            message: format!("rewrite must end with (replace ...), got `{replace_head}`"),
        }
        .fail();
    }
    Ok(Rewrite {
        name: name.clone(),
        body: parse_body(body)?,
        target: target.clone(),
        template: parse_template(template, *template_line)?,
        line,
    })
}

fn parse_lint(items: &[Sexpr], line: usize) -> Result<Lint, Error> {
    let [
        Sexpr::Atom { text: relation, .. },
        Sexpr::Atom { text: severity, line: severity_line },
        Sexpr::Str { text: message, line: message_line },
    ] = items
    else {
        return DslSnafu {
            line,
            message: "lint form is (lint <relation> <error|warning> \"message\")".to_owned(),
        }
        .fail();
    };
    let severity = match severity.as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        other => {
            return LintSeveritySnafu { got: other.to_owned(), line: *severity_line }.fail();
        }
    };
    Ok(Lint {
        relation: relation.clone(),
        severity,
        message: parse_template(message, *message_line)?,
        line,
    })
}

fn parse_body(atoms: &[Sexpr]) -> Result<Vec<BodyAtom>, Error> {
    atoms.iter().map(parse_body_atom).collect()
}

fn parse_body_atom(form: &Sexpr) -> Result<BodyAtom, Error> {
    let (name, args) = expect_list(form, "body atom")?;
    if name == "match" {
        let [Sexpr::Atom { text: lang_name, line: lang_line }, Sexpr::Str { text: query, line }] =
            args
        else {
            return DslSnafu {
                line: form.line(),
                message: "match form is (match <lang> \"<query>\")".to_owned(),
            }
            .fail();
        };
        return Ok(BodyAtom::Match(MatchAtom {
            lang: resolve_lang(lang_name, *lang_line)?,
            query: query.clone(),
            line: *line,
        }));
    }
    if name == "not" {
        let [inner] = args else {
            return DslSnafu {
                line: form.line(),
                message: "not form is (not (<relation> args...))".to_owned(),
            }
            .fail();
        };
        let BodyAtom::App(app) = parse_body_atom(inner)? else {
            return DslSnafu {
                line: form.line(),
                message: "(not ...) may only negate a relation atom".to_owned(),
            }
            .fail();
        };
        return Ok(BodyAtom::Negation(app));
    }
    let args = args
        .iter()
        .map(|arg| match arg {
            Sexpr::Atom { text, .. } => Ok(Term::Var(text.clone())),
            Sexpr::Str { text, .. } => Ok(Term::Text(text.clone())),
            Sexpr::List { line, .. } => DslSnafu {
                line: *line,
                message: "atom arguments are variables or strings".to_owned(),
            }
            .fail(),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BodyAtom::App(AppAtom { name: name.to_owned(), args, line: form.line() }))
}

/// Resolve a language name as written in `(match <lang> ...)`.
///
/// Accepts the profile name case-insensitively (`rust`, `toml`, `python`) or
/// any registered file extension (`rs`, `py`).
fn resolve_lang(name: &str, line: usize) -> Result<Lang, Error> {
    let lowered = name.to_lowercase();
    for lang in Lang::all() {
        if lang.profile().name.to_lowercase() == lowered {
            return Ok(*lang);
        }
    }
    ast_merge_langs::detect_from_extension(&lowered)
        .ok_or_else(|| UnknownLangNameSnafu { name: name.to_owned(), line }.build())
}

fn parse_template(template: &str, line: usize) -> Result<Template, Error> {
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
                            return DslSnafu {
                                line,
                                message: "unclosed `{` in template".to_owned(),
                            }
                            .fail();
                        }
                    }
                }
                segments.push(Segment::Var(var));
            }
            '}' => {
                return DslSnafu {
                    line,
                    message: "stray `}` in template (escape as `}}`)".to_owned(),
                }
                .fail();
            }
            other => lit.push(other),
        }
    }
    if !lit.is_empty() {
        segments.push(Segment::Lit(lit));
    }
    Ok(Template { segments, line })
}
