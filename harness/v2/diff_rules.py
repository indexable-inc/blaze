"""diff_rules.py: blaze-diff's tables from their one source, crates/blaze-diff/rules.json (the
operations, the derivative rules, the simplifier's rewrites). It writes the Rust tables the crate
runs and the Lean tables the proofs are about, so the two cannot drift
(docs/v2/plan/design/formal-verification.md, "Derivatives").

    uv run --frozen python harness/v2/diff_rules.py '{"command": "check"}'
    uv run --frozen python harness/v2/diff_rules.py '{"command": "write"}'
    uv run --frozen python harness/v2/diff_rules.py selftest

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
"""

import json
import re
import sys
from dataclasses import dataclass
from fractions import Fraction
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import parse

SCRIPT = "diff_rules.py"
SOURCE = "crates/blaze-diff/rules.json"
RUST = "crates/blaze-diff/src/rules_gen.rs"
LEAN = "lean/analysis/BlazeDiff"
DOMAINS = ("everywhere", "arg_positive", "arg_non_zero", "divisor_non_zero", "args_differ", "away_from_poles", "powi_base")
SLOTS = {"a0": ("A", 0), "a1": ("A", 1), "a2": ("A", 2), "t0": ("T", 0), "t1": ("T", 1), "t2": ("T", 2)}
LEAVES = {"out": ("Out", "out"), "choice": ("Choice", "choice"), "exponent": ("Exponent", "exponent"), "powi_down": ("PowiDown", "powiDown")}
VARS = ("a", "b", "c", "d")
BINARY = {"+": "add", "-": "sub", "*": "mul", "/": "div"}
COMPARE = {"<": "lt", "<=": "le", ">": "gt", ">=": "ge", "==": "eq", "!=": "ne"}
SYMBOL = {v: k for k, v in (BINARY | COMPARE).items()}
TOKEN = re.compile(r"\s*(?:(\d+(?:\.\d+)?)|([a-z_][a-z0-9_]*)|(<=|>=|==|!=|[-+*/(),<>]))")

type Node = tuple  # ("num", float) | ("id", str) | ("neg", Node) | ("op", str, Node, Node) | ("cmp", str, Node, Node) | ("call", str, tuple[Node, ...])


class Bad(Exception):
    """What is wrong with rules.json, naming the row."""


@dataclass(frozen=True)
class Op:
    name: str
    arity: int
    sort: Literal["float", "bool"]


@dataclass(frozen=True)
class Kink:
    point: str
    at: Literal["value", "condition"]


@dataclass(frozen=True)
class Rule:
    op: str
    tangent: str
    domain: str
    ieee: str
    kink: Kink | None = None
    note: str = ""


@dataclass(frozen=True)
class Rewrite:
    name: str
    lhs: str
    rhs: str
    # The class is `class` in the file; `read` renames it (a keyword here).
    klass: Literal["exact", "fast_math"]
    holds: str = ""
    requires: tuple[str, ...] = ()
    note: str = ""


@dataclass(frozen=True)
class Rules:
    about: str
    ops: tuple[Op, ...]
    rules: tuple[Rule, ...]
    rewrites: tuple[Rewrite, ...]


@dataclass(frozen=True)
class Input:
    command: Literal["check", "write"]
    root: str = "."


# The expression language.

def tokens(text: str) -> list[tuple[str, str]]:
    out: list[tuple[str, str]] = []
    at = 0
    while at < len(text):
        found = TOKEN.match(text, at)
        if found is None:
            if text[at:].strip() == "":
                break
            raise Bad(f"`{text}`: nothing reads `{text[at:].strip()}`")
        number, name, symbol = found.groups()
        out.append(("num", number) if number else ("id", name) if name else ("sym", symbol))
        at = found.end()
    return out


def read_expr(text: str) -> Node:
    toks = tokens(text)
    at = 0

    def peek() -> tuple[str, str]:
        return toks[at] if at < len(toks) else ("end", "")

    def take() -> tuple[str, str]:
        nonlocal at
        tok = peek()
        at += 1
        return tok

    def want(symbol: str) -> None:
        if take() != ("sym", symbol):
            raise Bad(f"`{text}`: `{symbol}` is missing")

    def compare() -> Node:
        left = total()
        if peek()[0] == "sym" and peek()[1] in COMPARE:
            op = COMPARE[take()[1]]
            return ("cmp", op, left, total())
        return left

    def total() -> Node:
        left = product()
        while peek() in (("sym", "+"), ("sym", "-")):
            left = ("op", BINARY[take()[1]], left, product())
        return left

    def product() -> Node:
        left = unary()
        while peek() in (("sym", "*"), ("sym", "/")):
            left = ("op", BINARY[take()[1]], left, unary())
        return left

    def unary() -> Node:
        if peek() == ("sym", "-"):
            take()
            if peek()[0] == "num":
                return ("num", -float(take()[1]))
            return ("neg", unary())
        return atom()

    def atom() -> Node:
        kind, value = take()
        if kind == "num":
            return ("num", float(value))
        if kind == "id":
            if peek() != ("sym", "("):
                return ("id", value)
            take()
            args = [compare()]
            while peek() == ("sym", ","):
                take()
                args.append(compare())
            want(")")
            return ("call", value, tuple(args))
        if (kind, value) == ("sym", "("):
            inner = compare()
            want(")")
            return inner
        raise Bad(f"`{text}`: an expression is missing" + (f" before `{value}`" if value else " at the end"))

    out = compare()
    if at != len(toks):
        raise Bad(f"`{text}`: `{toks[at][1]}` is left over")
    return out


def number(value: float) -> str:
    whole = value == int(value) and not (value == 0 and str(value).startswith("-"))
    return str(int(value)) if whole else repr(value)


def show(node: Node, least: int = 0) -> str:
    """`node` in the one canonical spelling: the fewest brackets that read back as it."""
    match node:
        case ("num", value):
            text, binds = number(value), 4
        case ("id", name):
            text, binds = name, 4
        case ("neg", inner):
            # `-1` is a constant and `--a` reads badly: both get brackets.
            text, binds = "-" + (f"({show(inner)})" if inner[0] in ("num", "neg") else show(inner, 3)), 3
        case ("op", op, left, right):
            binds = 1 if op in ("add", "sub") else 2
            text = f"{show(left, binds)} {SYMBOL[op]} {show(right, binds + 1)}"
        case ("cmp", op, left, right):
            text, binds = f"{show(left, 1)} {SYMBOL[op]} {show(right, 1)}", 0
        case ("call", name, args):
            text, binds = f"{name}({', '.join(show(a) for a in args)})", 4
        case _:
            raise TypeError(node)
    return f"({text})" if binds < least else text


def canonical(text: str, where: str) -> Node:
    try:
        node = read_expr(text)
    except Bad as e:
        raise Bad(f"{where}: {e}") from e
    if show(node) != text:
        raise Bad(f"{where}: `{text}` is written `{show(node)}`")
    return node


def camel(name: str, upper: bool) -> str:
    first, *rest = name.split("_")
    return (first.capitalize() if upper else first) + "".join(it.capitalize() for it in rest)


def real(value: float) -> str:
    """A constant as a Lean real."""
    exact = Fraction(value)
    if exact.denominator == 1:
        return f"({exact.numerator} : ℝ)"
    return f"(({exact.numerator} : ℝ) / {exact.denominator})"


# A tangent, to Rust's `P` and Lean's `P`.

def tangent(node: Node, ops: dict[str, Op], where: str, lean: bool) -> str:
    def go(node: Node) -> str:
        return tangent(node, ops, where, lean)

    def pair(rust: str, lean_name: str, *parts: str) -> str:
        if lean:
            return f"(P.{lean_name} {' '.join(parts)})" if parts else f"P.{lean_name}"
        return f"P::{rust}({', '.join('&' + p for p in parts)})" if parts else f"P::{rust}"

    match node:
        case ("num", value):
            return f"(P.lit {real(value)})" if lean else f"P::Lit({value!r})"
        case ("id", name) if name in SLOTS:
            kind, at = SLOTS[name]
            return f"(P.{kind.lower()} .s{at})" if lean else f"P::{kind}({at})"
        case ("id", name) if name in LEAVES:
            return pair(*LEAVES[name])
        case ("op", op, left, right):
            return pair(camel(op, True), op, go(left), go(right))
        case ("neg", inner):
            return pair("Neg", "neg", go(inner))
        case ("call", "powf", (base, exponent)):
            return pair("Powf", "powf", go(base), go(exponent))
        case ("call", "select", (("id", "cond"), then, other)):
            return f"(P.selCond {go(then)} {go(other)})" if lean else f"P::Select(C::Cond, &{go(then)}, &{go(other)})"
        case ("call", "select", (("cmp", op, left, right), then, other)) if op in ("lt", "gt", "eq"):
            if lean:
                return f"(P.sel .{op} {go(left)} {go(right)} {go(then)} {go(other)})"
            return f"P::Select(C::{camel(op, True)}(&{go(left)}, &{go(right)}), &{go(then)}, &{go(other)})"
        case ("call", name, (inner,)) if name in ops and ops[name].arity == 1 and ops[name].sort == "float" and name != "powi":
            return f"(P.call .{camel(name, False)} {go(inner)})" if lean else f"P::Call(Op::{camel(name, True)}, &{go(inner)})"
    raise Bad(f"{where}: a tangent has no `{show(node)}`")


# A rewrite's side, to Rust's `Pat` and to a Lean `Expr`.

def exponent_of(node: Node, where: str) -> int:
    if node[0] != "num" or node[1] != int(node[1]) or not -(2**31) <= node[1] < 2**31:
        raise Bad(f"{where}: a `powi` exponent is an i32 constant, not `{show(node)}`")
    return int(node[1])


def pattern(node: Node, ops: dict[str, Op], where: str, lean: bool) -> str:
    def go(node: Node) -> str:
        return pattern(node, ops, where, lean)

    def of(name: str, args: tuple[Node, ...]) -> str:
        if name not in ops or ops[name].arity != len(args) or len(args) == 0 or name == "powi":
            raise Bad(f"{where}: no operation `{name}` of {len(args)} arguments")
        parts = [go(a) for a in args]
        if lean:
            return f"(Expr.{('un', 'bin', 'ter')[len(args) - 1]} .{camel(name, False)} {' '.join(parts)})"
        return f"Pat::N(Op::{camel(name, True)}, &[{', '.join(parts)}])"

    match node:
        case ("num", value):
            if lean and value == 0 and str(value).startswith("-"):
                raise Bad(f"{where}: -0.0 is 0 over the reals: a fast_math rewrite does not name it")
            return f"(Expr.const {real(value)})" if lean else f"Pat::K({value!r})"
        case ("id", "true" | "false" as name):
            return f"(Expr.bool {name})" if lean else f"Pat::B({name})"
        case ("id", name) if name in VARS:
            return name if lean else f"Pat::V({VARS.index(name)})"
        case ("op", op, left, right):
            return of(op, (left, right))
        case ("cmp", op, left, right):
            return of(op, (left, right))
        case ("neg", inner):
            return of("neg", (inner,))
        case ("call", "powi", (base, exponent)):
            n = exponent_of(exponent, where)
            return f"(Expr.powi (Blaze.Scalar.lit .I32 {real_int(n)}) {go(base)})" if lean else f"Pat::Powi(&{go(base)}, {n})"
        case ("call", name, args):
            return of(name, args)
    raise Bad(f"{where}: a rewrite has no `{show(node)}`")


def real_int(n: int) -> str:
    return str(n) if n >= 0 else f"({n})"


def variables(node: Node) -> set[str]:
    match node:
        case ("id", name):
            return {name} if name in VARS else set()
        case ("num", _):
            return set()
        case ("neg", inner):
            return variables(inner)
        case ("op" | "cmp", _, left, right):
            return variables(left) | variables(right)
        case ("call", _, args):
            return set().union(*(variables(a) for a in args))
    raise TypeError(node)


def requirement(text: str, where: str) -> str:
    """A `requires` entry, `<variable> <comparison> <number>`, as a Lean hypothesis over `env`."""
    node = canonical(text, where)
    match node:
        case ("cmp", op, ("id", name), ("num", value)) if name in VARS:
            value_text, term = real(value), f"{name}.eval env"
            return {"ne": f"{term} ≠ {value_text}", "lt": f"{term} < {value_text}", "le": f"{term} ≤ {value_text}",
                    "gt": f"{value_text} < {term}", "ge": f"{value_text} ≤ {term}", "eq": f"{term} = {value_text}"}[op]
    raise Bad(f"{where}: a requirement is a variable compared with a number, not `{text}`")


# Reading and checking the source.

def read(text: str) -> Rules:
    """rules.json's text as `Rules`; `class` is the field `klass`."""
    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise Bad(f"{SOURCE} is not JSON: {e}") from e
    if isinstance(data, dict) and isinstance(data.get("rewrites"), list):
        for row in data["rewrites"]:
            if isinstance(row, dict) and "class" in row:
                row["klass"] = row.pop("class")
    try:
        return parse(Rules, json.dumps(data), SOURCE)
    except SystemExit as e:
        raise Bad(str(e)) from e


def checked(rules: Rules) -> dict[str, Op]:
    """The operations by name, once every row is well formed."""
    ops: dict[str, Op] = {}
    for op in rules.ops:
        if not re.fullmatch(r"[a-z]+(_[a-z]+)*", op.name) or op.name in ops or not 0 <= op.arity <= 3:
            raise Bad(f"operation `{op.name}`: a name is lower snake case and used once, an arity is 0 to 3")
        ops[op.name] = op
    ruled = [r.op for r in rules.rules]
    for op in rules.ops:
        differentiable = op.sort == "float" and op.name not in ("const", "var")
        if ruled.count(op.name) != int(differentiable):
            raise Bad(f"operation `{op.name}` has {ruled.count(op.name)} rules: a float operation but `const` and `var` has one, a condition none")
    for rule in rules.rules:
        where = f"the rule of `{rule.op}`"
        if rule.op not in ops:
            raise Bad(f"{where}: no such operation")
        if rule.domain not in DOMAINS:
            raise Bad(f"{where}: the domain `{rule.domain}` is none of {', '.join(DOMAINS)}")
        tangent(canonical(rule.tangent, where), ops, where, False)
    if [ops[r.op] for r in rules.rules] != [op for op in rules.ops if op.name in ruled]:
        raise Bad("the rules are not in the operations' order")
    names: set[str] = set()
    fast = False
    for row in rules.rewrites:
        where = f"the rewrite `{row.name}`"
        if not re.fullmatch(r"[a-z0-9]+(-[a-z0-9]+)*", row.name) or row.name in names:
            raise Bad(f"{where}: a name is lower kebab case and used once")
        names.add(row.name)
        lhs, rhs = canonical(row.lhs, where), canonical(row.rhs, where)
        pattern(lhs, ops, where, False)
        pattern(rhs, ops, where, False)
        if lhs[0] == "id":
            raise Bad(f"{where}: the left side matches everything")
        if not variables(rhs) <= variables(lhs):
            raise Bad(f"{where}: the right side names {', '.join(sorted(variables(rhs) - variables(lhs)))}, which the left does not bind")
        if row.klass == "exact":
            if fast:
                raise Bad(f"{where}: the exact rewrites come first")
            if row.holds or row.requires:
                raise Bad(f"{where}: an exact rewrite holds everywhere: it has no `holds` and no `requires`")
        else:
            fast = True
            if not row.holds:
                raise Bad(f"{where}: a fast_math rewrite says in `holds` when it holds")
            for at, text in enumerate(row.requires):
                requirement(text, f"{where}, requirement {at}")
                if not variables(read_expr(text)) <= variables(lhs):
                    raise Bad(f"{where}: `{text}` names a variable the left side does not bind")
    return ops


# The generated files.

def rust_string(text: str) -> str:
    if not text.isascii() or "\\" in text or '"' in text or "\n" in text:
        raise Bad(f"`{text}`: a string a table carries is plain ASCII on one line, with no quote or backslash")
    return f'"{text}"'


def rust(rules: Rules, ops: dict[str, Op]) -> str:
    out = [
        f"// Generated by harness/v2/{SCRIPT} from {SOURCE}: edit the rules there, then run it with",
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
    for op in rules.ops:
        out.append(f"    (Op::{camel(op.name, True)}, {op.arity}, Sort::{op.sort.capitalize()}),")
    out += ["];", "", "/// The table behind `diff::RULES`.", "pub const RULES: &[Rule] = &["]
    for rule in rules.rules:
        where = f"the rule of `{rule.op}`"
        if rule.note:
            out.append(f"    // {rule.note}")
        kink = "None"
        if rule.kink is not None:
            kink = f"Some(Kink {{ point: {rust_string(rule.kink.point)}, at: KinkAt::{rule.kink.at.capitalize()} }})"
        out += [
            "    Rule {",
            f"        op: Op::{camel(rule.op, True)},",
            f"        tangent: {tangent(read_expr(rule.tangent), ops, where, False)},",
            f"        kink: {kink},",
            f"        domain: Domain::{camel(rule.domain, True)},",
            f"        ieee: {rust_string(rule.ieee)},",
            "    },",
        ]
    out += ["];", "", "/// The table behind `simplify::REWRITES`.", "pub const REWRITES: &[Rewrite] = &["]
    for row in rules.rewrites:
        where = f"the rewrite `{row.name}`"
        if row.note:
            out.append(f"    // {row.note}")
        ieee = "Ieee::Exact" if row.klass == "exact" else f"Ieee::FastMath({rust_string(row.holds)})"
        out += [
            "    Rewrite {",
            f"        name: {rust_string(row.name)},",
            f"        lhs: {pattern(read_expr(row.lhs), ops, where, False)},",
            f"        rhs: {pattern(read_expr(row.rhs), ops, where, False)},",
            f"        ieee: {ieee},",
            "    },",
        ]
    out.append("];")
    return "\n".join(out) + "\n"


def lean_header(what: str) -> list[str]:
    return [
        "/-",
        f"Generated by harness/v2/{SCRIPT} from {SOURCE}: edit the rules there, then run it",
        f"with command `write`. {what}",
        "-/",
    ]


def lean_ops(rules: Rules) -> str:
    names = [camel(op.name, False) for op in rules.ops]
    out = lean_header("The operations of `blaze_diff::expr::Op`, in its order.")
    out += ["", "namespace BlazeDiff", "", "/-- A node's operation: `blaze_diff::expr::Op`. -/", "inductive Op where"]
    out += [f"  | {name}" for name in names]
    out += ["  deriving DecidableEq, Repr", "", "namespace Op", "", "/-- How many arguments a node of it has. -/", "def arity : Op → Nat"]
    out += [f"  | .{name} => {op.arity}" for name, op in zip(names, rules.ops)]
    out += ["", "/-- Whether a node of it is a float (a condition otherwise). -/", "def isFloat : Op → Bool"]
    out += [f"  | .{name} => {'true' if op.sort == 'float' else 'false'}" for name, op in zip(names, rules.ops)]
    out += ["", "end Op", "", "end BlazeDiff"]
    return "\n".join(out) + "\n"


def lean_rules(rules: Rules, ops: dict[str, Op]) -> str:
    by_op = {r.op: r for r in rules.rules}
    names = [(op, camel(op.name, False)) for op in rules.ops]
    out = lean_header("Each operation's derivative rule: `blaze_diff::diff::RULES`.")
    out += ["import BlazeDiff.Template", "", "namespace BlazeDiff.Rules", ""]

    def table(doc: str, head: str, value) -> None:
        out.extend([f"/-- {doc} -/", head])
        out.extend(f"  | .{name} => {value(op)}" for op, name in names)
        out.append("")

    def flag(holds) -> object:
        return lambda op: "true" if holds(op) else "false"

    table("Whether the operation has a rule: a float operation but `const` and `var`.", "def hasRule : Op → Bool", flag(lambda op: op.name in by_op))
    table("A ruled operation of one argument that is not `powi` (which carries its exponent).", "def unary : Op → Bool",
          flag(lambda op: op.name in by_op and op.arity == 1 and op.name != "powi"))
    table("A ruled operation of two arguments.", "def binary : Op → Bool", flag(lambda op: op.name in by_op and op.arity == 2))
    table("The node's derivative, as a template; `P.lit 0` for an operation without a rule.", "def tangent : Op → P",
          lambda op: tangent(read_expr(by_op[op.name].tangent), ops, op.name, True) if op.name in by_op else f"(P.lit {real(0.0)})")
    table("Where the rule holds; `everywhere` for an operation without a rule.", "def domain : Op → Domain",
          lambda op: f".{camel(by_op[op.name].domain, False)}" if op.name in by_op else ".everywhere")

    def kink(op: Op) -> str:
        rule = by_op.get(op.name)
        return "none" if rule is None or rule.kink is None else f"some .{rule.kink.at}"

    table("Where the operation has no derivative, when it has such a point.", "def kink : Op → Option KinkAt", kink)
    out.append("end BlazeDiff.Rules")
    return "\n".join(out) + "\n"


def lean_name(name: str) -> str:
    return name.replace("-", "_")


def lean_rewrites(rules: Rules, ops: dict[str, Op]) -> str:
    fast = [r for r in rules.rewrites if r.klass == "fast_math"]
    exact = [r.name for r in rules.rewrites if r.klass == "exact"]
    out = lean_header(
        "Each fast_math rewrite of `blaze_diff::simplify::REWRITES` as a statement over the reals:\n"
        "wherever the left side matches, the term means what the right side means, under the rewrite's\n"
        "`requires`. `Rewrite.AllSound` has one field per statement; BlazeDiff/RewriteProofs.lean gives\n"
        "its one value, so a rewrite added without a proof does not build.\n\n"
        "Nothing is stated for the exact rewrites: they claim equal IEEE 754 bits, which is not a claim\n"
        "about the reals; the crate's bit-exact tests hold them. They are: " + ", ".join(exact) + "."
    )
    out += ["import BlazeDiff.Expr", "", "namespace BlazeDiff.Rewrite", ""]
    for row in fast:
        where = f"the rewrite `{row.name}`"
        lhs, rhs = read_expr(row.lhs), read_expr(row.rhs)
        names = " ".join(v for v in VARS if v in variables(lhs))
        needs = "".join(f"{requirement(text, where)} →\n    " for text in row.requires)
        out += [
            f"/-- `{row.name}`: `{row.lhs}` is `{row.rhs}` ({row.holds}). -/",
            f"def {lean_name(row.name)} : Prop :=",
            f"  ∀ (env : ℕ → ℝ) ({names} : Expr),",
            f"    {needs}{pattern(lhs, ops, where, True)}.eval env = {pattern(rhs, ops, where, True)}.eval env",
            "",
        ]
    out += ["/-- Every fast_math rewrite preserves the term's meaning over the reals. -/", "structure AllSound : Prop where"]
    out += [f"  {lean_name(row.name)} : {lean_name(row.name)}" for row in fast]
    out += ["", "end BlazeDiff.Rewrite"]
    return "\n".join(out) + "\n"


def generated(rules: Rules) -> dict[str, str]:
    """Every generated file's text, by path."""
    ops = checked(rules)
    return {
        RUST: rust(rules, ops),
        f"{LEAN}/Ops.lean": lean_ops(rules),
        f"{LEAN}/Rules.lean": lean_rules(rules, ops),
        f"{LEAN}/Rewrites.lean": lean_rewrites(rules, ops),
    }


def stale(files: dict[str, str], root: Path) -> list[str]:
    return [path for path, text in files.items() if not (root / path).is_file() or (root / path).read_text() != text]


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    try:
        files = generated(read((root / SOURCE).read_text()))
    except Bad as e:
        sys.exit(f"{SCRIPT}: {SOURCE}: {e}")
    if it.command == "write":
        for path, text in files.items():
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(text)
        return
    if wrong := stale(files, root):
        sys.exit("\n".join(f"{SCRIPT}: {path} is stale: run {SCRIPT} with command `write` and commit it" for path in wrong))


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def refused(text: str, says: str) -> bool:
    try:
        generated(read(text))
    except Bad as e:
        return says in str(e)
    return False


def selftest() -> None:
    for text in ("t0 * a1 + a0 * t1 + t2", "(t0 - out * t1) / a1", "-(t0 * sin(a0))", "-(-a)", "a + -0.0", "a * -1", "-a * b",
                 "a + (b + c)", "a - (b - c)", "a / (b * c)", "select(a0 == 0, choice * t0, t0 / (2 * out))", "-(1)", "a - 1", "0.5 * a"):
        expect(show(read_expr(text)) == text, f"`{text}` reads and prints as itself, not `{show(read_expr(text))}`")
    expect(read_expr("a * -1") == ("op", "mul", ("id", "a"), ("num", -1.0)), "a minus before a number is the negative constant")
    expect(read_expr("-a * b") == ("op", "mul", ("neg", ("id", "a")), ("id", "b")), "a minus binds tighter than a product")
    expect(str(read_expr("a + -0.0")[3][1]) == "-0.0", "-0.0 keeps its sign")
    for text, says in (("(a + b)", "is written `a + b`"), ("a+b", "is written"), ("a + ", "is missing"), ("a $ b", "nothing reads")):
        try:
            canonical(text, "here")
            expect(False, f"`{text}` is refused")
        except Bad as e:
            expect(says in str(e), f"`{text}`: {e}")
    ops = {"sin": Op("sin", 1, "float"), "mul": Op("mul", 2, "float"), "neg": Op("neg", 1, "float"), "lt": Op("lt", 2, "bool")}
    expect(tangent(read_expr("t0 * sin(a0)"), ops, "x", False) == "P::Mul(&P::T(0), &P::Call(Op::Sin, &P::A(0)))", "a tangent in Rust")
    expect(tangent(read_expr("t0 * sin(a0)"), ops, "x", True) == "(P.mul (P.t .s0) (P.call .sin (P.a .s0)))", "a tangent in Lean")
    expect(tangent(read_expr("select(a0 < a1, -1, choice)"), ops, "x", True) == "(P.sel .lt (P.a .s0) (P.a .s1) (P.lit (-1 : ℝ)) P.choice)", "a select in Lean")
    expect(pattern(read_expr("-a * 2"), ops, "x", False) == "Pat::N(Op::Mul, &[Pat::N(Op::Neg, &[Pat::V(0)]), Pat::K(2.0)])", "a pattern in Rust")
    expect(pattern(read_expr("powi(a, 2) * b"), ops, "x", True) == "(Expr.bin .mul (Expr.powi (Blaze.Scalar.lit .I32 2) a) b)", "a pattern in Lean")
    expect(requirement("b != 0", "x") == "b.eval env ≠ (0 : ℝ)" and requirement("a >= 0", "x") == "(0 : ℝ) ≤ a.eval env", "requirements")
    expect(real(0.5) == "((1 : ℝ) / 2)" and real(-1.0) == "(-1 : ℝ)", "a constant as a real")

    small = {
        "about": "",
        "ops": [{"name": "const", "arity": 0, "sort": "float"}, {"name": "var", "arity": 0, "sort": "float"},
                {"name": "mul", "arity": 2, "sort": "float"}, {"name": "div", "arity": 2, "sort": "float"},
                {"name": "mul_add", "arity": 3, "sort": "float"}, {"name": "lt", "arity": 2, "sort": "bool"}],
        "rules": [{"op": "mul", "tangent": "t0 * a1 + a0 * t1", "domain": "everywhere", "ieee": "finite"},
                  {"op": "div", "tangent": "(t0 - out * t1) / a1", "domain": "divisor_non_zero", "ieee": "finite"},
                  {"op": "mul_add", "tangent": "t0 * a1 + a0 * t1 + t2", "domain": "everywhere", "ieee": "finite",
                   "kink": {"point": "0", "at": "value"}}],
        "rewrites": [{"name": "mul-one", "lhs": "a * 1", "rhs": "a", "class": "exact"},
                     {"name": "div-self", "lhs": "a / a", "rhs": "1", "class": "fast_math", "holds": "a divisor that is not 0", "requires": ["a != 0"]}],
    }
    files = generated(read(json.dumps(small)))
    expect(sorted(files) == sorted([RUST, f"{LEAN}/Ops.lean", f"{LEAN}/Rules.lean", f"{LEAN}/Rewrites.lean"]), f"the files: {sorted(files)}")
    expect("(Op::MulAdd, 3, Sort::Float)," in files[RUST] and "ieee: Ieee::FastMath(\"a divisor that is not 0\")," in files[RUST], files[RUST])
    expect("kink: Some(Kink { point: \"0\", at: KinkAt::Value })," in files[RUST], "a kink in Rust")
    expect("  | mulAdd\n" in files[f"{LEAN}/Ops.lean"] and "  | .lt => false" in files[f"{LEAN}/Ops.lean"], files[f"{LEAN}/Ops.lean"])
    expect("  | .div => .divisorNonZero" in files[f"{LEAN}/Rules.lean"] and "  | .mulAdd => some .value" in files[f"{LEAN}/Rules.lean"], files[f"{LEAN}/Rules.lean"])
    rewrites = files[f"{LEAN}/Rewrites.lean"]
    expect("def div_self : Prop :=\n  ∀ (env : ℕ → ℝ) (a : Expr),\n    a.eval env ≠ (0 : ℝ) →\n    (Expr.bin .div a a).eval env = (Expr.const (1 : ℝ)).eval env" in rewrites, rewrites)
    expect("  div_self : div_self" in rewrites and "mul_one" not in rewrites and "They are: mul-one." in rewrites, "only fast_math rewrites are stated")

    def changed(edit) -> str:
        copy = json.loads(json.dumps(small))
        edit(copy)
        return json.dumps(copy)

    expect(generated(read(changed(lambda d: d["rules"][0].update(tangent="t0 * a1 - a0 * t1"))))[RUST] != files[RUST], "a changed tangent changes the Rust table")
    expect(refused(changed(lambda d: d["rules"].pop(0)), "`mul` has 0 rules"), "a float operation without a rule is refused")
    expect(refused(changed(lambda d: d["rules"][0].update(domain="nowhere")), "the domain `nowhere`"), "an unknown domain is refused")
    expect(refused(changed(lambda d: d["rules"][0].update(tangent="t0 * b")), "a tangent has no `b`"), "a tangent naming a pattern variable is refused")
    expect(refused(changed(lambda d: d["rules"].reverse()), "not in the operations' order"), "rules out of order are refused")
    expect(refused(changed(lambda d: d["rewrites"].reverse()), "the exact rewrites come first"), "a fast_math rewrite before an exact one is refused")
    expect(refused(changed(lambda d: d["rewrites"][1].update(rhs="b")), "which the left does not bind"), "an unbound variable is refused")
    expect(refused(changed(lambda d: d["rewrites"][1].pop("holds")), "says in `holds`"), "a fast_math rewrite without `holds` is refused")
    expect(refused(changed(lambda d: d["rewrites"][1].update(name="mul-one")), "used once"), "a repeated name is refused")
    expect(refused(changed(lambda d: d["rewrites"][0].update(colour="red")), "no field"), "an unknown field is refused")

    import tempfile
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        expect(sorted(stale(files, root)) == sorted(files), "a missing file is stale")
        for path, text in files.items():
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text(text)
        expect(stale(files, root) == [], "written files are current")
        (root / RUST).write_text(files[RUST] + "\n")
        expect(stale(files, root) == [RUST], "one changed byte is stale")
    given = parse(Input, '{"command": "check"}', SCRIPT)
    expect(given.root == ".", f"the input: {given}")
    try:
        parse(Input, '{"command": "check", "lake": "x"}', SCRIPT)
        expect(False, "an unknown field is refused")
    except SystemExit:
        pass
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
