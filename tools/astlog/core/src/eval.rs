//! Bottom-up Datalog evaluation over the corpus.
//!
//! `match` atoms are materialized once into extensional rows (one row per
//! tree-sitter query match, columns named by `@capture`), then rules run to a
//! fixpoint with naive iteration: every value is a syntax node or a piece of
//! text derived from one, so the universe is finite and iteration terminates.
//! Rule bodies evaluate left to right as nested-loop joins; builtins act as
//! filters or generators and need their input arguments bound by the atoms to
//! their left.

use std::collections::BTreeMap;
use std::sync::Arc;

use ast_merge_langs::Lang;
use blaze_hash::{HashMap, HashSet};
use snafu::ResultExt as _;
use tree_sitter::StreamingIterator as _;

use crate::corpus::{Corpus, NodeRef, Value};
use crate::error::{
    BuiltinNotNodeSnafu, CaptureIndexSnafu, Error, InternalSnafu, PredicateUnsupportedSnafu,
    QuerySnafu, RegexSnafu, UnboundBuiltinArgSnafu, UnboundHeadVarSnafu, UnknownRelationSnafu,
};
use crate::program::{AppAtom, BodyAtom, MatchAtom, Program, Term, builtin_arity};

pub type Row = Vec<Value>;
type Env = HashMap<String, Value>;
type Binding = Vec<(String, Value)>;

/// A derived relation: named columns plus deduplicated rows in derivation
/// order.
#[derive(Debug)]
pub struct Relation {
    pub columns: Vec<String>,
    rows: Vec<Row>,
    seen: HashSet<Row>,
}

impl Relation {
    fn new(columns: Vec<String>) -> Self {
        Self { columns, rows: Vec::new(), seen: HashSet::default() }
    }

    #[must_use]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn insert(&mut self, row: Row) -> bool {
        if self.seen.contains(&row) {
            return false;
        }
        self.seen.insert(row.clone());
        self.rows.push(row);
        true
    }
}

/// Every relation derived by the program, keyed by name.
#[derive(Debug)]
pub struct Database {
    pub relations: BTreeMap<String, Relation>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct MatchKey {
    lang: Lang,
    query: String,
}

pub(crate) struct Evaluator<'a> {
    program: &'a Program,
    corpus: &'a Corpus,
    matches: HashMap<MatchKey, Vec<Binding>>,
    /// `text-match` patterns compiled once at setup, keyed by pattern source.
    regexes: HashMap<String, regex::Regex>,
}

impl<'a> Evaluator<'a> {
    /// Compile and run every distinct `match` query in the program, and
    /// compile every distinct `text-match` regex.
    ///
    /// # Errors
    ///
    /// Fails on an invalid tree-sitter query, a query using `#` predicates,
    /// a capture index the compiled query does not know, or an invalid
    /// `text-match` regex.
    pub(crate) fn new(program: &'a Program, corpus: &'a Corpus) -> Result<Self, Error> {
        let mut matches = HashMap::default();
        let mut regexes = HashMap::default();
        let rule_atoms = program.rules.iter().flat_map(|rule| &rule.body);
        let rewrite_atoms = program.rewrites.iter().flat_map(|rewrite| &rewrite.body);
        for atom in rule_atoms.chain(rewrite_atoms) {
            match atom {
                BodyAtom::Match(m) => {
                    let key = MatchKey { lang: m.lang, query: m.query.clone() };
                    if let std::collections::hash_map::Entry::Vacant(entry) = matches.entry(key) {
                        entry.insert(materialize(corpus, m)?);
                    }
                }
                BodyAtom::App(app) if app.name == "text-match" => {
                    // Program checking guarantees the pattern is a literal.
                    let Some(Term::Text(pattern)) = app.args.get(1) else {
                        return InternalSnafu {
                            what: format!(
                                "text-match at line {} survived checking without a literal pattern",
                                app.line
                            ),
                        }
                        .fail();
                    };
                    if !regexes.contains_key(pattern) {
                        let compiled = regex::Regex::new(pattern)
                            .context(RegexSnafu { line: app.line, pattern: pattern.clone() })?;
                        regexes.insert(pattern.clone(), compiled);
                    }
                }
                BodyAtom::App(_) | BodyAtom::Negation(_) => {}
            }
        }
        Ok(Self { program, corpus, matches, regexes })
    }

    /// Run all rules to a fixpoint.
    ///
    /// # Errors
    ///
    /// Fails when a rule head variable is never bound by its body or a
    /// builtin is invoked with required arguments unbound.
    pub(crate) fn fixpoint(&self) -> Result<Database, Error> {
        let mut db = Database { relations: BTreeMap::new() };
        for rule in &self.program.rules {
            db.relations
                .entry(rule.name.clone())
                .or_insert_with(|| Relation::new(rule.head_vars.clone()));
        }
        // Evaluate stratum by stratum: every relation a rule negates lives in an
        // earlier stratum, so by the time a stratum runs its negated relations are
        // final. Within a stratum, iterate the usual naive fixpoint to convergence.
        for stratum in self.program.strata()? {
            loop {
                let mut staged: Vec<(&str, Row)> = Vec::new();
                for &index in &stratum {
                    let rule = &self.program.rules[index];
                    let mut envs = Vec::new();
                    self.solve(&db, &rule.body, Env::default(), &mut envs)?;
                    for env in envs {
                        let row = rule
                            .head_vars
                            .iter()
                            .map(|var| {
                                env.get(var).cloned().ok_or_else(|| {
                                    UnboundHeadVarSnafu { line: rule.line, var: var.clone() }
                                        .build()
                                })
                            })
                            .collect::<Result<Row, _>>()?;
                        staged.push((&rule.name, row));
                    }
                }
                let mut grew = false;
                for (name, row) in staged {
                    let relation = db.relations.get_mut(name).ok_or_else(|| {
                        InternalSnafu { what: format!("relation `{name}` missing from database") }
                            .build()
                    })?;
                    grew |= relation.insert(row);
                }
                if !grew {
                    break;
                }
            }
        }
        Ok(db)
    }

    /// All variable bindings satisfying `atoms` against a finished database.
    ///
    /// # Errors
    ///
    /// Same conditions as rule bodies during [`Self::fixpoint`].
    pub(crate) fn bindings(&self, db: &Database, atoms: &[BodyAtom]) -> Result<Vec<Env>, Error> {
        let mut envs = Vec::new();
        self.solve(db, atoms, Env::default(), &mut envs)?;
        Ok(envs)
    }

    fn solve(
        &self,
        db: &Database,
        atoms: &[BodyAtom],
        env: Env,
        out: &mut Vec<Env>,
    ) -> Result<(), Error> {
        let Some((first, rest)) = atoms.split_first() else {
            out.push(env);
            return Ok(());
        };
        match first {
            BodyAtom::Match(m) => {
                let key = MatchKey { lang: m.lang, query: m.query.clone() };
                let bindings = self.matches.get(&key).ok_or_else(|| {
                    InternalSnafu {
                        what: format!("match query at line {} was not materialized", m.line),
                    }
                    .build()
                })?;
                for binding in bindings {
                    let mut next = Some(env.clone());
                    for (var, value) in binding {
                        next = next.and_then(|e| unify_var(&e, var, value));
                    }
                    if let Some(next) = next {
                        self.solve(db, rest, next, out)?;
                    }
                }
            }
            BodyAtom::App(app) if builtin_arity(&app.name).is_some() => {
                for next in self.builtin(app, &env)? {
                    self.solve(db, rest, next, out)?;
                }
            }
            BodyAtom::App(app) => {
                let relation = db.relations.get(&app.name).ok_or_else(|| {
                    UnknownRelationSnafu { name: app.name.clone(), line: app.line }.build()
                })?;
                for row in relation.rows() {
                    let mut next = Some(env.clone());
                    for (term, value) in app.args.iter().zip(row) {
                        next = next.and_then(|e| unify_term(&e, term, value));
                    }
                    if let Some(next) = next {
                        self.solve(db, rest, next, out)?;
                    }
                }
            }
            BodyAtom::Negation(neg) => {
                // Succeed iff no row of the (lower-stratum, final) relation unifies
                // with the current bindings. Negation filters; it binds nothing.
                let relation = db.relations.get(&neg.name).ok_or_else(|| {
                    UnknownRelationSnafu { name: neg.name.clone(), line: neg.line }.build()
                })?;
                let matched = relation.rows().iter().any(|row| {
                    let mut env = Some(env.clone());
                    for (term, value) in neg.args.iter().zip(row) {
                        env = env.and_then(|e| unify_term(&e, term, value));
                    }
                    env.is_some()
                });
                if !matched {
                    self.solve(db, rest, env, out)?;
                }
            }
        }
        Ok(())
    }

    fn builtin(&self, app: &AppAtom, env: &Env) -> Result<Vec<Env>, Error> {
        match app.name.as_str() {
            "parent" => {
                let child = bound_node(app, env, 1)?;
                let parent = self.corpus.node_info(child).parent;
                Ok(parent
                    .and_then(|node| {
                        let parent_ref = NodeRef { file: child.file, node };
                        unify_term(env, &app.args[0], &Value::Node(parent_ref))
                    })
                    .into_iter()
                    .collect())
            }
            "ancestor" => {
                let descendant = bound_node(app, env, 1)?;
                let mut envs = Vec::new();
                let mut current = self.corpus.node_info(descendant).parent;
                while let Some(node) = current {
                    let ancestor = NodeRef { file: descendant.file, node };
                    if let Some(next) = unify_term(env, &app.args[0], &Value::Node(ancestor)) {
                        envs.push(next);
                    }
                    current = self.corpus.node_info(ancestor).parent;
                }
                Ok(envs)
            }
            "text" => {
                let node = bound_node(app, env, 0)?;
                let text = Value::Text(Arc::from(self.corpus.node_text(node)));
                Ok(unify_term(env, &app.args[1], &text).into_iter().collect())
            }
            "kind" => {
                let node = bound_node(app, env, 0)?;
                let kind = Value::Text(Arc::from(self.corpus.node_info(node).kind));
                Ok(unify_term(env, &app.args[1], &kind).into_iter().collect())
            }
            "same-text" => {
                let first = bound_value(app, env, 0)?;
                let second = bound_value(app, env, 1)?;
                let equal = self.corpus.value_text(&first) == self.corpus.value_text(&second);
                Ok(equal.then(|| env.clone()).into_iter().collect())
            }
            "same-file" => {
                let first = bound_node(app, env, 0)?;
                let second = bound_node(app, env, 1)?;
                let equal = first.file == second.file;
                Ok(equal.then(|| env.clone()).into_iter().collect())
            }
            "file-path" => {
                // The path the file was loaded under, as given to the CLI or found by the directory walk. Lets a rule
                // scope itself to a tree (tests/, handlers/) that the syntax cannot show.
                let node = bound_node(app, env, 0)?;
                let path = self.corpus.files[node.file].path.to_string_lossy();
                let path = Value::Text(Arc::from(path.as_ref()));
                Ok(unify_term(env, &app.args[1], &path).into_iter().collect())
            }
            "text-match" => {
                let value = bound_value(app, env, 0)?;
                let Term::Text(pattern) = &app.args[1] else {
                    return InternalSnafu {
                        what: format!("text-match at line {} has a non-literal pattern", app.line),
                    }
                    .fail();
                };
                let regex = self.regexes.get(pattern).ok_or_else(|| {
                    InternalSnafu { what: format!("regex `{pattern}` was not compiled at setup") }
                        .build()
                })?;
                let matched = regex.is_match(self.corpus.value_text(&value));
                Ok(matched.then(|| env.clone()).into_iter().collect())
            }
            "misgrouped-digits" => self.misgrouped_digits(app, env),
            "no-descendant" => {
                let root = bound_node(app, env, 0)?;
                let kind = bound_value(app, env, 1)?;
                let text = bound_value(app, env, 2)?;
                let absent = !self.has_descendant(
                    root,
                    self.corpus.value_text(&kind),
                    self.corpus.value_text(&text),
                );
                Ok(absent.then(|| env.clone()).into_iter().collect())
            }
            "attached-sibling" => {
                let node = bound_node(app, env, 0)?;
                let kind = bound_value(app, env, 1)?;
                let text = bound_value(app, env, 2)?;
                // Holds when some named sibling *attached above* `node` has a
                // descendant with this kind and exact text. "Attached above" = the
                // preceding named siblings up to (not including) the nearest
                // preceding sibling of the same kind as `node` -- i.e. the
                // annotation block (`@spec`/`@impl`/`@doc`/comments) directly above
                // a definition, stopping at the previous definition/clause of the
                // same kind. Absence is expressed by negating this with `(not ...)`.
                let kt = self.corpus.value_text(&kind);
                let tt = self.corpus.value_text(&text);
                let present = self.attached_has_descendant(node, kt, tt);
                Ok(present.then(|| env.clone()).into_iter().collect())
            }
            other => InternalSnafu { what: format!("builtin `{other}` has no evaluator") }.fail(),
        }
    }

    /// `(misgrouped-digits <node-or-text> <fixed>)`: holds when the value's
    /// text is a decimal numeric literal whose underscore digit grouping is
    /// not canonical (see [`regroup_digits`]), unifying `<fixed>` with the
    /// canonical regrouping.
    fn misgrouped_digits(&self, app: &AppAtom, env: &Env) -> Result<Vec<Env>, Error> {
        let value = bound_value(app, env, 0)?;
        Ok(regroup_digits(self.corpus.value_text(&value))
            .and_then(|fixed| {
                unify_term(env, &app.args[1], &Value::Text(Arc::from(fixed.as_str())))
            })
            .into_iter()
            .collect())
    }

    /// Whether any named sibling attached above `node` has a descendant with
    /// this kind and exact text. Walks preceding siblings nearest-first and stops
    /// at the first sibling of the same kind as `node` (the previous
    /// definition/clause), so only the annotation block directly above `node`
    /// (comments and `@...` attributes) is searched. Bounded to that block, not
    /// the whole file.
    fn attached_has_descendant(&self, node: NodeRef, kind: &str, text: &str) -> bool {
        let file = &self.corpus.files[node.file];
        let Some(parent) = file.nodes[node.node].parent else {
            return false;
        };
        let node_kind = file.nodes[node.node].kind;
        // Preorder node table: a node's preceding siblings have lower indices,
        // and everything between `parent` and `node` is inside `parent`. Walk
        // backward, considering only direct children of `parent`.
        let mut index = node.node;
        while index > parent {
            index -= 1;
            let info = &file.nodes[index];
            if info.parent != Some(parent) {
                continue; // a descendant of an earlier sibling, skip
            }
            if info.kind == node_kind {
                break; // previous same-kind sibling: the annotation block ends here
            }
            if info.named
                && self.has_descendant(NodeRef { file: node.file, node: index }, kind, text)
            {
                return true;
            }
        }
        false
    }

    /// Whether `root` has a strict descendant with this kind and exact source
    /// text. Bounded to `root`'s subtree: in the preorder node table the strict
    /// descendants are the contiguous nodes after `root` whose start is within
    /// `root`'s byte span.
    fn has_descendant(&self, root: NodeRef, kind: &str, text: &str) -> bool {
        let file = &self.corpus.files[root.file];
        let root_end = file.nodes[root.node].end;
        let mut index = root.node + 1;
        while index < file.nodes.len() {
            let info = &file.nodes[index];
            if info.start >= root_end {
                break; // past root's subtree
            }
            if info.kind == kind && &file.text[info.start..info.end] == text {
                return true;
            }
            index += 1;
        }
        false
    }
}

/// Canonical underscore digit grouping for a numeric-literal text, following
/// the Rust convention: a digit run of five or more digits is grouped in
/// threes with `_` (integer part and exponent from the right, fractional part
/// from the left of the decimal point), and a run that already carries
/// separators must use exactly that grouping. Returns `Some(canonical)` iff
/// `text` is a decimal numeric literal (underscores between digits allowed,
/// like `10000`, `1_0000`, `1_000.000_1`, `2.5e1_0`) whose canonical grouping
/// DIFFERS from `text` -- i.e. the literal is ungrouped or misgrouped -- and
/// `None` for well-grouped literals and non-numeric text alike. Backs the
/// `misgrouped-digits` builtin.
fn regroup_digits(text: &str) -> Option<String> {
    /// Longest digit run that may stay bare; five or more digits must group.
    const MAX_UNGROUPED: usize = 4;
    // Split off an exponent (`e`/`E`, optional sign), then the fraction.
    let (mantissa, exponent) =
        text.find(['e', 'E']).map_or((text, None), |at| (&text[..at], Some(&text[at..])));
    let (int_part, frac_part) = mantissa
        .find('.')
        .map_or((mantissa, None), |at| (&mantissa[..at], Some(&mantissa[at + 1..])));
    // A digit run is digits with underscores strictly between digits. The
    // integer part may be absent (`.5`) and the fraction empty (`1.`), but
    // the mantissa needs at least one digit overall.
    let run_ok = |run: &str| {
        !run.is_empty()
            && run.starts_with(|c: char| c.is_ascii_digit())
            && run.ends_with(|c: char| c.is_ascii_digit())
            && run.chars().all(|c| c.is_ascii_digit() || c == '_')
    };
    if int_part.is_empty() && frac_part.is_none_or(str::is_empty) {
        return None;
    }
    if !(int_part.is_empty() || run_ok(int_part)) {
        return None;
    }
    if !frac_part.is_none_or(|frac| frac.is_empty() || run_ok(frac)) {
        return None;
    }
    let exp_run = match exponent {
        None => None,
        Some(exp) => {
            let body = &exp[1..];
            let run = body.strip_prefix(['+', '-']).unwrap_or(body);
            if !run_ok(run) {
                return None;
            }
            Some(run)
        }
    };
    // Regroup a run iff it already carries separators or is long enough to
    // need them; four digits or fewer stay bare (`1000` is fine as-is).
    let canonical = |run: &str, from_left: bool| {
        let digits: String = run.chars().filter(|c| *c != '_').collect();
        if run.contains('_') || digits.len() > MAX_UNGROUPED {
            group_digits(&digits, from_left)
        } else {
            digits
        }
    };
    let mut fixed = canonical(int_part, false);
    if let Some(frac) = frac_part {
        fixed.push('.');
        fixed.push_str(&canonical(frac, true));
    }
    if let (Some(exp), Some(run)) = (exponent, exp_run) {
        let marker_and_sign = &exp[..exp.len() - run.len()];
        fixed.push_str(marker_and_sign);
        fixed.push_str(&canonical(run, false));
    }
    (fixed != text).then_some(fixed)
}

/// Insert `_` group separators into a bare digit string, every three digits
/// counted from the right (integer part, exponent) or from the left of the
/// decimal point (fractional part).
fn group_digits(digits: &str, from_left: bool) -> String {
    const GROUP: usize = 3;
    let mut grouped = String::with_capacity(digits.len() + digits.len() / GROUP);
    for (index, c) in digits.chars().enumerate() {
        let at_boundary = if from_left {
            index.is_multiple_of(GROUP)
        } else {
            (digits.len() - index).is_multiple_of(GROUP)
        };
        if index > 0 && at_boundary {
            grouped.push('_');
        }
        grouped.push(c);
    }
    grouped
}

fn bound_value(app: &AppAtom, env: &Env, arg: usize) -> Result<Value, Error> {
    match &app.args[arg] {
        Term::Text(text) => Ok(Value::Text(Arc::from(text.as_str()))),
        Term::Var(var) => env.get(var).cloned().ok_or_else(|| {
            UnboundBuiltinArgSnafu { line: app.line, name: app.name.clone(), arg: var.clone() }
                .build()
        }),
    }
}

fn bound_node(app: &AppAtom, env: &Env, arg: usize) -> Result<NodeRef, Error> {
    match bound_value(app, env, arg)? {
        Value::Node(node) => Ok(node),
        Value::Text(_) => BuiltinNotNodeSnafu {
            line: app.line,
            name: app.name.clone(),
            arg: term_display(&app.args[arg]),
        }
        .fail(),
    }
}

fn term_display(term: &Term) -> String {
    match term {
        Term::Var(var) => var.clone(),
        Term::Text(text) => format!("\"{text}\""),
    }
}

fn unify_var(env: &Env, var: &str, value: &Value) -> Option<Env> {
    match env.get(var) {
        Some(bound) if bound == value => Some(env.clone()),
        Some(_) => None,
        None => {
            let mut next = env.clone();
            next.insert(var.to_owned(), value.clone());
            Some(next)
        }
    }
}

fn unify_term(env: &Env, term: &Term, value: &Value) -> Option<Env> {
    match term {
        Term::Var(var) => unify_var(env, var, value),
        Term::Text(lit) => match value {
            Value::Text(text) if text.as_ref() == lit.as_str() => Some(env.clone()),
            Value::Text(_) | Value::Node(_) => None,
        },
    }
}

/// Run one `match` query over every file of its language.
fn materialize(corpus: &Corpus, m: &MatchAtom) -> Result<Vec<Binding>, Error> {
    if has_predicate(&m.query) {
        return PredicateUnsupportedSnafu { line: m.line }.fail();
    }
    let language = m.lang.to_tree_sitter();
    let query =
        tree_sitter::Query::new(&language, &m.query).context(QuerySnafu { line: m.line })?;
    let names = query.capture_names();
    let mut bindings = Vec::new();
    for (file_index, file) in corpus.files.iter().enumerate() {
        if file.lang != m.lang {
            continue;
        }
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, file.root(), file.text.as_bytes());
        while let Some(found) = matches.next() {
            bindings.push(binding_of(file_index, names, found, m.line, |id| file.node_index(id))?);
        }
    }
    Ok(bindings)
}

/// One relation row from one query match. When a quantified capture matched
/// several nodes, the first occurrence wins (documented v0 limitation).
fn binding_of(
    file: usize,
    names: &[&str],
    found: &tree_sitter::QueryMatch<'_, '_>,
    line: usize,
    node_index: impl Fn(usize) -> Option<usize>,
) -> Result<Binding, Error> {
    let mut binding: Binding = Vec::new();
    for capture in found.captures {
        let Ok(index) = usize::try_from(capture.index) else {
            return CaptureIndexSnafu { line, index: capture.index }.fail();
        };
        let Some(name) = names.get(index) else {
            return CaptureIndexSnafu { line, index: capture.index }.fail();
        };
        if binding.iter().any(|(bound, _)| bound == name) {
            continue;
        }
        let node = node_index(capture.node.id()).ok_or_else(|| {
            InternalSnafu { what: format!("query capture `{name}` missing from the node table") }
                .build()
        })?;
        binding.push(((*name).to_owned(), Value::Node(NodeRef { file, node })));
    }
    Ok(binding)
}

/// Detect `#` predicate syntax outside string literals and `;` comments
/// (tree-sitter query comments run to end of line).
fn has_predicate(query: &str) -> bool {
    let mut in_string = false;
    let mut chars = query.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '\\' if in_string => {
                chars.next();
            }
            ';' if !in_string => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '#' if !in_string => return true,
            _ => {}
        }
    }
    false
}
