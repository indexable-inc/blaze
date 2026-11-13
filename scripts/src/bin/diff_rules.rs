//! diff_rules: blaze-diff's tables from their one source, crates/blaze-diff/rules.json. The port
//! of `harness/v2/diff_rules.py`, deleted once both were run on the same tree and agreed
//! (docs/v2/plan/design/scripts.md, "Ports and their parity"). `DOC` below is what its usage
//! error prints.
//!
//!     blaze run -q -p scripts --bin diff_rules -- '{"command": "check"}'
//!     blaze run -q -p scripts --bin diff_rules -- '{"command": "write"}'
//!     blaze run -q -p scripts --bin diff_rules -- selftest

/// What the usage error prints: harness/v2/diff_rules.py's docstring, with this script's commands.
const DOC: &str = r#"diff_rules.py: blaze-diff's tables from their one source, crates/blaze-diff/rules.json (the
operations, the derivative rules, the simplifier's rewrites). It writes the Rust tables the crate
runs and the Lean tables the proofs are about, so the two cannot drift
(docs/v2/plan/design/formal-verification.md, "Derivatives").

    blaze run -q -p scripts --bin diff_rules -- '{"command": "check"}'
    blaze run -q -p scripts --bin diff_rules -- '{"command": "write"}'
    blaze run -q -p scripts --bin diff_rules -- selftest

    command   check   rules.json is well formed and every generated file is what `write` would
                      write (CI)
              write   regenerate them
    root      the checkout (".")

What is written:

    crates/blaze-diff/src/rules_gen.rs         `OPS`, `RULES`, `REWRITES`, included by lib.rs
    lean/analysis/BlazeDiff/Ops.lean           `Op`, its arity and sort
    lean/analysis/BlazeDiff/Rules.lean         each rule's tangent, domain and kink
    lean/analysis/BlazeDiff/Rewrites.lean      each fast_math rewrite as a statement over the
                                               reals, and `Rewrite.AllSound` with one field per
                                               statement, so a rewrite without a proof fails
                                               `lake build`

An exact rewrite is a claim about IEEE 754 bits, not about the reals: nothing is written to Lean
for it, and the crate's bit-exact tests hold it.

The expressions of rules.json are one small language, in the one form `check` accepts (what this
script prints: a non-canonical spelling is refused, naming the canonical one):

    e ::= number | name | -e | e + e | e - e | e * e | e / e | name(e, ..) | (e)
        | e < e | e <= e | e > e | e >= e | e == e | e != e

A `-` directly before a number is that negative constant (`-1`, `-0.0`); `-(1)` is the negation.
In a tangent the names are a0 a1 a2 (arguments), t0 t1 t2 (their derivatives), out, choice,
exponent, powi_down; the calls are a float operation of one argument, `powf(e, e)` and
`select(cond | e < e | e > e | e == e, e, e)`. In a rewrite the names are a b c d (pattern
variables), true, false; the calls are any operation by name and `powi(e, n)`.
"#;

use std::{collections::BTreeSet, path::PathBuf};

use blaze_script::{
    TempDir,
    input::{self, Fields, Input, InputError},
    pyfmt::{float_repr, fraction, repr_str, repr_strs},
    pyjson::dumps,
    pystr::strip,
    read_text,
    regex::Regex,
};
use serde_json::{Value, json};

const SCRIPT: &str = "diff_rules.py";
const SOURCE: &str = "crates/blaze-diff/rules.json";
const RUST: &str = "crates/blaze-diff/src/rules_gen.rs";
const LEAN: &str = "lean/analysis/BlazeDiff";
const DOMAINS: [&str; 7] =
    ["everywhere", "arg_positive", "arg_non_zero", "divisor_non_zero", "args_differ", "away_from_poles", "powi_base"];
const SLOTS: [(&str, (&str, u8)); 6] =
    [("a0", ("A", 0)), ("a1", ("A", 1)), ("a2", ("A", 2)), ("t0", ("T", 0)), ("t1", ("T", 1)), ("t2", ("T", 2))];
const LEAVES: [(&str, (&str, &str)); 4] = [
    ("out", ("Out", "out")),
    ("choice", ("Choice", "choice")),
    ("exponent", ("Exponent", "exponent")),
    ("powi_down", ("PowiDown", "powiDown")),
];
const VARS: [&str; 4] = ["a", "b", "c", "d"];
const BINARY: [(&str, &str); 4] = [("+", "add"), ("-", "sub"), ("*", "mul"), ("/", "div")];
const COMPARE: [(&str, &str); 6] = [("<", "lt"), ("<=", "le"), (">", "gt"), (">=", "ge"), ("==", "eq"), ("!=", "ne")];

fn lookup<'a, V: Copy>(table: &[(&'a str, V)], key: &str) -> Option<V> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// `SYMBOL`: an operation's name to its symbol.
fn symbol(op: &str) -> &'static str {
    BINARY.iter().chain(&COMPARE).find(|(_, name)| *name == op).map_or("", |(sym, _)| *sym)
}

/// `Bad`: what is wrong with rules.json, naming the row.
type Bad = String;

/// An expression of rules.json, as the Python's tuples.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Num(f64),
    Id(String),
    Neg(Box<Node>),
    Op(String, Box<Node>, Box<Node>),
    Cmp(String, Box<Node>, Box<Node>),
    Call(String, Vec<Node>),
}

fn negative_zero(value: f64) -> bool {
    value == 0.0 && value.is_sign_negative()
}

struct Op {
    name: String,
    arity: i64,
    sort: String,
}

impl Input for Op {
    const FIELDS: &'static [&'static str] = &["name", "arity", "sort"];

    fn parse(f: &Fields<'_>) -> Result<Op, InputError> {
        Ok(Op { name: f.str("name", None)?, arity: f.int("arity", None)?, sort: f.one_of("sort", &["float", "bool"], None)? })
    }
}

struct Kink {
    point: String,
    at: String,
}

impl Input for Kink {
    const FIELDS: &'static [&'static str] = &["point", "at"];

    fn parse(f: &Fields<'_>) -> Result<Kink, InputError> {
        Ok(Kink { point: f.str("point", None)?, at: f.one_of("at", &["value", "condition"], None)? })
    }
}

struct Rule {
    op: String,
    tangent: String,
    domain: String,
    ieee: String,
    kink: Option<Kink>,
    note: String,
}

impl Input for Rule {
    const FIELDS: &'static [&'static str] = &["op", "tangent", "domain", "ieee", "kink", "note"];

    fn parse(f: &Fields<'_>) -> Result<Rule, InputError> {
        Ok(Rule {
            op: f.str("op", None)?,
            tangent: f.str("tangent", None)?,
            domain: f.str("domain", None)?,
            ieee: f.str("ieee", None)?,
            kink: f.object("kink", false)?,
            note: f.str("note", Some(""))?,
        })
    }
}

struct Rewrite {
    name: String,
    lhs: String,
    rhs: String,
    /// The class is `class` in the file; `read` renames it, as the Python did (a keyword there).
    klass: String,
    holds: String,
    requires: Vec<String>,
    note: String,
}

impl Input for Rewrite {
    const FIELDS: &'static [&'static str] = &["name", "lhs", "rhs", "klass", "holds", "requires", "note"];

    fn parse(f: &Fields<'_>) -> Result<Rewrite, InputError> {
        Ok(Rewrite {
            name: f.str("name", None)?,
            lhs: f.str("lhs", None)?,
            rhs: f.str("rhs", None)?,
            klass: f.one_of("klass", &["exact", "fast_math"], None)?,
            holds: f.str("holds", Some(""))?,
            requires: f.strs("requires")?,
            note: f.str("note", Some(""))?,
        })
    }
}

struct Rules {
    ops: Vec<Op>,
    rules: Vec<Rule>,
    rewrites: Vec<Rewrite>,
}

impl Input for Rules {
    const FIELDS: &'static [&'static str] = &["about", "ops", "rules", "rewrites"];

    fn parse(f: &Fields<'_>) -> Result<Rules, InputError> {
        // `about` is read for its refusals; nothing is generated from it.
        f.str("about", None)?;
        Ok(Rules { ops: f.objects("ops", true)?, rules: f.objects("rules", true)?, rewrites: f.objects("rewrites", true)? })
    }
}

struct Args {
    command: String,
    root: String,
}

impl Input for Args {
    const FIELDS: &'static [&'static str] = &["command", "root"];

    fn parse(f: &Fields<'_>) -> Result<Args, InputError> {
        Ok(Args { command: f.one_of("command", &["check", "write"], None)?, root: f.str("root", Some("."))? })
    }
}

struct Patterns {
    token: Regex,
    op_name: Regex,
    rewrite_name: Regex,
}

impl Patterns {
    fn new() -> Patterns {
        let re = |pattern: &str| Regex::new(pattern).unwrap_or_else(|e| blaze_script::fail(SCRIPT, &e.to_string()));
        Patterns {
            // Python's `TOKEN.match(text, at)`: anchored where the last token ended.
            token: re(r"\A\s*(?:(\d+(?:\.\d+)?)|([a-z_][a-z0-9_]*)|(<=|>=|==|!=|[-+*/(),<>]))"),
            op_name: re(r"\A(?:[a-z]+(_[a-z]+)*)\z"),
            rewrite_name: re(r"\A(?:[a-z0-9]+(-[a-z0-9]+)*)\z"),
        }
    }
}

// The expression language.

#[derive(Debug, Clone, PartialEq)]
struct Tok {
    kind: &'static str,
    value: String,
}

fn tok(kind: &'static str, value: &str) -> Tok {
    Tok { kind, value: value.to_owned() }
}

fn tokens(p: &Patterns, text: &str) -> Result<Vec<Tok>, Bad> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let Some(found) = p.token.captures(&text[at..]) else {
            if strip(&text[at..]).is_empty() {
                break;
            }
            return Err(format!("`{text}`: nothing reads `{}`", strip(&text[at..])));
        };
        let group = |i: usize| found.get(i).map(|m| m.as_str()).filter(|it| !it.is_empty());
        out.push(match (group(1), group(2), group(3)) {
            (Some(number), _, _) => tok("num", number),
            (None, Some(name), _) => tok("id", name),
            (None, None, symbol) => tok("sym", symbol.unwrap_or("")),
        });
        at += found.get(0).map_or(0, |m| m.end());
    }
    Ok(out)
}

/// Python's `float(text)` of a number token.
fn float_of(text: &str) -> Result<f64, Bad> {
    text.parse::<f64>().map_err(|_| format!("could not convert string to float: {}", repr_str(text)))
}

struct Reader<'t> {
    text: &'t str,
    toks: Vec<Tok>,
    at: usize,
}

impl Reader<'_> {
    fn peek(&self) -> Tok {
        self.toks.get(self.at).cloned().unwrap_or_else(|| tok("end", ""))
    }

    fn peek_is(&self, kind: &str, value: &str) -> bool {
        let next = self.peek();
        next.kind == kind && next.value == value
    }

    fn take(&mut self) -> Tok {
        let next = self.peek();
        self.at += 1;
        next
    }

    fn want(&mut self, symbol: &str) -> Result<(), Bad> {
        let next = self.take();
        if next.kind != "sym" || next.value != symbol {
            return Err(format!("`{}`: `{symbol}` is missing", self.text));
        }
        Ok(())
    }

    fn compare(&mut self) -> Result<Node, Bad> {
        let left = self.total()?;
        let next = self.peek();
        if next.kind == "sym"
            && let Some(op) = lookup(&COMPARE, &next.value)
        {
            self.take();
            return Ok(Node::Cmp(op.to_owned(), Box::new(left), Box::new(self.total()?)));
        }
        Ok(left)
    }

    fn total(&mut self) -> Result<Node, Bad> {
        let mut left = self.product()?;
        while self.peek_is("sym", "+") || self.peek_is("sym", "-") {
            let op = lookup(&BINARY, &self.take().value).unwrap_or("");
            left = Node::Op(op.to_owned(), Box::new(left), Box::new(self.product()?));
        }
        Ok(left)
    }

    fn product(&mut self) -> Result<Node, Bad> {
        let mut left = self.unary()?;
        while self.peek_is("sym", "*") || self.peek_is("sym", "/") {
            let op = lookup(&BINARY, &self.take().value).unwrap_or("");
            left = Node::Op(op.to_owned(), Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Node, Bad> {
        if self.peek_is("sym", "-") {
            self.take();
            if self.peek().kind == "num" {
                return Ok(Node::Num(-float_of(&self.take().value)?));
            }
            return Ok(Node::Neg(Box::new(self.unary()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Node, Bad> {
        let Tok { kind, value } = self.take();
        if kind == "num" {
            return Ok(Node::Num(float_of(&value)?));
        }
        if kind == "id" {
            if !self.peek_is("sym", "(") {
                return Ok(Node::Id(value));
            }
            self.take();
            let mut args = vec![self.compare()?];
            while self.peek_is("sym", ",") {
                self.take();
                args.push(self.compare()?);
            }
            self.want(")")?;
            return Ok(Node::Call(value, args));
        }
        if kind == "sym" && value == "(" {
            let inner = self.compare()?;
            self.want(")")?;
            return Ok(inner);
        }
        let place = if value.is_empty() { " at the end".to_owned() } else { format!(" before `{value}`") };
        Err(format!("`{}`: an expression is missing{place}", self.text))
    }
}

fn read_expr(p: &Patterns, text: &str) -> Result<Node, Bad> {
    let mut reader = Reader { text, toks: tokens(p, text)?, at: 0 };
    let out = reader.compare()?;
    if reader.at != reader.toks.len() {
        let left = reader.toks.get(reader.at).map_or("", |it| it.value.as_str());
        return Err(format!("`{text}`: `{left}` is left over"));
    }
    Ok(out)
}

/// Python's `number`: a whole value as an int, any other as its `repr`.
fn number(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && !negative_zero(value) {
        format!("{value:.0}")
    } else {
        float_repr(value)
    }
}

/// `node` in the one canonical spelling: the fewest brackets that read back as it.
fn show(node: &Node, least: u8) -> String {
    let (text, binds) = match node {
        Node::Num(value) => (number(*value), 4),
        Node::Id(name) => (name.clone(), 4),
        Node::Neg(inner) => {
            // `-1` is a constant and `--a` reads badly: both get brackets.
            let inside = match **inner {
                Node::Num(_) | Node::Neg(_) => format!("({})", show(inner, 0)),
                _ => show(inner, 3),
            };
            (format!("-{inside}"), 3)
        }
        Node::Op(op, left, right) => {
            let binds = if op == "add" || op == "sub" { 1 } else { 2 };
            (format!("{} {} {}", show(left, binds), symbol(op), show(right, binds + 1)), binds)
        }
        Node::Cmp(op, left, right) => (format!("{} {} {}", show(left, 1), symbol(op), show(right, 1)), 0),
        Node::Call(name, args) => {
            let args: Vec<String> = args.iter().map(|a| show(a, 0)).collect();
            (format!("{name}({})", args.join(", ")), 4)
        }
    };
    if binds < least { format!("({text})") } else { text }
}

fn canonical(p: &Patterns, text: &str, where_: &str) -> Result<Node, Bad> {
    let node = read_expr(p, text).map_err(|e| format!("{where_}: {e}"))?;
    if show(&node, 0) != text {
        return Err(format!("{where_}: `{text}` is written `{}`", show(&node, 0)));
    }
    Ok(node)
}

/// Python's `str.capitalize`: the first character upper case, the rest lower.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

fn camel(name: &str, upper: bool) -> String {
    let mut parts = name.split('_');
    let first = parts.next().unwrap_or("");
    let mut out = if upper { capitalize(first) } else { first.to_owned() };
    for it in parts {
        out.push_str(&capitalize(it));
    }
    out
}

/// A constant as a Lean real.
fn real(value: f64) -> String {
    let (numerator, denominator) = fraction(value);
    if denominator == "1" { format!("({numerator} : ℝ)") } else { format!("(({numerator} : ℝ) / {denominator})") }
}

fn op_of<'o>(ops: &'o [Op], name: &str) -> Option<&'o Op> {
    ops.iter().find(|it| it.name == name)
}

// A tangent, to Rust's `P` and Lean's `P`.

fn pair(lean: bool, rust: &str, lean_name: &str, parts: &[String]) -> String {
    if lean {
        return if parts.is_empty() { format!("P.{lean_name}") } else { format!("(P.{lean_name} {})", parts.join(" ")) };
    }
    if parts.is_empty() {
        format!("P::{rust}")
    } else {
        let parts: Vec<String> = parts.iter().map(|p| format!("&{p}")).collect();
        format!("P::{rust}({})", parts.join(", "))
    }
}

fn tangent(node: &Node, ops: &[Op], where_: &str, lean: bool) -> Result<String, Bad> {
    let go = |node: &Node| tangent(node, ops, where_, lean);
    match node {
        Node::Num(value) => {
            return Ok(if lean { format!("(P.lit {})", real(*value)) } else { format!("P::Lit({})", float_repr(*value)) });
        }
        Node::Id(name) => {
            if let Some((kind, at)) = lookup(&SLOTS, name) {
                return Ok(if lean { format!("(P.{} .s{at})", kind.to_lowercase()) } else { format!("P::{kind}({at})") });
            }
            if let Some((rust, lean_name)) = lookup(&LEAVES, name) {
                return Ok(pair(lean, rust, lean_name, &[]));
            }
        }
        Node::Op(op, left, right) => return Ok(pair(lean, &camel(op, true), op, &[go(left)?, go(right)?])),
        Node::Neg(inner) => return Ok(pair(lean, "Neg", "neg", &[go(inner)?])),
        Node::Call(name, args) if name == "powf" && args.len() == 2 => {
            return Ok(pair(lean, "Powf", "powf", &[go(&args[0])?, go(&args[1])?]));
        }
        // A `select` of three: on `cond`, or on a `<`, `>` or `==`; any other is no tangent.
        Node::Call(name, args) if name == "select" && args.len() == 3 => match &args[0] {
            Node::Id(cond) if cond == "cond" => {
                let (then, other) = (go(&args[1])?, go(&args[2])?);
                return Ok(if lean {
                    format!("(P.selCond {then} {other})")
                } else {
                    format!("P::Select(C::Cond, &{then}, &{other})")
                });
            }
            Node::Cmp(op, left, right) if ["lt", "gt", "eq"].contains(&op.as_str()) => {
                let (left, right, then, other) = (go(left)?, go(right)?, go(&args[1])?, go(&args[2])?);
                return Ok(if lean {
                    format!("(P.sel .{op} {left} {right} {then} {other})")
                } else {
                    format!("P::Select(C::{}(&{left}, &{right}), &{then}, &{other})", camel(op, true))
                });
            }
            _ => {}
        },
        Node::Call(name, args)
            if args.len() == 1
                && op_of(ops, name).is_some_and(|op| op.arity == 1 && op.sort == "float")
                && name != "powi" =>
        {
            let inner = go(&args[0])?;
            return Ok(if lean {
                format!("(P.call .{} {inner})", camel(name, false))
            } else {
                format!("P::Call(Op::{}, &{inner})", camel(name, true))
            });
        }
        Node::Cmp(..) | Node::Call(..) => {}
    }
    Err(format!("{where_}: a tangent has no `{}`", show(node, 0)))
}

// A rewrite's side, to Rust's `Pat` and to a Lean `Expr`.

fn exponent_of(node: &Node, where_: &str) -> Result<i64, Bad> {
    match node {
        Node::Num(value) if value.fract() == 0.0 && (-(2f64.powi(31))..2f64.powi(31)).contains(value) => {
            Ok(*value as i64)
        }
        _ => Err(format!("{where_}: a `powi` exponent is an i32 constant, not `{}`", show(node, 0))),
    }
}

fn pattern(node: &Node, ops: &[Op], where_: &str, lean: bool) -> Result<String, Bad> {
    let go = |node: &Node| pattern(node, ops, where_, lean);
    let of = |name: &str, args: &[&Node]| -> Result<String, Bad> {
        let fits = op_of(ops, name).is_some_and(|op| usize::try_from(op.arity) == Ok(args.len()));
        if !fits || args.is_empty() || name == "powi" {
            return Err(format!("{where_}: no operation `{name}` of {} arguments", args.len()));
        }
        let parts = args.iter().copied().map(&go).collect::<Result<Vec<String>, Bad>>()?;
        Ok(if lean {
            format!("(Expr.{} .{} {})", ["un", "bin", "ter"][args.len() - 1], camel(name, false), parts.join(" "))
        } else {
            format!("Pat::N(Op::{}, &[{}])", camel(name, true), parts.join(", "))
        })
    };
    match node {
        Node::Num(value) => {
            if lean && negative_zero(*value) {
                return Err(format!("{where_}: -0.0 is 0 over the reals: a fast_math rewrite does not name it"));
            }
            Ok(if lean { format!("(Expr.const {})", real(*value)) } else { format!("Pat::K({})", float_repr(*value)) })
        }
        Node::Id(name) if name == "true" || name == "false" => {
            Ok(if lean { format!("(Expr.bool {name})") } else { format!("Pat::B({name})") })
        }
        Node::Id(name) => match VARS.iter().position(|it| *it == name.as_str()) {
            Some(at) => Ok(if lean { name.clone() } else { format!("Pat::V({at})") }),
            None => Err(format!("{where_}: a rewrite has no `{}`", show(node, 0))),
        },
        Node::Op(op, left, right) | Node::Cmp(op, left, right) => of(op, &[&**left, &**right]),
        Node::Neg(inner) => of("neg", &[&**inner]),
        Node::Call(name, args) if name == "powi" && args.len() == 2 => {
            let n = exponent_of(&args[1], where_)?;
            let base = go(&args[0])?;
            Ok(if lean {
                format!("(Expr.powi (Blaze.Scalar.lit .I32 {}) {base})", real_int(n))
            } else {
                format!("Pat::Powi(&{base}, {n})")
            })
        }
        Node::Call(name, args) => of(name, &args.iter().collect::<Vec<_>>()),
    }
}

fn real_int(n: i64) -> String {
    if n >= 0 { n.to_string() } else { format!("({n})") }
}

fn variables(node: &Node) -> BTreeSet<String> {
    match node {
        Node::Id(name) if VARS.contains(&name.as_str()) => BTreeSet::from([name.clone()]),
        Node::Id(_) | Node::Num(_) => BTreeSet::new(),
        Node::Neg(inner) => variables(inner),
        Node::Op(_, left, right) | Node::Cmp(_, left, right) => variables(left).union(&variables(right)).cloned().collect(),
        Node::Call(_, args) => args.iter().flat_map(variables).collect(),
    }
}

/// A `requires` entry, `<variable> <comparison> <number>`, as a Lean hypothesis over `env`.
fn requirement(p: &Patterns, text: &str, where_: &str) -> Result<String, Bad> {
    if let Node::Cmp(op, left, right) = canonical(p, text, where_)?
        && let (Node::Id(name), Node::Num(value)) = (*left, *right)
        && VARS.contains(&name.as_str())
    {
        let (value_text, term) = (real(value), format!("{name}.eval env"));
        return Ok(match op.as_str() {
            "ne" => format!("{term} ≠ {value_text}"),
            "lt" => format!("{term} < {value_text}"),
            "le" => format!("{term} ≤ {value_text}"),
            "gt" => format!("{value_text} < {term}"),
            "ge" => format!("{value_text} ≤ {term}"),
            _ => format!("{term} = {value_text}"),
        });
    }
    Err(format!("{where_}: a requirement is a variable compared with a number, not `{text}`"))
}

// Reading and checking the source.

/// rules.json's text as `Rules`; `class` is the field `klass`.
fn read(text: &str) -> Result<Rules, Bad> {
    let mut data: Value = serde_json::from_str(text).map_err(|e| format!("{SOURCE} is not JSON: {e}"))?;
    if let Some(Value::Array(rows)) = data.get_mut("rewrites") {
        for row in rows {
            if let Value::Object(row) = row
                && let Some(class) = row.shift_remove("class")
            {
                row.insert("klass".to_owned(), class);
            }
        }
    }
    input::of::<Rules>(&data, "").map_err(|InputError(said)| format!("{SOURCE}: {said}"))
}

/// The operations by name, once every row is well formed.
fn checked<'r>(p: &Patterns, rules: &'r Rules) -> Result<&'r [Op], Bad> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for op in &rules.ops {
        if !p.op_name.is_match(&op.name) || seen.contains(op.name.as_str()) || !(0..=3).contains(&op.arity) {
            return Err(format!(
                "operation `{}`: a name is lower snake case and used once, an arity is 0 to 3",
                op.name
            ));
        }
        seen.insert(op.name.as_str());
    }
    let ops = rules.ops.as_slice();
    let ruled: Vec<&str> = rules.rules.iter().map(|r| r.op.as_str()).collect();
    for op in ops {
        let differentiable = op.sort == "float" && op.name != "const" && op.name != "var";
        let count = ruled.iter().filter(|it| **it == op.name).count();
        if count != usize::from(differentiable) {
            return Err(format!(
                "operation `{}` has {count} rules: a float operation but `const` and `var` has one, a condition none",
                op.name
            ));
        }
    }
    for rule in &rules.rules {
        let where_ = format!("the rule of `{}`", rule.op);
        if op_of(ops, &rule.op).is_none() {
            return Err(format!("{where_}: no such operation"));
        }
        if !DOMAINS.contains(&rule.domain.as_str()) {
            return Err(format!("{where_}: the domain `{}` is none of {}", rule.domain, DOMAINS.join(", ")));
        }
        tangent(&canonical(p, &rule.tangent, &where_)?, ops, &where_, false)?;
    }
    // Python compared the rules' operations with the ruled operations in order; names are unique.
    let in_ops: Vec<&str> = ops.iter().filter(|op| ruled.contains(&op.name.as_str())).map(|op| op.name.as_str()).collect();
    if ruled != in_ops {
        return Err("the rules are not in the operations' order".to_owned());
    }
    let mut names: BTreeSet<&str> = BTreeSet::new();
    let mut fast = false;
    for row in &rules.rewrites {
        let where_ = format!("the rewrite `{}`", row.name);
        if !p.rewrite_name.is_match(&row.name) || names.contains(row.name.as_str()) {
            return Err(format!("{where_}: a name is lower kebab case and used once"));
        }
        names.insert(row.name.as_str());
        let (lhs, rhs) = (canonical(p, &row.lhs, &where_)?, canonical(p, &row.rhs, &where_)?);
        pattern(&lhs, ops, &where_, false)?;
        pattern(&rhs, ops, &where_, false)?;
        if matches!(lhs, Node::Id(_)) {
            return Err(format!("{where_}: the left side matches everything"));
        }
        let (bound, used) = (variables(&lhs), variables(&rhs));
        if !used.is_subset(&bound) {
            let unbound: Vec<&str> = used.difference(&bound).map(String::as_str).collect();
            return Err(format!("{where_}: the right side names {}, which the left does not bind", unbound.join(", ")));
        }
        if row.klass == "exact" {
            if fast {
                return Err(format!("{where_}: the exact rewrites come first"));
            }
            if !row.holds.is_empty() || !row.requires.is_empty() {
                return Err(format!("{where_}: an exact rewrite holds everywhere: it has no `holds` and no `requires`"));
            }
        } else {
            fast = true;
            if row.holds.is_empty() {
                return Err(format!("{where_}: a fast_math rewrite says in `holds` when it holds"));
            }
            for (at, text) in row.requires.iter().enumerate() {
                requirement(p, text, &format!("{where_}, requirement {at}"))?;
                if !variables(&read_expr(p, text)?).is_subset(&bound) {
                    return Err(format!("{where_}: `{text}` names a variable the left side does not bind"));
                }
            }
        }
    }
    Ok(ops)
}

// The generated files.

fn rust_string(text: &str) -> Result<String, Bad> {
    if !text.is_ascii() || text.contains(['\\', '"', '\n']) {
        return Err(format!("`{text}`: a string a table carries is plain ASCII on one line, with no quote or backslash"));
    }
    Ok(format!("\"{text}\""))
}

fn rust(p: &Patterns, rules: &Rules, ops: &[Op]) -> Result<String, Bad> {
    let mut out: Vec<String> = [
        &*format!("// Generated by scripts/src/bin/diff_rules.rs from {SOURCE}: edit the rules there, then run it with"),
        "// command `write`. lib.rs includes this file as the module `rules_gen`; it is not a module file,",
        "// so no formatter rewrites it and `check` can hold it to its bytes.",
        "",
        "use crate::{",
        "    diff::{C, Domain, Kink, KinkAt, P, Rule},",
        "    expr::{Op, Sort},",
        "    simplify::{Ieee, Pat, Rewrite},",
        "};",
        "",
        "/// Every operation with its arity and sort, as rules.json lists them: what the Lean `Op` is",
        "/// generated from. A test holds it to `Op::ALL`, `Op::arity` and `Op::sort`.",
        "pub const OPS: &[(Op, usize, Sort)] = &[",
    ]
    .iter()
    .map(|it| (*it).to_owned())
    .collect();
    for op in &rules.ops {
        out.push(format!("    (Op::{}, {}, Sort::{}),", camel(&op.name, true), op.arity, capitalize(&op.sort)));
    }
    out.extend(["];", "", "/// The table behind `diff::RULES`.", "pub const RULES: &[Rule] = &["].map(str::to_owned));
    for rule in &rules.rules {
        let where_ = format!("the rule of `{}`", rule.op);
        if !rule.note.is_empty() {
            out.push(format!("    // {}", rule.note));
        }
        let kink = match &rule.kink {
            None => "None".to_owned(),
            Some(kink) => {
                format!("Some(Kink {{ point: {}, at: KinkAt::{} }})", rust_string(&kink.point)?, capitalize(&kink.at))
            }
        };
        out.extend([
            "    Rule {".to_owned(),
            format!("        op: Op::{},", camel(&rule.op, true)),
            format!("        tangent: {},", tangent(&read_expr(p, &rule.tangent)?, ops, &where_, false)?),
            format!("        kink: {kink},"),
            format!("        domain: Domain::{},", camel(&rule.domain, true)),
            format!("        ieee: {},", rust_string(&rule.ieee)?),
            "    },".to_owned(),
        ]);
    }
    out.extend(["];", "", "/// The table behind `simplify::REWRITES`.", "pub const REWRITES: &[Rewrite] = &["].map(str::to_owned));
    for row in &rules.rewrites {
        let where_ = format!("the rewrite `{}`", row.name);
        if !row.note.is_empty() {
            out.push(format!("    // {}", row.note));
        }
        let ieee =
            if row.klass == "exact" { "Ieee::Exact".to_owned() } else { format!("Ieee::FastMath({})", rust_string(&row.holds)?) };
        out.extend([
            "    Rewrite {".to_owned(),
            format!("        name: {},", rust_string(&row.name)?),
            format!("        lhs: {},", pattern(&read_expr(p, &row.lhs)?, ops, &where_, false)?),
            format!("        rhs: {},", pattern(&read_expr(p, &row.rhs)?, ops, &where_, false)?),
            format!("        ieee: {ieee},"),
            "    },".to_owned(),
        ]);
    }
    out.push("];".to_owned());
    Ok(out.join("\n") + "\n")
}

fn lean_header(what: &str) -> Vec<String> {
    vec![
        "/-".to_owned(),
        format!("Generated by scripts/src/bin/diff_rules.rs from {SOURCE}: edit the rules there, then run it"),
        format!("with command `write`. {what}"),
        "-/".to_owned(),
    ]
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|it| (*it).to_owned()).collect()
}

fn lean_ops(rules: &Rules) -> String {
    let names: Vec<String> = rules.ops.iter().map(|op| camel(&op.name, false)).collect();
    let mut out = lean_header("The operations of `blaze_diff::expr::Op`, in its order.");
    out.extend(strings(&["", "namespace BlazeDiff", "", "/-- A node's operation: `blaze_diff::expr::Op`. -/", "inductive Op where"]));
    out.extend(names.iter().map(|name| format!("  | {name}")));
    out.extend(strings(&[
        "  deriving DecidableEq, Repr",
        "",
        "namespace Op",
        "",
        "/-- How many arguments a node of it has. -/",
        "def arity : Op → Nat",
    ]));
    out.extend(names.iter().zip(&rules.ops).map(|(name, op)| format!("  | .{name} => {}", op.arity)));
    out.extend(strings(&["", "/-- Whether a node of it is a float (a condition otherwise). -/", "def isFloat : Op → Bool"]));
    out.extend(
        names
            .iter()
            .zip(&rules.ops)
            .map(|(name, op)| format!("  | .{name} => {}", if op.sort == "float" { "true" } else { "false" })),
    );
    out.extend(strings(&["", "end Op", "", "end BlazeDiff"]));
    out.join("\n") + "\n"
}

fn lean_rules(p: &Patterns, rules: &Rules, ops: &[Op]) -> Result<String, Bad> {
    // `{r.op: r for r in rules.rules}`: the last rule of an operation wins.
    let by_op = |name: &str| rules.rules.iter().rev().find(|r| r.op == name);
    let names: Vec<(&Op, String)> = rules.ops.iter().map(|op| (op, camel(&op.name, false))).collect();
    let mut out = lean_header("Each operation's derivative rule: `blaze_diff::diff::RULES`.");
    out.extend(strings(&["import BlazeDiff.Template", "", "noncomputable section", "", "namespace BlazeDiff.Rules", ""]));
    let mut table = |doc: &str, head: &str, value: &dyn Fn(&Op) -> Result<String, Bad>| -> Result<(), Bad> {
        out.push(format!("/-- {doc} -/"));
        out.push(head.to_owned());
        for (op, name) in &names {
            out.push(format!("  | .{name} => {}", value(op)?));
        }
        out.push(String::new());
        Ok(())
    };
    let flag = |holds: bool| -> Result<String, Bad> { Ok(if holds { "true" } else { "false" }.to_owned()) };
    table(
        "Whether the operation has a rule: a float operation but `const` and `var`.",
        "def hasRule : Op → Bool",
        &|op| flag(by_op(&op.name).is_some()),
    )?;
    table(
        "A ruled operation of one argument that is not `powi` (which carries its exponent).",
        "def unary : Op → Bool",
        &|op| flag(by_op(&op.name).is_some() && op.arity == 1 && op.name != "powi"),
    )?;
    table("A ruled operation of two arguments.", "def binary : Op → Bool", &|op| {
        flag(by_op(&op.name).is_some() && op.arity == 2)
    })?;
    table(
        "The node's derivative, as a template; `P.lit 0` for an operation without a rule.",
        "def tangent : Op → P",
        &|op| match by_op(&op.name) {
            Some(rule) => tangent(&read_expr(p, &rule.tangent)?, ops, &format!("the rule of `{}`", op.name), true),
            None => Ok(format!("(P.lit {})", real(0.0))),
        },
    )?;
    table("Where the rule holds; `everywhere` for an operation without a rule.", "def domain : Op → Domain", &|op| {
        Ok(match by_op(&op.name) {
            Some(rule) => format!(".{}", camel(&rule.domain, false)),
            None => ".everywhere".to_owned(),
        })
    })?;
    table("Where the operation has no derivative, when it has such a point.", "def kink : Op → Option KinkAt", &|op| {
        Ok(match by_op(&op.name).and_then(|rule| rule.kink.as_ref()) {
            None => "none".to_owned(),
            Some(kink) => format!("some .{}", kink.at),
        })
    })?;
    out.extend(strings(&["end BlazeDiff.Rules", "", "end"]));
    Ok(out.join("\n") + "\n")
}

fn lean_name(name: &str) -> String {
    name.replace('-', "_")
}

fn lean_rewrites(p: &Patterns, rules: &Rules, ops: &[Op]) -> Result<String, Bad> {
    let fast: Vec<&Rewrite> = rules.rewrites.iter().filter(|r| r.klass == "fast_math").collect();
    let exact: Vec<&str> = rules.rewrites.iter().filter(|r| r.klass == "exact").map(|r| r.name.as_str()).collect();
    let mut out = lean_header(&format!(
        "Each fast_math rewrite of `blaze_diff::simplify::REWRITES` as a statement over the reals:\n\
         wherever the left side matches, the term means what the right side means, under the rewrite's\n\
         `requires`. `Rewrite.AllSound` has one field per statement; BlazeDiff/RewriteProofs.lean gives\n\
         its one value, so a rewrite added without a proof does not build.\n\n\
         Nothing is stated for the exact rewrites: they claim equal IEEE 754 bits, which is not a claim\n\
         about the reals; the crate's bit-exact tests hold them. They are: {}.",
        exact.join(", ")
    ));
    out.extend(strings(&["import BlazeDiff.Expr", "", "namespace BlazeDiff.Rewrite", ""]));
    for row in &fast {
        let where_ = format!("the rewrite `{}`", row.name);
        let (lhs, rhs) = (read_expr(p, &row.lhs)?, read_expr(p, &row.rhs)?);
        let bound = variables(&lhs);
        let names: Vec<&str> = VARS.iter().copied().filter(|v| bound.contains(*v)).collect();
        let mut needs = String::new();
        for text in &row.requires {
            needs.push_str(&format!("{} →\n    ", requirement(p, text, &where_)?));
        }
        out.extend([
            format!("/-- `{}`: `{}` is `{}` ({}). -/", row.name, row.lhs, row.rhs, row.holds),
            format!("def {} : Prop :=", lean_name(&row.name)),
            format!("  ∀ (env : ℕ → ℝ) ({} : Expr),", names.join(" ")),
            format!(
                "    {needs}{}.eval env = {}.eval env",
                pattern(&lhs, ops, &where_, true)?,
                pattern(&rhs, ops, &where_, true)?
            ),
            String::new(),
        ]);
    }
    out.extend(strings(&[
        "/-- Every fast_math rewrite preserves the term's meaning over the reals. -/",
        "structure AllSound : Prop where",
    ]));
    out.extend(fast.iter().map(|row| format!("  {0} : BlazeDiff.Rewrite.{0}", lean_name(&row.name))));
    out.extend(strings(&["", "end BlazeDiff.Rewrite"]));
    Ok(out.join("\n") + "\n")
}

/// Every generated file's text, by path, in the Python dict's order.
fn generated(p: &Patterns, rules: &Rules) -> Result<Vec<(String, String)>, Bad> {
    let ops = checked(p, rules)?;
    Ok(vec![
        (RUST.to_owned(), rust(p, rules, ops)?),
        (format!("{LEAN}/Ops.lean"), lean_ops(rules)),
        (format!("{LEAN}/Rules.lean"), lean_rules(p, rules, ops)?),
        (format!("{LEAN}/Rewrites.lean"), lean_rewrites(p, rules, ops)?),
    ])
}

fn stale(files: &[(String, String)], root: &std::path::Path) -> Vec<String> {
    files
        .iter()
        .filter(|(path, text)| {
            let full = root.join(path);
            !full.is_file() || read_text(&full).as_deref() != Some(text.as_str())
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn main_with(p: &Patterns, it: &Args) {
    // `Path(root).resolve()`: a root that is not there is found out when rules.json is read.
    let root: PathBuf =
        std::fs::canonicalize(&it.root).unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(&it.root));
    let source = root.join(SOURCE);
    let Some(text) = read_text(&source) else {
        // The Python raised here (a traceback, exit status 1).
        blaze_script::fail(SCRIPT, &format!("{} could not be read as UTF-8 text", source.display()))
    };
    let files = match read(&text).and_then(|rules| generated(p, &rules)) {
        Ok(files) => files,
        Err(e) => blaze_script::fail(SCRIPT, &format!("{SOURCE}: {e}")),
    };
    if it.command == "write" {
        for (path, text) in &files {
            let full = root.join(path);
            let written = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
            if let Err(e) = written {
                blaze_script::fail(SCRIPT, &blaze_script::os_error(&e, &full.display().to_string()));
            }
        }
        return;
    }
    let wrong = stale(&files, &root);
    if !wrong.is_empty() {
        let lines: Vec<String> = wrong
            .iter()
            .map(|path| format!("{SCRIPT}: {path} is stale: run {SCRIPT} with command `write` and commit it"))
            .collect();
        eprintln!("{}", lines.join("\n"));
        std::process::exit(1);
    }
}

// The self-test.

fn expect(holds: bool, what: &str) {
    blaze_script::expect(SCRIPT, holds, what);
}

/// What failed in the selftest where the Python would have raised.
fn ok<T>(it: Result<T, Bad>) -> T {
    it.unwrap_or_else(|e| blaze_script::fail(&format!("{SCRIPT} selftest"), &e))
}

fn refused(p: &Patterns, text: &str, says: &str) -> bool {
    match read(text).and_then(|rules| generated(p, &rules)) {
        Err(e) => e.contains(says),
        Ok(_) => false,
    }
}

fn file<'f>(files: &'f [(String, String)], path: &str) -> &'f str {
    files.iter().find(|(it, _)| it == path).map_or("", |(_, text)| text.as_str())
}

fn selftest(p: &Patterns) {
    let reads = |text: &str| ok(read_expr(p, text));
    for text in [
        "t0 * a1 + a0 * t1 + t2",
        "(t0 - out * t1) / a1",
        "-(t0 * sin(a0))",
        "-(-a)",
        "a + -0.0",
        "a * -1",
        "-a * b",
        "a + (b + c)",
        "a - (b - c)",
        "a / (b * c)",
        "select(a0 == 0, choice * t0, t0 / (2 * out))",
        "-(1)",
        "a - 1",
        "0.5 * a",
    ] {
        let shown = show(&reads(text), 0);
        expect(shown == text, &format!("`{text}` reads and prints as itself, not `{shown}`"));
    }
    let id = |name: &str| Box::new(Node::Id(name.to_owned()));
    expect(
        reads("a * -1") == Node::Op("mul".to_owned(), id("a"), Box::new(Node::Num(-1.0))),
        "a minus before a number is the negative constant",
    );
    expect(
        reads("-a * b") == Node::Op("mul".to_owned(), Box::new(Node::Neg(id("a"))), id("b")),
        "a minus binds tighter than a product",
    );
    expect(
        matches!(reads("a + -0.0"), Node::Op(_, _, right) if matches!(*right, Node::Num(v) if negative_zero(v))),
        "-0.0 keeps its sign",
    );
    for (text, says) in [("(a + b)", "is written `a + b`"), ("a+b", "is written"), ("a + ", "is missing"), ("a $ b", "nothing reads")] {
        match canonical(p, text, "here") {
            Ok(_) => expect(false, &format!("`{text}` is refused")),
            Err(e) => expect(e.contains(says), &format!("`{text}`: {e}")),
        }
    }
    let op = |name: &str, arity: i64, sort: &str| Op { name: name.to_owned(), arity, sort: sort.to_owned() };
    let ops = [op("sin", 1, "float"), op("mul", 2, "float"), op("neg", 1, "float"), op("lt", 2, "bool")];
    expect(
        ok(tangent(&reads("t0 * sin(a0)"), &ops, "x", false)) == "P::Mul(&P::T(0), &P::Call(Op::Sin, &P::A(0)))",
        "a tangent in Rust",
    );
    expect(
        ok(tangent(&reads("t0 * sin(a0)"), &ops, "x", true)) == "(P.mul (P.t .s0) (P.call .sin (P.a .s0)))",
        "a tangent in Lean",
    );
    expect(
        ok(tangent(&reads("select(a0 < a1, -1, choice)"), &ops, "x", true))
            == "(P.sel .lt (P.a .s0) (P.a .s1) (P.lit (-1 : ℝ)) P.choice)",
        "a select in Lean",
    );
    expect(
        ok(pattern(&reads("-a * 2"), &ops, "x", false)) == "Pat::N(Op::Mul, &[Pat::N(Op::Neg, &[Pat::V(0)]), Pat::K(2.0)])",
        "a pattern in Rust",
    );
    expect(
        ok(pattern(&reads("powi(a, 2) * b"), &ops, "x", true)) == "(Expr.bin .mul (Expr.powi (Blaze.Scalar.lit .I32 2) a) b)",
        "a pattern in Lean",
    );
    expect(
        ok(requirement(p, "b != 0", "x")) == "b.eval env ≠ (0 : ℝ)"
            && ok(requirement(p, "a >= 0", "x")) == "(0 : ℝ) ≤ a.eval env",
        "requirements",
    );
    expect(real(0.5) == "((1 : ℝ) / 2)" && real(-1.0) == "(-1 : ℝ)", "a constant as a real");

    let small = json!({
        "about": "",
        "ops": [{"name": "const", "arity": 0, "sort": "float"}, {"name": "var", "arity": 0, "sort": "float"},
                {"name": "mul", "arity": 2, "sort": "float"}, {"name": "div", "arity": 2, "sort": "float"},
                {"name": "mul_add", "arity": 3, "sort": "float"}, {"name": "lt", "arity": 2, "sort": "bool"}],
        "rules": [{"op": "mul", "tangent": "t0 * a1 + a0 * t1", "domain": "everywhere", "ieee": "finite"},
                  {"op": "div", "tangent": "(t0 - out * t1) / a1", "domain": "divisor_non_zero", "ieee": "finite"},
                  {"op": "mul_add", "tangent": "t0 * a1 + a0 * t1 + t2", "domain": "everywhere", "ieee": "finite",
                   "kink": {"point": "0", "at": "value"}}],
        "rewrites": [{"name": "mul-one", "lhs": "a * 1", "rhs": "a", "class": "exact"},
                     {"name": "div-self", "lhs": "a / a", "rhs": "1", "class": "fast_math",
                      "holds": "a divisor that is not 0", "requires": ["a != 0"]}],
    });
    let files = ok(read(&dumps(&small)).and_then(|rules| generated(p, &rules)));
    let mut paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    paths.sort_unstable();
    let mut want = [RUST.to_owned(), format!("{LEAN}/Ops.lean"), format!("{LEAN}/Rules.lean"), format!("{LEAN}/Rewrites.lean")];
    want.sort();
    expect(paths == want, &format!("the files: {}", repr_strs(&paths)));
    let rust_file = file(&files, RUST);
    expect(
        rust_file.contains("(Op::MulAdd, 3, Sort::Float),") && rust_file.contains("ieee: Ieee::FastMath(\"a divisor that is not 0\"),"),
        rust_file,
    );
    expect(rust_file.contains("kink: Some(Kink { point: \"0\", at: KinkAt::Value }),"), "a kink in Rust");
    let ops_file = file(&files, &format!("{LEAN}/Ops.lean"));
    expect(ops_file.contains("  | mulAdd\n") && ops_file.contains("  | .lt => false"), ops_file);
    let rules_file = file(&files, &format!("{LEAN}/Rules.lean"));
    expect(rules_file.contains("  | .div => .divisorNonZero") && rules_file.contains("  | .mulAdd => some .value"), rules_file);
    let rewrites = file(&files, &format!("{LEAN}/Rewrites.lean"));
    expect(
        rewrites.contains(
            "def div_self : Prop :=\n  ∀ (env : ℕ → ℝ) (a : Expr),\n    a.eval env ≠ (0 : ℝ) →\n    \
             (Expr.bin .div a a).eval env = (Expr.const (1 : ℝ)).eval env",
        ),
        rewrites,
    );
    expect(
        rewrites.contains("  div_self : BlazeDiff.Rewrite.div_self")
            && !rewrites.contains("mul_one")
            && rewrites.contains("They are: mul-one."),
        "only fast_math rewrites are stated",
    );

    let changed = |edit: &dyn Fn(&mut Value)| {
        let mut copy = small.clone();
        edit(&mut copy);
        dumps(&copy)
    };
    let regenerated = ok(read(&changed(&|d| d["rules"][0]["tangent"] = json!("t0 * a1 - a0 * t1"))).and_then(|r| generated(p, &r)));
    expect(file(&regenerated, RUST) != rust_file, "a changed tangent changes the Rust table");
    let edit_rows = |key: &'static str, f: fn(&mut Vec<Value>)| {
        move |d: &mut Value| {
            if let Some(rows) = d[key].as_array_mut() {
                f(rows);
            }
        }
    };
    expect(
        refused(p, &changed(&edit_rows("rules", |rows| {
            rows.remove(0);
        })), "`mul` has 0 rules"),
        "a float operation without a rule is refused",
    );
    expect(
        refused(p, &changed(&|d| d["rules"][0]["domain"] = json!("nowhere")), "the domain `nowhere`"),
        "an unknown domain is refused",
    );
    expect(
        refused(p, &changed(&|d| d["rules"][0]["tangent"] = json!("t0 * b")), "a tangent has no `b`"),
        "a tangent naming a pattern variable is refused",
    );
    expect(
        refused(p, &changed(&edit_rows("rules", |rows| rows.reverse())), "not in the operations' order"),
        "rules out of order are refused",
    );
    expect(
        refused(p, &changed(&edit_rows("rewrites", |rows| rows.reverse())), "the exact rewrites come first"),
        "a fast_math rewrite before an exact one is refused",
    );
    expect(
        refused(p, &changed(&|d| d["rewrites"][1]["rhs"] = json!("b")), "which the left does not bind"),
        "an unbound variable is refused",
    );
    let drop_holds = |d: &mut Value| {
        if let Some(row) = d["rewrites"][1].as_object_mut() {
            row.shift_remove("holds");
        }
    };
    expect(refused(p, &changed(&drop_holds), "says in `holds`"), "a fast_math rewrite without `holds` is refused");
    expect(
        refused(p, &changed(&|d| d["rewrites"][1]["name"] = json!("mul-one")), "used once"),
        "a repeated name is refused",
    );
    expect(refused(p, &changed(&|d| d["rewrites"][0]["colour"] = json!("red")), "no field"), "an unknown field is refused");

    {
        let tmp = TempDir::new("tmp").unwrap_or_else(|e| blaze_script::fail(SCRIPT, &e.to_string()));
        let root = tmp.path();
        let mut missing = stale(&files, root);
        missing.sort();
        expect(missing == want, "a missing file is stale");
        for (path, text) in &files {
            let full = root.join(path);
            let written = full.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&full, text));
            expect(written.is_ok(), &format!("{path} is written"));
        }
        expect(stale(&files, root).is_empty(), "written files are current");
        let one_more = format!("{rust_file}\n");
        expect(std::fs::write(root.join(RUST), one_more).is_ok(), "the Rust table is written again");
        expect(stale(&files, root) == [RUST], "one changed byte is stale");
    }
    let given = input::parse::<Args>(r#"{"command": "check"}"#, SCRIPT);
    expect(
        given.root == ".",
        &format!("the input: Input(command={}, root={})", repr_str(&given.command), repr_str(&given.root)),
    );
    let unknown = input::read(r#"{"command": "check", "lake": "x"}"#).and_then(|it| input::of::<Args>(&it, ""));
    expect(unknown.is_err(), "an unknown field is refused");
    println!("{SCRIPT} selftest: ok");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let p = Patterns::new();
    match args.as_slice() {
        [one] if one == "selftest" => selftest(&p),
        [one] => main_with(&p, &input::parse::<Args>(one, SCRIPT)),
        _ => {
            eprintln!("usage: {SCRIPT} '<json>' | selftest\n\n{DOC}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pattern compiles with the regex features the lock builds (a `(?i)` needs
    /// `unicode-case`), so a missing feature fails `blaze test`, not the script's first run.
    #[test]
    fn the_patterns_compile() {
        let _ = Patterns::new();
    }
}
