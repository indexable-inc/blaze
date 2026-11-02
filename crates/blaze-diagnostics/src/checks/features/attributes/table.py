"""Writes `table.rs`: the `STABILITY` of every parser of rustc's `ATTRIBUTE_PARSERS`, by path,
read from a rust checkout's `compiler/rustc_attr_parsing/src/{context.rs,attributes/}` and the
symbols of `compiler/rustc_span/src/symbol.rs` (MIT or Apache-2.0).

Input: `{"rust": "<rust checkout>", "out": "<path of table.rs>"}` (inline, a path, or `-`).
"""

import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "table.py"


@dataclass(frozen=True)
class Input:
    rust: str
    out: str


@dataclass(frozen=True)
class Gate:
    feature: str
    notes: tuple[str, ...]


@dataclass(frozen=True)
class Parser:
    path: tuple[str, ...]
    gate: Gate | None


def skip_literal(text: str, i: int) -> int:
    """The index after the string, char or comment at `i`, or `i` when there is none."""
    if text.startswith("//", i):
        end = text.find("\n", i)
        return len(text) if end < 0 else end
    m = re.compile(r'b?r(#*)"').match(text, i)
    if m and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
        end = text.index('"' + m.group(1), m.end())
        return end + 1 + len(m.group(1))
    if text[i] == '"':
        j = i + 1
        while text[j] != '"':
            j += 2 if text[j] == "\\" else 1
        return j + 1
    m = re.compile(r"'(?:\\.|[^\\'])'").match(text, i)
    if m:
        return m.end()
    return i


def without_comments(text: str) -> str:
    """`text` with its `//` comments left out, its literals kept."""
    out, i = [], 0
    while i < len(text):
        j = skip_literal(text, i)
        if j == i:
            out.append(text[i])
            i += 1
        else:
            if not text.startswith("//", i):
                out.append(text[i:j])
            i = j
    return "".join(out)


def closing(text: str, open_at: int) -> int:
    """The index of the bracket closing the one at `open_at`."""
    pairs = {"(": ")", "[": "]", "{": "}"}
    stack = [pairs[text[open_at]]]
    i = open_at + 1
    while stack:
        j = skip_literal(text, i)
        if j != i:
            i = j
            continue
        c = text[i]
        if c in pairs:
            stack.append(pairs[c])
        elif c in ")]}":
            if c != stack.pop():
                sys.exit(f"{SCRIPT}: unbalanced `{c}` at {i}")
        i += 1
    return i - 1


def split_top(text: str) -> list[str]:
    """`text` split at its commas outside brackets and literals, each part stripped."""
    parts, depth, start, i = [], 0, 0, 0
    while i < len(text):
        j = skip_literal(text, i)
        if j != i:
            i = j
            continue
        c = text[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append(text[start:i].strip())
            start = i + 1
        i += 1
    parts.append(text[start:].strip())
    return [it for it in parts if it]


def string_value(literal: str) -> str:
    """The value of a Rust string literal (`"..."`, `r#"..."#`), as rustc reads it."""
    literal = literal.strip()
    m = re.fullmatch(r'r(#*)"(.*)"\1', literal, re.S)
    if m:
        return m.group(2)
    if not (literal.startswith('"') and literal.endswith('"')):
        sys.exit(f"{SCRIPT}: `{literal}` is not a string literal")
    body, out, i = literal[1:-1], [], 0
    escapes = {"n": "\n", "t": "\t", "\\": "\\", '"': '"', "'": "'", "0": "\0", "r": "\r"}
    while i < len(body):
        c = body[i]
        if c != "\\":
            out.append(c)
            i += 1
        elif body[i + 1] == "\n":
            i += 2
            while i < len(body) and body[i] in " \t\n\r":
                i += 1
        else:
            out.append(escapes[body[i + 1]])
            i += 2
    return "".join(out)


def symbols(rust: Path) -> dict[str, str]:
    """Each symbol whose text is not its name: `name: "text"` in `symbols!`."""
    text = (rust / "compiler/rustc_span/src/symbol.rs").read_text()
    return dict(re.findall(r'^\s+(\w+):\s*"([^"]*)",', text, re.M))


def path_of(expr: str, renamed: dict[str, str]) -> tuple[str, ...] | None:
    m = re.fullmatch(r"&\[\s*((?:sym::\w+\s*,?\s*)+)\]", expr.strip())
    if not m:
        return None
    return tuple(renamed.get(it, it) for it in re.findall(r"sym::(\w+)", m.group(1)))


def gate_of(expr: str) -> Gate | None | str:
    """The gate `expr` writes: `None` for `AttributeStability::Stable`, `"generic"` for a
    generic parser's `T::STABILITY`."""
    expr = expr.strip()
    if expr == "AttributeStability::Stable":
        return None
    if re.fullmatch(r"T::STABILITY", expr):
        return "generic"
    m = re.fullmatch(r"unstable!\s*\((.*)\)", expr, re.S)
    if not m:
        sys.exit(f"{SCRIPT}: unknown stability `{expr}`")
    args = split_top(m.group(1))
    return Gate(args[0], tuple(string_value(it) for it in args[1:]))


IMPL = re.compile(r"\bimpl\b(?:<[^{;]*?>)?\s+(?:(\w+)(?:<[^{;]*?>)?\s+for\s+)?(\w+)\s*(?:<[^{;]*?>)?\s*\{")


def parsers_of(text: str, renamed: dict[str, str]) -> dict[str, list[Parser]]:
    """Each type's parsers, by its name: those of its `PATH` and `STABILITY`, and those its
    `ATTRIBUTES` lists."""
    consts: dict[str, dict[str, str]] = {}
    mappings: dict[str, list[tuple[str, str]]] = {}
    for m in IMPL.finditer(text):
        name = m.group(2)
        body = text[m.end() : closing(text, m.end() - 1)]
        own = consts.setdefault(name, {})
        for c in re.finditer(r"\bconst (PATH|STABILITY)\s*:\s*[^=;]+=", body):
            end = c.end()
            while True:
                j = skip_literal(body, end)
                if j != end:
                    end = j
                elif body[end] in "([{":
                    end = closing(body, end) + 1
                elif body[end] == ";":
                    break
                else:
                    end += 1
            own[c.group(1)] = body[c.end() : end]
        a = re.search(r"\bconst ATTRIBUTES\s*:\s*AcceptMapping<Self>\s*=\s*&\[", body)
        if a:
            list_end = closing(body, a.end() - 1)
            for entry in split_top(body[a.end() : list_end]):
                if not entry.startswith("("):
                    sys.exit(f"{SCRIPT}: `{name}`'s ATTRIBUTES has `{entry[:40]}`")
                fields = split_top(entry[1 : closing(entry, 0)])
                mappings.setdefault(name, []).append((fields[0], fields[2]))
    out: dict[str, list[Parser]] = {}
    for name, own in consts.items():
        if "PATH" in own and "STABILITY" in own:
            path, gate = path_of(own["PATH"], renamed), gate_of(own["STABILITY"])
            if path is not None and gate != "generic":
                out.setdefault(name, []).append(Parser(path, gate))
    for name, entries in mappings.items():
        for path_expr, gate_expr in entries:
            gate = gate_of(gate_expr)
            if gate == "generic":
                continue
            if path_expr.strip() == "Self::PATH":
                path = path_of(consts.get(name, {}).get("PATH", ""), renamed)
            else:
                path = path_of(path_expr, renamed)
            if path is None:
                sys.exit(f"{SCRIPT}: `{name}` maps `{path_expr}`")
            out.setdefault(name, []).append(Parser(path, gate))
    return out


def registered(context: str) -> list[str]:
    """The parser types `ATTRIBUTE_PARSERS` lists, without their `Single<..>`, `Combine<..>`
    and `WithoutArgs<..>`."""
    aliases = {alias: name for name, alias in re.findall(r"\b(\w+) as (\w+)\b", context[: context.index("attribute_parsers!(")])}
    start = context.index("static ATTRIBUTE_PARSERS = [")
    body = context[start : context.index("];", start)]
    names = []
    for item in split_top(re.sub(r"//[^\n]*", "", body[body.index("[") + 1 :])):
        name = re.findall(r"\w+", item)[-1]
        names.append(aliases.get(name, name))
    return names


def rust_str(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n") + '"'


def render(parsers: list[Parser], source: str) -> str:
    lines = [
        "//! The `STABILITY` of each parser of rustc's `ATTRIBUTE_PARSERS`, from",
        "//! `compiler/rustc_attr_parsing/src/attributes/` of",
        f"//! {source} (MIT or Apache-2.0), generated by `table.py`; regenerate, don't edit.",
        "",
        "use super::Gate;",
        "",
        "/// Every parser's path and, for an unstable one, its gate, sorted by path.",
        "#[rustfmt::skip]",
        "pub(super) static STABILITY: &[(&[&str], Option<Gate>)] = &[",
    ]
    for p in parsers:
        path = ", ".join(rust_str(it) for it in p.path)
        if p.gate is None:
            lines.append(f"    (&[{path}], None),")
        else:
            notes = ", ".join(rust_str(it) for it in p.gate.notes)
            lines.append(f'    (&[{path}], Some(Gate {{ feature: "{p.gate.feature}", notes: &[{notes}] }})),')
    lines += ["];", ""]
    return "\n".join(lines)


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            config = parse(Input, text, SCRIPT)
        case _:
            print(f"usage: {SCRIPT} '{{\"rust\": \"<rust checkout>\", \"out\": \"<table.rs>\"}}'", file=sys.stderr)
            sys.exit(2)
    rust = Path(config.rust)
    src = rust / "compiler/rustc_attr_parsing/src"
    renamed = symbols(rust)
    by_type: dict[str, list[Parser]] = {}
    for file in sorted((src / "attributes").rglob("*.rs")):
        for name, found in parsers_of(without_comments(file.read_text()), renamed).items():
            if name in by_type:
                sys.exit(f"{SCRIPT}: `{name}` is declared twice")
            by_type[name] = found
    parsers = []
    for name in registered((src / "context.rs").read_text()):
        if name not in by_type:
            sys.exit(f"{SCRIPT}: `ATTRIBUTE_PARSERS` lists `{name}`, which has no path read")
        parsers.extend(by_type[name])
    parsers.sort(key=lambda p: p.path)
    paths = [p.path for p in parsers]
    if len(set(paths)) != len(paths):
        sys.exit(f"{SCRIPT}: a path has two parsers")
    commit = subprocess.run(
        ["git", "-C", config.rust, "rev-parse", "HEAD"], capture_output=True, text=True, check=True
    ).stdout.strip()
    Path(config.out).write_text(render(parsers, f"rust-lang/rust@{commit}"))


if __name__ == "__main__":
    main()
