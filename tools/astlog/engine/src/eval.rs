//! Bottom-up semi-naive Datalog evaluation over a fact source.
//!
//! Rules run stratum by stratum. Within a stratum, round 0 evaluates every rule over everything
//! known; each later round evaluates each recursive rule once per body position that names a
//! relation of the stratum, that position reading only the rows new in the last round and every
//! other position reading all rows. A relation's rows are append-only, so "new in the last round"
//! is a range of row numbers. Relation atoms join through hash indexes on the columns bound at
//! that position, registered before evaluation and kept as rows arrive. Variables are slots in a
//! per-rule environment, bound left to right as [`crate::program::check_modes`] proved they can
//! be.

use std::alloc::Global;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt;
use std::hash::Hash;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use blaze_hash::{HashMap, HashSet};
use snafu::ResultExt as _;

use crate::error::{
    BuiltinNotNodeSnafu, BuiltinNotNumberSnafu, Error, InternalSnafu, RegexSnafu,
    TooManyBindingsSnafu, TooManyRowsSnafu, TooManySourceRowsSnafu,
};
use crate::program::{AppAtom, BodyAtom, Language, Program, Term};

/// One value in a relation row.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Value<N> {
    /// A node of the fact source: a syntax node, or whatever else the source names (a def, a
    /// type).
    Node(N),
    Text(Arc<str>),
    Int(i64),
}

impl<N> Value<N> {
    /// What kind of value this is, for errors.
    #[must_use]
    pub const fn what(&self) -> &'static str {
        match self {
            Self::Node(_) => "a node",
            Self::Text(_) => "text",
            Self::Int(_) => "a number",
        }
    }
}

pub type Row<N> = Vec<Value<N>>;

/// One binding of a body's variables, by name.
pub type Binding<N> = HashMap<String, Value<N>, Global>;

type FError<F> = Error<<<F as Facts>::Lang as Language>::Error>;

/// A fact source at evaluation: its atoms' rows and the tree under its nodes.
pub trait Facts {
    type Lang: Language;
    type Node: Copy + Eq + Hash + Ord + fmt::Debug;

    /// Rows of `atom` that agree with `inputs` (one entry per argument, `Some` where it is bound
    /// or literal). Every row has one value per argument.
    ///
    /// # Errors
    ///
    /// The source's own failure, named by the atom.
    fn solve(
        &self,
        atom: &<Self::Lang as Language>::Atom,
        line: usize,
        inputs: &[Option<Value<Self::Node>>],
        out: &mut Vec<Row<Self::Node>>,
    ) -> Result<(), FError<Self>>;

    /// The syntactic parent; `None` at a root and for nodes that are not syntax.
    fn parent(&self, node: Self::Node) -> Option<Self::Node>;
    /// The node's kind (a grammar kind, or a def or type kind).
    fn kind(&self, node: Self::Node) -> Cow<'_, str>;
    /// The node's text: its source slice, or a def's name or a type's display.
    fn text(&self, node: Self::Node) -> Cow<'_, str>;
    /// The file a syntax node is in and its byte range there; `None` for nodes that are not
    /// syntax.
    fn span(&self, node: Self::Node) -> Option<(usize, Range<usize>)>;
    /// A file's path as the source loaded it.
    fn path(&self, file: usize) -> Cow<'_, str>;

    /// Text behind any value.
    fn value_text<'a>(&'a self, value: &'a Value<Self::Node>) -> Cow<'a, str> {
        match value {
            Value::Node(node) => self.text(*node),
            Value::Text(text) => Cow::Borrowed(text),
            Value::Int(int) => Cow::Owned(int.to_string()),
        }
    }
}

/// A derived relation: named columns plus deduplicated rows in derivation order.
pub struct Relation<N> {
    pub columns: Vec<String>,
    rows: Vec<Row<N>>,
    seen: HashSet<Row<N>, Global>,
    indexes: Vec<Index<N>>,
}

/// Row numbers of a relation by the values of some of its columns.
struct Index<N> {
    columns: Vec<usize>,
    rows: HashMap<Vec<Value<N>>, Vec<u32>, Global>,
}

impl<N: fmt::Debug> fmt::Debug for Relation<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relation")
            .field("columns", &self.columns)
            .field("rows", &self.rows)
            .finish()
    }
}

impl<N: Clone + Eq + Hash> Relation<N> {
    fn new(columns: Vec<String>) -> Self {
        Self { columns, rows: Vec::new(), seen: HashSet::new_in(Global), indexes: Vec::new() }
    }

    #[must_use]
    pub fn rows(&self) -> &[Row<N>] {
        &self.rows
    }

    fn insert(&mut self, row: Row<N>) -> bool {
        if self.seen.contains(&row) {
            return false;
        }
        let at = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
        for index in &mut self.indexes {
            let key = index.columns.iter().map(|&c| row[c].clone()).collect();
            index.rows.entry(key).or_default().push(at);
        }
        self.seen.insert(row.clone());
        self.rows.push(row);
        true
    }

    /// The index on `columns`, made (over the rows so far) when it is first asked for.
    fn index(&mut self, columns: &[usize]) -> usize {
        if let Some(at) = self.indexes.iter().position(|index| index.columns == columns) {
            return at;
        }
        let mut index = Index { columns: columns.to_vec(), rows: HashMap::new_in(Global) };
        for (at, row) in self.rows.iter().enumerate() {
            let key = columns.iter().map(|&c| row[c].clone()).collect();
            index.rows.entry(key).or_default().push(u32::try_from(at).unwrap_or(u32::MAX));
        }
        self.indexes.push(index);
        self.indexes.len() - 1
    }
}

/// Every relation derived by the program, keyed by name.
#[derive(Debug)]
pub struct Database<N> {
    pub relations: BTreeMap<String, Relation<N>>,
}

/// How an argument meets the environment at its atom.
#[derive(Debug, Clone)]
enum Arg<N> {
    /// A literal, or a variable bound by an atom to the left: an input.
    Const(Value<N>),
    In(usize),
    /// A variable this atom binds (its first occurrence here).
    Out(usize),
    /// A later occurrence, in the same atom, of a variable this atom binds.
    Check(usize),
}

impl<N> Arg<N> {
    const fn is_input(&self) -> bool {
        matches!(self, Self::Const(_) | Self::In(_))
    }
}

#[derive(Debug, Clone, Copy)]
enum Builtin {
    Parent,
    Ancestor,
    Precedes,
    Text,
    Kind,
    SameText,
    SameFile,
    FilePath,
    TextMatch,
    MisgroupedDigits,
    Lt,
    Le,
}

impl Builtin {
    fn of(name: &str) -> Option<Self> {
        Some(match name {
            "parent" => Self::Parent,
            "ancestor" => Self::Ancestor,
            "precedes" => Self::Precedes,
            "text" => Self::Text,
            "kind" => Self::Kind,
            "same-text" => Self::SameText,
            "same-file" => Self::SameFile,
            "file-path" => Self::FilePath,
            "text-match" => Self::TextMatch,
            "misgrouped-digits" => Self::MisgroupedDigits,
            "lt" => Self::Lt,
            "le" => Self::Le,
            _ => return None,
        })
    }
}

enum Step<'p, A, N> {
    Scan { relation: usize, args: Vec<Arg<N>>, index: Option<usize>, recursive: bool },
    Not { relation: usize, args: Vec<Arg<N>>, index: Option<usize> },
    Count { relation: usize, args: Vec<Arg<N>>, index: Option<usize>, result: Arg<N> },
    Builtin { builtin: Builtin, app: &'p AppAtom, args: Vec<Arg<N>>, regex: Option<regex::Regex> },
    /// `occurrence` keys the atom's answers in [`Evaluator::asked`]: one per atom in the program
    /// text, so two occurrences never share answers they might not agree on.
    Source { atom: &'p A, line: usize, args: Vec<Arg<N>>, occurrence: usize },
    NotSource { atom: &'p A, line: usize, args: Vec<Arg<N>>, occurrence: usize },
}

/// A source atom occurrence and the inputs it was asked with.
type Asked<N> = (usize, Vec<Option<Value<N>>>);

/// A body compiled to steps over variable slots.
struct Body<'p, A, N> {
    steps: Vec<Step<'p, A, N>>,
    names: Vec<String>,
}

impl<A, N> Body<'_, A, N> {
    fn slot(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }
}

struct CompiledRule<'p, A, N> {
    relation: usize,
    head: Vec<usize>,
    body: Body<'p, A, N>,
    line: usize,
}

/// The most rows one run holds at once by default ([`Limits::DEFAULT`]). A rule's join can
/// grow as the product of what it joins, and a request names the rules, so the run stops
/// with [`Error::TooManyRows`] before it takes the daemon's memory.
///
/// [`Error::TooManyRows`]: crate::Error::TooManyRows
pub const MAX_ROWS: usize = 10_000_000;

/// How much one run may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Rows held at once: every derived row, the source rows kept for the run
    /// ([`Evaluator`] asks a source atom each input once), and the rows a rule or a rewrite's
    /// body has made and not yet stored.
    pub rows: usize,
}

impl Limits {
    pub const DEFAULT: Self = Self { rows: MAX_ROWS };
}

/// The program compiled against one fact source, ready to run.
pub struct Evaluator<'p, F: Facts> {
    program: &'p Program<F::Lang>,
    facts: &'p F,
    limits: Limits,
    /// Rows stored: derived, and kept in `asked`.
    held: Cell<usize>,
    names: Vec<String>,
    rules: Vec<CompiledRule<'p, <F::Lang as Language>::Atom, F::Node>>,
    relations: Vec<Relation<F::Node>>,
    /// Each source atom's rows by the inputs it was asked with, for the run: a recursive rule
    /// asks again every round, and a join asks once per row to its left.
    asked: RefCell<HashMap<Asked<F::Node>, Rc<Vec<Row<F::Node>>>, Global>>,
    /// The next source atom occurrence [`Self::compile`] numbers.
    occurrences: usize,
}

type Env<N> = Vec<Option<Value<N>>>;

/// Which rows of a relation a scan reads.
struct Window {
    rows: Range<usize>,
}

impl<'p, F: Facts> Evaluator<'p, F> {
    /// Compile `program` against `facts`, to run within `limits`.
    ///
    /// # Errors
    ///
    /// An invalid `text-match` regex.
    pub fn new(
        program: &'p Program<F::Lang>,
        facts: &'p F,
        limits: Limits,
    ) -> Result<Self, FError<F>> {
        let mut names: Vec<String> = Vec::new();
        let mut relations = Vec::new();
        for rule in &program.rules {
            if !names.contains(&rule.name) {
                names.push(rule.name.clone());
                relations.push(Relation::new(rule.head_vars.clone()));
            }
        }
        let mut this = Self {
            program,
            facts,
            limits,
            held: Cell::new(0),
            names,
            rules: Vec::new(),
            relations,
            asked: RefCell::new(HashMap::new_in(Global)),
            occurrences: 0,
        };
        for rule in &program.rules {
            let relation = this.relation(&rule.name)?;
            let body = this.compile(&rule.body)?;
            let head = rule
                .head_vars
                .iter()
                .map(|var| {
                    body.slot(var).ok_or_else(|| {
                        InternalSnafu { what: format!("head variable `{var}` has no slot") }.build()
                    })
                })
                .collect::<Result<_, _>>()?;
            this.rules.push(CompiledRule { relation, head, body, line: rule.line });
        }
        Ok(this)
    }

    fn relation(&self, name: &str) -> Result<usize, FError<F>> {
        self.names.iter().position(|n| n == name).ok_or_else(|| {
            InternalSnafu { what: format!("relation `{name}` missing from the program") }.build()
        })
    }

    fn compile(
        &mut self,
        atoms: &'p [BodyAtom<<F::Lang as Language>::Atom>],
    ) -> Result<Body<'p, <F::Lang as Language>::Atom, F::Node>, FError<F>> {
        let mut names: Vec<String> = Vec::new();
        let mut steps = Vec::new();
        for atom in atoms {
            let step = match atom {
                BodyAtom::Source(source) => Step::Source {
                    atom: &source.value,
                    line: source.line,
                    args: args(&mut names, &source.args, true),
                    occurrence: self.occurrence(),
                },
                BodyAtom::NegatedSource(source) => Step::NotSource {
                    atom: &source.value,
                    line: source.line,
                    args: args(&mut names, &source.args, false),
                    occurrence: self.occurrence(),
                },
                BodyAtom::App(app) => match Builtin::of(&app.name) {
                    Some(builtin) => {
                        let regex = match (builtin, app.args.get(1)) {
                            (Builtin::TextMatch, Some(Term::Text(pattern))) => {
                                Some(regex::Regex::new(pattern).context(RegexSnafu {
                                    line: app.line,
                                    pattern: pattern.clone(),
                                })?)
                            }
                            _ => None,
                        };
                        Step::Builtin {
                            builtin,
                            app,
                            args: args(&mut names, &app.args, true),
                            regex,
                        }
                    }
                    None => {
                        let relation = self.relation(&app.name)?;
                        let args = args(&mut names, &app.args, true);
                        let index = self.index_of(relation, &args);
                        let recursive = false;
                        Step::Scan { relation, args, index, recursive }
                    }
                },
                BodyAtom::Negation(neg) => {
                    let relation = self.relation(&neg.name)?;
                    let args = args(&mut names, &neg.args, false);
                    let index = self.index_of(relation, &args);
                    Step::Not { relation, args, index }
                }
                BodyAtom::Count(count) => {
                    let relation = self.relation(&count.inner.name)?;
                    // Local variables get slots that nothing after the count reads.
                    let args = args(&mut names, &count.inner.args, true);
                    let index = self.index_of(relation, &args);
                    let mut result =
                        crate::eval::args(&mut names, std::slice::from_ref(&count.result), true);
                    let result = result.pop().ok_or_else(|| {
                        InternalSnafu { what: "count without a result".to_owned() }.build()
                    })?;
                    Step::Count { relation, args, index, result }
                }
            };
            steps.push(step);
        }
        Ok(Body { steps, names })
    }

    fn occurrence(&mut self) -> usize {
        self.occurrences += 1;
        self.occurrences - 1
    }

    /// The rows of a source atom occurrence for `inputs`: asked of the source the first time,
    /// then the same rows for the rest of the run.
    fn source_rows(
        &self,
        occurrence: usize,
        atom: &<F::Lang as Language>::Atom,
        line: usize,
        inputs: Vec<Option<Value<F::Node>>>,
    ) -> Result<Rc<Vec<Row<F::Node>>>, FError<F>> {
        let key = (occurrence, inputs);
        if let Some(rows) = self.asked.borrow().get(&key) {
            return Ok(Rc::clone(rows));
        }
        let mut rows = Vec::new();
        self.facts.solve(atom, line, &key.1, &mut rows)?;
        let held = self.held.get() + rows.len();
        if held > self.limits.rows {
            return TooManySourceRowsSnafu { line, cap: self.limits.rows }.fail();
        }
        self.held.set(held);
        let rows = Rc::new(rows);
        self.asked.borrow_mut().insert(key, Rc::clone(&rows));
        Ok(rows)
    }

    fn index_of(&mut self, relation: usize, args: &[Arg<F::Node>]) -> Option<usize> {
        let columns: Vec<usize> =
            args.iter().enumerate().filter(|(_, arg)| arg.is_input()).map(|(at, _)| at).collect();
        (!columns.is_empty()).then(|| self.relations[relation].index(&columns))
    }

    /// Run all rules to a fixpoint.
    ///
    /// # Errors
    ///
    /// A source atom's failure, or a builtin given a value of the wrong kind.
    pub fn fixpoint(&mut self) -> Result<(), FError<F>> {
        for stratum in self.program.strata()? {
            self.stratum(&stratum)?;
        }
        Ok(())
    }

    fn stratum(&mut self, stratum: &[usize]) -> Result<(), FError<F>> {
        let members: Vec<usize> = stratum.iter().map(|&rule| self.rules[rule].relation).collect();
        for &rule in stratum {
            for step in &mut self.rules[rule].body.steps {
                if let Step::Scan { relation, recursive, .. } = step {
                    *recursive = members.contains(relation);
                }
            }
        }
        // Round 0: everything known.
        let mut before: Vec<usize> = self.relations.iter().map(|r| r.rows.len()).collect();
        let mut staged = Vec::new();
        for &rule in stratum {
            self.run_rule(rule, None, &before, &before, &mut staged)?;
        }
        loop {
            let delta_from = before.clone();
            let mut grew = false;
            for (relation, row) in staged.drain(..) {
                if self.relations[relation].insert(row) {
                    grew = true;
                    self.held.set(self.held.get() + 1);
                }
            }
            if !grew {
                return Ok(());
            }
            before = self.relations.iter().map(|r| r.rows.len()).collect();
            for &rule in stratum {
                let positions: Vec<usize> = self.rules[rule]
                    .body
                    .steps
                    .iter()
                    .enumerate()
                    .filter(|(_, step)| matches!(step, Step::Scan { recursive: true, .. }))
                    .map(|(at, _)| at)
                    .collect();
                for position in positions {
                    self.run_rule(rule, Some(position), &delta_from, &before, &mut staged)?;
                }
            }
        }
    }

    /// Evaluate one rule: `delta` is the step reading only rows `from[r]..to[r]`, every other
    /// scan reads `0..to[r]`.
    fn run_rule(
        &self,
        rule: usize,
        delta: Option<usize>,
        from: &[usize],
        to: &[usize],
        staged: &mut Vec<(usize, Row<F::Node>)>,
    ) -> Result<(), FError<F>> {
        let compiled = &self.rules[rule];
        let mut env: Env<F::Node> = vec![None; compiled.body.names.len()];
        let windows = Windows { delta, from, to };
        self.solve(&compiled.body, 0, &windows, &mut env, &mut |env| {
            let row = compiled.head.iter().map(|&slot| env[slot].clone()).collect::<Option<_>>();
            match row {
                // A row the relation has already costs nothing to make again.
                Some(row) if self.relations[compiled.relation].seen.contains(&row) => Ok(()),
                Some(row) => {
                    if self.held.get() + staged.len() >= self.limits.rows {
                        return TooManyRowsSnafu {
                            relation: self.names[compiled.relation].clone(),
                            line: compiled.line,
                            cap: self.limits.rows,
                        }
                        .fail();
                    }
                    staged.push((compiled.relation, row));
                    Ok(())
                }
                None => InternalSnafu {
                    what: format!(
                        "rule for `{}` left a head slot unbound",
                        self.names[compiled.relation]
                    ),
                }
                .fail(),
            }
        })
    }

    fn solve(
        &self,
        body: &Body<'p, <F::Lang as Language>::Atom, F::Node>,
        at: usize,
        windows: &Windows<'_>,
        env: &mut Env<F::Node>,
        emit: &mut dyn FnMut(&Env<F::Node>) -> Result<(), FError<F>>,
    ) -> Result<(), FError<F>> {
        let Some(step) = body.steps.get(at) else {
            return emit(env);
        };
        match step {
            Step::Scan { relation, args, index, .. } => {
                let window = windows.of(at, *relation);
                let rel = &self.relations[*relation];
                let candidates = candidates(rel, *index, args, env, window);
                for row in candidates {
                    if let Some(set) = unify(args, row, env) {
                        self.solve(body, at + 1, windows, env, emit)?;
                        clear(env, &set);
                    }
                }
            }
            Step::Not { relation, args, index } => {
                let rel = &self.relations[*relation];
                let window = Window { rows: 0..rel.rows.len() };
                let mut found = false;
                for row in candidates(rel, *index, args, env, window) {
                    if let Some(set) = unify(args, row, env) {
                        clear(env, &set);
                        found = true;
                        break;
                    }
                }
                if !found {
                    self.solve(body, at + 1, windows, env, emit)?;
                }
            }
            Step::Count { relation, args, index, result } => {
                let rel = &self.relations[*relation];
                let window = Window { rows: 0..rel.rows.len() };
                let mut count = 0i64;
                for row in candidates(rel, *index, args, env, window) {
                    if let Some(set) = unify(args, row, env) {
                        clear(env, &set);
                        count += 1;
                    }
                }
                let value = [Value::Int(count)];
                if let Some(set) = unify(std::slice::from_ref(result), &value, env) {
                    self.solve(body, at + 1, windows, env, emit)?;
                    clear(env, &set);
                }
            }
            Step::Source { atom, line, args, occurrence } => {
                let inputs = args.iter().map(|arg| input(arg, env)).collect();
                let rows = self.source_rows(*occurrence, atom, *line, inputs)?;
                for row in rows.iter() {
                    if let Some(set) = unify(args, row, env) {
                        self.solve(body, at + 1, windows, env, emit)?;
                        clear(env, &set);
                    }
                }
            }
            Step::NotSource { atom, line, args, occurrence } => {
                let inputs: Vec<Option<Value<F::Node>>> =
                    args.iter().map(|arg| input(arg, env)).collect();
                let rows = self.source_rows(*occurrence, atom, *line, inputs.clone())?; // clone: the key keeps one
                let any = rows.iter().any(|row| {
                    row.iter().zip(&inputs).all(|(value, input)| input.as_ref().is_none_or(|it| it == value))
                });
                if !any {
                    self.solve(body, at + 1, windows, env, emit)?;
                }
            }
            Step::Builtin { builtin, app, args, regex } => {
                for row in self.builtin(*builtin, app, args, regex.as_ref(), env)? {
                    if let Some(set) = unify(args, &row, env) {
                        self.solve(body, at + 1, windows, env, emit)?;
                        clear(env, &set);
                    }
                }
            }
        }
        Ok(())
    }

    /// The rows a builtin holds for, given its inputs.
    fn builtin(
        &self,
        builtin: Builtin,
        app: &AppAtom,
        args: &[Arg<F::Node>],
        regex: Option<&regex::Regex>,
        env: &Env<F::Node>,
    ) -> Result<Vec<Row<F::Node>>, FError<F>> {
        let facts = self.facts;
        let value = |at: usize| {
            input(&args[at], env).ok_or_else(|| {
                InternalSnafu {
                    what: format!(
                        "builtin `{}` at rules:{} ran with an input unbound",
                        app.name, app.line
                    ),
                }
                .build()
            })
        };
        let node = |at: usize| -> Result<F::Node, FError<F>> {
            match value(at)? {
                Value::Node(node) => Ok(node),
                other => BuiltinNotNodeSnafu {
                    line: app.line,
                    name: app.name.clone(),
                    arg: crate::program::term_display(&app.args[at]),
                    what: other.what(),
                }
                .fail(),
            }
        };
        let span = |at: usize| -> Result<(usize, Range<usize>), FError<F>> {
            let n = node(at)?;
            facts.span(n).ok_or_else(|| {
                BuiltinNotNodeSnafu {
                    line: app.line,
                    name: app.name.clone(),
                    arg: crate::program::term_display(&app.args[at]),
                    what: "not syntax",
                }
                .build()
            })
        };
        let int = |at: usize| -> Result<i64, FError<F>> {
            match value(at)? {
                Value::Int(int) => Ok(int),
                other => BuiltinNotNumberSnafu {
                    line: app.line,
                    name: app.name.clone(),
                    arg: crate::program::term_display(&app.args[at]),
                    what: other.what(),
                }
                .fail(),
            }
        };
        let holds = |yes: bool, row: Row<F::Node>| if yes { vec![row] } else { Vec::new() };
        Ok(match builtin {
            Builtin::Parent => {
                let child = node(1)?;
                facts
                    .parent(child)
                    .map(|p| vec![Value::Node(p), Value::Node(child)])
                    .into_iter()
                    .collect()
            }
            Builtin::Ancestor => {
                let descendant = node(1)?;
                let mut rows = Vec::new();
                let mut current = facts.parent(descendant);
                while let Some(up) = current {
                    rows.push(vec![Value::Node(up), Value::Node(descendant)]);
                    current = facts.parent(up);
                }
                rows
            }
            Builtin::Precedes => {
                let (first_file, first) = span(0)?;
                let (second_file, second) = span(1)?;
                holds(
                    first_file == second_file && first.end <= second.start,
                    vec![value(0)?, value(1)?],
                )
            }
            Builtin::Text => {
                let n = node(0)?;
                vec![vec![Value::Node(n), Value::Text(Arc::from(facts.text(n).as_ref()))]]
            }
            Builtin::Kind => {
                let n = node(0)?;
                vec![vec![Value::Node(n), Value::Text(Arc::from(facts.kind(n).as_ref()))]]
            }
            Builtin::SameText => {
                let (first, second) = (value(0)?, value(1)?);
                let equal = facts.value_text(&first) == facts.value_text(&second);
                holds(equal, vec![first, second])
            }
            Builtin::SameFile => {
                let (first, _) = span(0)?;
                let (second, _) = span(1)?;
                holds(first == second, vec![value(0)?, value(1)?])
            }
            Builtin::FilePath => {
                let (file, _) = span(0)?;
                vec![vec![value(0)?, Value::Text(Arc::from(facts.path(file).as_ref()))]]
            }
            Builtin::TextMatch => {
                let regex = regex.ok_or_else(|| {
                    InternalSnafu { what: format!("text-match at rules:{} has no regex", app.line) }
                        .build()
                })?;
                let subject = value(0)?;
                let matched = regex.is_match(&facts.value_text(&subject));
                holds(matched, vec![subject, value(1)?])
            }
            Builtin::MisgroupedDigits => {
                let subject = value(0)?;
                let fixed = regroup_digits(&facts.value_text(&subject));
                fixed
                    .map(|fixed| vec![subject, Value::Text(Arc::from(fixed.as_str()))])
                    .into_iter()
                    .collect()
            }
            Builtin::Lt => holds(int(0)? < int(1)?, vec![value(0)?, value(1)?]),
            Builtin::Le => holds(int(0)? <= int(1)?, vec![value(0)?, value(1)?]),
        })
    }

    /// All variable bindings satisfying a rewrite body against a finished database.
    ///
    /// # Errors
    ///
    /// Same conditions as rule bodies during [`Self::fixpoint`], and
    /// [`Error::TooManyBindings`] naming `line`, the rewrite's.
    pub fn bindings(
        &mut self,
        atoms: &'p [BodyAtom<<F::Lang as Language>::Atom>],
        line: usize,
    ) -> Result<Vec<Binding<F::Node>>, FError<F>> {
        let body = self.compile(atoms)?;
        let mut out = Vec::new();
        let ends: Vec<usize> = self.relations.iter().map(|r| r.rows.len()).collect();
        let windows = Windows { delta: None, from: &ends, to: &ends };
        let mut env: Env<F::Node> = vec![None; body.names.len()];
        let (held, cap) = (self.held.get(), self.limits.rows);
        self.solve(&body, 0, &windows, &mut env, &mut |env| {
            if held + out.len() >= cap {
                return TooManyBindingsSnafu { line, cap }.fail();
            }
            let binding = body
                .names
                .iter()
                .zip(env)
                .filter_map(|(name, value)| value.clone().map(|value| (name.clone(), value)))
                .collect();
            out.push(binding);
            Ok(())
        })?;
        Ok(out)
    }

    /// The derived relations, by name.
    #[must_use]
    pub fn into_database(self) -> Database<F::Node> {
        let relations = self.names.into_iter().zip(self.relations).collect();
        Database { relations }
    }
}

/// Which rows each scan of the rule being run reads.
struct Windows<'a> {
    delta: Option<usize>,
    from: &'a [usize],
    to: &'a [usize],
}

impl Windows<'_> {
    fn of(&self, step: usize, relation: usize) -> Window {
        let end = self.to[relation];
        if self.delta == Some(step) {
            Window { rows: self.from[relation]..end }
        } else {
            Window { rows: 0..end }
        }
    }
}

/// Lower one atom's terms to arguments, giving each new variable a slot. With `binds` false (a
/// negation), every variable is already bound.
fn args<N>(names: &mut Vec<String>, terms: &[Term], binds: bool) -> Vec<Arg<N>> {
    let before = names.len();
    terms
        .iter()
        .map(|term| match term {
            Term::Text(text) => Arg::Const(Value::Text(Arc::from(text.as_str()))),
            Term::Int(int) => Arg::Const(Value::Int(*int)),
            Term::Var(var) => match names.iter().position(|n| n == var) {
                Some(slot) if slot < before => Arg::In(slot),
                Some(slot) => Arg::Check(slot),
                None => {
                    names.push(var.clone());
                    let slot = names.len() - 1;
                    if binds { Arg::Out(slot) } else { Arg::Check(slot) }
                }
            },
        })
        .collect()
}

fn input<N: Clone>(arg: &Arg<N>, env: &Env<N>) -> Option<Value<N>> {
    match arg {
        Arg::Const(value) => Some(value.clone()),
        Arg::In(slot) => env[*slot].clone(),
        Arg::Out(_) | Arg::Check(_) => None,
    }
}

/// The rows of `window` that may agree with the bound arguments: through the index when there is
/// one, else every row.
fn candidates<'r, N: Clone + Eq + Hash>(
    relation: &'r Relation<N>,
    index: Option<usize>,
    args: &[Arg<N>],
    env: &Env<N>,
    window: Window,
) -> Box<dyn Iterator<Item = &'r Row<N>> + 'r> {
    let Some(index) = index else {
        return Box::new(relation.rows[window.rows].iter());
    };
    let index = &relation.indexes[index];
    let key: Option<Vec<Value<N>>> = index.columns.iter().map(|&c| input(&args[c], env)).collect();
    let Some(ids) = key.and_then(|key| index.rows.get(&key)) else {
        return Box::new(std::iter::empty());
    };
    let start = ids.partition_point(|&id| (id as usize) < window.rows.start);
    let rows = &relation.rows;
    let end = window.rows.end;
    Box::new(
        ids[start..]
            .iter()
            .take_while(move |&&id| (id as usize) < end)
            .map(move |&id| &rows[id as usize]),
    )
}

/// Unify a row with the arguments, binding `Out` slots. Returns the slots it set, or `None` when
/// the row disagrees (leaving the environment as it was).
fn unify<N: Clone + PartialEq>(
    args: &[Arg<N>],
    row: &[Value<N>],
    env: &mut Env<N>,
) -> Option<Vec<usize>> {
    let mut set = Vec::new();
    for (arg, value) in args.iter().zip(row) {
        let ok = match arg {
            Arg::Const(expected) => expected == value,
            Arg::In(slot) | Arg::Check(slot) => env[*slot].as_ref() == Some(value),
            Arg::Out(slot) => {
                env[*slot] = Some(value.clone());
                set.push(*slot);
                true
            }
        };
        if !ok {
            clear(env, &set);
            return None;
        }
    }
    Some(set)
}

fn clear<N>(env: &mut Env<N>, set: &[usize]) {
    for &slot in set {
        env[slot] = None;
    }
}

/// Canonical underscore digit grouping for a numeric-literal text, following the Rust convention:
/// a digit run of five or more digits is grouped in threes with `_` (integer part and exponent
/// from the right, fractional part from the left of the decimal point), and a run that already
/// carries separators must use exactly that grouping. Returns `Some(canonical)` iff `text` is a
/// decimal numeric literal (underscores between digits allowed, like `10000`, `1_0000`,
/// `1_000.000_1`, `2.5e1_0`) whose canonical grouping DIFFERS from `text` -- i.e. the literal is
/// ungrouped or misgrouped -- and `None` for well-grouped literals and non-numeric text alike.
/// Backs the `misgrouped-digits` builtin.
fn regroup_digits(text: &str) -> Option<String> {
    /// Longest digit run that may stay bare; five or more digits must group.
    const MAX_UNGROUPED: usize = 4;
    // Split off an exponent (`e`/`E`, optional sign), then the fraction.
    let (mantissa, exponent) =
        text.find(['e', 'E']).map_or((text, None), |at| (&text[..at], Some(&text[at..])));
    let (int_part, frac_part) = mantissa
        .find('.')
        .map_or((mantissa, None), |at| (&mantissa[..at], Some(&mantissa[at + 1..])));
    // A digit run is digits with underscores strictly between digits. The integer part may be
    // absent (`.5`) and the fraction empty (`1.`), but the mantissa needs at least one digit.
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
    // Regroup a run iff it already carries separators or is long enough to need them; four
    // digits or fewer stay bare (`1000` is fine as-is).
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

/// Insert `_` group separators into a bare digit string, every three digits counted from the
/// right (integer part, exponent) or from the left of the decimal point (fractional part).
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
